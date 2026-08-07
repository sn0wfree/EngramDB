//! INSERT 攒批合并器（v0.18 P0-2：Ingest Buffer / Batcher）
//!
//! autocommit 逐行 INSERT 场景：每行一个事务（begin → WAL → commit → fsync）
//! 开销巨大。Batcher 在 executor 层把连续 INSERT 攒进内存缓冲，达到阈值
//! （行数 / 字节数 / 时间窗任一满足）后一次性走 `batch_insert`：
//! 一批 = 1 条 WAL InsertBatch 记录 + 1 次组提交计数 + 1 次 MVCC 批量写。
//!
//! 语义边界：
//! - 仅作用于 autocommit 路径（`insert::execute`）；显式事务（`Transaction`）
//!   持 `&mut Database`，与 executor 互斥，天然隔离
//! - 缓冲行在落盘前对其他语句不可见 → 所有非裸 INSERT 语句执行前必须
//!   `flush_all_batched`（读己之写 + 语句间顺序）
//! - 崩溃时丢失缓冲行 = 与 WAL 组提交窗口一致的异步窗口（`close` 时兜底 flush）
//! - INSERT ... RETURNING 绕过 batcher（需立即读回插入行）

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use crate::Value;

/// 单表攒批状态
struct BatchedRows {
    rows: Vec<Vec<Value>>,
    /// 估算字节数（行内值个数）
    bytes: usize,
    /// 首行入批时刻（时间窗从首行起算）
    first_ts: Instant,
    /// 批内约束键 seen-set（v0.20：约束表攒批入批预检用，O(1) 判重）
    /// 主键 seen（自动分配的自增值在 flush 才分配，入批跳过 NULL）
    pk_seen: HashSet<Value>,
    /// 唯一索引 seen（index name → 批内已见键）
    unique_seen: HashMap<String, HashSet<Value>>,
}

/// INSERT 批处理器（Database 内嵌，单线程 &mut 模型无需锁）
pub struct InsertBatcher {
    buffers: HashMap<String, BatchedRows>,
    max_rows: usize,
    max_bytes: usize,
    timeout: Duration,
}

impl InsertBatcher {
    pub fn new(max_rows: usize, max_bytes: usize, timeout_ms: u64) -> Self {
        Self {
            buffers: HashMap::new(),
            max_rows,
            max_bytes,
            timeout: Duration::from_millis(timeout_ms),
        }
    }

    /// 攒入一批行；返回 true 表示已达阈值，调用方应 drain + flush
    pub fn push(&mut self, table_name: &str, rows: Vec<Vec<Value>>) -> bool {
        if rows.is_empty() {
            return false;
        }
        let entry = self
            .buffers
            .entry(table_name.to_string())
            .or_insert_with(|| BatchedRows {
                rows: Vec::new(),
                bytes: 0,
                first_ts: Instant::now(),
                pk_seen: HashSet::new(),
                unique_seen: HashMap::new(),
            });
        entry.bytes += rows.iter().map(|r| r.len()).sum::<usize>();
        let total = entry.rows.len() + rows.len();
        entry.rows.extend(rows);
        total >= self.max_rows
            || entry.bytes >= self.max_bytes
            || entry.first_ts.elapsed() >= self.timeout
    }

    /// 攒入一批行并做约束预检（v0.20：约束表攒批用）
    ///
    /// 入批时即校验主键（批内 seen + 由调用方做已提交点查）与唯一索引
    /// 批内自重复；冲突返回 Err 且**不落批**（零副作用），错误在该语句
    /// 返回时暴露（与绕过攒批时的语义一致）。
    ///
    /// `pk_col`：主键列索引（auto_increment 主键由调用方决定是否跳过 NULL）。
    /// `unique_cols`：(索引名, 键列索引) 列表。
    pub fn push_checked(
        &mut self,
        table_name: &str,
        rows: Vec<Vec<Value>>,
        pk_col: Option<usize>,
        unique_cols: &[(String, usize)],
    ) -> crate::common::error::Result<bool> {
        use crate::common::error::EngramDbError;
        if rows.is_empty() {
            return Ok(false);
        }
        let entry = self
            .buffers
            .entry(table_name.to_string())
            .or_insert_with(|| BatchedRows {
                rows: Vec::new(),
                bytes: 0,
                first_ts: Instant::now(),
                pk_seen: HashSet::new(),
                unique_seen: HashMap::new(),
            });
        // 两阶段：先全量校验（批内自重复 + 与已攒行重复），全部通过再落批，
        // 避免中途失败留下脏 seen（零副作用语义）
        let mut local_pk: HashSet<Value> = HashSet::new();
        let mut local_unique: HashMap<&str, HashSet<Value>> = HashMap::new();
        for row in &rows {
            if let Some(pk) = pk_col {
                if let Some(cell) = row.get(pk) {
                    if !cell.is_null()
                        && (entry.pk_seen.contains(cell) || !local_pk.insert(cell.clone()))
                    {
                        return Err(EngramDbError::ConstraintViolation(format!(
                            "UNIQUE constraint failed: pk={:?}", cell
                        )));
                    }
                }
            }
            for (idx_name, key_col) in unique_cols {
                if let Some(cell) = row.get(*key_col) {
                    let entry_seen = entry.unique_seen.contains_key(idx_name)
                        && entry.unique_seen[idx_name].contains(cell);
                    let local_seen = local_unique.entry(idx_name).or_default();
                    if entry_seen || !local_seen.insert(cell.clone()) {
                        return Err(EngramDbError::ConstraintViolation(format!(
                            "UNIQUE constraint failed: index '{}'", idx_name
                        )));
                    }
                }
            }
        }
        // 全部通过：提交 seen + 落批
        entry.pk_seen.extend(local_pk.into_iter());
        for (idx_name, keys) in local_unique {
            entry.unique_seen.entry(idx_name.to_string()).or_default().extend(keys);
        }
        entry.bytes += rows.iter().map(|r| r.len()).sum::<usize>();
        let total = entry.rows.len() + rows.len();
        entry.rows.extend(rows);
        Ok(total >= self.max_rows
            || entry.bytes >= self.max_bytes
            || entry.first_ts.elapsed() >= self.timeout)
    }

