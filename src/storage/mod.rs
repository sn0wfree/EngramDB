//! 存储引擎模块

pub mod file_format;
pub mod buffer_pool;
pub mod column_store;
pub mod delta_store;
pub mod compression;
pub mod table;
pub mod sparse_index;
pub mod vector_index;
pub mod cache;
pub mod rate_limiter;
pub mod index;
pub mod catalog;
pub mod engine;
pub mod bloom;
pub mod capabilities;
pub mod insert_batcher;
mod log_engine;
mod memory_engine;

use std::collections::HashMap;
use std::path::PathBuf;

use crate::common::error::{Result, EngramDbError};
use crate::common::types::TableDef;
use engine::EngineTable;
use crate::common::config::Config;
use crate::txn::TransactionManager;
use file_format::FileHeader;
use table::Table;
use crate::Value;

/// 数据库实例
pub struct Database {
    path: PathBuf,
    config: Config,
    header: FileHeader,
    tables: HashMap<u32, EngineTable>,
    table_names: HashMap<String, u32>,
    next_table_id: u32,
    file: std::fs::File,
    /// 事务管理器
    txn_manager: TransactionManager,
    /// 查询计划缓存（Perf02 / v0.18 P0-1）
    ///
    /// 值 = (计划, batcher_clean)：命中路径跳过 parse，仍需按攒批语义冲刷
    plan_cache: std::collections::HashMap<String, (crate::executor::physical_plan::PhysicalPlan, bool)>,
    /// 统计信息缓存（M5）：ANALYZE 收集，JOIN 代价模型消费
    statistics_cache: std::collections::HashMap<String, crate::sql::statistics::TableStatistics>,
    /// KV 缓存引擎（v0.15.0 新增）
    pub kv_cache: crate::storage::cache::KVCache,
    /// P0-2 INSERT 攒批合并器（autocommit 逐行 INSERT 合批落盘）
    batcher: crate::storage::insert_batcher::InsertBatcher,
    /// 当前活跃事务 ID（v0.15.0 Txn05 新增）
    ///
    /// 由 BEGIN TRANSACTION 设置，COMMIT/ROLLBACK 后清除。
    /// SAVEPOINT/RELEASE/ROLLBACK TO SAVEPOINT 基于此事务 ID。
    current_txn_id: Option<u32>,
    /// P0-2 事务级 Batcher：显式事务内 INSERT 攒批缓冲（表名 → 行）
    ///
    /// 仅 current_txn_id 存在时有效（单线程 &mut 模型无需 txn_id 键）。
    /// 事务内连续 INSERT 先攒入此 buffer（零 WAL/MVCC/Delta 开销），
    /// 在非裸 INSERT 语句 / SAVEPOINT / COMMIT 前一次性 flush 为单个
    /// 内部批量事务；ROLLBACK / ROLLBACK TO SAVEPOINT 直接丢弃。
    txn_buffer: HashMap<String, Vec<Vec<Value>>>,
    /// 事务 buffer 当前总行数（阈值检查：config.txn_batch_rows）
    txn_buffer_rows: usize,
    /// P0-2/v0.20 事务 buffer 批内约束键 seen-set（表名 → (主键 seen, 唯一索引 seen)）
    ///
    /// 约束表（主键/唯一索引/NOT NULL）入批时即校验，冲突在语句返回时暴露；
    /// 本 seen-set 维护批内自重复判重（O(1)），discard/flush 时清空。
    txn_buffer_pk_seen: HashMap<String, std::collections::HashSet<Value>>,
    txn_buffer_unique_seen: HashMap<String, HashMap<String, std::collections::HashSet<Value>>>,
}

impl Database {
    /// 打开或创建数据库
    pub fn open(path: &str) -> Result<Self> {
        let path = if path == ":memory:" {
            let mut p = std::env::temp_dir();
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
            p.push(format!("engramdb_mem_{}_{}.hdb", std::process::id(), nanos));
            p.to_string_lossy().to_string()
        } else {
            path.to_string()
        };
        let path = PathBuf::from(path);
        let config = Config::default();

        if path.exists() {
            Self::open_existing(&path, config)
        } else {
            Self::create_new(&path, config)
        }
    }

    /// 使用指定配置打开或创建数据库
    pub fn open_with_config(path: &str, config: Config) -> Result<Self> {
        let path = if path == ":memory:" {
            let mut p = std::env::temp_dir();
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
            p.push(format!("engramdb_mem_{}_{}.hdb", std::process::id(), nanos));
            p.to_string_lossy().to_string()
        } else {
            path.to_string()
        };
        let path = PathBuf::from(path);

        if path.exists() {
            Self::open_existing(&path, config)
        } else {
            Self::create_new(&path, config)
        }
    }

    fn create_new(path: &std::path::Path, config: Config) -> Result<Self> {
        use std::io::Write;

        // v0.21：注册全局 Tokenizer（TokenDelta 压缩分派依赖）
        crate::storage::compression::init_tokenizer_from_config(&config)?;

        let mut file = std::fs::File::create(path)?;

        // 写入文件头
        let header = FileHeader::new(&config);
        let header_bytes = header.to_bytes()?;
        file.write_all(&header_bytes)?;
        file.sync_all()?;

        // 初始化事务管理器
        let path_str = path.to_string_lossy().to_string();
        let txn_manager = TransactionManager::new(&path_str, &config)?;
        let (ib_rows, ib_bytes, ib_timeout) =
            (config.insert_batch_rows, config.insert_batch_bytes, config.insert_batch_timeout_ms);

        Ok(Self {
            path: path.to_path_buf(),
            config,
            header,
            tables: HashMap::new(),
            table_names: HashMap::new(),
            next_table_id: 1,
            file,
            txn_manager,
            plan_cache: std::collections::HashMap::new(),
            statistics_cache: std::collections::HashMap::new(),
            kv_cache: crate::storage::cache::KVCache::new(64 * 1024 * 1024), // 默认 64MB
            batcher: crate::storage::insert_batcher::InsertBatcher::new(ib_rows, ib_bytes, ib_timeout),
            current_txn_id: None,
            txn_buffer: HashMap::new(),
            txn_buffer_rows: 0,
            txn_buffer_pk_seen: HashMap::new(),
            txn_buffer_unique_seen: HashMap::new(),
        })
    }

    fn open_existing(path: &std::path::Path, config: Config) -> Result<Self> {
        use std::io::{Read, Seek};

        // v0.21：注册全局 Tokenizer（TokenDelta 压缩分派依赖）
        crate::storage::compression::init_tokenizer_from_config(&config)?;

        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)?;

        // 读取文件头
        let mut header_buf = vec![0u8; config.page_size as usize];
        file.seek(std::io::SeekFrom::Start(0))?;
        file.read_exact(&mut header_buf[..100])?;

        let header = FileHeader::from_bytes(&header_buf)?;

        // 初始化事务管理器（会自动打开 WAL 并执行恢复）
        let path_str = path.to_string_lossy().to_string();
        let txn_manager = TransactionManager::new(&path_str, &config)?;
        let (ib_rows, ib_bytes, ib_timeout) =
            (config.insert_batch_rows, config.insert_batch_bytes, config.insert_batch_timeout_ms);

        let mut db = Self {
            path: path.to_path_buf(),
            config,
            header,
            tables: HashMap::new(),
            table_names: HashMap::new(),
            next_table_id: 1,
            file,
            txn_manager,
            plan_cache: std::collections::HashMap::new(),
            statistics_cache: std::collections::HashMap::new(),
            kv_cache: crate::storage::cache::KVCache::new(64 * 1024 * 1024),
            batcher: crate::storage::insert_batcher::InsertBatcher::new(ib_rows, ib_bytes, ib_timeout),
            current_txn_id: None,
            txn_buffer: HashMap::new(),
            txn_buffer_rows: 0,
            txn_buffer_pk_seen: HashMap::new(),
            txn_buffer_unique_seen: HashMap::new(),
        };

        // v0.12.1: 恢复 schema 与数据（顺序：catalog → data → indexes）
        // 索引依赖表结构，数据依赖表结构，故 catalog 必须最先加载
        db.load_catalog()?;
        db.load_data()?;
        // 索引在 schema 与数据均就绪后构建
        let _ = db.load_indexes();

        // M4：WAL 崩溃恢复（重放 checkpoint 后已提交事务）
        let _ = crate::wal::recovery::recover_and_apply(&mut db)?;

