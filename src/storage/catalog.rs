//! Catalog 持久化模块（v0.12.1 新增）
//!
//! 解决 P0 致命问题：表 schema 不持久化，重启后表全丢失。
//!
//! 将所有表的 schema 定义（TableDef / ColumnDef / IndexDef）序列化到
//! 文件的 Catalog 段，重启时反序列化恢复表结构。
//!
//! ## 文件布局
//! ```text
//! [FileHeader 4KB]
//! [Catalog Section]  ← 本模块负责
//! [Data Section]     ← data.rs 负责（RowGroup 数据）
//! [Index Section]    ← v0.12.0 索引持久化
//! ```
//!
//! ## Catalog 段格式
//! ```text
//! +------------------+
//! | next_table_id 4B |
//! +------------------+
//! | table_count 4B   |
//! +------------------+
//! | table[0] entry   |
//! +------------------+
//! | table[1] entry   |
//! +------------------+
//! | ...              |
//! +------------------+
//! ```
//!
//! 每个 table entry:
//! ```text
//! +-----------------------+
//! | table_id 4B           |
//! +-----------------------+
//! | TableDef bincode len  |
//! +-----------------------+
//! | TableDef bincode data |
//! +-----------------------+
//! ```

use crate::common::error::{EngramDbError, Result};
use crate::common::types::TableDef;
use std::collections::HashMap;

/// Catalog 快照（待持久化的元数据）
#[derive(Debug)]
pub struct CatalogSnapshot {
    pub next_table_id: u32,
    /// (table_id, table_def) 列表
    pub tables: Vec<(u32, TableDef)>,
}

impl CatalogSnapshot {
    /// 从数据库实例收集 catalog 快照
    pub fn collect(
        next_table_id: u32,
        tables: &HashMap<u32, crate::storage::engine::EngineTable>,
    ) -> Self {
        let mut snapshot_tables = Vec::with_capacity(tables.len());
        for (&table_id, table) in tables {
            snapshot_tables.push((table_id, table.def().clone()));
        }
        snapshot_tables.sort_by_key(|(id, _)| *id);
        Self {
            next_table_id,
            tables: snapshot_tables,
        }
    }

    /// 序列化为字节
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        let mut buf = Vec::new();

        // next_table_id
        buf.extend_from_slice(&self.next_table_id.to_le_bytes());
        // table_count
        let count = self.tables.len() as u32;
        buf.extend_from_slice(&count.to_le_bytes());

        for (table_id, table_def) in &self.tables {
            // table_id
            buf.extend_from_slice(&table_id.to_le_bytes());
            // TableDef bincode
            let def_bytes = bincode::serialize(table_def)
                .map_err(|e| EngramDbError::Internal(format!("serialize TableDef: {}", e)))?;
            buf.extend_from_slice(&(def_bytes.len() as u32).to_le_bytes());
            buf.extend_from_slice(&def_bytes);
        }

