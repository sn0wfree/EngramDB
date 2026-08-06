//! 稀疏主索引 (Sparse Primary Index)
//!
//! 借鉴 ClickHouse MergeTree 稀疏索引思想：
//! 每 N 行（granule）只记一条索引，索引体积极小，可全量缓存内存。
//!
//! 默认 granule 大小：8192 行（与 ClickHouse 一致）
//! 10 亿行仅需数 MB 索引（vs B+Tree 数 GB）

use crate::Value;

/// 默认 granule 大小（每 granule 的行数）
pub const DEFAULT_GRANULE_SIZE: u32 = 8192;

/// 稀疏索引条目
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct IndexGranule {
    /// 该 granule 的首行主键值（用于范围定位）
    pub first_key: Value,
    /// 该 granule 在 Row Group 中的起始行偏移
    pub row_offset: u32,
    /// 该 granule 的行数（通常 = granule_size，最后一个可能更少）
    pub row_count: u32,
}

/// 稀疏主索引
///
/// 每个 Row Group 对应一个稀疏索引，按主键有序存储。
/// 查询时通过二分查找快速定位 granule 范围，无需扫描全部数据。
#[derive(Debug, Clone)]
pub struct SparseIndex {
    /// 索引条目（按 first_key 排序）
    granules: Vec<IndexGranule>,
    /// 每个 granule 的行数
    granule_size: u32,
}

impl SparseIndex {
    /// 创建空的稀疏索引
    pub fn new(granule_size: u32) -> Self {
        Self {
            granules: Vec::new(),
            granule_size,
        }
    }

    /// 从一组有序的主键值构建稀疏索引
    pub fn build_from_keys(&mut self, keys: &[Value]) {
        self.granules.clear();

        if keys.is_empty() {
            return;
        }

        let gs = self.granule_size as usize;
        let mut offset = 0u32;

        for chunk in keys.chunks(gs) {
            let first_key = chunk[0].clone();
            let row_count = chunk.len() as u32;

            self.granules.push(IndexGranule {
                first_key,
                row_offset: offset,
                row_count,
            });

            offset += row_count;
        }
    }

    /// 追加一个 granule（列存 compact 增量维护用）
    ///
    /// `row_offset` 为全局行序偏移（与 row_id 对齐）。
    /// 调用方需保证同一索引内 row_offset + row_count 单调不重叠。
    pub fn append_granule(&mut self, first_key: Value, row_offset: u32, row_count: u32) {
        self.granules.push(IndexGranule {
            first_key,
            row_offset,
            row_count,
        });
    }

    /// 追加一段有序数据（compact 增量路径）：按 granule 切分并追加
    ///
    /// `keys` 为新增段的主键列（段内有序），`base_offset` 为该段起始全局行偏移。
    /// 最后一个不足 granule 大小的片段也强制成条（append 边界处切 granule）。
    pub fn append_sorted_keys(&mut self, keys: &[Value], base_offset: u32) {
        if keys.is_empty() {
            return;
        }
        let gs = self.granule_size as usize;
        let mut offset = base_offset;
        for chunk in keys.chunks(gs) {
            self.granules.push(IndexGranule {
                first_key: chunk[0].clone(),
                row_offset: offset,
                row_count: chunk.len() as u32,
            });
            offset += chunk.len() as u32;
        }
    }

    /// 查找值所在的 granule 范围（返回 granule 索引范围）
    ///
    /// 对于等值查询：返回可能包含该值的 granule 索引
    /// 对于范围查询：返回可能在 [low, high] 范围内的 granule 索引范围
    pub fn locate_range(&self, low: &Value, high: &Value) -> std::ops::Range<usize> {
        if self.granules.is_empty() {
            return 0..0;
        }

        // 找第一个 first_key > low 的 granule，向前退一个
        let start = self.find_first_gt(low).saturating_sub(1);

        // 找第一个 first_key > high 的 granule
        let end = self.find_first_gt(high);

        start..end.min(self.granules.len())
    }

