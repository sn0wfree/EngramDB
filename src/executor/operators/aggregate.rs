//! 聚合算子
//!
//! 两阶段聚合架构（借鉴 ClickHouse）：
//! - 第一阶段（Partial）：每个 DataChunk 独立计算部分聚合状态
//! - 第二阶段（Merge）：合并所有 partial state，得到最终结果
//!
//! 支持两种模式：
//! 1. 无 GROUP BY：全表聚合，输出单行结果
//! 2. 有 GROUP BY：基于哈希表的分组聚合
//!
//! 性能优化：
//! - Partial 阶段可线性并行扩展
//! - Merge 阶段 O(N) 其中 N = 线程数 << 数据行数
//! - 哈希分组使用 fxhash 快速哈希

use crate::common::error::Result;
use crate::Value;

use super::super::physical_plan::AggregateFunc;
use super::super::vector::{DataChunk, Vector, VECTOR_SIZE};

use fxhash::{FxHashMap, FxHashSet};

// ============================================================================
// 部分聚合状态（Partial Aggregation State）
// ============================================================================

/// 部分聚合状态
///
/// 两阶段聚合的核心数据结构：
/// - Partial：每个数据块独立计算
/// - Merge：合并多个 partial state
/// - Finalize：得到最终结果
///
/// 支持可交换、可结合的聚合函数都能用此架构。
#[derive(Debug, Clone)]
pub enum PartialAggState {
    Count { count: i64 },
    Sum { sum: f64, has_value: bool },
    Avg { sum: f64, count: i64 },
    Min { value: Option<Value> },
    Max { value: Option<Value> },
}

impl PartialAggState {
    /// 创建初始状态
    pub fn new(func: AggregateFunc) -> Self {
        match func {
            AggregateFunc::Count => PartialAggState::Count { count: 0 },
            AggregateFunc::Sum => PartialAggState::Sum { sum: 0.0, has_value: false },
            AggregateFunc::Avg => PartialAggState::Avg { sum: 0.0, count: 0 },
            AggregateFunc::Min => PartialAggState::Min { value: None },
            AggregateFunc::Max => PartialAggState::Max { value: None },
        }
    }

    /// 累积一个值到状态
    pub fn accumulate(&mut self, val: &Value) {
        match self {
            PartialAggState::Count { count } => {
                if !val.is_null() {
                    *count += 1;
                }
            }
            PartialAggState::Sum { sum, has_value } => {
                if let Some(v) = val.as_f64() {
                    *sum += v;
                    *has_value = true;
                }
            }
            PartialAggState::Avg { sum, count } => {
                if let Some(v) = val.as_f64() {
                    *sum += v;
                    *count += 1;
                }
            }
            PartialAggState::Min { value } => {
                if val.is_null() {
                    return;
                }
                match value {
                    None => *value = Some(val.clone()),
                    Some(m) => {
                        if value_less(val, m) {
                            *value = Some(val.clone());
                        }
                    }
                }
            }
            PartialAggState::Max { value } => {
                if val.is_null() {
                    return;
                }
                match value {
                    None => *value = Some(val.clone()),
                    Some(m) => {
                        if value_greater(val, m) {
                            *value = Some(val.clone());
                        }
                    }
                }
            }
        }
    }

    /// 合并另一个 partial state（可交换、可结合）
    pub fn merge(&mut self, other: &PartialAggState) {
        match (self, other) {
            (PartialAggState::Count { count }, PartialAggState::Count { count: other_count }) => {
                *count += other_count;
            }
            (
                PartialAggState::Sum { sum, has_value },
                PartialAggState::Sum { sum: other_sum, has_value: other_hv },
            ) => {
                *sum += other_sum;
                *has_value = *has_value || *other_hv;
            }
            (
                PartialAggState::Avg { sum, count },
                PartialAggState::Avg { sum: other_sum, count: other_count },
            ) => {
                *sum += other_sum;
                *count += other_count;
            }
            (PartialAggState::Min { value }, PartialAggState::Min { value: other_val }) => {
                if let Some(other_v) = other_val {
                    match value {
                        None => *value = Some(other_v.clone()),
                        Some(m) => {
                            if value_less(other_v, m) {
                                *value = Some(other_v.clone());
                            }
                        }
                    }
                }
            }
            (PartialAggState::Max { value }, PartialAggState::Max { value: other_val }) => {
                if let Some(other_v) = other_val {
                    match value {
                        None => *value = Some(other_v.clone()),
                        Some(m) => {
                            if value_greater(other_v, m) {
                                *value = Some(other_v.clone());
                            }
                        }
                    }
                }
            }
            _ => {} // 类型不匹配，忽略
        }
    }

