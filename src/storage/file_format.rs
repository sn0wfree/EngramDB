//! 文件格式定义

use crate::common::config::{Config, CompressionType};
use crate::common::error::{HybridDbError, Result};

/// 文件魔数
pub const MAGIC: &[u8; 17] = b"HYBRIDDB_FORMAT1\0";

/// 文件头（4KB 页对齐）
#[derive(Debug, Clone)]
pub struct FileHeader {
    pub magic: [u8; 17],
    pub version: u16,
    pub page_size: u32,
    pub block_size: u32,
    pub meta_root: u32,
    pub total_rows: u64,
    pub total_data_blocks: u64,
    pub schema_cookie: u32,
    pub checkpoint_lsn: u64,
    pub uuid: [u8; 16],
    pub compression_default: CompressionType,
    /// 索引段偏移（v0.12.0 新增，0 = 无索引）
    pub index_root: u32,
    /// 索引段大小（字节）
    pub index_size: u32,
    /// Catalog 段偏移（v0.12.1 新增，0 = 无 catalog）
    ///
    /// 存储所有表的 schema 定义（TableDef / ColumnDef / IndexDef）。
    pub catalog_root: u32,
    /// Catalog 段大小（字节）
    pub catalog_size: u32,
    /// 数据段偏移（v0.12.1 新增，0 = 无数据）
    ///
    /// 存储所有表的列存 RowGroup 数据。
    pub data_root: u32,
    /// 数据段大小（字节）
    pub data_size: u32,
}

impl FileHeader {
    pub fn new(config: &Config) -> Self {
        use rand::Rng;
        let mut uuid = [0u8; 16];
        rand::thread_rng().fill(&mut uuid);

        Self {
            magic: *MAGIC,
            version: 1,
            page_size: config.page_size,
            block_size: config.block_size,
            meta_root: 0,
            total_rows: 0,
            total_data_blocks: 0,
            schema_cookie: 0,
            checkpoint_lsn: 0,
            uuid,
            compression_default: config.default_compression,
            index_root: 0,
            index_size: 0,
            catalog_root: 0,
            catalog_size: 0,
            data_root: 0,
            data_size: 0,
        }
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        let mut buf = Vec::with_capacity(self.page_size as usize);

        // Magic (16B)
        buf.extend_from_slice(&self.magic);
        // Version (2B)
        buf.extend_from_slice(&self.version.to_le_bytes());
        // Page size (4B)
        buf.extend_from_slice(&self.page_size.to_le_bytes());
        // Block size (4B)
        buf.extend_from_slice(&self.block_size.to_le_bytes());
        // Meta root (4B)
        buf.extend_from_slice(&self.meta_root.to_le_bytes());
        // Total rows (8B)
        buf.extend_from_slice(&self.total_rows.to_le_bytes());
        // Total data blocks (8B)
        buf.extend_from_slice(&self.total_data_blocks.to_le_bytes());
        // Schema cookie (4B)
        buf.extend_from_slice(&self.schema_cookie.to_le_bytes());
        // Checkpoint LSN (8B)
        buf.extend_from_slice(&self.checkpoint_lsn.to_le_bytes());
        // UUID (16B)
        buf.extend_from_slice(&self.uuid);
        // Compression default (1B)
        buf.push(self.compression_default as u8);

        // Index root (4B)
        buf.extend_from_slice(&self.index_root.to_le_bytes());
        // Index size (4B)
        buf.extend_from_slice(&self.index_size.to_le_bytes());

        // Catalog root (4B) — v0.12.1
        buf.extend_from_slice(&self.catalog_root.to_le_bytes());
        // Catalog size (4B)
        buf.extend_from_slice(&self.catalog_size.to_le_bytes());
        // Data root (4B) — v0.12.1
        buf.extend_from_slice(&self.data_root.to_le_bytes());
        // Data size (4B)
        buf.extend_from_slice(&self.data_size.to_le_bytes());

        // 填充到页大小
        buf.resize(self.page_size as usize, 0);

        Ok(buf)
    }

    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        if data.len() < 100 {
            return Err(HybridDbError::InvalidFormat(
                "File header too short".into()
            ));
        }

        // 检查魔数
        if &data[..MAGIC.len()] != MAGIC {
            return Err(HybridDbError::InvalidFormat(
                "Invalid magic number, not a HybridDB file".into()
            ));
        }

