//! Delta 编码（增量编码）
//!
//! 对有序/近似有序的数值列，存储相邻元素的差值而非原值。
//! 差值通常远小于原值，配合 FOR/Bit-packing 可进一步压缩。
//!
//! ClickHouse 对应：Delta codec（Delta(delta_bytes, true)）
//! 时间戳列效果尤佳：连续时间戳差值常为常数（如 1s、1min）。

/// Delta 编码 i64 序列
///
/// 格式：[first_value: 8 bytes][deltas: varint...]
/// 第一个值直接存储，后续存储与前一个值的差。
pub fn encode_i64(values: &[i64]) -> Vec<u8> {
    if values.is_empty() {
        return Vec::new();
    }

    let mut result = Vec::with_capacity(values.len() * 4);

    // 第一个值直接存
    result.extend_from_slice(&values[0].to_le_bytes());

    // 后续存差值（varint 编码）
    for i in 1..values.len() {
        let delta = values[i] - values[i - 1];
        write_varint(&mut result, delta);
    }

    result
}

/// Delta 解码 i64 序列
pub fn decode_i64(data: &[u8]) -> Option<Vec<i64>> {
    if data.is_empty() {
        return Some(Vec::new());
    }
    if data.len() < 8 {
        return None;
    }

    let first = i64::from_le_bytes(data[0..8].try_into().unwrap());
    let mut result = vec![first];
    let mut offset = 8;
    let mut prev = first;

    while offset < data.len() {
        let (delta, consumed) = read_varint(&data[offset..])?;
        let value = prev + delta;
        result.push(value);
        prev = value;
        offset += consumed;
    }

    Some(result)
}

/// Delta 编码 i32 序列
pub fn encode_i32(values: &[i32]) -> Vec<u8> {
    let i64_values: Vec<i64> = values.iter().map(|&v| v as i64).collect();
    encode_i64(&i64_values)
}

/// Delta 解码 i32 序列
pub fn decode_i32(data: &[u8]) -> Option<Vec<i32>> {
    let i64_values = decode_i64(data)?;
    Some(i64_values.into_iter().map(|v| v as i32).collect())
}

// ============================================================================
// Varint（变长整数）编码
// ============================================================================

/// ZigZag 编码：有符号 → 无符号
/// -1 → 1, 1 → 2, -2 → 3, 2 → 4, ...
/// 让小的负数也能有紧凑的 varint 表示
#[inline]
fn zigzag_encode(n: i64) -> u64 {
    ((n << 1) ^ (n >> 63)) as u64
}

/// ZigZag 解码
#[inline]
fn zigzag_decode(n: u64) -> i64 {
    ((n >> 1) as i64) ^ -((n & 1) as i64)
}

/// 写入 varint（ZigZag + LEB128）
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

/// 读取 varint，返回 (value, bytes_consumed)
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
            return None; // 溢出
        }
    }

    None // 不完整
}

