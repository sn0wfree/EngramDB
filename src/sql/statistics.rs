//! 统计信息模块（CBO 基础）
//!
//! 收集表和列的统计数据，供代价优化器估算执行计划的代价。
//!
//! 统计信息类型：
//! - 表级：行数、页面数
//! - 列级：NDV（不同值数量）、空值率、Min/Max、直方图
//! - 简单统计即可支撑大部分优化决策（PostgreSQL 风格）

use crate::Value;
use crate::executor::vector::{DataChunk, Vector};

/// 表级统计信息
#[derive(Debug, Clone)]
pub struct TableStatistics {
    /// 表名
    pub table_name: String,
    /// 引擎（M5：JOIN 代价模型按引擎加权扫描成本）
    pub engine: crate::common::types::EngineType,
    /// 总行数
    pub row_count: u64,
    /// 列统计
    pub columns: Vec<ColumnStatistics>,
}

/// 列级统计信息
#[derive(Debug, Clone)]
pub struct ColumnStatistics {
    /// 列名
    pub column_name: String,
    /// 不同值数量（Number of Distinct Values）
    pub ndv: u64,
    /// 空值数量
    pub null_count: u64,
    /// 最小值
    pub min_value: Option<Value>,
    /// 最大值
    pub max_value: Option<Value>,
    /// 等宽直方图（简化版：每个桶的计数）
    pub histogram: Option<Histogram>,
}

/// 等宽直方图
///
/// 简化实现：将 [min, max] 区间均分为 N 个桶，记录每个桶的行数。
/// 用于估算范围查询的选择性。
#[derive(Debug, Clone)]
pub struct Histogram {
    pub buckets: Vec<u64>,
    pub min: f64,
    pub max: f64,
    pub bucket_width: f64,
}

impl Histogram {
    /// 从数据构建等宽直方图
    pub fn from_values(values: &[Value], num_buckets: usize) -> Option<Self> {
        if values.is_empty() || num_buckets == 0 {
            return None;
        }

        // 只对数值类型构建直方图
        let numeric_values: Vec<f64> = values.iter()
            .filter_map(|v| v.as_f64())
            .collect();

        if numeric_values.is_empty() {
            return None;
        }

        let min = *numeric_values.iter().min_by(|a, b| a.partial_cmp(b).unwrap())?;
        let max = *numeric_values.iter().max_by(|a, b| a.partial_cmp(b).unwrap())?;

        if min == max {
            // 所有值相同，一个桶就行
            return Some(Histogram {
                buckets: vec![numeric_values.len() as u64],
                min,
                max,
                bucket_width: 1.0,
            });
        }

        let bucket_width = (max - min) / num_buckets as f64;
        let mut buckets = vec![0u64; num_buckets];

        for &val in &numeric_values {
            let mut idx = ((val - min) / bucket_width) as usize;
            if idx >= num_buckets {
                idx = num_buckets - 1; // 边界值放入最后一个桶
            }
            buckets[idx] += 1;
        }

        Some(Histogram {
            buckets,
            min,
            max,
            bucket_width,
        })
    }

    /// 估算范围查询 [low, high] 的行数
    pub fn estimate_range(&self, low: f64, high: f64) -> u64 {
        if high < self.min || low > self.max {
            return 0;
        }

        let clamped_low = low.max(self.min);
        let clamped_high = high.min(self.max);

        let start_bucket = ((clamped_low - self.min) / self.bucket_width) as usize;
        let end_bucket = ((clamped_high - self.min) / self.bucket_width) as usize;

        let num_buckets = self.buckets.len();
        if start_bucket >= num_buckets {
            return 0;
        }

        let mut total = 0u64;

        for i in start_bucket..=end_bucket.min(num_buckets - 1) {
            let bucket_start = self.min + (i as f64) * self.bucket_width;
            let bucket_end = bucket_start + self.bucket_width;

            let overlap_start = clamped_low.max(bucket_start);
            let overlap_end = clamped_high.min(bucket_end);

            if overlap_end > overlap_start && self.bucket_width > 0.0 {
                let fraction = (overlap_end - overlap_start) / self.bucket_width;
                total += (self.buckets[i] as f64 * fraction) as u64;
            }
        }

        total
    }
}

