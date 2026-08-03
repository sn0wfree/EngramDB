//! 事务管理器
//!
//! 全局事务协调者：
//! - 分配事务 ID 和时间戳
//! - 管理活跃事务表
//! - 协调 WAL 写入和 MVCC 版本提交
//! - 管理 Checkpoint

use std::collections::HashMap;
use std::path::PathBuf;

use crate::common::error::Result;
use crate::common::config::{Config, WalFlushMode};
use crate::wal::{WalWriter, WalRecordType, make_insert_payload, make_update_payload, make_delete_payload};
use crate::Value;

use super::{TxnState, IsolationLevel, TxnError, TxnId, Timestamp, MvccStore, ActiveTxnTable, ApplyOp, CommitResult};

/// 事务上下文（单个事务的状态）
#[derive(Debug)]
struct TxnContext {
    id: TxnId,
    state: TxnState,
    isolation_level: IsolationLevel,
    start_ts: Timestamp,
    /// 该事务写入的 key 列表（用于提交/回滚时处理）
    /// (table_id, rowid)
    write_set: Vec<(u32, u64)>,
}

/// 事务管理器
pub struct TransactionManager {
    /// 活跃事务表 + 时间戳分配
    active_table: ActiveTxnTable,
    /// MVCC 存储（按表分区: table_id -> MvccStore<Vec<Value>>）
    mvcc: HashMap<u32, MvccStore<Vec<Value>>>,
    /// WAL 写入器
    wal: WalWriter,
    /// 事务上下文表
    txns: HashMap<TxnId, TxnContext>,
    /// 数据库路径（用于 WAL 路径生成）
    #[allow(dead_code)]
    db_path: PathBuf,
}

impl TransactionManager {
    /// 创建事务管理器（带配置）
    pub fn new(db_path: &str, config: &Config) -> Result<Self> {
        let wal_path = format!("{}-wal", db_path);
        let wal = WalWriter::with_config(
            &wal_path,
            config.wal_flush_mode,
            config.wal_buffer_size,
            config.wal_group_commit_size,
            config.wal_group_commit_max_bytes,
        )?;

        Ok(Self {
            active_table: ActiveTxnTable::new(),
            mvcc: HashMap::new(),
            wal,
            txns: HashMap::new(),
            db_path: PathBuf::from(db_path),
        })
    }

    /// 开始一个新事务
    pub fn begin(&mut self, isolation_level: IsolationLevel) -> Result<TxnId> {
        let (txn_id, start_ts) = self.active_table.begin_txn();

        // 写入 WAL BEGIN 记录
        self.wal.write_record(WalRecordType::Begin, txn_id, 0, &[])?;

        let ctx = TxnContext {
            id: txn_id,
            state: TxnState::Active,
            isolation_level,
            start_ts,
            write_set: Vec::new(),
        };

        self.txns.insert(txn_id, ctx);
        Ok(txn_id)
    }

    /// 提交事务
    pub fn commit(&mut self, txn_id: TxnId) -> Result<CommitResult> {
        // 检查状态
        let ctx = self.txns.get_mut(&txn_id)
            .ok_or_else(|| TxnError::NotFound(txn_id))?;

        if ctx.state != TxnState::Active {
            return Err(TxnError::InvalidState(
                format!("Cannot commit: transaction {} is not active", txn_id)
            ).into());
        }

        ctx.state = TxnState::Committed;
        let write_set = std::mem::take(&mut ctx.write_set);

        // 写入 WAL COMMIT 记录并刷盘（根据配置的刷盘策略）
        self.wal.write_record(WalRecordType::Commit, txn_id, 0, &[])?;
        self.wal.commit_flush()?;

        // 获取 commit_ts
        let commit_ts = self.active_table.commit_txn(txn_id);

        // 提交 MVCC 版本
        for (table_id, _rowid) in &write_set {
            if let Some(store) = self.mvcc.get_mut(table_id) {
                store.commit_txn(txn_id, commit_ts);
            }
        }
        
        // 收集待应用操作（方案 B：返回 apply_ops，由 executor 应用到存储层）
        let apply_ops = self.collect_apply_ops(txn_id, commit_ts)?;

        Ok(CommitResult { commit_ts, apply_ops })
    }
    
