//! 列存主存储
//!
//! 基于 Row Group 的列式存储，支持轻量级压缩

use crate::common::error::Result;
use crate::common::types::{DataType, TableDef};
use crate::common::config::CompressionType;
use crate::Value;

use super::compression;
use super::file_format::{ColumnChunkHeader, RowGroupHeader};

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
}

/// 列 Chunk
///
/// **已知限制（P0-3）**：`values: Vec<Value>` 使用带 tag 的 enum，
/// 破坏列存的内存连续性，无法做 SIMD 向量化。
/// 后续应重构为 `enum ColumnData { Int64(Vec<i64>), Float64(Vec<f64>), ... }`，
/// 但需同步修改 `read_column` 签名与所有调用方，工作量较大，留作独立后续任务。
#[derive(Debug, Clone)]
pub struct ColumnChunk {
    pub data_type: DataType,
    pub values: Vec<Value>,
    pub null_count: u32,
    pub compression: CompressionType,
    /// 压缩后的数据（当 values 为空且 compressed_data 非空时表示已压缩）
    pub compressed_data: Vec<u8>,
    /// 未压缩时的行数（用于解压后验证）
    pub uncompressed_count: u32,
    /// MinMax 跳过索引（数据写入时自动维护）
    pub min_value: Option<Value>,
    pub max_value: Option<Value>,
}

