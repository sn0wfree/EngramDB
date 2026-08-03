//! 存储引擎模块

pub mod file_format;
pub mod buffer_pool;
pub mod column_store;
pub mod delta_store;
pub mod compression;
pub mod table;
pub mod sparse_index;
pub mod vector_index;
pub mod index;
pub mod catalog;

use std::collections::HashMap;
use std::path::PathBuf;

use crate::common::error::{Result, HybridDbError};
use crate::common::types::TableDef;
use crate::common::config::Config;
use crate::txn::TransactionManager;
use file_format::FileHeader;
use table::Table;

/// 数据库实例
pub struct Database {
    path: PathBuf,
    config: Config,
    header: FileHeader,
    tables: HashMap<u32, Table>,
    table_names: HashMap<String, u32>,
    next_table_id: u32,
    file: std::fs::File,
    /// 事务管理器
    txn_manager: TransactionManager,
}

impl Database {
    /// 打开或创建数据库
    pub fn open(path: &str) -> Result<Self> {
        let path = if path == ":memory:" {
            let mut p = std::env::temp_dir();
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
            p.push(format!("hybriddb_mem_{}_{}.hdb", std::process::id(), nanos));
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
            p.push(format!("hybriddb_mem_{}_{}.hdb", std::process::id(), nanos));
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
        let mut file = std::fs::File::create(path)?;

        // 写入文件头
        let header = FileHeader::new(&config);
        let header_bytes = header.to_bytes()?;
        file.write_all(&header_bytes)?;
        file.sync_all()?;

        // 初始化事务管理器
        let path_str = path.to_string_lossy().to_string();
        let txn_manager = TransactionManager::new(&path_str, &config)?;

        Ok(Self {
            path: path.to_path_buf(),
            config,
            header,
            tables: HashMap::new(),
            table_names: HashMap::new(),
            next_table_id: 1,
            file,
            txn_manager,
        })
    }

    fn open_existing(path: &std::path::Path, config: Config) -> Result<Self> {
        use std::io::{Read, Seek};
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

        let mut db = Self {
            path: path.to_path_buf(),
            config,
            header,
            tables: HashMap::new(),
            table_names: HashMap::new(),
            next_table_id: 1,
            file,
            txn_manager,
        };

        // v0.12.1: 恢复 schema 与数据（顺序：catalog → data → indexes）
        // 索引依赖表结构，数据依赖表结构，故 catalog 必须最先加载
        db.load_catalog()?;
        db.load_data()?;
        // 索引在 schema 与数据均就绪后构建
        let _ = db.load_indexes();

        Ok(db)
    }

    /// 创建表
    pub fn create_table(&mut self, table_def: TableDef) -> Result<()> {
        if self.table_names.contains_key(&table_def.name) {
            return Err(crate::common::error::HybridDbError::ConstraintViolation(
                format!("Table '{}' already exists", table_def.name)
            ));
        }

        let table_id = self.next_table_id;
        self.next_table_id += 1;

        let table = Table::new(table_def.clone(), self.config.compact_strategy);
        self.tables.insert(table_id, table);
        self.table_names.insert(table_def.name.clone(), table_id);

        // v0.12.1: 持久化 catalog 到文件
        let _ = self.save_catalog();

        Ok(())
    }

    /// 获取表
    pub fn get_table(&self, name: &str) -> Option<&Table> {
        self.table_names.get(name).and_then(|id| self.tables.get(id))
    }

    /// 获取可变表
    pub fn get_table_mut(&mut self, name: &str) -> Option<&mut Table> {
        let id = *self.table_names.get(name)?;
        self.tables.get_mut(&id)
    }

    /// 创建覆盖索引（v0.12.0 新增）
    pub fn create_index(&mut self, table_name: &str, index_name: &str,
                        key_col_idx: usize, included_cols: &[usize], unique: bool) -> Result<()> {
        let table = self.get_table_mut(table_name)
            .ok_or_else(|| HybridDbError::TableNotFound(table_name.to_string()))?;
        table.create_index(index_name, key_col_idx, included_cols, unique)
    }

    /// 获取表名到 ID 的映射（只读）
    pub fn table_names(&self) -> &HashMap<String, u32> {
        &self.table_names
    }
    
    /// 获取所有表的可变引用（用于事务提交后应用）
    pub fn tables_mut(&mut self) -> &mut HashMap<u32, table::Table> {
        &mut self.tables
    }

    /// 获取配置
    pub fn config(&self) -> &Config {
        &self.config
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
                if let Some(table) = self.tables.get_mut(&self.table_names[name]) {
                    let _ = table.maybe_compact()?;
                }
            }
        }

