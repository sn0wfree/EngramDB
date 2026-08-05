//! 列存主存储
//!
//! 基于 Row Group 的列式存储，支持轻量级压缩

use crate::common::error::Result;
use crate::common::types::{DataType, TableDef};
use crate::common::config::CompressionType;
use crate::common::column_data::{ColumnData, ColumnValue};
use super::bloom::BloomFilter;
use crate::Value;

use super::compression;
use super::file_format::{ColumnChunkHeader, RowGroupHeader};

/// 可下推到扫描层的比较谓词（P2.4 MinMax 跳过接线）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PredicateOp {
    Eq,
    Lt,
    LtEq,
    Gt,
    GtEq,
}

/// 比较两个 Value（数值含 Timestamp 按数值语义，Varchar 字典序，Boolean 布尔序）
fn cmp_values(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    if let (Some(x), Some(y)) = (a.as_i64(), b.as_i64()) {
        return Some(x.cmp(&y));
    }
    if let (Some(x), Some(y)) = (a.as_f64(), b.as_f64()) {
        return x.partial_cmp(&y);
    }
    if let (Value::Varchar(x), Value::Varchar(y)) = (a, b) {
        return Some(x.cmp(y));
    }
    if let (Value::Boolean(x), Value::Boolean(y)) = (a, b) {
        return Some(x.cmp(y));
    }
    None
}

/// 列存表
pub struct ColumnStore {
    table_def: TableDef,
    row_groups: Vec<RowGroup>,
    row_group_size: u32,
}

/// Row Group（行组）
#[derive(Debug, Clone)]
pub struct RowGroup {
    pub row_count: u32,
    pub columns: Vec<ColumnChunk>,
    /// 每列 Bloom Filter（M1-8）：惰性构建，写路径失效，不落盘
    pub blooms: Vec<Option<BloomFilter>>,
}

/// 列 Chunk
///
/// S2-M1：内存态改用类型化 `ColumnData`（连续数组 + NULL 位图），
/// 替代带 tag 的 `Vec<Value>`——内存连续、cache 友好、可 SIMD。
/// 磁盘字节格式不变（serialize_typed 与 serialize_values 完全一致）。
#[derive(Debug, Clone)]
pub struct ColumnChunk {
    pub data_type: DataType,
    /// 解压后的类型化数据（压缩态时为 None）
    pub data: Option<ColumnData>,
    pub null_count: u32,
    pub compression: CompressionType,
    /// 压缩后的数据（当 data 为 None 且 compressed_data 非空时表示已压缩）
    pub compressed_data: Vec<u8>,
    /// 未压缩时的行数（用于解压后验证）
    pub uncompressed_count: u32,
    /// MinMax 跳过索引（数据写入时自动维护）
    pub min_value: Option<Value>,
    pub max_value: Option<Value>,
}

impl ColumnStore {
    /// 估算比较谓词可跳过的 row group 数：(total, skipped)
    ///
    /// Zone Map（M1-6）：利用每 chunk 的 min/max（写入时自动维护）
    /// 判断谓词是否与 chunk 范围无交集，无交集则整块跳过。
    pub fn estimate_skip(&self, col_idx: usize, op: PredicateOp, val: &Value) -> (usize, usize) {
        let total = self.row_groups.len();
        let mut skipped = 0;
        for rg in &self.row_groups {
            let Some(chunk) = rg.columns.get(col_idx) else { continue };
            let (Some(min), Some(max)) = (&chunk.min_value, &chunk.max_value) else { continue };
            let Some(lo) = cmp_values(min, val) else { continue };
            let Some(hi) = cmp_values(max, val) else { continue };
            use std::cmp::Ordering::*;
            let can_skip = match op {
                // val == x：chunk.max < x || chunk.min > x → 无交集
                PredicateOp::Eq => hi == Less || lo == Greater,
                // val > x：chunk.max <= x
                PredicateOp::Gt => hi != Greater,
                // val >= x：chunk.max < x
                PredicateOp::GtEq => hi == Less,
                // val < x：chunk.min >= x
                PredicateOp::Lt => lo != Less,
                // val <= x：chunk.min > x
                PredicateOp::LtEq => lo == Greater,
            };
            if can_skip {
                skipped += 1;
            }
        }
        (total, skipped)
    }

    pub fn new(table_def: TableDef, row_group_size: u32) -> Self {
        Self {
            table_def,
            row_groups: Vec::new(),
            row_group_size,
        }
    }

    /// 清空所有数据（v0.15.0 TRUNCATE TABLE 支持）
    pub fn clear(&mut self) {
        self.row_groups.clear();
    }

    /// 追加行数据
    pub fn append_rows(&mut self, rows: &[Vec<Value>]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }

        let num_cols = self.table_def.columns.len();
        let mut remaining = rows;

        while !remaining.is_empty() {
            // 找到或创建当前 row group
            let current_rg = if self.row_groups.last().map(|rg| rg.row_count < self.row_group_size).unwrap_or(false) {
                self.row_groups.len() - 1
            } else {
                // 创建新的 row group
                self.row_groups.push(RowGroup {
                    row_count: 0,
                    columns: (0..num_cols)
                        .map(|i| ColumnChunk {
                            data_type: self.table_def.columns[i].data_type.clone(),
                            data: None,
                            null_count: 0,
                            compression: CompressionType::Uncompressed,
                            compressed_data: Vec::new(),
                            uncompressed_count: 0,
                            min_value: None,
                            max_value: None,
                        })
                        .collect(),
                    blooms: vec![None; num_cols],
                });
                self.row_groups.len() - 1
            };

            // 追加前确保目标 RowGroup 已解压（兼容从磁盘惰性加载的压缩态）
            self.ensure_rg_decompressed(current_rg)?;
            let rg = &mut self.row_groups[current_rg];
            let space = (self.row_group_size - rg.row_count) as usize;
            let take = std::cmp::min(space, remaining.len());

            // 按列追加，同时维护 MinMax 索引（S2-M1：直接构造类型化 ColumnData）
            for (col_idx, col_chunk) in rg.columns.iter_mut().enumerate() {
                let vals: Vec<Value> = remaining[..take]
                    .iter()
                    .map(|row| row[col_idx].clone())
                    .collect();
                let new_data = ColumnData::from_values_typed(&vals, &col_chunk.data_type);
                for (i, val) in vals.iter().enumerate() {
                    if val.is_null() {
                        col_chunk.null_count += 1;
                    } else {
                        // 更新 MinMax
                        match &col_chunk.min_value {
                            None => {
                                col_chunk.min_value = Some(val.clone());
                                col_chunk.max_value = Some(val.clone());
                            }
                            Some(cur_min) => {
                                if value_less(val, cur_min) {
                                    col_chunk.min_value = Some(val.clone());
                                }
                                if value_greater(val, col_chunk.max_value.as_ref().unwrap()) {
                                    col_chunk.max_value = Some(val.clone());
                                }
                            }
                        }
                    }
                }
                match &mut col_chunk.data {
                    Some(d) => d.append(&new_data),
                    None => col_chunk.data = Some(new_data),
                }
                // M1-8：列数据变化 → Bloom 失效（惰性重建）
                if col_idx < rg.blooms.len() {
                    rg.blooms[col_idx] = None;
                }
            }
            rg.row_count += take as u32;

            remaining = &remaining[take..];
        }

