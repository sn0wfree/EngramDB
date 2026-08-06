//! 窗口函数执行器
//!
//! 支持 ROW_NUMBER, RANK, DENSE_RANK, LAG, LEAD, FIRST_VALUE, LAST_VALUE,
//! 以及 COUNT/SUM/AVG/MIN/MAX OVER 窗口聚合。
//!
//! 执行流程：
//! 1. 收集所有行，按 PARTITION BY + ORDER BY 排序
//! 2. 按 PARTITION BY 分区
//! 3. 在每个分区内计算窗口函数

use crate::common::error::Result;
use crate::common::value_cmp::total_cmp;
use crate::executor::physical_plan::{WindowFunctionExpr, WindowFuncType};
use crate::executor::vector::DataChunk;
use crate::sql::ast::{WindowSpec, WindowFrameBound, Expression};
use crate::Value;

pub fn execute(
    input: &[DataChunk],
    window_funcs: &[WindowFunctionExpr],
    column_names: &[String],
) -> Result<Vec<DataChunk>> {
    if input.is_empty() || window_funcs.is_empty() {
        return Ok(input.to_vec());
    }

    let all_rows = chunks_to_rows(input);

    if let Some(first_wf) = window_funcs.first() {
        let sorted_rows = sort_rows(&all_rows, &first_wf.window_spec, column_names);
        let partitions = find_partitions(&sorted_rows, &first_wf.window_spec, column_names);

        let mut output_rows = Vec::with_capacity(sorted_rows.len());
        for partition in &partitions {
            let partition_rows = &sorted_rows[partition.start..partition.end];
            let partition_output = compute_window_functions(partition_rows, window_funcs);
            output_rows.extend(partition_output);
        }

        Ok(rows_to_chunks(&output_rows))
    } else {
        Ok(input.to_vec())
    }
}

struct Partition {
    start: usize,
    end: usize,
}

fn sort_rows(rows: &[Vec<Value>], spec: &WindowSpec, column_names: &[String]) -> Vec<Vec<Value>> {
    let mut sorted = rows.to_vec();
    sorted.sort_by(|a, b| {
        for expr in &spec.partition_by {
            let idx = resolve_column_index(expr, column_names);
            if let Some(i) = idx {
                if i < a.len() && i < b.len() {
                    let cmp = compare_values(&a[i], &b[i]);
                    if cmp != std::cmp::Ordering::Equal {
                        return cmp;
                    }
                }
            }
        }
        for item in &spec.order_by {
            let idx = resolve_column_index(&item.expr, column_names);
            if let Some(i) = idx {
                if i < a.len() && i < b.len() {
                    let cmp = compare_values(&a[i], &b[i]);
                    if cmp != std::cmp::Ordering::Equal {
                        return if item.ascending { cmp } else { cmp.reverse() };
                    }
                }
            }
        }
        std::cmp::Ordering::Equal
    });
    sorted
}

fn find_partitions(rows: &[Vec<Value>], spec: &WindowSpec, column_names: &[String]) -> Vec<Partition> {
    if rows.is_empty() || spec.partition_by.is_empty() {
        return vec![Partition { start: 0, end: rows.len() }];
    }
    let mut partitions = Vec::new();
    let mut start = 0;
    for i in 1..rows.len() {
        let mut same = true;
        for expr in &spec.partition_by {
            let idx = resolve_column_index(expr, column_names);
            if let Some(idx) = idx {
                if idx < rows[i].len() && idx < rows[i - 1].len() && rows[i][idx] != rows[i - 1][idx] {
                    same = false;
                    break;
                }
            }
        }
        if !same {
            partitions.push(Partition { start, end: i });
            start = i;
        }
    }
    partitions.push(Partition { start, end: rows.len() });
    partitions
}

fn compute_window_functions(rows: &[Vec<Value>], window_funcs: &[WindowFunctionExpr]) -> Vec<Vec<Value>> {
    let mut result = Vec::with_capacity(rows.len());
    for i in 0..rows.len() {
        let mut row = rows[i].clone();
        for wf in window_funcs {
            row.push(compute_single_window_function(rows, i, wf));
        }
        result.push(row);
    }
    result
}

