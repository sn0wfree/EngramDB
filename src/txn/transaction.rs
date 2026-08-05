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

use std::collections::HashMap;

use crate::common::error::{EngramDbError, Result};
use crate::executor::operators::insert::apply_to_storage;
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
    /// 每个表的 rowid 分配游标（事务内单调递增，避免 rowid 冲突/覆盖）
    next_rowids: HashMap<u32, u64>,
}

impl<'a> Transaction<'a> {
    /// 开始一个新事务（由 Database 调用）
    pub(crate) fn begin(db: &'a mut Database, isolation_level: IsolationLevel) -> Result<Self> {
        let txn_id = db.txn_manager_mut().begin(isolation_level)?;
        Ok(Self { id: txn_id, db, read_only: false, next_rowids: HashMap::new() })
    }

    /// 开始一个只读事务（跳过 WAL，v0.15.0 Txn09）
    pub(crate) fn begin_readonly(db: &'a mut Database, isolation_level: IsolationLevel) -> Result<Self> {
        let txn_id = db.txn_manager_mut().begin_readonly(isolation_level)?;
        Ok(Self { id: txn_id, db, read_only: true, next_rowids: HashMap::new() })
    }

    /// 是否为只读事务
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// 提交事务
    ///
    /// 注意：commit 后事务状态变为 Committed，Drop 不会再回滚。
    pub fn commit(&mut self) -> Result<()> {
        let txn_id = self.id;
        let result = self.db.txn_manager_mut().commit(txn_id)?;
        // P1.5 修复：之前丢弃 apply_ops 导致数据只停留在 MVCC/WAL，
        // 必须将写操作应用到存储层（否则读取路径看不到提交的数据）
        apply_to_storage(self.db, result.apply_ops)?;
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
        // 注意：表的权威 ID 在 table_names 映射中（TableDef.id 未由 create_table 回填）
        let (table_id, base_row_count) = self.db.table_names().get(table_name)
            .and_then(|id| self.db.get_engine_table(table_name).map(|t| (*id, t.def().row_count)))
            .ok_or_else(|| EngramDbError::TableNotFound(table_name.into()))?;

        let txn_id = self.id;
        // P1.5 修复：rowid 必须从表当前行数开始分配（原来固定从 1 开始，
        // 会导致与已有行冲突，且多次 insert 之间互相覆盖成 Update）
        let base = *self.next_rowids
            .entry(table_id)
            .or_insert(base_row_count);
        let rows_len = rows.len();

        for (i, row) in rows.into_iter().enumerate() {
            let rowid = base + i as u64;
            self.db.txn_manager_mut().insert(txn_id, table_id, rowid, row)?;
        }

        if let Some(cursor) = self.next_rowids.get_mut(&table_id) {
            *cursor = base + rows_len as u64;
        }

        Ok(rows_len as u64)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Connection;

    #[test]
    fn test_txn_insert_commit_persists() {
        // P1.5 回归：commit 必须把 apply_ops 应用到存储层（之前数据只停留在 MVCC/WAL）
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (id INT)").unwrap();

        let mut txn = conn.begin().unwrap();
        txn.insert("t", vec![vec![Value::Int64(1)], vec![Value::Int64(2)]]).unwrap();
        txn.commit().unwrap();
        drop(txn);

        let count = conn.execute("SELECT COUNT(*) FROM t").unwrap();
        assert_eq!(count.rows[0][0], Value::Int64(2));
    }

    #[test]
    fn test_txn_multiple_inserts_no_overwrite() {
        // P1.5 回归：同事务内多次 insert 不应互相覆盖（rowid 必须单调递增）
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (id INT)").unwrap();

        let mut txn = conn.begin().unwrap();
        txn.insert("t", vec![vec![Value::Int64(1)]]).unwrap();
        txn.insert("t", vec![vec![Value::Int64(2)]]).unwrap();
        txn.commit().unwrap();
        drop(txn);

        let count = conn.execute("SELECT COUNT(*) FROM t").unwrap();
        assert_eq!(count.rows[0][0], Value::Int64(2));

        let rows = conn.execute("SELECT id FROM t").unwrap();
        assert_eq!(rows.rows.len(), 2);
    }

    #[test]
    fn test_txn_rowid_continues_after_commit() {
        // P1.5 回归：新事务的 rowid 应从表当前行数继续，不与已有行冲突
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (id INT)").unwrap();
        conn.execute("INSERT INTO t VALUES (100)").unwrap();

        let mut txn = conn.begin().unwrap();
        txn.insert("t", vec![vec![Value::Int64(200)]]).unwrap();
        txn.commit().unwrap();
        drop(txn);

        let count = conn.execute("SELECT COUNT(*) FROM t").unwrap();
        assert_eq!(count.rows[0][0], Value::Int64(2));
    }

    #[test]
    fn test_txn_rollback_discards() {
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (id INT)").unwrap();

        let mut txn = conn.begin().unwrap();
        txn.insert("t", vec![vec![Value::Int64(1)]]).unwrap();
        txn.rollback().unwrap();

        let count = conn.execute("SELECT COUNT(*) FROM t").unwrap();
        assert_eq!(count.rows[0][0], Value::Int64(0));
    }

    // ========================================================================
    // P-W2：事务内批量 INSERT 接线
    // ========================================================================

    #[test]
    fn test_txn_batch_insert_commit_persists() {
        // P-W2a：单事务批量 100 行 → commit 后全部持久化
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (id INT, val INT)").unwrap();

        let mut txn = conn.begin().unwrap();
        let mut rows = Vec::with_capacity(100);
        for i in 0..100 {
            rows.push(vec![Value::Int64(i), Value::Int64(i * 2)]);
        }
        txn.insert("t", rows).unwrap();
        txn.commit().unwrap();
        drop(txn);

        let count = conn.execute("SELECT COUNT(*) FROM t").unwrap();
        assert_eq!(count.rows[0][0], Value::Int64(100));

        // 抽查数据
        let r = conn.execute("SELECT id, val FROM t WHERE id = 50").unwrap();
        assert_eq!(r.rows.len(), 1);
        assert_eq!(r.rows[0][0], Value::Int64(50));
        assert_eq!(r.rows[0][1], Value::Int64(100));
    }

    #[test]
    fn test_txn_batch_insert_rollback_discards() {
        // P-W2a：单事务批量 100 行 → rollback 后 0 行
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (id INT, val INT)").unwrap();

        let mut txn = conn.begin().unwrap();
        let mut rows = Vec::with_capacity(100);
        for i in 0..100 {
            rows.push(vec![Value::Int64(i), Value::Int64(i * 2)]);
        }
        txn.insert("t", rows).unwrap();
        txn.rollback().unwrap();

        let count = conn.execute("SELECT COUNT(*) FROM t").unwrap();
        assert_eq!(count.rows[0][0], Value::Int64(0));
    }

    #[test]
    fn test_txn_batch_insert_with_index() {
        // P-W2a + P-W2c：批量事务 + 二级索引维护正确
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (id INT, score INT)").unwrap();
        conn.execute("CREATE INDEX idx_score ON t (score)").unwrap();

        let mut txn = conn.begin().unwrap();
        let mut rows = Vec::with_capacity(50);
        for i in 0..50 {
            rows.push(vec![Value::Int64(i), Value::Int64(100 - i)]);
        }
        txn.insert("t", rows).unwrap();
        txn.commit().unwrap();
        drop(txn);

        // 通过索引查询
        let r = conn.execute("SELECT id FROM t WHERE score = 95").unwrap();
        assert_eq!(r.rows.len(), 1);
        assert_eq!(r.rows[0][0], Value::Int64(5));

        // 全量正确
        let count = conn.execute("SELECT COUNT(*) FROM t").unwrap();
        assert_eq!(count.rows[0][0], Value::Int64(50));
    }

    #[test]
    fn test_txn_batch_insert_sql_multi_values() {
        // P-W2a：SQL 多行 VALUES 在事务模式下走批量路径
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (id INT, name VARCHAR)").unwrap();

        // 多行 VALUES（走 Insert 计划 → execute_with_txn → batch_insert）
        conn.execute("INSERT INTO t VALUES (1, 'a'), (2, 'b'), (3, 'c'), (4, 'd')").unwrap();

        let count = conn.execute("SELECT COUNT(*) FROM t").unwrap();
        assert_eq!(count.rows[0][0], Value::Int64(4));

        let r = conn.execute("SELECT name FROM t WHERE id = 3").unwrap();
        assert_eq!(r.rows[0][0], Value::Varchar("c".into()));
    }

    #[test]
    fn test_txn_batch_insert_primary_key() {
        // P-W2a：批量事务 + 主键索引
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (id INT PRIMARY KEY, val INT)").unwrap();

        let mut txn = conn.begin().unwrap();
        let mut rows = Vec::with_capacity(20);
        for i in 0..20 {
            rows.push(vec![Value::Int64(i), Value::Int64(i * 3)]);
        }
        txn.insert("t", rows).unwrap();
        txn.commit().unwrap();
        drop(txn);

        // 主键点查
        let r = conn.execute("SELECT val FROM t WHERE id = 7").unwrap();
        assert_eq!(r.rows.len(), 1);
        assert_eq!(r.rows[0][0], Value::Int64(21));
    }
}