impl TableStatistics {
    /// 从数据块收集统计信息
    pub fn from_chunks(
        table_name: &str,
        engine: crate::common::types::EngineType,
        column_names: &[String],
        chunks: &[DataChunk],
        with_histogram: bool,
    ) -> Self {
        let total_rows: u64 = chunks.iter().map(|c| c.count as u64).sum();
        let num_cols = column_names.len();

        let mut columns = Vec::with_capacity(num_cols);

        for col_idx in 0..num_cols {
            // 收集该列的所有值
            let mut all_values: Vec<Value> = Vec::new();
            for chunk in chunks {
                if col_idx < chunk.columns.len() {
                    all_values.extend(chunk.columns[col_idx].to_flat());
                }
            }

            let col_stats = ColumnStatistics::from_values(
                &column_names[col_idx],
                &all_values,
                with_histogram,
            );
            columns.push(col_stats);
        }

        TableStatistics {
            table_name: table_name.to_string(),
            engine,
            row_count: total_rows,
            columns,
        }
    }
}

impl ColumnStatistics {
    /// 从值列表计算列统计
    pub fn from_values(column_name: &str, values: &[Value], with_histogram: bool) -> Self {
        let total = values.len() as u64;
        let null_count = values.iter().filter(|v| v.is_null()).count() as u64;

        let non_null: Vec<&Value> = values.iter().filter(|v| !v.is_null()).collect();
        let ndv = count_distinct(&non_null);

        // Min/Max
        let (min_val, max_val) = if non_null.is_empty() {
            (None, None)
        } else {
            let mut min = non_null[0];
            let mut max = non_null[0];
            for v in &non_null {
                if value_less(v, min) {
                    min = v;
                }
                if value_greater(v, max) {
                    max = v;
                }
            }
            (Some(min.clone()), Some(max.clone()))
        };

        // 直方图
        let histogram = if with_histogram {
            let owned: Vec<Value> = non_null.into_iter().cloned().collect();
            Histogram::from_values(&owned, 10) // 默认 10 个桶
        } else {
            None
        };

        ColumnStatistics {
            column_name: column_name.to_string(),
            ndv,
            null_count,
            min_value: min_val,
            max_value: max_val,
            histogram,
        }
    }

    /// 空值率（0.0 ~ 1.0）
    pub fn null_fraction(&self, total_rows: u64) -> f64 {
        if total_rows == 0 {
            0.0
        } else {
            self.null_count as f64 / total_rows as f64
        }
    }

    /// 估算等值比较的选择性（= 值的行数 / 总行数）
    pub fn estimate_eq_selectivity(&self, total_rows: u64, _value: &Value) -> f64 {
        if total_rows == 0 || self.ndv == 0 {
            return 0.0;
        }
        // 均匀分布假设：1 / NDV
        let non_null_rows = (total_rows - self.null_count) as f64;
        let eq_rows = non_null_rows / self.ndv as f64;
        eq_rows / total_rows as f64
    }

    /// 估算范围比较的选择性
    pub fn estimate_range_selectivity(&self, total_rows: u64, low: f64, high: f64) -> f64 {
        if total_rows == 0 {
            return 0.0;
        }

        // 优先用直方图
        if let Some(hist) = &self.histogram {
            let estimated = hist.estimate_range(low, high);
            return estimated as f64 / total_rows as f64;
        }

        // 回退到均匀分布假设
        if let (Some(min_v), Some(max_v)) = (&self.min_value, &self.max_value) {
            if let (Some(min_f), Some(max_f)) = (min_v.as_f64(), max_v.as_f64()) {
                if max_f > min_f {
                    let clamped_low = low.max(min_f);
                    let clamped_high = high.min(max_f);
                    if clamped_high > clamped_low {
                        let range_frac = (clamped_high - clamped_low) / (max_f - min_f);
                        let non_null_frac = (total_rows - self.null_count) as f64 / total_rows as f64;
                        return range_frac * non_null_frac;
                    }
                }
            }
        }

        0.33 // 默认估计：1/3（经典数据库默认值）
    }
}

/// 计算不同值的数量（近似：使用简单哈希去重）
fn count_distinct(values: &[&Value]) -> u64 {
    use std::collections::HashSet;
    let mut set = HashSet::new();
    for v in values {
        set.insert(*v as *const Value); // 用指针地址近似，实际应比较值
    }
    // 上面的方法不对，改用值比较
    let mut unique: Vec<&Value> = Vec::new();
    for v in values {
        if !unique.iter().any(|u| value_eq(u, v)) {
            unique.push(v);
        }
        // 限制计算量：超过 1000 个不同值就停止精确计数
        if unique.len() > 1000 {
            break;
        }
    }
    unique.len() as u64
}

