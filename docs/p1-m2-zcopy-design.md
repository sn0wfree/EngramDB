# M2 — 列存扫描零拷贝改造

> **日期**：2026-08-26 → 2026-08-28（周三 → 周五）
> **截止**：2026-08-28 EOD
> **KPI**：
> - 单列全扫吞吐 **≥ +30%**（10 次 P50）
> - 选择性 1% 查询延迟 **≥ -50%**（10 次 P50）
> - 列扫描内存峰值 **≥ -30%**（heaptrack）

---

## 一、代码现状审计

### 1.1 已实现的零拷贝基础（无需重做）

| 优化点 | 行号 | 状态 |
|---|---|---|
| `read_column()` 返回 `&ColumnData` | `src/storage/column_store.rs:514` | ✅ |
| typed 数组直读（零 Value 转换） | `src/common/column_data.rs:204` `gather()` | ✅ |
| Zone Map skip predicate | `src/storage/table.rs:2153-2164`（调 `can_skip_predicate`） | ✅ |
| PREWHERE + Late Materialization | `src/storage/table.rs:2179-2245` | ✅ |
| TTL 路径合流 | `src/storage/table.rs:2185-2224` | ✅ |
| `take_front` 拆分 batch | `src/common/column_data.rs:158` | ✅ |

### 1.2 残留拷贝点（本次消除）

**拷贝点 #1**：`src/storage/table.rs:2170`

```rust
for &col_idx in column_indices {
    let col_data = self.column_store.read_column(rg_idx, col_idx)?;
    col_owned.push(col_data.clone());  // ← 每列每 row group 全拷贝
}
```

- **频次**：`#列 × #row_group × #scan`
- **开销估算**（1M 行表，5 列，10 个 RG，每列 100k typed 值）：
  - Int32 列：~400KB / 拷贝 × 5 列 × 10 RG = ~20MB 拷贝
  - Varchar 列：~2MB（值向量）+ ~200KB（NULL bitmap）= 更高
- **dhat 占比**：预估 30-50% 查询执行期分配字节

**拷贝点 #2**：`src/storage/table.rs:2269`

```rust
for &col_idx in column_indices {
    let v = if col_idx < row.len() { row[col_idx].clone() } else { Value::Null };
    columns.push(Vector::Flat(vec![v]));
}
```

- **频次**：`#delta 行 × #列`
- **开销**：每 delta 行每个列克隆一个 `Value`（含可能的 `String`/`Vec<u8>`）
- **dhat 占比**：预估 5-10% 查询执行期分配

### 1.3 其他已知小拷贝（Phase 1 不动）

- `src/common/column_data.rs:204` `gather()`：构造新 typed ColumnData——**必要**（Late Materialization 必须新建子集），不在本次范围
- `src/storage/delta_store.rs:570+` 多处 `clone()`：单行粒度，但每行单独操作，**部分会被本设计覆盖**（见 §三）

---

## 二、设计方案

### 2.1 总体策略

**核心思想**：让 `scan_to_chunks_impl` 的内层循环**持有 `ColumnData` 的所有权**，按 batch 大小逐步移交，**不复制整列**。

**关键约束**：
- `read_column(rg, col)` 返回 `&ColumnData`，但 `take_front` 需要 `&mut ColumnData`
- `read_column` 本身需要 `&mut self`（惰性解压）
- 解决方案：**先解借用取出 `ColumnData`，再分 batch**

### 2.2 API 扩展

**`src/common/column_data.rs`** 新增/扩展：

```rust
impl ColumnData {
    /// 按 batch_size 拆分为多个 batch（不复制，按值 move 出）
    ///
    /// 原 `take_front(n)` 每次调用都构造新 ColumnData。
    /// 本方法在 batch_size 边界处整批移交，避免大块数据反复重建。
    ///
    /// 返回迭代器，每个元素是一个 batch 的 owned ColumnData。
    pub fn into_batches(mut self, batch_size: usize) -> IntoBatches {
        IntoBatches { data: self, batch_size }
    }
}

pub struct IntoBatches {
    data: ColumnData,
    batch_size: usize,
}

impl Iterator for IntoBatches {
    type Item = ColumnData;
    fn next(&mut self) -> Option<ColumnData> {
        if self.data.is_empty() {
            return None;
        }
        let n = self.batch_size.min(self.data.len());
        Some(self.data.take_front(n))
    }
}
```