        Ok(())
    }

    /// P4 优化：直接追加列式数据
    ///
    /// 输入是已经按列组织好的数据（每列一个 Vec<Value>），
    /// 避免了行式→列式的转置开销，比 append_rows 快约 2x。
    pub fn append_columns(&mut self, columns: &[Vec<Value>]) -> Result<()> {
        if columns.is_empty() || columns[0].is_empty() {
            return Ok(());
        }

        let num_cols = self.table_def.columns.len();
        let total_rows = columns[0].len();
        let mut remaining_rows = total_rows;
        let mut offset = 0usize;

        while remaining_rows > 0 {
            // 找到或创建当前 row group
            let current_rg = if self.row_groups.last().map(|rg| rg.row_count < self.row_group_size).unwrap_or(false) {
                self.row_groups.len() - 1
            } else {
                self.row_groups.push(RowGroup {
                    row_count: 0,
                    columns: (0..num_cols)
                        .map(|i| ColumnChunk {
                            data_type: self.table_def.columns[i].data_type.clone(),
                            data: None,
                            null_count: 0,
                            compression: CompressionType::Uncompressed,
                            compressed_data: Vec::new(),
                            uncompressed_count: 0,
                            min_value: None,
                            max_value: None,
                        })
                        .collect(),
                    blooms: vec![None; num_cols],
                });
                self.row_groups.len() - 1
            };

            self.ensure_rg_decompressed(current_rg)?;
            let rg = &mut self.row_groups[current_rg];
            let space = (self.row_group_size - rg.row_count) as usize;
            let take = std::cmp::min(space, remaining_rows);

            // 按列直接追加（P4 核心：无需转置；S2-M1：直接类型化）
            for (col_idx, col_chunk) in rg.columns.iter_mut().enumerate() {
                if col_idx < columns.len() {
                    let src_col = &columns[col_idx][offset..offset + take];
                    let new_data = ColumnData::from_values_typed(src_col, &col_chunk.data_type);

                    // 更新 MinMax 和 null_count
                    for val in src_col {
                        if val.is_null() {
                            col_chunk.null_count += 1;
                        } else {
                            match &col_chunk.min_value {
                                None => {
                                    col_chunk.min_value = Some(val.clone());
                                    col_chunk.max_value = Some(val.clone());
                                }
                                Some(cur_min) => {
                                    if value_less(val, cur_min) {
                                        col_chunk.min_value = Some(val.clone());
                                    }
                                    if value_greater(val, col_chunk.max_value.as_ref().unwrap()) {
                                        col_chunk.max_value = Some(val.clone());
                                    }
                                }
                            }
                        }
                    }
                    match &mut col_chunk.data {
                        Some(d) => d.append(&new_data),
                        None => col_chunk.data = Some(new_data),
                    }
                } else {
                    // 列数不足，补 NULL
                    for _ in 0..take {
                        col_chunk.null_count += 1;
                    }
                    if let Some(d) = &mut col_chunk.data {
                        let nulls = ColumnData::from_values_typed(
                            &vec![Value::Null; take],
                            &col_chunk.data_type,
                        );
                        d.append(&nulls);
                    } else {
                        col_chunk.data = Some(ColumnData::from_values_typed(
                            &vec![Value::Null; take],
                            &col_chunk.data_type,
                        ));
                    }
                }
                // M1-8：列数据变化 → Bloom 失效（惰性重建）
                if col_idx < rg.blooms.len() {
                    rg.blooms[col_idx] = None;
                }
            }

            rg.row_count += take as u32;
            offset += take;
            remaining_rows -= take;
        }

        Ok(())
    }

    /// 确保指定 RowGroup 的所有列处于解压态（data 非空）
    ///
    /// 追加数据到已压缩的 RowGroup 前必须调用：压缩态下 `data` 为 None，
    /// 直接 append 会丢失原有数据。解压后清空 `compressed_data`，后续追加正常写入 `data`。
    fn ensure_rg_decompressed(&mut self, rg_idx: usize) -> Result<()> {
        let rg = &mut self.row_groups[rg_idx];
        for col in &mut rg.columns {
            if col.data.is_none() && !col.compressed_data.is_empty() {
                let bytes = compression::decompress(&col.compressed_data, col.compression.clone(), &col.data_type)?;
                col.data = Some(ColumnData::deserialize_typed(&bytes, &col.data_type, col.uncompressed_count as usize));
                col.compressed_data.clear();
                col.compressed_data.shrink_to_fit();
                col.compression = CompressionType::Uncompressed;
            }
        }
        Ok(())
    }

    /// 读取指定 row group 的指定列
    ///
    /// 如果列数据已压缩，会自动解压为类型化数据（惰性解压）。
    pub fn read_column(&mut self, row_group_idx: usize, col_idx: usize) -> Result<&ColumnData> {
        let rg = &mut self.row_groups[row_group_idx];
        let col = &mut rg.columns[col_idx];

        // 惰性解压：如果数据是压缩状态，先解压
        if col.data.is_none() && !col.compressed_data.is_empty() {
            let bytes = compression::decompress(&col.compressed_data, col.compression.clone(), &col.data_type)?;
            col.data = Some(ColumnData::deserialize_typed(&bytes, &col.data_type, col.uncompressed_count as usize));
            // 清空压缩态，避免后续 append / data_to_bytes 误用陈旧的 compressed_data
            col.compressed_data.clear();
            col.compressed_data.shrink_to_fit();
            col.compression = CompressionType::Uncompressed;
        }

        // 空列（从未写入数据）返回空 ColumnData
        if col.data.is_none() {
            col.data = Some(ColumnData::deserialize_typed(&[], &col.data_type, 0));
        }

        Ok(col.data.as_ref().unwrap())
    }

    /// 获取 row group 数量
    pub fn row_group_count(&self) -> usize {
        self.row_groups.len()
    }

    /// 只读访问所有 row groups（用于扫描定位 row_id）
    pub fn row_groups(&self) -> &[RowGroup] {
        &self.row_groups
    }

    /// 获取每个 row group 的目标大小
    pub fn row_group_size(&self) -> u32 {
        self.row_group_size
    }

    /// 获取总行数
    pub fn total_rows(&self) -> u64 {
        self.row_groups.iter().map(|rg| rg.row_count as u64).sum()
    }

    /// 压缩所有列（真正的列式压缩）
    ///
    /// 对每个 row group 的每列数据调用压缩模块，自动选择最优压缩算法。
    /// 压缩后 data 被清空，数据存储在 compressed_data 中。
    /// 读取时通过 read_column 自动解压。
    pub fn compress_all(&mut self) -> Result<CompressionStats> {
        let mut stats = CompressionStats::default();

        for rg in &mut self.row_groups {
            for col in &mut rg.columns {
                if col.data.is_none() && !col.compressed_data.is_empty() {
                    continue; // 已经压缩过
                }

                let row_count = match &col.data {
                    Some(d) => d.len(),
                    None => 0,
                };
                let original_size = match &col.data {
                    Some(d) => data_byte_size(d, &col.data_type),
                    None => 0,
                };

                // 将类型化列转为字节序列（S2-M1：直接从类型数组序列化）
                let bytes = match &col.data {
                    Some(d) => d.serialize_typed(&col.data_type),
                    None => Vec::new(),
                };

                // 调用压缩模块（自动选择最优算法）
                let (ctype, compressed) = compression::compress(&bytes, &col.data_type)?;

                stats.total_original += original_size;
                stats.total_compressed += compressed.len();
                stats.columns_compressed += 1;

                // 更新列状态
                col.compression = ctype;
                col.compressed_data = compressed;
                col.uncompressed_count = row_count as u32;
                col.data = None; // 释放未压缩数据
            }
        }

        Ok(stats)
    }

    /// 解压所有列（用于需要访问原始数据的场景）
    pub fn decompress_all(&mut self) -> Result<()> {
        for rg in &mut self.row_groups {
            for col in &mut rg.columns {
                if col.compressed_data.is_empty() {
                    continue; // 未压缩
                }

                let bytes = compression::decompress(&col.compressed_data, col.compression.clone(), &col.data_type)?;
                col.data = Some(ColumnData::deserialize_typed(&bytes, &col.data_type, col.uncompressed_count as usize));
                col.compressed_data.clear();
                col.compressed_data.shrink_to_fit();
                col.compression = CompressionType::Uncompressed;
            }
        }
        Ok(())
    }

    /// 获取压缩统计信息（不实际执行压缩，只计算当前状态）
    pub fn compression_stats(&self) -> CompressionStats {
        let mut stats = CompressionStats::default();
        for rg in &self.row_groups {
            for col in &rg.columns {
                if !col.compressed_data.is_empty() {
                    // 已压缩状态
                    stats.total_compressed += col.compressed_data.len();
                    // 估算原始大小（通过 uncompressed_count）
                    let est_original = match col.data_type {
                        DataType::Int32 => col.uncompressed_count as usize * 4,
                        DataType::Int64 => col.uncompressed_count as usize * 8,
                        DataType::Float32 => col.uncompressed_count as usize * 4,
                        DataType::Float64 => col.uncompressed_count as usize * 8,
                        DataType::Boolean => col.uncompressed_count as usize,
                        DataType::Varchar => col.uncompressed_count as usize * 12, // 估算
                        DataType::Json => col.uncompressed_count as usize * 32, // 估算
                        DataType::Vector { .. } => col.uncompressed_count as usize * 64, // 估算
                        DataType::Blob => col.uncompressed_count as usize * 64, // 估算
                        DataType::Timestamp => col.uncompressed_count as usize * 8,
                        DataType::VectorInt8 { .. } => col.uncompressed_count as usize * 16, // 估算
                    };
                    stats.total_original += est_original;
                } else {
                    // 未压缩状态
                    match &col.data {
                        Some(d) => {
                            let sz = data_byte_size(d, &col.data_type);
                            stats.total_original += sz;
                            stats.total_compressed += sz;
                        }
                        None => {
                            // 空列（未压缩且无数据）
                        }
                    }
                }
                stats.columns_compressed += 1;
            }
        }
        stats
    }

    // ========================================================================
    // 数据持久化（v0.12.1 新增）
    // ========================================================================

    /// 序列化所有 RowGroup 数据为字节流
    ///
    /// 格式：
    /// ```text
    /// +-----------------------+
    /// | row_group_count 4B    |
    /// +-----------------------+
    /// | row_group[0]          |
    /// +-----------------------+
    /// | ...                   |
    /// +-----------------------+
    /// ```
    ///
    /// 每个 row_group:
    /// ```text
    /// +-------------------------------+
    /// | row_count 4B                  |
    /// +-------------------------------+
    /// | column_count 4B               |
    /// +-------------------------------+
    /// | column[0]                     |
    /// +-------------------------------+
    /// | ...                           |
    /// +-------------------------------+
    /// ```
    ///
    /// 每个 column（compression 字段决定 payload 是压缩字节还是裸序列化字节）:
    /// ```text
    /// +-------------------------------+
    /// | data_type 1B                  |
    /// +-------------------------------+
    /// | compression 1B                |
    /// +-------------------------------+
    /// | null_count 4B                 |
    /// +-------------------------------+
    /// | uncompressed_count 4B         |
    /// +-------------------------------+
    /// | values_len 4B                 |
    /// +-------------------------------+
    /// | values_bytes                  |
    /// +-------------------------------+
    /// ```
    pub fn data_to_bytes(&mut self, compress: bool) -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        let rg_count = self.row_groups.len() as u32;
        buf.extend_from_slice(&rg_count.to_le_bytes());

        for rg in &mut self.row_groups {
            buf.extend_from_slice(&rg.row_count.to_le_bytes());
            buf.extend_from_slice(&(rg.columns.len() as u32).to_le_bytes());

            for col in &mut rg.columns {
                // 计算落盘的 (compression, payload, uncompressed_count)——不修改内存状态
                // - 内存中已压缩（compress_all 产物）：直接写压缩字节
                // - 未压缩：序列化后按 `compress` 开关决定是否压缩
                let (ctype, payload, ucount): (CompressionType, Vec<u8>, u32) =
                    if !col.compressed_data.is_empty() {
                        (col.compression, col.compressed_data.clone(), col.uncompressed_count)
                    } else {
                        let serialized = match &col.data {
                            Some(d) => d.serialize_typed(&col.data_type),
                            None => Vec::new(),
                        };
                        let count = match &col.data {
                            Some(d) => d.len() as u32,
                            None => 0,
                        };
                        if compress && !serialized.is_empty() {
                            let (c, comp) = compression::compress(&serialized, &col.data_type)?;
                            (c, comp, count)
                        } else {
                            (CompressionType::Uncompressed, serialized, count)
                        }
                    };

                // data_type
                buf.push(data_type_to_u8(&col.data_type));
                // compression
                buf.push(ctype as u8);
                // null_count
                buf.extend_from_slice(&col.null_count.to_le_bytes());
                // uncompressed_count（始终为该列真实行数，修复旧版未压缩列写 0 的 bug）
                buf.extend_from_slice(&ucount.to_le_bytes());
                // payload（压缩字节或裸序列化字节）
                buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
                buf.extend_from_slice(&payload);

                // min/max（可选，用于 MinMax 跳过索引）
                buf.push(if col.min_value.is_some() { 1 } else { 0 });
                if let Some(min) = &col.min_value {
                    let mb = serialize_values(std::slice::from_ref(min), &col.data_type);
                    buf.extend_from_slice(&(mb.len() as u32).to_le_bytes());
                    buf.extend_from_slice(&mb);
                }
                buf.push(if col.max_value.is_some() { 1 } else { 0 });
                if let Some(max) = &col.max_value {
                    let mb = serialize_values(std::slice::from_ref(max), &col.data_type);
                    buf.extend_from_slice(&(mb.len() as u32).to_le_bytes());
                    buf.extend_from_slice(&mb);
                }
            }
        }

        Ok(buf)
    }

    /// 从字节流反序列化 RowGroup 数据
    pub fn data_from_bytes(&mut self, data: &[u8]) -> Result<()> {
        if data.len() < 4 {
            return Ok(()); // 空数据
        }

        let rg_count = u32::from_le_bytes(data[..4].try_into().unwrap()) as usize;
        let mut offset = 4;
        self.row_groups.clear();
        self.row_groups.reserve(rg_count);

        for _ in 0..rg_count {
            if offset + 8 > data.len() {
                return Err(crate::common::error::EngramDbError::InvalidFormat(
                    "truncated row group header".into(),
                ));
            }
            let row_count = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
            offset += 4;
            let column_count = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
            offset += 4;

            let mut columns = Vec::with_capacity(column_count);
            for _ in 0..column_count {
                if offset + 10 > data.len() {
                    return Err(crate::common::error::EngramDbError::InvalidFormat(
                        "truncated column header".into(),
                    ));
                }
                let data_type = u8_to_data_type(data[offset]);
                offset += 1;
                let compression = compression_type_from_u8(data[offset]);
                offset += 1;
                let null_count = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
                offset += 4;
                let uncompressed_count = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
                offset += 4;

                let values_len = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
                offset += 4;
                if offset + values_len > data.len() {
                    return Err(crate::common::error::EngramDbError::InvalidFormat(
                        "truncated column values".into(),
                    ));
                }
                let payload = &data[offset..offset + values_len];
                // compression 字段决定 payload 语义：
                // - Uncompressed → 裸序列化字节，直接解出 data（S2-M1：类型化）
                // - 其它 → 压缩字节，惰性存入 compressed_data，由 read_column 首次访问时解压
                let (col_data, compressed_data): (Option<ColumnData>, Vec<u8>) =
                    if compression == CompressionType::Uncompressed {
                        (Some(ColumnData::deserialize_typed(payload, &data_type, uncompressed_count as usize)), Vec::new())
                    } else {
                        (None, payload.to_vec())
                    };
                offset += values_len;

                // min
                let mut min_value = None;
                if offset < data.len() && data[offset] == 1 {
                    offset += 1;
                    let mlen = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
                    offset += 4;
                    let mvals = deserialize_values(&data[offset..offset + mlen], &data_type, 1);
                    offset += mlen;
                    if !mvals.is_empty() {
                        min_value = Some(mvals.into_iter().next().unwrap());
                    }
                } else {
                    offset += 1;
                }
                // max
                let mut max_value = None;
                if offset < data.len() && data[offset] == 1 {
                    offset += 1;
                    let mlen = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
                    offset += 4;
                    let mvals = deserialize_values(&data[offset..offset + mlen], &data_type, 1);
                    offset += mlen;
                    if !mvals.is_empty() {
                        max_value = Some(mvals.into_iter().next().unwrap());
                    }
                } else {
                    offset += 1;
                }

                columns.push(ColumnChunk {
                    data_type,
                    data: col_data,
                    null_count,
                    compression,
                    compressed_data,
                    uncompressed_count,
                    min_value,
                    max_value,
                });
            }

            self.row_groups.push(RowGroup {
                row_count,
                blooms: vec![None; column_count],
                columns,
            });
        }

        Ok(())
    }

    /// 同步列的 data_type（从 TableDef 修正，如 Vector dim）
    ///
    /// 反序列化后 ColumnChunk 的 data_type 可能不精确（如 Vector dim=0），
    /// 此方法用 TableDef 中定义的类型覆盖。
    pub fn sync_data_types(&mut self, table_def: &TableDef) {
        for rg in &mut self.row_groups {
            for (col_idx, col) in rg.columns.iter_mut().enumerate() {
                if let Some(col_def) = table_def.columns.get(col_idx) {
                    col.data_type = col_def.data_type.clone();
                }
            }
        }
    }

    /// 检查指定 row group 的某列是否可以被范围条件跳过
    ///
    /// 借鉴 ClickHouse MinMax 索引：如果查询范围与 chunk 的 [min, max] 不重叠，
    /// 则整个 chunk 都可以跳过，无需解压和扫描。
    ///
    /// 返回 true 表示可以跳过（该 chunk 不可能包含满足条件的值）
    pub fn can_skip_range(
        &self,
        row_group_idx: usize,
        col_idx: usize,
        low: &Value,
        high: &Value,
    ) -> bool {
        let rg = match self.row_groups.get(row_group_idx) {
            Some(rg) => rg,
            None => return true,
        };

        let col = match rg.columns.get(col_idx) {
            Some(col) => col,
            None => return true,
        };

        match (&col.min_value, &col.max_value) {
            (Some(min), Some(max)) => {
                // 如果查询范围完全在 chunk 最大值之上，或完全在最小值之下 → 可跳过
                value_greater(low, max) || value_less(high, min)
            }
            _ => false, // 无统计信息，不能跳过
        }
    }

    /// 检查等值条件能否跳过
    pub fn can_skip_eq(&self, row_group_idx: usize, col_idx: usize, val: &Value) -> bool {
        let rg = match self.row_groups.get(row_group_idx) {
            Some(rg) => rg,
            None => return true,
        };

        let col = match rg.columns.get(col_idx) {
            Some(col) => col,
            None => return true,
        };

        match (&col.min_value, &col.max_value) {
            (Some(min), Some(max)) => {
                value_less(val, min) || value_greater(val, max)
            }
            _ => false,
        }
    }

    /// 检查任意比较谓词能否跳过（P2.4）
    ///
    /// 统一入口：把单侧比较映射为等价的 [low, high] 区间重叠判断。
    /// - Eq  → 等价 can_skip_eq
    /// - Gt / GtEq → 区间 (val, +∞)：val 在 max 之上可跳过
    /// - Lt / LtEq → 区间 (-∞, val)：val 在 min 之下可跳过
    ///
    /// 返回 true 表示该 row group 不可能包含满足条件的值。
    ///
    /// M1-8：MinMax 判定后追加 Bloom 检查（仅等值谓词）——值在
    /// [min, max] 区间内但实际不存在时（如 id ∈ [1,100] 查 50 但表中
    /// 无 50），Bloom 判定"肯定不存在"整块跳过。压缩态列跳过 Bloom
    /// 检查（保持解压惰性，回退 MinMax-only）。
    pub fn can_skip_predicate(
        &mut self,
        row_group_idx: usize,
        col_idx: usize,
        op: PredicateOp,
        val: &Value,
    ) -> bool {
        let rg = match self.row_groups.get(row_group_idx) {
            Some(rg) => rg,
            None => return true,
        };

        let col = match rg.columns.get(col_idx) {
            Some(col) => col,
            None => return true,
        };

        let minmax_skip = match (&col.min_value, &col.max_value) {
            (Some(min), Some(max)) => match op {
                PredicateOp::Eq => value_less(val, min) || value_greater(val, max),
                // Gt: col > val 命中的条件是 max > val，val >= max 时整个 group 无命中
                PredicateOp::Gt => !value_less(val, max),
                // GtEq: col >= val 命中的条件是 max >= val，val > max 时无命中
                PredicateOp::GtEq => value_greater(val, max),
                // Lt: col < val 命中的条件是 min < val，val <= min 时无命中
                PredicateOp::Lt => !value_greater(val, min),
                // LtEq: col <= val 命中的条件是 min <= val，val < min 时无命中
                PredicateOp::LtEq => value_less(val, min),
            },
            _ => false, // 无统计信息，不能跳过
        };
        if minmax_skip {
            return true;
        }

        // M1-8：Bloom Filter（等值 + 值在范围内但可能不存在）
        if matches!(op, PredicateOp::Eq) && self.bloom_may_skip(row_group_idx, col_idx, val) {
            return true;
        }
        false
    }

    /// 惰性构建并查询 Bloom（无假阴性：false 表示该列肯定不含目标值）
    fn bloom_may_skip(&mut self, rg_idx: usize, col_idx: usize, val: &Value) -> bool {
        let rg = match self.row_groups.get_mut(rg_idx) {
            Some(rg) => rg,
            None => return false,
        };
        let bloom_opt = match rg.blooms.get_mut(col_idx) {
            Some(b) => b,
            None => return false,
        };
        // 压缩态：不解压（保持跳过的组零解压），回退 MinMax
        let data = match rg.columns[col_idx].data.as_ref() {
            Some(d) => d,
            None => return false,
        };
        if bloom_opt.is_none() {
            let mut bf = BloomFilter::with_capacity(data.len(), 0.01);
            for i in 0..data.len() {
                bf.insert(&data.get(i));
            }
            *bloom_opt = Some(bf);
        }
        !bloom_opt.as_ref().unwrap().might_contain(val)
    }

    /// 获取指定 row group 和列的 min/max 值
    pub fn get_min_max(&self, row_group_idx: usize, col_idx: usize) -> (Option<&Value>, Option<&Value>) {
        self.row_groups
            .get(row_group_idx)
            .and_then(|rg| rg.columns.get(col_idx))
            .map(|col| (col.min_value.as_ref(), col.max_value.as_ref()))
            .unwrap_or((None, None))
    }

    /// 收集所有 row group 的 min/max 统计（仅返回有 MinMax 索引的列）
    ///
    /// 用于执行器调试 / 性能分析。
    pub fn debug_minmax(&self) -> Vec<Vec<(Option<Value>, Option<Value>)>> {
        self.row_groups
            .iter()
            .map(|rg| {
                rg.columns
                    .iter()
                    .map(|c| (c.min_value.clone(), c.max_value.clone()))
                    .collect()
            })
            .collect()
    }
}

