# M3 — Arena 查询分配器

> **日期**：2026-08-31 → 2026-09-02（周一 → 周三）
> **截止**：2026-09-02 EOD
> **KPI**：查询执行期分配次数 **≥ -60%**（dhat，决策 A 主指标）
> **特性门控**：`feature = "query-arena"` 默认 OFF，Phase 1 验收时合入主分支再默认 ON

---

## 一、目标与范围

### 1.1 目标

引入 `bumpalo`（bump allocator）管理查询执行期所有临时分配，使每个查询的内存占用在查询结束时一次性回收，降低：
- 分配器压力（高频小对象分配）
- GC 等价开销（jemalloc tcache 抖动）
- 碎片化（顺序 bump，无回收开销）

### 1.2 范围（Phase 1）

- 表达式求值器的临时结果
- DataChunk / Vector 中 Varchar/Blob/Json/Vector 类型的 owned 路径
- hash_join 的 hash key
- sort/limit/window 的临时 buffer

### 1.3 范围外（推迟）

- 存储层（column_store、memory_engine、log_engine）—— 持久化数据不能用 arena
- WAL、压缩、序列化 —— 文件 I/O 路径
- 单写多读下的并发安全 —— Phase 3 单线程事件循环模式

---

## 二、bumpalo 基础

### 2.1 为什么选 bumpalo

- **最快**：单线程下零分配开销，仅指针推进
- **简单**：API 极简（`alloc()` + `with_arena(\|\|)`）
- **零拷贝友好**：`&'a T` 借用周期清晰
- **批量释放**：查询结束一次性 `reset()`，无需逐对象 drop

### 2.2 风险

- **生命周期**：arena 内分配的 `&T` 借用周期 ≤ arena 本身
- **非 Send**：单线程场景；多线程需要 `Arc<Mutex<Bump>>` 或 per-thread arena
- **不可 Drop**：bumpalo 不调用 drop（性能优化），需要保证 arena 内对象无重要析构逻辑

---

## 三、API 设计

### 3.1 `src/executor/arena.rs`（新建）

```rust
//! 查询生命周期 Arena
//!
//! 每个查询分配一个 Arena，所有查询执行期临时对象都在 Arena 内分配。
//! 查询结束时整体 reset（不调用 drop，但内部 typed 数组 drop）。
//!
//! 用法：
//! ```ignore
//! with_query_arena(|arena| {
//!     let s: &mut String = arena.alloc_str("hello");
//!     // ... 执行查询 ...
//! });
//! // arena.reset() 在此自动调用
//! ```

use bumpalo::Bump;
use std::cell::RefCell;

/// 线程局部 query arena（单线程场景）
///
/// 使用 `thread_local!` 因为：
/// 1. executor 是单线程事件循环（v0.14 已经是同步 API）
/// 2. 避免 Arc<Mutex> 同步开销
/// 3. 每个查询独立 arena，跨查询无污染
thread_local! {
    static QUERY_ARENA: RefCell<Bump> = RefCell::new(Bump::with_capacity(64 * 1024));
}

/// RAII 守卫：构造时 reset arena，析构时（query 结束）自动 reset
///
/// 调用方在 RAII 范围内执行查询；arena 借用周期 = RAII 周期。
pub struct QueryArenaGuard {
    arena: &'static Bump,
}

impl QueryArenaGuard {
    pub fn new() -> Self {
        QUERY_ARENA.with(|a| a.borrow_mut().reset());
        Self {
            arena: QUERY_ARENA.with(|a| a.borrow()),
        }
    }

    /// 在 arena 内分配 T
    pub fn alloc<T>(&self, val: T) -> &mut T {
        self.arena.alloc(val)
    }

    /// 在 arena 内分配 str
    pub fn alloc_str(&self, s: &str) -> &mut str {
        self.arena.alloc_str(s)
    }

    /// 分配 typed Vec<T>
    pub fn alloc_vec<T>(&self, cap: usize) -> &mut Vec<T> {
        let mut v = Vec::with_capacity_in(cap, self.arena);
        // 安全：刚分配的 Vec 借用周期 = self 借用周期
        unsafe { std::mem::transmute(self.arena.alloc(v)) }
    }
}

