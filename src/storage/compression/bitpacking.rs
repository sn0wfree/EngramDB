//! 位打包压缩
//!
//! 适用于整数值域较小的列，用最小位数表示所有值

/// 计算表示值范围所需的最小位数
pub fn min_bit_width(max_value: u64) -> u8 {
    if max_value == 0 {
        return 1;
    }
    (64 - max_value.leading_zeros()) as u8
}

/// 对 u64 序列进行位打包
pub fn encode_u64(values: &[u64], bit_width: u8) -> Vec<u8> {
    if values.is_empty() || bit_width == 0 {
        return Vec::new();
    }

    let total_bits = values.len() * bit_width as usize;
    let total_bytes = (total_bits + 7) / 8;
    let mut result = vec![0u8; total_bytes + 1]; // +1 for bit_width header

    result[0] = bit_width;

    let mut bit_pos = 8; // 跳过第 1 字节（bit_width）
    for &val in values {
        for b in 0..bit_width {
            let byte_idx = bit_pos / 8;
            let bit_idx = bit_pos % 8;
            let bit = (val >> (bit_width as usize - 1 - b as usize)) & 1;
            result[byte_idx] |= (bit as u8) << (7 - bit_idx);
            bit_pos += 1;
        }
    }

    result
}

/// 位包解压为 u64 序列
pub fn decode_u64(data: &[u8], count: usize) -> Vec<u64> {
    if data.is_empty() || count == 0 {
        return Vec::new();
    }

    let bit_width = data[0];
    let mut result = Vec::with_capacity(count);
    let mut bit_pos = 8;

    for _ in 0..count {
        let mut val: u64 = 0;
        for b in 0..bit_width {
            let byte_idx = bit_pos / 8;
            let bit_idx = bit_pos % 8;
            if byte_idx < data.len() {
                let bit = (data[byte_idx] >> (7 - bit_idx)) & 1;
                val = (val << 1) | bit as u64;
            }
            bit_pos += 1;
        }
        result.push(val);
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bitpack_small_values() {
        let values: Vec<u64> = vec![1, 2, 3, 4, 5, 6, 7, 0];
        let bit_width = min_bit_width(7);
        assert_eq!(bit_width, 3);

        let encoded = encode_u64(&values, bit_width);
        assert!(encoded.len() < values.len() * 8);

        let decoded = decode_u64(&encoded, values.len());
        assert_eq!(decoded, values);
    }

    #[test]
    fn test_min_bit_width() {
        assert_eq!(min_bit_width(0), 1);
        assert_eq!(min_bit_width(1), 1);
        assert_eq!(min_bit_width(2), 2);
        assert_eq!(min_bit_width(7), 3);
        assert_eq!(min_bit_width(255), 8);
        assert_eq!(min_bit_width(u64::MAX), 64);
    }

    #[test]
    fn test_bitpack_empty() {
        let values: Vec<u64> = vec![];
        let encoded = encode_u64(&values, 8);
        assert!(encoded.is_empty());
        let decoded = decode_u64(&encoded, 0);
        assert!(decoded.is_empty());
    }

    #[test]
    fn test_bitpack_single_value() {
        let values = vec![42u64];
        let bit_width = min_bit_width(42);
        let encoded = encode_u64(&values, bit_width);
        let decoded = decode_u64(&encoded, 1);
        assert_eq!(decoded, values);
    }

    #[test]
    fn test_bitpack_all_zeros() {
        let values = vec![0u64; 100];
        let bit_width = min_bit_width(0);
        assert_eq!(bit_width, 1);
        let encoded = encode_u64(&values, bit_width);
        // 100 个 1 位值 + 1 字节头 = 约 13-14 字节
        assert!(encoded.len() < 20);
        let decoded = decode_u64(&encoded, 100);
        assert_eq!(decoded, values);
    }

    #[test]
    fn test_bitpack_all_max() {
        let values = vec![u64::MAX; 10];
        let bit_width = min_bit_width(u64::MAX);
        assert_eq!(bit_width, 64);
        let encoded = encode_u64(&values, bit_width);
        // 64 位打包应该和原始大小差不多（加 1 字节头）
        assert_eq!(encoded.len(), 1 + 10 * 8);
        let decoded = decode_u64(&encoded, 10);
        assert_eq!(decoded, values);
    }

    #[test]
    fn test_bitpack_1bit_values() {
        // 只有 0 和 1
        let values: Vec<u64> = vec![0, 1, 1, 0, 1, 0, 0, 1, 1, 1, 0, 1, 0, 1, 0, 0];
        let encoded = encode_u64(&values, 1);
        // 16 个 1 位值 = 2 字节 + 1 字节头 = 3 字节
        assert_eq!(encoded.len(), 3);
        let decoded = decode_u64(&encoded, values.len());
        assert_eq!(decoded, values);
    }

    #[test]
    fn test_bitpack_boundary_values() {
        // 刚好在 2^n - 1 边界上的值
        let values = vec![0u64, 1, 254, 255, 256, 65535, 65536];
        let max_val = *values.iter().max().unwrap();
        let bit_width = min_bit_width(max_val);
        let encoded = encode_u64(&values, bit_width);
        let decoded = decode_u64(&encoded, values.len());
        assert_eq!(decoded, values);
    }

    #[test]
    fn test_bitpack_large_dataset() {
        // 较大规模数据验证 roundtrip
        let values: Vec<u64> = (0..1000).map(|i| i * 7 % 500).collect();
        let max_val = *values.iter().max().unwrap();
        let bit_width = min_bit_width(max_val);
        let encoded = encode_u64(&values, bit_width);
        assert!(encoded.len() < values.len() * 8);
        let decoded = decode_u64(&encoded, values.len());
        assert_eq!(decoded, values);
    }

    #[test]
    fn test_bitpack_zero_bit_width() {
        // bit_width = 0 应该返回空
        let values = vec![42u64; 10];
        let encoded = encode_u64(&values, 0);
        assert!(encoded.is_empty());
    }

    #[test]
    fn test_min_bit_width_power_of_two() {
        // 2^n 需要 n+1 位（因为从 0 开始算）
        assert_eq!(min_bit_width(1), 1);    // 2^0
        assert_eq!(min_bit_width(2), 2);    // 2^1
        assert_eq!(min_bit_width(4), 3);    // 2^2
        assert_eq!(min_bit_width(8), 4);    // 2^3
        assert_eq!(min_bit_width(128), 8);  // 2^7
        assert_eq!(min_bit_width(256), 9);  // 2^8
    }
}