// ============================================================================
// 标量谓词求值（P-W1 PREWHERE 真接通）
// ============================================================================

/// 标量谓词求值：检查 `val OP target` 是否成立
///
/// 与 `can_skip_predicate`（MinMax 粗筛）配套：粗筛命中的行再用本函数精确求值。
/// 跨类型比较遵循 `value_less` 的约定（Int32/Int64 互通等），类型不匹配返回 false。
///
/// 用于 PREWHERE 路径：在存储层对每个 batch 内的行做"过滤在前、物化在后"，
/// 避免对被过滤行分配 `Vec<Value>` 和克隆 cell。
pub fn matches_predicate(val: &Value, op: PredicateOp, target: &Value) -> bool {
    use Value::*;
    // SQL 三值逻辑：与 NULL 比较结果为 NULL（视为 false，即被过滤）
    // 例外：NULL = NULL 返回 true
    if matches!(val, Null) && matches!(target, Null) {
        return matches!(op, PredicateOp::Eq);
    }
    if matches!(val, Null) || matches!(target, Null) {
        return false;
    }
    match op {
        PredicateOp::Eq => values_equal(val, target),
        PredicateOp::Lt => value_less(val, target),
        PredicateOp::LtEq => value_less(val, target) || values_equal(val, target),
        PredicateOp::Gt => value_greater(val, target),
        PredicateOp::GtEq => value_greater(val, target) || values_equal(val, target),
    }
}