fn compute_single_window_function(rows: &[Vec<Value>], current_idx: usize, wf: &WindowFunctionExpr) -> Value {
    match wf.func {
        WindowFuncType::RowNumber => Value::Int64((current_idx + 1) as i64),
        WindowFuncType::Rank => {
            if current_idx == 0 { return Value::Int64(1); }
            let mut rank = current_idx as i64 + 1;
            for j in (0..current_idx).rev() {
                if rows_equal_on_order(&rows[current_idx], &rows[j], &wf.window_spec) {
                    rank = j as i64 + 1;
                } else { break; }
            }
            Value::Int64(rank)
        }
        WindowFuncType::DenseRank => {
            if current_idx == 0 { return Value::Int64(1); }
            let mut dr = 1i64;
            for j in 1..=current_idx {
                if !rows_equal_on_order(&rows[j], &rows[j - 1], &wf.window_spec) {
                    dr += 1;
                }
            }
            Value::Int64(dr)
        }
        WindowFuncType::Lag(offset) => {
            if current_idx >= offset {
                if let Some(col) = wf.input_column {
                    if col < rows[current_idx - offset].len() {
                        return rows[current_idx - offset][col].clone();
                    }
                }
            }
            Value::Null
        }
        WindowFuncType::Lead(offset) => {
            if current_idx + offset < rows.len() {
                if let Some(col) = wf.input_column {
                    if col < rows[current_idx + offset].len() {
                        return rows[current_idx + offset][col].clone();
                    }
                }
            }
            Value::Null
        }
        WindowFuncType::FirstValue => {
            if let Some(col) = wf.input_column {
                if col < rows[0].len() { return rows[0][col].clone(); }
            }
            Value::Null
        }
        WindowFuncType::LastValue => {
            if let Some(col) = wf.input_column {
                if let Some(last) = rows.last() {
                    if col < last.len() { return last[col].clone(); }
                }
            }
            Value::Null
        }
        WindowFuncType::Count => {
            let (start, end) = compute_frame_bounds(rows.len(), current_idx, &wf.window_spec);
            let mut count = 0i64;
            for j in start..end {
                if let Some(col) = wf.input_column {
                    if col < rows[j].len() && !rows[j][col].is_null() { count += 1; }
                } else { count += 1; }
            }
            Value::Int64(count)
        }
        WindowFuncType::Sum => {
            let (start, end) = compute_frame_bounds(rows.len(), current_idx, &wf.window_spec);
            let mut sum = 0.0f64;
            let mut has = false;
            for j in start..end {
                if let Some(col) = wf.input_column {
                    if col < rows[j].len() {
                        if let Some(f) = rows[j][col].as_f64() { sum += f; has = true; }
                    }
                }
            }
            if has { Value::Float64(sum) } else { Value::Null }
        }
        WindowFuncType::Avg => {
            let (start, end) = compute_frame_bounds(rows.len(), current_idx, &wf.window_spec);
            let mut sum = 0.0f64;
            let mut cnt = 0i64;
            for j in start..end {
                if let Some(col) = wf.input_column {
                    if col < rows[j].len() {
                        if let Some(f) = rows[j][col].as_f64() { sum += f; cnt += 1; }
                    }
                }
            }
            if cnt > 0 { Value::Float64(sum / cnt as f64) } else { Value::Null }
        }
        WindowFuncType::Min => {
            let (start, end) = compute_frame_bounds(rows.len(), current_idx, &wf.window_spec);
            let mut min_val: Option<Value> = None;
            for j in start..end {
                if let Some(col) = wf.input_column {
                    if col < rows[j].len() && !rows[j][col].is_null() {
                        match &min_val {
                            None => min_val = Some(rows[j][col].clone()),
                            Some(cur) => {
                                if compare_values(&rows[j][col], cur) == std::cmp::Ordering::Less {
                                    min_val = Some(rows[j][col].clone());
                                }
                            }
                        }
                    }
                }
            }
            min_val.unwrap_or(Value::Null)
        }
        WindowFuncType::Max => {
            let (start, end) = compute_frame_bounds(rows.len(), current_idx, &wf.window_spec);
            let mut max_val: Option<Value> = None;
            for j in start..end {
                if let Some(col) = wf.input_column {
                    if col < rows[j].len() && !rows[j][col].is_null() {
                        match &max_val {
                            None => max_val = Some(rows[j][col].clone()),
                            Some(cur) => {
                                if compare_values(&rows[j][col], cur) == std::cmp::Ordering::Greater {
                                    max_val = Some(rows[j][col].clone());
                                }
                            }
                        }
                    }
                }
            }
            max_val.unwrap_or(Value::Null)
        }
        WindowFuncType::NthValue(_) => Value::Null,
    }
}

