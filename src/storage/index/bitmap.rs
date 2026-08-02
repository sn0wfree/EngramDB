//! 位图索引 (Bitmap Index)
//!
//! 低基数列的分析查询加速器：
//! - 每个不同值对应一个位图（bit = 1 表示该行包含该值）
//! - AND/OR/XOR 位运算极快，适合多条件过滤
//! - 空间效率：100 万行 × 100 个值 ≈ 12.5 MB
//!
//! 适用场景：
//! - WHERE category = 'A' AND status = 'active'
//! - GROUP BY 低基数列
//! - 基数 < 1000 的列效果最佳

use crate::Value;
use std::collections::HashMap;

/// 位图（用 Vec<u64> 实现，64 位为一组）
#[derive(Debug, Clone, Default)]
pub struct Bitmap {
    bits: Vec<u64>,
}

impl Bitmap {
    /// 创建空位图
    pub fn new() -> Self {
        Self { bits: Vec::new() }
    }

    /// 创建指定容量的位图（预分配）
    pub fn with_capacity(num_rows: u32) -> Self {
        let chunks = (num_rows as usize + 63) / 64;
        Self {
            bits: vec![0u64; chunks],
        }
    }

    /// 设置某一位为 1
    pub fn set(&mut self, row: u32) {
        let chunk = row as usize / 64;
        let bit = row as usize % 64;
        if chunk >= self.bits.len() {
            self.bits.resize(chunk + 1, 0);
        }
        self.bits[chunk] |= 1u64 << bit;
    }

    /// 检查某一位是否为 1
    pub fn get(&self, row: u32) -> bool {
        let chunk = row as usize / 64;
        if chunk >= self.bits.len() {
            return false;
        }
        let bit = row as usize % 64;
        (self.bits[chunk] & (1u64 << bit)) != 0
    }

    /// 位图与运算（in-place）
    pub fn and(&mut self, other: &Bitmap) {
        let min_len = self.bits.len().min(other.bits.len());
        for i in 0..min_len {
            self.bits[i] &= other.bits[i];
        }
        // 超出部分清零
        for i in min_len..self.bits.len() {
            self.bits[i] = 0;
        }
    }

    /// 位图或运算（in-place）
    pub fn or(&mut self, other: &Bitmap) {
        if other.bits.len() > self.bits.len() {
            self.bits.resize(other.bits.len(), 0);
        }
        for i in 0..other.bits.len() {
            self.bits[i] |= other.bits[i];
        }
    }

    /// 位图异或运算（in-place）
    pub fn xor(&mut self, other: &Bitmap) {
        if other.bits.len() > self.bits.len() {
            self.bits.resize(other.bits.len(), 0);
        }
        for i in 0..other.bits.len() {
            self.bits[i] ^= other.bits[i];
        }
    }

    /// 位图取反（只取前 num_bits 位）
    pub fn not(&mut self, num_bits: u32) {
        let chunks = (num_bits as usize + 63) / 64;
        if self.bits.len() < chunks {
            self.bits.resize(chunks, 0);
        }
        for i in 0..chunks {
            self.bits[i] = !self.bits[i];
        }
        // 清除超出的位
        let remainder = num_bits as usize % 64;
        if remainder != 0 && chunks > 0 {
            let mask = (1u64 << remainder) - 1;
            self.bits[chunks - 1] &= mask;
        }
    }

    /// 统计 1 的个数（popcount）
    pub fn count_ones(&self) -> u64 {
        self.bits.iter().map(|&x| x.count_ones() as u64).sum()
    }

    /// 是否全零
    pub fn is_empty(&self) -> bool {
        self.bits.iter().all(|&x| x == 0)
    }

    /// 迭代所有为 1 的行号
    pub fn iter_ones(&self) -> BitmapIter<'_> {
        BitmapIter {
            bitmap: self,
            chunk_idx: 0,
            bit_idx: 0,
        }
    }

    /// 内存占用（字节）
    pub fn memory_usage(&self) -> usize {
        self.bits.len() * 8
    }
}

/// 位图迭代器（遍历所有为 1 的位）
pub struct BitmapIter<'a> {
    bitmap: &'a Bitmap,
    chunk_idx: usize,
    bit_idx: u32,
}

impl<'a> Iterator for BitmapIter<'a> {
    type Item = u32;

    fn next(&mut self) -> Option<Self::Item> {
        while self.chunk_idx < self.bitmap.bits.len() {
            let chunk = self.bitmap.bits[self.chunk_idx];
            while self.bit_idx < 64 {
                let mask = 1u64 << self.bit_idx;
                let row = (self.chunk_idx * 64) as u32 + self.bit_idx;
                self.bit_idx += 1;
                if chunk & mask != 0 {
                    return Some(row);
                }
            }
            self.chunk_idx += 1;
            self.bit_idx = 0;
        }
        None
    }
}