/// RAII 风格入口：自动 reset + 提供 guard
///
/// ```ignore
/// with_query_arena(|guard| {
///     // ... 执行查询 ...
/// });
/// ```
pub fn with_query_arena<F, R>(f: F) -> R
where
    F: FnOnce(&QueryArenaGuard) -> R,
{
    let guard = QueryArenaGuard::new();
    let result = f(&guard);
    // arena 在这里自动 reset（借用结束时）
    drop(guard);
    // 显式 reset 防止 drop 的 drop 残留
    QUERY_ARENA.with(|a| a.borrow_mut().reset());
    result
}
```

### 3.2 `src/executor/vector.rs` 扩展

**新增 arena-backed typed 替代路径**：

```rust
use crate::executor::arena::QueryArenaGuard;

impl Vector {
    /// 创建 arena-allocated typed Vector（Varchar/Blob/Json/Vector 借用 arena）
    ///
    /// arena 借用周期内可用；查询结束随 arena 一起 reset。
    pub fn from_typed_arena(data: ColumnData) -> Vector {
        Vector::Typed(data)
    }

    /// 获取第 i 行的 Value（typed → Value 转换，必要时克隆字符串到 arena）
    pub fn get_in_arena(&self, idx: usize, arena: &QueryArenaGuard) -> Value {
        match self {
            Vector::Flat(v) => v[idx].clone(),
            Vector::Constant(val, _) => val.clone(),
            Vector::Typed(d) => d.get_in_arena(idx, arena),
        }
    }
}

impl ColumnData {
    /// typed 数组 get，字符串分配在传入的 arena 上
    pub fn get_in_arena(&self, i: usize, arena: &QueryArenaGuard) -> Value {
        if let Some(nulls) = &self.nulls {
            if nulls.test(i) { return Value::Null; }
        }
        match &self.values {
            ColumnValue::Varchar(v) => {
                let s = v[i].as_str();
                Value::Varchar(arena.alloc_str(s).to_string())
                // ↑ 这里仍 to_string()，是 trade-off：要么改 Value API 支持 &str，要么构造 owned
                // 短期方案：维持 owned，依赖 arena 提供分配池降低总体分配压力
            }
            ColumnValue::Blob(v) => {
                let b = &v[i];
                Value::Blob(arena.alloc(b.clone()).to_vec())
            }
            // ... 其他变体保持原样
            _ => self.get(i),
        }
    }
}
```

### 3.3 `src/executor/operators/hash_join.rs` 改造

**当前（`hash_join.rs:121,259,471`）**：
```rust
let mut map: FxHashMap<Vec<Value>, Vec<(usize, usize)>> = FxHashMap::default();
```

**改造后**：
```rust
let mut map: FxHashMap<&arena::Bump, Vec<(usize, usize)>> = FxHashMap::default();
// ↑ 复杂度过高：&Bump 不能作 key（无 Hash + Eq）
```

**实际方案**：保持 `Vec<Value>`，但 Value 中 Varchar/Blob 改用 arena-backed owned：

```rust
// 让 Value::Varchar 内部字符串使用 arena 分配的 String
// 通过 with_query_arena 包裹整个 hash_join 调用
```

短期实现：在 hash_join 入口处调 `with_query_arena`，所有 key 构造走 arena，复用字符串池（去重）。

### 3.4 `src/executor/executor.rs` 改造

**核心执行入口**：

```rust
impl Executor {
    pub fn execute(&mut self, plan: &PhysicalPlan) -> Result<Vec<DataChunk>> {
        // 整个查询用 arena 包裹
        crate::executor::arena::with_query_arena(|_guard| {
            self.execute_inner(plan)
        })
    }

    fn execute_inner(&mut self, plan: &PhysicalPlan) -> Result<Vec<DataChunk>> {
        // 不变 —— 调用方已提供 arena 借用周期
    }
}
```

---

## 四、`Cargo.toml` 配置

```toml
[dependencies]
bumpalo = "3"

[features]
default = []
# Phase 1：query arena 默认 OFF（灰度）
# Phase 1 验收后合入主分支时改为 ON（见 `docs/phase1-report.md`）
query-arena = []

[dev-dependencies]
# 不需要额外配置
```

`src/lib.rs` 在 jemalloc 后添加：

```rust
#[cfg(all(feature = "query-arena", not(feature = "dhat-heap")))]
mod query_arena {
    // feature gate：启用 query-arena 时编译以下模块
}
```

实际上 `src/executor/arena.rs` 模块本身就是 `#[cfg(feature = "query-arena")]`，调用点也是 `#[cfg(feature = "query-arena")]`。

---

## 五、本周工作量分配