    /// 等值查找：返回可能包含该值的 granule 索引
    pub fn locate_eq(&self, key: &Value) -> Option<usize> {
        if self.granules.is_empty() {
            return None;
        }

        let idx = self.find_first_gt(key).saturating_sub(1);
        if idx < self.granules.len() {
            Some(idx)
        } else {
            None
        }
    }

    /// 二分查找：找到第一个 first_key > target 的 granule 索引
    fn find_first_gt(&self, target: &Value) -> usize {
        let mut lo = 0usize;
        let mut hi = self.granules.len();

        while lo < hi {
            let mid = (lo + hi) / 2;
            if value_gt(&self.granules[mid].first_key, target) {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }

        lo
    }

    /// 获取 granule 数量
    pub fn granule_count(&self) -> usize {
        self.granules.len()
    }

    /// 清空全部 granule
    pub fn clear_index(&mut self) {
        self.granules.clear();
    }

    /// 最后一个 granule（追加式维护用）
    pub fn last_granule(&self) -> Option<&IndexGranule> {
        self.granules.last()
    }

    /// 获取指定索引的 granule
    pub fn get_granule(&self, idx: usize) -> Option<&IndexGranule> {
        self.granules.get(idx)
    }

    /// 获取 granule 大小
    pub fn granule_size(&self) -> u32 {
        self.granule_size
    }

    /// 估算索引内存大小（字节）
    pub fn estimated_memory_size(&self) -> usize {
        // 每个 granule 约 32 字节（Value 引用 + u32 + u32 + 开销）
        self.granules.len() * 32
    }

    /// 序列化（bincode：granules + granule_size）
    pub fn to_bytes(&self) -> Vec<u8> {
        bincode::serialize(&(&self.granules, self.granule_size))
            .unwrap_or_default()
    }

    /// 反序列化（失败返回 None）
    pub fn from_bytes(data: &[u8]) -> Option<SparseIndex> {
        let (granules, granule_size): (Vec<IndexGranule>, u32) =
            bincode::deserialize(data).ok()?;
        Some(SparseIndex { granules, granule_size })
    }
}

/// Value 比较：a > b
fn value_gt(a: &Value, b: &Value) -> bool {
    use Value::*;
    match (a, b) {
        (Int32(x), Int32(y)) => x > y,
        (Int64(x), Int64(y)) => x > y,
        (Int32(x), Int64(y)) => (*x as i64) > *y,
        (Int64(x), Int32(y)) => *x > (*y as i64),
        (Int32(x), Float64(y)) => (*x as f64) > *y,
        (Float64(x), Int32(y)) => *x > (*y as f64),
        (Int64(x), Float64(y)) => (*x as f64) > *y,
        (Float64(x), Int64(y)) => *x > (*y as f64),
        (Float64(x), Float64(y)) => x > y,
        (Timestamp(x), Timestamp(y)) => x > y,
        (Timestamp(x), Int32(y)) => *x > (*y as i64),
        (Timestamp(x), Int64(y)) => *x > *y,
        (Int32(x), Timestamp(y)) => (*x as i64) > *y,
        (Int64(x), Timestamp(y)) => *x > *y,
        (Timestamp(x), Float64(y)) => (*x as f64) > *y,
        (Float64(x), Timestamp(y)) => *x > (*y as f64),
        (Varchar(x), Varchar(y)) => x > y,
        (Boolean(x), Boolean(y)) => x > y,
        (Null, _) => false,
        (_, Null) => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sparse_index_build_and_locate() {
        let mut idx = SparseIndex::new(4); // 小 granule 便于测试
        let keys: Vec<Value> = (1..=10u32).map(|i| Value::Int32(i as i32)).collect();
        idx.build_from_keys(&keys);

        // 应有 3 个 granule: [1-4], [5-8], [9-10]
        assert_eq!(idx.granule_count(), 3);

        // 等值查找
        assert_eq!(idx.locate_eq(&Value::Int32(1)), Some(0));
        assert_eq!(idx.locate_eq(&Value::Int32(4)), Some(0));
        assert_eq!(idx.locate_eq(&Value::Int32(5)), Some(1));
        assert_eq!(idx.locate_eq(&Value::Int32(8)), Some(1));
        assert_eq!(idx.locate_eq(&Value::Int32(9)), Some(2));
        assert_eq!(idx.locate_eq(&Value::Int32(10)), Some(2));

        // 范围查找
        let range = idx.locate_range(&Value::Int32(3), &Value::Int32(7));
        assert_eq!(range, 0..2); // granule 0 和 1

        let range = idx.locate_range(&Value::Int32(6), &Value::Int32(10));
        assert_eq!(range, 1..3); // granule 1 和 2
    }

    #[test]
    fn test_sparse_index_empty() {
        let idx = SparseIndex::new(8192);
        assert_eq!(idx.granule_count(), 0);
        assert_eq!(idx.locate_eq(&Value::Int32(1)), None);
    }

    #[test]
    fn test_sparse_index_single_granule() {
        let mut idx = SparseIndex::new(100);
        let keys: Vec<Value> = (1..=10u32).map(|i| Value::Int64(i as i64)).collect();
        idx.build_from_keys(&keys);

        assert_eq!(idx.granule_count(), 1);
        assert_eq!(idx.locate_eq(&Value::Int64(1)), Some(0));
        assert_eq!(idx.locate_eq(&Value::Int64(10)), Some(0));
        assert_eq!(idx.locate_eq(&Value::Int64(5)), Some(0));

        let range = idx.locate_range(&Value::Int64(3), &Value::Int64(7));
        assert_eq!(range, 0..1);
    }

    #[test]
    fn test_sparse_index_exact_granule_boundary() {
        // 刚好填满整数个 granule
        let mut idx = SparseIndex::new(4);
        let keys: Vec<Value> = (1..=8u32).map(|i| Value::Int32(i as i32)).collect();
        idx.build_from_keys(&keys);

        assert_eq!(idx.granule_count(), 2);
        assert_eq!(idx.get_granule(0).unwrap().row_count, 4);
        assert_eq!(idx.get_granule(1).unwrap().row_count, 4);
        assert_eq!(idx.get_granule(0).unwrap().row_offset, 0);
        assert_eq!(idx.get_granule(1).unwrap().row_offset, 4);
    }

    #[test]
    fn test_sparse_index_varchar_keys() {
        let mut idx = SparseIndex::new(3);
        let keys = vec![
            Value::Varchar("apple".to_string()),
            Value::Varchar("banana".to_string()),
            Value::Varchar("cherry".to_string()),
            Value::Varchar("date".to_string()),
            Value::Varchar("elderberry".to_string()),
        ];
        idx.build_from_keys(&keys);

        assert_eq!(idx.granule_count(), 2);
        assert_eq!(idx.locate_eq(&Value::Varchar("banana".to_string())), Some(0));
        assert_eq!(idx.locate_eq(&Value::Varchar("date".to_string())), Some(1));

        let range = idx.locate_range(
            &Value::Varchar("banana".to_string()),
            &Value::Varchar("date".to_string()),
        );
        assert_eq!(range, 0..2);
    }

    #[test]
    fn test_sparse_index_boolean_keys() {
        let mut idx = SparseIndex::new(3);
        let keys = vec![
            Value::Boolean(false),
            Value::Boolean(false),
            Value::Boolean(true),
            Value::Boolean(true),
            Value::Boolean(true),
        ];
        idx.build_from_keys(&keys);

        assert_eq!(idx.granule_count(), 2);
        assert_eq!(idx.locate_eq(&Value::Boolean(false)), Some(0));
        assert_eq!(idx.locate_eq(&Value::Boolean(true)), Some(1)); // 最后一个 first_key <= true 的 granule
    }

    #[test]
    fn test_sparse_index_float_keys() {
        let mut idx = SparseIndex::new(3);
        let keys = vec![
            Value::Float64(1.0),
            Value::Float64(2.5),
            Value::Float64(3.7),
            Value::Float64(5.0),
            Value::Float64(7.2),
        ];
        idx.build_from_keys(&keys);

        assert_eq!(idx.granule_count(), 2);
        assert_eq!(idx.locate_eq(&Value::Float64(2.5)), Some(0));
        assert_eq!(idx.locate_eq(&Value::Float64(5.0)), Some(1));
    }

    #[test]
    fn test_sparse_index_granule_size() {
        let idx = SparseIndex::new(4096);
        assert_eq!(idx.granule_size(), 4096);

        let idx = SparseIndex::new(DEFAULT_GRANULE_SIZE);
        assert_eq!(idx.granule_size(), 8192);
    }

    #[test]
    fn test_sparse_index_estimated_memory() {
        let mut idx = SparseIndex::new(100);
        let keys: Vec<Value> = (1..=500u32).map(|i| Value::Int32(i as i32)).collect();
        idx.build_from_keys(&keys);

        // 500 行 / 100 = 5 个 granule
        assert_eq!(idx.granule_count(), 5);
        let mem = idx.estimated_memory_size();
        assert_eq!(mem, 5 * 32);
    }

    #[test]
    fn test_sparse_index_get_granule_out_of_bounds() {
        let idx = SparseIndex::new(100);
        assert!(idx.get_granule(0).is_none());
        assert!(idx.get_granule(999).is_none());
    }

    #[test]
    fn test_sparse_index_range_entire_dataset() {
        let mut idx = SparseIndex::new(4);
        let keys: Vec<Value> = (1..=10u32).map(|i| Value::Int32(i as i32)).collect();
        idx.build_from_keys(&keys);

        // 范围覆盖所有数据
        let range = idx.locate_range(&Value::Int32(0), &Value::Int32(100));
        assert_eq!(range, 0..3);
    }

    #[test]
    fn test_sparse_index_range_before_all() {
        let mut idx = SparseIndex::new(4);
        let keys: Vec<Value> = (10..=20u32).map(|i| Value::Int32(i as i32)).collect();
        idx.build_from_keys(&keys);

        // 范围在所有数据之前
        let range = idx.locate_range(&Value::Int32(0), &Value::Int32(5));
        assert_eq!(range, 0..0);
    }

    #[test]
    fn test_sparse_index_range_after_all() {
        let mut idx = SparseIndex::new(4);
        let keys: Vec<Value> = (1..=10u32).map(|i| Value::Int32(i as i32)).collect();
        idx.build_from_keys(&keys);

        // 范围在所有数据之后：最后一个 granule 可能包含（稀疏索引不知道上界）
        let range = idx.locate_range(&Value::Int32(20), &Value::Int32(30));
        assert_eq!(range.start, 2);
        assert_eq!(range.end, 3);
    }

    #[test]
    fn test_sparse_index_null_key() {
        let mut idx = SparseIndex::new(4);
        let keys = vec![
            Value::Null,
            Value::Int32(1),
            Value::Int32(2),
        ];
        idx.build_from_keys(&keys);

        assert_eq!(idx.granule_count(), 1);
        // Null 被认为小于任何非 Null 值
        assert_eq!(idx.locate_eq(&Value::Null), Some(0));
    }

    #[test]
    fn test_sparse_index_single_key() {
        let mut idx = SparseIndex::new(100);
        let keys = vec![Value::Int64(42)];
        idx.build_from_keys(&keys);

        assert_eq!(idx.granule_count(), 1);
        assert_eq!(idx.locate_eq(&Value::Int64(42)), Some(0));
        assert_eq!(idx.get_granule(0).unwrap().row_count, 1);
    }

    #[test]
    fn test_sparse_index_rebuild_clears_old() {
        let mut idx = SparseIndex::new(4);
        let keys1: Vec<Value> = (1..=10u32).map(|i| Value::Int32(i as i32)).collect();
        idx.build_from_keys(&keys1);
        assert_eq!(idx.granule_count(), 3);

        // 重新构建更少的 key
        let keys2: Vec<Value> = (1..=3u32).map(|i| Value::Int32(i as i32)).collect();
        idx.build_from_keys(&keys2);
        assert_eq!(idx.granule_count(), 1);
    }

    #[test]
    fn test_sparse_index_mixed_numeric_types() {
        // value_gt 跨 Int32/Int64/Float64 比较
        let mut idx = SparseIndex::new(3);
        let keys = vec![
            Value::Int32(1),
            Value::Int64(2),
            Value::Int64(3),
            Value::Float64(4.5),
            Value::Int64(10),
        ];
        idx.build_from_keys(&keys);
        assert_eq!(idx.granule_count(), 2);
        assert_eq!(idx.locate_eq(&Value::Int32(2)), Some(0));
        assert_eq!(idx.locate_eq(&Value::Int64(4)), Some(0), "4 < 4.5，落在 granule 0");
        assert_eq!(idx.locate_eq(&Value::Int64(10)), Some(1));
        // 范围跨类型
        let range = idx.locate_range(&Value::Int32(1), &Value::Float64(4.5));
        assert_eq!(range, 0..2);
    }

    #[test]
    fn test_sparse_index_locate_eq_missing_key() {
        // 等值查找不存在的键：返回最后一个 first_key <= key 的 granule（上界语义，
        // 稀疏索引只能回答"可能包含"，不能回答"不存在"）
        let mut idx = SparseIndex::new(4);
        let keys: Vec<Value> = (1..=10u32).map(|i| Value::Int32(i as i32)).collect();
        idx.build_from_keys(&keys);
        // 5 不存在于 first_key（first_keys = 1,5,9），但可能存在于 granule 0..=1
        assert_eq!(idx.locate_eq(&Value::Int32(5)), Some(1));
        // 介于 first_key 之间的值：落在前一 granule
        assert_eq!(idx.locate_eq(&Value::Int32(6)), Some(1));
        assert_eq!(idx.locate_eq(&Value::Int32(8)), Some(1));
        // 小于所有 first_key：granule 0
        assert_eq!(idx.locate_eq(&Value::Int32(0)), Some(0));
    }

    #[test]
    fn test_sparse_index_range_inside_gap() {
        // 范围完全落在区间间隙（[5-8] 之后、[9-10] 之前不存在）：返回空范围的相邻区间
        let mut idx = SparseIndex::new(4);
        let keys: Vec<Value> = (1..=10u32).map(|i| Value::Int32(i as i32)).collect();
        idx.build_from_keys(&keys);
        // first_keys = 1, 5, 9
        let range = idx.locate_range(&Value::Int32(6), &Value::Int32(8));
        assert_eq!(range, 1..2, "间隙范围应收缩到相邻 granule 1");
        let range = idx.locate_range(&Value::Int32(8), &Value::Int32(9));
        assert_eq!(range, 1..3, "high=9 与 first_key=9 相等：保守包含该 granule");
    }

    #[test]
    fn test_sparse_index_duplicate_first_keys() {
        // 重复键：find_first_gt 二分在全部相等时正确
        let mut idx = SparseIndex::new(2);
        let keys = vec![
            Value::Int32(7), Value::Int32(7),
            Value::Int32(7), Value::Int32(8),
            Value::Int32(8),
        ];
        idx.build_from_keys(&keys);
        assert_eq!(idx.granule_count(), 3);
        assert_eq!(idx.locate_eq(&Value::Int32(7)), Some(1), "最后一个 first_key<=7 的 granule");
        assert_eq!(idx.locate_eq(&Value::Int32(8)), Some(2));
        // 全部键都小于 target：返回最后一个 granule
        assert_eq!(idx.locate_eq(&Value::Int32(100)), Some(2));
    }
}