        let magic_len = MAGIC.len();
        let version = u16::from_le_bytes(data[magic_len..magic_len+2].try_into().unwrap());
        let page_size = u32::from_le_bytes(data[magic_len+2..magic_len+6].try_into().unwrap());
        let block_size = u32::from_le_bytes(data[magic_len+6..magic_len+10].try_into().unwrap());
        let meta_root = u32::from_le_bytes(data[magic_len+10..magic_len+14].try_into().unwrap());
        let total_rows = u64::from_le_bytes(data[magic_len+14..magic_len+22].try_into().unwrap());
        let total_data_blocks = u64::from_le_bytes(data[magic_len+22..magic_len+30].try_into().unwrap());
        let schema_cookie = u32::from_le_bytes(data[magic_len+30..magic_len+34].try_into().unwrap());
        let checkpoint_lsn = u64::from_le_bytes(data[magic_len+34..magic_len+42].try_into().unwrap());

        let mut uuid = [0u8; 16];
        uuid.copy_from_slice(&data[magic_len+42..magic_len+58]);

        let compression_default = match data[magic_len+58] {
            0 => CompressionType::Uncompressed,
            1 => CompressionType::Rle,
            2 => CompressionType::BitPacking,
            3 => CompressionType::Dictionary,
            4 => CompressionType::For,
            5 => CompressionType::Delta,
            6 => CompressionType::Zstd,
            _ => return Err(HybridDbError::InvalidFormat(
                format!("Unknown compression type: {}", data[74])
            )),
        };

        // Index root + size (v0.12.0)
        let index_root = u32::from_le_bytes(data[magic_len+59..magic_len+63].try_into().unwrap());
        let index_size = u32::from_le_bytes(data[magic_len+63..magic_len+67].try_into().unwrap());

        // Catalog + Data 段（v0.12.1 新增）
        // 老文件无此字段，默认为 0（无 catalog/数据，按空库处理）
        let catalog_root = if data.len() >= magic_len + 75 {
            u32::from_le_bytes(data[magic_len+67..magic_len+71].try_into().unwrap())
        } else { 0 };
        let catalog_size = if data.len() >= magic_len + 79 {
            u32::from_le_bytes(data[magic_len+71..magic_len+75].try_into().unwrap())
        } else { 0 };
        let data_root = if data.len() >= magic_len + 83 {
            u32::from_le_bytes(data[magic_len+75..magic_len+79].try_into().unwrap())
        } else { 0 };
        let data_size = if data.len() >= magic_len + 87 {
            u32::from_le_bytes(data[magic_len+79..magic_len+83].try_into().unwrap())
        } else { 0 };

        Ok(Self {
            magic: MAGIC.clone(),
            version,
            page_size,
            block_size,
            meta_root,
            total_rows,
            total_data_blocks,
            schema_cookie,
            checkpoint_lsn,
            uuid,
            compression_default,
            index_root,
            index_size,
            catalog_root,
            catalog_size,
            data_root,
            data_size,
        })
    }
}

/// Row Group 头
#[derive(Debug, Clone)]
pub struct RowGroupHeader {
    pub row_count: u32,
    pub column_count: u16,
    pub column_offsets: Vec<u64>,
    pub column_sizes: Vec<u64>,
}

/// 列 Chunk 头
#[derive(Debug, Clone)]
pub struct ColumnChunkHeader {
    pub compression_type: CompressionType,
    pub uncompressed_size: u32,
    pub compressed_size: u32,
    pub null_count: u32,
    pub min_value: Vec<u8>,
    pub max_value: Vec<u8>,
}

impl ColumnChunkHeader {
    pub fn serialized_size(&self) -> usize {
        1 + 4 + 4 + 4 + 4 + self.min_value.len() + 4 + self.max_value.len()
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.push(self.compression_type as u8);
        buf.extend_from_slice(&self.uncompressed_size.to_le_bytes());
        buf.extend_from_slice(&self.compressed_size.to_le_bytes());
        buf.extend_from_slice(&self.null_count.to_le_bytes());
        buf.extend_from_slice(&(self.min_value.len() as u32).to_le_bytes());
        buf.extend_from_slice(&self.min_value);
        buf.extend_from_slice(&(self.max_value.len() as u32).to_le_bytes());
        buf.extend_from_slice(&self.max_value);
        buf
    }
}
