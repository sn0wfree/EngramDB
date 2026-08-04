//! 压缩算法模块 —— ClickHouse 风格的分字段类型压缩策略
//!
//! 借鉴 ClickHouse 的 codec 设计思想：按列的数据类型自动选择最优压缩算法，
//! 而非对所有列使用统一算法。每种类型尝试多种 codec，选择压缩率最高的。
//!
//! 策略映射（ClickHouse 对照）：
//! - Boolean  → BooleanPack (1 bit/value，类似 ClickHouse 的位打包)
//! - Int32/64 → Delta + FOR + Bit-packing + RLE 组合择优（类似 CODEC_Delta + CODEC_For）
//! - Float64  → Gorilla XOR（类似 CODEC_Gorilla，源自 Facebook Gorilla 论文）
//! - Varchar  → Dictionary（低基数时，类似 CODEC_Dictionary）

pub mod rle;
pub mod bitpacking;
pub mod dictionary;
pub mod for_encoding;
pub mod delta;
pub mod gorilla;
pub mod double_delta;

use crate::common::config::CompressionType;
use crate::common::error::Result;
use crate::common::types::DataType;

// ============================================================================
// 顶层 API：compress / decompress
// ============================================================================

/// 压缩一列数据（ClickHouse 风格：按类型自动选择最优 codec）
///
/// 对每种数据类型尝试多种 codec，选择压缩后体积最小的方案。
/// 返回 (选用的压缩类型, 压缩后数据)。
pub fn compress(data: &[u8], data_type: &DataType) -> Result<(CompressionType, Vec<u8>)> {
    if data.is_empty() {
        return Ok((CompressionType::Uncompressed, Vec::new()));
    }

    match data_type {
        DataType::Boolean => compress_boolean(data),
        DataType::Int32 => compress_integer::<i32>(data),
        DataType::Int64 => compress_integer::<i64>(data),
        DataType::Float32 => compress_float32(data),
        DataType::Float64 => compress_float64(data),
        DataType::Timestamp => compress_timestamp(data),
        DataType::Varchar => compress_varchar(data),
        // JSON 和 Vector 暂不压缩，直接存储
        DataType::Json | DataType::Vector { .. } | DataType::VectorInt8 { .. } | DataType::Blob => {
            Ok((CompressionType::Uncompressed, data.to_vec()))
        }
    }
}

/// 解压一列数据
///
/// `data_type` 用于消歧整数列的宽度（Int32 vs Int64）——
/// Delta / ForBitPack 编码内部统一用 i64，解压时需按列真实类型输出字节，
/// 否则 `deserialize_values` 会按错误步长读取，导致数据错位。
pub fn decompress(data: &[u8], compression_type: CompressionType, data_type: &DataType) -> Result<Vec<u8>> {
    match compression_type {
        CompressionType::Uncompressed => Ok(data.to_vec()),
        CompressionType::Rle => Ok(rle::decode(data)),
        CompressionType::BitPacking => {
            // 通用 bit-packing 暂不支持直接解压（需配合 count）
            // 实际使用通过 ForBitPack / BooleanPack 等组合类型
            Ok(data.to_vec())
        }
        CompressionType::Dictionary => decompress_dictionary(data),
        CompressionType::For => {
            // FOR 编码需配合 bit-packing 使用，走 ForBitPack 路径
            Ok(data.to_vec())
        }
        CompressionType::Delta => decompress_delta(data, data_type),
        CompressionType::Zstd => Ok(data.to_vec()),
        CompressionType::Gorilla => decompress_gorilla(data),
        CompressionType::ForBitPack => decompress_for_bitpack(data, data_type),
        CompressionType::BooleanPack => decompress_boolean_pack(data),
        CompressionType::DoubleDelta => decompress_double_delta(data, data_type),
    }
}

// ============================================================================
// Boolean 列压缩：位打包（64 个布尔 → 8 字节）
// ============================================================================

fn compress_boolean(data: &[u8]) -> Result<(CompressionType, Vec<u8>)> {
    let count = data.len();
    let packed = boolean_pack(data);
    // 压缩后大小 = 4 (count) + ceil(count/8)
    let original = count;
    if packed.len() < original {
        Ok((CompressionType::BooleanPack, packed))
    } else {
        Ok((CompressionType::Uncompressed, data.to_vec()))
    }
}