/// 位图索引
///
/// 存储每个 distinct 值对应的位图，支持快速等值查询和多条件组合。
#[derive(Debug, Clone)]
pub struct BitmapIndex {
    /// 值到位图的映射
    bitmaps: HashMap<Value, Bitmap>,
    /// 总行数
    num_rows: u32,
    /// 不同值的数量（基数）
    cardinality: usize,
}

impl BitmapIndex {
    /// 创建空的位图索引
    pub fn new() -> Self {
        Self {
            bitmaps: HashMap::new(),
            num_rows: 0,
            cardinality: 0,
        }
    }

    /// 从一列数据构建位图索引
    pub fn build(values: &[Value]) -> Self {
        let mut index = Self::new();
        for (row_idx, value) in values.iter().enumerate() {
            index.insert(value.clone(), row_idx as u32);
        }
        index
    }

    /// 插入一个值
    pub fn insert(&mut self, value: Value, row_id: u32) {
        if row_id >= self.num_rows {
            self.num_rows = row_id + 1;
        }

        let bitmap = self.bitmaps.entry(value).or_insert_with(Bitmap::new);
        if bitmap.bits.is_empty() {
            self.cardinality += 1;
        }
        bitmap.set(row_id);
    }

    /// 等值查询：返回匹配的行号位图
    pub fn equals(&self, value: &Value) -> Option<&Bitmap> {
        self.bitmaps.get(value)
    }

    /// 不等于查询：返回所有不匹配的行（NOT）
    pub fn not_equals(&self, value: &Value) -> Bitmap {
        let mut result = Bitmap::with_capacity(self.num_rows);
        result.not(self.num_rows); // 全 1
        if let Some(bm) = self.bitmaps.get(value) {
            let mut bm_copy = bm.clone();
            bm_copy.not(self.num_rows);
            result.and(&bm_copy);
        }
        result
    }

    /// IN 查询：多个值的 OR
    pub fn is_in(&self, values: &[Value]) -> Bitmap {
        let mut result = Bitmap::new();
        for v in values {
            if let Some(bm) = self.bitmaps.get(v) {
                result.or(bm);
            }
        }
        result
    }

    /// 基数（不同值的数量）
    pub fn cardinality(&self) -> usize {
        self.cardinality
    }

    /// 总行数
    pub fn num_rows(&self) -> u32 {
        self.num_rows
    }

    /// 获取所有不同的值
    pub fn distinct_values(&self) -> Vec<&Value> {
        self.bitmaps.keys().collect()
    }

    /// 内存占用（字节）
    pub fn memory_usage(&self) -> usize {
        self.bitmaps.values().map(|b| b.memory_usage()).sum()
    }
}

impl Default for BitmapIndex {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// 测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // --- Bitmap 基础 ---

    #[test]
    fn test_bitmap_set_get() {
        let mut bm = Bitmap::new();
        bm.set(0);
        bm.set(5);
        bm.set(63);
        bm.set(64);
        bm.set(100);

        assert!(bm.get(0));
        assert!(bm.get(5));
        assert!(bm.get(63));
        assert!(bm.get(64));
        assert!(bm.get(100));
        assert!(!bm.get(1));
        assert!(!bm.get(99));
        assert!(!bm.get(200));
    }

    #[test]
    fn test_bitmap_empty() {
        let bm = Bitmap::new();
        assert!(bm.is_empty());
        assert_eq!(bm.count_ones(), 0);
    }

    #[test]
    fn test_bitmap_count_ones() {
        let mut bm = Bitmap::new();
        bm.set(0);
        bm.set(1);
        bm.set(100);
        assert_eq!(bm.count_ones(), 3);
    }

    #[test]
    fn test_bitmap_and() {
        let mut a = Bitmap::new();
        a.set(0);
        a.set(1);
        a.set(2);

        let mut b = Bitmap::new();
        b.set(1);
        b.set(2);
        b.set(3);

        a.and(&b);
        assert!(a.get(1));
        assert!(a.get(2));
        assert!(!a.get(0));
        assert!(!a.get(3));
        assert_eq!(a.count_ones(), 2);
    }

    #[test]
    fn test_bitmap_or() {
        let mut a = Bitmap::new();
        a.set(0);
        a.set(1);

        let mut b = Bitmap::new();
        b.set(2);
        b.set(3);

        a.or(&b);
        assert_eq!(a.count_ones(), 4);
        for i in 0..4 {
            assert!(a.get(i));
        }
    }

    #[test]
    fn test_bitmap_xor() {
        let mut a = Bitmap::new();
        a.set(0);
        a.set(1);
        a.set(2);

        let mut b = Bitmap::new();
        b.set(1);
        b.set(2);
        b.set(3);

        a.xor(&b);
        assert!(a.get(0));
        assert!(!a.get(1));
        assert!(!a.get(2));
        assert!(a.get(3));
        assert_eq!(a.count_ones(), 2);
    }

