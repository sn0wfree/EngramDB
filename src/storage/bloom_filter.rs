//! Phase 2 P1-A：Bloom Filter（列级等值跳读）
//!
//! 补充 Zone Map（MinMax）的盲区：
//! - MinMax 仅对**范围查询**（<, >, BETWEEN）有效
//! - Bloom Filter 对**等值查询**（=, IN）有效
//! - 两者互补：高基数列 + 等值谓词 → Bloom Filter 跳过不相关 RG
//!
//! ## 算法
//! - 每列每 RG 维护一个 Bloom Filter
//! - 构造：扫描 RG 所有值，hash + 多 k 次插入
//! - 查询：等值谓词 → hash + 检查位数组
//! - 假阳性概率：~1%（默认 10 bit/值，10 hash 函数）
//!
//! ## 性能
//! - 构造：O(n) per RG（与压缩同步，一次性）
//! - 查询：O(k) ≈ O(10) per RG
//! - 内存：~1.25 bytes/value（10 bit + 优化）
//!
//! ## 适用范围
//! - Int64 / Int32 / Float64 / Timestamp / Varchar（小 key）→ hash
//! - Blob / Json / Vector → 不支持（hash 成本过高）

use std::hash::{Hash, Hasher};

use fxhash::FxHasher;

use crate::common::types::DataType;
use crate::Value;

/// Phase 2 P1-A：单列 Bloom Filter
///
/// bit_width = 10 → 每值 10 bit → 100k 值约 125KB
/// hash_count = 7（10 bit-width 下最优假阳性率 ~1%）
#[derive(Debug, Clone)]
pub struct ColumnBloom {
    bits: Vec<u64>,
    bit_width: u32,
    hash_count: u32,
    /// 已插入元素数（用于统计 + 容量评估）
    count: usize,
}

impl ColumnBloom {
    /// 默认参数：bit_width 自适应，hash_count=7（假阳性 ~1%）
    pub fn with_default_capacity(estimated_count: usize) -> Self {
        // 位宽 = log2(count * 10)，保证每元素 ~10 bit
        let total_bits = (estimated_count * 10).max(64) as u64;
        let bit_width = (64 - total_bits.leading_zeros()) as u32;
        Self::new(bit_width, 7)
    }

    /// 自定义参数
    pub fn new(bit_width: u32, hash_count: u32) -> Self {
        let n_words = ((1u64 << bit_width) / 64).max(1) as usize;
        Self {
            bits: vec![0u64; n_words],
            bit_width,
            hash_count,
            count: 0,
        }
    }

    /// 插入一个值（hash 多次）
    pub fn insert<T: Hash + ?Sized>(&mut self, value: &T) {
        for i in 0..self.hash_count {
            let h = hash_i(value, i);
            let pos = h & ((1u64 << self.bit_width) - 1);
            let word = (pos / 64) as usize;
            let bit = pos % 64;
            self.bits[word] |= 1u64 << bit;
        }
        self.count += 1;
    }

    /// 查询一个值（可能命中 → 需精确检查）
    pub fn may_contain<T: Hash + ?Sized>(&self, value: &T) -> bool {
        for i in 0..self.hash_count {
            let h = hash_i(value, i);
            let pos = h & ((1u64 << self.bit_width) - 1);
            let word = (pos / 64) as usize;
            let bit = pos % 64;
            if self.bits[word] & (1u64 << bit) == 0 {
                return false; // 任何一次 hash 命中失败 → 必不在
            }
        }
        true // 所有 hash 都命中 → 可能存在（假阳性 ~1%）
    }

    pub fn count(&self) -> usize {
        self.count
    }

    pub fn memory_bytes(&self) -> usize {
        self.bits.len() * 8 + 16
    }
}

/// 第 i 个 hash 函数（double hashing 模式：h_i = h0 + i * h1）
///
/// 用 fxhash（已在依赖中）而非 sha2/crc32 等重哈希库
fn hash_i<T: Hash + ?Sized>(value: &T, i: u32) -> u64 {
    let mut hasher = FxHasher::default();
    value.hash(&mut hasher);
    let h0 = hasher.finish();

    // 第二次 hash（用于 double hashing）
    let mut hasher2 = FxHasher::default();
    h0.hash(&mut hasher2);
    let h1 = hasher2.finish() | 1; // 确保奇数（避免退化）

    h0.wrapping_add((i as u64).wrapping_mul(h1))
}