fn boolean_pack(data: &[u8]) -> Vec<u8> {
    let count = data.len();
    let num_bytes = (count + 7) / 8;
    let mut result = Vec::with_capacity(4 + num_bytes);
    result.extend_from_slice(&(count as u32).to_le_bytes());

    for i in 0..num_bytes {
        let mut byte: u8 = 0;
        for bit in 0..8 {
            let idx = i * 8 + bit;
            if idx < count && data[idx] != 0 {
                byte |= 1 << (7 - bit);
            }
        }
        result.push(byte);
    }

    result
}

fn decompress_boolean_pack(data: &[u8]) -> Result<Vec<u8>> {
    if data.len() < 4 {
        return Ok(data.to_vec());
    }
    let count = u32::from_le_bytes(data[0..4].try_into().unwrap()) as usize;
    let mut result = Vec::with_capacity(count);

    for i in 0..count {
        let byte_idx = 4 + i / 8;
        let bit_idx = 7 - (i % 8);
        let val = if byte_idx < data.len() && (data[byte_idx] >> bit_idx) & 1 != 0 {
            1u8
        } else {
            0u8
        };
        result.push(val);
    }

    Ok(result)
}

// ============================================================================
// 整数列压缩：Delta / FOR+Bit-packing / RLE 择优
// ============================================================================

trait IntegerCodec: Sized {
    fn fixed_size() -> usize;
    fn from_le_bytes(bytes: &[u8]) -> Self;
    fn to_le_bytes_vec(values: &[Self]) -> Vec<u8>;
    fn try_delta(data: &[u8]) -> Option<(CompressionType, Vec<u8>)>;
    fn try_for_bitpack(data: &[u8]) -> Option<(CompressionType, Vec<u8>)>;
}

impl IntegerCodec for i32 {
    fn fixed_size() -> usize { 4 }

    fn from_le_bytes(bytes: &[u8]) -> Self {
        i32::from_le_bytes(bytes[..4].try_into().unwrap())
    }

    fn to_le_bytes_vec(values: &[Self]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    fn try_delta(data: &[u8]) -> Option<(CompressionType, Vec<u8>)> {
        let values = bytes_to_i32(data)?;
        let encoded = delta::encode_i32(&values);
        if encoded.len() < data.len() {
            Some((CompressionType::Delta, encoded))
        } else {
            None
        }
    }

    fn try_for_bitpack(data: &[u8]) -> Option<(CompressionType, Vec<u8>)> {
        let values = bytes_to_i32(data)?;
        // 转为 u64：加上偏移使所有值非负
        let min_val = values.iter().min().copied().unwrap_or(0);
        let uvalues: Vec<u64> = values.iter().map(|&v| (v as i64 - min_val as i64) as u64).collect();
        let encoded = for_encoding::encode_for_bitpack(&uvalues);
        // 额外存 min_val（i32，4 字节）在开头
        let mut result = Vec::with_capacity(4 + encoded.len());
        result.extend_from_slice(&min_val.to_le_bytes());
        result.extend_from_slice(&encoded);
        if result.len() < data.len() {
            Some((CompressionType::ForBitPack, result))
        } else {
            None
        }
    }
}

impl IntegerCodec for i64 {
    fn fixed_size() -> usize { 8 }

    fn from_le_bytes(bytes: &[u8]) -> Self {
        i64::from_le_bytes(bytes[..8].try_into().unwrap())
    }