impl ColumnStore {
    pub fn new(table_def: TableDef, row_group_size: u32) -> Self {
        Self {
            table_def,
            row_groups: Vec::new(),
            row_group_size,
        }
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
                            values: Vec::new(),
                            null_count: 0,
                            compression: CompressionType::Uncompressed,
                            compressed_data: Vec::new(),
                            uncompressed_count: 0,
                            min_value: None,
                            max_value: None,
                        })
                        .collect(),
                });
                self.row_groups.len() - 1
            };

            // 追加前确保目标 RowGroup 已解压（兼容从磁盘惰性加载的压缩态）
            self.ensure_rg_decompressed(current_rg)?;
            let rg = &mut self.row_groups[current_rg];
            let space = (self.row_group_size - rg.row_count) as usize;
            let take = std::cmp::min(space, remaining.len());

            // 按列追加，同时维护 MinMax 索引
            for (col_idx, col_chunk) in rg.columns.iter_mut().enumerate() {
                for row in &remaining[..take] {
                    let val = &row[col_idx];
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
                    col_chunk.values.push(val.clone());
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
                            values: Vec::new(),
                            null_count: 0,
                            compression: CompressionType::Uncompressed,
                            compressed_data: Vec::new(),
                            uncompressed_count: 0,
                            min_value: None,
                            max_value: None,
                        })
                        .collect(),
                });
                self.row_groups.len() - 1
            };

            self.ensure_rg_decompressed(current_rg)?;
            let rg = &mut self.row_groups[current_rg];
            let space = (self.row_group_size - rg.row_count) as usize;
            let take = std::cmp::min(space, remaining_rows);

            // 按列直接追加（P4 核心：无需转置）
            for (col_idx, col_chunk) in rg.columns.iter_mut().enumerate() {
                if col_idx < columns.len() {
                    let src_col = &columns[col_idx];
                    col_chunk.values.extend_from_slice(&src_col[offset..offset + take]);

                    // 更新 MinMax 和 null_count
                    for val in &src_col[offset..offset + take] {
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
                } else {
                    // 列数不足，补 NULL
                    for _ in 0..take {
                        col_chunk.values.push(Value::Null);
                        col_chunk.null_count += 1;
                    }
                }
            }

            rg.row_count += take as u32;
            offset += take;
            remaining_rows -= take;
        }

        Ok(())
    }

    /// 确保指定 RowGroup 的所有列处于解压态（values 非空）
    ///
    /// 追加数据到已压缩的 RowGroup 前必须调用：压缩态下 `values` 为空，
    /// 直接 `extend` 会丢失原有数据。解压后清空 `compressed_data`，后续追加正常写入 `values`。
    fn ensure_rg_decompressed(&mut self, rg_idx: usize) -> Result<()> {
        let rg = &mut self.row_groups[rg_idx];
        for col in &mut rg.columns {
            if col.values.is_empty() && !col.compressed_data.is_empty() {
                let bytes = compression::decompress(&col.compressed_data, col.compression.clone(), &col.data_type)?;
                col.values = deserialize_values(&bytes, &col.data_type, col.uncompressed_count as usize);
                col.compressed_data.clear();
                col.compressed_data.shrink_to_fit();
                col.compression = CompressionType::Uncompressed;
            }
        }
        Ok(())
    }

    /// 读取指定 row group 的指定列
    ///
    /// 如果列数据已压缩，会自动解压到 values 中（惰性解压）。
    pub fn read_column(&mut self, row_group_idx: usize, col_idx: usize) -> Result<&[Value]> {
        let rg = &mut self.row_groups[row_group_idx];
        let col = &mut rg.columns[col_idx];

        // 惰性解压：如果数据是压缩状态，先解压
        if !col.compressed_data.is_empty() && col.values.is_empty() {
            let bytes = compression::decompress(&col.compressed_data, col.compression.clone(), &col.data_type)?;
            col.values = deserialize_values(&bytes, &col.data_type, col.uncompressed_count as usize);
            // 清空压缩态，避免后续 append / data_to_bytes 误用陈旧的 compressed_data
            col.compressed_data.clear();
            col.compressed_data.shrink_to_fit();
            col.compression = CompressionType::Uncompressed;
        }

        Ok(&col.values)
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
    /// 压缩后 values 被清空，数据存储在 compressed_data 中。
    /// 读取时通过 read_column 自动解压。
    pub fn compress_all(&mut self) -> Result<CompressionStats> {
        let mut stats = CompressionStats::default();

        for rg in &mut self.row_groups {
            for col in &mut rg.columns {
                if col.values.is_empty() && !col.compressed_data.is_empty() {
                    continue; // 已经压缩过
                }

                let row_count = col.values.len();
                let original_size = values_byte_size(&col.values, &col.data_type);

                // 将 Value 列转为字节序列
                let bytes = values_to_bytes(&col.values, &col.data_type);

                // 调用压缩模块（自动选择最优算法）
                let (ctype, compressed) = compression::compress(&bytes, &col.data_type)?;

                stats.total_original += original_size;
                stats.total_compressed += compressed.len();
                stats.columns_compressed += 1;

                // 更新列状态
                col.compression = ctype;
                col.compressed_data = compressed;
                col.uncompressed_count = row_count as u32;
                col.values.clear(); // 释放未压缩数据
                col.values.shrink_to_fit();
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
                col.values = bytes_to_values(&bytes, &col.data_type, col.uncompressed_count as usize);
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
                    stats.total_original += values_byte_size(&col.values, &col.data_type);
                    stats.total_compressed += values_byte_size(&col.values, &col.data_type);
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
                        let serialized = serialize_values(&col.values, &col.data_type);
                        let count = col.values.len() as u32;
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
                // - Uncompressed → 裸序列化字节，直接解出 values
                // - 其它 → 压缩字节，惰性存入 compressed_data，由 read_column 首次访问时解压
                let (values, compressed_data): (Vec<Value>, Vec<u8>) =
                    if compression == CompressionType::Uncompressed {
                        (deserialize_values(payload, &data_type, uncompressed_count as usize), Vec::new())
                    } else {
                        (Vec::new(), payload.to_vec())
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
                    values,
                    null_count,
                    compression,
                    compressed_data,
                    uncompressed_count,
                    min_value,
                    max_value,
                });
            }

            self.row_groups.push(RowGroup { row_count, columns });
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

    /// 获取指定 row group 和列的 min/max 值
    pub fn get_min_max(&self, row_group_idx: usize, col_idx: usize) -> (Option<&Value>, Option<&Value>) {
        self.row_groups
            .get(row_group_idx)
            .and_then(|rg| rg.columns.get(col_idx))
            .map(|col| (col.min_value.as_ref(), col.max_value.as_ref()))
            .unwrap_or((None, None))
    }
}

// ============================================================================
// 辅助函数：Value 比较（用于 MinMax 索引）
// ============================================================================

fn value_less(a: &Value, b: &Value) -> bool {
    use Value::*;
    match (a, b) {
        (Int32(x), Int32(y)) => x < y,
        (Int64(x), Int64(y)) => x < y,
        (Int32(x), Int64(y)) => (*x as i64) < *y,
        (Int64(x), Int32(y)) => *x < (*y as i64),
        (Float64(x), Float64(y)) => x < y,
        (Varchar(x), Varchar(y)) => x < y,
        (Boolean(x), Boolean(y)) => x < y,
        (Null, _) => true,
        (_, Null) => false,
        _ => false,
    }
}

fn value_greater(a: &Value, b: &Value) -> bool {
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