    /// 最终化：从 partial state 得到最终值
    pub fn finalize(self) -> Value {
        match self {
            PartialAggState::Count { count } => Value::Int64(count),
            PartialAggState::Sum { sum, has_value } => {
                if has_value {
                    Value::Float64(sum)
                } else {
                    Value::Null
                }
            }
            PartialAggState::Avg { sum, count } => {
                if count > 0 {
                    Value::Float64(sum / count as f64)
                } else {
                    Value::Null
                }
            }
            PartialAggState::Min { value } => value.unwrap_or(Value::Null),
            PartialAggState::Max { value } => value.unwrap_or(Value::Null),
        }
    }
}

// ============================================================================
// 无 GROUP BY 简单聚合
// ============================================================================

/// 执行简单聚合（无 GROUP BY）
///
/// 使用两阶段聚合：每个 chunk 先 partial，再 merge。
/// 对于 DISTINCT 聚合，使用 HashSet 去重路径。
pub fn execute(
    input: &[DataChunk],
    aggregates: &[(AggregateFunc, usize, bool)],
) -> Result<Vec<DataChunk>> {
    if input.is_empty() {
        return Ok(vec![]);
    }

    let mut results: Vec<Value> = Vec::with_capacity(aggregates.len());

    for (func, col_idx, distinct) in aggregates {
        if *distinct {
            // DISTINCT 路径：收集唯一值后聚合
            let mut seen: FxHashSet<Value> = FxHashSet::default();
            for chunk in input {
                if *col_idx >= chunk.columns.len() {
                    continue;
                }
                let col = &chunk.columns[*col_idx];
                for i in 0..chunk.count {
                    let val = col.get(i);
                    if !val.is_null() {
                        seen.insert(val.clone());
                    }
                }
            }
            let mut merged = PartialAggState::new(*func);
            for val in &seen {
                merged.accumulate(val);
            }
            results.push(merged.finalize());
        } else {
            // 第一阶段：每个 chunk 计算 partial
            let mut merged = PartialAggState::new(*func);
            for chunk in input {
                let partial = aggregate_chunk_partial(chunk, *func, *col_idx);
                merged.merge(&partial);
            }
            // 第二阶段：finalize
            results.push(merged.finalize());
        }
    }

    let chunk = DataChunk::from_rows(&[results]);
    Ok(vec![chunk])
}

/// 对单个 DataChunk 计算部分聚合
fn aggregate_chunk_partial(chunk: &DataChunk, func: AggregateFunc, col_idx: usize) -> PartialAggState {
    let mut state = PartialAggState::new(func);

    if col_idx >= chunk.columns.len() {
        return state;
    }

    let col = &chunk.columns[col_idx];
    for i in 0..chunk.count {
        state.accumulate(col.get(i));
    }

    state
}

// ============================================================================
// GROUP BY 分组聚合
// ============================================================================

/// 执行分组聚合（带 GROUP BY）
///
/// 使用哈希表分组，每个组维护一组聚合状态。
/// 同样采用两阶段架构：
/// - Partial：每个 chunk 独立构建哈希表（可并行）
/// - Merge：合并所有 chunk 的哈希表
///
/// group_by: 分组列索引
/// aggregates: 聚合函数定义（函数 + 输入列索引）
pub fn execute_grouped(
    input: &[DataChunk],
    group_by: &[usize],
    aggregates: &[(AggregateFunc, usize, bool)],
) -> Result<Vec<DataChunk>> {
    if input.is_empty() {
        return Ok(vec![]);
    }

    // 检查是否有 DISTINCT 聚合
    let has_distinct = aggregates.iter().any(|(_, _, d)| *d);

    if has_distinct {
        return execute_grouped_distinct(input, group_by, aggregates);
    }

    // 第一阶段：每个 chunk 计算 partial 哈希表
    let mut merged_map: FxHashMap<Vec<Value>, Vec<PartialAggState>> = FxHashMap::default();

    for chunk in input {
        let partial_map = aggregate_chunk_grouped_partial(chunk, group_by, aggregates);
        merge_grouped_map(&mut merged_map, partial_map, aggregates.len());
    }

    // 第二阶段：finalize 所有组，转换为 DataChunk
    let num_group_cols = group_by.len();
    let num_agg_cols = aggregates.len();
    let total_cols = num_group_cols + num_agg_cols;

    let mut result_chunks = Vec::new();
    let mut rows: Vec<Vec<Value>> = Vec::with_capacity(VECTOR_SIZE);

    for (key, states) in &merged_map {
        let mut row = Vec::with_capacity(total_cols);

        for v in key {
            row.push(v.clone());
        }

        for state in states {
            row.push(state.clone().finalize());
        }

        rows.push(row);
        if rows.len() >= VECTOR_SIZE {
            result_chunks.push(DataChunk::from_rows(&rows));
            rows.clear();
        }
    }

    if !rows.is_empty() {
        result_chunks.push(DataChunk::from_rows(&rows));
    }

    Ok(result_chunks)
}