        Ok(buf)
    }

    /// 从字节反序列化
    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        if data.len() < 8 {
            return Err(EngramDbError::InvalidFormat("catalog section too short".into()));
        }

        let next_table_id = u32::from_le_bytes(data[..4].try_into().unwrap());
        let count = u32::from_le_bytes(data[4..8].try_into().unwrap()) as usize;
        let mut offset = 8;
        let mut tables = Vec::with_capacity(count);

        for _ in 0..count {
            if offset + 4 > data.len() {
                return Err(EngramDbError::InvalidFormat("truncated table id".into()));
            }
            let table_id = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
            offset += 4;

            if offset + 4 > data.len() {
                return Err(EngramDbError::InvalidFormat("truncated table def len".into()));
            }
            let def_len = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
            offset += 4;

            if offset + def_len > data.len() {
                return Err(EngramDbError::InvalidFormat("truncated table def data".into()));
            }
            let table_def: TableDef = bincode::deserialize(&data[offset..offset + def_len])
                .map_err(|e| EngramDbError::InvalidFormat(format!("deserialize TableDef: {}", e)))?;
            offset += def_len;

            tables.push((table_id, table_def));
        }

        Ok(Self {
            next_table_id,
            tables,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::types::{ColumnDef, DataType};

    #[test]
    fn test_catalog_roundtrip() {
        let def1 = TableDef::new(
            1,
            "users",
            vec![
                ColumnDef::new("id", DataType::Int64).primary_key(),
                ColumnDef::new("name", DataType::Varchar).not_null(),
                ColumnDef::new("score", DataType::Float64),
            ],
        );
        let def2 = TableDef::new(
            2,
            "orders",
            vec![
                ColumnDef::new("id", DataType::Int64).primary_key(),
                ColumnDef::new("user_id", DataType::Int64).not_null(),
                ColumnDef::new("amount", DataType::Float64),
            ],
        );

        let snapshot = CatalogSnapshot {
            next_table_id: 3,
            tables: vec![(1, def1), (2, def2)],
        };

        let bytes = snapshot.to_bytes().unwrap();
        let restored = CatalogSnapshot::from_bytes(&bytes).unwrap();

        assert_eq!(restored.next_table_id, 3);
        assert_eq!(restored.tables.len(), 2);
        assert_eq!(restored.tables[0].0, 1);
        assert_eq!(restored.tables[0].1.name, "users");
        assert_eq!(restored.tables[1].0, 2);
        assert_eq!(restored.tables[1].1.name, "orders");
        assert!(restored.tables[0].1.columns[0].is_primary_key);
        assert!(!restored.tables[0].1.columns[1].nullable);
    }

    #[test]
    fn test_catalog_empty() {
        let snapshot = CatalogSnapshot {
            next_table_id: 1,
            tables: vec![],
        };
        let bytes = snapshot.to_bytes().unwrap();
        let restored = CatalogSnapshot::from_bytes(&bytes).unwrap();
        assert_eq!(restored.next_table_id, 1);
        assert!(restored.tables.is_empty());
    }

    #[test]
    fn test_catalog_truncated_data_rejected() {
        assert!(CatalogSnapshot::from_bytes(&[0u8; 7]).is_err(), "短于 8 字节应报错");
        // 截断的 table id
        let bytes = vec![0u8, 0, 0, 0, 1, 0, 0, 0, 0, 0];
        assert!(CatalogSnapshot::from_bytes(&bytes).is_err());
    }

    #[test]
    fn test_catalog_corrupt_def_rejected() {
        let snapshot = CatalogSnapshot {
            next_table_id: 1,
            tables: vec![(
                1,
                TableDef::new(1, "t", vec![ColumnDef::new("id", DataType::Int64)]),
            )],
        };
        let mut bytes = snapshot.to_bytes().unwrap();
        // 破坏 def 字节
        let def_len = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
        let def_start = 16;
        for b in &mut bytes[def_start..def_start + def_len] {
            *b ^= 0xFF;
        }
        assert!(CatalogSnapshot::from_bytes(&bytes).is_err(), "损坏的 TableDef 应报错");
    }

    #[test]
    fn test_catalog_collect_sorts_by_id() {
        let mut tables = HashMap::new();
        tables.insert(
            2,
            crate::storage::engine::EngineTable::Memory(crate::storage::memory_engine::MemoryTable::new(
                TableDef::new(2, "b", vec![ColumnDef::new("id", DataType::Int64)]),
            )),
        );
        tables.insert(
            1,
            crate::storage::engine::EngineTable::Memory(crate::storage::memory_engine::MemoryTable::new(
                TableDef::new(1, "a", vec![ColumnDef::new("id", DataType::Int64)]),
            )),
        );
        let snap = CatalogSnapshot::collect(3, &tables);
        assert_eq!(snap.tables[0].0, 1);
        assert_eq!(snap.tables[1].0, 2);
        assert_eq!(snap.next_table_id, 3);
    }
}