    /// 收集待应用操作（内部辅助方法）
    fn collect_apply_ops(&self, txn_id: TxnId, commit_ts: Timestamp) -> Result<Vec<ApplyOp>> {
        let ctx = self.txns.get(&txn_id)
            .ok_or_else(|| TxnError::NotFound(txn_id))?;
        
        let mut ops = Vec::new();
        
        for (table_id, rowid) in &ctx.write_set {
            if let Some(store) = self.mvcc.get(table_id) {
                // 从 MVCC 版本链获取提交的数据
                if let Some(row) = store.get_for_txn(*rowid, commit_ts, txn_id) {
                    ops.push(ApplyOp::Insert {
                        table_id: *table_id,
                        row_id: *rowid,
                        row: row.clone(),
                    });
                }
            }
        }
        
        Ok(ops)
    }

    /// 回滚事务
    pub fn rollback(&mut self, txn_id: TxnId) -> Result<()> {
        // 检查状态
        let ctx = self.txns.get_mut(&txn_id)
            .ok_or_else(|| TxnError::NotFound(txn_id))?;

        if ctx.state != TxnState::Active {
            return Err(TxnError::InvalidState(
                format!("Cannot rollback: transaction {} is not active", txn_id)
            ).into());
        }

        ctx.state = TxnState::RolledBack;
        let write_set = std::mem::take(&mut ctx.write_set);

        // 写入 WAL ROLLBACK 记录
        self.wal.write_record(WalRecordType::Rollback, txn_id, 0, &[])?;
        self.wal.commit_flush()?;

        // 回滚 MVCC 版本（移除未提交版本）
        for (table_id, _rowid) in &write_set {
            if let Some(store) = self.mvcc.get_mut(table_id) {
                store.rollback_txn(txn_id);
            }
        }

        self.active_table.rollback_txn(txn_id);
        Ok(())
    }

    /// 事务内插入一行
    pub fn insert(&mut self, txn_id: TxnId, table_id: u32, rowid: u64, row: Vec<Value>) -> Result<()> {
        self.ensure_active(txn_id)?;

        let ctx = self.txns.get(&txn_id).unwrap();
        let write_ts = ctx.start_ts;

        // 写入 WAL
        let payload = make_insert_payload(rowid, &row);
        self.wal.write_record(WalRecordType::Insert, txn_id, table_id, &payload)?;

        // 写入 MVCC
        let store = self.mvcc.entry(table_id).or_insert_with(MvccStore::new);
        if !store.write(rowid, row, txn_id, write_ts) {
            return Err(TxnError::WriteConflict(
                format!("Write conflict on table {} row {}", table_id, rowid)
            ).into());
        }

        // 记录到 write_set
        let ctx = self.txns.get_mut(&txn_id).unwrap();
        ctx.write_set.push((table_id, rowid));

        Ok(())
    }

    /// 事务内更新一行
    pub fn update(&mut self, txn_id: TxnId, table_id: u32, rowid: u64, old_row: Vec<Value>, new_row: Vec<Value>) -> Result<()> {
        self.ensure_active(txn_id)?;

        let ctx = self.txns.get(&txn_id).unwrap();
        let write_ts = ctx.start_ts;

        // 写入 WAL（含旧值，用于回滚）
        let payload = make_update_payload(rowid, &old_row, &new_row);
        self.wal.write_record(WalRecordType::Update, txn_id, table_id, &payload)?;

        // 写入 MVCC
        let store = self.mvcc.entry(table_id).or_insert_with(MvccStore::new);
        if !store.write(rowid, new_row, txn_id, write_ts) {
            return Err(TxnError::WriteConflict(
                format!("Write conflict on table {} row {}", table_id, rowid)
            ).into());
        }

        // 记录到 write_set
        let ctx = self.txns.get_mut(&txn_id).unwrap();
        ctx.write_set.push((table_id, rowid));

        Ok(())
    }

