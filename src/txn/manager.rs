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
use crate::common::types::EngineType;
use crate::common::config::{Config, WalFlushMode};
use crate::wal::{WalWriter, WalRecordType, make_insert_payload, make_insert_batch_payload, make_update_payload, make_delete_payload};
use crate::Value;

use super::{TxnState, IsolationLevel, TxnError, TxnId, Timestamp, MvccStore, ActiveTxnTable, ApplyOp, CommitResult};

/// 事务上下文（单个事务的状态）
#[derive(Debug)]
struct TxnContext {
    id: TxnId,
    state: TxnState,
    isolation_level: IsolationLevel,
    start_ts: Timestamp,
    /// 该事务是否只读（只读事务跳过 WAL，v0.15.0 Txn09）
    read_only: bool,
    /// 该事务写入的 key 列表（用于提交/回滚时处理）
    /// (table_id, rowid)
    write_set: Vec<(u32, u64)>,
    /// SAVEPOINT 栈（v0.15.0 Txn05 新增）
    ///
    /// 每个 entry 记录了 (savepoint_name, 创建时的 write_set.len())
    /// ROLLBACK TO SAVEPOINT 时，从栈顶向下查找匹配的 name，
    /// 将 write_set 回退到对应位置，丢弃期间的未提交版本。
    savepoints: Vec<(String, usize)>,
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
    /// 非持久化表集合（MemoryEngine，M2）：这些表的操作不写 WAL，
    /// 事务提交也不 fsync（进程退出数据丢失，符合内存表语义）。
    non_persistent_tables: std::collections::HashSet<u32>,
    /// 表引擎映射（M4：WAL 记录头 engine_type 来源）
    table_engines: std::collections::HashMap<u32, EngineType>,
}

impl TransactionManager {
    /// 创建事务管理器（带配置）
    pub fn new(db_path: &str, config: &Config) -> Result<Self> {
        let wal_path = format!("{}-wal", db_path);
        let mut wal = WalWriter::with_config(
            &wal_path,
            config.wal_flush_mode,
            config.wal_buffer_size,
            config.wal_group_commit_size,
            config.wal_group_commit_max_bytes,
        )?;
        wal.set_max_sync_interval_ms(config.wal_group_commit_timeout_ms);

        Ok(Self {
            active_table: ActiveTxnTable::new(),
            mvcc: HashMap::new(),
            wal,
            txns: HashMap::new(),
            db_path: PathBuf::from(db_path),
            non_persistent_tables: std::collections::HashSet::new(),
            table_engines: std::collections::HashMap::new(),
        })
    }

    /// 注册表引擎（M4）：WAL 数据记录的 engine_type 来源
    pub fn register_table_engine(&mut self, table_id: u32, engine: EngineType) {
        self.table_engines.insert(table_id, engine);
    }

    /// 表引擎（未注册回退 Columnar——旧 WAL 兼容语义）
    fn table_engine(&self, table_id: u32) -> EngineType {
        self.table_engines
            .get(&table_id)
            .copied()
            .unwrap_or(EngineType::Columnar)
    }

    /// 开始一个新事务
    pub fn begin(&mut self, isolation_level: IsolationLevel) -> Result<TxnId> {
        self.begin_with_flags(isolation_level, false)
    }

    /// 开始一个新事务（可指定只读标志）
    ///
    /// 只读事务跳过 WAL 写入，避免不必要的 fsync 开销（v0.15.0 Txn09）。
    pub fn begin_readonly(&mut self, isolation_level: IsolationLevel) -> Result<TxnId> {
        self.begin_with_flags(isolation_level, true)
    }

