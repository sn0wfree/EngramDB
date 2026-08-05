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

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::Value;

/// 单表攒批状态
struct BatchedRows {
    rows: Vec<Vec<Value>>,
    /// 估算字节数（行内值个数）
    bytes: usize,
    /// 首行入批时刻（时间窗从首行起算）
    first_ts: Instant,
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
            });
        entry.bytes += rows.iter().map(|r| r.len()).sum::<usize>();
        let total = entry.rows.len() + rows.len();
        entry.rows.extend(rows);
        total >= self.max_rows
            || entry.bytes >= self.max_bytes
            || entry.first_ts.elapsed() >= self.timeout
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