/// DISTINCT 分组聚合：每组维护独立 HashSet 去重
fn execute_grouped_distinct(
    input: &[DataChunk],
    group_by: &[usize],
    aggregates: &[(AggregateFunc, usize, bool)],
) -> Result<Vec<DataChunk>> {
    let mut group_sets: FxHashMap<Vec<Value>, Vec<Option<FxHashSet<Value>>>> = FxHashMap::default();

    for chunk in input {
        for row_idx in 0..chunk.count {
            let key: Vec<Value> = group_by.iter()
                .map(|&col| {
                    if col < chunk.columns.len() {
                        chunk.columns[col].get(row_idx).clone()
                    } else {
                        Value::Null
                    }
                })
                .collect();

            let sets = group_sets.entry(key).or_insert_with(|| {
                aggregates.iter()
                    .map(|(_, _, d)| if *d { Some(FxHashSet::default()) } else { None })
                    .collect()
            });

            for (agg_idx, (_, col_idx, distinct)) in aggregates.iter().enumerate() {
                if !distinct {
                    continue;
                }
                if *col_idx < chunk.columns.len() {
                    let val = chunk.columns[*col_idx].get(row_idx);
                    if !val.is_null() {
                        if let Some(Some(s)) = sets.get_mut(agg_idx) {
                            s.insert(val.clone());
                        }
                    }
                }
            }
        }
    }

    let num_group_cols = group_by.len();
    let num_agg_cols = aggregates.len();
    let total_cols = num_group_cols + num_agg_cols;

    let mut result_chunks = Vec::new();
    let mut rows: Vec<Vec<Value>> = Vec::with_capacity(VECTOR_SIZE);

    for (key, sets) in &group_sets {
        let mut row = Vec::with_capacity(total_cols);
        for v in key {
            row.push(v.clone());
        }
        for (agg_idx, (func, _, distinct)) in aggregates.iter().enumerate() {
            if *distinct {
                let set = sets[agg_idx].as_ref().unwrap();
                let mut state = PartialAggState::new(*func);
                for val in set {
                    state.accumulate(val);
                }
                row.push(state.finalize());
            } else {
                row.push(Value::Null);
            }
        }
        rows.push(row);
        if rows.len() >= VECTOR_SIZE {
            result_chunks.push(DataChunk::from_rows(&rows));
            rows.clear();
        }
    }

    if !rows.is_empty() {
        result_chunks.push(DataChunk::from_rows(&rows));
    }

    Ok(result_chunks)
}

/// 对单个 DataChunk 计算分组 partial 聚合
fn aggregate_chunk_grouped_partial(
    chunk: &DataChunk,
    group_by: &[usize],
    aggregates: &[(AggregateFunc, usize, bool)],
) -> FxHashMap<Vec<Value>, Vec<PartialAggState>> {
    let mut map: FxHashMap<Vec<Value>, Vec<PartialAggState>> = FxHashMap::default();

    for row_idx in 0..chunk.count {
        let key: Vec<Value> = group_by.iter()
            .map(|&col| {
                if col < chunk.columns.len() {
                    chunk.columns[col].get(row_idx).clone()
                } else {
                    Value::Null
                }
            })
            .collect();

        let states = map.entry(key).or_insert_with(|| {
            aggregates.iter()
                .map(|(func, _, _)| PartialAggState::new(*func))
                .collect()
        });

        for (agg_idx, (_, col_idx, _)) in aggregates.iter().enumerate() {
            if *col_idx < chunk.columns.len() {
                let val = chunk.columns[*col_idx].get(row_idx);
                states[agg_idx].accumulate(val);
            }
        }
    }

    map
}