fn value_eq(a: &Value, b: &Value) -> bool {
    a == b
}

fn value_less(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Int32(x), Value::Int32(y)) => x < y,
        (Value::Int64(x), Value::Int64(y)) => x < y,
        (Value::Float64(x), Value::Float64(y)) => x < y,
        (Value::Varchar(x), Value::Varchar(y)) => x < y,
        (Value::Boolean(x), Value::Boolean(y)) => x < y,
        _ => false,
    }
}

fn value_greater(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Int32(x), Value::Int32(y)) => x > y,
        (Value::Int64(x), Value::Int64(y)) => x > y,
        (Value::Float64(x), Value::Float64(y)) => x > y,
        (Value::Varchar(x), Value::Varchar(y)) => x > y,
        (Value::Boolean(x), Value::Boolean(y)) => x > y,
        _ => false,
    }
}

// ============================================================================
// 测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::vector::{Vector, DataChunk};

    fn make_test_chunk() -> DataChunk {
        let ids = Vector::Flat(vec![
            Value::Int64(1), Value::Int64(2), Value::Int64(3),
            Value::Int64(4), Value::Int64(5), Value::Int64(6),
            Value::Int64(7), Value::Int64(8), Value::Int64(9),
            Value::Int64(10),
        ]);
        let names = Vector::Flat(vec![
            Value::Varchar("a".into()),
            Value::Varchar("b".into()),
            Value::Varchar("a".into()),
            Value::Varchar("c".into()),
            Value::Varchar("b".into()),
            Value::Varchar("a".into()),
            Value::Varchar("d".into()),
            Value::Varchar("e".into()),
            Value::Varchar("a".into()),
            Value::Varchar("f".into()),
        ]);
        DataChunk { columns: vec![ids, names], count: 10 }
    }

    #[test]
    fn test_table_stats_basic() {
        let chunk = make_test_chunk();
        let cols = vec!["id".to_string(), "name".to_string()];
        let stats = TableStatistics::from_chunks(
            "test", crate::common::types::EngineType::Columnar, &cols, &[chunk], false
        );

        assert_eq!(stats.row_count, 10);
        assert_eq!(stats.columns.len(), 2);

        // id 列：10 个不同值，0 个 NULL
        assert_eq!(stats.columns[0].ndv, 10);
        assert_eq!(stats.columns[0].null_count, 0);
    }

    #[test]
    fn test_column_stats_ndv() {
        let chunk = make_test_chunk();
        let cols = vec!["id".to_string(), "name".to_string()];
        let stats = TableStatistics::from_chunks(
            "test", crate::common::types::EngineType::Columnar, &cols, &[chunk], false
        );

        // name 列：a,b,c,d,e,f = 6 个不同值
        assert_eq!(stats.columns[1].ndv, 6);
    }

    #[test]
    fn test_histogram_range() {
        let values: Vec<Value> = (1..=100).map(|i| Value::Int64(i)).collect();
        let hist = Histogram::from_values(&values, 10).unwrap();

        assert_eq!(hist.buckets.len(), 10);
        // 每个桶应该大约 10 个值
        for &count in &hist.buckets {
            assert_eq!(count, 10);
        }

        // 估算范围 [20, 50] → 约 31 行 (20,21,...,50)
        let estimated = hist.estimate_range(20.0, 50.0);
        assert!(estimated >= 25 && estimated <= 35);
    }

    #[test]
    fn test_histogram_out_of_range() {
        let values: Vec<Value> = (1..=100).map(|i| Value::Int64(i)).collect();
        let hist = Histogram::from_values(&values, 10).unwrap();

        assert_eq!(hist.estimate_range(200.0, 300.0), 0);
        assert_eq!(hist.estimate_range(-100.0, 0.0), 0);
    }

    #[test]
    fn test_null_stats() {
        let values = vec![
            Value::Int64(1), Value::Null, Value::Int64(3),
            Value::Null, Value::Int64(5),
        ];
        let stats = ColumnStatistics::from_values("col", &values, false);

        assert_eq!(stats.null_count, 2);
        assert_eq!(stats.ndv, 3);
    }

    #[test]
    fn test_eq_selectivity() {
        let values: Vec<Value> = (1..=10).map(|i| Value::Int64(i)).collect();
        let stats = ColumnStatistics::from_values("col", &values, false);

        let sel = stats.estimate_eq_selectivity(10, &Value::Int64(5));
        // 10 个不同值 → 选择性 1/10 = 0.1
        assert!((sel - 0.1).abs() < 0.001);
    }
}