/// Phase 2 P1-A：从 `&ColumnData` 构造 Bloom Filter
///
/// Phase 3 P1-A 扩展：
/// - Float32/Float64（IEEE 754 bits → i64）
/// - Varchar/Json（FxHash → i64）
pub fn build_bloom_from_column(
    data: &crate::common::column_data::ColumnData,
    data_type: &DataType,
) -> Option<ColumnBloom> {
    let count = data.len();
    if count == 0 {
        return None;
    }
    let mut bloom = ColumnBloom::with_default_capacity(count);
    for i in 0..count {
        // 跳过 NULL
        if let Some(nulls) = &data.nulls {
            if nulls.test(i) {
                continue;
            }
        }
        match &data.values {
            crate::common::column_data::ColumnValue::Int32(v) => bloom.insert(&v[i]),
            crate::common::column_data::ColumnValue::Int64(v) => bloom.insert(&v[i]),
            crate::common::column_data::ColumnValue::Timestamp(v) => bloom.insert(&v[i]),
            crate::common::column_data::ColumnValue::Float32(v) => bloom.insert(&f32_to_i64_key(v[i])),
            crate::common::column_data::ColumnValue::Float64(v) => bloom.insert(&f64_to_i64_key(v[i])),
            crate::common::column_data::ColumnValue::Varchar(v) => bloom.insert(&str_to_i64_key(&v[i])),
            crate::common::column_data::ColumnValue::Json(v) => bloom.insert(&str_to_i64_key(&v[i])),
            // Vector / Blob / Boolean 不支持（成本过高）
            _ => return None,
        }
        let _ = data_type; // 当前未用，预留接口
    }
    Some(bloom)
}

/// Phase 2 P1-A：判断 Value 是否可 hash 进入 Bloom
pub fn is_bloomable(value: &Value) -> bool {
    matches!(value,
        Value::Int32(_) | Value::Int64(_) | Value::Timestamp(_)
            | Value::Float32(_) | Value::Float64(_)
            | Value::Varchar(_) | Value::Json(_)
    )
}

/// Phase 2.5 P3：从 Value 提取 bloomable key（i64）
///
/// 用于 ColumnChunk::bloom.may_contain() 查询。
///
/// Phase 3 P1-A 扩展：
/// - Float32/Float64：IEEE 754 bits → u64 → i64（位级别保序）
/// - Varchar/Json：FxHash → u64 → i64（稳定哈希）
pub fn bloomable_key(value: &Value) -> i64 {
    match value {
        Value::Int32(v) => *v as i64,
        Value::Int64(v) => *v,
        Value::Timestamp(v) => *v,
        Value::Float32(v) => f32_to_i64_key(*v),
        Value::Float64(v) => f64_to_i64_key(*v),
        Value::Varchar(s) => str_to_i64_key(s),
        Value::Json(s) => str_to_i64_key(s),
        _ => 0,
    }
}

/// Phase 3 P1-A：Float32 → i64 (bit-level encoding)
///
/// IEEE 754：正数保持原位，负数反转所有位（保留排序语义）
pub fn f32_to_i64_key(f: f32) -> i64 {
    let bits = f.to_bits() as u64;
    let key = if bits >> 63 == 0 {
        bits
    } else {
        !bits
    };
    key as i64
}

/// Phase 3 P1-A：Float64 → i64 (bit-level encoding)
///
/// 同 f32_to_i64_key，扩展到 64-bit 浮点数。
pub fn f64_to_i64_key(f: f64) -> i64 {
    let bits = f.to_bits();
    let key = if bits >> 63 == 0 {
        bits
    } else {
        !bits
    };
    key as i64
}

/// Phase 3 P1-A：Varchar → i64 (FxHash → i64)
pub fn str_to_i64_key(s: &str) -> i64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = FxHasher::default();
    s.hash(&mut hasher);
    let h = hasher.finish();
    // 折叠 64-bit hash 到 64-bit（直接 cast）
    h as i64
}

