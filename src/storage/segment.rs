//! LSM 段文件格式（Step 2.1）
//!
//! 每个段是一个不可变的排序行组集合，包含：
//! - N 个 RowGroup（每个 = 多个 ColumnChunk）
//! - 段尾 Footer（RG 偏移表 + 列级 zone map + Bloom）
//!
//! 段文件命名：`segments/seg_{table_id}_{seq:08}.hdb`
//! 单段上限：SEGMENT_MAX_SIZE（128MB）

use std::path::Path;
use std::sync::Arc;

use crate::Value;
use crate::common::column_data::ColumnData;
use crate::common::config::CompressionType;
use crate::common::types::{DataType, TableDef};
use crate::common::error::{EngramDbError, Result};
use crate::storage::bloom_filter::ColumnBloom;
use crate::storage::column_store::{ColumnChunk, ColumnStore, RowGroup};

/// 单段最大字节数（128MB）
pub const SEGMENT_MAX_SIZE: u64 = 128 * 1024 * 1024;

/// 段文件魔数
pub const SEGMENT_MAGIC: u32 = 0x5345_474D; // "SEGM"

/// 段文件 Footer
#[derive(Debug, Clone)]
pub struct SegmentFooter {
    pub rg_offsets: Vec<u64>,
    pub zone_maps: Vec<Vec<(Option<Value>, Option<Value>)>>,
    pub blooms: Vec<Vec<Option<Arc<ColumnBloom>>>>,
    pub total_rows: u32,
}

/// 段元数据（存储在 Manifest 中）
#[derive(Debug, Clone)]
pub struct SegmentEntry {
    pub path: std::path::PathBuf,
    pub table_id: u32,
    pub row_range: (u64, u64),
    pub total_rows: u32,
    pub size_bytes: u64,
    pub zone_map_summary: Vec<(Option<Value>, Option<Value>)>,
    pub bloom_summary: Vec<Option<Arc<ColumnBloom>>>,
}

impl SegmentEntry {
    pub fn may_contain(&self, col_idx: usize, val: &Value) -> bool {
        if let Some((min, max)) = self.zone_map_summary.get(col_idx) {
            if let (Some(min), Some(max)) = (min, max) {
                if val < min || val > max {
                    return false;
                }
            }
        }
        true
    }

    pub fn bloom_may_contain(&self, col_idx: usize, val: &Value) -> bool {
        if let Some(bloom) = self.bloom_summary.get(col_idx) {
            if let Some(bloom) = bloom {
                use crate::storage::bloom_filter::{bloomable_key, is_bloomable};
                if is_bloomable(val) {
                    return bloom.may_contain(&bloomable_key(val));
                }
            }
        }
        true
    }
}

