//! 游程编码 (Run-Length Encoding)
//!
//! 格式：[count: u32][value: 固定长度] ...
//! 适用于重复值较多的列

/// RLE 编码（固定宽度值）
pub fn encode(data: &[u8]) -> Vec<u8> {
    if data.is_empty() {
        return Vec::new();
    }

    // 简单实现：按 8 字节值进行 RLE
    // 实际应用中应根据数据类型确定值宽度
    let value_size = 8; // 默认 8 字节
    let mut result = Vec::new();

    if data.len() < value_size {
        return data.to_vec();
    }

    let mut i = 0;
    while i < data.len() {
        let end = std::cmp::min(i + value_size, data.len());
        let current_val = &data[i..end];

        // 计算连续重复次数
        let mut count = 1u32;
        let mut j = i + value_size;
        while j + value_size <= data.len() && &data[j..j + value_size] == current_val {
            count += 1;
            j += value_size;
        }

        if count > 1 {
            // 压缩：标记 + 计数 + 值
            result.push(0xFF); // RLE 标记
            result.extend_from_slice(&count.to_le_bytes());
            result.extend_from_slice(current_val);
            i = j;
        } else if current_val[0] == 0xFF {
            // 单次出现的值但首字节为 0xFF：必须转义，否则解码器会误判为 RLE 标记。
            // 用 count=1 的 RLE 段编码（13 字节 vs 裸 8 字节，但保证正确性）。
            result.push(0xFF);
            result.extend_from_slice(&1u32.to_le_bytes());
            result.extend_from_slice(current_val);
            i += value_size;
        } else {
            // 原值输出
            result.extend_from_slice(current_val);
            i += value_size;
        }
    }

    result
}

