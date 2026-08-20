# Phase 1 执行计划 — 低功耗高吞吐改造

> **版本**：v0.22.0 Phase 1
> **起止**：2026-08-24 → 2026-09-04（10 个工作日）
> **基线**：v0.14.0（commit `1e1ab8f`，2026-08-08）
> **分支**：`phase1/perf-optimization`
> **作者**：EngramDB 性能优化组

---

## 一、目标与指标

### 1.1 阶段目标

聚焦三个最小可行优化，最大化 ROI：

1. **WAL 组提交验证**：核心代码已实现，本次只做验证 + 边界测试 + 性能基准
2. **列存扫描零拷贝**：消除两处剩余克隆（`table.rs:2170` / `table.rs:2269`），降低扫描路径拷贝与分配
3. **Arena 查询分配器**：引入 `bumpalo` 管理查询生命周期内的临时分配，降低 GC/分配器压力

### 1.2 关键 KPI（10 次 P50 + dhat 分配次数绝对值）

| KPI | 当前基线 | 验收阈值 | 判定方法 |
|---|---|---|---|
| 小事务写入吞吐（M1） | `group=0` 单 fsync | `group=16` vs `group=0` ≥ 3× | `wal_group_commit_bench`，10 次 P50 |
| 单列全扫吞吐（M2） | 1.0× | ≥ +30% | `m3_log_bench` + `select_star_bench`，10 次 P50 |
| 选择性 1% 查询延迟（M2） | 1.0× | ≥ -50% | 自建选择性 bench，10 次 P50 |
| 列扫描内存峰值（M2） | 1.0× | ≥ -30% | heaptrack |
| 查询执行期分配次数（M3） | 基线 | ≥ -60% | dhat（决策 A 主指标） |
| 综合写入吞吐（M4） | 1.0× | ≥ +50% | `v018_write_bench`，10 次 P50 |
| 综合查询内存占用（M4） | 1.0× | ≥ -30% | dhat |

### 1.3 范围外（推迟到 Phase 2/3）

- 冷热数据自动流动（Phase 2）
- LogEngine 写入路径极致优化（Phase 2）
- 列存 PREWHERE + Late Materialization 深度优化（Phase 2，P1-3 任务）
- 列级自动编码选择（Phase 2）
- 电源感知模式 / mmap 统一读路径 / 单线程事件循环（Phase 3）
- Buffer pool 处置（Phase 3 与 mmap 化一并讨论）

---

## 二、当前代码校准（v0.14.0 实测）

| 项目 | 现状 | 证据 | 结论 |
|---|---|---|---|
| WAL 组提交核心 | ✅ 实现完成 | `src/wal/writer.rs:180-222` + 10 个 `mod tests:485-675` | 本期只做验证 |
| WAL 默认值 | ✅ 已生产启用 | `src/common/config.rs:307-312`：`16/64KB/10ms` | 无需改动 |
| 事务层链路 | ✅ 接通 | `src/txn/manager.rs:54-71` + `:132-180` | 已有 e2e 但需补充 |
| 列存扫描零拷贝（部分） | ✅ | `column_store.rs:514 read_column()` 返回 `&ColumnData` | 基础已具备 |
| 列存扫描零拷贝（残留） | ⚠️ 2 处 | `table.rs:2170` `col_data.clone()` + `table.rs:2269` `row[col_idx].clone()` | 本期消除 |
| 列存扫描零拷贝（gather） | ✅ | `column_data.rs:204` typed gather 零 Value 构造 | 已达成 |
| Arena 分配器 | ❌ | Cargo.lock 有 `bumpalo` 但 src/ 完全未使用 | 本期引入 |
| Buffer pool | 🪦 死代码 | `buffer_pool.rs` 仅在自身 `mod tests` 中使用 | 不在本期处理 |
| jemalloc | ✅ 已全局启用 | `src/lib.rs:70-72` | 本期不改动 |

---

## 三、详细排期

### 第 1 周（2026-08-24 → 2026-08-28）

#### 周一 8/24 — D0：基线建立

| 时段 | 任务 | 工时 | 产出 |
|---|---|---|---|
| 上午 | 跑通生产 benchmarks：核心 5 项（`core_bench`/`m1_acceptance_bench`/`v018_write_bench`/`m3_log_bench`/`m2_memory_bench`/`select_star_bench`），每项 10 次取 P50 | 3h | 基线数字 |
| 上午 | dhat 跑核心查询场景（`core_bench` 中查询路径），记录分配次数绝对值 | 4h | dhat 基线报告 |
| 下午 | 整理基线报告 | 1h | `docs/p1-d0-baseline.md` |

#### 周二 8/25 — M1：WAL 组提交验证