/// 从字节缓冲读取段文件
pub fn read_segment_from_bytes(data: &[u8]) -> Result<(ColumnStore, SegmentFooter)> {
    let mut offset = 0usize;

    let rg_count = read_u32(data, &mut offset)? as usize;

    let mut rg_offsets = Vec::with_capacity(rg_count);
    let mut zone_maps = Vec::with_capacity(rg_count);
    let mut blooms = Vec::with_capacity(rg_count);
    let mut row_groups = Vec::with_capacity(rg_count);
    let mut total_rows = 0u32;

    for _ in 0..rg_count {
        rg_offsets.push(offset as u64);

        let row_count = read_u32(data, &mut offset)?;
        total_rows += row_count;
        let col_count = read_u32(data, &mut offset)? as usize;

        let mut columns = Vec::with_capacity(col_count);
        let mut rg_zone_maps = Vec::with_capacity(col_count);
        let mut rg_blooms = Vec::with_capacity(col_count);

        for _ in 0..col_count {
            let data_type = u8_to_data_type(data[offset]);
            offset += 1;

            let ctype_byte = data[offset];
            offset += 1;

            let null_count = read_u32(data, &mut offset)?;
            let uncompressed_count = read_u32(data, &mut offset)?;
            let payload_len = read_u32(data, &mut offset)? as usize;
            let payload = data[offset..offset + payload_len].to_vec();
            offset += payload_len;

            // zone map
            let min_value = if data[offset] == 1 {
                offset += 1;
                let mlen = read_u32(data, &mut offset)? as usize;
                let mbytes = &data[offset..offset + mlen];
                offset += mlen;
                deserialize_value(mbytes, &data_type)
            } else {
                offset += 1;
                None
            };
            let max_value = if data[offset] == 1 {
                offset += 1;
                let mlen = read_u32(data, &mut offset)? as usize;
                let mbytes = &data[offset..offset + mlen];
                offset += mlen;
                deserialize_value(mbytes, &data_type)
            } else {
                offset += 1;
                None
            };

            // Bloom
            let bloom_len = read_u32(data, &mut offset)? as usize;
            let bloom = if bloom_len > 0 {
                let bloom_bytes = &data[offset..offset + bloom_len];
                offset += bloom_len;
                ColumnBloom::from_bytes(bloom_bytes).map(Arc::new)
            } else {
                None
            };

            let (col_data, compressed_data) = if ctype_byte == 0 {
                let cd = ColumnData::deserialize_typed(&payload, &data_type, uncompressed_count as usize);
                (Some(cd), Vec::new())
            } else {
                (None, payload)
            };

            rg_zone_maps.push((min_value.clone(), max_value.clone()));
            rg_blooms.push(bloom.clone());

            columns.push(ColumnChunk {
                data_type,
                data: col_data,
                null_count,
                compression: CompressionType::Uncompressed, // simplified
                compressed_data,
                uncompressed_count,
                min_value,
                max_value,
                bloom,
            });
        }

        row_groups.push(RowGroup {
            row_count,
            columns,
            blooms: vec![None; col_count],
        });
        zone_maps.push(rg_zone_maps);
        blooms.push(rg_blooms);
    }

    let column_store = ColumnStore::from_row_groups(row_groups);

    Ok((column_store, SegmentFooter {
        rg_offsets,
        zone_maps,
        blooms,
        total_rows,
    }))
}

// ============================================================================
// 辅助函数
// ============================================================================

fn data_type_to_u8(dt: &DataType) -> u8 {
    match dt {
        DataType::Boolean => 0,
        DataType::Int32 => 1,
        DataType::Int64 => 2,
        DataType::Float32 => 3,
        DataType::Float64 => 4,
        DataType::Varchar => 5,
        DataType::Json => 6,
        DataType::Blob => 7,
        DataType::Vector { .. } => 8,
        DataType::VectorInt8 { .. } => 9,
        DataType::Timestamp => 10,
        // v0.22.0 新增类型
        DataType::Jsonb => 11,
        DataType::Date => 12,
        DataType::Time => 13,
        DataType::Uuid => 14,
        DataType::Array { .. } => 15,
        DataType::Enum { .. } => 16,
    }
}

fn u8_to_data_type(v: u8) -> DataType {
    match v {
        0 => DataType::Boolean,
        1 => DataType::Int32,
        2 => DataType::Int64,
        3 => DataType::Float32,
        4 => DataType::Float64,
        5 => DataType::Varchar,
        6 => DataType::Json,
        7 => DataType::Blob,
        10 => DataType::Timestamp,
        _ => DataType::Int64,
    }
}

fn deserialize_value(bytes: &[u8], data_type: &DataType) -> Option<Value> {
    if bytes.is_empty() {
        return None;
    }
    match data_type {
        DataType::Int32 => {
            let v = i32::from_le_bytes(bytes[..4].try_into().ok()?);
            Some(Value::Int32(v))
        }
        DataType::Int64 => {
            let v = i64::from_le_bytes(bytes[..8].try_into().ok()?);
            Some(Value::Int64(v))
        }
        DataType::Float32 => {
            let v = f32::from_le_bytes(bytes[..4].try_into().ok()?);
            Some(Value::Float32(v))
        }
        DataType::Float64 => {
            let v = f64::from_le_bytes(bytes[..8].try_into().ok()?);
            Some(Value::Float64(v))
        }
        DataType::Timestamp => {
            let v = i64::from_le_bytes(bytes[..8].try_into().ok()?);
            Some(Value::Timestamp(v))
        }
        DataType::Varchar => {
            let v = String::from_utf8_lossy(bytes).to_string();
            Some(Value::Varchar(v))
        }
        DataType::Json => {
            let v = String::from_utf8_lossy(bytes).to_string();
            Some(Value::Json(v))
        }
        _ => None,
    }
}

