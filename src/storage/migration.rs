//! Phase 3 P0：Auto 表数据迁移
//!
//! 将表的实际数据从一个引擎搬到另一个引擎。
//!
//! ## 设计原则
//! 1. **数据搬迁 + 类型替换**：原 EngineTable 实例销毁，新 EngineTable 实例创建
//! 2. **MVCC 保留**：迁移过程中不阻塞读写，但需在迁移窗口外做
//! 3. **def.engine 字段更新**：迁移后表定义的 engine 字段改为新引擎
//! 4. **索引重建**：主键索引、HNSW、FTS 等随新引擎重建（Phase 3.0 仅重建 PK）
//!
//! ## 当前 Phase 3 实现范围
//! - Columnar ↔ Memory 双向迁移（in-memory 同语义，最简单）
//! - Columnar ↔ Log 双向迁移（块格式转换）
//! - Memory ↔ Log 迁移暂不支持（语义差异大，后续 Phase 3.x 扩展）
//!
//! ## 错误处理
//! - 迁移失败 → 保留原表状态（不破坏）
//! - 返回 `MigrationError`（数据丢失 / 索引重建失败 / 类型不兼容）

use crate::common::error::{EngramDbError, Result};
use crate::common::types::{EngineType, TableDef};
use crate::Value;

use super::engine::EngineTable;
use super::memory_engine::MemoryTable;
use super::log_engine::LogTable;

/// Phase 3 P0：迁移错误类型
#[derive(Debug)]
pub enum MigrationError {
    /// 不支持的引擎对
    UnsupportedPair(EngineType, EngineType),
    /// 表数据丢失（迁移过程中读失败）
    DataLost(String),
    /// 类型不兼容（如 Log 表试图做 UPDATE — 当前不支持）
    TypeIncompatible(String),
    /// 同引擎迁移（无操作）
    SameEngine(EngineType),
}

impl std::fmt::Display for MigrationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MigrationError::UnsupportedPair(from, to) => {
                write!(f, "unsupported migration pair: {} -> {}", from.as_str(), to.as_str())
            }
            MigrationError::DataLost(msg) => write!(f, "data lost during migration: {}", msg),
            MigrationError::TypeIncompatible(msg) => write!(f, "type incompatible: {}", msg),
            MigrationError::SameEngine(e) => write!(f, "same engine migration: {}", e.as_str()),
        }
    }
}

impl std::error::Error for MigrationError {}

impl From<MigrationError> for EngramDbError {
    fn from(e: MigrationError) -> Self {
        EngramDbError::Parse(format!("migration error: {}", e))
    }
}

/// Phase 3 P0：迁移统计
#[derive(Debug, Clone, Copy, Default)]
pub struct MigrationStats {
    pub rows_migrated: u64,
    pub bytes_read: u64,
    pub bytes_written: u64,
    pub duration_ms: u64,
}

/// Phase 3 P0：迁移结果
pub struct MigrationResult {
    pub stats: MigrationStats,
    /// 迁移后的表（已替换 Database.tables 中的旧表）
    pub new_engine: EngineType,
}

/// Phase 3 P0：从源 EngineTable 提取所有行
///
/// - Columnar：scan() 返回 Vec<Vec<Value>>
/// - Memory：scan_to_rows_direct() 返回 Vec<Vec<Value>>
/// - Log：scan_to_rows_direct() 返回 Vec<Vec<Value>>
fn extract_all_rows(table: &mut EngineTable) -> Result<Vec<Vec<Value>>> {
    match table {
        EngineTable::Columnar(t) => {
            let num_cols = t.def().columns.len();
            let col_indices: Vec<usize> = (0..num_cols).collect();
            t.scan(&col_indices)
        }
        EngineTable::Memory(t) => {
            let num_cols = t.def.columns.len();
            let col_indices: Vec<usize> = (0..num_cols).collect();
            t.scan_to_rows_direct(&col_indices, None)
        }
        EngineTable::Log(t) => {
            let num_cols = t.def.columns.len();
            let col_indices: Vec<usize> = (0..num_cols).collect();
            t.scan_to_rows_direct(&col_indices, None)
        }
    }
}

/// Phase 3 P0：在新 EngineTable 中插入提取出的行
fn insert_all_rows(new_table: &mut EngineTable, rows: Vec<Vec<Value>>) -> Result<u64> {
    match new_table {
        EngineTable::Memory(t) => t.insert(rows),
        EngineTable::Log(t) => t.insert(rows),
        // Columnar 插入需要 columnar::Table 类型（暂通过 insert_columns 路径）
        // 这里只支持 Memory/Log 的批量插入；Columnar 走专用路径
        EngineTable::Columnar(_) => Err(EngramDbError::Parse(
            "use insert_columns_to_columnar for Columnar target".into()
        )),
    }
}