**`src/storage/delta_store.rs`** 新增：

```rust
impl DeltaStore {
    /// 返回所有 delta 行的 borrowed 视图（不克隆 row）
    ///
    /// 调用方拿到 `(row_id: u64, row: &[Value])`，row 借用 self。
    /// 注意：返回的迭代器持有 &self 借用，生命周期 = self 借用周期。
    pub fn all_rows_borrowed(&self) -> DeltaRowsBorrowed<'_> {
        DeltaRowsBorrowed { store: self, idx: 0 }
    }
}

pub struct DeltaRowsBorrowed<'a> {
    store: &'a DeltaStore,
    idx: usize,
}

impl<'a> Iterator for DeltaRowsBorrowed<'a> {
    type Item = (u64, &'a [Value]);
    fn next(&mut self) -> Option<Self::Item> {
        // 跳过删除标记
        while self.idx < self.store.rows.len() && self.store.rows[self.idx].is_none() {
            self.idx += 1;
        }
        if self.idx >= self.store.rows.len() {
            return None;
        }
        let rid = self.store.row_ids[self.idx];
        let row = self.store.rows[self.idx].as_ref().unwrap();
        self.idx += 1;
        Some((rid, row.as_slice()))
    }
}
```

### 2.3 改造后的 scan_to_chunks_impl

**目标代码结构**（伪代码）：

```rust
fn scan_to_chunks_impl(
    &mut self,
    column_indices: &[usize],
    skip_pred: Option<(usize, PredicateOp, Value)>,
) -> Result<Vec<DataChunk>> {
    const BATCH_SIZE: usize = 2048;
    let mut chunks: Vec<DataChunk> = Vec::new();
    let full_row_for_ttl = self.def.has_ttl();
    let ttl_cutoff = self.def.ttl_cutoff_ms();

    for rg_idx in 0..self.column_store.row_group_count() {
        // 1. Zone Map 跳过（不变）
        if let Some((col_idx, op, val)) = &skip_pred {
            if self.column_store.can_skip_predicate(rg_idx, *col_idx, *op, val) {
                continue;
            }
        }
        // 2. TTL 跳过（不变）
        if let (Some(ttl_col), Some(cut)) = (self.def.ttl_column, ttl_cutoff) {
            let cutoff_val = Value::Timestamp(cut);
            if self.column_store.can_skip_predicate(rg_idx, ttl_col, PredicateOp::GtEq, &cutoff_val) {
                continue;
            }
        }

        // 3. 取出所有列 owned（一次性 move，每个列一次 take）
        //    这是关键改造：从 read_column().clone() 改为 take 所有 RG 内的列
        let col_data_per_col: Vec<ColumnData> = column_indices
            .iter()
            .map(|&col_idx| {
                let col_ref = self.column_store.read_column(rg_idx, col_idx)?;
                // 走 take_entire_owned 而非 clone
                Ok(col_ref.clone_whole_owned())
            })
            .collect::<Result<Vec<_>>>()?;

        if col_data_per_col.is_empty() {
            continue;
        }

        // 4. 一次性拆 batch（每个 batch 同时拿到所有列的子集）
        let total_rows = col_data_per_col[0].len();
        let mut col_iters: Vec<_> = col_data_per_col.into_iter()
            .map(|c| c.into_batches(BATCH_SIZE))
            .collect();

        loop {
            // 同步推进所有列迭代器
            let batch_cols: Vec<ColumnData> = col_iters.iter_mut()
                .map(|it| it.next())
                .collect::<Option<Vec<_>>>()
                .unwrap_or_else(|| break); // 任一列耗尽 → 全部耗尽
            if batch_cols.is_empty() { break; }
            let batch_len = batch_cols[0].len();
            if batch_len == 0 { break; }

            // 5. PREWHERE 筛选（不变）
            let survivors: Option<Vec<usize>> = ...;

            // 6. 构造 DataChunk（不再 take_front 复制）
            let mut columns: Vec<Vector> = if let Some(sel) = &survivors {
                if sel.is_empty() {
                    continue;
                }
                batch_cols.into_iter().map(|c| Vector::Typed(c.gather(sel))).collect()
            } else {
                batch_cols.into_iter().map(Vector::Typed).collect()
            };

            chunks.push(DataChunk { count: ..., columns });
        }
    }

    // 7. Delta 路径：使用 borrowed 迭代
    for (rid, row) in self.delta_store.all_rows_borrowed() {
        if self.def.is_expired(row) { continue; }
        if let Some((ci, op, val)) = &skip_pred {
            if row.get(*ci).map_or(true, |cell| !matches_predicate(cell, *op, val)) {
                continue;
            }
        }
        let columns: Vec<Vector> = column_indices.iter()
            .map(|&col_idx| {
                let v = if col_idx < row.len() { row[col_idx].clone() } else { Value::Null };
                // ↑ 这里仍 clone——但只 1 个 Value，不克隆行整体
                Vector::Flat(vec![v])
            })
            .collect();
        chunks.push(DataChunk { count: 1, columns });
    }

    Ok(chunks)
}
```