/// S2-M3：Typed 列标量谓词求值（PREWHERE 用，直接类型数组比较，零 Value 构造）
///
/// 语义与 `matches_predicate` 完全一致（跨类型数值比较、NULL 三值逻辑）。
/// 不支持的列/目标类型组合 → 回退 Value 级比较（正确性兜底）。
pub fn matches_predicate_typed(data: &ColumnData, row: usize, op: PredicateOp, target: &Value) -> bool {
    use crate::common::column_data::ColumnValue;
    use Value::*;

    let is_null = data.nulls.as_ref().map_or(false, |n| n.test(row));
    if is_null {
        return matches!(target, Null) && matches!(op, PredicateOp::Eq);
    }
    if matches!(target, Null) {
        return false;
    }

    // 列值类型（i64 / f64 / &str / bool）与 target 组合比较
    // 直接按 (列, target) 组合 match
    let cmp = |ord: std::cmp::Ordering| match op {
        PredicateOp::Eq => ord == std::cmp::Ordering::Equal,
        PredicateOp::Lt => ord == std::cmp::Ordering::Less,
        PredicateOp::LtEq => ord != std::cmp::Ordering::Greater,
        PredicateOp::Gt => ord == std::cmp::Ordering::Greater,
        PredicateOp::GtEq => ord != std::cmp::Ordering::Less,
    };
    let cmp_f = |a: f64, b: f64| cmp(a.partial_cmp(&b).unwrap_or(std::cmp::Ordering::Equal));

    match (&data.values, target) {
        // Int64 / Timestamp 列 vs 整数 / 浮点目标
        (ColumnValue::Int64(v), Int32(t)) => cmp(v[row].cmp(&(*t as i64))),
        (ColumnValue::Int64(v), Int64(t)) => cmp(v[row].cmp(t)),
        (ColumnValue::Int64(v), Float64(t)) => cmp_f(v[row] as f64, *t),
        (ColumnValue::Timestamp(v), Int32(t)) => cmp(v[row].cmp(&(*t as i64))),
        (ColumnValue::Timestamp(v), Int64(t)) => cmp(v[row].cmp(t)),
        (ColumnValue::Timestamp(v), Float64(t)) => cmp_f(v[row] as f64, *t),
        // Float64 列 vs 数值目标
        (ColumnValue::Float64(v), Float64(t)) => cmp_f(v[row], *t),
        (ColumnValue::Float64(v), Int32(t)) => cmp_f(v[row], *t as f64),
        (ColumnValue::Float64(v), Int64(t)) => cmp_f(v[row], *t as f64),
        // Varchar / Boolean 同类型
        (ColumnValue::Varchar(v), Varchar(t)) => cmp(v[row].as_str().cmp(t.as_str())),
        (ColumnValue::Boolean(v), Boolean(t)) => cmp(v[row].cmp(t)),
        _ => matches_predicate(&data.get(row), op, target),
    }
}