        Ok(())
    }

    /// 设置 WAL 刷盘策略
    pub fn set_wal_flush_mode(&mut self, mode: crate::common::config::WalFlushMode) {
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

    /// 设置指定表的聚簇列（方案B：Delta 聚簇）
    ///
    /// 设置后，compact 时会按该列的值分组写入列存，
    /// 相同 key 的行物理上连续，可大幅提升按该列的范围查询性能。
    pub fn set_cluster_key(&mut self, table_name: &str, column_name: &str) -> Result<()> {
        let table = self.get_table_mut(table_name)
            .ok_or_else(|| crate::common::error::HybridDbError::TableNotFound(table_name.into()))?;
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
            .ok_or_else(|| crate::common::error::HybridDbError::TableNotFound(table_name.into()))?;

        let col_idx = table.def().columns.iter()
            .position(|c| c.name == column_name)
            .ok_or_else(|| crate::common::error::HybridDbError::ColumnNotFound(column_name.into()))?;

        table.create_vector_index(index_name, col_idx, metric, m, ef_construction)
    }

    /// 向量相似度搜索
    ///
    /// 返回 top-k 最近邻的行 ID 和距离。
    pub fn vector_search(&self, table_name: &str, index_name: &str, query: &[f32], k: usize) -> Result<Vec<crate::storage::vector_index::Neighbor>> {
        let table = self.get_table(table_name)
            .ok_or_else(|| crate::common::error::HybridDbError::TableNotFound(table_name.into()))?;
        table.vector_search(index_name, query, k)
    }

    /// 设置全局默认合并策略（新建表生效，已有表不受影响）
    pub fn set_default_compact_strategy(&mut self, strategy: crate::common::config::CompactStrategy) {
        self.config.compact_strategy = strategy;
    }

    /// 设置指定表的合并策略（运行时动态切换）
    pub fn set_table_compact_strategy(&mut self, table_name: &str, strategy: crate::common::config::CompactStrategy) -> Result<()> {
        let table = self.get_table_mut(table_name)
            .ok_or_else(|| crate::common::error::HybridDbError::TableNotFound(table_name.into()))?;
        table.set_compact_strategy(strategy);
        Ok(())
    }

    /// 合并指定表的 Delta 层到列存（全量合并）
    ///
    /// 返回合并的行数。
    pub fn compact_table(&mut self, table_name: &str) -> Result<u64> {
        let table = self.get_table_mut(table_name)
            .ok_or_else(|| crate::common::error::HybridDbError::TableNotFound(table_name.into()))?;
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
        Ok(())
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
            return Ok(0); // 无索引
        }

        // 读取索引段
        let mut data = vec![0u8; self.header.index_size as usize];
        self.file.seek(std::io::SeekFrom::Start(self.header.index_root as u64))?;
        self.file.read_exact(&mut data)?;

        if data.len() < 4 {
            return Err(HybridDbError::InvalidFormat("index section too short".into()));
        }

        let table_count = u32::from_le_bytes(data[..4].try_into().unwrap()) as usize;
        let mut offset = 4;
        let mut total_indexes = 0;

        for _ in 0..table_count {
            if offset + 4 > data.len() {
                return Err(HybridDbError::InvalidFormat("truncated table id".into()));
            }
            let table_id = u32::from_le_bytes(data[offset..offset+4].try_into().unwrap());
            offset += 4;

            if offset + 4 > data.len() {
                return Err(HybridDbError::InvalidFormat("truncated table index size".into()));
            }
            let index_data_len = u32::from_le_bytes(data[offset..offset+4].try_into().unwrap()) as usize;
            offset += 4;

            if offset + index_data_len > data.len() {
                return Err(HybridDbError::InvalidFormat("truncated table index data".into()));
            }

            // 加载到对应表
            if let Some(table) = self.tables.get_mut(&table_id) {
                let before_skip = table.indexes().len();
                let before_vec = table.vector_indexes().len();
                table.indexes_from_bytes(&data[offset..offset+index_data_len])?;
                total_indexes += table.indexes().len() - before_skip;
                total_indexes += table.vector_indexes().len() - before_vec;
            }
            // 如果表不存在，跳过该表的索引（表可能已被删除）

            offset += index_data_len;
        }

        Ok(total_indexes)
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
            let table = Table::new(table_def.clone(), self.config.compact_strategy);
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
        let mut section_buf = Vec::new();
        let table_count = self.tables.len() as u32;
        section_buf.extend_from_slice(&table_count.to_le_bytes());

        // 收集 table_id 列表（避免借用冲突）
        let table_ids: Vec<u32> = self.tables.keys().copied().collect();
        let compress = self.config.compress_on_persist;
        for table_id in table_ids {
            let table = self.tables.get_mut(&table_id).unwrap();
            let data_bytes = table.column_store_mut().data_to_bytes(compress)?;
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
                return Err(HybridDbError::InvalidFormat("truncated data table header".into()));
            }
            let table_id = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
            offset += 4;
            let data_len = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
            offset += 4;

            if offset + data_len > data.len() {
                return Err(HybridDbError::InvalidFormat("truncated data table body".into()));
            }

            if let Some(table) = self.tables.get_mut(&table_id) {
                table.column_store_mut().data_from_bytes(&data[offset..offset + data_len])?;
                // 同步列的 data_type（修正 Vector dim 等）
                table.sync_column_data_types();
                loaded += 1;
            }
            // 表不存在则跳过（schema 已删但数据未清理）
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
        // 1. 先把 Delta 合并到列存（确保数据完整）
        let _ = self.compact_all()?;

        // 1.5 压缩列存（v0.12.x 压缩接线）
        // compact 后对每张表调用 compress_all：数据以压缩态落盘 + 降低内存占用。
        // 后续 append 路径会通过 ensure_rg_decompressed 按需惰性解压。
        if self.config.compress_on_persist {
            for table in self.tables.values_mut() {
                let _ = table.column_store_mut().compress_all()?;
            }
        }

        // 2. 保存 catalog（schema）
        let _ = self.save_catalog()?;

        // 3. 保存 data（列存数据）
        let _ = self.save_data()?;

        // 4. 保存 indexes（二级索引）
        let _ = self.save_indexes()?;

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
        p.push(format!("hybriddb_{}_{}_{}.hdb", suffix, std::process::id(), tid));
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
        table.create_index("idx_name", 1, &[2], false).unwrap(); // name 键，覆盖 score
        table.create_index("idx_id", 0, &[], true).unwrap(); // id 唯一键

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
        // skip_count(4B) = 0 + vec_count(4B) = 0 = 8 bytes
        assert_eq!(bytes.len(), 8);

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
            table.create_index("idx_name", 1, &[], false).unwrap();
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
            crate::common::error::HybridDbError::IndexNotFound(_)));

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
                table.create_index("idx_name", 1, &[], false).unwrap();
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
}