| 时段 | 任务 | 工时 | 产出 |
|---|---|---|---|
| 上午 | 端到端链路回归测试：`TransactionManager::commit()` → `wal.commit_flush()` 全通路 | 3h | `tests/group_commit_e2e.rs` |
| 下午 | 边界测试：空事务 / 超 buffer 大事务（>64KB）/ 多线程并发 commit（每线程独立 + 共享 writer） | 3h | `tests/group_commit_concurrency.rs` |
| 下午 | 崩溃一致性：`kill -9` 注入脚本（写入中途 / fsync 后 commit 前 / commit 后），恢复后无悬挂事务、无丢已 fsync 数据 | 2h | `tests/crash_consistency.rs` + `scripts/inject_crash.sh` |

**M1 验收**：事务吞吐 `group=16` vs `group=0` ≥ 3×（10 次 P50），所有测试绿，无功能回归。

#### 周三 8/26 — M2 准备：扫描路径审计与设计

| 时段 | 任务 | 工时 | 产出 |
|---|---|---|---|
| 上午 | 精确定位两处拷贝点：`table.rs:2170` `col_data.clone()` 每列每 RG；`table.rs:2269` `row[col_idx].clone()` 每 delta 行每列。dhat 抽样确认占总分配比例 | 4h | `docs/p1-zcopy-audit.md` |
| 下午 | 设计零拷贝 API：扩 `ColumnData::take_into(batch_size, dst: &mut ColumnData)`；扩 `delta_store::all_rows_borrowed() -> impl Iterator<Item=(u64, &[Value])>`；草图 PR diff | 4h | 设计文档 |

#### 周四 8/27 — M2 实施：列存零拷贝改造（上）

| 时段 | 任务 | 工时 | 产出 |
|---|---|---|---|
| 上午 | `scan_to_chunks_impl` 重构：消除 `col_data.clone()`，改为每 RG 累计到 batch 边界整批移交 `take_front` | 5h | `src/storage/table.rs:2167-2245` |
| 下午 | Zone Map 命中埋点（验证 P2.4 `can_skip_predicate` 工作，无 regression） | 3h | `tests/zone_map_stats.rs` |

#### 周五 8/28 — M2 实施：列存零拷贝改造（下）+ M2 验收

| 时段 | 任务 | 工时 | 产出 |
|---|---|---|---|
| 上午 | 消除 delta 路径克隆：新增 `delta_store::all_rows_borrowed()` 返回 `(rid, &[Value])` 视图 | 3h | `src/storage/delta_store.rs` |
| 下午 | 校验 PREWHERE 路径 `gather()` 复用而非新建（已 OK，仅核对） | 1h | 回归测试 |
| 下午 | 重跑 D0 全套 bench + dhat，记录 M2 delta；功能全量回归 | 4h | `docs/p1-m2-report.md` |

**M2 验收**：单列全扫 +30%、1% 选择性查询 -50%、列扫描内存峰值 -30%（10 次 P50 + heaptrack）。

### 第 2 周（2026-08-31 → 2026-09-04）

#### 周一 8/31 — M3 起步：Arena 试点

| 时段 | 任务 | 工时 | 产出 |
|---|---|---|---|
| 上午 | `Cargo.toml` 引入 `bumpalo = "3"`；新建 `src/executor/arena.rs` 定义 `QueryArena` + RAII `with_arena(\|\|{ ... })`；`feature = "query-arena"` 默认 OFF | 4h | 新模块 + feature flag |
| 下午 | 试点：在 `src/executor/expression.rs` 选最热路径（建议 `eval_const_fold` 或列裁剪），临时 `Vec<Value>` 改 `&bumpalo` | 4h | 已迁移算子列表 |

#### 周二 9/1 — M3 扩展到数据流

| 时段 | 任务 | 工时 | 产出 |
|---|---|---|---|
| 上午 | `DataChunk` 增加 `arena: Option<&'a bumpalo::Bump>` 字段；为 Typed Vector 中 Varchar/Blob/Json/Vector 提供 arena-backed 替代路径 | 5h | `src/executor/vector.rs` |
| 下午 | 物理计划执行入口 `executor.execute()` 用 `with_arena` 包裹整棵算子树 | 3h | `src/executor/executor.rs` |

#### 周三 9/2 — M3 集成算子层

| 时段 | 任务 | 工时 | 产出 |
|---|---|---|---|
| 上午 | `hash_join` key arena 化（`src/executor/operators/hash_join.rs:121,259,471` 三处 `FxHashMap<Vec<Value>,...>`） | 4h | arena-backed key |
| 下午 | sort/limit/window 临时 buffer arena 化 | 4h | 三文件 |

#### 周四 9/3 — 集成调优 + 回归

| 时段 | 任务 | 工时 | 产出 |
|---|---|---|---|
| 上午 | 三项优化联调：默认配置 vs `--features query-arena` 对照 | 4h | 联调报告 |
| 下午 | dhat 重跑核心查询，记录分配次数绝对值（决策 A 主指标） | 4h | `docs/p1-profile.md` |

#### 周五 9/4 — M4 验收 + 交付

| 时段 | 任务 | 工时 | 产出 |
|---|---|---|---|
| 上午 | D0 bench 全量重跑 + dhat（10 次 P50） | 4h | `docs/phase1-report.md`（含所有 KPI 对比表） |
| 上午 | `cargo fmt --check` + `cargo clippy --all-targets --all-features -- -D warnings` 通过 CI | 1h | CI 绿 |
| 下午 | 代码整理、注释、PR 草稿（不动 main，等用户 review） | 3h | 可合并分支 |