    fn to_le_bytes_vec(values: &[Self]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    fn try_delta(data: &[u8]) -> Option<(CompressionType, Vec<u8>)> {
        let values = bytes_to_i64(data)?;
        let encoded = delta::encode_i64(&values);
        if encoded.len() < data.len() {
            Some((CompressionType::Delta, encoded))
        } else {
            None
        }
    }

    fn try_for_bitpack(data: &[u8]) -> Option<(CompressionType, Vec<u8>)> {
        let values = bytes_to_i64(data)?;
        let min_val = values.iter().min().copied().unwrap_or(0);
        let uvalues: Vec<u64> = values.iter().map(|&v| (v as i128 - min_val as i128) as u64).collect();
        let encoded = for_encoding::encode_for_bitpack(&uvalues);
        let mut result = Vec::with_capacity(8 + encoded.len());
        result.extend_from_slice(&min_val.to_le_bytes());
        result.extend_from_slice(&encoded);
        if result.len() < data.len() {
            Some((CompressionType::ForBitPack, result))
        } else {
            None
        }
    }
}

fn compress_integer<T: IntegerCodec>(data: &[u8]) -> Result<(CompressionType, Vec<u8>)> {
    let mut best = (CompressionType::Uncompressed, data.to_vec());
    let mut best_size = data.len();

    // 尝试 Delta
    if let Some((ctype, encoded)) = T::try_delta(data) {
        if encoded.len() < best_size {
            best_size = encoded.len();
            best = (ctype, encoded);
        }
    }

    // 尝试 FOR + Bit-packing
    if let Some((ctype, encoded)) = T::try_for_bitpack(data) {
        if encoded.len() < best_size {
            best_size = encoded.len();
            best = (ctype, encoded);
        }
    }

    // 尝试 RLE
    let rle_result = rle::encode(data);
    if rle_result.len() < best_size {
        best = (CompressionType::Rle, rle_result);
    }

    Ok(best)
}

/// Timestamp 专用压缩：先试 DoubleDelta，再回退到通用整数压缩
fn compress_timestamp(data: &[u8]) -> Result<(CompressionType, Vec<u8>)> {
    if let Some(values) = bytes_to_i64(data) {
        if let Some((ctype, encoded)) = double_delta::encode_i64(&values) {
            if encoded.len() < data.len() {
                return Ok((ctype, encoded));
            }
        }
    }
    compress_integer::<i64>(data)
}

fn decompress_double_delta(data: &[u8], data_type: &DataType) -> Result<Vec<u8>> {
    match data_type {
        DataType::Timestamp | DataType::Int64 => {
            let max_count = data.len() + 2;
            let values = double_delta::decode_i64(data, max_count)
                .ok_or_else(|| crate::common::error::EngramDbError::Parse("DoubleDelta decompress failed".into()))?;
            let result: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
            Ok(result)
        }
        _ => Ok(data.to_vec()),
    }
}

fn bytes_to_i32(data: &[u8]) -> Option<Vec<i32>> {
    if data.len() % 4 != 0 {
        return None;
    }
    let mut result = Vec::with_capacity(data.len() / 4);
    for chunk in data.chunks_exact(4) {
        result.push(i32::from_le_bytes(chunk.try_into().unwrap()));
    }
    Some(result)
}

fn bytes_to_i64(data: &[u8]) -> Option<Vec<i64>> {
    if data.len() % 8 != 0 {
        return None;
    }
    let mut result = Vec::with_capacity(data.len() / 8);
    for chunk in data.chunks_exact(8) {
        result.push(i64::from_le_bytes(chunk.try_into().unwrap()));
    }
    Some(result)
}

fn bytes_to_f64(data: &[u8]) -> Option<Vec<f64>> {
    if data.len() % 8 != 0 {
        return None;
    }
    let mut result = Vec::with_capacity(data.len() / 8);
    for chunk in data.chunks_exact(8) {
        result.push(f64::from_le_bytes(chunk.try_into().unwrap()));
    }
    Some(result)
}

// ============================================================================
// Delta 解压（整数通用）
// ============================================================================

fn decompress_delta(data: &[u8], data_type: &DataType) -> Result<Vec<u8>> {
    // delta::encode_i32 内部转 i64 编码，decode_i64 可正确解码两者。
    // 关键：输出字节宽度必须匹配列真实类型，否则 deserialize_values 步长错位。
    if let Some(values) = delta::decode_i64(data) {
        match data_type {
            DataType::Int32 => {
                // Int32 列：每值 4 字节
                Ok(values.iter().flat_map(|v| (*v as i32).to_le_bytes()).collect())
            }
            // Int64（及其它整数宽度的兜底）：每值 8 字节
            _ => Ok(values.iter().flat_map(|v| v.to_le_bytes()).collect()),
        }
    } else {
        Ok(data.to_vec())
    }
}

// ============================================================================
// FOR + Bit-packing 解压
// ============================================================================

fn decompress_for_bitpack(data: &[u8], data_type: &DataType) -> Result<Vec<u8>> {
    // 编码格式因类型而异：
    //   i32 列：[min_val: 4 bytes (i32 LE)][for_bitpack data...]
    //   i64 列：[min_val: 8 bytes (i64 LE)][for_bitpack data...]
    // for_bitpack data 内部：[base: 8B][count: 4B][bit_width: 1B][packed...]
    match data_type {
        DataType::Int32 => {
            if data.len() < 4 {
                return Ok(data.to_vec());
            }
            let min_val = i32::from_le_bytes(data[0..4].try_into().unwrap());
            let uvalues = for_encoding::decode_for_bitpack(&data[4..]);
            if uvalues.is_empty() {
                return Ok(Vec::new());
            }
            let values: Vec<i32> = uvalues
                .iter()
                .map(|&u| (min_val as i64 + u as i64) as i32)
                .collect();
            Ok(values.iter().flat_map(|v| v.to_le_bytes()).collect())
        }
        _ => {
            if data.len() < 8 {
                return Ok(data.to_vec());
            }
            let min_val = i64::from_le_bytes(data[0..8].try_into().unwrap());
            let uvalues = for_encoding::decode_for_bitpack(&data[8..]);
            if uvalues.is_empty() {
                return Ok(Vec::new());
            }
            let values: Vec<i64> = uvalues
                .iter()
                .map(|&u| min_val.wrapping_add(u as i64))
                .collect();
            Ok(values.iter().flat_map(|v| v.to_le_bytes()).collect())
        }
    }
}

// ============================================================================
// Float64 列压缩：Gorilla XOR / RLE 择优
// ============================================================================

fn compress_float64(data: &[u8]) -> Result<(CompressionType, Vec<u8>)> {
    let mut best = (CompressionType::Uncompressed, data.to_vec());
    let mut best_size = data.len();

    // 尝试 Gorilla XOR
    if let Some(values) = bytes_to_f64(data) {
        let encoded = gorilla::encode_f64(&values);
        if encoded.len() < best_size {
            best_size = encoded.len();
            best = (CompressionType::Gorilla, encoded);
        }
    }

    // 尝试 RLE
    let rle_result = rle::encode(data);
    if rle_result.len() < best_size {
        best = (CompressionType::Rle, rle_result);
    }

    Ok(best)
}

fn compress_float32(data: &[u8]) -> Result<(CompressionType, Vec<u8>)> {
    // Float32 与 Float64 同构：4 字节模式，直接复用 RLE（不分位数）
    let mut best = (CompressionType::Uncompressed, data.to_vec());
    let mut best_size = data.len();
    let rle_result = rle::encode(data);
    if rle_result.len() < best_size {
        best = (CompressionType::Rle, rle_result);
    }
    Ok(best)
}

fn decompress_gorilla(data: &[u8]) -> Result<Vec<u8>> {
    if let Some(values) = gorilla::decode_f64(data) {
        Ok(values.iter().flat_map(|v| v.to_le_bytes()).collect())
    } else {
        Ok(data.to_vec())
    }
}

// ============================================================================
// Varchar 列压缩：Dictionary（低基数时）
// ============================================================================

fn compress_varchar(data: &[u8]) -> Result<(CompressionType, Vec<u8>)> {
    // Varchar 列数据格式：[len: 4 bytes][value bytes]...
    // 解析出所有字符串
    let strings = parse_varchar_column(data);

    if strings.is_empty() {
        return Ok((CompressionType::Uncompressed, data.to_vec()));
    }

    let string_slices: Vec<&[u8]> = strings.iter().map(|s| s.as_slice()).collect();
    let encoded = dictionary::encode(&string_slices);
    let ratio = dictionary::compression_ratio(&encoded, data.len());

    if ratio < 0.9 {
        // 压缩率超过 10% 才使用字典编码
        let serialized = serialize_dictionary(&encoded);
        Ok((CompressionType::Dictionary, serialized))
    } else {
        Ok((CompressionType::Uncompressed, data.to_vec()))
    }
}

fn parse_varchar_column(data: &[u8]) -> Vec<Vec<u8>> {
    let mut result = Vec::new();
    let mut offset = 0;
    while offset + 4 <= data.len() {
        let len = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
        offset += 4;
        if offset + len <= data.len() {
            result.push(data[offset..offset + len].to_vec());
            offset += len;
        } else {
            break;
        }
    }
    result
}

fn serialize_dictionary(encoded: &dictionary::DictionaryEncoded) -> Vec<u8> {
    let mut result = Vec::new();

    // 字典大小
    result.extend_from_slice(&(encoded.dictionary.len() as u32).to_le_bytes());
    // 每个字典条目：[len: 4 bytes][value bytes]
    for entry in &encoded.dictionary {
        result.extend_from_slice(&(entry.len() as u32).to_le_bytes());
        result.extend_from_slice(entry);
    }

    // 索引数量
    result.extend_from_slice(&(encoded.indices.len() as u32).to_le_bytes());
    // 索引数组
    for &idx in &encoded.indices {
        result.extend_from_slice(&idx.to_le_bytes());
    }

    result
}

/// 字典解码：反向 `serialize_dictionary`，重建 Varchar 列的原始字节序列
///
/// 输入格式（`serialize_dictionary` 产物）：
/// ```text
/// [dict_count: 4B]
/// for each entry: [len: 4B][value bytes]
/// [index_count: 4B]
/// for each index: [idx: 4B]
/// ```
///
/// 输出：Varchar 列的 `[len: 4B][value bytes]...` 序列（与 `serialize_values` 格式一致），
/// 供 `deserialize_values` 直接消费。
fn decompress_dictionary(data: &[u8]) -> Result<Vec<u8>> {
    if data.len() < 4 {
        return Ok(Vec::new());
    }
    let mut offset = 0;
    let dict_count = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
    offset += 4;

    let mut dictionary: Vec<&[u8]> = Vec::with_capacity(dict_count);
    for _ in 0..dict_count {
        if offset + 4 > data.len() {
            return Ok(data.to_vec());
        }
        let len = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
        offset += 4;
        if offset + len > data.len() {
            return Ok(data.to_vec());
        }
        dictionary.push(&data[offset..offset + len]);
        offset += len;
    }

    if offset + 4 > data.len() {
        return Ok(data.to_vec());
    }
    let index_count = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
    offset += 4;

    // 重建 Varchar 列格式：[len: 4B][value bytes]...
    let mut result = Vec::new();
    for _ in 0..index_count {
        if offset + 4 > data.len() {
            break;
        }
        let idx = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
        offset += 4;
        let entry = dictionary.get(idx).copied().unwrap_or(&[]);
        result.extend_from_slice(&(entry.len() as u32).to_le_bytes());
        result.extend_from_slice(entry);
    }

    Ok(result)
}

// ============================================================================
// 测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // --- Boolean 列 ---

