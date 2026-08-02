//! 索引模块
//!
//! 面向分析型工作负载的索引体系：
//! - 跳表索引 (SkipListIndex): 有序二级索引，支持范围查询
//! - 位图索引 (BitmapIndex): 低基数列加速，支持 AND/OR 位运算
//! - 布隆过滤器 (BloomFilter): 存在性快速判断，点查询加速
//!
//! 设计原则：索引为分析查询服务，不追求 OLTP 级别的写入性能，
//! 而是在批量导入后构建，加速后续的交互式分析查询。
//!
//! 定位：专用分析型嵌入 AI Agent 数据引擎

pub mod skiplist;
pub mod bitmap;
pub mod bloom;

pub use skiplist::SkipListIndex;
pub use bitmap::BitmapIndex;
pub use bloom::BloomFilter;

use crate::Value;

/// 索引类型
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexType {
    /// 跳表索引（有序，支持范围查询）
    SkipList,
    /// 位图索引（低基数列，支持位运算）
    Bitmap,
    /// 布隆过滤器（存在性快速判断）
    Bloom,
}

impl IndexType {
    pub fn name(&self) -> &'static str {
        match self {
            IndexType::SkipList => "skiplist",
            IndexType::Bitmap => "bitmap",
            IndexType::Bloom => "bloom",
        }
    }

    /// 根据列基数推荐索引类型
    pub fn recommend(cardinality: usize, total_rows: usize) -> Self {
        if total_rows == 0 {
            return IndexType::SkipList;
        }
        let ratio = cardinality as f64 / total_rows as f64;
        if ratio < 0.05 {
            // 基数 < 5%，位图索引最优
            IndexType::Bitmap
        } else if ratio < 0.5 {
            // 中等基数，跳表索引
            IndexType::SkipList
        } else {
            // 高基数，布隆过滤器 + 跳表
            IndexType::SkipList
        }
    }
}

/// 索引定义
#[derive(Debug, Clone)]
pub struct IndexDef {
    /// 索引名称
    pub name: String,
    /// 所属表名
    pub table: String,
    /// 索引列名
    pub column: String,
    /// 索引类型
    pub index_type: IndexType,
    /// 是否唯一索引
    pub unique: bool,
}

/// 索引查询结果（行号集合）
#[derive(Debug, Clone)]
pub enum IndexResult {
    /// 单个行号（点查询命中）
    Single(u32),
    /// 行号范围 [start, end)
    Range(u32, u32),
    /// 行号列表（非连续）
    List(Vec<u32>),
    /// 空结果
    Empty,
}

impl IndexResult {
    pub fn is_empty(&self) -> bool {
        match self {
            IndexResult::Empty => true,
            IndexResult::Range(s, e) => s >= e,
            IndexResult::List(v) => v.is_empty(),
            _ => false,
        }
    }

    pub fn len(&self) -> u64 {
        match self {
            IndexResult::Single(_) => 1,
            IndexResult::Range(s, e) => (*e as u64).saturating_sub(*s as u64),
            IndexResult::List(v) => v.len() as u64,
            IndexResult::Empty => 0,
        }
    }

    pub fn to_vec(&self) -> Vec<u32> {
        match self {
            IndexResult::Single(row) => vec![*row],
            IndexResult::Range(s, e) => (*s..*e).collect(),
            IndexResult::List(v) => v.clone(),
            IndexResult::Empty => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_index_type_names() {
        assert_eq!(IndexType::SkipList.name(), "skiplist");
        assert_eq!(IndexType::Bitmap.name(), "bitmap");
        assert_eq!(IndexType::Bloom.name(), "bloom");
    }

    #[test]
    fn test_index_result_empty() {
        assert!(IndexResult::Empty.is_empty());
        assert!(IndexResult::Range(10, 10).is_empty());
        assert!(IndexResult::List(vec![]).is_empty());
        assert!(!IndexResult::Single(5).is_empty());
        assert!(!IndexResult::Range(0, 10).is_empty());
    }

    #[test]
    fn test_index_result_len() {
        assert_eq!(IndexResult::Empty.len(), 0);
        assert_eq!(IndexResult::Single(42).len(), 1);
        assert_eq!(IndexResult::Range(0, 100).len(), 100);
        assert_eq!(IndexResult::List(vec![1, 2, 3]).len(), 3);
    }

    #[test]
    fn test_index_result_to_vec() {
        assert_eq!(IndexResult::Single(5).to_vec(), vec![5]);
        assert_eq!(IndexResult::Range(0, 3).to_vec(), vec![0, 1, 2]);
        assert_eq!(IndexResult::List(vec![10, 20]).to_vec(), vec![10, 20]);
        assert!(IndexResult::Empty.to_vec().is_empty());
    }

    #[test]
    fn test_index_def_creation() {
        let def = IndexDef {
            name: "idx_cat".to_string(),
            table: "users".to_string(),
            column: "category".to_string(),
            index_type: IndexType::Bitmap,
            unique: false,
        };
        assert_eq!(def.name, "idx_cat");
        assert_eq!(def.table, "users");
        assert_eq!(def.column, "category");
        assert_eq!(def.index_type, IndexType::Bitmap);
        assert!(!def.unique);
    }

    #[test]
    fn test_index_type_equality() {
        assert_eq!(IndexType::SkipList, IndexType::SkipList);
        assert_ne!(IndexType::SkipList, IndexType::Bitmap);
        assert_ne!(IndexType::Bitmap, IndexType::Bloom);
    }

    #[test]
    fn test_index_result_range_empty_when_start_ge_end() {
        assert!(IndexResult::Range(5, 5).is_empty());
        assert!(IndexResult::Range(10, 5).is_empty());
        assert_eq!(IndexResult::Range(10, 5).len(), 0);
    }

    #[test]
    fn test_index_result_list_empty() {
        let empty: Vec<u32> = vec![];
        assert!(IndexResult::List(empty.clone()).is_empty());
        assert_eq!(IndexResult::List(empty).len(), 0);
    }

    #[test]
    fn test_recommend_index_type() {
        // 极低基数 → 位图
        assert_eq!(IndexType::recommend(10, 10000), IndexType::Bitmap);
        // 中等基数 → 跳表
        assert_eq!(IndexType::recommend(2000, 10000), IndexType::SkipList);
        // 高基数 → 跳表
        assert_eq!(IndexType::recommend(8000, 10000), IndexType::SkipList);
        // 空表 → 跳表（默认）
        assert_eq!(IndexType::recommend(0, 0), IndexType::SkipList);
    }
}
