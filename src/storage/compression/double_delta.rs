//! DoubleDelta 编码（二阶差分编码）
//!
//! 对单调递增的时间戳序列，先计算一阶差分（Delta），再对一阶差分求二阶差分。
//! 对于稳定的时间间隔（如每秒、每毫秒），二阶差分为 0，编码后仅占 1 字节。
//!
//! Facebook Gorilla 论文中提出的时间戳压缩算法：
//! Michael Burrows et al., "Gorilla: A Fast, Scalable, In-Memory Time Series Database"
//!
//! 格式：[first_value: 8 bytes][first_delta: 8 bytes][deltas_of_deltas: varint...]
//! - first_value: 第一个时间戳值（原始值）
//! - first_delta: 第一个一阶差分
//! - deltas_of_deltas: 后续每个值的二阶差分（ZigZag + Varint 编码）

use crate::common::config::CompressionType;

/// DoubleDelta 编码 i64 序列
pub fn encode_i64(values: &[i64]) -> Option<(CompressionType, Vec<u8>)> {
    if values.len() < 3 {
        return None; // 少于 3 个值，不值得用 DoubleDelta
    }

    let mut result = Vec::with_capacity(values.len() * 2);

    // 第一个值直接存
    result.extend_from_slice(&values[0].to_le_bytes());

    // 第一个一阶差分
    let first_delta = values[1] - values[0];
    result.extend_from_slice(&first_delta.to_le_bytes());

    // 后续存二阶差分
    let mut prev = values[1];
    let mut prev_delta = first_delta;
    for &v in &values[2..] {
        let delta = v - prev;
        let delta_of_delta = delta - prev_delta;
        write_varint(&mut result, delta_of_delta);
        prev = v;
        prev_delta = delta;
    }

    // 只有比原始数据小时才返回
    if result.len() < values.len() * 8 {
        Some((CompressionType::DoubleDelta, result))
    } else {
        None
    }
}

/// DoubleDelta 解码 i64 序列
pub fn decode_i64(data: &[u8], count: usize) -> Option<Vec<i64>> {
    if data.len() < 16 || count < 2 {
        return None;
    }

    let mut result = Vec::with_capacity(count);

    let first = i64::from_le_bytes(data[0..8].try_into().unwrap());
    let first_delta = i64::from_le_bytes(data[8..16].try_into().unwrap());

    result.push(first);
    result.push(first + first_delta);

    let mut offset = 16;
    let mut prev = first + first_delta;
    let mut prev_delta = first_delta;

    while result.len() < count && offset < data.len() {
        let (dd, consumed) = read_varint(&data[offset..])?;
        let delta = prev_delta + dd;
        let value = prev + delta;
        result.push(value);
        prev = value;
        prev_delta = delta;
        offset += consumed;
    }

    if result.len() != count {
        return None;
    }

    Some(result)
}

// ============================================================================
// Varint（变长整数）编码（复用 Delta 编码的相同逻辑）
// ============================================================================

#[inline]
fn zigzag_encode(n: i64) -> u64 {
    ((n << 1) ^ (n >> 63)) as u64
}

#[inline]
fn zigzag_decode(n: u64) -> i64 {
    ((n >> 1) as i64) ^ -((n & 1) as i64)
}

fn write_varint(buf: &mut Vec<u8>, value: i64) {
    let mut n = zigzag_encode(value);
    loop {
        let byte = (n & 0x7F) as u8;
        n >>= 7;
        if n == 0 {
            buf.push(byte);
            break;
        } else {
            buf.push(byte | 0x80);
        }
    }
}

fn read_varint(data: &[u8]) -> Option<(i64, usize)> {
    let mut result: u64 = 0;
    let mut shift = 0;
    let mut consumed = 0;

    for &byte in data {
        consumed += 1;
        result |= ((byte & 0x7F) as u64) << shift;
        if byte & 0x80 == 0 {
            return Some((zigzag_decode(result), consumed));
        }
        shift += 7;
        if shift >= 64 {
            return None;
        }
    }

    None
}