    #[test]
    fn test_boolean_pack_roundtrip() {
        let data: Vec<u8> = vec![1, 0, 1, 1, 0, 0, 1, 0, 1, 1, 1];
        let packed = boolean_pack(&data);
        assert!(packed.len() < data.len());
        let unpacked = decompress_boolean_pack(&packed).unwrap();
        assert_eq!(unpacked, data);
    }

    #[test]
    fn test_boolean_compress_decompress() {
        let data: Vec<u8> = (0..100).map(|i| if i % 3 == 0 { 1 } else { 0 }).collect();
        let (ctype, compressed) = compress(&data, &DataType::Boolean).unwrap();
        assert_eq!(ctype, CompressionType::BooleanPack);
        assert!(compressed.len() < data.len());
        let decompressed = decompress(&compressed, ctype, &DataType::Boolean).unwrap();
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_boolean_all_true() {
        let data: Vec<u8> = vec![1; 64];
        let (ctype, compressed) = compress(&data, &DataType::Boolean).unwrap();
        assert_eq!(ctype, CompressionType::BooleanPack);
        // 4 (count) + 8 (64 bits) = 12 bytes vs 64 bytes original
        assert_eq!(compressed.len(), 12);
        let decompressed = decompress(&compressed, ctype, &DataType::Boolean).unwrap();
        assert_eq!(decompressed, data);
    }

    // --- Int64 列 ---

    #[test]
    fn test_int64_delta_compress() {
        // 连续递增的时间戳，Delta 效果最好
        let values: Vec<i64> = (0..1000).map(|i| 1_700_000_000 + i).collect();
        let data: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        let (ctype, compressed) = compress(&data, &DataType::Int64).unwrap();
        assert_eq!(ctype, CompressionType::Delta);
        assert!(compressed.len() < data.len() / 4, "压缩率不足: {} / {}", compressed.len(), data.len());
        let decompressed = decompress(&compressed, ctype, &DataType::Int64).unwrap();
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_int64_for_bitpack_compress() {
        // 范围较小的随机整数，FOR+Bit-packing 效果好
        let values: Vec<i64> = (0..100).map(|i| 1000 + (i % 50)).collect();
        let data: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        let (ctype, compressed) = compress(&data, &DataType::Int64).unwrap();
        // 应该选择 Delta 或 ForBitPack，取决于数据模式
        assert!(compressed.len() < data.len());
        let decompressed = decompress(&compressed, ctype, &DataType::Int64).unwrap();
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_int64_rle_compress() {
        // 大量相同值，RLE 效果好
        let values: Vec<i64> = vec![42; 100];
        let data: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        let (ctype, compressed) = compress(&data, &DataType::Int64).unwrap();
        assert!(compressed.len() < data.len());
        let decompressed = decompress(&compressed, ctype, &DataType::Int64).unwrap();
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_int64_random_uncompressible() {
        // 完全随机的大整数，可能不压缩
        let values: Vec<i64> = (0..10).map(|i| i * 1_000_000_000_000).collect();
        let data: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        let (ctype, compressed) = compress(&data, &DataType::Int64).unwrap();
        // 数据量小可能不压缩，但必须能正确 roundtrip
        let decompressed = decompress(&compressed, ctype, &DataType::Int64).unwrap();
        assert_eq!(decompressed, data);
    }

    // --- Int32 列 ---

    #[test]
    fn test_int32_delta_compress() {
        let values: Vec<i32> = (0..500).collect();
        let data: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        let (ctype, compressed) = compress(&data, &DataType::Int32).unwrap();
        assert!(compressed.len() < data.len());
        // v0.12.x 修复：decompress 现按 data_type 输出正确宽度（i32=4B），
        // 解压后字节与原始 data 直接相等，无需手动截断
        let decompressed = decompress(&compressed, ctype, &DataType::Int32).unwrap();
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_int32_for_bitpack_compress() {
        // 小范围 Int32，FOR+Bit-packing 效果好
        let values: Vec<i32> = (0..100).map(|i| 1000 + (i % 50)).collect();
        let data: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        let (ctype, compressed) = compress(&data, &DataType::Int32).unwrap();
        assert!(compressed.len() < data.len());
        let decompressed = decompress(&compressed, ctype, &DataType::Int32).unwrap();
        assert_eq!(decompressed, data);
    }

    // --- Float64 列 ---

    #[test]
    fn test_float64_gorilla_compress() {
        // 缓慢变化的浮点数据，Gorilla 效果好
        let mut values: Vec<f64> = Vec::new();
        let mut val: f64 = 100.0;
        for _ in 0..200 {
            values.push(val);
            val += 0.001;
        }
        let data: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        let (ctype, compressed) = compress(&data, &DataType::Float64).unwrap();
        assert_eq!(ctype, CompressionType::Gorilla);
        assert!(compressed.len() < data.len());
        let decompressed = decompress(&compressed, ctype, &DataType::Float64).unwrap();
        assert_eq!(decompressed.len(), data.len());
        for (a, b) in decompressed.chunks_exact(8).zip(data.chunks_exact(8)) {
            let fa = f64::from_le_bytes(a.try_into().unwrap());
            let fb = f64::from_le_bytes(b.try_into().unwrap());
            assert_eq!(fa.to_bits(), fb.to_bits());
        }
    }

    #[test]
    fn test_float64_all_same() {
        let values: Vec<f64> = vec![3.14; 100];
        let data: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        let (ctype, compressed) = compress(&data, &DataType::Float64).unwrap();
        // 全部相同的值，RLE 或 Gorilla 都可能被选中（取决于谁更小）
        assert!(compressed.len() < data.len() / 2);
        let decompressed = decompress(&compressed, ctype, &DataType::Float64).unwrap();
        assert_eq!(decompressed, data);
    }

    // --- Varchar 列 ---

    #[test]
    fn test_varchar_dictionary_compress() {
        // 低基数字符串（大量重复），字典编码效果好
        let mut data = Vec::new();
        let values = vec!["active", "inactive", "pending"];
        for i in 0..300 {
            let v = values[i % 3];
            let bytes = v.as_bytes();
            data.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            data.extend_from_slice(bytes);
        }
        let (ctype, compressed) = compress(&data, &DataType::Varchar).unwrap();
        assert_eq!(ctype, CompressionType::Dictionary);
        assert!(compressed.len() < data.len());
        // v0.12.x 修复：Dictionary 解压原为空实现，现重建 Varchar 列字节
        let decompressed = decompress(&compressed, ctype, &DataType::Varchar).unwrap();
        assert_eq!(decompressed, data);
    }

    #[test]
    fn test_varchar_high_cardinality() {
        // 高基数字符串，不压缩
        let mut data = Vec::new();
        for i in 0..10u32 {
            let s = format!("unique_value_{}", i);
            let bytes = s.as_bytes();
            data.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            data.extend_from_slice(bytes);
        }
        let (ctype, _compressed) = compress(&data, &DataType::Varchar).unwrap();
        // 高基数小数据量可能不压缩（或压缩），但必须不报错
        let _ = ctype;
    }

    // --- 空数据 ---

    #[test]
    fn test_empty_data() {
        for dt in &[DataType::Boolean, DataType::Int32, DataType::Int64,
                    DataType::Float64, DataType::Varchar] {
            let (ctype, compressed) = compress(&[], dt).unwrap();
            assert_eq!(ctype, CompressionType::Uncompressed);
            assert!(compressed.is_empty());
        }
    }

    // --- 综合 roundtrip 测试 ---

    #[test]
    fn test_all_types_roundtrip() {
        // Boolean
        let bool_data: Vec<u8> = (0..50).map(|i| (i % 2) as u8).collect();
        let (bc, bcomp) = compress(&bool_data, &DataType::Boolean).unwrap();
        let bdec = decompress(&bcomp, bc, &DataType::Boolean).unwrap();
        assert_eq!(bdec, bool_data);

        // Int64
        let i64_values: Vec<i64> = (0..100).map(|i| i * 1000).collect();
        let i64_data: Vec<u8> = i64_values.iter().flat_map(|v| v.to_le_bytes()).collect();
        let (ic, icomp) = compress(&i64_data, &DataType::Int64).unwrap();
        let idec = decompress(&icomp, ic, &DataType::Int64).unwrap();
        assert_eq!(idec, i64_data);

        // Int32
        let i32_values: Vec<i32> = (0..100).collect();
        let i32_data: Vec<u8> = i32_values.iter().flat_map(|v| v.to_le_bytes()).collect();
        let (i32c, i32comp) = compress(&i32_data, &DataType::Int32).unwrap();
        let i32dec = decompress(&i32comp, i32c, &DataType::Int32).unwrap();
        assert_eq!(i32dec, i32_data);

        // Float64
        let f64_values: Vec<f64> = (0..50).map(|i| i as f64 * 0.1).collect();
        let f64_data: Vec<u8> = f64_values.iter().flat_map(|v| v.to_le_bytes()).collect();
        let (fc, fcomp) = compress(&f64_data, &DataType::Float64).unwrap();
        let fdec = decompress(&fcomp, fc, &DataType::Float64).unwrap();
        assert_eq!(fdec.len(), f64_data.len());
        for (a, b) in fdec.chunks_exact(8).zip(f64_data.chunks_exact(8)) {
            assert_eq!(a, b);
        }

        // Varchar（低基数 → Dictionary）
        let mut vc_data = Vec::new();
        for i in 0..100 {
            let s: &[u8] = if i % 2 == 0 { b"yes" } else { b"no" };
            vc_data.extend_from_slice(&(s.len() as u32).to_le_bytes());
            vc_data.extend_from_slice(s);
        }
        let (vc, vcomp) = compress(&vc_data, &DataType::Varchar).unwrap();
        let vdec = decompress(&vcomp, vc, &DataType::Varchar).unwrap();
        assert_eq!(vdec, vc_data);
    }
}