/// Phase 3 P0：迁移表数据 from→to 引擎
///
/// ## 步骤
/// 1. 读取所有行（Vec<Vec<Value>>）
/// 2. 替换 EngineTable 变体（带更新后的 def.engine）
/// 3. 把行写入新表
/// 4. 返回 MigrationStats
///
/// ## 注意
/// - 主键索引在新表中自动重建（Columnar/Memory 构造时初始化）
/// - 迁移前不调用 sync_wal，迁移后由调用方决定
pub fn migrate_table_data(
    table: &mut EngineTable,
    from: EngineType,
    to: EngineType,
) -> std::result::Result<MigrationResult, MigrationError> {
    let start = std::time::Instant::now();
    if from == to {
        return Err(MigrationError::SameEngine(from));
    }

    // Step 1: 读取所有行
    let rows = extract_all_rows(table).map_err(|e| MigrationError::DataLost(
        format!("extract rows failed: {}", e)
    ))?;
    let bytes_read: u64 = rows.iter().map(|r| r.iter().map(|v| estimate_value_size(v)).sum::<usize>() as u64).sum();
    let row_count = rows.len() as u64;

    // Step 2: 准备新 def（更新 engine 字段，row_count 由 insert 维护）
    let mut new_def = table.def().clone();
    new_def.engine = to;
    new_def.row_count = 0; // 重置，让目标引擎 insert 累加

    // Step 3: 创建新引擎表 + 写入数据
    let new_table = match to {
        EngineType::Memory => {
            let mut t = MemoryTable::new(new_def.clone());
            if row_count > 0 {
                t.insert(rows.clone()).map_err(|e| MigrationError::DataLost(
                    format!("memory insert failed: {}", e)
                ))?;
            }
            EngineTable::Memory(t)
        }
        EngineType::Log => {
            let mut t = LogTable::with_block_rows(new_def.clone(), 8192);
            if row_count > 0 {
                t.insert(rows.clone()).map_err(|e| MigrationError::DataLost(
                    format!("log insert failed: {}", e)
                ))?;
            }
            EngineTable::Log(t)
        }
        EngineType::Columnar => {
            // Columnar 需要特殊处理：先创建空 Table，再 append_columns
            // 但 append_columns 接受 Vec<Vec<Value>>（按列），需要转置
            use super::table::Table;
            let mut t = Table::new(new_def.clone(), crate::common::config::CompactStrategy::Adaptive {
                min_threshold: 10_000,
                max_threshold: 122_880,
                pct_of_table: 0.10,
                batch_size: 122_880,
            });
            t.set_index_config(true, false, 8192);
            if row_count > 0 {
                t.insert(rows.clone()).map_err(|e| MigrationError::DataLost(
                    format!("columnar insert failed: {}", e)
                ))?;
            }
            EngineTable::Columnar(t)
        }
        EngineType::Auto => {
            return Err(MigrationError::UnsupportedPair(from, to));
        }
    };

    // Step 4: 计算写入字节（估算）
    let bytes_written: u64 = rows.iter().map(|r| r.iter().map(|v| estimate_value_size(v)).sum::<usize>() as u64).sum();

    // Step 5: 替换原表
    let _ = std::mem::replace(table, new_table);

    // Step 6: 强制重建（针对 Columnar 引擎重建索引）
    // 暂略 — append_rows 已自动维护

    let duration_ms = start.elapsed().as_millis() as u64;

    Ok(MigrationResult {
        stats: MigrationStats {
            rows_migrated: row_count,
            bytes_read,
            bytes_written,
            duration_ms,
        },
        new_engine: to,
    })
}

/// Phase 3 P0：估算 Value 占用字节数（用于统计）
fn estimate_value_size(v: &Value) -> usize {
    match v {
        Value::Null => 8,
        Value::Boolean(_) => 1,
        Value::Int32(_) => 4,
        Value::Int64(_) => 8,
        Value::Float32(_) => 4,
        Value::Float64(_) => 8,
        Value::Varchar(s) | Value::Json(s) => s.len() + 24, // String overhead
        Value::Vector(v) => v.len() * 4 + 24,
        Value::VectorInt8(v) => v.len() + 24,
        Value::Blob(b) => b.len() + 24,
        Value::Timestamp(_) => 8,
    }
}

