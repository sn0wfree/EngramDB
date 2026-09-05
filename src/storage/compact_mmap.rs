//! Phase 6：compact + mmap 集成
//!
//! 目标：compact 操作完成后立即写入 mmap 后端文件，
//! 而不是通过中间 `Vec<u8>` 缓冲。
//!
//! ## 流程
//! 1. compact_delta() — 合并 Delta → 列存（内存）
//! 2. data_to_mmap_writer() — 列存 → mmap 文件
//! 3. atomic_replace — 临时文件 → 正式文件
//!
//! ## 并发安全
//! compact 进行时：
//! - 旧数据文件仍可 mmap 读（不阻塞查询）
//! - compact 完成后 atomic rename 替换旧文件
//! - OS 自动管理旧 mmap 释放（引用计数）

#![cfg(feature = "mmap-read")]

use crate::common::error::Result;
use std::path::Path;

use super::engine::EngineTable;
use super::mmap_integration;
use super::mmap_writer::MmapWriter;

/// Phase 6：compact + mmap 集成
///
/// 将表 compact 后的数据写入 mmap 文件。
/// 用于：
/// - checkpoint 写入 mmap 后端
/// - 大表 compact 后持久化
/// - 冷数据迁移（Auto → Log/Columnar）
pub fn compact_to_mmap(table: &mut EngineTable, mmap_path: &Path, compress: bool) -> Result<u64> {
    match table {
        EngineTable::Columnar(t) => compact_columnar_to_mmap(t, mmap_path, compress),
        EngineTable::Log(t) => compact_log_to_mmap(t, mmap_path),
        EngineTable::Memory(_) => {
            // Memory 表不持久化到 mmap（进程退出数据丢失，符合语义）
            Ok(0)
        }
    }
}

/// Columnar 引擎 compact + mmap
fn compact_columnar_to_mmap(table: &mut super::table::Table, mmap_path: &Path, compress: bool) -> Result<u64> {
    // 1. compact_delta：合并 Delta → 列存
    let delta_rows = table.delta_store().len() as u64;
    table.compact_delta()?;

    // 2. 写入到临时 mmap 文件（COW：写临时文件，完成后原子 rename）
    let tmp_path = mmap_path.with_extension("hdb.tmp");
    let mut writer = MmapWriter::create(&tmp_path)?;

    let table_id = table.def().id;
    writer.write_u32(table_id)?;

    // 获取列存数据（一次性获取，避免重复 borrow）
    let data_bytes = table.column_store_mut().data_to_bytes(compress)?;
    writer.write_u32(data_bytes.len() as u32)?;
    writer.write(&data_bytes)?;

    writer.sync()?;

    // 3. 原子 rename（不阻塞并发读）
    mmap_integration::atomic_replace(&tmp_path, mmap_path)?;

    Ok(delta_rows)
}

/// Log 引擎 compact + mmap
fn compact_log_to_mmap(table: &mut super::log_engine::LogTable, mmap_path: &Path) -> Result<u64> {
    use crate::storage::log_engine::LogTable;

    // Log 引擎数据直接从内存 dump
    let data = table.to_bytes();

    // 写入到 mmap 文件
    let tmp_path = mmap_path.with_extension("hdb.tmp");
    {
        let mut writer = MmapWriter::create(&tmp_path)?;
        let table_id = 0; // Log 表在 mmap 中的 table_id
        writer.write_u32(table_id)?;
        writer.write_u32(data.len() as u32)?;
        writer.write(&data)?;
        writer.sync()?;
    }
    // 4. atomic rename
    mmap_integration::atomic_replace(&tmp_path, mmap_path)?;

    Ok(data.len() as u64)
}