fn compute_frame_bounds(total_rows: usize, current_idx: usize, spec: &WindowSpec) -> (usize, usize) {
    let frame = match &spec.window_frame {
        Some(f) => f,
        None => return (0, current_idx + 1),
    };
    let start = match &frame.start {
        WindowFrameBound::UnboundedPreceding => 0,
        WindowFrameBound::NPreceding(n) => current_idx.saturating_sub(*n),
        WindowFrameBound::CurrentRow => current_idx,
        WindowFrameBound::NFollowing(n) => (current_idx + n).min(total_rows - 1),
        WindowFrameBound::UnboundedFollowing => total_rows - 1,
    };
    let end = match &frame.end {
        Some(WindowFrameBound::UnboundedFollowing) => total_rows,
        Some(WindowFrameBound::NFollowing(n)) => (current_idx + n + 1).min(total_rows),
        Some(WindowFrameBound::CurrentRow) => current_idx + 1,
        Some(WindowFrameBound::NPreceding(n)) => current_idx.saturating_sub(*n) + 1,
        None | Some(WindowFrameBound::UnboundedPreceding) => current_idx + 1,
    };
    (start.min(total_rows), end.min(total_rows))
}

fn rows_equal_on_order(a: &[Value], b: &[Value], _spec: &WindowSpec) -> bool {
    a == b
}

fn resolve_column_index(expr: &Expression, column_names: &[String]) -> Option<usize> {
    match expr {
        Expression::ColumnRef { column, .. } => column_names.iter().position(|c| c == column),
        _ => None,
    }
}

fn compare_values(a: &Value, b: &Value) -> std::cmp::Ordering {
    total_cmp(a, b)
}

fn chunks_to_rows(chunks: &[DataChunk]) -> Vec<Vec<Value>> {
    crate::executor::vector::flatten_to_rows(chunks)
}

fn rows_to_chunks(rows: &[Vec<Value>]) -> Vec<DataChunk> {
    crate::executor::vector::from_rows_batched(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::physical_plan::WindowFunctionExpr;
    use crate::executor::physical_plan::WindowFuncType;
    use crate::sql::ast::WindowSpec;

    #[test]
    fn test_row_number() {
        let rows = vec![
            vec![Value::Int64(1)],
            vec![Value::Int64(2)],
            vec![Value::Int64(3)],
        ];
        let chunk = rows_to_chunks(&rows);
        let wf = WindowFunctionExpr {
            func: WindowFuncType::RowNumber,
            input_column: None,
            window_spec: WindowSpec { partition_by: vec![], order_by: vec![], window_frame: None },
            output_name: "rn".to_string(),
        };
        let result = execute(&chunk, &[wf], &["val".to_string()]).unwrap();
        let result_rows = chunks_to_rows(&result);
        assert_eq!(result_rows.len(), 3);
        assert_eq!(result_rows[0][1], Value::Int64(1));
        assert_eq!(result_rows[1][1], Value::Int64(2));
        assert_eq!(result_rows[2][1], Value::Int64(3));
    }

    #[test]
    fn test_lag() {
        let rows = vec![
            vec![Value::Int64(10)],
            vec![Value::Int64(20)],
            vec![Value::Int64(30)],
        ];
        let chunk = rows_to_chunks(&rows);
        let wf = WindowFunctionExpr {
            func: WindowFuncType::Lag(1),
            input_column: Some(0),
            window_spec: WindowSpec { partition_by: vec![], order_by: vec![], window_frame: None },
            output_name: "lag".to_string(),
        };
        let result = execute(&chunk, &[wf], &["val".to_string()]).unwrap();
        let result_rows = chunks_to_rows(&result);
        assert_eq!(result_rows[0][1], Value::Null);
        assert_eq!(result_rows[1][1], Value::Int64(10));
        assert_eq!(result_rows[2][1], Value::Int64(20));
    }

    #[test]
    fn test_lead() {
        let rows = vec![
            vec![Value::Int64(10)],
            vec![Value::Int64(20)],
            vec![Value::Int64(30)],
        ];
        let chunk = rows_to_chunks(&rows);
        let wf = WindowFunctionExpr {
            func: WindowFuncType::Lead(1),
            input_column: Some(0),
            window_spec: WindowSpec { partition_by: vec![], order_by: vec![], window_frame: None },
            output_name: "lead".to_string(),
        };
        let result = execute(&chunk, &[wf], &["val".to_string()]).unwrap();
        let result_rows = chunks_to_rows(&result);
        assert_eq!(result_rows[0][1], Value::Int64(20));
        assert_eq!(result_rows[1][1], Value::Int64(30));
        assert_eq!(result_rows[2][1], Value::Null);
    }

    #[test]
    fn test_empty_input() {
        let result = execute(&[], &[], &[] as &[String]).unwrap();
        assert!(result.is_empty());
    }
}