/// RLE 解码
pub fn decode(data: &[u8]) -> Vec<u8> {
    if data.is_empty() {
        return Vec::new();
    }

    let value_size = 8;
    let mut result = Vec::new();
    let mut i = 0;

    while i < data.len() {
        // i+5 < len（而非 <=）确保 count 之后至少有 1 字节 value 数据，
        // 防止短尾数据中恰好出现 0xFF 时被误判为 RLE 段。
        if data[i] == 0xFF && i + 5 < data.len() {
            // RLE 编码段
            let count = u32::from_le_bytes(data[i + 1..i + 5].try_into().unwrap());
            let val_start = i + 5;
            let val_end = std::cmp::min(val_start + value_size, data.len());
            let value = &data[val_start..val_end];

            for _ in 0..count {
                result.extend_from_slice(value);
            }
            i = val_end;
        } else {
            // 原值
            let end = std::cmp::min(i + value_size, data.len());
            result.extend_from_slice(&data[i..end]);
            i = end;
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rle_all_same() {
        let data: Vec<u8> = (0..100).map(|_| 42u8).collect();
        // 填充为 8 字节对齐
        let mut padded = Vec::new();
        for _ in 0..12 {
            padded.extend_from_slice(&[42u8; 8]);
        }
        let encoded = encode(&padded);
        assert!(encoded.len() < padded.len());
        let decoded = decode(&encoded);
        assert_eq!(decoded, padded);
    }

    #[test]
    fn test_rle_mixed() {
        let mut data = Vec::new();
        // 10 个相同的 8 字节值
        for _ in 0..10 {
            data.extend_from_slice(&12345u64.to_le_bytes());
        }
        // 5 个不同的
        for i in 0..5 {
            data.extend_from_slice(&(i as u64).to_le_bytes());
        }
        let encoded = encode(&data);
        let decoded = decode(&encoded);
        assert_eq!(decoded, data);
    }

    #[test]
    fn test_rle_empty() {
        let data: Vec<u8> = vec![];
        let encoded = encode(&data);
        assert!(encoded.is_empty());
        let decoded = decode(&encoded);
        assert!(decoded.is_empty());
    }

    #[test]
    fn test_rle_single_byte() {
        // 不足 8 字节，直接返回
        let data = vec![42u8];
        let encoded = encode(&data);
        assert_eq!(encoded, data);
        let decoded = decode(&encoded);
        assert_eq!(decoded, data);
    }

    #[test]
    fn test_rle_all_unique() {
        // 所有值都不同，不压缩
        let mut data = Vec::new();
        for i in 0..10u64 {
            data.extend_from_slice(&i.to_le_bytes());
        }
        let encoded = encode(&data);
        // 全不相同的情况下，每个值直接输出（无标记）
        assert_eq!(encoded.len(), data.len());
        let decoded = decode(&encoded);
        assert_eq!(decoded, data);
    }

    #[test]
    fn test_rle_alternating() {
        // 交替出现两个值，每个只出现一次，不压缩
        let mut data = Vec::new();
        for i in 0..10 {
            let val = if i % 2 == 0 { 0xAAAAAAAAu64 } else { 0xBBBBBBBBu64 };
            data.extend_from_slice(&val.to_le_bytes());
        }
        let encoded = encode(&data);
        let decoded = decode(&encoded);
        assert_eq!(decoded, data);
    }

    #[test]
    fn test_rle_long_run_at_start() {
        // 开头有一个长 run
        let mut data = Vec::new();
        for _ in 0..100 {
            data.extend_from_slice(&0xFFu64.to_le_bytes());
        }
        for i in 0..5u64 {
            data.extend_from_slice(&i.to_le_bytes());
        }
        let encoded = encode(&data);
        assert!(encoded.len() < data.len());
        let decoded = decode(&encoded);
        assert_eq!(decoded, data);
    }

    #[test]
    fn test_rle_long_run_at_end() {
        // 结尾有一个长 run
        let mut data = Vec::new();
        for i in 0..5u64 {
            data.extend_from_slice(&i.to_le_bytes());
        }
        for _ in 0..100 {
            data.extend_from_slice(&0xEEu64.to_le_bytes());
        }
        let encoded = encode(&data);
        assert!(encoded.len() < data.len());
        let decoded = decode(&encoded);
        assert_eq!(decoded, data);
    }

    #[test]
    fn test_rle_multiple_runs() {
        // 多个不同长度的 run
        let mut data = Vec::new();
        // run 1: 5 个 A
        for _ in 0..5 { data.extend_from_slice(&0xAAAAAAAAu64.to_le_bytes()); }
        // run 2: 1 个 B
        data.extend_from_slice(&0xBBBBBBBBu64.to_le_bytes());
        // run 3: 20 个 C
        for _ in 0..20 { data.extend_from_slice(&0xCCCCCCCCu64.to_le_bytes()); }
        // run 4: 3 个 D
        for _ in 0..3 { data.extend_from_slice(&0xDDDDDDDDu64.to_le_bytes()); }

        let encoded = encode(&data);
        let decoded = decode(&encoded);
        assert_eq!(decoded, data);
    }

    #[test]
    fn test_rle_single_value_many_times() {
        // 大量相同值，压缩率应该很高
        let mut data = Vec::new();
        for _ in 0..1000 {
            data.extend_from_slice(&42u64.to_le_bytes());
        }
        let encoded = encode(&data);
        // 1000 个 8 字节值 = 8000 字节，压缩后约 1+4+8 = 13 字节
        assert!(encoded.len() < 50);
        let decoded = decode(&encoded);
        assert_eq!(decoded.len(), data.len());
        assert_eq!(decoded, data);
    }

    #[test]
    fn test_rle_value_starting_with_0xff() {
        // 非重复值首字节为 0xFF：编码器必须转义，否则解码器误判为 RLE 标记
        let mut data = Vec::new();
        // 一个首字节为 0xFF 的 8 字节值（如 i64 = -1 → 0xFFFFFFFFFFFFFFFF）
        data.extend_from_slice(&(-1i64).to_le_bytes());
        // 一个普通值
        data.extend_from_slice(&12345u64.to_le_bytes());
        let encoded = encode(&data);
        let decoded = decode(&encoded);
        assert_eq!(decoded, data);
    }

    #[test]
    fn test_rle_mixed_with_0xff_prefixes() {
        // 混合场景：重复值 + 0xFF 开头的非重复值 + 普通值
        let mut data = Vec::new();
        // 10 个相同的值
        for _ in 0..10 {
            data.extend_from_slice(&0xAAAAAAAAu64.to_le_bytes());
        }
        // 一个 0xFF 开头的值（i64 = -256 → 字节序开头是 0x00, 但 -1 开头是 0xFF）
        data.extend_from_slice(&(-1i64).to_le_bytes());
        // 5 个另一个相同值
        for _ in 0..5 {
            data.extend_from_slice(&0xBBBBBBBBu64.to_le_bytes());
        }
        // 又一个 0xFF 开头的值（LE 首字节为 0xFF）
        data.extend_from_slice(&0x00FFFFFFFFFFFFFFu64.to_le_bytes());
        let encoded = encode(&data);
        let decoded = decode(&encoded);
        assert_eq!(decoded, data);
    }
}