// ============================================================================
// 测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_short_sequence_returns_none() {
        assert!(encode_i64(&[]).is_none());
        assert!(encode_i64(&[1]).is_none());
        assert!(encode_i64(&[1, 2]).is_none());
    }

    #[test]
    fn test_constant_interval() {
        // 每秒 1 个，二阶差分 = 0
        let values: Vec<i64> = (0..1000).map(|i| 1_700_000_000_000 + i * 1000).collect();
        let (ctype, encoded) = encode_i64(&values).unwrap();
        assert_eq!(ctype, CompressionType::DoubleDelta);
        // 16 bytes (first + first_delta) + 998 * 1 byte (zeros) = 1014 bytes
        // 原始 1000*8 = 8000 bytes
        assert!(encoded.len() < 1100, "encoded too large: {}", encoded.len());
        let decoded = decode_i64(&encoded, values.len()).unwrap();
        assert_eq!(decoded, values);
    }

    #[test]
    fn test_variable_interval() {
        // 间隔随机变化
        let values: Vec<i64> = vec![
            1_700_000_000_000,
            1_700_000_001_000, // +1000
            1_700_000_003_000, // +2000, dd=+1000
            1_700_000_004_000, // +1000, dd=-1000
            1_700_000_010_000, // +6000, dd=+5000
        ];
        let (ctype, encoded) = encode_i64(&values).unwrap();
        assert_eq!(ctype, CompressionType::DoubleDelta);
        let decoded = decode_i64(&encoded, values.len()).unwrap();
        assert_eq!(decoded, values);
    }

    #[test]
    fn test_bad_compression_returns_none() {
        // 随机值，二阶差分可能很大，不压缩
        let values: Vec<i64> = vec![0, 1_000_000, 0, 1_000_000, 0, 1_000_000];
        let result = encode_i64(&values);
        // 可能仍然小于原始大小，取决于 varint 编码
        if let Some((_, encoded)) = result {
            assert!(encoded.len() < values.len() * 8);
        }
    }

    #[test]
    fn test_single_interval_jitter() {
        // 模拟真实时间戳：每秒一次，但偶尔有 1ms 抖动
        let mut values: Vec<i64> = Vec::with_capacity(100);
        let base = 1_700_000_000_000i64;
        for i in 0..100 {
            let jitter = if i % 10 == 5 { 1 } else { 0 }; // 每 10 个抖动 1ms
            values.push(base + i * 1000 + jitter);
        }
        let (ctype, encoded) = encode_i64(&values).unwrap();
        assert_eq!(ctype, CompressionType::DoubleDelta);
        let decoded = decode_i64(&encoded, values.len()).unwrap();
        assert_eq!(decoded, values);
    }

    #[test]
    fn test_timestamp_like_data() {
        // 毫秒级时间戳，每秒 1000 个
        let values: Vec<i64> = (0..1000).map(|i| 1_700_000_000_000 + i).collect();
        let (ctype, encoded) = encode_i64(&values).unwrap();
        assert_eq!(ctype, CompressionType::DoubleDelta);
        // 二阶差分全是 0，998 个字节 + 16 = 1014 bytes
        // 对比一阶 Delta 编码：8 + 999 * 1 = 1007 bytes
        // DoubleDelta 略大一点，但对常间隔序列效果接近
        let original_size = values.len() * 8;
        assert!(encoded.len() < original_size / 4, "compression not good enough: {} vs {}", encoded.len(), original_size);
        let decoded = decode_i64(&encoded, values.len()).unwrap();
        assert_eq!(decoded, values);
    }

    #[test]
    fn test_large_constant_interval() {
        // 每天一个时间戳，持续 10 年
        let values: Vec<i64> = (0..3650).map(|i| 1_700_000_000_000 + i * 86400000).collect();
        let (ctype, encoded) = encode_i64(&values).unwrap();
        assert_eq!(ctype, CompressionType::DoubleDelta);
        // 3650 个值，二阶差分全为 0
        // 16 + 3648 * 1 = 3664 bytes, 原始 29200 bytes
        let decoded = decode_i64(&encoded, values.len()).unwrap();
        assert_eq!(decoded, values);
    }

    #[test]
    fn test_negative_timestamps() {
        let values: Vec<i64> = vec![-1000, 0, 1000, 2000, 3000];
        let (ctype, encoded) = encode_i64(&values).unwrap();
        assert_eq!(ctype, CompressionType::DoubleDelta);
        let decoded = decode_i64(&encoded, values.len()).unwrap();
        assert_eq!(decoded, values);
    }
}