**M4 验收**：综合写入吞吐 +50%、综合查询内存占用 -30%、CI 全绿、报告交付。

---

## 四、风险与缓解

| 风险 | 概率 | 影响 | 缓解 |
|---|---|---|---|
| 列存零拷贝触及 `take_front` + `gather` 所有权链，借用检查过严 | 中 | 高 | 先在 `memory_engine.scan_to_chunks` 试点（量小、好回退），再扩到 columnar |
| `bumpalo` 与 `&mut self`（`column_store::read_column` 是 `&mut`）冲突 | 中-高 | 中 | 列存零拷贝用 `&` 解借用；arena 设计时优先考虑非 `&mut self` 的路径 |
| 测量噪声掩盖真实收益 | 中 | 高 | 10 次 P50 + dhat 分配次数绝对值（强证据） |
| WAL 组提交在 `sync_wal()` 强制刷盘后恢复路径 regression | 低 | 高 | 跑 `tests/persistence_test.rs` 完整回归 |
| jemalloc 在某些 OS 引起 timing 漂移 | 低 | 低 | 已默认启用作为基线，本期不改动 |

---

## 五、交付清单

### 5.1 文档

- `docs/p1-phase1-execution-plan.md`（本文档）
- `docs/p1-d0-baseline.md`（D0 基线报告）
- `docs/p1-zcopy-audit.md`（列存零拷贝审计与设计）
- `docs/p1-m2-report.md`（M2 验收报告）
- `docs/p1-profile.md`（M3 性能 profiling）
- `docs/phase1-report.md`（Phase 1 总结，含 KPI 对比）

### 5.2 代码

- `src/wal/writer.rs`（不变，仅验证）
- `src/storage/table.rs`（消除 `col_data.clone()`）
- `src/storage/delta_store.rs`（新增 `all_rows_borrowed()`）
- `src/executor/arena.rs`（新增）
- `src/executor/vector.rs`（arena-backed 替代路径）
- `src/executor/executor.rs`（`with_arena` 包裹）
- `src/executor/operators/hash_join.rs`（key arena 化）
- `src/executor/operators/sort.rs` / `limit.rs` / `window.rs`（buffer arena 化）
- `Cargo.toml`（引入 `bumpalo = "3"`，新增 `query-arena` feature）

### 5.3 测试

- `tests/group_commit_e2e.rs`
- `tests/group_commit_concurrency.rs`
- `tests/crash_consistency.rs`
- `tests/zone_map_stats.rs`

### 5.4 基准

- `benches/wal_group_commit_bench.rs`（新增）
- `Cargo.toml` `[[bench]]` 注册

### 5.5 脚本

- `scripts/inject_crash.sh`（崩溃注入测试）

---

## 六、决策日志

| 日期 | 决策 | 上下文 |
|---|---|---|
| 2026-08-20 | D0 基线单独成日 | 后续每项优化需可量化对比 |
| 2026-08-20 | Arena feature flag 默认 OFF | 借用检查是核心难点，灰度发布可控 |
| 2026-08-20 | 测量方案 A（10 次 P50 + dhat 分配次数绝对值） | 强证据、低噪声敏感 |
| 2026-08-20 | Buffer pool 不在 Phase 1 处理 | 实测为死代码，Phase 3 与 mmap 化一并讨论 |
| 2026-08-20 | Phase 1 总工期 10 个工作日 | 原 P0-1 工作量比预期小 60%，多余预算投入深度优化 |

---

## 七、状态追踪

| 阶段 | 状态 | 起始 | 截止 | 验收 |
|---|---|---|---|---|
| D0 基线 | 待启动 | 8/24 上午 | 8/24 EOD | `docs/p1-d0-baseline.md` |
| M1 WAL 验证 | 待启动 | 8/25 | 8/25 EOD | `wal_group_commit_bench` ≥ 3× |
| M2 列存零拷贝 | 待启动 | 8/26 | 8/28 EOD | 单列 +30% / 选择性 -50% / 内存 -30% |
| M3 Arena | 待启动 | 8/31 | 9/2 EOD | 分配次数 -60% |
| M4 验收 | 待启动 | 9/3 | 9/4 EOD | 综合 +50% / -30% / CI 绿 |

---

## 八、附录：基线命令模板

```bash
# D0 基线 bench（10 次 P50）
for bench in core_bench m1_acceptance_bench v018_write_bench m3_log_bench m2_memory_bench select_star_bench; do
    cargo bench --bench $bench -- --warm-up-time 2 --measurement-time 5 --sample-size 10 2>&1 | tee bench_baseline_${bench}.log
done

# D0 基线 dhat（查询执行期分配次数）
cargo run --release --example dhat_query_trace 2>&1 | tee dhat_baseline.log

# 编译检查
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --release --lib --tests
```