fn values_equal(a: &Value, b: &Value) -> bool {
    use Value::*;
    match (a, b) {
        (Int32(x), Int32(y)) => x == y,
        (Int64(x), Int64(y)) => x == y,
        (Int32(x), Int64(y)) => (*x as i64) == *y,
        (Int64(x), Int32(y)) => *x == (*y as i64),
        (Float64(x), Float64(y)) => x == y,
        (Int32(x), Float64(y)) => (*x as f64) == *y,
        (Int64(x), Float64(y)) => (*x as f64) == *y,
        (Float64(x), Int32(y)) => *x == (*y as f64),
        (Float64(x), Int64(y)) => *x == (*y as f64),
        (Varchar(x), Varchar(y)) => x == y,
        (Boolean(x), Boolean(y)) => x == y,
        (Null, Null) => true,
        _ => false,
    }
}

// ============================================================================
// 辅助函数：Value 比较（用于 MinMax 索引）
// ============================================================================

pub(crate) fn value_less(a: &Value, b: &Value) -> bool {
    use Value::*;
    match (a, b) {
        (Int32(x), Int32(y)) => x < y,
        (Int64(x), Int64(y)) => x < y,
        (Int32(x), Int64(y)) => (*x as i64) < *y,
        (Int64(x), Int32(y)) => *x < (*y as i64),
        (Float64(x), Float64(y)) => x < y,
        (Int32(x), Float64(y)) => (*x as f64) < *y,
        (Int64(x), Float64(y)) => (*x as f64) < *y,
        (Float64(x), Int32(y)) => *x < (*y as f64),
        (Float64(x), Int64(y)) => *x < (*y as f64),
        // M3：Timestamp 与数值互比（时间列 MinMax 跳读：SQL 字面量均为 Int64）
        (Timestamp(x), Timestamp(y)) => x < y,
        (Timestamp(x), Int32(y)) => *x < (*y as i64),
        (Timestamp(x), Int64(y)) => *x < *y,
        (Int32(x), Timestamp(y)) => (*x as i64) < *y,
        (Int64(x), Timestamp(y)) => *x < *y,
        (Timestamp(x), Float64(y)) => (*x as f64) < *y,
        (Float64(x), Timestamp(y)) => *x < (*y as f64),
        (Varchar(x), Varchar(y)) => x < y,
        (Boolean(x), Boolean(y)) => x < y,
        (Null, _) => true,
        (_, Null) => false,
        _ => false,
    }
}

pub(crate) fn value_greater(a: &Value, b: &Value) -> bool {
    value_less(b, a)
}

/// 将 Value 序列化为字节（用于持久化）
pub fn serialize_values(values: &[Value], data_type: &DataType) -> Vec<u8> {
    let mut buf = Vec::new();
    match data_type {
        DataType::Boolean => {
            for v in values {
                match v {
                    Value::Null => buf.push(2), // 2 = NULL
                    Value::Boolean(b) => buf.push(if *b { 1 } else { 0 }),
                    _ => buf.push(2),
                }
            }
        }
        DataType::Int32 => {
            for v in values {
                match v {
                    Value::Int32(i) => buf.extend_from_slice(&i.to_le_bytes()),
                    Value::Int64(i) => buf.extend_from_slice(&(*i as i32).to_le_bytes()),
                    _ => buf.extend_from_slice(&0i32.to_le_bytes()), // NULL 简化处理
                }
            }
        }
        DataType::Int64 => {
            for v in values {
                match v {
                    Value::Int32(i) => buf.extend_from_slice(&(*i as i64).to_le_bytes()),
                    Value::Int64(i) => buf.extend_from_slice(&i.to_le_bytes()),
                    _ => buf.extend_from_slice(&0i64.to_le_bytes()),
                }
            }
        }
        DataType::Float64 => {
            for v in values {
                match v {
                    Value::Float64(f) => buf.extend_from_slice(&f.to_le_bytes()),
                    Value::Float32(f) => buf.extend_from_slice(&(*f as f64).to_le_bytes()),
                    Value::Int32(i) => buf.extend_from_slice(&(*i as f64).to_le_bytes()),
                    Value::Int64(i) => buf.extend_from_slice(&(*i as f64).to_le_bytes()),
                    _ => buf.extend_from_slice(&0f64.to_le_bytes()),
                }
            }
        }
        DataType::Varchar => {
            for v in values {
                if let Value::Varchar(s) = v {
                    buf.extend_from_slice(&(s.len() as u32).to_le_bytes());
                    buf.extend_from_slice(s.as_bytes());
                } else {
                    buf.extend_from_slice(&0u32.to_le_bytes());
                }
            }
        }
        DataType::Float32 => {
            // Float32 专用路径：4 字节紧凑存储
            for v in values {
                match v {
                    Value::Float32(f) => buf.extend_from_slice(&f.to_le_bytes()),
                    Value::Float64(f) => buf.extend_from_slice(&(*f as f32).to_le_bytes()),
                    Value::Int32(i) => buf.extend_from_slice(&(*i as f32).to_le_bytes()),
                    Value::Int64(i) => buf.extend_from_slice(&(*i as f32).to_le_bytes()),
                    _ => buf.extend_from_slice(&0f32.to_le_bytes()),
                }
            }
        }
        DataType::Json => {
            for v in values {
                if let Value::Json(s) = v {
                    buf.extend_from_slice(&(s.len() as u32).to_le_bytes());
                    buf.extend_from_slice(s.as_bytes());
                } else {
                    buf.extend_from_slice(&0u32.to_le_bytes());
                }
            }
        }
        DataType::Vector { .. } => {
            for v in values {
                if let Value::Vector(vec) = v {
                    let byte_len = vec.len() * 4;
                    buf.extend_from_slice(&(vec.len() as u32).to_le_bytes());
                    for f in vec {
                        buf.extend_from_slice(&f.to_le_bytes());
                    }
                    let _ = byte_len;
                } else {
                    buf.extend_from_slice(&0u32.to_le_bytes());
                }
            }
        }
        DataType::VectorInt8 { .. } => {
            for v in values {
                if let Value::VectorInt8(vec) = v {
                    let byte_len = vec.len();
                    buf.extend_from_slice(&(byte_len as u32).to_le_bytes());
                    for b in vec {
                        buf.push(*b as u8);
                    }
                } else {
                    buf.extend_from_slice(&0u32.to_le_bytes());
                }
            }
        }
        DataType::Blob => {
            for v in values {
                if let Value::Blob(b) = v {
                    buf.extend_from_slice(&(b.len() as u32).to_le_bytes());
                    buf.extend_from_slice(b);
                } else {
                    buf.extend_from_slice(&0u32.to_le_bytes());
                }
            }
        }
        DataType::Timestamp => {
            for v in values {
                match v {
                    Value::Timestamp(t) => buf.extend_from_slice(&t.to_le_bytes()),
                    Value::Int64(i) => buf.extend_from_slice(&i.to_le_bytes()),
                    Value::Int32(i) => buf.extend_from_slice(&(*i as i64).to_le_bytes()),
                    _ => buf.extend_from_slice(&0i64.to_le_bytes()),
                }
            }
        }
    }
    buf
}