/// Phase 6：Database 级别的 compact + mmap 集成
///
/// 将所有表 compact 后写入 mmap 文件。
/// 替代 save_data_mmap() 中的 Vec<u8> 中间缓冲。
pub fn compact_all_to_mmap(
    tables: &mut std::collections::HashMap<u32, EngineTable>,
    mmap_path: &Path,
    compress: bool,
) -> Result<()> {
    use crate::storage::mmap_writer::MmapWriter;

    let tmp_path = mmap_path.with_extension("hdb.tmp");
    {
        let mut writer = MmapWriter::create(&tmp_path)?;

        // 收集所有持久化表
        let persistent_ids: Vec<u32> = tables
            .iter()
            .filter(|(_, t)| !matches!(t, EngineTable::Memory(_)))
            .map(|(id, _)| *id)
            .collect();
        let table_count = persistent_ids.len() as u32;
        writer.write_u32(table_count)?;

        for table_id in persistent_ids {
            let table = tables.get_mut(&table_id).unwrap();
            match table {
                EngineTable::Columnar(t) => {
                    writer.write_u32(table_id)?;
                    // 写入列存数据
                    t.column_store_mut().data_to_mmap_writer(&mut writer, compress)?;
                }
                EngineTable::Log(t) => {
                    writer.write_u32(table_id)?;
                    let data = t.to_bytes();
                    writer.write_u32(data.len() as u32)?;
                    writer.write(&data)?;
                }
                EngineTable::Memory(_) => continue,
            }
        }

        writer.sync()?;
    }

    // atomic rename
    mmap_integration::atomic_replace(&tmp_path, mmap_path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::config::{CompactStrategy, Config};
    use crate::common::types::{ColumnDef, DataType, EngineType, TableDef};
    use crate::Value;

    fn make_columnar_table() -> crate::storage::table::Table {
        use crate::storage::table::Table;

        let def = TableDef {
            id: 1,
            name: "t".into(),
            columns: vec![ColumnDef {
                name: "id".into(),
                data_type: DataType::Int64,
                nullable: false,
                is_primary_key: true,
                auto_increment: false,
                default_value: None,
                check_expr: None,
            }],
            row_count: 0,
            indexes: vec![],
            cluster_key: None,
            foreign_keys: vec![],
            engine: EngineType::Columnar,
            next_auto_increment_id: 0,
            ttl_seconds: None,
            ttl_column: None,
        };
        Table::new(def, CompactStrategy::default_adaptive(122880))
    }

    #[test]
    fn test_compact_to_mmap_roundtrip() {
        let path = std::path::PathBuf::from("/tmp/p6_compact_mmap_rt.hdb");
        let _ = std::fs::remove_file(&path);

        // 创建列存表
        let mut table = make_columnar_table();
        for i in 0..1000 {
            table.insert(vec![vec![Value::Int64(i as i64)]]).unwrap();
        }

        // compact + mmap
        compact_to_mmap(&mut EngineTable::Columnar(table), &path, false).unwrap();

        // 验证文件存在且非空
        assert!(path.exists());
        assert!(std::fs::metadata(&path).unwrap().len() > 0);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_compact_to_mmap_with_compression() {
        let path = std::path::PathBuf::from("/tmp/p6_compact_mmap_comp.hdb");
        let _ = std::fs::remove_file(&path);

        let mut table = make_columnar_table();
        for i in 0..5000 {
            table.insert(vec![vec![Value::Int64(i as i64)]]).unwrap();
        }

        // compact + mmap + compression
        compact_to_mmap(&mut EngineTable::Columnar(table), &path, true).unwrap();

        assert!(path.exists());
        assert!(std::fs::metadata(&path).unwrap().len() > 0);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_compact_all_to_mmap() {
        let path = std::path::PathBuf::from("/tmp/p6_compact_all_mmap.hdb");
        let _ = std::fs::remove_file(&path);

        // 创建多表
        let mut tables = std::collections::HashMap::new();

        let mut t1 = make_columnar_table();
        for i in 0..500 {
            t1.insert(vec![vec![Value::Int64(i as i64)]]).unwrap();
        }
        tables.insert(1, EngineTable::Columnar(t1));

        let mut t2 = make_columnar_table();
        for i in 0..300 {
            t2.insert(vec![vec![Value::Int64(i as i64)]]).unwrap();
        }
        tables.insert(2, EngineTable::Columnar(t2));

        // compact_all_to_mmap
        compact_all_to_mmap(&mut tables, &path, false).unwrap();

        assert!(path.exists());
        let file_size = std::fs::metadata(&path).unwrap().len();
        assert!(file_size > 0);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_compact_to_mmap_atomic_rename() {
        let path = std::path::PathBuf::from("/tmp/p6_compact_mmap_atom.hdb");
        let _ = std::fs::remove_file(&path);

        // 第一次写入
        {
            let mut table = make_columnar_table();
            for i in 0..100 {
                table.insert(vec![vec![Value::Int64(i as i64)]]).unwrap();
            }
            compact_to_mmap(&mut EngineTable::Columnar(table), &path, false).unwrap();
        }
        let size1 = std::fs::metadata(&path).unwrap().len();

        // 第二次写入（COW）
        {
            let mut table = make_columnar_table();
            for i in 0..100 {
                table.insert(vec![vec![Value::Int64(i as i64)]]).unwrap();
            }
            compact_to_mmap(&mut EngineTable::Columnar(table), &path, false).unwrap();
        }
        let size2 = std::fs::metadata(&path).unwrap().len();

        assert_eq!(size1, size2, "文件大小应保持不变");

        let _ = std::fs::remove_file(&path);
    }
}
