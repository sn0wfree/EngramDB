//! Bloom Filter（v0.17.0 M1-8 / P3.5）：row group 级等值过滤
//!
//! 定位：等值谓词（`col = x`）在 MinMax 范围内但实际值不存在时，
//! MinMax 无法跳过（值在 [min, max] 区间内），Bloom 可判定"肯定不存在"
//! 从而整块跳过。假阳性只浪费过滤，**绝无假阴性**（不丢数据）。
//!
//! 惰性构建：首次等值查询时从列数据构建一次并缓存（内存换查询），
//! 任何写路径失效重建。不落盘（重启后惰性重建，避免磁盘格式变更）。

use std::hash::{Hash, Hasher};

use crate::Value;

/// 位数组大小为 2 的幂（`& mask` 取模）
#[derive(Debug, Clone)]
pub struct BloomFilter {
    bits: Vec<u64>,
    num_hashes: usize,
    mask: u64,
    /// 已插入元素数（调试/统计用）
    inserted: u64,
}

impl BloomFilter {
    /// 按期望元素数与目标假阳性率构造（m = 2^k 位数组）
    pub fn with_capacity(expected_items: usize, false_positive_rate: f64) -> Self {
        // m = -n * ln(p) / ln(2)^2；k = m/n * ln(2)
        let bpi = -(false_positive_rate.ln()) / std::f64::consts::LN_2.powi(2);
        let mut m = (expected_items as f64 * bpi).ceil() as u64;
        m = m.max(64).next_power_of_two();
        let num_hashes = ((m as f64 / expected_items.max(1) as f64) * std::f64::consts::LN_2)
            .ceil()
            .max(1.0) as usize;
        Self {
            bits: vec![0u64; (m / 64).max(1) as usize],
            num_hashes,
            mask: m - 1,
            inserted: 0,
        }
    }

    /// 插入一个值（全 k 位置位）
    pub fn insert(&mut self, value: &Value) {
        let (a, b) = hash_pair(value);
        for i in 0..self.num_hashes as u64 {
            let bit = a.wrapping_add(i.wrapping_mul(b)) & self.mask;
            self.bits[(bit / 64) as usize] |= 1u64 << (bit % 64);
        }
        self.inserted += 1;
    }

    /// 可能包含（false = 肯定不存在；true = 可能存在）
    ///
    /// 数值归一化：`Value` 哈希严格区分 Int32/Int64/Timestamp/Float64，
    /// 但 SQL 等值语义是数值互通的（如 Int32 列 + Int64 字面量）。
    /// 查询时对数值 key 探测全部数值形式，任一命中即保留（零假阴性）。
    pub fn might_contain(&self, value: &Value) -> bool {
        if self.might(value) {
            return true;
        }
        for alt in numeric_forms(value) {
            if self.might(&alt) {
                return true;
            }
        }
        false
    }

    fn might(&self, value: &Value) -> bool {
        let (a, b) = hash_pair(value);
        for i in 0..self.num_hashes as u64 {
            let bit = a.wrapping_add(i.wrapping_mul(b)) & self.mask;
            if self.bits[(bit / 64) as usize] & (1u64 << (bit % 64)) == 0 {
                return false;
            }
        }
        true
    }

    pub fn inserted(&self) -> u64 {
        self.inserted
    }
}

/// 数值 key 的等价形式（与主键点查归一化同语义，含浮点整数值）
fn numeric_forms(value: &Value) -> Vec<Value> {
    use Value::*;
    match value {
        Int32(v) => vec![Int64(*v as i64), Timestamp(*v as i64), Float64(*v as f64)],
        Int64(v) => vec![
            Int32(*v as i32),
            Timestamp(*v),
            Float64(*v as f64),
        ],
        Timestamp(v) => vec![Int64(*v), Int32(*v as i32), Float64(*v as f64)],
        Float64(v) => {
            // 整数值浮点 → 整数形式互查（Float64 列 + Int 字面量场景）
            if v.fract() == 0.0 && v.abs() < 9.007199254740992e15 {
                let i = *v as i64;
                vec![Int64(i), Int32(i as i32), Timestamp(i)]
            } else {
                vec![]
            }
        }
        _ => vec![],
    }
}

/// 双哈希（独立两路 FxHasher，第二路加盐）
fn hash_pair(value: &Value) -> (u64, u64) {
    let mut h1 = fxhash::FxHasher::default();
    value.hash(&mut h1);
    let a = h1.finish();

    let mut h2 = fxhash::FxHasher::default();
    value.hash(&mut h2);
    0x9e3779b97f4a7c15u64.hash(&mut h2);
    let b = h2.finish();
    (a, b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bloom_no_false_negative() {
        let mut bf = BloomFilter::with_capacity(10_000, 0.01);
        for i in 0..10_000i64 {
            bf.insert(&Value::Int64(i));
        }
        // 所有插入的值必须命中（无假阴性）
        for i in 0..10_000i64 {
            assert!(bf.might_contain(&Value::Int64(i)), "miss {i}");
        }
        // 大部分未插入值应被拒绝（假阳性率 < 5%）
        let mut hits = 0;
        for i in 100_000..110_000i64 {
            if bf.might_contain(&Value::Int64(i)) {
                hits += 1;
            }
        }
        assert!(
            hits < 500,
            "false positive too high: {hits}/10000 (need < 5%)"
        );
    }

    #[test]
    fn test_bloom_cross_type() {
        // 数值归一化：Int32 列 + Int64/Timestamp/Float64 整数字面量等值互通
        let mut bf = BloomFilter::with_capacity(10, 0.01);
        bf.insert(&Value::Int32(7));
        assert!(bf.might_contain(&Value::Int32(7)));
        assert!(bf.might_contain(&Value::Int64(7)), "Int64 字面量应命中 Int32 列");
        assert!(bf.might_contain(&Value::Timestamp(7)), "Timestamp 字面量应命中");
        assert!(bf.might_contain(&Value::Float64(7.0)), "整数值浮点应命中");
        assert!(!bf.might_contain(&Value::Float64(7.5)), "非整数值浮点不应命中");
        assert!(!bf.might_contain(&Value::Int64(8)), "不同值不应命中");

        // 反向：Int64 列 + Int32 字面量
        let mut bf = BloomFilter::with_capacity(10, 0.01);
        bf.insert(&Value::Int64(42));
        assert!(bf.might_contain(&Value::Int32(42)));
        assert!(bf.might_contain(&Value::Float64(42.0)));

        // 字符串严格区分
        let mut bf = BloomFilter::with_capacity(10, 0.01);
        bf.insert(&Value::Varchar("abc".into()));
        assert!(bf.might_contain(&Value::Varchar("abc".into())));
        assert!(!bf.might_contain(&Value::Varchar("abd".into())));
    }
}