| 日期 | 任务 | 工时 | 产出 |
|---|---|---|---|
| 8/31 周一上午 | `Cargo.toml` 引入 bumpalo；新建 `src/executor/arena.rs` + `feature = "query-arena"` | 4h | 新模块 + feature flag |
| 8/31 周一下午 | 试点：`src/executor/expression.rs` `eval_const_fold` 改用 arena；单测验证 | 4h | 已迁移算子列表 |
| 9/1 周二上午 | `DataChunk` arena-backed typed 替代路径；`Vector::get_in_arena` 实现 | 5h | `src/executor/vector.rs` |
| 9/1 周二下午 | `executor.execute()` `with_query_arena` 包裹整棵算子树 | 3h | `src/executor/executor.rs` |
| 9/2 周三上午 | `hash_join` key arena 化（3 处 `FxHashMap<Vec<Value>,...>`） | 4h | `src/executor/operators/hash_join.rs` |
| 9/2 周三下午 | sort/limit/window 临时 buffer arena 化 | 4h | 三文件 |

---

## 六、风险与缓解

| 风险 | 概率 | 影响 | 缓解 |
|---|---|---|---|
| `&arena::Bump` 不能作 HashMap key | 高 | 中 | 保持 `Vec<Value>`，但 Value 内部字符串用 arena-allocated String 复用池 |
| `&str` 借用周期与 `Value` API 不兼容（Value::Varchar 需要 owned String） | 中 | 中 | 短期：`to_string()` 拷贝到 arena（仍省分配器抖动）；中期：扩展 Value 支持 `Borrowed(&'arena str)` |
| `executor.execute()` 嵌套调用时 arena reset 污染父查询 | 低 | 高 | 单一入口点 `with_query_arena`；嵌套调用复用同一 guard |
| 多线程场景下 thread_local 隔离但执行顺序乱 | 低 | 中 | executor 是同步 API（v0.14 已验证），单线程安全 |
| bumpalo 容量耗尽 → 重新分配 → 旧引用失效 | 低 | 高 | `with_capacity_and_alignment` 预留大块（默认 64KB，可按查询调整） |

---

## 七、验收

### 7.1 功能验收

- [ ] `cargo build --features query-arena` 通过
- [ ] `cargo build`（默认）通过（无回归）
- [ ] `cargo test --release --features query-arena` 全绿
- [ ] `cargo test --release`（默认）全绿
- [ ] `cargo clippy --all-targets --all-features -- -D warnings` 通过

### 7.2 性能验收（决策 A）

- [ ] dhat 跑核心查询：`default` vs `--features query-arena`
- [ ] 分配次数绝对值降低 ≥ 60%
- [ ] 总分配字节降低 ≥ 30%
- [ ] 10 次 P50 延迟对比（不应有 10%+ regression）

### 7.3 内存安全验收

- [ ] miri 检查 arena 借用周期：`cargo +nightly miri test --features query-arena`
- [ ] 无 use-after-free / double-free
- [ ] 大量查询无内存累积增长（连续 1000 次查询后 RSS 稳定）

---

## 八、M3 验收报告模板（执行后填写 → `docs/p1-m3-report.md`）

```markdown
# M3 验收报告

**日期**：2026-09-02
**commit**：（git rev-parse HEAD）
**feature flag**：query-arena 默认 OFF

## 1. 迁移范围

| 模块 | 迁移路径数 | 备注 |
|---|---|---|
| src/executor/expression.rs | _____ | |
| src/executor/vector.rs | _____ | |
| src/executor/executor.rs | _____ | |
| src/executor/operators/hash_join.rs | _____ | |
| src/executor/operators/sort.rs | _____ | |
| src/executor/operators/limit.rs | _____ | |
| src/executor/operators/window.rs | _____ | |

## 2. dhat 分配次数对比

| 场景 | 默认（OFF） | query-arena（ON） | 下降 |
|---|---|---|---|
| 全表扫描 100k 行 | _____ | _____ | _____ % |
| 选择性 1% 查询 | _____ | _____ | _____ % |
| hash_join 1k×1k | _____ | _____ | _____ % |
| sort 100k 行 | _____ | _____ | _____ % |

**KPI 结论**：(达成/未达成)

## 3. 延迟对比（10 次 P50）

| 场景 | OFF | ON | 变化 |
|---|---|---|---|
| 全表扫描 | _____ ms | _____ ms | _____ % |
| 选择性 1% | _____ ms | _____ ms | _____ % |
| hash_join | _____ ms | _____ ms | _____ % |
| sort | _____ ms | _____ ms | _____ % |

## 4. miri 内存安全

- [ ] 通过 / 失败
- 任何 use-after-free / 数据竞争：_____

## 5. 已知问题

（任何遗留或推迟项）
```