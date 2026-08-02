//! Frame of Reference (FOR) 编码
//!
//! 存储值与基准值（最小值）的差值，减少表示所需位数

/// FOR 编码 u64 序列
pub fn encode_u64(values: &[u64]) -> (u64, Vec<u64>) {
    if values.is_empty() {
        return (0, Vec::new());
    }

    let base = *values.iter().min().unwrap();
    let deltas: Vec<u64> = values.iter().map(|v| v - base).collect();
    (base, deltas)
}

/// FOR 解码
pub fn decode_u64(base: u64, deltas: &[u64]) -> Vec<u64> {
    deltas.iter().map(|d| base + d).collect()
}

/// FOR + Bit-packing 组合编码
pub fn encode_for_bitpack(values: &[u64]) -> Vec<u8> {
    if values.is_empty() {
        return Vec::new();
    }

    let (base, deltas) = encode_u64(values);
    let max_delta = *deltas.iter().max().unwrap_or(&0);
    let bit_width = super::bitpacking::min_bit_width(max_delta);

    let mut result = Vec::new();
    result.extend_from_slice(&base.to_le_bytes());
    result.extend_from_slice(&(values.len() as u32).to_le_bytes());
    result.push(bit_width);

    let packed = super::bitpacking::encode_u64(&deltas, bit_width);
    // 跳过 packed 的第 1 字节（bit_width 已存）
    result.extend_from_slice(&packed[1..]);

    result
}

/// FOR + Bit-packing 组合解码
pub fn decode_for_bitpack(data: &[u8]) -> Vec<u64> {
    if data.len() < 13 {
        return Vec::new();
    }

    let base = u64::from_le_bytes(data[0..8].try_into().unwrap());
    let count = u32::from_le_bytes(data[8..12].try_into().unwrap()) as usize;
    let bit_width = data[12];

    // 构造带 bit_width 头的 packed 数据
    let mut packed = vec![bit_width];
    packed.extend_from_slice(&data[13..]);

    let deltas = super::bitpacking::decode_u64(&packed, count);
    decode_u64(base, &deltas)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_for_simple() {
        let values: Vec<u64> = vec![100, 101, 105, 102, 103, 100, 110];
        let (base, deltas) = encode_u64(&values);
        assert_eq!(base, 100);
        assert_eq!(deltas, vec![0, 1, 5, 2, 3, 0, 10]);

        let decoded = decode_u64(base, &deltas);
        assert_eq!(decoded, values);
    }

    #[test]
    fn test_for_bitpack() {
        let values: Vec<u64> = (1000..1050).collect();
        let encoded = encode_for_bitpack(&values);
        assert!(encoded.len() < values.len() * 8);

        let decoded = decode_for_bitpack(&encoded);
        assert_eq!(decoded, values);
    }

    #[test]
    fn test_for_empty() {
        let values: Vec<u64> = vec![];
        let (base, deltas) = encode_u64(&values);
        assert_eq!(base, 0);
        assert!(deltas.is_empty());

        let decoded = decode_u64(base, &deltas);
        assert!(decoded.is_empty());
    }

    #[test]
    fn test_for_single_value() {
        let values = vec![42u64];
        let (base, deltas) = encode_u64(&values);
        assert_eq!(base, 42);
        assert_eq!(deltas, vec![0]);

        let decoded = decode_u64(base, &deltas);
        assert_eq!(decoded, values);
    }

    #[test]
    fn test_for_all_same() {
        let values = vec![1000u64; 100];
        let (base, deltas) = encode_u64(&values);
        assert_eq!(base, 1000);
        assert!(deltas.iter().all(|&d| d == 0));

        let decoded = decode_u64(base, &deltas);
        assert_eq!(decoded, values);
    }

    #[test]
    fn test_for_descending() {
        // 降序排列，最小值在最后
        let values: Vec<u64> = vec![100, 90, 80, 70, 60, 50];
        let (base, deltas) = encode_u64(&values);
        assert_eq!(base, 50);
        assert_eq!(deltas, vec![50, 40, 30, 20, 10, 0]);

        let decoded = decode_u64(base, &deltas);
        assert_eq!(decoded, values);
    }

    #[test]
    fn test_for_bitpack_single_value() {
        let values = vec![999u64];
        let encoded = encode_for_bitpack(&values);
        let decoded = decode_for_bitpack(&encoded);
        assert_eq!(decoded, values);
    }

    #[test]
    fn test_for_bitpack_all_same() {
        let values = vec![5000u64; 200];
        let encoded = encode_for_bitpack(&values);
        // 所有值相同，delta 都为 0，只需 1 位打包
        // 8 (base) + 4 (count) + 1 (bit_width) + ceil(200/8) ≈ 38 字节
        assert!(encoded.len() < 100);
        let decoded = decode_for_bitpack(&encoded);
        assert_eq!(decoded, values);
    }

    #[test]
    fn test_for_bitpack_small_range() {
        // 范围很小，压缩率应该很高
        let values: Vec<u64> = (1_000_000..1_000_100).collect();
        let encoded = encode_for_bitpack(&values);
        // 原始 800 字节，范围 100 只需 7 位
        assert!(encoded.len() < 200);
        let decoded = decode_for_bitpack(&encoded);
        assert_eq!(decoded, values);
    }

    #[test]
    fn test_for_bitpack_empty() {
        let values: Vec<u64> = vec![];
        let encoded = encode_for_bitpack(&values);
        assert!(encoded.is_empty());
        let decoded = decode_for_bitpack(&encoded);
        assert!(decoded.is_empty());
    }

    #[test]
    fn test_for_bitpack_large_range() {
        // 范围很大（接近 u64 最大值），压缩效果有限但应正确 roundtrip
        let values: Vec<u64> = vec![0, u64::MAX / 2, u64::MAX];
        let encoded = encode_for_bitpack(&values);
        let decoded = decode_for_bitpack(&encoded);
        assert_eq!(decoded, values);
    }
}