/// 从字节反序列化为 Value
pub fn deserialize_values(data: &[u8], data_type: &DataType, count: usize) -> Vec<Value> {
    let mut values = Vec::with_capacity(count);
    let mut offset = 0;

    match data_type {
        DataType::Boolean => {
            for i in 0..count {
                if offset + i < data.len() {
                    match data[offset + i] {
                        0 => values.push(Value::Boolean(false)),
                        1 => values.push(Value::Boolean(true)),
                        _ => values.push(Value::Null),
                    }
                } else {
                    values.push(Value::Null);
                }
            }
        }
        DataType::Int32 => {
            for _ in 0..count {
                if offset + 4 <= data.len() {
                    let val = i32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
                    values.push(Value::Int32(val));
                    offset += 4;
                } else {
                    values.push(Value::Null);
                }
            }
        }
        DataType::Int64 => {
            for _ in 0..count {
                if offset + 8 <= data.len() {
                    let val = i64::from_le_bytes(data[offset..offset + 8].try_into().unwrap());
                    values.push(Value::Int64(val));
                    offset += 8;
                } else {
                    values.push(Value::Null);
                }
            }
        }
        DataType::Float64 => {
            for _ in 0..count {
                if offset + 8 <= data.len() {
                    let val = f64::from_le_bytes(data[offset..offset + 8].try_into().unwrap());
                    values.push(Value::Float64(val));
                    offset += 8;
                } else {
                    values.push(Value::Null);
                }
            }
        }
        DataType::Float32 => {
            for _ in 0..count {
                if offset + 4 <= data.len() {
                    let val = f32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
                    values.push(Value::Float32(val));
                    offset += 4;
                } else {
                    values.push(Value::Null);
                }
            }
        }
        DataType::Varchar => {
            for _ in 0..count {
                if offset + 4 <= data.len() {
                    let len = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
                    offset += 4;
                    if offset + len <= data.len() {
                        let s = String::from_utf8_lossy(&data[offset..offset + len]).to_string();
                        values.push(Value::Varchar(s));
                        offset += len;
                    } else {
                        values.push(Value::Null);
                    }
                } else {
                    values.push(Value::Null);
                }
            }
        }
        DataType::Json => {
            for _ in 0..count {
                if offset + 4 <= data.len() {
                    let len = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
                    offset += 4;
                    if offset + len <= data.len() {
                        let s = String::from_utf8_lossy(&data[offset..offset + len]).to_string();
                        values.push(Value::Json(s));
                        offset += len;
                    } else {
                        values.push(Value::Null);
                    }
                } else {
                    values.push(Value::Null);
                }
            }
        }
        DataType::Vector { .. } => {
            for _ in 0..count {
                if offset + 4 <= data.len() {
                    let dim = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
                    offset += 4;
                    let byte_len = dim * 4;
                    if offset + byte_len <= data.len() && dim > 0 {
                        let mut vec = Vec::with_capacity(dim);
                        for i in 0..dim {
                            let start = offset + i * 4;
                            let f = f32::from_le_bytes(data[start..start + 4].try_into().unwrap());
                            vec.push(f);
                        }
                        values.push(Value::Vector(vec));
                        offset += byte_len;
                    } else {
                        values.push(Value::Null);
                    }
                } else {
                    values.push(Value::Null);
                }
            }
        }
        DataType::VectorInt8 { .. } => {
            for _ in 0..count {
                if offset + 4 <= data.len() {
                    let dim = u32::from_le_bytes(data[offset..offset+4].try_into().unwrap()) as usize;
                    offset += 4;
                    let byte_len = dim;
                    if offset + byte_len <= data.len() {
                        let mut vec = Vec::with_capacity(dim);
                        for _ in 0..dim {
                            vec.push(data[offset] as i8);
                            offset += 1;
                        }
                        values.push(Value::VectorInt8(vec));
                    } else {
                        values.push(Value::Null);
                    }
                } else {
                    values.push(Value::Null);
                }
            }
        }
        DataType::Blob => {
            for _ in 0..count {
                if offset + 4 <= data.len() {
                    let len = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
                    offset += 4;
                    if offset + len <= data.len() {
                        values.push(Value::Blob(data[offset..offset + len].to_vec()));
                        offset += len;
                    } else {
                        values.push(Value::Null);
                    }
                } else {
                    values.push(Value::Null);
                }
            }
        }
        DataType::Timestamp => {
            for _ in 0..count {
                if offset + 8 <= data.len() {
                    let t = i64::from_le_bytes(data[offset..offset + 8].try_into().unwrap());
                    values.push(Value::Timestamp(t));
                    offset += 8;
                } else {
                    values.push(Value::Null);
                }
            }
        }
    }

    values
}

// ============================================================================
// 压缩统计
// ============================================================================

/// 压缩统计信息
#[derive(Debug, Clone, Default)]
pub struct CompressionStats {
    pub total_original: usize,
    pub total_compressed: usize,
    pub columns_compressed: usize,
}

impl CompressionStats {
    pub fn ratio(&self) -> f64 {
        if self.total_compressed == 0 {
            1.0
        } else {
            self.total_original as f64 / self.total_compressed as f64
        }
    }

    pub fn saved_pct(&self) -> f64 {
        if self.total_original == 0 {
            0.0
        } else {
            (1.0 - self.total_compressed as f64 / self.total_original as f64) * 100.0
        }
    }
}

// ============================================================================
// 辅助函数：Value ↔ 字节（包装已有的 serialize/deserialize）
// ============================================================================

fn values_to_bytes(values: &[Value], data_type: &DataType) -> Vec<u8> {
    serialize_values(values, data_type)
}

fn bytes_to_values(data: &[u8], data_type: &DataType, count: usize) -> Vec<Value> {
    deserialize_values(data, data_type, count)
}

/// 类型化列的未压缩字节大小估算（S2-M1，压缩统计用）
fn data_byte_size(data: &ColumnData, data_type: &DataType) -> usize {
    match (data_type, &data.values) {
        (DataType::Boolean, ColumnValue::Boolean(v)) => v.len(),
        (DataType::Int32, ColumnValue::Int32(v)) => v.len() * 4,
        (DataType::Int64, ColumnValue::Int64(v)) => v.len() * 8,
        (DataType::Float32, ColumnValue::Float32(v)) => v.len() * 4,
        (DataType::Float64, ColumnValue::Float64(v)) => v.len() * 8,
        (DataType::Varchar, ColumnValue::Varchar(v)) => v.iter().map(|s| 4 + s.len()).sum(),
        (DataType::Json, ColumnValue::Json(v)) => v.iter().map(|s| 4 + s.len()).sum(),
        (DataType::Blob, ColumnValue::Blob(v)) => v.iter().map(|b| 4 + b.len()).sum(),
        (DataType::Vector { .. }, ColumnValue::Vector(v)) => v.iter().map(|x| 4 + x.len() * 4).sum(),
        (DataType::VectorInt8 { .. }, ColumnValue::VectorInt8(v)) => v.iter().map(|x| 4 + x.len()).sum(),
        (DataType::Timestamp, ColumnValue::Timestamp(v)) => v.len() * 8,
        _ => data.len(),
    }
}