// ============================================================================
// 测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty() {
        let encoded = encode_i64(&[]);
        assert!(encoded.is_empty());
        let decoded = decode_i64(&encoded).unwrap();
        assert!(decoded.is_empty());
    }

    #[test]
    fn test_single_value() {
        let values = vec![42i64];
        let encoded = encode_i64(&values);
        assert_eq!(encoded.len(), 8); // 只有第一个值
        let decoded = decode_i64(&encoded).unwrap();
        assert_eq!(decoded, values);
    }

    #[test]
    fn test_constant_sequence() {
        // 全部相同，delta = 0，压缩率极高
        let values: Vec<i64> = vec![100; 100];
        let encoded = encode_i64(&values);
        // 8 bytes (first) + 99 * 1 byte (delta=0) = 107 bytes
        assert!(encoded.len() < values.len() * 8);
        let decoded = decode_i64(&encoded).unwrap();
        assert_eq!(decoded, values);
    }

    #[test]
    fn test_increasing_by_one() {
        // 每次 +1，delta = 1
        let values: Vec<i64> = (0..100).collect();
        let encoded = encode_i64(&values);
        assert!(encoded.len() < values.len() * 8);
        let decoded = decode_i64(&encoded).unwrap();
        assert_eq!(decoded, values);
    }

    #[test]
    fn test_random_values() {
        // 随机值，delta 可能大
        let values: Vec<i64> = vec![100, 200, 150, 300, 250, 400, 350, 500];
        let encoded = encode_i64(&values);
        let decoded = decode_i64(&encoded).unwrap();
        assert_eq!(decoded, values);
    }

    #[test]
    fn test_negative_deltas() {
        // 递减序列
        let values: Vec<i64> = (0..100).rev().collect();
        let encoded = encode_i64(&values);
        let decoded = decode_i64(&encoded).unwrap();
        assert_eq!(decoded, values);
    }

    #[test]
    fn test_large_jumps() {
        // 大跨度
        let values: Vec<i64> = vec![0, 1_000_000, 2_000_000, 3_000_000];
        let encoded = encode_i64(&values);
        let decoded = decode_i64(&encoded).unwrap();
        assert_eq!(decoded, values);
    }

    #[test]
    fn test_negative_values() {
        let values: Vec<i64> = vec![-100, -50, 0, 50, 100];
        let encoded = encode_i64(&values);
        let decoded = decode_i64(&encoded).unwrap();
        assert_eq!(decoded, values);
    }

    #[test]
    fn test_i32_roundtrip() {
        let values: Vec<i32> = vec![1, 2, 3, 4, 5, 4, 3, 2, 1];
        let encoded = encode_i32(&values);
        let decoded = decode_i32(&encoded).unwrap();
        assert_eq!(decoded, values);
    }

    #[test]
    fn test_i32_large() {
        let values: Vec<i32> = (0..1000).collect();
        let encoded = encode_i32(&values);
        // 1000 个 i32 = 4000 bytes，delta 编码后应该小很多
        assert!(encoded.len() < 2000);
        let decoded = decode_i32(&encoded).unwrap();
        assert_eq!(decoded, values);
    }

    #[test]
    fn test_zigzag_roundtrip() {
        for &v in &[0i64, -1, 1, -2, 2, -63, 63, -64, 64, -127, 127, i64::MIN, i64::MAX] {
            let encoded = zigzag_encode(v);
            let decoded = zigzag_decode(encoded);
            assert_eq!(decoded, v, "failed for {}", v);
        }
    }

    #[test]
    fn test_varint_roundtrip() {
        let values: Vec<i64> = vec![0, 1, -1, 63, -63, 64, -64, 127, -127, 128, -128,
                                   1000, -1000, 100000, -100000, i64::MAX, i64::MIN];
        for &v in &values {
            let mut buf = Vec::new();
            write_varint(&mut buf, v);
            let (decoded, consumed) = read_varint(&buf).unwrap();
            assert_eq!(decoded, v, "failed for {} (buf len={}, consumed={})", v, buf.len(), consumed);
            assert_eq!(consumed, buf.len());
        }
    }

    #[test]
    fn test_varint_small_values_are_compact() {
        let mut buf = Vec::new();
        write_varint(&mut buf, 0);
        assert_eq!(buf.len(), 1);

        buf.clear();
        write_varint(&mut buf, 63);
        assert_eq!(buf.len(), 1);

        buf.clear();
        write_varint(&mut buf, 64);
        assert_eq!(buf.len(), 2);
    }

    #[test]
    fn test_truncated_varint() {
        // 只有一个字节但最高位为 1（不完整）
        assert!(read_varint(&[0x80]).is_none());
    }

    #[test]
    fn test_timestamp_like_data() {
        // 模拟时间戳：每秒一个，连续 1000 个
        let values: Vec<i64> = (0..1000).map(|i| 1_700_000_000 + i).collect();
        let encoded = encode_i64(&values);
        let original_size = values.len() * 8; // 8000 bytes
        // delta=1 的 varint 只占 1 字节，加上第一个 8 字节 ≈ 1007 bytes
        assert!(encoded.len() < original_size / 4, "compression not good enough: {} vs {}", encoded.len(), original_size);
        let decoded = decode_i64(&encoded).unwrap();
        assert_eq!(decoded, values);
    }
}
