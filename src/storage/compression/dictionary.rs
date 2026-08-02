//! 字典编码
//!
//! 适用于低基数字符串列

use std::collections::HashMap;

/// 字典编码结果
pub struct DictionaryEncoded {
    pub dictionary: Vec<Vec<u8>>,
    pub indices: Vec<u32>,
}

/// 对字节串序列进行字典编码
pub fn encode(strings: &[&[u8]]) -> DictionaryEncoded {
    let mut dict_map: HashMap<&[u8], u32> = HashMap::new();
    let mut dictionary: Vec<Vec<u8>> = Vec::new();
    let mut indices: Vec<u32> = Vec::with_capacity(strings.len());

    for &s in strings {
        let idx = match dict_map.get(s) {
            Some(&idx) => idx,
            None => {
                let idx = dictionary.len() as u32;
                dict_map.insert(s, idx);
                dictionary.push(s.to_vec());
                idx
            }
        };
        indices.push(idx);
    }

    DictionaryEncoded {
        dictionary,
        indices,
    }
}

/// 字典解码
pub fn decode(encoded: &DictionaryEncoded) -> Vec<Vec<u8>> {
    encoded.indices
        .iter()
        .map(|&idx| encoded.dictionary[idx as usize].clone())
        .collect()
}

/// 计算压缩率（字典编码后大小 vs 原始大小）
pub fn compression_ratio(encoded: &DictionaryEncoded, original_size: usize) -> f64 {
    let dict_size: usize = encoded.dictionary.iter().map(|s| s.len()).sum();
    let indices_size = encoded.indices.len() * 4; // u32
    let encoded_size = dict_size + indices_size;
    encoded_size as f64 / original_size as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dictionary_simple() {
        let strings: Vec<&[u8]> = vec![
            b"apple", b"banana", b"apple", b"cherry", b"banana", b"apple",
        ];
        let encoded = encode(&strings);
        assert_eq!(encoded.dictionary.len(), 3);
        assert_eq!(encoded.indices.len(), 6);
        assert_eq!(encoded.indices[0], encoded.indices[2]);
        assert_eq!(encoded.indices[0], encoded.indices[5]);

        let decoded = decode(&encoded);
        assert_eq!(decoded.len(), strings.len());
        for (i, s) in strings.iter().enumerate() {
            assert_eq!(decoded[i], *s);
        }
    }

    #[test]
    fn test_dictionary_empty() {
        let strings: Vec<&[u8]> = vec![];
        let encoded = encode(&strings);
        assert_eq!(encoded.dictionary.len(), 0);
        assert_eq!(encoded.indices.len(), 0);

        let decoded = decode(&encoded);
        assert_eq!(decoded.len(), 0);
    }

    #[test]
    fn test_dictionary_single_value() {
        let strings: Vec<&[u8]> = vec![b"hello"; 100];
        let encoded = encode(&strings);
        assert_eq!(encoded.dictionary.len(), 1);
        assert_eq!(encoded.indices.len(), 100);
        assert_eq!(encoded.indices[0], 0);
        assert_eq!(encoded.indices[99], 0);

        let decoded = decode(&encoded);
        assert_eq!(decoded.len(), 100);
        assert_eq!(decoded[0], b"hello");
    }

    #[test]
    fn test_dictionary_all_unique() {
        let strings: Vec<&[u8]> = vec![
            b"a", b"b", b"c", b"d", b"e",
        ];
        let encoded = encode(&strings);
        assert_eq!(encoded.dictionary.len(), 5);
        assert_eq!(encoded.indices.len(), 5);
        // 每个值的索引应该不同
        let mut indices_set = std::collections::HashSet::new();
        for &idx in &encoded.indices {
            indices_set.insert(idx);
        }
        assert_eq!(indices_set.len(), 5);

        let decoded = decode(&encoded);
        for (i, s) in strings.iter().enumerate() {
            assert_eq!(decoded[i], *s);
        }
    }

    #[test]
    fn test_dictionary_empty_strings() {
        let strings: Vec<&[u8]> = vec![b"", b"abc", b"", b"def", b""];
        let encoded = encode(&strings);
        assert_eq!(encoded.dictionary.len(), 3);
        assert_eq!(encoded.indices.len(), 5);

        let decoded = decode(&encoded);
        assert_eq!(decoded[0], b"");
        assert_eq!(decoded[2], b"");
        assert_eq!(decoded[4], b"");
    }

    #[test]
    fn test_dictionary_unicode() {
        let s1 = "你好世界".as_bytes();
        let s2 = "こんにちは".as_bytes();
        let s3 = "Hello World".as_bytes();
        let strings: Vec<&[u8]> = vec![s1, s2, s1, s3, s2, s1];
        let encoded = encode(&strings);
        assert_eq!(encoded.dictionary.len(), 3);

        let decoded = decode(&encoded);
        assert_eq!(decoded[0], s1);
        assert_eq!(decoded[1], s2);
        assert_eq!(decoded[3], s3);
    }

    #[test]
    fn test_dictionary_large_strings() {
        let long = vec![b'x'; 1024];
        let strings: Vec<&[u8]> = vec![&long, &long, &long];
        let encoded = encode(&strings);
        assert_eq!(encoded.dictionary.len(), 1);
        assert_eq!(encoded.indices.len(), 3);

        let decoded = decode(&encoded);
        assert_eq!(decoded[0].len(), 1024);
        assert_eq!(decoded[0], long);
    }

    #[test]
    fn test_compression_ratio_high_cardinality() {
        // 高基数：压缩率应该接近或大于 1（几乎不压缩甚至膨胀）
        let strings: Vec<Vec<u8>> = (0..100u8).map(|i| vec![i; 8]).collect();
        let refs: Vec<&[u8]> = strings.iter().map(|s| s.as_slice()).collect();
        let encoded = encode(&refs);
        let original_size = 100 * 8;
        let ratio = compression_ratio(&encoded, original_size);
        // 高基数下字典编码压缩率较差
        assert!(ratio > 0.0);
    }

    #[test]
    fn test_compression_ratio_low_cardinality() {
        // 低基数：压缩率应该很好
        let strings: Vec<&[u8]> = vec![b"status_active"; 1000];
        let encoded = encode(&strings);
        let original_size = 1000 * 13;
        let ratio = compression_ratio(&encoded, original_size);
        // 只有一个字典条目 + 1000 个 u32 索引
        // 压缩后 ≈ 13 + 4000 = 4013 字节，原始 13000 字节
        assert!(ratio < 1.0, "低基数应压缩: ratio={}", ratio);
    }

    #[test]
    fn test_dictionary_order_preservation() {
        // 字典中条目的顺序应该按首次出现顺序
        let strings: Vec<&[u8]> = vec![b"c", b"a", b"b", b"a", b"c"];
        let encoded = encode(&strings);
        assert_eq!(encoded.dictionary[0], b"c");
        assert_eq!(encoded.dictionary[1], b"a");
        assert_eq!(encoded.dictionary[2], b"b");
        assert_eq!(encoded.indices, vec![0, 1, 2, 1, 0]);
    }
}
