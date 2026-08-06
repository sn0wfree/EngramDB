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
use crate::common::value_cmp::total_cmp;
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
        // SQL 语义：无 GROUP BY 聚合在空输入下仍返回一行
        // （COUNT → 0，SUM/AVG/MIN/MAX → NULL——finalize 空态即正确值）
        let chunk = DataChunk {
            columns: aggregates
                .iter()
                .map(|(func, _, _)| Vector::Constant(PartialAggState::new(*func).finalize(), 1))
                .collect(),
            count: 1,
        };
        return Ok(vec![chunk]);
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
    // S2-M3：Typed 数值列快路径（直接数组循环，零 Value 构造）
    if let Some(agg) = aggregate_typed_partial(col, func, chunk.count) {
        return agg;
    }
    for i in 0..chunk.count {
        state.accumulate(&col.get(i));
    }

    state
}

/// S2-M3：Typed 列聚合快路径（Int64/Float64 直接数组循环）
fn aggregate_typed_partial(col: &Vector, func: AggregateFunc, count: usize) -> Option<PartialAggState> {
    use crate::common::column_data::ColumnValue;
    let data = col.as_typed()?;
    let null_at = |i: usize| data.nulls.as_ref().map_or(false, |n| n.test(i));
    match (&data.values, func) {
        (ColumnValue::Int64(v), AggregateFunc::Sum) => {
            let mut sum = 0.0f64;
            let mut has = false;
            for i in 0..v.len().min(count) {
                if !null_at(i) {
                    sum += v[i] as f64;
                    has = true;
                }
            }
            Some(PartialAggState::Sum { sum, has_value: has })
        }
        (ColumnValue::Int64(v), AggregateFunc::Avg) => {
            let mut sum = 0.0f64;
            let mut cnt = 0i64;
            for i in 0..v.len().min(count) {
                if !null_at(i) {
                    sum += v[i] as f64;
                    cnt += 1;
                }
            }
            Some(PartialAggState::Avg { sum, count: cnt })
        }
        (ColumnValue::Int64(v), AggregateFunc::Count) => {
            let mut cnt = 0i64;
            for i in 0..v.len().min(count) {
                if !null_at(i) {
                    cnt += 1;
                }
            }
            Some(PartialAggState::Count { count: cnt })
        }
        (ColumnValue::Int64(v), AggregateFunc::Min) => {
            let mut min: Option<i64> = None;
            for i in 0..v.len().min(count) {
                if !null_at(i) {
                    min = Some(min.map_or(v[i], |m| m.min(v[i])));
                }
            }
            Some(PartialAggState::Min { value: min.map(Value::Int64) })
        }
        (ColumnValue::Int64(v), AggregateFunc::Max) => {
            let mut max: Option<i64> = None;
            for i in 0..v.len().min(count) {
                if !null_at(i) {
                    max = Some(max.map_or(v[i], |m| m.max(v[i])));
                }
            }
            Some(PartialAggState::Max { value: max.map(Value::Int64) })
        }
        (ColumnValue::Float64(v), AggregateFunc::Sum) => {
            let mut sum = 0.0f64;
            let mut has = false;
            for i in 0..v.len().min(count) {
                if !null_at(i) {
                    sum += v[i];
                    has = true;
                }
            }
            Some(PartialAggState::Sum { sum, has_value: has })
        }
        (ColumnValue::Float64(v), AggregateFunc::Avg) => {
            let mut sum = 0.0f64;
            let mut cnt = 0i64;
            for i in 0..v.len().min(count) {
                if !null_at(i) {
                    sum += v[i];
                    cnt += 1;
                }
            }
            Some(PartialAggState::Avg { sum, count: cnt })
        }
        (ColumnValue::Float64(v), AggregateFunc::Count) => {
            let mut cnt = 0i64;
            for i in 0..v.len().min(count) {
                if !null_at(i) {
                    cnt += 1;
                }
            }
            Some(PartialAggState::Count { count: cnt })
        }
        (ColumnValue::Float64(v), AggregateFunc::Min) => {
            let mut min: Option<f64> = None;
            for i in 0..v.len().min(count) {
                if !null_at(i) {
                    min = Some(min.map_or(v[i], |m| m.min(v[i])));
                }
            }
            Some(PartialAggState::Min { value: min.map(Value::Float64) })
        }
        (ColumnValue::Float64(v), AggregateFunc::Max) => {
            let mut max: Option<f64> = None;
            for i in 0..v.len().min(count) {
                if !null_at(i) {
                    max = Some(max.map_or(v[i], |m| m.max(v[i])));
                }
            }
            Some(PartialAggState::Max { value: max.map(Value::Float64) })
        }
        _ => None,
    }
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

    // S1.2：单列整数键快速路径（FxHashMap<i64, …>，NULL 组单独跟踪）
    if group_by.len() == 1 {
        if let Some(result) = try_execute_grouped_int(input, group_by[0], aggregates) {
            return Ok(result);
        }
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

/// 分组键的整数类型（S1.2）
#[derive(Debug, Clone, Copy, PartialEq)]
enum GroupKeyType {
    Int32,
    Int64,
    Boolean,
    Timestamp,
}

fn detect_key_type(v: &Value) -> Option<GroupKeyType> {
    match v {
        Value::Int32(_) => Some(GroupKeyType::Int32),
        Value::Int64(_) => Some(GroupKeyType::Int64),
        Value::Boolean(_) => Some(GroupKeyType::Boolean),
        Value::Timestamp(_) => Some(GroupKeyType::Timestamp),
        _ => None,
    }
}

/// 整数键 → i64（类型不匹配返回 None，触发 fallback）
fn key_to_i64(v: &Value, t: GroupKeyType) -> Option<i64> {
    match (t, v) {
        (GroupKeyType::Int32, Value::Int32(x)) => Some(*x as i64),
        (GroupKeyType::Int64, Value::Int64(x)) => Some(*x),
        (GroupKeyType::Boolean, Value::Boolean(b)) => Some(if *b { 1 } else { 0 }),
        (GroupKeyType::Timestamp, Value::Timestamp(x)) => Some(*x),
        _ => None,
    }
}

/// S1.2：单列整数键分组聚合快速路径。
///
/// 分组列全部为 Int32/Int64/Boolean/Timestamp（允许 NULL）时，
/// 用 `FxHashMap<i64, Vec<PartialAggState>>` 替代 `FxHashMap<Vec<Value>, …>`：
/// - key 8 字节寄存器级，hash 单指令，比较单条 cmp，无堆分配
/// - NULL 组单独跟踪（不占用哨兵，避免真实键冲突）
/// - 混合类型列 / 非整数列 → 返回 None，调用方走原路径（保持旧语义）
fn try_execute_grouped_int(
    input: &[DataChunk],
    group_col: usize,
    aggregates: &[(AggregateFunc, usize, bool)],
) -> Option<Vec<DataChunk>> {
    let mut key_type: Option<GroupKeyType> = None;
    let mut merged: FxHashMap<i64, Vec<PartialAggState>> = FxHashMap::default();
    let mut merged_null: Option<Vec<PartialAggState>> = None;

    for chunk in input {
        let mut map: FxHashMap<i64, Vec<PartialAggState>> = FxHashMap::default();
        let mut null_states: Option<Vec<PartialAggState>> = None;

        if group_col < chunk.columns.len() {
            let col = &chunk.columns[group_col];
            for row_idx in 0..chunk.count {
                let v = col.get(row_idx);
                if v.is_null() {
                    ensure_states(&mut null_states, aggregates);
                    let states = null_states.as_mut().unwrap();
                    accumulate_all(states, chunk, row_idx, aggregates);
                    continue;
                }
                let t = match key_type {
                    Some(t) => t,
                    None => {
                        let t = detect_key_type(&v)?;
                        key_type = Some(t);
                        t
                    }
                };
                let k = key_to_i64(&v, t)?;
                let states = map
                    .entry(k)
                    .or_insert_with(|| new_states(aggregates));
                accumulate_all(states, chunk, row_idx, aggregates);
            }
        }

        // 合并进全局表
        use std::collections::hash_map::Entry;
        for (key, source_states) in map {
            match merged.entry(key) {
                Entry::Vacant(v) => {
                    v.insert(source_states);
                }
                Entry::Occupied(mut o) => {
                    for (t, s) in o.get_mut().iter_mut().zip(source_states.iter()) {
                        t.merge(s);
                    }
                }
            }
        }
        if let Some(s) = null_states {
            match &mut merged_null {
                None => merged_null = Some(s),
                Some(t) => {
                    for (t, s) in t.iter_mut().zip(s.iter()) {
                        t.merge(s);
                    }
                }
            }
        }
    }

    // key_type None 且 merged 为空 → 全 NULL / 列缺失，交还原路径处理
    let t = key_type?;

    // 第二阶段：finalize 所有组
    let num_agg_cols = aggregates.len();
    let mut result_chunks = Vec::new();
    let mut rows: Vec<Vec<Value>> = Vec::with_capacity(VECTOR_SIZE);

    for (k, states) in &merged {
        let mut row = Vec::with_capacity(1 + num_agg_cols);
        row.push(int_to_key_value(*k, t));
        for state in states {
            row.push(state.clone().finalize());
        }
        rows.push(row);
        if rows.len() >= VECTOR_SIZE {
            result_chunks.push(DataChunk::from_rows(&rows));
            rows.clear();
        }
    }
    if let Some(states) = &merged_null {
        let mut row = Vec::with_capacity(1 + num_agg_cols);
        row.push(Value::Null);
        for state in states {
            row.push(state.clone().finalize());
        }
        rows.push(row);
    }
    if !rows.is_empty() {
        result_chunks.push(DataChunk::from_rows(&rows));
    }

    Some(result_chunks)
}

#[inline]
fn new_states(aggregates: &[(AggregateFunc, usize, bool)]) -> Vec<PartialAggState> {
    aggregates.iter().map(|(func, _, _)| PartialAggState::new(*func)).collect()
}

#[inline]
fn ensure_states(states: &mut Option<Vec<PartialAggState>>, aggregates: &[(AggregateFunc, usize, bool)]) {
    if states.is_none() {
        *states = Some(new_states(aggregates));
    }
}

#[inline]
fn accumulate_all(states: &mut [PartialAggState], chunk: &DataChunk, row_idx: usize, aggregates: &[(AggregateFunc, usize, bool)]) {
    for (agg_idx, (_, col_idx, _)) in aggregates.iter().enumerate() {
        if *col_idx < chunk.columns.len() {
            let val = chunk.columns[*col_idx].get(row_idx);
            states[agg_idx].accumulate(&val);
        }
    }
}

#[inline]
fn int_to_key_value(k: i64, t: GroupKeyType) -> Value {
    match t {
        GroupKeyType::Int32 => Value::Int32(k as i32),
        GroupKeyType::Int64 => Value::Int64(k),
        GroupKeyType::Boolean => Value::Boolean(k != 0),
        GroupKeyType::Timestamp => Value::Timestamp(k),
    }
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
                states[agg_idx].accumulate(&val);
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
    total_cmp(a, b) == std::cmp::Ordering::Less
}

fn value_greater(a: &Value, b: &Value) -> bool {
    total_cmp(a, b) == std::cmp::Ordering::Greater
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

    // ========================================================================
    // S1.2 整数键快速路径测试
    // ========================================================================

    /// 强制走原 Vec<Value> 键路径（等价性测试对照）
    fn execute_grouped_original(
        input: &[DataChunk],
        group_by: &[usize],
        aggregates: &[(AggregateFunc, usize, bool)],
    ) -> Result<Vec<DataChunk>> {
        let mut merged_map: FxHashMap<Vec<Value>, Vec<PartialAggState>> = FxHashMap::default();
        for chunk in input {
            let partial_map = aggregate_chunk_grouped_partial(chunk, group_by, aggregates);
            merge_grouped_map(&mut merged_map, partial_map, aggregates.len());
        }
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

    fn sorted_rows(chunks: &[DataChunk]) -> Vec<String> {
        let mut rows: Vec<String> = chunks
            .iter()
            .flat_map(|c| c.to_rows())
            .map(|r| format!("{:?}", r))
            .collect();
        rows.sort();
        rows
    }

    #[test]
    fn test_grouped_int_matches_original_random() {
        use rand::Rng;
        let mut rng = rand::thread_rng();
        for trial in 0..20 {
            let n = 50 + trial * 13;
            let pure_int32 = trial % 2 == 1;
            let mut keys: Vec<Value> = Vec::with_capacity(n);
            for _ in 0..n {
                if pure_int32 {
                    match rng.gen_range(0..10) {
                        0 => keys.push(Value::Null),
                        _ => keys.push(Value::Int32(rng.gen_range(-20..20))),
                    }
                } else {
                    match rng.gen_range(0..10) {
                        0 => keys.push(Value::Null),
                        _ => keys.push(Value::Int64(rng.gen_range(-100..100))),
                    }
                }
            }
            let vals: Vec<Value> = (0..n)
                .map(|_| Value::Int64(rng.gen_range(0..1000)))
                .collect();
            let chunk = DataChunk {
                columns: vec![Vector::Flat(keys), Vector::Flat(vals)],
                count: n,
            };
            let aggs = vec![
                (AggregateFunc::Count, 1, false),
                (AggregateFunc::Sum, 1, false),
                (AggregateFunc::Min, 1, false),
            ];
            let fast = execute_grouped(&[chunk.clone()], &[0], &aggs).unwrap();
            let orig = execute_grouped_original(&[chunk], &[0], &aggs).unwrap();
            assert_eq!(sorted_rows(&fast), sorted_rows(&orig), "trial {}", trial);
        }
    }

    #[test]
    fn test_grouped_int_null_group() {
        // NULL 键组单独跟踪，与整数键共存
        let chunk = DataChunk {
            columns: vec![
                Vector::Flat(vec![
                    Value::Int64(10), Value::Null, Value::Int64(10), Value::Null, Value::Int64(20),
                ]),
                Vector::Flat(vec![
                    Value::Int64(1), Value::Int64(2), Value::Int64(3), Value::Int64(4), Value::Int64(5),
                ]),
            ],
            count: 5,
        };
        let result = execute_grouped(
            &[chunk],
            &[0],
            &[(AggregateFunc::Count, 1, false), (AggregateFunc::Sum, 1, false)],
        ).unwrap();

        let all_rows: Vec<_> = result.iter().flat_map(|c| c.to_rows()).collect();
        assert_eq!(all_rows.len(), 3); // 10 / NULL / 20

        let g10 = all_rows.iter().find(|r| r[0] == Value::Int64(10)).unwrap();
        assert_eq!(g10[1], Value::Int64(2)); // count
        assert_eq!(g10[2], Value::Float64(4.0)); // sum=1+3

        let gnull = all_rows.iter().find(|r| r[0] == Value::Null).unwrap();
        assert_eq!(gnull[1], Value::Int64(2)); // count
        assert_eq!(gnull[2], Value::Float64(6.0)); // sum=2+4
    }

    #[test]
    fn test_grouped_int_boolean_timestamp() {
        // Boolean / Timestamp 键
        let chunk = DataChunk {
            columns: vec![
                Vector::Flat(vec![
                    Value::Boolean(true), Value::Boolean(false), Value::Boolean(true),
                    Value::Boolean(false), Value::Null,
                ]),
                Vector::Flat(vec![
                    Value::Int64(1), Value::Int64(2), Value::Int64(3), Value::Int64(4), Value::Int64(5),
                ]),
            ],
            count: 5,
        };
        let result = execute_grouped(
            &[chunk],
            &[0],
            &[(AggregateFunc::Sum, 1, false)],
        ).unwrap();
        let all_rows: Vec<_> = result.iter().flat_map(|c| c.to_rows()).collect();
        assert_eq!(all_rows.len(), 3);
        let g_true = all_rows.iter().find(|r| r[0] == Value::Boolean(true)).unwrap();
        assert_eq!(g_true[1], Value::Float64(4.0)); // 1+3

        let ts_chunk = DataChunk {
            columns: vec![
                Vector::Flat(vec![
                    Value::Timestamp(100), Value::Timestamp(200), Value::Timestamp(100),
                ]),
                Vector::Flat(vec![Value::Int64(1), Value::Int64(2), Value::Int64(3)]),
            ],
            count: 3,
        };
        let result = execute_grouped(
            &[ts_chunk],
            &[0],
            &[(AggregateFunc::Sum, 1, false)],
        ).unwrap();
        let all_rows: Vec<_> = result.iter().flat_map(|c| c.to_rows()).collect();
        assert_eq!(all_rows.len(), 2);
        let g100 = all_rows.iter().find(|r| r[0] == Value::Timestamp(100)).unwrap();
        assert_eq!(g100[1], Value::Float64(4.0));
    }

    #[test]
    fn test_grouped_int_mixed_type_fallback() {
        // Int32(1) 与 Int64(1) 是不同键（derive PartialEq variant 不同）→ fallback 原路径
        let chunk = DataChunk {
            columns: vec![
                Vector::Flat(vec![Value::Int32(1), Value::Int64(1)]),
                Vector::Flat(vec![Value::Int64(10), Value::Int64(20)]),
            ],
            count: 2,
        };
        let result = execute_grouped(
            &[chunk],
            &[0],
            &[(AggregateFunc::Sum, 1, false)],
        ).unwrap();
        let all_rows: Vec<_> = result.iter().flat_map(|c| c.to_rows()).collect();
        // 两组：Int32(1) sum=10、Int64(1) sum=20
        assert_eq!(all_rows.len(), 2);
        let g32 = all_rows.iter().find(|r| r[0] == Value::Int32(1)).unwrap();
        assert_eq!(g32[1], Value::Float64(10.0));
        let g64 = all_rows.iter().find(|r| r[0] == Value::Int64(1)).unwrap();
        assert_eq!(g64[1], Value::Float64(20.0));
    }

    #[test]
    fn test_grouped_int_all_null_fallback() {
        // 全 NULL 键 → 单组，与原路径一致
        let chunk = DataChunk {
            columns: vec![
                Vector::Flat(vec![Value::Null, Value::Null, Value::Null]),
                Vector::Flat(vec![Value::Int64(1), Value::Int64(2), Value::Int64(3)]),
            ],
            count: 3,
        };
        let result = execute_grouped(
            &[chunk],
            &[0],
            &[(AggregateFunc::Sum, 1, false)],
        ).unwrap();
        let all_rows: Vec<_> = result.iter().flat_map(|c| c.to_rows()).collect();
        assert_eq!(all_rows.len(), 1);
        assert_eq!(all_rows[0][0], Value::Null);
        assert_eq!(all_rows[0][1], Value::Float64(6.0));
    }

    /// S2-M3：Typed 列聚合与 Flat 等价性
    #[test]
    fn test_typed_aggregate_matches_flat() {
        use crate::common::column_data::ColumnData;
        use rand::Rng;
        let mut rng = rand::thread_rng();
        for trial in 0..30 {
            let n = 10 + rng.gen_range(0..500);
            let is_float = trial % 2 == 1;
            let values: Vec<Value> = (0..n)
                .map(|_| {
                    if rng.gen_bool(0.1) {
                        return Value::Null;
                    }
                    if is_float {
                        Value::Float64(rng.gen_range(-1000.0..1000.0))
                    } else {
                        Value::Int64(rng.gen_range(-100000..100000))
                    }
                })
                .collect();
            let data = ColumnData::try_from_values(&values).unwrap();
            let typed_chunk = DataChunk {
                columns: vec![Vector::Typed(data)],
                count: n,
            };
            let flat_chunk = DataChunk {
                columns: vec![Vector::Flat(values)],
                count: n,
            };

            let funcs = [
                AggregateFunc::Count,
                AggregateFunc::Sum,
                AggregateFunc::Avg,
                AggregateFunc::Min,
                AggregateFunc::Max,
            ];
            for func in funcs {
                let typed = execute(&[typed_chunk.clone()], &[(func, 0, false)]).unwrap();
                let flat = execute(&[flat_chunk.clone()], &[(func, 0, false)]).unwrap();
                let tv = &typed[0].to_rows()[0][0];
                let fv = &flat[0].to_rows()[0][0];
                match (tv, fv) {
                    (Value::Float64(a), Value::Float64(b)) => {
                        assert!((a - b).abs() < 1e-9, "trial {} {:?} {} vs {}", trial, func, a, b)
                    }
                    _ => assert_eq!(tv, fv, "trial {} {:?}", trial, func),
                }
            }
        }
    }

    /// 排序核心对比（S1.2 微基准），仅手动运行
    #[test]
    #[ignore]
    fn bench_grouped_int_vs_vec_key() {
        use std::time::Instant;
        use rand::Rng;
        let mut rng = rand::thread_rng();
        let n = 1_000_000usize;
        // 1000 组，均匀分布
        let keys: Vec<Value> = (0..n).map(|_| Value::Int64(rng.gen_range(0..1000))).collect();
        let vals: Vec<Value> = (0..n).map(|_| Value::Int64(rng.gen_range(0..1000))).collect();
        let chunk = DataChunk { columns: vec![Vector::Flat(keys), Vector::Flat(vals)], count: n };
        let aggs = vec![(AggregateFunc::Sum, 1, false), (AggregateFunc::Count, 1, false)];

        let t0 = Instant::now();
        let fast = execute_grouped(&[chunk.clone()], &[0], &aggs).unwrap();
        let fast_time = t0.elapsed();
        assert!(!fast.is_empty());

        let t0 = Instant::now();
        let orig = execute_grouped_original(&[chunk], &[0], &aggs).unwrap();
        let orig_time = t0.elapsed();
        assert!(!orig.is_empty());

        println!("1M 行: int_key = {:?}, vec_key = {:?}, int 快 {}x",
                 fast_time, orig_time,
                 orig_time.as_nanos() as f64 / fast_time.as_nanos() as f64);
    }
}
