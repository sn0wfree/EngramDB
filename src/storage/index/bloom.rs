//! 布隆过滤器 (Bloom Filter)
//!
//! 空间高效的概率性数据结构，用于测试元素是否存在于集合中。
//! - 可能有误报（false positive）：说存在但实际不存在
//! - 不会有漏报（false negative）：说不存在就一定不存在
//!
//! 适用场景：
//! - 点查询前快速判断 key 是否存在（避免不必要的 IO）
//! - 减少数据库查询次数（AI Agent 高频查询场景）
//! - 内存占用极小：100 万元素、1% 误报率仅需 ~1.13 MB

use crate::Value;
use std::hash::{Hash, Hasher};
use fxhash::FxHasher64;

/// 布隆过滤器
#[derive(Debug, Clone)]
pub struct BloomFilter {
    /// 位数组
    bits: Vec<u64>,
    /// 哈希函数数量
    num_hashes: u32,
    /// 插入元素数量估计
    count: u64,
    /// 总位数
    total_bits: u64,
}

impl BloomFilter {
    /// 创建布隆过滤器
    ///
    /// - `expected_items`: 预期插入的元素数量
    /// - `false_positive_rate`: 可接受的误报率（如 0.01 = 1%）
    pub fn new(expected_items: u64, false_positive_rate: f64) -> Self {
        // 计算最优位数和哈希函数数量
        // m = -n * ln(p) / (ln(2))^2
        // k = m/n * ln(2)
        let ln2 = std::f64::consts::LN_2;
        let m = -1.0 * expected_items as f64 * false_positive_rate.ln() / (ln2 * ln2);
        let k = (m / expected_items as f64) * ln2;

        let total_bits = m.ceil() as u64;
        let num_hashes = k.round().max(1.0) as u32;

        // 向上取整到 64 的倍数
        let chunks = (total_bits + 63) / 64;
        let actual_bits = chunks * 64;

        Self {
            bits: vec![0u64; chunks as usize],
            num_hashes,
            count: 0,
            total_bits: actual_bits,
        }
    }

    /// 插入一个值
    pub fn insert(&mut self, value: &Value) {
        let (h1, h2) = self.hash_value(value);
        for i in 0..self.num_hashes {
            let bit_idx = self.compute_index(h1, h2, i);
            self.set_bit(bit_idx);
        }
        self.count += 1;
    }

    /// 检查值是否可能存在
    ///
    /// - true: 可能存在（有误报可能）
    /// - false: 一定不存在
    pub fn contains(&self, value: &Value) -> bool {
        let (h1, h2) = self.hash_value(value);
        for i in 0..self.num_hashes {
            let bit_idx = self.compute_index(h1, h2, i);
            if !self.get_bit(bit_idx) {
                return false;
            }
        }
        true
    }

    /// 已插入元素数量
    pub fn count(&self) -> u64 {
        self.count
    }

    /// 总位数
    pub fn total_bits(&self) -> u64 {
        self.total_bits
    }

    /// 哈希函数数量
    pub fn num_hashes(&self) -> u32 {
        self.num_hashes
    }

    /// 内存占用（字节）
    pub fn memory_usage(&self) -> usize {
        self.bits.len() * 8
    }

    /// 估计当前误报率
    pub fn estimated_false_positive_rate(&self) -> f64 {
        // (1 - e^(-k*n/m))^k
        let k = self.num_hashes as f64;
        let n = self.count as f64;
        let m = self.total_bits as f64;
        (1.0 - (-k * n / m).exp()).powf(k)
    }

    /// 清空过滤器
    pub fn clear(&mut self) {
        for chunk in &mut self.bits {
            *chunk = 0;
        }
        self.count = 0;
    }

    // --- 内部方法 ---

    /// 双重哈希法：用两个哈希函数生成 k 个哈希值
    /// h(i) = h1 + i * h2
    fn compute_index(&self, h1: u64, h2: u64, i: u32) -> u64 {
        let idx = h1.wrapping_add((i as u64).wrapping_mul(h2));
        idx % self.total_bits
    }

    /// 对 Value 进行双重哈希
    fn hash_value(&self, value: &Value) -> (u64, u64) {
        use std::hash::{Hash, Hasher};

        let mut hasher1 = FxHasher64::default();
        value.hash(&mut hasher1);
        let h1 = hasher1.finish();

        // 第二个哈希：从 h1 派生（旋转 + 黄金比例常数）
        // 双重哈希法的 h2 只需要与 h1 独立即可，不需要加密强度
        let h2 = h1.rotate_left(21) ^ 0x9E3779B97F4A7C15;

        (h1, h2)
    }

    fn set_bit(&mut self, idx: u64) {
        let chunk = (idx / 64) as usize;
        let bit = (idx % 64) as u32;
        self.bits[chunk] |= 1u64 << bit;
    }

    fn get_bit(&self, idx: u64) -> bool {
        let chunk = (idx / 64) as usize;
        let bit = (idx % 64) as u32;
        (self.bits[chunk] & (1u64 << bit)) != 0
    }
}