    /// 某表当前缓冲行（v0.20 攒批入批预检用；冲突点查需与已攒行对比）
    pub fn pending_rows(&self, table_name: &str) -> &[Vec<Value>] {
        self.buffers.get(table_name).map(|e| e.rows.as_slice()).unwrap_or(&[])
    }

    /// 取走某表全部缓冲（清零该表）
    pub fn drain(&mut self, table_name: &str) -> Vec<Vec<Value>> {
        self.buffers.remove(table_name).map(|e| e.rows).unwrap_or_default()
    }

    /// 取走全部表的缓冲（非 INSERT 语句 / close / checkpoint 前置）
    pub fn drain_all(&mut self) -> Vec<(String, Vec<Vec<Value>>)> {
        self.buffers
            .drain()
            .map(|(t, e)| (t, e.rows))
            .collect()
    }

    /// 当前缓冲行数（监控用）
    pub fn pending(&self) -> usize {
        self.buffers.values().map(|e| e.rows.len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.buffers.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(v: i64) -> Vec<Vec<Value>> {
        vec![vec![Value::Int64(v)]]
    }

    #[test]
    fn push_row_threshold_trigger() {
        // 行阈值：total >= max_rows 时触发（max_rows=4）
        let mut b = InsertBatcher::new(4, 1024, 10_000);
        assert!(!b.push("t", row(1)));
        assert!(!b.push("t", row(2)));
        assert!(!b.push("t", row(3)));
        assert!(b.push("t", row(4)), "累计行数达到阈值应触发");
        assert_eq!(b.pending(), 4, "触发后缓冲仍保留（drain 前）");
    }

    #[test]
    fn push_batch_spans_threshold() {
        // 单次 push 多行跨过阈值：立即触发（4 行 > max_rows=3）
        let mut b = InsertBatcher::new(3, 1024, 10_000);
        let rows = vec![
            vec![Value::Int64(1)],
            vec![Value::Int64(2)],
            vec![Value::Int64(3)],
            vec![Value::Int64(4)],
        ];
        assert!(b.push("t", rows));
    }

    #[test]
    fn push_empty_no_op() {
        let mut b = InsertBatcher::new(1, 1, 1);
        assert!(!b.push("t", Vec::new()), "空行集不得触发");
        assert!(b.is_empty(), "空行集不得改变状态");
    }

    #[test]
    fn push_byte_threshold_trigger() {
        // 字节阈值：值为个数（每行 2 值），max_bytes=3 时两行触发
        let mut b = InsertBatcher::new(1000, 3, 10_000);
        let two = vec![vec![Value::Int64(1), Value::Int64(2)]];
        assert!(!b.push("t", two));
        assert!(b.push("t", row(3)), "字节累计达到阈值应触发");
    }

    #[test]
    fn push_timeout_trigger() {
        // 时间阈值：窗口从首行起算，sleep 后任意 push 触发
        let mut b = InsertBatcher::new(1000, 1024, 1);
        assert!(!b.push("t", row(1)));
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert!(b.push("t", row(2)), "时间窗到期后应触发");
    }

    #[test]
    fn multi_table_isolated() {
        // 多表隔离：各表独立计数与缓冲（全局阈值 3）
        let mut b = InsertBatcher::new(3, 1024, 10_000);
        assert!(!b.push("a", row(1)));
        assert!(!b.push("a", row(2)), "a 表未满");
        assert!(!b.push("b", row(1)), "b 表写入不影响 a 表计数");
        assert!(b.push("a", row(3)), "a 表满 3 行触发");
        assert_eq!(b.pending(), 4, "b 表 1 行仍缓冲");
        assert_eq!(b.drain("b").len(), 1, "b 表未触发，缓冲保留");
    }

    #[test]
    fn drain_single_table() {
        let mut b = InsertBatcher::new(4, 1024, 10_000);
        b.push("t", row(1));
        b.push("t", row(2));
        let rows = b.drain("t");
        assert_eq!(rows.len(), 2);
        assert!(b.is_empty(), "drain 后应清空该表");
        assert!(b.drain("t").is_empty(), "重复 drain 得空");
        assert!(b.drain("absent").is_empty(), "未知表得空");
    }

    #[test]
    fn drain_all_tables() {
        let mut b = InsertBatcher::new(100, 1024, 10_000);
        b.push("a", row(1));
        b.push("b", row(2));
        b.push("b", row(3));
        let all = b.drain_all();
        assert_eq!(all.len(), 2);
        assert!(all.iter().any(|(t, r)| t == "a" && r.len() == 1));
        assert!(all.iter().any(|(t, r)| t == "b" && r.len() == 2));
        assert!(b.is_empty());
        assert!(b.drain_all().is_empty(), "重复 drain_all 得空");
    }

    #[test]
    fn pending_counts_rows() {
        let mut b = InsertBatcher::new(10, 1024, 10_000);
        assert_eq!(b.pending(), 0);
        b.push("a", row(1));
        b.push("a", row(2));
        b.push("b", vec![row(3).pop().unwrap(), row(4).pop().unwrap()]);
        assert_eq!(b.pending(), 4);
    }
}