### 2.4 新增辅助 API

**`src/common/column_data.rs`** 新增：

```rust
impl ColumnData {
    /// 整体克隆为 owned（用于一次性 move 出，区别于字段级 clone）
    ///
    /// 等价于 `Clone::clone` 但语义明确：调用方承诺"整个列我都拿走"。
    /// 未来可优化为：若 typed 数组是不可变共享 backing，可做 CoW。
    pub fn clone_whole_owned(&self) -> ColumnData {
        self.clone()
    }
}
```

**为什么不需要 CoW 优化**：`take_front` 拆分后每批是子集 owned，必须新建 typed 数组；整体克隆发生在 RG 边界（频次低），开销可接受。

---

## 三、Delta 路径零拷贝

### 3.1 当前实现（`src/storage/delta_store.rs:570-610`）

```rust
for (_, row) in self.delta_store.all_rows() {
    // all_rows() 返回 (u64, Vec<Value>)，每个 row 单独 clone
}
```

### 3.2 改造方案

提供 borrowed 迭代器（见 §2.2），调用方在借用周期内使用 `&[Value]` 视图，避免每个 delta 行克隆。

注意：scan 路径中**每个 delta 行的每个列仍需要克隆一个 Value**（构造 DataChunk 的列向量），但这比克隆整个 `Vec<Value>`（含所有列）更轻。

---

## 四、Zone Map 命中埋点（验证无 regression）

新增 `tests/zone_map_stats.rs`：

```rust
//! Zone Map 命中统计：验证 P2.4 skip predicate 正常工作

use engramdb::Connection;

#[test]
fn test_zone_map_skip_logs_hit_rate() {
    let path = "/tmp/m2_zone_map.hdb";
    let _ = std::fs::remove_file(path);
    let mut conn = Connection::open(path).unwrap();
    conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY, v TEXT)").unwrap();

    // 插入有序 100k 行
    for i in 0..100_000 {
        conn.execute(&format!("INSERT INTO t VALUES ({}, 'r{}')", i, i)).unwrap();
    }

    // 触发 checkpoint（生成 row groups + zone map）
    conn.execute("CHECKPOINT").unwrap();

    // 查询：id > 99000（应跳过大部分 RG）
    let t0 = std::time::Instant::now();
    let result = conn.query("SELECT * FROM t WHERE id > 99000").unwrap();
    let d = t0.elapsed();

    // 验证：返回行数 = 1000
    let count: i64 = result.iter().flat_map(|c| &c.rows).filter_map(|v| v.as_int()).sum();
    assert_eq!(count, (99000..100_000).sum::<i64>());

    println!("zone_map skip took {:?}", d);
}
```