    /// 事务内删除一行
    pub fn delete(&mut self, txn_id: TxnId, table_id: u32, rowid: u64, old_row: Vec<Value>) -> Result<()> {
        self.ensure_active(txn_id)?;

        // 写入 WAL（含旧值，用于回滚）
        let payload = make_delete_payload(rowid, &old_row);
        self.wal.write_record(WalRecordType::Delete, txn_id, table_id, &payload)?;

        // MVCC 中删除 = 写入一个 tombstone（空行或标记）
        // 简化：写入一个特殊的删除标记版本
        let ctx = self.txns.get(&txn_id).unwrap();
        let write_ts = ctx.start_ts;

        let store = self.mvcc.entry(table_id).or_insert_with(MvccStore::new);
        // 用空 Vec 表示删除（tombstone）
        if !store.write(rowid, Vec::new(), txn_id, write_ts) {
            return Err(TxnError::WriteConflict(
                format!("Write conflict on table {} row {}", table_id, rowid)
            ).into());
        }

        let ctx = self.txns.get_mut(&txn_id).unwrap();
        ctx.write_set.push((table_id, rowid));

        Ok(())
    }

    /// 读取一行（事务内快照读）
    pub fn read(&self, txn_id: TxnId, table_id: u32, rowid: u64) -> Option<&Vec<Value>> {
        let ctx = self.txns.get(&txn_id)?;
        let store = self.mvcc.get(&table_id)?;
        store.get_for_txn(rowid, ctx.start_ts, txn_id)
    }

    /// 获取事务的 start_ts
    pub fn start_ts(&self, txn_id: TxnId) -> Option<Timestamp> {
        self.txns.get(&txn_id).map(|ctx| ctx.start_ts)
    }

    /// 获取事务状态
    pub fn state(&self, txn_id: TxnId) -> Option<TxnState> {
        self.txns.get(&txn_id).map(|ctx| ctx.state)
    }

    /// 活跃事务数
    pub fn active_count(&self) -> usize {
        self.active_table.active_count()
    }

    /// 获取当前 WAL LSN
    pub fn current_wal_lsn(&self) -> u64 {
        self.wal.current_lsn()
    }

    /// 手动触发 WAL fsync（用于 Periodic 模式）
    pub fn sync_wal(&mut self) -> Result<()> {
        self.wal.sync()
    }

    /// 设置 WAL 刷盘策略
    pub fn set_wal_flush_mode(&mut self, mode: WalFlushMode) {
        self.wal.set_flush_mode(mode);
    }

    /// 获取 WAL 刷盘策略
    pub fn wal_flush_mode(&self) -> WalFlushMode {
        self.wal.flush_mode()
    }

    /// 设置 WAL 组提交大小（0 = 禁用，每次 commit 都 fsync）
    ///
    /// 组提交是 Sync 模式下的核心 WAL 加速机制：
    /// 多条事务共享一次 fsync，写入吞吐可提升数倍至数十倍。
    pub fn set_wal_group_commit_size(&mut self, size: usize) {
        self.wal.set_group_commit_size(size);
    }

    /// 获取当前待 fsync 的 commit 数量
    pub fn wal_pending_commits(&self) -> usize {
        self.wal.pending_commits()
    }

    /// 执行 Checkpoint
    pub fn checkpoint(&mut self) -> Result<u64> {
        // 写入 Checkpoint 记录
        let checkpoint_lsn = self.wal.current_lsn();
        let payload = super::super::wal::make_checkpoint_payload(checkpoint_lsn);
        self.wal.write_record(WalRecordType::Checkpoint, 0, 0, &payload)?;
        self.wal.sync()?;

        // 截断 WAL（保留 Checkpoint 之后的部分）
        // 实际生产中需要确保所有数据已刷到主存储
        // MVP：保留 Checkpoint 记录本身
        self.wal.truncate(checkpoint_lsn)?;

        Ok(checkpoint_lsn)
    }

    /// 垃圾回收旧版本
    pub fn gc_old_versions(&mut self) {
        let oldest_ts = self.active_table.oldest_start_ts()
            .unwrap_or(self.active_table.current_ts());

        for store in self.mvcc.values_mut() {
            store.gc(oldest_ts);
        }
    }