    #[test]
    fn test_bitmap_not() {
        let mut bm = Bitmap::new();
        bm.set(0);
        bm.set(2);
        bm.not(5); // 取反前 5 位

        assert!(!bm.get(0)); // 原 1 → 0
        assert!(bm.get(1));  // 原 0 → 1
        assert!(!bm.get(2)); // 原 1 → 0
        assert!(bm.get(3));  // 原 0 → 1
        assert!(bm.get(4));  // 原 0 → 1
        assert!(!bm.get(5)); // 超出范围，应该为 0
    }

    #[test]
    fn test_bitmap_iter() {
        let mut bm = Bitmap::new();
        bm.set(0);
        bm.set(5);
        bm.set(100);

        let ones: Vec<u32> = bm.iter_ones().collect();
        assert_eq!(ones, vec![0, 5, 100]);
    }

    #[test]
    fn test_bitmap_memory_usage() {
        let bm = Bitmap::with_capacity(100);
        // 100 bits = 2 chunks of 64 = 16 bytes
        assert_eq!(bm.memory_usage(), 16);
    }

    // --- BitmapIndex ---

    #[test]
    fn test_index_build() {
        let values = vec![
            Value::Int64(1),
            Value::Int64(2),
            Value::Int64(1),
            Value::Int64(3),
            Value::Int64(2),
        ];
        let index = BitmapIndex::build(&values);

        assert_eq!(index.num_rows(), 5);
        assert_eq!(index.cardinality(), 3);

        let bm1 = index.equals(&Value::Int64(1)).unwrap();
        assert_eq!(bm1.count_ones(), 2);
        assert!(bm1.get(0));
        assert!(bm1.get(2));
    }

    #[test]
    fn test_index_equals() {
        let mut index = BitmapIndex::new();
        index.insert(Value::Varchar("A".into()), 0);
        index.insert(Value::Varchar("B".into()), 1);
        index.insert(Value::Varchar("A".into()), 2);

        let result = index.equals(&Value::Varchar("A".into())).unwrap();
        assert_eq!(result.count_ones(), 2);
        assert!(result.get(0));
        assert!(result.get(2));
    }

    #[test]
    fn test_index_not_equals() {
        let mut index = BitmapIndex::new();
        index.insert(Value::Int64(1), 0);
        index.insert(Value::Int64(2), 1);
        index.insert(Value::Int64(1), 2);

        let result = index.not_equals(&Value::Int64(1));
        assert_eq!(result.count_ones(), 1);
        assert!(result.get(1));
    }

    #[test]
    fn test_index_is_in() {
        let mut index = BitmapIndex::new();
        index.insert(Value::Int64(1), 0);
        index.insert(Value::Int64(2), 1);
        index.insert(Value::Int64(3), 2);
        index.insert(Value::Int64(4), 3);

        let values = vec![Value::Int64(1), Value::Int64(3)];
        let result = index.is_in(&values);
        assert_eq!(result.count_ones(), 2);
        assert!(result.get(0));
        assert!(result.get(2));
    }

    #[test]
    fn test_index_distinct_values() {
        let mut index = BitmapIndex::new();
        index.insert(Value::Int64(1), 0);
        index.insert(Value::Int64(2), 1);
        index.insert(Value::Int64(1), 2);

        let values = index.distinct_values();
        assert_eq!(values.len(), 2);
    }

    #[test]
    fn test_index_memory_usage() {
        let mut index = BitmapIndex::new();
        for i in 0..1000 {
            index.insert(Value::Int64((i % 10) as i64), i as u32);
        }
        // 10 个值 × 1000 行 ≈ 10 × 128 bytes = 1280 bytes（位图数据本身）
        let mem = index.memory_usage();
        assert!(mem > 0);
        assert!(mem < 2000); // 应该远小于 2KB
    }

    #[test]
    fn test_index_empty() {
        let index = BitmapIndex::new();
        assert_eq!(index.cardinality(), 0);
        assert_eq!(index.num_rows(), 0);
        assert!(index.equals(&Value::Int64(1)).is_none());
    }

    #[test]
    fn test_index_with_boolean() {
        let mut index = BitmapIndex::new();
        index.insert(Value::Boolean(true), 0);
        index.insert(Value::Boolean(false), 1);
        index.insert(Value::Boolean(true), 2);

        let trues = index.equals(&Value::Boolean(true)).unwrap();
        assert_eq!(trues.count_ones(), 2);

        let falses = index.equals(&Value::Boolean(false)).unwrap();
        assert_eq!(falses.count_ones(), 1);
    }

    #[test]
    fn test_bitmap_and_different_sizes() {
        let mut a = Bitmap::new();
        a.set(0);
        a.set(100); // 2 chunks

        let mut b = Bitmap::new();
        b.set(0); // 1 chunk

        a.and(&b);
        assert!(a.get(0));
        assert!(!a.get(100)); // a 的第 2 个 chunk 被清零
    }

    #[test]
    fn test_bitmap_or_different_sizes() {
        let mut a = Bitmap::new();
        a.set(0); // 1 chunk

        let mut b = Bitmap::new();
        b.set(100); // 2 chunks

        a.or(&b);
        assert!(a.get(0));
        assert!(a.get(100));
    }
}