fn read_u32(data: &[u8], offset: &mut usize) -> Result<u32> {
    if *offset + 4 > data.len() {
        return Err(EngramDbError::InvalidFormat("segment file truncated".into()));
    }
    let v = u32::from_le_bytes(data[*offset..*offset + 4].try_into().unwrap());
    *offset += 4;
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::types::ColumnDef;

    fn make_test_column_store() -> ColumnStore {
        use crate::common::types::{ColumnDef, DataType};

        let mut cs = ColumnStore::new(
            TableDef {
                id: 1,
                name: "test".into(),
                columns: vec![ColumnDef { name: "id".into(), data_type: DataType::Int64, nullable: false, is_primary_key: false, default_value: None, auto_increment: false, check_expr: None }],
                row_count: 0,
                indexes: vec![],
                cluster_key: None,
                foreign_keys: vec![],
                engine: crate::common::types::EngineType::Columnar,
                next_auto_increment_id: 0,
                ttl_seconds: None,
                ttl_column: None,
            },
            122_880,
        );
        // Append some data
        let values: Vec<Value> = (0..1000i64).map(|i| Value::Int64(i)).collect();
        cs.append_rows(&[values]).unwrap();
        cs
    }

    #[test]
    fn test_segment_entry_may_contain() {
        let entry = SegmentEntry {
            path: "test.hdb".into(),
            table_id: 1,
            row_range: (0, 1000),
            total_rows: 1000,
            size_bytes: 1024,
            zone_map_summary: vec![(Some(Value::Int64(0)), Some(Value::Int64(999)))],
            bloom_summary: vec![],
        };
        assert!(entry.may_contain(0, &Value::Int64(500)));
        assert!(!entry.may_contain(0, &Value::Int64(1000)));
        assert!(!entry.may_contain(0, &Value::Int64(-1)));
    }

    #[test]
    fn test_manifest_add_remove() {
        let mut manifest = crate::storage::manifest::Manifest::new();
        assert_eq!(manifest.version, 1);

        manifest.add_segment(crate::storage::manifest::ManifestSegment {
            path: "seg_1.hdb".into(),
            table_id: 1,
            row_range: (0, 1000),
            total_rows: 1000,
            size_bytes: 1024,
        });
        assert_eq!(manifest.version, 2);
        assert_eq!(manifest.segments.len(), 1);

        manifest.remove_segments(&["seg_1.hdb"]);
        assert_eq!(manifest.version, 3);
        assert_eq!(manifest.segments.len(), 0);
    }

    #[test]
    fn test_manifest_table_segments() {
        let mut manifest = crate::storage::manifest::Manifest::new();
        manifest.add_segment(crate::storage::manifest::ManifestSegment {
            path: "seg_1.hdb".into(),
            table_id: 1,
            row_range: (0, 1000),
            total_rows: 1000,
            size_bytes: 1024,
        });
        manifest.add_segment(crate::storage::manifest::ManifestSegment {
            path: "seg_2.hdb".into(),
            table_id: 2,
            row_range: (0, 500),
            total_rows: 500,
            size_bytes: 512,
        });

        assert_eq!(manifest.table_segments(1).len(), 1);
        assert_eq!(manifest.table_segments(2).len(), 1);
        assert_eq!(manifest.table_row_count(1), 1000);
        assert_eq!(manifest.table_row_count(2), 500);
    }
}