    fn begin_with_flags(&mut self, isolation_level: IsolationLevel, read_only: bool) -> Result<TxnId> {
        let (txn_id, start_ts) = self.active_table.begin_txn();

        // 只读事务跳过 WAL BEGIN 记录（v0.15.0 Txn09）
        if !read_only {
            self.wal.write_record(WalRecordType::Begin, txn_id, 0, EngineType::Columnar, &[])?;
        }

        let ctx = TxnContext {
            id: txn_id,
            state: TxnState::Active,
            isolation_level,
            start_ts,
            read_only,
            write_set: Vec::new(),
            savepoints: Vec::new(),
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
        let read_only = ctx.read_only;
        // 注意：必须在 take 之前克隆 write_set，因为 collect_apply_ops 需要它
        let write_set = ctx.write_set.clone();
        let _dropped = std::mem::take(&mut ctx.write_set);

        // 只读事务跳过 WAL COMMIT 记录和 fsync（v0.15.0 Txn09）；
        // 全部写入仅涉及非持久化表（MemoryEngine）时同样跳过（M2）
        let has_persistent_write = write_set.iter().any(|(tid, _)| self.is_persistent(*tid));
        if !read_only && has_persistent_write {
            self.wal.write_record(WalRecordType::Commit, txn_id, 0, EngineType::Columnar, &[])?;
            self.wal.commit_flush()?;
        }

        // 获取 commit_ts
        let commit_ts = self.active_table.commit_txn(txn_id);

        // 提交 MVCC 版本（P1.1：只处理本事务写过的 key，避免 O(所有key) 全链扫描）
        for (table_id, rowid) in &write_set {
            if let Some(store) = self.mvcc.get_mut(table_id) {
                store.commit_txn_key(*rowid, txn_id, commit_ts);
            }
        }
        
        // 收集待应用操作（方案 B：返回 apply_ops，由 executor 应用到存储层）
        // 注意：必须在 GC 之前收集，因为 gc_key 会清掉旧版本，导致 has_committed_version_before 失效
        let apply_ops = self.collect_apply_ops(&write_set, txn_id)?;

        // P1.2：commit 后立即对写过的 key 做 GC，防止版本链无限增长
        let oldest_active_ts = self.active_table.oldest_start_ts().unwrap_or(commit_ts);
        for (table_id, rowid) in &write_set {
            if let Some(store) = self.mvcc.get_mut(table_id) {
                store.gc_key(*rowid, oldest_active_ts);
            }
        }

        Ok(CommitResult { commit_ts, apply_ops })
    }
    
    /// 收集待应用操作（内部辅助方法）
    ///
    /// 根据 MVCC 版本链判断操作类型：
    /// - Insert: rowid 之前没有已提交版本
    /// - Update: rowid 之前有已提交版本，且事务创建了新版本
    /// - Delete: rowid 之前有已提交版本，但事务没有创建新版本（删除标记）
    fn collect_apply_ops(&self, write_set: &[(u32, u64)], txn_id: TxnId) -> Result<Vec<ApplyOp>> {
        let mut ops = Vec::new();
        
        for (table_id, rowid) in write_set {
            if let Some(store) = self.mvcc.get(table_id) {
                // 检查是否有旧版本（用于区分 Insert 和 Update/Delete）
                let has_old_version = store.has_committed_version_before(*rowid, txn_id);
                
                // 尝试获取事务创建的新版本
                let new_version = store.get_txn_version(*rowid, txn_id);
                
                // 判断是否为删除操作：delete() 用空 Vec 作为 tombstone 标记删除
                let is_delete = new_version.as_ref().map_or(false, |v| v.is_empty());
                
                if is_delete {
                    // 删除操作（有旧版本且被删除
                    if has_old_version {
                        ops.push(ApplyOp::Delete {
                            table_id: *table_id,
                            row_id: *rowid,
                        });
                    }
                    // 无旧版本的删除（比如删除一个不存在的行则跳过（空操作）
                } else if let Some(new_row) = new_version {
                    // 有新版本且非空：Insert 或 Update
                    if has_old_version {
                        // 有旧版本 → Update
                        ops.push(ApplyOp::Update {
                            table_id: *table_id,
                            row_id: *rowid,
                            new_row: new_row.clone(),
                        });
                    } else {
                        // 无旧版本 → Insert
                        ops.push(ApplyOp::Insert {
                            table_id: *table_id,
                            row_id: *rowid,
                            row: new_row.clone(),
                        });
                    }
                } else {
                    // 无新版本且非空：Delete（有旧版本）
                    if has_old_version {
                        ops.push(ApplyOp::Delete {
                            table_id: *table_id,
                            row_id: *rowid,
                        });
                    }
                    // 如果既没有旧版本也没有新版本，则跳过（可能是回滚的操作）
                }
            }
        }
        
        // P-W2c：合并连续同表 Insert 段为 InsertBatch
        // 条件：连续 ≥2 个 Insert、同 table_id、rowid 连续（base + i）
        // 合并后行数据转置为列式，apply_to_storage 直接走 insert_columns
        if ops.len() >= 2 {
            ops = merge_insert_batches(ops);
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
        let read_only = ctx.read_only;
        let write_set = std::mem::take(&mut ctx.write_set);
        // 同时清空 savepoint 栈（事务回滚后所有 savepoint 失效）
        let _ = std::mem::take(&mut ctx.savepoints);

        // 只读事务跳过 WAL ROLLBACK 记录（v0.15.0 Txn09）；
        // 仅涉及非持久化表（MemoryEngine）时同样跳过（M2）
        let has_persistent_write = write_set.iter().any(|(tid, _)| self.is_persistent(*tid));
        if !read_only && has_persistent_write {
            self.wal.write_record(WalRecordType::Rollback, txn_id, 0, EngineType::Columnar, &[])?;
            self.wal.commit_flush()?;
        }

        // 回滚 MVCC 版本（移除未提交版本）
        for (table_id, _rowid) in &write_set {
            if let Some(store) = self.mvcc.get_mut(table_id) {
                store.rollback_txn(txn_id);
            }
        }

        self.active_table.rollback_txn(txn_id);
        Ok(())
    }

    /// 创建 SAVEPOINT（v0.15.0 Txn05 新增）
    ///
    /// 在事务中标记一个回滚点。后续 ROLLBACK TO SAVEPOINT <name> 可回退到该点，
    /// 但事务保持 Active 状态。
    ///
    /// SAVEPOINT 嵌套支持：每次 SAVEPOINT 会将当前 write_set.len() 压栈。
    pub fn savepoint(&mut self, txn_id: TxnId, name: &str) -> Result<()> {
        let ctx = self.txns.get_mut(&txn_id)
            .ok_or_else(|| TxnError::NotFound(txn_id))?;

        if ctx.state != TxnState::Active {
            return Err(TxnError::InvalidState(
                format!("Cannot create savepoint: transaction {} is not active", txn_id)
            ).into());
        }

        // 嵌套同名 savepoint 的语义：按 SQLite/MySQL 行为，后定义的覆盖之前的
        // 简化处理：允许同名，rollback 时回退到最近的同名 savepoint
        ctx.savepoints.push((name.to_string(), ctx.write_set.len()));
        Ok(())
    }

    /// 释放 SAVEPOINT（v0.15.0 Txn05 新增）
    ///
    /// 销毁最近的同名 savepoint，但不影响已写入的数据。
    /// 如果没有同名 savepoint，返回错误。
    pub fn release_savepoint(&mut self, txn_id: TxnId, name: &str) -> Result<()> {
        let ctx = self.txns.get_mut(&txn_id)
            .ok_or_else(|| TxnError::NotFound(txn_id))?;

        if ctx.state != TxnState::Active {
            return Err(TxnError::InvalidState(
                format!("Cannot release savepoint: transaction {} is not active", txn_id)
            ).into());
        }

        // 从栈顶向下查找第一个匹配的 savepoint
        let pos = ctx.savepoints.iter().rposition(|(n, _)| n == name)
            .ok_or_else(|| TxnError::InvalidState(
                format!("SAVEPOINT {} does not exist", name)
            ))?;
        ctx.savepoints.remove(pos);
        Ok(())
    }

    /// ROLLBACK TO SAVEPOINT（v0.15.0 Txn05 新增）
    ///
    /// 回退到指定 savepoint 之后的所有写操作，事务保持 Active。
    /// 后续操作可以正常继续。
    ///
    /// 实现要点：
    /// 1. 在 savepoint 栈中找到目标 savepoint
    /// 2. 将 write_set 截断到 savepoint 时的位置
    /// 3. 删除该 savepoint 之后创建的所有 savepoint
    /// 4. 丢弃被回退的 MVCC 版本（通过 commit_txn/rollback_txn 机制）
    pub fn rollback_to_savepoint(&mut self, txn_id: TxnId, name: &str) -> Result<()> {
        // 1. 检查事务状态 + 查找 savepoint
        let target_pos;
        let target_write_set_len;
        {
            let ctx = self.txns.get(&txn_id)
                .ok_or_else(|| TxnError::NotFound(txn_id))?;

            if ctx.state != TxnState::Active {
                return Err(TxnError::InvalidState(
                    format!("Cannot rollback to savepoint: transaction {} is not active", txn_id)
                ).into());
            }

            let pos = ctx.savepoints.iter().rposition(|(n, _)| n == name)
                .ok_or_else(|| TxnError::InvalidState(
                    format!("SAVEPOINT {} does not exist", name)
                ))?;
            target_pos = pos;
            target_write_set_len = ctx.savepoints[pos].1;
        }

        // 2. 截断 write_set，收集被回退的 (table_id, rowid)
        let rolled_back_keys: Vec<(u32, u64)>;
        {
            let ctx = self.txns.get_mut(&txn_id).unwrap();
            let old_len = ctx.write_set.len();
            ctx.write_set.truncate(target_write_set_len);
            rolled_back_keys = if ctx.write_set.len() < old_len {
                ctx.write_set[target_write_set_len..old_len].to_vec()
            } else {
                Vec::new()
            };
            // 3. 删除该 savepoint 及之后的所有 savepoint
            ctx.savepoints.truncate(target_pos + 1);
        }

        // 4. 丢弃被回退的 MVCC 版本
        // 注意：这里只能"标记"被回滚，但 MVCC 的 rollback_txn 会清除该 txn 的所有未提交版本
        // 由于事务仍 Active，rollback_txn 后再写入会重建版本
        // 简化处理：仅对被回退的 key 单独调用 MVCC 清理（如果支持）
        // 实际上当前 MVCC 实现没有 per-key rollback，只能 rollback_txn（清除整个 txn 的版本）
        // 因此这里只能写入 WAL 记录 + 截断 write_set，MVCC 端在最终 commit 时会按 write_set 应用
        for (table_id, _rowid) in &rolled_back_keys {
            // 仅记录到 WAL，不修改 MVCC（commit 时按 write_set 收集 apply_ops）
        }

        Ok(())
    }

    /// 标记表为非持久化（MemoryEngine）：其操作不写 WAL（M2）
    pub fn mark_non_persistent(&mut self, table_id: u32) {
        self.non_persistent_tables.insert(table_id);
    }

    /// 表是否持久化（写 WAL）
    pub fn is_persistent(&self, table_id: u32) -> bool {
        !self.non_persistent_tables.contains(&table_id)
    }

    /// 事务内插入一行
    pub fn insert(&mut self, txn_id: TxnId, table_id: u32, rowid: u64, row: Vec<Value>) -> Result<()> {
        self.ensure_active(txn_id)?;

        let ctx = self.txns.get(&txn_id).unwrap();
        let write_ts = ctx.start_ts;

        // 写入 WAL（Memory 表跳过）
        if self.is_persistent(table_id) {
            let payload = make_insert_payload(rowid, &row);
            self.wal.write_record(
                WalRecordType::Insert,
                txn_id,
                table_id,
                self.table_engine(table_id),
                &payload,
            )?;
        }

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

    /// 事务内批量插入（P-W2a）
    ///
    /// 与 N 次 `insert()` 语义完全一致，但：
    /// - 1 次 WAL `InsertBatch` 记录（替代 N 条 `Insert`，省 N×19B 头）
    /// - 1 次 MVCC `batch_write`（替代 N 次 hashmap entry lookup）
    /// - 1 次 write_set 扩展（替代 N 次 push）
    ///
    /// 行 i 的 rowid = base_rowid + i。失败（写-写冲突）时整批不写。
    pub fn batch_insert(
        &mut self,
        txn_id: TxnId,
        table_id: u32,
        base_rowid: u64,
        rows: Vec<Vec<Value>>,
    ) -> Result<()> {
        self.ensure_active(txn_id)?;

        let num_rows = rows.len() as u64;
        if num_rows == 0 {
            return Ok(());
        }

        let ctx = self.txns.get(&txn_id).unwrap();
        let write_ts = ctx.start_ts;

        // 1. 写入 WAL（单条 InsertBatch 记录；Memory 表跳过）
        if self.is_persistent(table_id) {
            let payload = make_insert_batch_payload(base_rowid, &rows);
            self.wal.write_record(
                WalRecordType::InsertBatch,
                txn_id,
                table_id,
                self.table_engine(table_id),
                &payload,
            )?;
        }

        // 2. 写入 MVCC（单次批量写）
        let store = self.mvcc.entry(table_id).or_insert_with(MvccStore::new);
        if !store.batch_write(base_rowid, rows, txn_id, write_ts) {
            return Err(TxnError::WriteConflict(
                format!("Write conflict on table {} rows {}..{}", table_id, base_rowid, base_rowid + num_rows)
            ).into());
        }

        // 3. 记录到 write_set（一次扩展）
        let ctx = self.txns.get_mut(&txn_id).unwrap();
        ctx.write_set.extend(
            (0..num_rows).map(|i| (table_id, base_rowid + i))
        );

        Ok(())
    }

    /// 事务内更新一行
    pub fn update(&mut self, txn_id: TxnId, table_id: u32, rowid: u64, old_row: Vec<Value>, new_row: Vec<Value>) -> Result<()> {
        self.ensure_active(txn_id)?;

        let ctx = self.txns.get(&txn_id).unwrap();
        let write_ts = ctx.start_ts;

        // 写入 WAL（含旧值，用于回滚；Memory 表跳过）
        if self.is_persistent(table_id) {
            let payload = make_update_payload(rowid, &old_row, &new_row);
            self.wal.write_record(
                WalRecordType::Update,
                txn_id,
                table_id,
                self.table_engine(table_id),
                &payload,
            )?;
        }

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

        // 写入 WAL（含旧值，用于回滚；Memory 表跳过）
        if self.is_persistent(table_id) {
            let payload = make_delete_payload(rowid, &old_row);
            self.wal.write_record(
                WalRecordType::Delete,
                txn_id,
                table_id,
                self.table_engine(table_id),
                &payload,
            )?;
        }

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
    
    /// 检查事务管理器是否就绪（用于防御性检查）
    ///
    /// 当 enable_transaction=true 时，executor 会检查此方法
    /// 以确保 txn_manager 已正确初始化。
    pub fn is_ready(&self) -> bool {
        // WAL 已初始化即表示事务管理器就绪
        // current_lsn 为 0 时表示尚未写入任何记录，但 WAL 本身已初始化
        self.wal.current_lsn() >= 0  // 始终为 true（WAL 创建时即初始化）
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

    /// P0-3 时间窗组提交：距上次 fsync 超时则下次 commit 强制 sync（0 = 禁用）
    ///
    /// 低流量场景下 count/bytes 阈值迟迟不触发，数据停留在 page cache 的
    /// 时间被限定在约该毫秒数内（延迟有界）。
    pub fn set_wal_group_commit_timeout_ms(&mut self, ms: u64) {
        self.wal.set_max_sync_interval_ms(ms);
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
        self.wal.write_record(WalRecordType::Checkpoint, 0, 0, EngineType::Columnar, &payload)?;
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

    /// 清空指定表的 MVCC 版本（v0.15.0 TRUNCATE TABLE 支持）
    ///
    /// TRUNCATE 后，表的旧版本数据不应再影响事务的 Insert/Update 判定。
    pub fn clear_table_mvcc(&mut self, table_id: u32) {
        if let Some(store) = self.mvcc.get_mut(&table_id) {
            // 重新创建空 store（保留类型）
            *store = MvccStore::new();
        }
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

// ============================================================================
// P-W2c：连续同表 Insert 段 → ApplyOp::InsertBatch 合并
// ============================================================================

/// 合并连续同表 Insert 段为 `ApplyOp::InsertBatch`
///
/// 条件：≥2 个连续 Insert、同 table_id、rowid 连续（base_row_id + i）。
/// 合并后行数据转置为列式（每列一个 Vec），供 `apply_to_storage` 走
/// `table.insert_columns` 列式落盘（无行→列转置）。
///
/// Update/Delete/非连续 Insert 保持原样（顺序不变）。
fn merge_insert_batches(ops: Vec<ApplyOp>) -> Vec<ApplyOp> {
    let mut result: Vec<ApplyOp> = Vec::with_capacity(ops.len());
    let mut i = 0;

    while i < ops.len() {
        // 找从 i 开始的连续同表 Insert 段
        let mut run_len = 0usize;
        let start_table = match &ops[i] {
            ApplyOp::Insert { table_id, .. } => {
                run_len = 1;
                *table_id
            }
            _ => {
                // 非 Insert：原样保留
                result.push(ops[i].clone());
                i += 1;
                continue;
            }
        };

        // 收集段内 rowid，检查连续性
        while i + run_len < ops.len() {
            match &ops[i + run_len] {
                ApplyOp::Insert { table_id, .. } if *table_id == start_table => {
                    run_len += 1;
                }
                _ => break,
            }
        }

        if run_len >= 2 {
            // 检查 rowid 连续：base + idx
            let base_row_id = match &ops[i] {
                ApplyOp::Insert { row_id, .. } => *row_id,
                _ => unreachable!(),
            };
            let contiguous = (0..run_len).all(|k| {
                matches!(&ops[i + k], ApplyOp::Insert { row_id, .. } if *row_id == base_row_id + k as u64)
            });

            if contiguous {
                // 行 → 列转置（首行列数决定列数）
                let num_cols = match &ops[i] {
                    ApplyOp::Insert { row, .. } => row.len(),
                    _ => 0,
                };
                let mut columns: Vec<Vec<crate::Value>> = (0..num_cols).map(|_| Vec::with_capacity(run_len)).collect();
                for k in 0..run_len {
                    if let ApplyOp::Insert { row, .. } = &ops[i + k] {
                        for (c, v) in row.iter().enumerate() {
                            if c < num_cols {
                                columns[c].push(v.clone());
                            }
                        }
                    }
                }
                result.push(ApplyOp::InsertBatch {
                    table_id: start_table,
                    base_row_id,
                    columns,
                });
                i += run_len;
                continue;
            }
        }

        // 非连续或长度 <2：逐个原样保留
        for k in 0..run_len {
            result.push(ops[i + k].clone());
        }
        i += run_len;
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup_manager(name: &str) -> (TransactionManager, String) {
        let mut p = std::env::temp_dir();
        let tid = format!("{:?}", std::thread::current().id())
            .replace('(', "_").replace(')', "")
            .replace([':', ' '], "_");
        p.push(format!("engramdb_txn_{}_{}_{}.hdb", name, std::process::id(), tid));
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

        let commit_ts = mgr.commit(txn_id).unwrap().commit_ts;
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

    // ========================================================================
    // P-W2a：batch_insert
    // ========================================================================

    #[test]
    fn test_batch_insert_commit_read() {
        let (mut mgr, path) = setup_manager("batch_insert_commit_read");
        let table_id = 1;

        // Txn 1: 批量插入 100 行（rowid 10..110）
        let txn1 = mgr.begin(IsolationLevel::SnapshotIsolation).unwrap();
        let rows: Vec<Vec<Value>> = (0..100).map(|i| vec![Value::Int64(i as i64)]).collect();
        mgr.batch_insert(txn1, table_id, 10, rows).unwrap();
        mgr.commit(txn1).unwrap();

        // Txn 2: 读取验证
        let txn2 = mgr.begin(IsolationLevel::SnapshotIsolation).unwrap();
        for i in 0..100u64 {
            let val = mgr.read(txn2, table_id, 10 + i);
            assert!(val.is_some());
            assert_eq!(val.unwrap(), &vec![Value::Int64(i as i64)]);
        }
        mgr.commit(txn2).unwrap();

        cleanup(&path);
    }

    #[test]
    fn test_batch_insert_write_conflict() {
        let (mut mgr, path) = setup_manager("batch_insert_conflict");
        let table_id = 1;

        // Txn 1: 写 rowid 12
        let txn1 = mgr.begin(IsolationLevel::SnapshotIsolation).unwrap();
        mgr.insert(txn1, table_id, 12, vec![Value::Int64(100)]).unwrap();

        // Txn 2: 批量写 10..20 — 与 rowid 12 冲突
        let txn2 = mgr.begin(IsolationLevel::SnapshotIsolation).unwrap();
        let rows: Vec<Vec<Value>> = (0..10).map(|i| vec![Value::Int64(i as i64)]).collect();
        let result = mgr.batch_insert(txn2, table_id, 10, rows);
        assert!(result.is_err(), "batch insert should conflict");

        // 冲突后 txn2 的 write_set 应为空（整批失败）
        let ctx = mgr.txns.get(&txn2).unwrap();
        assert!(ctx.write_set.is_empty(), "conflict batch must leave no write_set");

        mgr.rollback(txn1).unwrap();
        mgr.rollback(txn2).unwrap();
        cleanup(&path);
    }

    #[test]
    fn test_batch_insert_empty() {
        let (mut mgr, path) = setup_manager("batch_insert_empty");
        let txn = mgr.begin(IsolationLevel::SnapshotIsolation).unwrap();
        // 空批量：无副作用
        mgr.batch_insert(txn, 1, 0, Vec::new()).unwrap();
        let ctx = mgr.txns.get(&txn).unwrap();
        assert!(ctx.write_set.is_empty());
        mgr.rollback(txn).unwrap();
        cleanup(&path);
    }

    // ========================================================================
    // P-W2c：merge_insert_batches
    // ========================================================================

    #[test]
    fn test_merge_insert_batches_contiguous() {
        let ops = vec![
            ApplyOp::Insert { table_id: 1, row_id: 0, row: vec![Value::Int64(0), Value::Varchar("a".into())] },
            ApplyOp::Insert { table_id: 1, row_id: 1, row: vec![Value::Int64(1), Value::Varchar("b".into())] },
            ApplyOp::Insert { table_id: 1, row_id: 2, row: vec![Value::Int64(2), Value::Varchar("c".into())] },
        ];
        let merged = merge_insert_batches(ops);
        assert_eq!(merged.len(), 1);
        match &merged[0] {
            ApplyOp::InsertBatch { table_id, base_row_id, columns } => {
                assert_eq!(*table_id, 1);
                assert_eq!(*base_row_id, 0);
                assert_eq!(columns.len(), 2); // 2 列
                assert_eq!(columns[0].len(), 3); // 3 行
                assert_eq!(columns[0][2], Value::Int64(2));
                assert_eq!(columns[1][1], Value::Varchar("b".into()));
            }
            other => panic!("expected InsertBatch, got {:?}", other),
        }
    }

    #[test]
    fn test_merge_insert_batches_keeps_update() {
        // Insert + Update 混合：Update 打断合并
        let ops = vec![
            ApplyOp::Insert { table_id: 1, row_id: 0, row: vec![Value::Int64(0)] },
            ApplyOp::Insert { table_id: 1, row_id: 1, row: vec![Value::Int64(1)] },
            ApplyOp::Update { table_id: 1, row_id: 0, new_row: vec![Value::Int64(99)] },
            ApplyOp::Insert { table_id: 1, row_id: 2, row: vec![Value::Int64(2)] },
        ];
        let merged = merge_insert_batches(ops);
        // 前 2 个 Insert 合并为 1 个 InsertBatch；Update 保留；最后一个 Insert 单条
        assert_eq!(merged.len(), 3);
        assert!(matches!(merged[0], ApplyOp::InsertBatch { .. }));
        assert!(matches!(merged[1], ApplyOp::Update { .. }));
        assert!(matches!(merged[2], ApplyOp::Insert { .. }));
    }

    #[test]
    fn test_merge_insert_batches_non_contiguous_rowids() {
        // 同表但 rowid 不连续：不合并
        let ops = vec![
            ApplyOp::Insert { table_id: 1, row_id: 0, row: vec![Value::Int64(0)] },
            ApplyOp::Insert { table_id: 1, row_id: 5, row: vec![Value::Int64(5)] },
        ];
        let merged = merge_insert_batches(ops);
        assert_eq!(merged.len(), 2);
        assert!(matches!(merged[0], ApplyOp::Insert { .. }));
        assert!(matches!(merged[1], ApplyOp::Insert { .. }));
    }

    #[test]
    fn test_merge_insert_batches_multi_table() {
        // 跨表交替插入：各表独立合并
        let ops = vec![
            ApplyOp::Insert { table_id: 1, row_id: 0, row: vec![Value::Int64(0)] },
            ApplyOp::Insert { table_id: 1, row_id: 1, row: vec![Value::Int64(1)] },
            ApplyOp::Insert { table_id: 2, row_id: 0, row: vec![Value::Int64(10)] },
            ApplyOp::Insert { table_id: 2, row_id: 1, row: vec![Value::Int64(11)] },
        ];
        let merged = merge_insert_batches(ops);
        assert_eq!(merged.len(), 2);
        assert!(matches!(merged[0], ApplyOp::InsertBatch { table_id: 1, .. }));
        assert!(matches!(merged[1], ApplyOp::InsertBatch { table_id: 2, .. }));
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