// ============================================================================
// 测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_bloom() {
        let bf = BloomFilter::new(1000, 0.01);
        assert_eq!(bf.count(), 0);
        assert!(bf.total_bits() > 0);
        assert!(bf.num_hashes() > 0);
    }

    #[test]
    fn test_insert_and_contains() {
        let mut bf = BloomFilter::new(1000, 0.01);

        // 空过滤器不包含任何东西
        assert!(!bf.contains(&Value::Int64(42)));

        bf.insert(&Value::Int64(42));
        assert!(bf.contains(&Value::Int64(42)));
        assert_eq!(bf.count(), 1);
    }

    #[test]
    fn test_no_false_negatives() {
        let mut bf = BloomFilter::new(10000, 0.01);

        // 插入 1000 个元素
        for i in 0..1000 {
            bf.insert(&Value::Int64(i));
        }

        // 所有插入的元素都应该能找到（没有漏报）
        for i in 0..1000 {
            assert!(bf.contains(&Value::Int64(i)), "missing element {}", i);
        }
    }

    #[test]
    fn test_false_positive_rate() {
        let mut bf = BloomFilter::new(10000, 0.01);

        // 插入 10000 个元素
        for i in 0..10000 {
            bf.insert(&Value::Int64(i));
        }

        // 测试 10000 个未插入的元素
        let mut false_positives = 0;
        for i in 10000..20000 {
            if bf.contains(&Value::Int64(i)) {
                false_positives += 1;
            }
        }

        let rate = false_positives as f64 / 10000.0;
        // 实际误报率应该接近 1%，允许一定波动
        assert!(rate < 0.05, "false positive rate too high: {}", rate);
    }

    #[test]
    fn test_varchar_values() {
        let mut bf = BloomFilter::new(1000, 0.01);
        bf.insert(&Value::Varchar("hello".into()));
        bf.insert(&Value::Varchar("world".into()));

        assert!(bf.contains(&Value::Varchar("hello".into())));
        assert!(bf.contains(&Value::Varchar("world".into())));
        assert!(!bf.contains(&Value::Varchar("foo".into())));
    }

    #[test]
    fn test_boolean_values() {
        let mut bf = BloomFilter::new(100, 0.01);
        bf.insert(&Value::Boolean(true));
        bf.insert(&Value::Boolean(false));

        assert!(bf.contains(&Value::Boolean(true)));
        assert!(bf.contains(&Value::Boolean(false)));
    }

    #[test]
    fn test_null_value() {
        let mut bf = BloomFilter::new(100, 0.01);
        bf.insert(&Value::Null);
        assert!(bf.contains(&Value::Null));
        assert!(!bf.contains(&Value::Int64(0)));
    }

    #[test]
    fn test_clear() {
        let mut bf = BloomFilter::new(1000, 0.01);
        bf.insert(&Value::Int64(1));
        bf.insert(&Value::Int64(2));

        assert!(bf.contains(&Value::Int64(1)));
        assert_eq!(bf.count(), 2);

        bf.clear();
        assert!(!bf.contains(&Value::Int64(1)));
        assert_eq!(bf.count(), 0);
    }

    #[test]
    fn test_memory_usage() {
        // 10000 元素、1% 误报率 ≈ 9585 bits ≈ 1199 bytes ≈ 1.2 KB
        let bf = BloomFilter::new(10000, 0.01);
        let mem = bf.memory_usage();
        assert!(mem > 0);
        assert!(mem < 20000, "memory usage too high: {} bytes", mem);
    }

    #[test]
    fn test_estimated_false_positive_rate() {
        let mut bf = BloomFilter::new(10000, 0.01);

        // 空过滤器误报率接近 0
        assert!(bf.estimated_false_positive_rate() < 0.001);

        // 插入 10000 个元素后接近 1%
        for i in 0..10000 {
            bf.insert(&Value::Int64(i));
        }
        let rate = bf.estimated_false_positive_rate();
        assert!(rate > 0.005 && rate < 0.05, "unexpected FPR: {}", rate);
    }

    #[test]
    fn test_different_false_positive_rates() {
        let bf1 = BloomFilter::new(10000, 0.1); // 10%
        let bf2 = BloomFilter::new(10000, 0.01); // 1%
        let bf3 = BloomFilter::new(10000, 0.001); // 0.1%

        // 更低的误报率需要更多空间
        assert!(bf1.memory_usage() < bf2.memory_usage());
        assert!(bf2.memory_usage() < bf3.memory_usage());

        // 更低的误报率需要更多哈希函数
        assert!(bf1.num_hashes() <= bf2.num_hashes());
        assert!(bf2.num_hashes() <= bf3.num_hashes());
    }

    #[test]
    fn test_large_insert() {
        let mut bf = BloomFilter::new(100000, 0.01);
        for i in 0..100000 {
            bf.insert(&Value::Int64(i));
        }
        assert_eq!(bf.count(), 100000);

        // 验证没有漏报
        for i in (0..100000).step_by(1000) {
            assert!(bf.contains(&Value::Int64(i)));
        }
    }

    #[test]
    fn test_float_values() {
        let mut bf = BloomFilter::new(1000, 0.01);
        bf.insert(&Value::Float64(3.14));
        bf.insert(&Value::Float64(2.718));

        assert!(bf.contains(&Value::Float64(3.14)));
        assert!(!bf.contains(&Value::Float64(1.414)));
    }

    #[test]
    fn test_int32_values() {
        let mut bf = BloomFilter::new(1000, 0.01);
        bf.insert(&Value::Int32(42));
        assert!(bf.contains(&Value::Int32(42)));
        assert!(!bf.contains(&Value::Int32(43)));
    }
}