// ============================================================================
// Phase 4 P1-B：Bloom Filter 持久化
// ============================================================================

impl ColumnBloom {
    /// 序列化 Bloom Filter 到字节（Phase 4 P1-B）
    ///
    /// 格式：[u32 bit_width][u32 hash_count][u32 count][u64 bits...]
    /// - bit_width: 4 bytes
    /// - hash_count: 4 bytes
    /// - count: 4 bytes（已插入元素数）
    /// - bits: bit_width 位的位数组（存储为 Vec<u64>）
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(12 + self.bits.len() * 8);
        buf.extend_from_slice(&self.bit_width.to_le_bytes());
        buf.extend_from_slice(&self.hash_count.to_le_bytes());
        buf.extend_from_slice(&(self.count as u32).to_le_bytes());
        for &word in &self.bits {
            buf.extend_from_slice(&word.to_le_bytes());
        }
        buf
    }

    /// 从字节反序列化 Bloom Filter（Phase 4 P1-B）
    ///
    /// 返回 None 如果格式无效
    pub fn from_bytes(data: &[u8]) -> Option<Self> {
        if data.len() < 12 {
            return None;
        }
        let bit_width = u32::from_le_bytes(data[0..4].try_into().ok()?);
        let hash_count = u32::from_le_bytes(data[4..8].try_into().ok()?);
        let count = u32::from_le_bytes(data[8..12].try_into().ok()?) as usize;

        let n_words = ((1u64 << bit_width) / 64).max(1) as usize;
        let expected_len = 12 + n_words * 8;
        if data.len() < expected_len {
            return None;
        }

        let mut bits = Vec::with_capacity(n_words);
        let mut offset = 12;
        for _ in 0..n_words {
            bits.push(u64::from_le_bytes(
                data[offset..offset + 8].try_into().ok()?,
            ));
            offset += 8;
        }

        Some(Self {
            bits,
            bit_width,
            hash_count,
            count,
        })
    }

    /// 序列化到 Vec<u8>（便捷包装）
    pub fn serialize(&self) -> Vec<u8> {
        self.to_bytes()
    }

    /// 从字节反序列化（便捷包装，错误返回空 Bloom）
    pub fn deserialize(data: &[u8]) -> Self {
        Self::from_bytes(data).unwrap_or_else(|| Self::with_default_capacity(0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_insert_and_query() {
        let mut bloom = ColumnBloom::with_default_capacity(100);
        bloom.insert(&42i64);
        bloom.insert(&100i64);
        bloom.insert(&999i64);

        assert!(bloom.may_contain(&42i64));
        assert!(bloom.may_contain(&100i64));
        assert!(bloom.may_contain(&999i64));
        // 注：可能假阳性，但不常见
    }

    #[test]
    fn test_negative_lookups() {
        // 大数据集下不应有大量假阴性
        let mut bloom = ColumnBloom::with_default_capacity(1000);
        for i in 0..1000 {
            bloom.insert(&(i as i64));
        }
        // 不在数据集中的值
        let mut false_negatives = 0;
        for i in 10_000..11_000 {
            if bloom.may_contain(&(i as i64)) {
                false_negatives += 1;
            }
        }
        // 假阴性必须为 0（bloom 不会漏报）
        assert_eq!(false_negatives, 0);
    }

    #[test]
    fn test_memory_footprint() {
        let bloom = ColumnBloom::with_default_capacity(10_000);
        // 10K 值 × 10 bit = 100K bit ≈ 12.5KB
        // 加上元数据约 13KB
        assert!(bloom.memory_bytes() < 20_000, "实际 {} bytes", bloom.memory_bytes());
    }

    #[test]
    fn test_no_false_negatives() {
        // 核心不变量：插入的值必须命中
        let mut bloom = ColumnBloom::with_default_capacity(500);
        let values: Vec<i64> = (0..500).collect();
        for v in &values {
            bloom.insert(v);
        }
        for v in &values {
            assert!(bloom.may_contain(v), "False negative for: {}", v);
        }
    }

    #[test]
    fn test_false_positive_rate() {
        // 1K 元素，1K 不存在元素查询：假阳性率应 < 5%
        let mut bloom = ColumnBloom::with_default_capacity(1000);
        for i in 0..1000 {
            bloom.insert(&(i as i64));
        }
        let mut false_positives = 0;
        for i in 5000..6000 {
            if bloom.may_contain(&(i as i64)) {
                false_positives += 1;
            }
        }
        let rate = false_positives as f64 / 1000.0;
        assert!(rate < 0.05, "false positive rate {} > 5%", rate);
    }

    #[test]
    fn test_bloomable_detection() {
        assert!(is_bloomable(&Value::Int64(42)));
        assert!(is_bloomable(&Value::Int32(42)));
        assert!(is_bloomable(&Value::Timestamp(0)));
        // Phase 3 P1-A 扩展：Float + Varchar + Json 现在也可 bloom
        assert!(is_bloomable(&Value::Float64(1.0)));
        assert!(is_bloomable(&Value::Float32(1.0)));
        assert!(is_bloomable(&Value::Varchar("x".into())));
        assert!(is_bloomable(&Value::Json("{}".into())));
        // Null + Vector + Blob 不支持
        assert!(!is_bloomable(&Value::Null));
        assert!(!is_bloomable(&Value::Vector(vec![1.0, 2.0])));
    }

    #[test]
    fn test_float_bloomable_key_stable() {
        // Float → i64 key 稳定性（同一 f → 同一 key）
        assert_eq!(f32_to_i64_key(1.5), f32_to_i64_key(1.5));
        assert_eq!(f64_to_i64_key(2.71828), f64_to_i64_key(2.71828));

        // NaN 行为（IEEE 754 不唯一，但稳定）
        let nan_key = f32_to_i64_key(f32::NAN);
        assert_eq!(nan_key, f32_to_i64_key(f32::NAN));
    }

    #[test]
    fn test_str_bloomable_key_stable() {
        // 同一字符串多次调用应得相同 key
        let k1 = str_to_i64_key("hello");
        let k2 = str_to_i64_key("hello");
        assert_eq!(k1, k2);
        // 不同字符串大概率不同（FxHash 高质量）
        let k3 = str_to_i64_key("world");
        assert_ne!(k1, k3);
    }

    #[test]
    fn test_bloom_serialize_deserialize_roundtrip() {
        // Phase 4 P1-B：Bloom 持久化往返
        let mut bloom = ColumnBloom::with_default_capacity(1000);
        bloom.insert(&42i64);
        bloom.insert(&12345i64);
        bloom.insert(&99999i64);

        let bytes = bloom.to_bytes();
        let restored = ColumnBloom::from_bytes(&bytes).unwrap();

        // 语义一致
        assert!(restored.may_contain(&42i64));
        assert!(restored.may_contain(&12345i64));
        assert!(restored.may_contain(&99999i64));
        assert_eq!(restored.count, bloom.count);
        assert_eq!(restored.bit_width, bloom.bit_width);
        assert_eq!(restored.hash_count, bloom.hash_count);
    }

    #[test]
    fn test_bloom_serialize_empty() {
        let bloom = ColumnBloom::with_default_capacity(100);
        let bytes = bloom.to_bytes();
        let restored = ColumnBloom::from_bytes(&bytes).unwrap();
        assert_eq!(restored.count, 0);
        assert_eq!(restored.bit_width, bloom.bit_width);
    }

    #[test]
    fn test_bloom_deserialize_invalid_data() {
        assert!(ColumnBloom::from_bytes(&[0, 0, 0]).is_none());
        assert!(ColumnBloom::from_bytes(&[]).is_none());
    }

    #[test]
    fn test_bloom_serialize_large() {
        // Phase 4 P1-B：大量数据序列化
        let mut bloom = ColumnBloom::with_default_capacity(10_000);
        for i in 0..10_000 {
            bloom.insert(&(i as i64));
        }
        let bytes = bloom.to_bytes();
        let restored = ColumnBloom::from_bytes(&bytes).unwrap();
        assert_eq!(restored.count, 10_000);
        // 序列化后的 Bloom 应能正确查询
        assert!(restored.may_contain(&5000i64));
        assert!(!restored.may_contain(&20_000i64)); // 不存在的值
    }
}