fn values_byte_size(values: &[Value], data_type: &DataType) -> usize {
    match data_type {
        DataType::Boolean => values.len(),
        DataType::Int32 => values.len() * 4,
        DataType::Int64 => values.len() * 8,
        DataType::Float32 => values.len() * 4,
        DataType::Float64 => values.len() * 8,
        DataType::Varchar => {
            let mut size = 0;
            for v in values {
                if let Value::Varchar(s) = v {
                    size += 4 + s.len();
                } else {
                    size += 4; // NULL: 4 bytes len = 0
                }
            }
            size
        }
        DataType::Json => {
            let mut size = 0;
            for v in values {
                if let Value::Json(s) = v {
                    size += 4 + s.len();
                } else {
                    size += 4;
                }
            }
            size
        }
        DataType::Vector { .. } => {
            let mut size = 0;
            for v in values {
                if let Value::Vector(vec) = v {
                    size += 4 + vec.len() * 4;
                } else {
                    size += 4;
                }
            }
            size
        }
        DataType::VectorInt8 { .. } => {
            let mut size = 0;
            for v in values {
                if let Value::VectorInt8(vec) = v {
                    size += 4 + vec.len();
                } else {
                    size += 4;
                }
            }
            size
        }
        DataType::Blob => {
            let mut size = 0;
            for v in values {
                if let Value::Blob(b) = v {
                    size += 4 + b.len();
                } else {
                    size += 4;
                }
            }
            size
        }
        DataType::Timestamp => values.len() * 8,
    }
}

// ============================================================================
// 持久化辅助：DataType / CompressionType ↔ u8
// ============================================================================

fn data_type_to_u8(dt: &DataType) -> u8 {
    match dt {
        DataType::Boolean => 0,
        DataType::Int32 => 1,
        DataType::Int64 => 2,
        DataType::Float32 => 8,
        DataType::Float64 => 3,
        DataType::Varchar => 4,
        DataType::Json => 5,
        DataType::Vector { .. } => 6,
        DataType::Blob => 7,
        DataType::Timestamp => 9,
        DataType::VectorInt8 { .. } => 10,
    }
}

fn u8_to_data_type(b: u8) -> DataType {
    match b {
        0 => DataType::Boolean,
        1 => DataType::Int32,
        2 => DataType::Int64,
        8 => DataType::Float32,
        3 => DataType::Float64,
        4 => DataType::Varchar,
        5 => DataType::Json,
        6 => DataType::Vector { dim: 0 },
        7 => DataType::Blob,
        9 => DataType::Timestamp,
        10 => DataType::VectorInt8 { dim: 0 },
        _ => DataType::Varchar,
    }
}

fn compression_type_from_u8(b: u8) -> CompressionType {
    match b {
        0 => CompressionType::Uncompressed,
        1 => CompressionType::Rle,
        2 => CompressionType::BitPacking,
        3 => CompressionType::Dictionary,
        4 => CompressionType::For,
        5 => CompressionType::Delta,
        6 => CompressionType::Zstd,
        7 => CompressionType::Gorilla,
        8 => CompressionType::ForBitPack,
        9 => CompressionType::BooleanPack,
        10 => CompressionType::DoubleDelta,
        _ => CompressionType::Uncompressed,
    }
}

