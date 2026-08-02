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

/// 事务隔离级别
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsolationLevel {
    ReadUncommitted,
    ReadCommitted,
    RepeatableRead,
    SnapshotIsolation,
    Serializable,
}

impl Default for IsolationLevel {
    fn default() -> Self {
        IsolationLevel::SnapshotIsolation
    }
}

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

// 确保 Result 类型兼容
impl From<TxnError> for crate::common::error::HybridDbError {
    fn from(e: TxnError) -> Self {
        crate::common::error::HybridDbError::Transaction(e.to_string())
    }
}