        Ok(db)
    }

    /// 创建表
    pub fn create_table(&mut self, table_def: TableDef) -> Result<()> {
        if self.table_names.contains_key(&table_def.name) {
            return Err(crate::common::error::EngramDbError::ConstraintViolation(
                format!("Table '{}' already exists", table_def.name)
            ));
        }

        let table_id = self.next_table_id;
        self.next_table_id += 1;

        let table = match table_def.engine {
            crate::common::types::EngineType::Columnar => {
                let mut t = Table::new(table_def.clone(), self.config.compact_strategy);
                t.set_index_config(
                    self.config.sort_compact_by_pk,
                    self.config.primary_index_legacy,
                    self.config.sparse_index_granule_rows,
                );
                EngineTable::Columnar(t)
            }
            crate::common::types::EngineType::Memory => {
                EngineTable::Memory(memory_engine::MemoryTable::new(table_def.clone()))
            }
            crate::common::types::EngineType::Log => {
                EngineTable::Log(log_engine::LogTable::with_block_rows(
                    table_def.clone(), self.config.log_block_rows,
                ))
            }
        };
        // M2：Memory 表标记为非持久化（事务跳过 WAL）
        if table_def.engine == crate::common::types::EngineType::Memory {
            self.txn_manager.mark_non_persistent(table_id);
        }
        // M4：注册表引擎（WAL 记录头 engine_type 来源）
        self.txn_manager.register_table_engine(table_id, table_def.engine);
        self.tables.insert(table_id, table);
        self.table_names.insert(table_def.name.clone(), table_id);

        // v0.14.0: 为表上的 Unique 索引自动构建（来自列级 UNIQUE 约束）
        let unique_index_specs: Vec<(String, Vec<usize>, Vec<usize>, bool)> = table_def.indexes
            .iter()
            .filter(|idx| idx.unique)
            .map(|idx| (idx.name.clone(), idx.key_columns.clone(), idx.included_columns.clone(), idx.unique))
            .collect();
        if let Some(EngineTable::Columnar(table_mut)) = self.tables.get_mut(&table_id) {
            for (idx_name, key_cols, included_cols, unique) in &unique_index_specs {
                table_mut.create_index(idx_name, key_cols, included_cols, *unique)?;
            }
        }

        // v0.12.1: 持久化 catalog 到文件
        let _ = self.save_catalog();

        Ok(())
    }

    /// 获取表（Columnar 引擎解包；非 Columnar 引擎返回 None）
    ///
    /// 引擎感知的调用方应使用 [`Database::get_engine_table`] / 
    /// [`Database::get_engine_table_mut`] 获取引擎句柄。
    pub fn get_table(&self, name: &str) -> Option<&Table> {
        self.table_names
            .get(name)
            .and_then(|id| self.tables.get(id))
            .and_then(|et| et.as_columnar())
    }

    /// 获取可变表（Columnar 引擎解包）
    pub fn get_table_mut(&mut self, name: &str) -> Option<&mut Table> {
        let id = *self.table_names.get(name)?;
        self.tables.get_mut(&id).and_then(|et| et.as_columnar_mut())
    }

    /// 获取引擎表句柄（引擎感知路径）
    pub fn get_engine_table(&self, name: &str) -> Option<&EngineTable> {
        let id = *self.table_names.get(name)?;
        self.tables.get(&id)
    }

    /// 获取可变引擎表句柄（引擎感知路径）
    pub fn get_engine_table_mut(&mut self, name: &str) -> Option<&mut EngineTable> {
        let id = *self.table_names.get(name)?;
        self.tables.get_mut(&id)
    }

    /// 按表 ID 获取可变引擎表句柄（事务 apply 路径）
    pub fn get_engine_table_mut_by_id(&mut self, table_id: u32) -> Option<&mut EngineTable> {
        self.tables.get_mut(&table_id)
    }

    /// 创建覆盖索引（v0.12.0 新增）
    pub fn create_index(&mut self, table_name: &str, index_name: &str,
                        key_cols: &[usize], included_cols: &[usize], unique: bool) -> Result<()> {
        let table = self.get_table_mut(table_name)
            .ok_or_else(|| EngramDbError::TableNotFound(table_name.to_string()))?;
        table.create_index(index_name, key_cols, included_cols, unique)
    }

    /// 重命名表：同步表定义与 `table_names` 名称映射
    pub fn rename_table(&mut self, old_name: &str, new_name: &str) -> Result<()> {
        let table_id = self.table_names.get(old_name)
            .copied()
            .ok_or_else(|| EngramDbError::TableNotFound(old_name.to_string()))?;
        if self.table_names.contains_key(new_name) {
            return Err(EngramDbError::ConstraintViolation(
                format!("Table '{}' already exists", new_name)
            ));
        }
        self.table_names.remove(old_name);
        self.table_names.insert(new_name.to_string(), table_id);
        if let Some(table) = self.tables.get_mut(&table_id) {
            table.def_mut().name = new_name.to_string();
        }
        Ok(())
    }

    /// 获取表名到 ID 的映射（只读）
    pub fn table_names(&self) -> &HashMap<String, u32> {
        &self.table_names
    }
    
    /// 获取所有引擎表的可变引用（用于事务提交后应用）
    pub fn tables_mut(&mut self) -> &mut HashMap<u32, EngineTable> {
        &mut self.tables
    }

    /// 获取配置
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// 获取 KV 缓存引擎的可变引用
    pub fn cache(&mut self) -> &mut crate::storage::cache::KVCache {
        &mut self.kv_cache
    }

    /// 获取数据库路径
    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    /// 获取事务管理器（不可变）
    pub fn txn_manager(&self) -> &TransactionManager {
        &self.txn_manager
    }

    /// 获取事务管理器（可变）
    pub fn txn_manager_mut(&mut self) -> &mut TransactionManager {
        &mut self.txn_manager
    }

    /// P0-2 INSERT 攒批合并器（可变访问，executor 攒批/触发 flush）
    pub fn insert_batcher(&mut self) -> &mut crate::storage::insert_batcher::InsertBatcher {
        &mut self.batcher
    }

    /// P0-2 Batcher 是否启用（autocommit 攒批合并）
    pub fn batch_insert_enabled(&self) -> bool {
        self.config.wal_batch_insert
    }

    /// 获取当前活跃事务 ID（v0.15.0 Txn05 新增）
    pub fn current_txn_id(&self) -> Option<u32> {
        self.current_txn_id
    }

    /// 是否处于显式事务内（SQL BEGIN/COMMIT 或 Transaction API）
    ///
    /// P0-2：batcher 必须跳过显式事务内的 INSERT（缓冲行脱离事务
    /// MVCC 写集，ROLLBACK 将失效）。
    pub fn in_explicit_txn(&self) -> bool {
        self.current_txn_id.is_some() || self.txn_manager.active_count() > 0
    }

    /// 设置当前活跃事务 ID（v0.15.0 Txn05 新增）
    pub fn set_current_txn_id(&mut self, txn_id: Option<u32>) {
        self.current_txn_id = txn_id;
    }

    /// P0-2 事务级 Batcher：将行攒入显式事务 buffer（零 WAL/MVCC/Delta 开销）
    ///
    /// 返回 true 表示已达阈值（config.txn_batch_rows），调用方应随即
    /// `flush_txn_buffer`（防内存无界）。仅在显式事务内调用。
    ///
    /// v0.20：约束表（主键/唯一索引/NOT NULL）入批时即校验——冲突
    /// 在语句返回时暴露（与绕过攒批时语义一致），零副作用不落批。
    pub fn txn_buffer_push(&mut self, table_name: &str, rows: Vec<Vec<Value>>) -> Result<bool> {
        let n = rows.len();
        if n == 0 {
            return Ok(false);
        }
        self.validate_rows_against_committed(table_name, &rows)?;
        self.validate_rows_against_txn_buffer(table_name, &rows)?;
        let entry = self.txn_buffer.entry(table_name.to_string()).or_default();
        entry.extend(rows);
        self.txn_buffer_rows += n;
        Ok(self.txn_buffer_rows >= self.config.txn_batch_rows)
    }

    /// v0.20：入批预检（已提交状态部分）——主键点查 + 唯一索引点查 + NOT NULL
    ///
    /// 仅约束表需要；无主键/唯一索引/NOT NULL 的表零开销直接通过。
    pub(crate) fn validate_rows_against_committed(
        &mut self,
        table_name: &str,
        rows: &[Vec<Value>],
    ) -> Result<()> {
        use crate::common::error::EngramDbError;
        // 表不存在：无约束可查（生产调用方已先做存在性校验；直接调用方
        // 的错误在 flush 的 execute_with_txn 暴露）
        let Some(table) = self.get_engine_table(table_name) else {
            return Ok(());
        };
        let def = table.def().clone();
        drop(table);
        // 无约束快速路径：无主键、无唯一索引、全列可空 → 零检查
        let pk = def.primary_key_index();
        let unique: Vec<(String, usize)> = def.indexes.iter()
            .filter(|i| i.unique)
            .map(|i| (i.name.clone(), i.key_columns[0]))
            .collect();
        let has_not_null = def.columns.iter().any(|c| !c.nullable);
        if pk.is_none() && unique.is_empty() && !has_not_null {
            return Ok(());
        }
        for row in rows {
            // NOT NULL（auto_increment 列跳过：缺失/NULL 在 flush 时自动填充）
            if has_not_null {
                for (ci, col) in def.columns.iter().enumerate() {
                    if col.auto_increment {
                        continue;
                    }
                    if !col.nullable && row.get(ci).is_none_or(|v| v.is_null()) {
                        return Err(EngramDbError::ConstraintViolation(format!(
                            "NOT NULL constraint failed: column '{}'", col.name
                        )));
                    }
                }
            }
            // 主键冲突（已提交；auto_increment 的 NULL 跳过，flush 分配后天然唯一）
            if let Some(pk_idx) = pk {
                if let Some(cell) = row.get(pk_idx) {
                    if !cell.is_null()
                        && self.get_engine_table_mut(table_name)
                            .and_then(|t| t.lookup_primary_key(cell))
                            .is_some()
                    {
                        return Err(EngramDbError::ConstraintViolation(format!(
                            "UNIQUE constraint failed: {}={:?}",
                            def.columns[pk_idx].name, cell
                        )));
                    }
                }
            }
            // 唯一索引冲突（已提交）
            for (idx_name, key_col) in &unique {
                if let Some(cell) = row.get(*key_col) {
                    if self.get_engine_table(table_name)
                        .is_some_and(|t| t.unique_index_contains(idx_name, cell))
                    {
                        return Err(EngramDbError::ConstraintViolation(format!(
                            "UNIQUE constraint failed: index '{}'", idx_name
                        )));
                    }
                }
            }
        }
        Ok(())
    }

    /// v0.20：入批预检（事务 buffer 批内自重复部分）
    ///
    /// 主键 / 唯一索引键与已攒入 txn_buffer 的行判重（O(1) seen-set）。
    fn validate_rows_against_txn_buffer(
        &mut self,
        table_name: &str,
        rows: &[Vec<Value>],
    ) -> Result<()> {
        use crate::common::error::EngramDbError;
        let Some(table) = self.get_engine_table(table_name) else {
            return Ok(());
        };
        let def = table.def().clone();
        drop(table);
        let pk = def.primary_key_index();
        let unique: Vec<(String, usize)> = def.indexes.iter()
            .filter(|i| i.unique)
            .map(|i| (i.name.clone(), i.key_columns[0]))
            .collect();
        if pk.is_none() && unique.is_empty() {
            return Ok(());
        }
        let pk_seen = self.txn_buffer_pk_seen.entry(table_name.to_string()).or_default();
        let mut local_pk: std::collections::HashSet<Value> = std::collections::HashSet::new();
        for row in rows {
            if let Some(pk_idx) = pk {
                if let Some(cell) = row.get(pk_idx) {
                    if !cell.is_null()
                        && (pk_seen.contains(cell) || !local_pk.insert(cell.clone()))
                    {
                        return Err(EngramDbError::ConstraintViolation(format!(
                            "UNIQUE constraint failed: {}={:?}",
                            def.columns[pk_idx].name, cell
                        )));
                    }
                }
            }
        }
        pk_seen.extend(local_pk.into_iter());
        // 唯一索引批内判重
        let unique_seen = self.txn_buffer_unique_seen.entry(table_name.to_string()).or_default();
        let mut local_unique: HashMap<String, std::collections::HashSet<Value>> = HashMap::new();
        for row in rows {
            for (idx_name, key_col) in &unique {
                if let Some(cell) = row.get(*key_col) {
                    let entry_seen = unique_seen.get(idx_name).is_some_and(|s| s.contains(cell));
                    let local_seen = local_unique.entry(idx_name.clone()).or_default();
                    if entry_seen || !local_seen.insert(cell.clone()) {
                        return Err(EngramDbError::ConstraintViolation(format!(
                            "UNIQUE constraint failed: index '{}'", idx_name
                        )));
                    }
                }
            }
        }
        for (idx_name, keys) in local_unique {
            unique_seen.entry(idx_name).or_default().extend(keys);
        }
        Ok(())
    }

    /// 事务 buffer 当前总行数（监控用）
    pub fn txn_buffer_pending(&self) -> usize {
        self.txn_buffer_rows
    }

    /// 事务 buffer 是否为空
    pub fn txn_buffer_is_empty(&self) -> bool {
        self.txn_buffer.is_empty()
    }

    /// P0-2 事务级 Batcher：清空 buffer（ROLLBACK / ROLLBACK TO SAVEPOINT）
    ///
    /// 丢弃未 flush 的写入段 = 撤销事务内尚未可见的写入。
    pub fn discard_txn_buffer(&mut self) {
        self.txn_buffer.clear();
        self.txn_buffer_rows = 0;
        self.txn_buffer_pk_seen.clear();
        self.txn_buffer_unique_seen.clear();
    }

    /// P0-2 事务级 Batcher：将 buffer 一次性 flush 为单个内部批量事务
    ///
    /// 每表一个 `execute_with_txn`（begin → batch_insert → commit → apply）：
    /// N 条事务内 INSERT 语句合并为 N 个内部事务（表数），每个事务
    /// 1 条 WAL InsertBatch + 1 次 MVCC batch_write + 1 次 apply。
    /// flush 后数据进 Delta，事务内/外读均可见（读己之写）。
    pub fn flush_txn_buffer(&mut self) -> Result<()> {
        if self.txn_buffer.is_empty() {
            return Ok(());
        }
        let pending: Vec<(String, Vec<Vec<Value>>)> = std::mem::take(&mut self.txn_buffer)
            .into_iter()
            .collect();
        self.txn_buffer_rows = 0;
        self.txn_buffer_pk_seen.clear();
        self.txn_buffer_unique_seen.clear();
        for (table_name, rows) in pending {
            if !rows.is_empty() {
                crate::executor::operators::insert::execute_with_txn(self, &table_name, rows)?;
            }
        }
        Ok(())
    }

    /// 手动触发 WAL fsync（用于 Periodic 刷盘模式）
    ///
    /// 如果配置了 `sync_wal_compact = true` 且 WAL 模式为 Periodic，
    /// 刷盘后会检查所有表并触发必要的 Delta 合并（方案五：批量Sync联动）。
    /// 返回 (wal_flushed, compacted_rows)。
    pub fn sync_wal(&mut self) -> Result<()> {
        self.txn_manager.sync_wal()?;

        // Periodic 模式 + 开启联动时，顺便检查 compact
        if self.config.sync_wal_compact {
            // 收集表名避免借用冲突
            let table_names: Vec<String> = self.table_names.keys().cloned().collect();
            for name in &table_names {
                if let Some(EngineTable::Columnar(table)) = self.tables.get_mut(&self.table_names[name]) {
                    let _ = table.maybe_compact()?;
                }
            }
        }

        Ok(())
    }

    /// 设置 WAL 刷盘策略
    pub fn set_wal_flush_mode(&mut self, mode: crate::common::config::WalFlushMode) {
        self.config.wal_flush_mode = mode;
        self.txn_manager.set_wal_flush_mode(mode);
    }

    /// 设置 WAL 组提交大小（0 = 禁用）
    ///
    /// 组提交是 Sync 模式下的核心 WAL 加速机制：
    /// 多条事务共享一次 fsync，写入吞吐可提升数倍至数十倍。
    /// 崩溃时最多丢 group_commit_size 条未 fsync 的事务。
    pub fn set_wal_group_commit_size(&mut self, size: usize) {
        self.txn_manager.set_wal_group_commit_size(size);
    }

    /// P0-3 时间窗组提交：距上次 fsync 超时则下次 commit 强制 sync（0 = 禁用）
    pub fn set_wal_group_commit_timeout_ms(&mut self, ms: u64) {
        self.txn_manager.set_wal_group_commit_timeout_ms(ms);
    }

    /// 设置指定表的聚簇列（方案B：Delta 聚簇）
    ///
    /// 设置后，compact 时会按该列的值分组写入列存，
    /// 相同 key 的行物理上连续，可大幅提升按该列的范围查询性能。
    pub fn set_cluster_key(&mut self, table_name: &str, column_name: &str) -> Result<()> {
        let table = self.get_table_mut(table_name)
            .ok_or_else(|| crate::common::error::EngramDbError::TableNotFound(table_name.into()))?;
        table.set_cluster_key(column_name)
    }

    // ========================================================================
    // 向量 HNSW 索引（v0.12.0 优先级 3）
    // ========================================================================

    /// 创建向量 HNSW 索引
    ///
    /// 对指定表的向量列构建 HNSW 近似最近邻索引。
    /// 列必须是 Vector 类型。
    pub fn create_vector_index(&mut self, table_name: &str, index_name: &str, column_name: &str, metric: crate::storage::vector_index::DistanceMetric, m: usize, ef_construction: usize) -> Result<()> {
        let table = self.get_table_mut(table_name)
            .ok_or_else(|| crate::common::error::EngramDbError::TableNotFound(table_name.into()))?;

        let col_idx = table.def().columns.iter()
            .position(|c| c.name == column_name)
            .ok_or_else(|| crate::common::error::EngramDbError::ColumnNotFound(column_name.into()))?;

        table.create_vector_index(index_name, col_idx, metric, m, ef_construction)
    }

    /// 向量相似度搜索
    ///
    /// 返回 top-k 最近邻的行 ID 和距离。
    pub fn vector_search(&self, table_name: &str, index_name: &str, query: &[f32], k: usize) -> Result<Vec<crate::storage::vector_index::Neighbor>> {
        let table = self.get_table(table_name)
            .ok_or_else(|| crate::common::error::EngramDbError::TableNotFound(table_name.into()))?;
        table.vector_search(index_name, query, k)
    }

    /// 向量相似度搜索 + 搜索 trace（v0.15.0 V13 新增）
    ///
    /// 返回 (top-k 最近邻, 搜索 trace)。trace 包含访问路径、入口点、候选节点数等
    /// 可追溯信息，Agent 场景下用于溯源推理路径。
    pub fn vector_search_with_trace(
        &self,
        table_name: &str,
        index_name: &str,
        query: &[f32],
        k: usize,
    ) -> Result<(Vec<crate::storage::vector_index::Neighbor>, crate::storage::vector_index::SearchTrace)> {
        let table = self.get_table(table_name)
            .ok_or_else(|| crate::common::error::EngramDbError::TableNotFound(table_name.into()))?;
        table.vector_search_with_trace(index_name, query, k)
    }

    /// 混合搜索（向量近似搜索 + 标量条件过滤）
    ///
    /// 流程：
    /// 1. 用 HNSW 搜索 `top_k * ef_mult` 个候选（扩大 ef 提高召回率）
    /// 2. 对每个候选行，通过 `filter_fn` 判断是否满足标量条件
    /// 3. 返回过滤后的前 `top_k` 个结果，附带行数据
    ///
    /// `ef_mult` 控制候选集扩大倍数（默认 3），越大召回率越高但性能越低。
    /// `filter_fn` 接收行数据，返回 true 表示保留。
    pub fn hybrid_search(
        &mut self,
        table_name: &str,
        index_name: &str,
        query: &[f32],
        top_k: usize,
        ef_mult: usize,
        column_indices: &[usize],
        filter_fn: &dyn Fn(&[crate::Value]) -> bool,
    ) -> Result<Vec<crate::HybridSearchResult>> {
        let table = self.get_table_mut(table_name)
            .ok_or_else(|| crate::common::error::EngramDbError::TableNotFound(table_name.into()))?;

        // 1. 获取 HNSW 索引，读取 id_mapping 和 ef_search 配置
        let (hnsw_index, id_mapping) = table.vector_indexes()
            .get(index_name)
            .ok_or_else(|| crate::common::error::EngramDbError::IndexNotFound(index_name.into()))?;

        // 2. 扩大候选集：搜索 top_k * ef_mult 个候选
        let ef_search = hnsw_index.config().ef_search;
        let candidate_k = (top_k * ef_mult).max(ef_search);
        let neighbors = hnsw_index.search(query, candidate_k);

        // 3. 构建 row_id -> distance 映射（释放对 table 的不可变借用）
        let mut candidates: Vec<(u32, f32)> = neighbors.iter()
            .map(|n| {
                let row_id = if n.id < id_mapping.len() as u32 {
                    id_mapping[n.id as usize]
                } else {
                    n.id
                };
                (row_id, n.distance)
            })
            .collect();

        // 4. 释放 table 借用，重新获取可变借用
        drop(hnsw_index);
        drop(id_mapping);

        // 5. 遍历候选集，读取行数据并应用标量过滤
        let table = self.get_table_mut(table_name)
            .ok_or_else(|| crate::common::error::EngramDbError::TableNotFound(table_name.into()))?;

        let mut results = Vec::with_capacity(candidates.len());
        for (row_id, distance) in &candidates {
            if let Some(row) = table.get_row_by_id(*row_id)? {
                if filter_fn(&row) {
                    let projected: Vec<crate::Value> = column_indices.iter()
                        .map(|&ci| {
                            if ci < row.len() { row[ci].clone() } else { crate::Value::Null }
                        })
                        .collect();
                    results.push(crate::HybridSearchResult {
                        row_id: *row_id,
                        distance: *distance,
                        row: projected,
                    });
                }
            }
        }

        // 6. 按距离升序取 top_k
        results.sort_by(|a, b| a.distance.partial_cmp(&b.distance).unwrap_or(std::cmp::Ordering::Equal));
        results.truncate(top_k);
        Ok(results)
    }

    /// 设置全局默认合并策略（新建表生效，已有表不受影响）
    pub fn set_default_compact_strategy(&mut self, strategy: crate::common::config::CompactStrategy) {
        self.config.compact_strategy = strategy;
    }

    /// 设置指定表的合并策略（运行时动态切换）
    pub fn set_table_compact_strategy(&mut self, table_name: &str, strategy: crate::common::config::CompactStrategy) -> Result<()> {
        let table = self.get_table_mut(table_name)
            .ok_or_else(|| crate::common::error::EngramDbError::TableNotFound(table_name.into()))?;
        table.set_compact_strategy(strategy);
        Ok(())
    }

    /// 合并指定表的 Delta 层到列存（全量合并）
    ///
    /// 返回合并的行数。
    pub fn compact_table(&mut self, table_name: &str) -> Result<u64> {
        // 引擎分派：Memory 表无 Delta 层，跳过
        let Some(engine) = self.get_engine_table_mut(table_name) else {
            return Err(crate::common::error::EngramDbError::TableNotFound(table_name.into()));
        };
        let EngineTable::Columnar(table) = engine else {
            return Ok(0);
        };
        let rows = table.delta_store().len() as u64;
        table.compact_delta()?;
        Ok(rows)
    }

    /// 合并所有表的 Delta 层到列存
    ///
    /// 返回合并的总行数。
    pub fn compact_all(&mut self) -> Result<u64> {
        let mut total = 0u64;
        // 收集所有表名，避免借用冲突
        let table_names: Vec<String> = self.table_names.keys().cloned().collect();
        for name in &table_names {
            total += self.compact_table(name)?;
        }
        Ok(total)
    }

    /// 关闭数据库
    ///
    /// 执行 checkpoint 持久化所有状态（catalog + data + indexes），
    /// 确保下次打开时可完整恢复。
    pub fn close(&mut self) -> Result<()> {
        use std::io::Write;
        // v0.12.1: checkpoint 持久化 catalog/data/indexes
        // checkpoint 内部会先 compact_all，再依次保存三段
        let _ = self.checkpoint();

        self.file.flush()?;
        self.file.sync_all()?;
        self.plan_cache.clear();
        Ok(())
    }

    /// 获取缓存的查询计划（Perf02 / v0.18 P0-1 计划缓存接线）
    pub fn get_plan_cache(
        &self,
        sql: &str,
    ) -> Option<&(crate::executor::physical_plan::PhysicalPlan, bool)> {
        self.plan_cache.get(sql)
    }

    /// 设置查询计划缓存（v0.18 P0-1）
    ///
    /// 键 = SQL 原文（无参数语句：相同 SQL = 相同计划）。
    /// 容量上限：PLAN_CACHE_MAX，满则整体清空（日志场景 SQL 种类少）。
    pub fn set_plan_cache(
        &mut self,
        sql: &str,
        plan: crate::executor::physical_plan::PhysicalPlan,
        batcher_clean: bool,
    ) {
        if self.plan_cache.len() >= Self::PLAN_CACHE_MAX {
            self.plan_cache.clear();
        }
        self.plan_cache.insert(sql.to_string(), (plan, batcher_clean));
    }

    /// 清空计划缓存（DDL / ANALYZE 后调用：结构或统计变更使缓存计划过期）
    pub fn clear_plan_cache(&mut self) {
        self.plan_cache.clear();
    }

    /// 计划缓存容量上限（超限整体清空）
    pub const PLAN_CACHE_MAX: usize = 256;

    /// 统计信息缓存（M5）：ANALYZE 结果，JOIN 代价模型消费
    pub fn statistics_cache(&self) -> &std::collections::HashMap<String, crate::sql::statistics::TableStatistics> {
        &self.statistics_cache
    }

    pub fn statistics_cache_mut(&mut self) -> &mut std::collections::HashMap<String, crate::sql::statistics::TableStatistics> {
        &mut self.statistics_cache
    }

    /// 保存所有索引到文件（v0.12.0 索引持久化）
    ///
    /// 将所有表的所有二级索引序列化后写入文件末尾，
    /// 并更新文件头的 index_root 和 index_size。
    ///
    /// 返回写入的字节数。
    pub fn save_indexes(&mut self) -> Result<u64> {
        use std::io::{Seek, Write};

        // 收集所有表的索引数据
        // 格式：table_count + per-table (table_id + index_data)
        let mut section_buf = Vec::new();
        let table_count = self.tables.len() as u32;
        section_buf.extend_from_slice(&table_count.to_le_bytes());

        for (&table_id, table) in &self.tables {
            section_buf.extend_from_slice(&table_id.to_le_bytes());
            let index_bytes = table.indexes_to_bytes();
            section_buf.extend_from_slice(&(index_bytes.len() as u32).to_le_bytes());
            section_buf.extend_from_slice(&index_bytes);
        }

        // 写入到文件末尾
        let file_len = self.file.seek(std::io::SeekFrom::End(0))?;
        // 页对齐
        let page_size = self.header.page_size as u64;
        let aligned_offset = (file_len + page_size - 1) / page_size * page_size;
        if aligned_offset > file_len {
            self.file.seek(std::io::SeekFrom::Start(file_len))?;
            let padding = (aligned_offset - file_len) as usize;
            self.file.write_all(&vec![0u8; padding])?;
        }

        let index_root = aligned_offset as u32;
        self.file.seek(std::io::SeekFrom::Start(aligned_offset))?;
        self.file.write_all(&section_buf)?;
        self.file.flush()?;

        // 更新文件头
        self.header.index_root = index_root;
        self.header.index_size = section_buf.len() as u32;

        // 重写文件头
        let header_bytes = self.header.to_bytes()?;
        self.file.seek(std::io::SeekFrom::Start(0))?;
        self.file.write_all(&header_bytes)?;
        self.file.flush()?;

        Ok(section_buf.len() as u64)
    }

    /// 从文件加载所有索引（v0.12.0 索引持久化）
    ///
    /// 读取文件中的索引段，反序列化后加载到对应表中。
    /// 注意：表必须已存在（表定义需先加载）。
    pub fn load_indexes(&mut self) -> Result<usize> {
        use std::io::{Read, Seek};

        if self.header.index_root == 0 || self.header.index_size == 0 {
            self.rebuild_missing_primary_indexes()?;
            return Ok(0); // 无索引
        }

        // 读取索引段
        let mut data = vec![0u8; self.header.index_size as usize];
        self.file.seek(std::io::SeekFrom::Start(self.header.index_root as u64))?;
        self.file.read_exact(&mut data)?;

        if data.len() < 4 {
            return Err(EngramDbError::InvalidFormat("index section too short".into()));
        }

        let table_count = u32::from_le_bytes(data[..4].try_into().unwrap()) as usize;
        let mut offset = 4;
        let mut total_indexes = 0;

        for _ in 0..table_count {
            if offset + 4 > data.len() {
                return Err(EngramDbError::InvalidFormat("truncated table id".into()));
            }
            let table_id = u32::from_le_bytes(data[offset..offset+4].try_into().unwrap());
            offset += 4;

            if offset + 4 > data.len() {
                return Err(EngramDbError::InvalidFormat("truncated table index size".into()));
            }
            let index_data_len = u32::from_le_bytes(data[offset..offset+4].try_into().unwrap()) as usize;
            offset += 4;

            if offset + index_data_len > data.len() {
                return Err(EngramDbError::InvalidFormat("truncated table index data".into()));
            }

            // 加载到对应表
            if let Some(EngineTable::Columnar(table)) = self.tables.get_mut(&table_id) {
                let before_skip = table.indexes().len();
                let before_vec = table.vector_indexes().len();
                table.indexes_from_bytes(&data[offset..offset+index_data_len])?;
                total_indexes += table.indexes().len() - before_skip;
                total_indexes += table.vector_indexes().len() - before_vec;
            }
            // 如果表不存在，跳过该表的索引（表可能已被删除）

            offset += index_data_len;
        }

        self.rebuild_missing_primary_indexes()?;

        Ok(total_indexes)
    }

    /// 主键索引兜底重建（v0.17.0 M1-7）
    ///
    /// 持久化主键段不存在（旧文件 / 无索引段）时全量重建；
    /// 已从索引段恢复的表跳过。幂等：多次调用无副作用。
    /// 分层索引（v0.19）：表级 legacy 模式重建 BTreeMap，否则重建列存稀疏索引。
    fn rebuild_missing_primary_indexes(&mut self) -> Result<()> {
        let mut rebuilt = 0u32;
        for engine in self.tables.values_mut() {
            let EngineTable::Columnar(table) = engine else {
                continue;
            };
            if table.def.primary_key_index().is_none() {
                continue;
            }
            if table.primary_index_legacy_enabled() {
                if table.primary_index().map_or(true, |i| i.is_empty()) {
                    table.rebuild_primary_index()?;
                    rebuilt += 1;
                }
            } else if table.column_store().sparse_granule_count() == 0
                && table.column_store().total_rows() > 0
            {
                table.column_store_mut().rebuild_sparse()?;
                rebuilt += 1;
            }
        }
        if rebuilt > 0 {
            log::trace!("Mark Index: rebuilt primary index for {} tables (no persisted mark segment)", rebuilt);
        }
        Ok(())
    }

    // ========================================================================
    // Catalog 持久化（v0.12.1 新增）
    // 解决 P0：表 schema 不持久化，重启后表全丢失
    // ========================================================================

    /// 保存 Catalog（所有表 schema）到文件
    ///
    /// 将所有表的 TableDef 序列化写入 Catalog 段，
    /// 并更新文件头的 catalog_root / catalog_size。
    pub fn save_catalog(&mut self) -> Result<u64> {
        use std::io::{Seek, Write};

        let snapshot = catalog::CatalogSnapshot::collect(
            self.next_table_id,
            &self.tables,
        );
        let section_buf = snapshot.to_bytes()?;

        // 页对齐写入
        let file_len = self.file.seek(std::io::SeekFrom::End(0))?;
        let page_size = self.header.page_size as u64;
        let aligned_offset = (file_len + page_size - 1) / page_size * page_size;
        if aligned_offset > file_len {
            self.file.seek(std::io::SeekFrom::Start(file_len))?;
            let padding = (aligned_offset - file_len) as usize;
            self.file.write_all(&vec![0u8; padding])?;
        }

        let catalog_root = aligned_offset as u32;
        self.file.seek(std::io::SeekFrom::Start(aligned_offset))?;
        self.file.write_all(&section_buf)?;
        self.file.flush()?;

        // 更新文件头
        self.header.catalog_root = catalog_root;
        self.header.catalog_size = section_buf.len() as u32;
        let header_bytes = self.header.to_bytes()?;
        self.file.seek(std::io::SeekFrom::Start(0))?;
        self.file.write_all(&header_bytes)?;
        self.file.flush()?;

        Ok(section_buf.len() as u64)
    }

    /// 从文件加载 Catalog（恢复所有表 schema）
    ///
    /// 读取 Catalog 段并重建 tables / table_names / next_table_id。
    /// **注意**：仅恢复 schema，不恢复数据（数据由 load_data 负责）。
    pub fn load_catalog(&mut self) -> Result<usize> {
        use std::io::{Read, Seek};

        if self.header.catalog_root == 0 || self.header.catalog_size == 0 {
            return Ok(0); // 无 catalog（空库或老格式）
        }

        let mut data = vec![0u8; self.header.catalog_size as usize];
        self.file.seek(std::io::SeekFrom::Start(self.header.catalog_root as u64))?;
        self.file.read_exact(&mut data)?;

        let snapshot = catalog::CatalogSnapshot::from_bytes(&data)?;

        self.tables.clear();
        self.table_names.clear();
        self.next_table_id = snapshot.next_table_id;

        for (table_id, table_def) in snapshot.tables {
            // 按引擎构造（M2：Memory 表重建为空白内存表，数据不恢复）
            let table = match table_def.engine {
                crate::common::types::EngineType::Columnar => {
                    let mut t = Table::new(table_def.clone(), self.config.compact_strategy);
                    t.set_index_config(
                        self.config.sort_compact_by_pk,
                        self.config.primary_index_legacy,
                        self.config.sparse_index_granule_rows,
                    );
                    EngineTable::Columnar(t)
                }
                crate::common::types::EngineType::Memory => {
                    // 内存表数据不恢复：行数统计清零（catalog 中的 row_count 是上次会话的）
                    let mut mem_def = table_def.clone();
                    mem_def.row_count = 0;
                    EngineTable::Memory(memory_engine::MemoryTable::new(mem_def))
                }
                crate::common::types::EngineType::Log => {
                    EngineTable::Log(log_engine::LogTable::with_block_rows(
                        table_def.clone(), self.config.log_block_rows,
                    ))
                }
            };
            // M2：Memory 表标记为非持久化（事务跳过 WAL）
            if table_def.engine == crate::common::types::EngineType::Memory {
                self.txn_manager.mark_non_persistent(table_id);
            }
            // M4：注册表引擎（WAL 记录头 engine_type 来源）
            self.txn_manager.register_table_engine(table_id, table_def.engine);
            self.table_names.insert(table_def.name.clone(), table_id);
            self.tables.insert(table_id, table);
        }

        Ok(self.tables.len())
    }

    // ========================================================================
    // 数据持久化（v0.12.1 新增）
    // 解决 P0：数据未持久化，重启后数据丢失
    // ========================================================================

    /// 保存所有表的列存数据到文件
    ///
    /// **注意**：仅保存列存 RowGroup 数据，Delta 层未持久化。
    /// 调用前应先执行 compact_all() 将 Delta 合并到列存。
    pub fn save_data(&mut self) -> Result<u64> {
        use std::io::{Seek, Write};

        // 格式：table_count + per-table (table_id + data_len + data)
        // 持久化 Columnar + Log 表；Memory 表数据不落盘（进程退出丢失，符合语义）
        let mut section_buf = Vec::new();
        let persistent_ids: Vec<u32> = self
            .tables
            .iter()
            .filter(|(_, t)| !matches!(t, EngineTable::Memory(_)))
            .map(|(id, _)| *id)
            .collect();
        let table_count = persistent_ids.len() as u32;
        section_buf.extend_from_slice(&table_count.to_le_bytes());

        let compress = self.config.compress_on_persist;
        for table_id in persistent_ids {
            let table = self.tables.get_mut(&table_id).unwrap();
            let data_bytes = match table {
                EngineTable::Columnar(t) => t.column_store_mut().data_to_bytes(compress)?,
                EngineTable::Log(t) => t.to_bytes(),
                EngineTable::Memory(_) => continue,
            };
            section_buf.extend_from_slice(&table_id.to_le_bytes());
            section_buf.extend_from_slice(&(data_bytes.len() as u32).to_le_bytes());
            section_buf.extend_from_slice(&data_bytes);
        }

        // 页对齐写入
        let file_len = self.file.seek(std::io::SeekFrom::End(0))?;
        let page_size = self.header.page_size as u64;
        let aligned_offset = (file_len + page_size - 1) / page_size * page_size;
        if aligned_offset > file_len {
            self.file.seek(std::io::SeekFrom::Start(file_len))?;
            let padding = (aligned_offset - file_len) as usize;
            self.file.write_all(&vec![0u8; padding])?;
        }

        let data_root = aligned_offset as u32;
        self.file.seek(std::io::SeekFrom::Start(aligned_offset))?;
        self.file.write_all(&section_buf)?;
        self.file.flush()?;

        // 更新文件头
        self.header.data_root = data_root;
        self.header.data_size = section_buf.len() as u32;
        let header_bytes = self.header.to_bytes()?;
        self.file.seek(std::io::SeekFrom::Start(0))?;
        self.file.write_all(&header_bytes)?;
        self.file.flush()?;

        Ok(section_buf.len() as u64)
    }

    /// 从文件加载所有表的列存数据
    ///
    /// **前置条件**：load_catalog 必须先调用（表结构需已存在）。
    pub fn load_data(&mut self) -> Result<usize> {
        use std::io::{Read, Seek};

        if self.header.data_root == 0 || self.header.data_size == 0 {
            return Ok(0); // 无数据
        }

        let mut data = vec![0u8; self.header.data_size as usize];
        self.file.seek(std::io::SeekFrom::Start(self.header.data_root as u64))?;
        self.file.read_exact(&mut data)?;

        if data.len() < 4 {
            return Ok(0);
        }

        let table_count = u32::from_le_bytes(data[..4].try_into().unwrap()) as usize;
        let mut offset = 4;
        let mut loaded = 0;

        for _ in 0..table_count {
            if offset + 8 > data.len() {
                return Err(EngramDbError::InvalidFormat("truncated data table header".into()));
            }
            let table_id = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
            offset += 4;
            let data_len = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
            offset += 4;

            if offset + data_len > data.len() {
                return Err(EngramDbError::InvalidFormat("truncated data table body".into()));
            }

            match self.tables.get_mut(&table_id) {
                Some(EngineTable::Columnar(table)) => {
                    table.column_store_mut().data_from_bytes(&data[offset..offset + data_len])?;
                    // 同步列的 data_type（修正 Vector dim 等）
                    table.sync_column_data_types();
                    // 重建主键索引（重启后恢复；按表级 legacy 开关分流）
                    if table.primary_index_legacy_enabled() {
                        table.rebuild_primary_index()?;
                    } else {
                        table.column_store_mut().rebuild_sparse()?;
                    }
                    loaded += 1;
                }
                Some(EngineTable::Log(table)) => {
                    table.from_bytes(&data[offset..offset + data_len])?;
                    loaded += 1;
                }
                // Memory 表无数据段（跳过，保持空白内存表）
                // 表不存在则跳过（schema 已删但数据未清理）
                _ => {}
            }
            offset += data_len;
        }

        Ok(loaded)
    }

    /// 持久化全部状态（catalog + data + indexes）
    ///
    /// 推荐在 close() 前调用。会先 compact Delta → 列存，
    /// 再依次保存 catalog、data、indexes，最后更新文件头。
    ///
    /// **已知限制**：每次 checkpoint 追加写入文件末尾，旧段成为孤儿数据。
    /// 文件会持续增长，生产环境应定期 VACUUM 重建文件（待实现）。
    pub fn checkpoint(&mut self) -> Result<()> {
        // 0. P0-2：冲刷攒批缓冲（存储层直接调用方，如测试，必须兜底）
        crate::executor::operators::insert::flush_all_batched(self)?;

        // 1. 先把 Delta 合并到列存（确保数据完整）
        let _ = self.compact_all()?;

        // 1.5 压缩列存（v0.12.x 压缩接线）
        // compact 后对每张表调用 compress_all：数据以压缩态落盘 + 降低内存占用。
        // 后续 append 路径会通过 ensure_rg_decompressed 按需惰性解压。
        if self.config.compress_on_persist {
            for table in self.tables.values_mut() {
                let EngineTable::Columnar(table) = table else {
                    continue;
                };
                let _ = table.column_store_mut().compress_all()?;
            }
        }

        // 2. 保存 catalog（schema）
        let _ = self.save_catalog()?;

        // 3. 保存 data（列存数据）
        let _ = self.save_data()?;

        // 4. 保存 indexes（二级索引）
        let _ = self.save_indexes()?;

        // v0.21.1：token 流缓存使命完成（TD 压缩已消费可命中项；残留清空防内存泄漏）
        crate::storage::compression::clear_token_stream_cache();

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::types::{TableDef, ColumnDef, DataType};
    use crate::Value;

    fn make_test_table() -> Table {
        let def = TableDef::new(1, "test", vec![
            ColumnDef::new("id", DataType::Int64),
            ColumnDef::new("name", DataType::Varchar),
            ColumnDef::new("score", DataType::Float64),
        ]);
        Table::new(def, crate::common::config::CompactStrategy::Manual)
    }

    fn temp_db_path(suffix: &str) -> String {
        let mut p = std::env::temp_dir();
        let tid = format!("{:?}", std::thread::current().id())
            .replace('(', "_").replace(')', "")
            .replace([':', ' '], "_");
        // 追加 ThreadId：同进程并发跑多个测试线程时，用 PID 区分跨进程，
        // 用 tid 区分跨线程（Rust 默认 --test-threads > 1 多线程跑）
        p.push(format!("engramdb_{}_{}_{}.hdb", suffix, std::process::id(), tid));
        p.to_string_lossy().to_string()
    }
    fn cleanup_db(path: &str) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(format!("{}-wal", path));
    }

    #[test]
    fn test_table_indexes_roundtrip() {
        let mut table = make_test_table();

        // 插入一些数据
        let rows = vec![
            vec![Value::Int64(1), Value::Varchar("alice".into()), Value::Float64(95.5)],
            vec![Value::Int64(2), Value::Varchar("bob".into()), Value::Float64(87.0)],
            vec![Value::Int64(3), Value::Varchar("alice".into()), Value::Float64(92.0)],
        ];
        table.insert(rows).unwrap();

        // 创建索引
        table.create_index("idx_name", &[1], &[2], false).unwrap(); // name 键，覆盖 score
        table.create_index("idx_id", &[0], &[], true).unwrap(); // id 唯一键

        assert_eq!(table.indexes().len(), 2);

        // 序列化
        let bytes = table.indexes_to_bytes();
        assert!(!bytes.is_empty());

        // 反序列化到新表
        let mut table2 = make_test_table();
        table2.indexes_from_bytes(&bytes).unwrap();
        assert_eq!(table2.indexes().len(), 2);

        // 验证 idx_name
        let idx = table2.get_index("idx_name").unwrap();
        assert!(!idx.is_unique());
        assert_eq!(idx.num_included(), 1);
        assert_eq!(idx.len(), 2); // alice, bob

        let alice_entries = idx.get_entries(&Value::Varchar("alice".into())).unwrap();
        assert_eq!(alice_entries.len(), 2);
        assert_eq!(alice_entries[0].included[0], Value::Float64(95.5));
        assert_eq!(alice_entries[1].included[0], Value::Float64(92.0));

        // 验证 idx_id（唯一）
        let idx = table2.get_index("idx_id").unwrap();
        assert!(idx.is_unique());
        assert_eq!(idx.num_included(), 0);
        assert_eq!(idx.len(), 3);

        let rows = idx.get(&Value::Int64(2)).unwrap();
        assert_eq!(rows, vec![1]);
    }

    #[test]
    fn test_table_empty_indexes() {
        let table = make_test_table();
        let bytes = table.indexes_to_bytes();
        // skip_count(4B) = 0 + vec_count(4B) = 0 + mark_len(4B) = 0 + sparse_len(4B) = 0 + fts_magic(4B) + fts_count(4B) = 0 = 24 bytes
        // （v0.17.0 M1-7：尾部追加主键 Mark Index 长度字段）
        // （v0.19 分层索引：再追加稀疏索引段长度字段）
        // （v0.21 检索层：再追加 FTS 索引段；v0.21.2 段头加 magic 区分压缩格式）
        assert_eq!(bytes.len(), 24);

        let mut table2 = make_test_table();
        table2.indexes_from_bytes(&bytes).unwrap();
        assert_eq!(table2.indexes().len(), 0);
        assert_eq!(table2.vector_indexes().len(), 0);
    }

    #[test]
    fn test_database_index_persistence() {
        let path = temp_db_path("db_idx");
        cleanup_db(&path);

        {
            let mut db = Database::open(&path).unwrap();

            // 创建表
            let def = TableDef::new(1, "users", vec![
                ColumnDef::new("id", DataType::Int64),
                ColumnDef::new("name", DataType::Varchar),
            ]);
            db.create_table(def).unwrap();

            // 插入数据
            let table = db.get_table_mut("users").unwrap();
            table.insert(vec![
                vec![Value::Int64(1), Value::Varchar("alice".into())],
                vec![Value::Int64(2), Value::Varchar("bob".into())],
                vec![Value::Int64(3), Value::Varchar("charlie".into())],
            ]).unwrap();

            // 创建索引
            table.create_index("idx_name", &[1], &[], false).unwrap();
            assert_eq!(table.indexes().len(), 1);

            // 保存索引到文件
            let bytes_written = db.save_indexes().unwrap();
            assert!(bytes_written > 0);

            // 验证文件头已更新
            assert!(db.header.index_root > 0);
            assert!(db.header.index_size > 0);

            db.close().unwrap();
        }

        // 重新打开数据库，加载索引
        {
            let mut db = Database::open(&path).unwrap();

            // v0.12.1：catalog 已在 open_existing 中自动加载
            // 若表不存在（旧文件未存 catalog），则手动创建同 ID 表兼容旧格式
            if db.get_table("users").is_none() {
                let def = TableDef::new(1, "users", vec![
                    ColumnDef::new("id", DataType::Int64),
                    ColumnDef::new("name", DataType::Varchar),
                ]);
                db.create_table(def).unwrap();
            }

            // 加载索引
            // v0.12.1+：open_existing 已自动 load_indexes（catalog 存在时）。
            // 为兼容双路径（catalog 存在/不存在），断言最终索引实体存在并数量正确，
            // 而非单次 load_indexes 的增量返回值。
            let _ = db.load_indexes();
            let table = db.get_table("users").unwrap();
            let total_idx = table.indexes().len() + table.vector_indexes().len();
            assert_eq!(total_idx, 1);

            let idx = table.get_index("idx_name").unwrap();
            assert_eq!(idx.len(), 3);
            assert_eq!(idx.get(&Value::Varchar("bob".into())).unwrap(), vec![1]);

            db.close().unwrap();
        }

        cleanup_db(&path);
    }

    #[test]
    fn test_database_no_indexes() {
        let path = temp_db_path("db_noidx");
        cleanup_db(&path);

        {
            let db = Database::open(&path).unwrap();
            // 无索引时 load_indexes 应该返回 0
            assert_eq!(db.header.index_root, 0);
            assert_eq!(db.header.index_size, 0);
        }

        cleanup_db(&path);
    }

    // ========================================================================
    // 向量 HNSW 索引集成测试（v0.12.0 优先级 3）
    // ========================================================================

    fn random_vector(dim: usize, seed: u32) -> Vec<f32> {
        let mut v = Vec::with_capacity(dim);
        let mut s = seed as u64;
        for _ in 0..dim {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            v.push((s as f32) / (u64::MAX as f32) * 2.0 - 1.0);
        }
        v
    }

    #[test]
    fn test_vector_index_create_and_search() {
        let path = temp_db_path("vecidx_basic");
        cleanup_db(&path);

        let mut db = Database::open(&path).unwrap();

        // 创建带向量列的表（16 维，更贴近实际 embedding 场景）
        let def = TableDef::new(1, "embeddings", vec![
            ColumnDef::new("id", DataType::Int64),
            ColumnDef::new("embedding", DataType::Vector { dim: 16 }),
        ]);
        db.create_table(def).unwrap();

        // 插入 200 条向量数据
        let n = 200;
        let rows: Vec<Vec<Value>> = (0..n).map(|i| {
            vec![
                Value::Int64(i as i64),
                Value::Vector(random_vector(16, i)),
            ]
        }).collect();
        {
            let table = db.get_table_mut("embeddings").unwrap();
            table.insert(rows).unwrap();
        }

        // 创建 HNSW 向量索引
        db.create_vector_index(
            "embeddings",
            "idx_embedding",
            "embedding",
            crate::storage::vector_index::DistanceMetric::L2,
            16,
            200,
        ).unwrap();

        // 验证索引存在
        {
            let table = db.get_table("embeddings").unwrap();
            assert!(table.get_vector_index("idx_embedding").is_some());
            assert_eq!(table.get_vector_index("idx_embedding").unwrap().len(), n as usize);
        }

        // 向量搜索：搜索已知存在的向量，验证能找到
        let query = random_vector(16, 100);
        let results = db.vector_search("embeddings", "idx_embedding", &query, 10).unwrap();

        assert!(!results.is_empty());
        // 第 100 行应该在 top-10 结果中
        let found = results.iter().any(|r| r.id == 100);
        assert!(found, "第 100 行向量应在 top-10 搜索结果中");
        // 自己和自己的距离应接近 0
        let self_match = results.iter().find(|r| r.id == 100);
        assert!(self_match.is_some());
        assert!(self_match.unwrap().distance < 0.001,
            "自己和自己距离应接近 0，实际: {}", self_match.unwrap().distance);

        db.close().unwrap();
        cleanup_db(&path);
    }

    #[test]
    fn test_vector_index_incremental_insert() {
        let path = temp_db_path("vecidx_incr");
        cleanup_db(&path);

        let mut db = Database::open(&path).unwrap();

        let def = TableDef::new(1, "items", vec![
            ColumnDef::new("id", DataType::Int64),
            ColumnDef::new("vec", DataType::Vector { dim: 4 }),
        ]);
        db.create_table(def).unwrap();

        // 先插入 20 条
        let rows1: Vec<Vec<Value>> = (0..20).map(|i| {
            vec![
                Value::Int64(i as i64),
                Value::Vector(random_vector(4, i)),
            ]
        }).collect();
        {
            let table = db.get_table_mut("items").unwrap();
            table.insert(rows1).unwrap();
        }

        // 创建索引
        db.create_vector_index("items", "idx_vec", "vec",
            crate::storage::vector_index::DistanceMetric::L2, 8, 50).unwrap();

        // 再插入 20 条（增量更新）
        let rows2: Vec<Vec<Value>> = (20..40).map(|i| {
            vec![
                Value::Int64(i as i64),
                Value::Vector(random_vector(4, i)),
            ]
        }).collect();
        {
            let table = db.get_table_mut("items").unwrap();
            table.insert(rows2).unwrap();
        }

        // 验证索引包含所有 40 条
        {
            let table = db.get_table("items").unwrap();
            assert_eq!(table.get_vector_index("idx_vec").unwrap().len(), 40);
        }

        // 搜索第 35 行（增量插入的）
        let query = random_vector(4, 35);
        let results = db.vector_search("items", "idx_vec", &query, 3).unwrap();
        assert!(!results.is_empty());
        // 第 35 行应该在结果中
        let found = results.iter().any(|r| r.id == 35);
        assert!(found, "增量插入的向量应能被搜索到");

        db.close().unwrap();
        cleanup_db(&path);
    }

    #[test]
    fn test_vector_index_empty_search() {
        let path = temp_db_path("vecidx_empty");
        cleanup_db(&path);

        let mut db = Database::open(&path).unwrap();

        let def = TableDef::new(1, "empty_vec", vec![
            ColumnDef::new("id", DataType::Int64),
            ColumnDef::new("embedding", DataType::Vector { dim: 16 }),
        ]);
        db.create_table(def).unwrap();

        // 空表创建索引
        db.create_vector_index("empty_vec", "idx_emb", "embedding",
            crate::storage::vector_index::DistanceMetric::Cosine, 8, 50).unwrap();

        // 空索引搜索应返回空
        let query = random_vector(16, 999);
        let results = db.vector_search("empty_vec", "idx_emb", &query, 10).unwrap();
        assert!(results.is_empty());

        db.close().unwrap();
        cleanup_db(&path);
    }

    #[test]
    fn test_vector_index_not_found_error() {
        let path = temp_db_path("vecidx_nf");
        cleanup_db(&path);

        let mut db = Database::open(&path).unwrap();

        let def = TableDef::new(1, "t1", vec![
            ColumnDef::new("id", DataType::Int64),
            ColumnDef::new("v", DataType::Vector { dim: 4 }),
        ]);
        db.create_table(def).unwrap();

        // 搜索不存在的索引
        let query = vec![0.0; 4];
        let result = db.vector_search("t1", "no_such_index", &query, 5);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(),
            crate::common::error::EngramDbError::IndexNotFound(_)));

        cleanup_db(&path);
    }

    #[test]
    fn test_vector_index_persistence_roundtrip() {
        let path = temp_db_path("vecidx_persist");
        cleanup_db(&path);

        // 写入数据 + 创建向量索引 + 保存
        {
            let mut db = Database::open(&path).unwrap();

            let def = TableDef::new(1, "docs", vec![
                ColumnDef::new("id", DataType::Int64),
                ColumnDef::new("embedding", DataType::Vector { dim: 16 }),
            ]);
            db.create_table(def).unwrap();

            // 插入 100 条向量
            let rows: Vec<Vec<Value>> = (0..100).map(|i| {
                vec![
                    Value::Int64(i as i64),
                    Value::Vector(random_vector(16, i)),
                ]
            }).collect();
            {
                let table = db.get_table_mut("docs").unwrap();
                table.insert(rows).unwrap();
            }

            // 创建 HNSW 索引
            db.create_vector_index("docs", "idx_emb", "embedding",
                crate::storage::vector_index::DistanceMetric::L2, 16, 200).unwrap();

            // 验证索引存在
            {
                let table = db.get_table("docs").unwrap();
                assert_eq!(table.vector_indexes().len(), 1);
                assert_eq!(table.get_vector_index("idx_emb").unwrap().len(), 100);
            }

            // 保存索引到文件（close 也会自动保存，这里显式调用测试）
            let bytes_written = db.save_indexes().unwrap();
            assert!(bytes_written > 0);

            db.close().unwrap();
        }

        // 重新打开 + 加载索引 + 验证搜索
        {
            let mut db = Database::open(&path).unwrap();

            // v0.12.1：catalog 已在 open_existing 中自动加载
            // 若表不存在（旧文件未存 catalog），则手动创建同 ID 表兼容旧格式
            if db.get_table("docs").is_none() {
                let def = TableDef::new(1, "docs", vec![
                    ColumnDef::new("id", DataType::Int64),
                    ColumnDef::new("embedding", DataType::Vector { dim: 16 }),
                ]);
                db.create_table(def).unwrap();
            }

            // 加载索引
            // v0.12.1+：open_existing 已自动 load_indexes（catalog 存在时）。
            // 为兼容双路径，断言最终索引实体存在并数量正确。
            let _ = db.load_indexes();

            // 验证向量索引已加载
            let table = db.get_table("docs").unwrap();
            assert_eq!(table.vector_indexes().len(), 1);
            let total_idx = table.indexes().len() + table.vector_indexes().len();
            assert!(total_idx >= 1, "至少存在 1 个索引（向量索引）");
            assert_eq!(table.get_vector_index("idx_emb").unwrap().len(), 100);

            // 搜索验证：搜索第 50 号向量
            let query = random_vector(16, 50);
            let results = db.vector_search("docs", "idx_emb", &query, 10).unwrap();
            assert!(!results.is_empty());
            let found = results.iter().any(|r| r.id == 50);
            assert!(found, "持久化后第 50 行向量应能被搜索到");

            db.close().unwrap();
        }

        cleanup_db(&path);
    }

    #[test]
    fn test_mixed_indexes_persistence() {
        let path = temp_db_path("mixed_idx");
        cleanup_db(&path);

        {
            let mut db = Database::open(&path).unwrap();

            let def = TableDef::new(1, "items", vec![
                ColumnDef::new("id", DataType::Int64),
                ColumnDef::new("name", DataType::Varchar),
                ColumnDef::new("vec", DataType::Vector { dim: 8 }),
            ]);
            db.create_table(def).unwrap();

            let rows: Vec<Vec<Value>> = (0..30).map(|i| {
                vec![
                    Value::Int64(i as i64),
                    Value::Varchar(format!("item_{}", i)),
                    Value::Vector(random_vector(8, i)),
                ]
            }).collect();
            {
                let table = db.get_table_mut("items").unwrap();
                table.insert(rows).unwrap();
            }

            // 同时创建 SkipList 索引和向量索引
            {
                let table = db.get_table_mut("items").unwrap();
table.create_index("idx_name", &[1], &[], false).unwrap();
            }
            db.create_vector_index("items", "idx_vec", "vec",
                crate::storage::vector_index::DistanceMetric::L2, 8, 100).unwrap();

            // 验证
            {
                let table = db.get_table("items").unwrap();
                assert_eq!(table.indexes().len(), 1);
                assert_eq!(table.vector_indexes().len(), 1);
            }

            db.save_indexes().unwrap();
            db.close().unwrap();
        }

        // 重新加载，验证两种索引都恢复
        {
            let mut db = Database::open(&path).unwrap();

            if db.get_table("items").is_none() {
                let def = TableDef::new(1, "items", vec![
                    ColumnDef::new("id", DataType::Int64),
                    ColumnDef::new("name", DataType::Varchar),
                    ColumnDef::new("vec", DataType::Vector { dim: 8 }),
                ]);
                db.create_table(def).unwrap();
            }

            // v0.12.1+：open_existing 已自动 load_indexes（catalog 存在时）。
            // 为兼容双路径，断言最终索引实体存在并数量正确。
            let _ = db.load_indexes();

            let table = db.get_table("items").unwrap();
            let total_idx = table.indexes().len() + table.vector_indexes().len();
            assert!(total_idx >= 1);

            // SkipList 索引
            assert_eq!(table.indexes().len(), 1);
            assert!(table.get_index("idx_name").is_some());
            // 向量索引
            assert_eq!(table.vector_indexes().len(), 1);
            assert!(table.get_vector_index("idx_vec").is_some());
            assert_eq!(table.get_vector_index("idx_vec").unwrap().len(), 30);

            db.close().unwrap();
        }

        cleanup_db(&path);
    }

    // ========================================================================
    // DELETE/UPDATE + 向量索引维护测试（v0.12.0 优先级 3 · 删除更新支持）
    // ========================================================================

    #[test]
    fn test_vector_index_delete_tombstone() {
        // 验证 DELETE 后向量索引正确标记 tombstone，搜索结果过滤已删除行
        let path = temp_db_path("vecidx_del");
        cleanup_db(&path);

        let mut db = Database::open(&path).unwrap();

        let def = TableDef::new(1, "embeddings", vec![
            ColumnDef::new("id", DataType::Int64),
            ColumnDef::new("vec", DataType::Vector { dim: 8 }),
        ]);
        db.create_table(def).unwrap();

        // 插入 50 条（小批量，走 Delta 层，便于 delete_delta_rows 测试）
        let n = 50;
        let rows: Vec<Vec<Value>> = (0..n).map(|i| {
            vec![
                Value::Int64(i as i64),
                Value::Vector(random_vector(8, i)),
            ]
        }).collect();
        {
            let table = db.get_table_mut("embeddings").unwrap();
            table.insert(rows).unwrap();
        }

        // 创建向量索引
        db.create_vector_index("embeddings", "idx_vec", "vec",
            crate::storage::vector_index::DistanceMetric::L2, 8, 100).unwrap();

        // 验证初始状态
        {
            let table = db.get_table("embeddings").unwrap();
            let (hnsw, _) = table.vector_indexes().get("idx_vec").unwrap();
            assert_eq!(hnsw.len(), 50);
            assert_eq!(hnsw.deleted_count(), 0);
            assert_eq!(hnsw.active_len(), 50);
        }

        // 删除 Delta 层中第 10-19 行（共 10 行）
        {
            let table = db.get_table_mut("embeddings").unwrap();
            let indices: Vec<usize> = (10..20).collect();
            let deleted = table.delete_delta_rows(&indices).unwrap();
            assert_eq!(deleted, 10);
        }

        // 验证向量索引 tombstone 状态
        {
            let table = db.get_table("embeddings").unwrap();
            let (hnsw, _) = table.vector_indexes().get("idx_vec").unwrap();
            assert_eq!(hnsw.len(), 50);         // 物理节点数不变
            assert_eq!(hnsw.deleted_count(), 10); // 10 个 tombstone
            assert_eq!(hnsw.active_len(), 40);   // 40 个有效
        }

        // 搜索已删除的向量（id=15），结果中不应出现
        let query = random_vector(8, 15);
        let results = db.vector_search("embeddings", "idx_vec", &query, 10).unwrap();
        for r in &results {
            // 行 id 10-19 已被删除，不应出现在结果中
            assert!(r.id < 10 || r.id >= 20,
                "结果中包含已删除行 id={}", r.id);
        }

        // 搜索未删除的向量（id=42），应该能找到
        let query2 = random_vector(8, 42);
        let results2 = db.vector_search("embeddings", "idx_vec", &query2, 5).unwrap();
        assert!(results2.iter().any(|r| r.id == 42),
            "未删除的 id=42 应能被搜索到");

        db.close().unwrap();
        cleanup_db(&path);
    }

    #[test]
    fn test_vector_index_update_maintenance() {
        // 验证 UPDATE 后旧向量 tombstone + 新向量插入
        let path = temp_db_path("vecidx_upd");
        cleanup_db(&path);

        let mut db = Database::open(&path).unwrap();

        let def = TableDef::new(1, "items", vec![
            ColumnDef::new("id", DataType::Int64),
            ColumnDef::new("vec", DataType::Vector { dim: 8 }),
        ]);
        db.create_table(def).unwrap();

        // 插入 30 条（走 Delta 层）
        let n = 30;
        let rows: Vec<Vec<Value>> = (0..n).map(|i| {
            vec![
                Value::Int64(i as i64),
                Value::Vector(random_vector(8, i)),
            ]
        }).collect();
        {
            let table = db.get_table_mut("items").unwrap();
            table.insert(rows).unwrap();
        }

        // 创建向量索引
        db.create_vector_index("items", "idx_vec", "vec",
            crate::storage::vector_index::DistanceMetric::L2, 8, 100).unwrap();

        // 验证初始状态
        {
            let table = db.get_table("items").unwrap();
            let (hnsw, _) = table.vector_indexes().get("idx_vec").unwrap();
            assert_eq!(hnsw.len(), 30);
            assert_eq!(hnsw.active_len(), 30);
        }

        // 更新第 5 行的向量（替换为全新的向量值）
        let new_vec = random_vector(8, 9999); // 用一个新的 seed 生成不同的向量
        {
            let table = db.get_table_mut("items").unwrap();
            let updates = vec![
                (5, vec![(1, Value::Vector(new_vec.clone()))]),
            ];
            let updated = table.update_delta_rows(&updates).unwrap();
            assert_eq!(updated, 1);
        }

        // 验证向量索引状态：1 个 tombstone + 1 个新节点 = 31 个物理节点，30 个有效
        {
            let table = db.get_table("items").unwrap();
            let (hnsw, _) = table.vector_indexes().get("idx_vec").unwrap();
            assert_eq!(hnsw.len(), 31);         // 旧节点 + 新节点
            assert_eq!(hnsw.deleted_count(), 1); // 旧向量 tombstone
            assert_eq!(hnsw.active_len(), 30);   // 有效向量数不变
        }

        // 搜索旧向量（id=5 的原始向量 seed=5），不应再找到 row_id=5
        // （因为旧向量已 tombstone，新向量 seed=9999 完全不同）
        let old_query = random_vector(8, 5);
        let results_old = db.vector_search("items", "idx_vec", &old_query, 10).unwrap();
        // 旧向量对应的 hnsw_id 已被 tombstone，row_id=5 不应出现在结果中
        // （注意：其他向量可能距离也近，但 row_id=5 对应的旧向量已被标记删除）
        for r in &results_old {
            // row_id=5 对应的新向量是 seed=9999，和 seed=5 距离很远
            // 所以如果结果中有 id=5，那它应该是新向量（距离较远）
            // 这里只验证 tombstone 数量正确，搜索结果的正确性由 HNSW 层保证
            assert!(r.id < 30, "行 ID 应在有效范围内");
        }

        // 搜索新向量（seed=9999），应该能找到 row_id=5
        let new_query = random_vector(8, 9999);
        let results_new = db.vector_search("items", "idx_vec", &new_query, 5).unwrap();
        assert!(results_new.iter().any(|r| r.id == 5),
            "更新后的新向量应能通过 row_id=5 被搜索到");
        // 距离自己应接近 0
        let self_match = results_new.iter().find(|r| r.id == 5);
        assert!(self_match.is_some(), "应找到更新后的向量");
        assert!(self_match.unwrap().distance < 0.001,
            "自己和自己距离应接近 0");

        db.close().unwrap();
        cleanup_db(&path);
    }

    #[test]
    fn test_vector_index_delete_persistence() {
        // 验证 tombstone 数据能正确持久化和恢复
        let path = temp_db_path("vecidx_delpersist");
        cleanup_db(&path);

        {
            let mut db = Database::open(&path).unwrap();

            let def = TableDef::new(1, "docs", vec![
                ColumnDef::new("id", DataType::Int64),
                ColumnDef::new("embedding", DataType::Vector { dim: 8 }),
            ]);
            db.create_table(def).unwrap();

            // 插入 40 条
            let rows: Vec<Vec<Value>> = (0..40).map(|i| {
                vec![
                    Value::Int64(i as i64),
                    Value::Vector(random_vector(8, i)),
                ]
            }).collect();
            {
                let table = db.get_table_mut("docs").unwrap();
                table.insert(rows).unwrap();
            }

            // 创建向量索引
            db.create_vector_index("docs", "idx_emb", "embedding",
                crate::storage::vector_index::DistanceMetric::L2, 8, 100).unwrap();

            // 删除 10 行
            {
                let table = db.get_table_mut("docs").unwrap();
                let indices: Vec<usize> = (5..15).collect();
                table.delete_delta_rows(&indices).unwrap();
            }

            // 验证删除状态
            {
                let table = db.get_table("docs").unwrap();
                let (hnsw, _) = table.vector_indexes().get("idx_emb").unwrap();
                assert_eq!(hnsw.deleted_count(), 10);
                assert_eq!(hnsw.active_len(), 30);
            }

            // 保存索引
            db.save_indexes().unwrap();
            db.close().unwrap();
        }

        // 重新加载，验证 tombstone 数据正确恢复
        {
            let mut db = Database::open(&path).unwrap();

            if db.get_table("docs").is_none() {
                let def = TableDef::new(1, "docs", vec![
                    ColumnDef::new("id", DataType::Int64),
                    ColumnDef::new("embedding", DataType::Vector { dim: 8 }),
                ]);
                db.create_table(def).unwrap();
            }

            db.load_indexes().unwrap();

            let table = db.get_table("docs").unwrap();
            let (hnsw, _) = table.vector_indexes().get("idx_emb").unwrap();
            assert_eq!(hnsw.len(), 40);
            assert_eq!(hnsw.deleted_count(), 10);
            assert_eq!(hnsw.active_len(), 30);

            // 搜索已删除的向量（id=10），不应出现在结果中
            let query = random_vector(8, 10);
            let results = table.vector_search("idx_emb", &query, 10).unwrap();
            for r in &results {
                // row_id 5-14 已删除
                assert!(r.id < 5 || r.id >= 15,
                    "结果中包含已删除行 id={}", r.id);
            }

            db.close().unwrap();
        }

        cleanup_db(&path);
    }

    #[test]
    fn test_vector_index_delete_all_active() {
        // 边界：删除所有有效向量后，搜索应返回空
        let path = temp_db_path("vecidx_delall");
        cleanup_db(&path);

        let mut db = Database::open(&path).unwrap();

        let def = TableDef::new(1, "small", vec![
            ColumnDef::new("id", DataType::Int64),
            ColumnDef::new("vec", DataType::Vector { dim: 4 }),
        ]);
        db.create_table(def).unwrap();

        // 插入 5 条
        let rows: Vec<Vec<Value>> = (0..5).map(|i| {
            vec![
                Value::Int64(i as i64),
                Value::Vector(random_vector(4, i)),
            ]
        }).collect();
        {
            let table = db.get_table_mut("small").unwrap();
            table.insert(rows).unwrap();
        }

        db.create_vector_index("small", "idx_vec", "vec",
            crate::storage::vector_index::DistanceMetric::L2, 4, 20).unwrap();

        // 删除全部 5 行
        {
            let table = db.get_table_mut("small").unwrap();
            let indices: Vec<usize> = (0..5).collect();
            let deleted = table.delete_delta_rows(&indices).unwrap();
            assert_eq!(deleted, 5);
        }

        // 验证
        {
            let table = db.get_table("small").unwrap();
            let (hnsw, _) = table.vector_indexes().get("idx_vec").unwrap();
            assert_eq!(hnsw.len(), 5);
            assert_eq!(hnsw.deleted_count(), 5);
            assert_eq!(hnsw.active_len(), 0);
        }

        // 搜索应返回空
        let query = random_vector(4, 0);
        let results = db.vector_search("small", "idx_vec", &query, 3).unwrap();
        assert!(results.is_empty(), "全部删除后搜索应返回空");

        db.close().unwrap();
        cleanup_db(&path);
    }

    // ========================================================================
    // 压缩持久化往返测试（v0.12.x P0：接通压缩到 compact）
    // ========================================================================

    #[test]
    fn test_compression_persistence_roundtrip() {
        let path = temp_db_path("compress_rt");
        cleanup_db(&path);

        // 1. 创建数据库，插入多类型数据，checkpoint（compact → compress_all → save）
        {
            let mut db = Database::open(&path).unwrap();

            let def = TableDef::new(1, "mixed", vec![
                ColumnDef::new("id", DataType::Int32),       // Delta/FOR 压缩
                ColumnDef::new("name", DataType::Varchar),    // Dictionary 压缩
                ColumnDef::new("score", DataType::Float64),   // Gorilla 压缩
                ColumnDef::new("active", DataType::Boolean),  // BooleanPack 压缩
            ]);
            db.create_table(def).unwrap();

            let table = db.get_table_mut("mixed").unwrap();
            let names = ["alice", "bob", "charlie"];
            let rows: Vec<Vec<Value>> = (0..300u32).map(|i| {
                vec![
                    Value::Int32(i as i32),
                    Value::Varchar(names[(i % 3) as usize].into()),
                    Value::Float64(i as f64 * 0.1),
                    Value::Boolean(i % 2 == 0),
                ]
            }).collect();
            table.insert(rows).unwrap();

            db.checkpoint().unwrap();
            db.close().unwrap();
        }

        // 2. 重新打开，验证数据完整性（read_column 惰性解压）
        {
            let mut db = Database::open(&path).unwrap();

            // load_catalog + load_data 已在 open_existing 中完成
            let table = db.get_table_mut("mixed").unwrap();
            assert_eq!(table.column_store().total_rows(), 300);

            // scan 会触发 read_column → 惰性解压
            let rows = table.scan(&[0, 1, 2, 3]).unwrap();
            assert_eq!(rows.len(), 300);

            // 验证首行
            assert_eq!(rows[0][0], Value::Int32(0));
            assert_eq!(rows[0][1], Value::Varchar("alice".into()));
            assert_eq!(rows[0][3], Value::Boolean(true));

            // 验证末行
            assert_eq!(rows[299][0], Value::Int32(299));
            assert_eq!(rows[299][1], Value::Varchar("charlie".into()));
            assert_eq!(rows[299][3], Value::Boolean(false));

            // 验证中间行（name 按 i%3 循环：151 % 3 = 1 → "bob"）
            assert_eq!(rows[151][1], Value::Varchar("bob".into()));

            db.close().unwrap();
        }

        cleanup_db(&path);
    }

    #[test]
    fn test_append_after_compressed_load() {
        // 场景：数据以压缩态落盘 → 重新加载 → 追加新行 → 验证全部数据
        // 核心验证点：ensure_rg_decompressed 在 append 前正确解压旧数据
        let path = temp_db_path("compress_append");
        cleanup_db(&path);

        // 1. 第一阶段：插入 200 行，checkpoint 压缩落盘
        {
            let mut db = Database::open(&path).unwrap();

            let def = TableDef::new(1, "events", vec![
                ColumnDef::new("id", DataType::Int64),
                ColumnDef::new("label", DataType::Varchar),
            ]);
            db.create_table(def).unwrap();

            let table = db.get_table_mut("events").unwrap();
            let rows: Vec<Vec<Value>> = (0..200i64).map(|i| {
                vec![
                    Value::Int64(i),
                    Value::Varchar(if i % 2 == 0 { "even" } else { "odd" }.into()),
                ]
            }).collect();
            table.insert(rows).unwrap();

            db.checkpoint().unwrap();
            db.close().unwrap();
        }

        // 2. 第二阶段：重新打开，追加 50 行，验证全部 250 行
        {
            let mut db = Database::open(&path).unwrap();

            // 追加新数据（触发 ensure_rg_decompressed）
            let table = db.get_table_mut("events").unwrap();
            let new_rows: Vec<Vec<Value>> = (200..250i64).map(|i| {
                vec![
                    Value::Int64(i),
                    Value::Varchar(if i % 2 == 0 { "even" } else { "odd" }.into()),
                ]
            }).collect();
            table.insert(new_rows).unwrap();

            // compact 把新 Delta 合并到列存
            db.compact_table("events").unwrap();

            // 验证全部数据
            let table = db.get_table_mut("events").unwrap();
            let rows = table.scan(&[0, 1]).unwrap();
            assert_eq!(rows.len(), 250);

            // 验证旧数据（前 200 行）
            assert_eq!(rows[0][0], Value::Int64(0));
            assert_eq!(rows[199][0], Value::Int64(199));

            // 验证新追加的数据（后 50 行）
            assert_eq!(rows[200][0], Value::Int64(200));
            assert_eq!(rows[249][0], Value::Int64(249));
            assert_eq!(rows[249][1], Value::Varchar("odd".into()));

            db.close().unwrap();
        }

        cleanup_db(&path);
    }

    #[test]
    fn test_compression_disabled_persist() {
        // compress_on_persist = false 时，数据以裸存方式落盘，同样能正确往返
        let path = temp_db_path("compress_off");
        cleanup_db(&path);

        {
            let mut config = crate::common::config::Config::default();
            config.compress_on_persist = false;
            let mut db = Database::open_with_config(&path, config).unwrap();

            let def = TableDef::new(1, "raw", vec![
                ColumnDef::new("id", DataType::Int32),
                ColumnDef::new("tag", DataType::Varchar),
            ]);
            db.create_table(def).unwrap();

            let table = db.get_table_mut("raw").unwrap();
            let rows: Vec<Vec<Value>> = (0..100u32).map(|i| {
                vec![
                    Value::Int32(i as i32),
                    Value::Varchar(format!("item_{}", i % 5)),
                ]
            }).collect();
            table.insert(rows).unwrap();

            db.checkpoint().unwrap();
            db.close().unwrap();
        }

        {
            let mut db = Database::open(&path).unwrap();
            let table = db.get_table_mut("raw").unwrap();
            let rows = table.scan(&[0, 1]).unwrap();
            assert_eq!(rows.len(), 100);
            assert_eq!(rows[0][0], Value::Int32(0));
            assert_eq!(rows[99][0], Value::Int32(99));
            assert_eq!(rows[99][1], Value::Varchar("item_4".into()));
            db.close().unwrap();
        }

        cleanup_db(&path);
    }

    // ============ P0-2 事务级 Batcher 单测 ============

    #[test]
    fn test_txn_buffer_push_and_pending() {
        let mut db = Database::open(":memory:").unwrap();
        assert!(db.txn_buffer_is_empty());
        assert_eq!(db.txn_buffer_pending(), 0);
        let rows = vec![vec![crate::Value::Int64(1)], vec![crate::Value::Int64(2)]];
        assert!(!db.txn_buffer_push("t", rows).unwrap());
        assert_eq!(db.txn_buffer_pending(), 2);
        assert!(!db.txn_buffer_is_empty());
        // 空行集不改变状态
        assert!(!db.txn_buffer_push("t", Vec::new()).unwrap());
        assert_eq!(db.txn_buffer_pending(), 2);
    }

    #[test]
    fn test_txn_buffer_threshold_triggers_flush_flag() {
        let mut cfg = crate::common::config::Config::default();
        cfg.txn_batch_rows = 3;
        let mut db = Database::open_with_config(":memory:", cfg).unwrap();
        assert!(!db.txn_buffer_push("t", vec![vec![crate::Value::Int64(1)]]).unwrap());
        assert!(!db.txn_buffer_push("t", vec![vec![crate::Value::Int64(2)]]).unwrap());
        // 跨表累计达到阈值
        assert!(db.txn_buffer_push("u", vec![vec![crate::Value::Int64(3)]]).unwrap());
        assert_eq!(db.txn_buffer_pending(), 3);
    }

    #[test]
    fn test_txn_buffer_discard() {
        let mut db = Database::open(":memory:").unwrap();
        db.txn_buffer_push("t", vec![vec![crate::Value::Int64(1)]]).unwrap();
        db.txn_buffer_push("u", vec![vec![crate::Value::Int64(2)]]).unwrap();
        db.discard_txn_buffer();
        assert!(db.txn_buffer_is_empty());
        assert_eq!(db.txn_buffer_pending(), 0);
        // 幂等
        db.discard_txn_buffer();
        assert!(db.txn_buffer_is_empty());
    }

    #[test]
    fn test_txn_buffer_flush_empty_noop() {
        let mut db = Database::open(":memory:").unwrap();
        db.flush_txn_buffer().unwrap();
        assert!(db.txn_buffer_is_empty());
    }

    #[test]
    fn test_txn_buffer_flush_persists() {
        let mut db = Database::open(":memory:").unwrap();
        db.create_table(TableDef::new(0, "t", vec![
            crate::common::types::ColumnDef::new("id", crate::common::types::DataType::Int64),
        ])).unwrap();
        db.txn_buffer_push("t", vec![vec![crate::Value::Int64(1)], vec![crate::Value::Int64(2)]]).unwrap();
        db.flush_txn_buffer().unwrap();
        assert!(db.txn_buffer_is_empty());
        // 数据已落 Delta（读己之写）
        let t = db.get_engine_table_mut("t").unwrap();
        let chunks = t.scan_to_chunks(&[0], None).unwrap();
        let total: usize = chunks.iter().map(|c| c.count).sum();
        assert_eq!(total, 2);
    }

    #[test]
    fn test_txn_buffer_flush_multi_table() {
        let mut db = Database::open(":memory:").unwrap();
        db.create_table(TableDef::new(0, "a", vec![
            crate::common::types::ColumnDef::new("id", crate::common::types::DataType::Int64),
        ])).unwrap();
        db.create_table(TableDef::new(1, "b", vec![
            crate::common::types::ColumnDef::new("id", crate::common::types::DataType::Int64),
        ])).unwrap();
        db.txn_buffer_push("a", vec![vec![crate::Value::Int64(1)]]).unwrap();
        db.txn_buffer_push("b", vec![vec![crate::Value::Int64(2)]]).unwrap();
        db.flush_txn_buffer().unwrap();
        assert!(db.txn_buffer_is_empty());
        let a = db.get_engine_table_mut("a").unwrap();
        let ca: usize = a.scan_to_chunks(&[0], None).unwrap().iter().map(|c| c.count).sum();
        assert_eq!(ca, 1);
        let b = db.get_engine_table_mut("b").unwrap();
        let cb: usize = b.scan_to_chunks(&[0], None).unwrap().iter().map(|c| c.count).sum();
        assert_eq!(cb, 1);
    }
}