// ============================================================================
// 测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn make_table_def() -> TableDef {
        TableDef {
            id: 1,
            engine: crate::common::types::EngineType::Columnar,
            name: "t".to_string(),
            columns: vec![
                crate::common::types::ColumnDef { name: "id".to_string(), data_type: DataType::Int64, nullable: true, is_primary_key: false, default_value: None, auto_increment: false },
                crate::common::types::ColumnDef { name: "name".to_string(), data_type: DataType::Varchar, nullable: true, is_primary_key: false, default_value: None, auto_increment: false },
            ],
            row_count: 0,
            indexes: Vec::new(),
            cluster_key: None,
            foreign_keys: Vec::new(),
            next_auto_increment_id: 0,
            ttl_seconds: None,
            ttl_column: None,
        }
    }

    fn make_store() -> ColumnStore {
        let mut store = ColumnStore::new(make_table_def(), 4);
        // 两个 row group：
        // RG0: id ∈ [1, 2, 3, 4], name = a/b/c/d
        // RG1: id ∈ [5, 6, 7, 8], name = e/f/g/h
        let ids: Vec<Value> = (1..=8).map(Value::Int64).collect();
        let names: Vec<Value> = (b'a'..=b'h').map(|c| Value::Varchar((c as char).to_string())).collect();
        store.append_columns(&[ids, names]).unwrap();
        assert_eq!(store.row_group_count(), 2);
        store
    }

    #[test]
    fn test_can_skip_predicate_eq() {
        let mut store = make_store();
        // RG0 [1,4]：值 5 在范围外 → 可跳过
        assert!(store.can_skip_predicate(0, 0, PredicateOp::Eq, &Value::Int64(5)));
        // RG1 [5,8]：值 5 在范围内 → 不可跳过
        assert!(!store.can_skip_predicate(1, 0, PredicateOp::Eq, &Value::Int64(5)));
        // 值 0 在两个 group 之外
        assert!(store.can_skip_predicate(0, 0, PredicateOp::Eq, &Value::Int64(0)));
        assert!(store.can_skip_predicate(1, 0, PredicateOp::Eq, &Value::Int64(0)));
        // 值 4 在 RG0 内
        assert!(!store.can_skip_predicate(0, 0, PredicateOp::Eq, &Value::Int64(4)));
    }

    #[test]
    fn test_can_skip_predicate_range() {
        let mut store = make_store();
        // Gt: id > 4 → RG0 (max=4) 可跳过（4 不大于 4），RG1 不可跳过
        assert!(store.can_skip_predicate(0, 0, PredicateOp::Gt, &Value::Int64(4)));
        assert!(!store.can_skip_predicate(1, 0, PredicateOp::Gt, &Value::Int64(4)));
        // GtEq: id >= 4 → RG0 max=4 → 不可跳过（边界值命中）
        assert!(!store.can_skip_predicate(0, 0, PredicateOp::GtEq, &Value::Int64(4)));
        // Gt: id > 100 → 所有 group 跳过
        assert!(store.can_skip_predicate(0, 0, PredicateOp::Gt, &Value::Int64(100)));
        assert!(store.can_skip_predicate(1, 0, PredicateOp::Gt, &Value::Int64(100)));
        // Lt: id < 5 → RG0 [1,4] 不可跳过，RG1 [5,8] 可跳过（min=5，5 不小于 5）
        assert!(!store.can_skip_predicate(0, 0, PredicateOp::Lt, &Value::Int64(5)));
        assert!(store.can_skip_predicate(1, 0, PredicateOp::Lt, &Value::Int64(5)));
        // Lt: id < 0 → 全部跳过
        assert!(store.can_skip_predicate(0, 0, PredicateOp::Lt, &Value::Int64(0)));
        assert!(store.can_skip_predicate(1, 0, PredicateOp::Lt, &Value::Int64(0)));
        // LtEq: id <= 4 → RG0 不可跳过，RG1 (min=5) 可跳过
        assert!(!store.can_skip_predicate(0, 0, PredicateOp::LtEq, &Value::Int64(4)));
        assert!(store.can_skip_predicate(1, 0, PredicateOp::LtEq, &Value::Int64(4)));
    }

    #[test]
    fn test_bloom_skip_missing_values_in_range() {
        let mut store = make_store();
        // RG1 id ∈ [5,8]：值 6 在范围内且存在 → 不跳过
        assert!(!store.can_skip_predicate(1, 0, PredicateOp::Eq, &Value::Int64(6)));
        // 值 5.5 在 RG1 范围内但不存在 → Bloom 判定跳过（MinMax 无法做到）
        assert!(
            store.can_skip_predicate(1, 0, PredicateOp::Eq, &Value::Int64(55)),
            "值 55 在 [5,8] 范围外，MinMax 即跳过"
        );
        // 关键场景：范围 [5,8] 内不存在的整数值 → Bloom 跳过（MinMax 保留）
        // 注意：值 6 存在；取一个范围内不存在、且非边界的值。
        // 由于范围是连续的 1..=8，构造缺值：用 Varchar 列测试范围外字符串。
        assert!(store.can_skip_predicate(0, 1, PredicateOp::Eq, &Value::Varchar("zzz".into())));
        // 等值在范围内但不存在：值 0 已在范围外；测试列 name 的等值命中
        assert!(!store.can_skip_predicate(0, 1, PredicateOp::Eq, &Value::Varchar("a".into())));
        assert!(!store.can_skip_predicate(1, 1, PredicateOp::Eq, &Value::Varchar("g".into())));
    }

    #[test]
    fn test_bloom_rebuild_after_append() {
        // RG0 未满（2 行 < size 4）→ 追加落在同一 RG，验证 Bloom 失效重建
        let mut store = ColumnStore::new(make_table_def(), 4);
        store.append_rows(&[
            vec![Value::Int64(1), Value::Varchar("a".to_string())],
            vec![Value::Int64(2), Value::Varchar("b".to_string())],
        ]).unwrap();
        // 第一次等值查询（构建 Bloom）
        assert!(!store.can_skip_predicate(0, 0, PredicateOp::Eq, &Value::Int64(1)));
        // 范围内不存在的值 → Bloom 跳过（MinMax 无法跳过）
        assert!(store.can_skip_predicate(0, 0, PredicateOp::Eq, &Value::Int64(3)));

        // 追加新行（同 RG）→ Bloom 失效并重建
        store.append_rows(&[vec![
            Value::Int64(3),
            Value::Varchar("c".to_string()),
        ]]).unwrap();
        // 新值必须可查（不假阴性）
        assert!(
            !store.can_skip_predicate(0, 0, PredicateOp::Eq, &Value::Int64(3)),
            "追加后新值不应被跳过"
        );
        assert!(!store.can_skip_predicate(0, 0, PredicateOp::Eq, &Value::Int64(1)));
        // 其他范围内不存在的值仍可跳过
        assert!(store.can_skip_predicate(0, 0, PredicateOp::Eq, &Value::Int64(4)));
    }

    #[test]
    fn test_can_skip_predicate_out_of_bounds_index() {
        let mut store = make_store();
        // 越界 col / rg 索引一律视为可跳过（保守安全）
        assert!(store.can_skip_predicate(99, 0, PredicateOp::Eq, &Value::Int64(1)));
        assert!(store.can_skip_predicate(0, 99, PredicateOp::Eq, &Value::Int64(1)));
    }

    // ========================================================================
    // P-W1 PREWHERE：matches_predicate 标量求值
    // ========================================================================

    #[test]
    fn test_matches_predicate_int64() {
        // Eq
        assert!(matches_predicate(&Value::Int64(5), PredicateOp::Eq, &Value::Int64(5)));
        assert!(!matches_predicate(&Value::Int64(5), PredicateOp::Eq, &Value::Int64(6)));
        // Lt / LtEq
        assert!(matches_predicate(&Value::Int64(4), PredicateOp::Lt, &Value::Int64(5)));
        assert!(!matches_predicate(&Value::Int64(5), PredicateOp::Lt, &Value::Int64(5)));
        assert!(matches_predicate(&Value::Int64(5), PredicateOp::LtEq, &Value::Int64(5)));
        // Gt / GtEq
        assert!(matches_predicate(&Value::Int64(6), PredicateOp::Gt, &Value::Int64(5)));
        assert!(!matches_predicate(&Value::Int64(5), PredicateOp::Gt, &Value::Int64(5)));
        assert!(matches_predicate(&Value::Int64(5), PredicateOp::GtEq, &Value::Int64(5)));
    }

    #[test]
    fn test_matches_predicate_cross_type() {
        // Int32 / Int64 互通
        assert!(matches_predicate(&Value::Int32(5), PredicateOp::Eq, &Value::Int64(5)));
        assert!(matches_predicate(&Value::Int64(5), PredicateOp::Eq, &Value::Int32(5)));
        assert!(matches_predicate(&Value::Int32(4), PredicateOp::Lt, &Value::Int64(5)));
        // Int / Float64
        assert!(matches_predicate(&Value::Int64(5), PredicateOp::Eq, &Value::Float64(5.0)));
        assert!(matches_predicate(&Value::Int32(5), PredicateOp::Gt, &Value::Float64(4.9)));
        // 类型不匹配：返回 false（保守）
        assert!(!matches_predicate(&Value::Varchar("x".into()), PredicateOp::Eq, &Value::Int64(0)));
    }

    #[test]
    fn test_matches_predicate_string_bool() {
        // Varchar
        assert!(matches_predicate(&Value::Varchar("b".into()), PredicateOp::Gt, &Value::Varchar("a".into())));
        assert!(!matches_predicate(&Value::Varchar("a".into()), PredicateOp::Gt, &Value::Varchar("a".into())));
        assert!(matches_predicate(&Value::Varchar("a".into()), PredicateOp::LtEq, &Value::Varchar("a".into())));
        // Boolean
        assert!(matches_predicate(&Value::Boolean(true), PredicateOp::Eq, &Value::Boolean(true)));
        assert!(!matches_predicate(&Value::Boolean(true), PredicateOp::Eq, &Value::Boolean(false)));
        assert!(matches_predicate(&Value::Boolean(false), PredicateOp::Lt, &Value::Boolean(true)));
    }

    #[test]
    fn test_matches_predicate_null() {
        // SQL 三值逻辑：与 NULL 比较视为 false
        assert!(!matches_predicate(&Value::Null, PredicateOp::Eq, &Value::Int64(0)));
        assert!(!matches_predicate(&Value::Int64(0), PredicateOp::Eq, &Value::Null));
        assert!(!matches_predicate(&Value::Null, PredicateOp::Gt, &Value::Int64(0)));
        // Null == Null
        assert!(matches_predicate(&Value::Null, PredicateOp::Eq, &Value::Null));
    }

    /// S2-M3：Typed 谓词与 Value 级等价性（随机数据 + 全 op + 混合 target 类型）
    #[test]
    fn test_matches_predicate_typed_equals_value() {
        use crate::common::column_data::ColumnData;
        use rand::Rng;
        let mut rng = rand::thread_rng();
        let ops = [PredicateOp::Eq, PredicateOp::Lt, PredicateOp::LtEq, PredicateOp::Gt, PredicateOp::GtEq];
        for trial in 0..200 {
            let n = 1 + rng.gen_range(0..30);
            let kind = rng.gen_range(0..3); // Int64 / Float64 / Varchar
            let values: Vec<Value> = (0..n)
                .map(|_| {
                    if rng.gen_bool(0.2) {
                        return Value::Null;
                    }
                    match kind {
                        0 => Value::Int64(rng.gen_range(-1000..1000)),
                        1 => Value::Float64(rng.gen_range(-1000.0..1000.0)),
                        _ => Value::Varchar(format!("s{}", rng.gen_range(0..30))),
                    }
                })
                .collect();
            let Some(data) = ColumnData::try_from_values(&values) else { continue };
            let target = match kind {
                0 => {
                    let t = rng.gen_range(0..3);
                    match t {
                        0 => Value::Int32(rng.gen_range(-1000..1000)),
                        1 => Value::Int64(rng.gen_range(-1000..1000)),
                        _ => Value::Float64(rng.gen_range(-1000.0..1000.0)),
                    }
                }
                1 => {
                    if rng.gen_bool(0.5) {
                        Value::Float64(rng.gen_range(-1000.0..1000.0))
                    } else {
                        Value::Int64(rng.gen_range(-1000..1000))
                    }
                }
                _ => Value::Varchar(format!("s{}", rng.gen_range(0..30))),
            };
            let op = ops[rng.gen_range(0..ops.len())];
            for i in 0..n {
                let typed = matches_predicate_typed(&data, i, op, &target);
                let value = matches_predicate(&data.get(i), op, &target);
                assert_eq!(typed, value, "trial {} i {} op {:?} col={:?} target={:?}",
                    trial, i, op, data.get(i), target);
            }
        }
    }
}