    /// 获取 MVCC 存储引用（用于扫描等）
    pub fn mvcc_store(&self, table_id: u32) -> Option<&MvccStore<Vec<Value>>> {
        self.mvcc.get(&table_id)
    }

    // --- 内部方法 ---

    fn ensure_active(&self, txn_id: TxnId) -> Result<()> {
        let ctx = self.txns.get(&txn_id)
            .ok_or_else(|| TxnError::NotFound(txn_id))?;

        if ctx.state != TxnState::Active {
            return Err(TxnError::InvalidState(
                format!("Transaction {} is not active", txn_id)
            ).into());
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup_manager(name: &str) -> (TransactionManager, String) {
        let mut p = std::env::temp_dir();
        let tid = format!("{:?}", std::thread::current().id())
            .replace('(', "_").replace(')', "")
            .replace([':', ' '], "_");
        p.push(format!("hybriddb_txn_{}_{}_{}.hdb", name, std::process::id(), tid));
        let tmp = p.to_string_lossy().to_string();
        cleanup(&tmp);
        let config = Config::default();
        (TransactionManager::new(&tmp, &config).unwrap(), tmp)
    }

    fn cleanup(path: &str) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(format!("{}-wal", path));
    }

    #[test]
    fn test_begin_commit() {
        let (mut mgr, path) = setup_manager("begin_commit");
        let txn_id = mgr.begin(IsolationLevel::SnapshotIsolation).unwrap();
        assert_eq!(mgr.state(txn_id), Some(TxnState::Active));
        assert_eq!(mgr.active_count(), 1);

        let commit_ts = mgr.commit(txn_id).unwrap();
        assert!(commit_ts > 0);
        assert_eq!(mgr.state(txn_id), Some(TxnState::Committed));
        assert_eq!(mgr.active_count(), 0);

        cleanup(&path);
    }

    #[test]
    fn test_begin_rollback() {
        let (mut mgr, path) = setup_manager("begin_rollback");
        let txn_id = mgr.begin(IsolationLevel::SnapshotIsolation).unwrap();
        assert_eq!(mgr.active_count(), 1);

        mgr.rollback(txn_id).unwrap();
        assert_eq!(mgr.state(txn_id), Some(TxnState::RolledBack));
        assert_eq!(mgr.active_count(), 0);

        cleanup(&path);
    }

    #[test]
    fn test_insert_and_read() {
        let (mut mgr, path) = setup_manager("insert_and_read");
        let table_id = 1;

        // Txn 1: 插入并提交
        let txn1 = mgr.begin(IsolationLevel::SnapshotIsolation).unwrap();
        mgr.insert(txn1, table_id, 1, vec![Value::Int64(42)]).unwrap();
        mgr.commit(txn1).unwrap();

        // Txn 2: 读取（应该能看到 txn1 提交的数据）
        let txn2 = mgr.begin(IsolationLevel::SnapshotIsolation).unwrap();
        let val = mgr.read(txn2, table_id, 1);
        assert!(val.is_some());
        assert_eq!(val.unwrap(), &vec![Value::Int64(42)]);
        mgr.commit(txn2).unwrap();

        cleanup(&path);
    }

    #[test]
    fn test_write_write_conflict() {
        let (mut mgr, path) = setup_manager("write_write_conflict");
        let table_id = 1;

        let txn1 = mgr.begin(IsolationLevel::SnapshotIsolation).unwrap();
        let txn2 = mgr.begin(IsolationLevel::SnapshotIsolation).unwrap();

        // Txn 1 先写
        mgr.insert(txn1, table_id, 1, vec![Value::Int64(100)]).unwrap();

        // Txn 2 写同一行，应该冲突
        let result = mgr.insert(txn2, table_id, 1, vec![Value::Int64(200)]);
        assert!(result.is_err());

        mgr.rollback(txn1).unwrap();
        mgr.rollback(txn2).unwrap();

        cleanup(&path);
    }

    #[test]
    fn test_snapshot_isolation() {
        let (mut mgr, path) = setup_manager("snapshot_isolation");
        let table_id = 1;

        // Txn 1: 插入数据并提交
        let txn1 = mgr.begin(IsolationLevel::SnapshotIsolation).unwrap();
        mgr.insert(txn1, table_id, 1, vec![Value::Int64(100)]).unwrap();
        mgr.commit(txn1).unwrap();

        // Txn 2: 开始（此时快照包含 txn1 的数据）
        let txn2 = mgr.begin(IsolationLevel::SnapshotIsolation).unwrap();

        // Txn 3: 更新并提交
        let txn3 = mgr.begin(IsolationLevel::SnapshotIsolation).unwrap();
        mgr.insert(txn3, table_id, 1, vec![Value::Int64(200)]).unwrap();
        // 注意：同一行的更新会冲突，这里用不同行
        mgr.insert(txn3, table_id, 2, vec![Value::Int64(300)]).unwrap();
        mgr.commit(txn3).unwrap();

        // Txn 2 应该看不到 txn3 写入的 row 2（快照隔离）
        let val = mgr.read(txn2, table_id, 2);
        assert!(val.is_none());

        // 但能看到 txn1 写入的 row 1
        let val = mgr.read(txn2, table_id, 1);
        assert!(val.is_some());

        mgr.commit(txn2).unwrap();

        cleanup(&path);
    }

    #[test]
    fn test_rollback_removes_versions() {
        let (mut mgr, path) = setup_manager("rollback_removes_versions");
        let table_id = 1;

        let txn = mgr.begin(IsolationLevel::SnapshotIsolation).unwrap();
        mgr.insert(txn, table_id, 1, vec![Value::Int64(999)]).unwrap();
        mgr.rollback(txn).unwrap();

        // 回滚后，其他事务不应该看到这个版本
        let txn2 = mgr.begin(IsolationLevel::SnapshotIsolation).unwrap();
        let val = mgr.read(txn2, table_id, 1);
        assert!(val.is_none());
        mgr.commit(txn2).unwrap();

        cleanup(&path);
    }

    #[test]
    fn test_update_operation() {
        let (mut mgr, path) = setup_manager("update_operation");
        let table_id = 1;

        // 插入初始值
        let txn1 = mgr.begin(IsolationLevel::SnapshotIsolation).unwrap();
        mgr.insert(txn1, table_id, 1, vec![Value::Int64(100)]).unwrap();
        mgr.commit(txn1).unwrap();

        // 更新
        let txn2 = mgr.begin(IsolationLevel::SnapshotIsolation).unwrap();
        mgr.update(txn2, table_id, 1, vec![Value::Int64(100)], vec![Value::Int64(200)]).unwrap();
        mgr.commit(txn2).unwrap();

        // 读取更新后的值
        let txn3 = mgr.begin(IsolationLevel::SnapshotIsolation).unwrap();
        let val = mgr.read(txn3, table_id, 1);
        assert_eq!(val.unwrap(), &vec![Value::Int64(200)]);
        mgr.commit(txn3).unwrap();

        cleanup(&path);
    }

    #[test]
    fn test_delete_operation() {
        let (mut mgr, path) = setup_manager("delete_operation");
        let table_id = 1;

        // 插入
        let txn1 = mgr.begin(IsolationLevel::SnapshotIsolation).unwrap();
        mgr.insert(txn1, table_id, 1, vec![Value::Int64(100)]).unwrap();
        mgr.commit(txn1).unwrap();

        // 删除
        let txn2 = mgr.begin(IsolationLevel::SnapshotIsolation).unwrap();
        mgr.delete(txn2, table_id, 1, vec![Value::Int64(100)]).unwrap();
        mgr.commit(txn2).unwrap();

        // 删除后读不到（返回空 Vec 即 tombstone）
        let txn3 = mgr.begin(IsolationLevel::SnapshotIsolation).unwrap();
        let val = mgr.read(txn3, table_id, 1);
        assert!(val.is_some());
        assert!(val.unwrap().is_empty()); // tombstone
        mgr.commit(txn3).unwrap();

        cleanup(&path);
    }

    #[test]
    fn test_checkpoint() {
        let (mut mgr, path) = setup_manager("checkpoint");
        let table_id = 1;

        // 插入一些数据
        let txn1 = mgr.begin(IsolationLevel::SnapshotIsolation).unwrap();
        mgr.insert(txn1, table_id, 1, vec![Value::Int64(42)]).unwrap();
        mgr.commit(txn1).unwrap();

        let lsn_before = mgr.current_wal_lsn();
        let checkpoint_lsn = mgr.checkpoint().unwrap();
        assert!(checkpoint_lsn > 0);
        // Checkpoint 后 WAL 应该被截断，LSN 可能重置
        let lsn_after = mgr.current_wal_lsn();
        assert!(lsn_after <= lsn_before || lsn_after == checkpoint_lsn);

        cleanup(&path);
    }

    #[test]
    fn test_gc_old_versions() {
        let (mut mgr, path) = setup_manager("gc_old_versions");
        let table_id = 1;

        // 提交多个版本
        for i in 0..5 {
            let txn = mgr.begin(IsolationLevel::SnapshotIsolation).unwrap();
            mgr.insert(txn, table_id, i as u64, vec![Value::Int64(i as i64 * 10)]).unwrap();
            mgr.commit(txn).unwrap();
        }

        // GC 不应在有活跃事务时清理活跃事务之前的版本
        let txn_active = mgr.begin(IsolationLevel::SnapshotIsolation).unwrap();
        mgr.gc_old_versions();
        // 活跃事务仍然能读到数据
        let val = mgr.read(txn_active, table_id, 0);
        assert!(val.is_some());
        mgr.commit(txn_active).unwrap();

        cleanup(&path);
    }

    #[test]
    fn test_commit_already_committed_fails() {
        let (mut mgr, path) = setup_manager("commit_already_committed_fails");
        let txn_id = mgr.begin(IsolationLevel::SnapshotIsolation).unwrap();
        mgr.commit(txn_id).unwrap();

        // 再次提交应该失败
        let result = mgr.commit(txn_id);
        assert!(result.is_err());

        cleanup(&path);
    }

    #[test]
    fn test_rollback_already_committed_fails() {
        let (mut mgr, path) = setup_manager("rollback_already_committed_fails");
        let txn_id = mgr.begin(IsolationLevel::SnapshotIsolation).unwrap();
        mgr.commit(txn_id).unwrap();

        // 回滚已提交事务应该失败
        let result = mgr.rollback(txn_id);
        assert!(result.is_err());

        cleanup(&path);
    }

    #[test]
    fn test_read_nonexistent_row() {
        let (mut mgr, path) = setup_manager("read_nonexistent_row");
        let table_id = 1;

        let txn = mgr.begin(IsolationLevel::SnapshotIsolation).unwrap();
        let val = mgr.read(txn, table_id, 999);
        assert!(val.is_none());
        mgr.commit(txn).unwrap();

        cleanup(&path);
    }

    #[test]
    fn test_read_nonexistent_table() {
        let (mut mgr, path) = setup_manager("read_nonexistent_table");
        let txn = mgr.begin(IsolationLevel::SnapshotIsolation).unwrap();
        let val = mgr.read(txn, 999, 1);
        assert!(val.is_none());
        mgr.commit(txn).unwrap();

        cleanup(&path);
    }

    #[test]
    fn test_wal_lsn_increases() {
        let (mut mgr, path) = setup_manager("wal_lsn_increases");
        let table_id = 1;

        let lsn0 = mgr.current_wal_lsn();

        let txn1 = mgr.begin(IsolationLevel::SnapshotIsolation).unwrap();
        let lsn1 = mgr.current_wal_lsn();
        assert!(lsn1 > lsn0);

        mgr.insert(txn1, table_id, 1, vec![Value::Int64(42)]).unwrap();
        let lsn2 = mgr.current_wal_lsn();
        assert!(lsn2 > lsn1);

        mgr.commit(txn1).unwrap();
        let lsn3 = mgr.current_wal_lsn();
        assert!(lsn3 > lsn2);

        cleanup(&path);
    }

    #[test]
    fn test_multiple_tables() {
        let (mut mgr, path) = setup_manager("multiple_tables");

        // 在两个不同的表中插入数据
        let txn = mgr.begin(IsolationLevel::SnapshotIsolation).unwrap();
        mgr.insert(txn, 1, 1, vec![Value::Int64(100)]).unwrap();
        mgr.insert(txn, 2, 1, vec![Value::Varchar("hello".to_string())]).unwrap();
        mgr.commit(txn).unwrap();

        // 验证两个表的数据独立
        let txn2 = mgr.begin(IsolationLevel::SnapshotIsolation).unwrap();
        let val1 = mgr.read(txn2, 1, 1);
        assert_eq!(val1.unwrap(), &vec![Value::Int64(100)]);

        let val2 = mgr.read(txn2, 2, 1);
        assert_eq!(val2.unwrap(), &vec![Value::Varchar("hello".to_string())]);
        mgr.commit(txn2).unwrap();

        cleanup(&path);
    }

    #[test]
    fn test_start_ts() {
        let (mut mgr, path) = setup_manager("start_ts");
        let txn_id = mgr.begin(IsolationLevel::SnapshotIsolation).unwrap();
        let ts = mgr.start_ts(txn_id);
        assert!(ts.is_some());
        assert!(ts.unwrap() > 0);

        // 不存在的事务
        assert!(mgr.start_ts(9999).is_none());

        mgr.commit(txn_id).unwrap();
        // 提交后仍然能查到 start_ts
        assert!(mgr.start_ts(txn_id).is_some());

        cleanup(&path);
    }

    #[test]
    fn test_state_transitions() {
        let (mut mgr, path) = setup_manager("state_transitions");
        let txn_id = mgr.begin(IsolationLevel::SnapshotIsolation).unwrap();
        assert_eq!(mgr.state(txn_id), Some(TxnState::Active));

        mgr.commit(txn_id).unwrap();
        assert_eq!(mgr.state(txn_id), Some(TxnState::Committed));

        // 不存在的事务
        assert!(mgr.state(9999).is_none());

        cleanup(&path);
    }

    #[test]
    fn test_multiple_concurrent_txns() {
        let (mut mgr, path) = setup_manager("multiple_concurrent_txns");
        let table_id = 1;

        // 开启 3 个并发事务
        let txn1 = mgr.begin(IsolationLevel::SnapshotIsolation).unwrap();
        let txn2 = mgr.begin(IsolationLevel::SnapshotIsolation).unwrap();
        let txn3 = mgr.begin(IsolationLevel::SnapshotIsolation).unwrap();

        assert_eq!(mgr.active_count(), 3);

        // 每个事务写不同的行
        mgr.insert(txn1, table_id, 1, vec![Value::Int64(1)]).unwrap();
        mgr.insert(txn2, table_id, 2, vec![Value::Int64(2)]).unwrap();
        mgr.insert(txn3, table_id, 3, vec![Value::Int64(3)]).unwrap();

        // 各自提交
        mgr.commit(txn1).unwrap();
        mgr.commit(txn2).unwrap();
        mgr.commit(txn3).unwrap();

        assert_eq!(mgr.active_count(), 0);

        // 新事务能看到所有数据
        let txn4 = mgr.begin(IsolationLevel::SnapshotIsolation).unwrap();
        assert_eq!(mgr.read(txn4, table_id, 1).unwrap(), &vec![Value::Int64(1)]);
        assert_eq!(mgr.read(txn4, table_id, 2).unwrap(), &vec![Value::Int64(2)]);
        assert_eq!(mgr.read(txn4, table_id, 3).unwrap(), &vec![Value::Int64(3)]);
        mgr.commit(txn4).unwrap();

        cleanup(&path);
    }

    #[test]
    fn test_mvcc_store_accessor() {
        let (mut mgr, path) = setup_manager("mvcc_store_accessor");
        let table_id = 1;

        // 没有数据时 mvcc_store 返回 None
        assert!(mgr.mvcc_store(table_id).is_none());

        // 插入数据后返回 Some
        let txn = mgr.begin(IsolationLevel::SnapshotIsolation).unwrap();
        mgr.insert(txn, table_id, 1, vec![Value::Int64(42)]).unwrap();
        mgr.commit(txn).unwrap();

        assert!(mgr.mvcc_store(table_id).is_some());
        assert!(mgr.mvcc_store(999).is_none());

        cleanup(&path);
    }
}