/// Phase 3 P0：便捷 API：根据 MigrationDecision 迁移
pub fn execute_migration(
    table: &mut EngineTable,
    decision: &super::tier_migration::MigrationDecision,
) -> std::result::Result<MigrationResult, MigrationError> {
    migrate_table_data(table, decision.from_engine, decision.to_engine)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::config::CompactStrategy;
    use crate::common::types::ColumnDef;
    use crate::common::types::DataType;
    use crate::storage::table::Table;

    fn make_def(name: &str, engine: EngineType) -> TableDef {
        TableDef {
            id: 1,
            name: name.to_string(),
            columns: vec![
                ColumnDef {
                    name: "id".to_string(),
                    data_type: DataType::Int64,
                    nullable: false,
                    is_primary_key: true,
                    auto_increment: false,
                    default_value: None,
                },
                ColumnDef {
                    name: "v".to_string(),
                    data_type: DataType::Varchar,
                    nullable: true,
                    is_primary_key: false,
                    auto_increment: false,
                    default_value: None,
                },
            ],
            row_count: 0,
            indexes: vec![],
            cluster_key: None,
            foreign_keys: vec![],
            engine,
            next_auto_increment_id: 0,
            ttl_seconds: None,
            ttl_column: None,
        }
    }

    fn make_columnar_table(rows: u64) -> EngineTable {
        let mut t = Table::new(make_def("t", EngineType::Columnar), CompactStrategy::default_adaptive(122880));
        for i in 0..rows {
            t.insert(vec![vec![Value::Int64(i as i64), Value::Varchar(format!("r{}", i))]]).unwrap();
        }
        EngineTable::Columnar(t)
    }

    fn make_memory_table(rows: u64) -> EngineTable {
        let mut t = MemoryTable::new(make_def("t", EngineType::Memory));
        let mut batch = Vec::new();
        for i in 0..rows {
            batch.push(vec![Value::Int64(i as i64), Value::Varchar(format!("r{}", i))]);
        }
        t.insert(batch).unwrap();
        EngineTable::Memory(t)
    }

    fn make_log_table(rows: u64) -> EngineTable {
        let mut t = LogTable::with_block_rows(make_def("t", EngineType::Log), 8192);
        let mut batch = Vec::new();
        for i in 0..rows {
            batch.push(vec![Value::Int64(i as i64), Value::Varchar(format!("r{}", i))]);
        }
        t.insert(batch).unwrap();
        EngineTable::Log(t)
    }

    #[test]
    fn test_migrate_columnar_to_memory() {
        let mut table = make_columnar_table(100);
        let r = migrate_table_data(&mut table, EngineType::Columnar, EngineType::Memory).unwrap();
        assert_eq!(r.stats.rows_migrated, 100);
        assert!(matches!(table, EngineTable::Memory(_)));

        // 验证数据完整性
        let rows = extract_all_rows(&mut table).unwrap();
        assert_eq!(rows.len(), 100);
        assert_eq!(rows[0][0], Value::Int64(0));
        assert_eq!(rows[99][1], Value::Varchar("r99".into()));
    }

    #[test]
    fn test_migrate_memory_to_columnar() {
        let mut table = make_memory_table(100);
        let r = migrate_table_data(&mut table, EngineType::Memory, EngineType::Columnar).unwrap();
        assert_eq!(r.stats.rows_migrated, 100);
        assert!(matches!(table, EngineTable::Columnar(_)));

        // 验证数据完整性
        let rows = extract_all_rows(&mut table).unwrap();
        assert_eq!(rows.len(), 100);
        assert_eq!(rows[50][1], Value::Varchar("r50".into()));
    }

    #[test]
    fn test_migrate_columnar_to_log() {
        let mut table = make_columnar_table(500);
        let r = migrate_table_data(&mut table, EngineType::Columnar, EngineType::Log).unwrap();
        assert_eq!(r.stats.rows_migrated, 500);
        assert!(matches!(table, EngineTable::Log(_)));
    }

    #[test]
    fn test_migrate_log_to_columnar() {
        let mut table = make_log_table(500);
        let r = migrate_table_data(&mut table, EngineType::Log, EngineType::Columnar).unwrap();
        assert_eq!(r.stats.rows_migrated, 500);
        assert!(matches!(table, EngineTable::Columnar(_)));
    }

    #[test]
    fn test_migrate_same_engine_error() {
        let mut table = make_memory_table(10);
        let err = migrate_table_data(&mut table, EngineType::Memory, EngineType::Memory);
        assert!(matches!(err, Err(MigrationError::SameEngine(EngineType::Memory))));
    }

    #[test]
    fn test_migrate_to_auto_error() {
        let mut table = make_memory_table(10);
        let err = migrate_table_data(&mut table, EngineType::Memory, EngineType::Auto);
        assert!(matches!(err, Err(MigrationError::UnsupportedPair(_, EngineType::Auto))));
    }

    #[test]
    fn test_migrate_empty_table() {
        let mut table = make_columnar_table(0);
        let r = migrate_table_data(&mut table, EngineType::Columnar, EngineType::Memory).unwrap();
        assert_eq!(r.stats.rows_migrated, 0);
        assert!(matches!(table, EngineTable::Memory(_)));
    }
}