/// 合并两个分组聚合哈希表
fn merge_grouped_map(
    target: &mut FxHashMap<Vec<Value>, Vec<PartialAggState>>,
    source: FxHashMap<Vec<Value>, Vec<PartialAggState>>,
    _num_aggs: usize,
) {
    use std::collections::hash_map::Entry;

    for (key, source_states) in source {
        match target.entry(key) {
            Entry::Vacant(v) => {
                v.insert(source_states);
            }
            Entry::Occupied(mut o) => {
                let target_states = o.get_mut();
                for (t, s) in target_states.iter_mut().zip(source_states.iter()) {
                    t.merge(s);
                }
            }
        }
    }
}

// ============================================================================
// 辅助函数
// ============================================================================

fn value_less(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Int32(x), Value::Int32(y)) => x < y,
        (Value::Int64(x), Value::Int64(y)) => x < y,
        (Value::Float64(x), Value::Float64(y)) => x < y,
        (Value::Varchar(x), Value::Varchar(y)) => x < y,
        _ => false,
    }
}

fn value_greater(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Int32(x), Value::Int32(y)) => x > y,
        (Value::Int64(x), Value::Int64(y)) => x > y,
        (Value::Float64(x), Value::Float64(y)) => x > y,
        (Value::Varchar(x), Value::Varchar(y)) => x > y,
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
        // dept_id, salary
        let dept = Vector::Flat(vec![
            Value::Int64(10), Value::Int64(20), Value::Int64(10),
            Value::Int64(30), Value::Int64(20), Value::Int64(10),
        ]);
        let salary = Vector::Flat(vec![
            Value::Int64(5000), Value::Int64(6000), Value::Int64(5500),
            Value::Int64(7000), Value::Int64(6500), Value::Int64(4500),
        ]);
        DataChunk { columns: vec![dept, salary], count: 6 }
    }

    #[test]
    fn test_simple_count() {
        let chunk = make_test_chunk();
        let result = execute(&[chunk], &[(AggregateFunc::Count, 1, false)]).unwrap();
        assert_eq!(result.len(), 1);
        let rows = result[0].to_rows();
        assert_eq!(rows[0][0], Value::Int64(6));
    }

    #[test]
    fn test_simple_sum() {
        let chunk = make_test_chunk();
        let result = execute(&[chunk], &[(AggregateFunc::Sum, 1, false)]).unwrap();
        let rows = result[0].to_rows();
        // 5000 + 6000 + 5500 + 7000 + 6500 + 4500 = 34500
        match &rows[0][0] {
            Value::Float64(f) => assert!((f - 34500.0).abs() < 0.001),
            _ => panic!("expected Float64"),
        }
    }

    #[test]
    fn test_simple_avg_min_max() {
        let chunk = make_test_chunk();
        let aggs = vec![
            (AggregateFunc::Avg, 1, false),
            (AggregateFunc::Min, 1, false),
            (AggregateFunc::Max, 1, false),
        ];
        let result = execute(&[chunk], &aggs).unwrap();
        let rows = result[0].to_rows();

        // AVG = 34500 / 6 = 5750
        match &rows[0][0] {
            Value::Float64(f) => assert!((f - 5750.0).abs() < 0.001),
            _ => panic!("expected Float64"),
        }
        // MIN = 4500
        match &rows[0][1] {
            Value::Int64(v) => assert_eq!(*v, 4500),
            _ => panic!("expected Int64"),
        }
        // MAX = 7000
        match &rows[0][2] {
            Value::Int64(v) => assert_eq!(*v, 7000),
            _ => panic!("expected Int64"),
        }
    }

    #[test]
    fn test_grouped_count() {
        let chunk = make_test_chunk();
        let result = execute_grouped(
            &[chunk],
            &[0],  // GROUP BY dept_id
            &[(AggregateFunc::Count, 1, false)],
        ).unwrap();

        let total_rows: usize = result.iter().map(|c| c.count).sum();
        assert_eq!(total_rows, 3); // 3 个部门

        // 收集结果
        let all_rows: Vec<_> = result.iter().flat_map(|c| c.to_rows()).collect();

        // 部门 10: 3 人
        let dept10 = all_rows.iter().find(|r| r[0] == Value::Int64(10)).unwrap();
        assert_eq!(dept10[1], Value::Int64(3));

        // 部门 20: 2 人
        let dept20 = all_rows.iter().find(|r| r[0] == Value::Int64(20)).unwrap();
        assert_eq!(dept20[1], Value::Int64(2));

        // 部门 30: 1 人
        let dept30 = all_rows.iter().find(|r| r[0] == Value::Int64(30)).unwrap();
        assert_eq!(dept30[1], Value::Int64(1));
    }

    #[test]
    fn test_grouped_sum_avg() {
        let chunk = make_test_chunk();
        let result = execute_grouped(
            &[chunk],
            &[0],
            &[(AggregateFunc::Sum, 1, false), (AggregateFunc::Avg, 1, false)],
        ).unwrap();

        let all_rows: Vec<_> = result.iter().flat_map(|c| c.to_rows()).collect();

        // 部门 10: sum=5000+5500+4500=15000, avg=5000
        let dept10 = all_rows.iter().find(|r| r[0] == Value::Int64(10)).unwrap();
        match &dept10[1] {
            Value::Float64(f) => assert!((f - 15000.0).abs() < 0.001),
            _ => panic!("expected Float64"),
        }
        match &dept10[2] {
            Value::Float64(f) => assert!((f - 5000.0).abs() < 0.001),
            _ => panic!("expected Float64"),
        }
    }

    #[test]
    fn test_grouped_multi_chunk() {
        // 两个 chunk，验证两阶段合并
        let chunk1 = make_test_chunk();
        let chunk2 = DataChunk {
            columns: vec![
                Vector::Flat(vec![Value::Int64(10), Value::Int64(20), Value::Int64(40)]),
                Vector::Flat(vec![Value::Int64(8000), Value::Int64(9000), Value::Int64(10000)]),
            ],
            count: 3,
        };

        let result = execute_grouped(
            &[chunk1, chunk2],
            &[0],
            &[(AggregateFunc::Count, 1, false), (AggregateFunc::Sum, 1, false)],
        ).unwrap();

        let all_rows: Vec<_> = result.iter().flat_map(|c| c.to_rows()).collect();
        assert_eq!(all_rows.len(), 4); // 4 个部门 (10, 20, 30, 40)

        // 部门 10: 3+1=4 人, sum=15000+8000=23000
        let dept10 = all_rows.iter().find(|r| r[0] == Value::Int64(10)).unwrap();
        assert_eq!(dept10[1], Value::Int64(4));
        match &dept10[2] {
            Value::Float64(f) => assert!((f - 23000.0).abs() < 0.001),
            _ => panic!("expected Float64"),
        }
    }

    #[test]
    fn test_grouped_with_null() {
        let chunk = DataChunk {
            columns: vec![
                Vector::Flat(vec![
                    Value::Int64(1), Value::Int64(1), Value::Null,
                    Value::Int64(2), Value::Null,
                ]),
                Vector::Flat(vec![
                    Value::Int64(10), Value::Int64(20), Value::Int64(30),
                    Value::Int64(40), Value::Int64(50),
                ]),
            ],
            count: 5,
        };

        let result = execute_grouped(
            &[chunk],
            &[0],
            &[(AggregateFunc::Count, 1, false)],
        ).unwrap();

        let all_rows: Vec<_> = result.iter().flat_map(|c| c.to_rows()).collect();

        // NULL 组：2 行
        let null_group = all_rows.iter().find(|r| r[0] == Value::Null).unwrap();
        assert_eq!(null_group[1], Value::Int64(2));

        // 组 1：2 行
        let g1 = all_rows.iter().find(|r| r[0] == Value::Int64(1)).unwrap();
        assert_eq!(g1[1], Value::Int64(2));
    }

    #[test]
    fn test_empty_input() {
        let result = execute_grouped(
            &[],
            &[0],
            &[(AggregateFunc::Count, 1, false)],
        ).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_count_distinct() {
        let chunk = DataChunk {
            columns: vec![
                Vector::Flat(vec![
                    Value::Int64(10), Value::Int64(20), Value::Int64(10),
                    Value::Int64(30), Value::Int64(20), Value::Int64(10),
                ]),
            ],
            count: 6,
        };
        let result = execute(&[chunk], &[(AggregateFunc::Count, 0, true)]).unwrap();
        let rows = result[0].to_rows();
        assert_eq!(rows[0][0], Value::Int64(3));
    }

    #[test]
    fn test_count_distinct_all_null() {
        let chunk = DataChunk {
            columns: vec![
                Vector::Flat(vec![
                    Value::Null, Value::Null, Value::Null,
                ]),
            ],
            count: 3,
        };
        let result = execute(&[chunk], &[(AggregateFunc::Count, 0, true)]).unwrap();
        let rows = result[0].to_rows();
        assert_eq!(rows[0][0], Value::Int64(0));
    }
}
