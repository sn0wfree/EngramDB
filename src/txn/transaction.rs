//! 事务句柄
//!
//! 提供面向用户的事务 API，内部委托给 TransactionManager
//!
//! 使用方式：
//! ```ignore
//! let mut txn = db.begin().unwrap();
//! txn.insert("table1", rows).unwrap();
//! txn.commit().unwrap();
//! ```

use crate::common::error::{EngramDbError, Result};
use crate::storage::Database;
use crate::Value;

use super::{TxnState, IsolationLevel, TxnId};

/// 事务句柄
///
/// 持有对 Database 的可变引用，通过事务管理器操作数据
pub struct Transaction<'a> {
    id: TxnId,
    db: &'a mut Database,
    /// 是否只读事务（v0.15.0 Txn09）
    read_only: bool,
}

impl<'a> Transaction<'a> {
    /// 开始一个新事务（由 Database 调用）
    pub(crate) fn begin(db: &'a mut Database, isolation_level: IsolationLevel) -> Result<Self> {
        let txn_id = db.txn_manager_mut().begin(isolation_level)?;
        Ok(Self { id: txn_id, db, read_only: false })
    }

    /// 开始一个只读事务（跳过 WAL，v0.15.0 Txn09）
    pub(crate) fn begin_readonly(db: &'a mut Database, isolation_level: IsolationLevel) -> Result<Self> {
        let txn_id = db.txn_manager_mut().begin_readonly(isolation_level)?;
        Ok(Self { id: txn_id, db, read_only: true })
    }

    /// 是否为只读事务
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// 提交事务
    pub fn commit(self) -> Result<()> {
        let txn_id = self.id;
        let _result = self.db.txn_manager_mut().commit(txn_id)?;
        // 忽略 CommitResult，apply_ops 将在其他路径处理
        Ok(())
    }

    /// 回滚事务
    pub fn rollback(self) -> Result<()> {
        let txn_id = self.id;
        self.db.txn_manager_mut().rollback(txn_id)?;
        Ok(())
    }

    /// 执行插入（在事务内）
    pub fn insert(&mut self, table_name: &str, rows: Vec<Vec<Value>>) -> Result<u64> {
        let table_id = self.db.get_table(table_name)
            .map(|t| t.def.id)
            .ok_or_else(|| EngramDbError::TableNotFound(table_name.into()))?;

        let txn_id = self.id;
        let mut rowid_start = 0u64;

        // 逐行插入（MVP 简化，实际应该批量）
        for (i, row) in rows.iter().enumerate() {
            let rowid = (i + 1) as u64; // 简化 rowid 分配
            if i == 0 { rowid_start = rowid; }
            self.db.txn_manager_mut().insert(txn_id, table_id, rowid, row.clone())?;
        }

        Ok(rows.len() as u64)
    }

    /// 获取事务 ID
    pub fn id(&self) -> TxnId {
        self.id
    }

    /// 获取事务状态
    pub fn state(&self) -> Option<TxnState> {
        self.db.txn_manager().state(self.id)
    }
}

// Drop 时自动回滚（如果还在 Active 状态）
impl<'a> Drop for Transaction<'a> {
    fn drop(&mut self) {
        // 如果事务还在活跃状态，自动回滚
        // 注意：drop 中不能保证成功，但尽量回滚
        if matches!(self.db.txn_manager().state(self.id), Some(TxnState::Active)) {
            let _ = self.db.txn_manager_mut().rollback(self.id);
        }
    }
}