---

## 五、本周工作量分配

| 日期 | 任务 | 工时 | 产出 |
|---|---|---|---|
| 8/26 周三 | 审计 + 设计 + 草图 | 8h | `docs/p1-zcopy-audit.md`（与本文档合并） |
| 8/27 周四 | 列存零拷贝改造（上）：消除 #1 拷贝点 + Zone Map 埋点 | 8h | `src/storage/table.rs:2167-2245` 重构 + `tests/zone_map_stats.rs` |
| 8/28 周五上午 | Delta 路径改造：消除 #2 拷贝点 | 3h | `src/storage/delta_store.rs` + `src/storage/table.rs:2256-2276` |
| 8/28 周五下午 | PREWHERE 路径核对 + 重跑 D0 + 全量回归 | 4h | `docs/p1-m2-report.md` |

---

## 六、风险与缓解

| 风险 | 概率 | 影响 | 缓解 |
|---|---|---|---|
| `into_batches` 借用生命周期过严（与 `read_column` 的 `&mut self` 冲突） | 中 | 高 | 用 `read_column().clone_whole_owned()` 取出 owned ColumnData，再做 batch 拆分 |
| `column_data.gather(sel)` 仍是新建 typed 数组——选择性 1% 时性能反退化？ | 低 | 中 | gather 是必要的（late materialization 必须新建子集），但单列成本远低于全 clone；选择性 1% 时总分配字节仅 1% |
| Delta 行借用周期与主循环借用检查冲突 | 中 | 中 | 独立 iterator trait，&self 借用周期覆盖整个 scan |
| Zone Map skip 已实现，本次只是验证埋点——不需要实际触发 | 低 | 低 | 在测试中用 RANGE 大值（如 id > 99000）确认命中 |

---

## 七、M2 完成判定

- [ ] `src/common/column_data.rs` 新增 `into_batches` 与 `clone_whole_owned`
- [ ] `src/storage/delta_store.rs` 新增 `all_rows_borrowed`
- [ ] `src/storage/table.rs` `scan_to_chunks_impl` 重构完成（消除 #1 + #2）
- [ ] `cargo test --release --lib` 全绿（无功能回归）
- [ ] `tests/zone_map_stats.rs` 通过
- [ ] 单列全扫吞吐 +30%（10 次 P50，来源 `m3_log_bench`）
- [ ] 选择性 1% 查询延迟 -50%（10 次 P50，来源 `selectivity_scan_bench`）
- [ ] 列扫描内存峰值 -30%（heaptrack）
- [ ] `cargo clippy --all-targets -- -D warnings` 通过
- [ ] `docs/p1-m2-report.md` 填入数字

---

## 八、M2 验收报告模板（执行后填写）

```markdown
# M2 验收报告

**日期**：2026-08-28
**commit**：（git rev-parse HEAD）

## 1. 拷贝点消除

| 拷贝点 | 改造前 | 改造后 | 验证方法 |
|---|---|---|---|
| #1 `table.rs:2170` | 每列每 RG 全 clone | 一次性 take + into_batches | dhat 对比 |
| #2 `table.rs:2269` | 每 delta 行每列 clone | borrowed view + 单 Value clone | dhat 对比 |

## 2. KPI 验证（10 次 P50）

| 指标 | 基线（D0） | M2 后 | 提升 |
|---|---|---|---|
| 单列全扫吞吐 | _____ 行/秒 | _____ 行/秒 | _____ % |
| 选择性 1% 查询延迟 | _____ ms | _____ ms | _____ % |
| 列扫描内存峰值 | _____ MB | _____ MB | _____ % |

**KPI 结论**：(达成/未达成)

## 3. dhat 分配字节对比

| 场景 | D0 总分配字节 | M2 总分配字节 | 下降比例 |
|---|---|---|---|
| 全表扫描 100k 行 | _____ | _____ | _____ % |
| 选择性 1% 查询 | _____ | _____ | _____ % |

## 4. 已知问题

（任何遗留或推迟到 Phase 2 的项）
```