//! 事务管理模块
//!
//! 完整的 ACID 事务实现：
//! - A (Atomicity): WAL + Undo 保证原子性
//! - C (Consistency): 约束检查 + 事务正确执行
//! - I (Isolation): MVCC 快照隔离
//! - D (Durability): WAL fsync 保证持久化

pub mod transaction;
pub mod mvcc;
pub mod manager;

use crate::common::error::Result;

pub use mvcc::{Timestamp, TxnId, MvccStore, ActiveTxnTable, Snapshot};
pub use manager::TransactionManager;
pub use transaction::Transaction;

/// 事务状态
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxnState {
    Active,
    Committed,
    RolledBack,
}

// 使用 config 模块中的 IsolationLevel 定义
pub use crate::common::config::IsolationLevel;

/// 事务错误
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TxnError {
    /// 写-写冲突
    WriteConflict(String),
    /// 事务已提交/回滚，不能再操作
    InvalidState(String),
    /// 事务 ID 不存在
    NotFound(u32),
}

impl std::fmt::Display for TxnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TxnError::WriteConflict(msg) => write!(f, "Write conflict: {}", msg),
            TxnError::InvalidState(msg) => write!(f, "Invalid transaction state: {}", msg),
            TxnError::NotFound(id) => write!(f, "Transaction not found: {}", id),
        }
    }
}

impl std::error::Error for TxnError {}

/// 事务结果
pub type TxnResult<T> = std::result::Result<T, TxnError>;

/// 待应用操作（MVCC 提交后应用到存储层）
///
/// 设计背景：`TransactionManager.commit()` 时需要将 MVCC 版本应用到存储层，
/// 但 Rust 借用规则不允许同时持有 `&mut TransactionManager` 和 `&mut Database`。
/// 解决方案：`commit()` 返回待应用操作，由 executor 调用存储层。
#[derive(Debug, Clone)]
pub enum ApplyOp {
    Insert {
        table_id: u32,
        row_id: u64,
        row: Vec<crate::Value>,
    },
    Update {
        table_id: u32,
        row_id: u64,
        new_row: Vec<crate::Value>,
    },
    Delete {
        table_id: u32,
        row_id: u64,
    },
}

/// 提交结果
#[derive(Debug)]
pub struct CommitResult {
    /// 提交时间戳
    pub commit_ts: Timestamp,
    /// 待应用到存储层的操作列表
    pub apply_ops: Vec<ApplyOp>,
}

// 确保 Result 类型兼容
impl From<TxnError> for crate::common::error::HybridDbError {
    fn from(e: TxnError) -> Self {
        crate::common::error::HybridDbError::Transaction(e.to_string())
    }
}
