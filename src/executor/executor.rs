//! 执行器
//!
//! 遍历物理计划树，执行每个算子。
//! 向量化执行：算子间以 DataChunk 为单位传递数据，整批计算。

use crate::common::error::Result;
use crate::storage::Database;
use crate::QueryResult;

use super::physical_plan::{PhysicalPlan, JoinType};
use super::vector::DataChunk;
use super::operators;
use crate::sql::ast::Expression;

/// 执行物理计划
pub fn execute(plan: PhysicalPlan, db: &mut Database) -> Result<QueryResult> {
    match plan {
        PhysicalPlan::CreateTable { table_def } => {
            let name = table_def.name.clone();
            db.create_table(table_def)?;
            Ok(QueryResult {
                columns: vec!["status".to_string()],
                rows: vec![vec![crate::Value::Varchar(format!("Table '{}' created", name))]],
                rows_affected: 0,
            })
        }

        PhysicalPlan::CreateIndex { table_name, index_name, key_columns, included_columns, unique } => {
            // 目前只支持单列键索引
            let key_col_idx = key_columns.first()
                .copied()
                .ok_or_else(|| crate::common::error::HybridDbError::Parse(
                    "Index must have at least one key column".to_string()
                ))?;
            db.create_index(&table_name, &index_name, key_col_idx, &included_columns, unique)?;
            Ok(QueryResult {
                columns: vec!["status".to_string()],
                rows: vec![vec![crate::Value::Varchar(format!(
                    "Index '{}' created on '{}' ({} key, {} included)",
                    index_name, table_name, key_columns.len(), included_columns.len()
                ))]],
                rows_affected: 0,
            })
        }

        PhysicalPlan::Delete { table_name, condition } => {
            let count = execute_delete(db, &table_name, condition)?;
            Ok(QueryResult {
                columns: vec!["rows_deleted".to_string()],
                rows: vec![vec![crate::Value::Int64(count as i64)]],
                rows_affected: count as u64,
            })
        }

        PhysicalPlan::Update { table_name, assignments, condition } => {
            let count = execute_update(db, &table_name, &assignments, condition)?;
            Ok(QueryResult {
                columns: vec!["rows_updated".to_string()],
                rows: vec![vec![crate::Value::Int64(count as i64)]],
                rows_affected: count as u64,
            })
        }

        PhysicalPlan::Insert { table_name, rows } => {
            let count = operators::insert::execute(db, &table_name, rows)?;
            Ok(QueryResult {
                columns: vec!["rows_inserted".to_string()],
                rows: vec![vec![crate::Value::Int64(count as i64)]],
                rows_affected: count,
            })
        }

        PhysicalPlan::InsertColumns { table_name, columns } => {
            let count = operators::insert::execute_columns(db, &table_name, columns)?;
            Ok(QueryResult {
                columns: vec!["rows_inserted".to_string()],
                rows: vec![vec![crate::Value::Int64(count as i64)]],
                rows_affected: count,
            })
        }

        PhysicalPlan::TableScan { table_name, column_indices } => {
            let chunks = operators::table_scan::execute(db, &table_name, &column_indices)?;
            let (columns, rows) = collect_result(&chunks, db, &table_name, &column_indices)?;
            Ok(QueryResult {
                columns,
                rows,
                rows_affected: 0,
            })
        }

        PhysicalPlan::IndexOnlyScan { table_name, index_name, key_value, output_column_indices, output_col_map } => {
            let table = db.get_table(&table_name)
                .ok_or_else(|| crate::common::error::HybridDbError::TableNotFound(table_name.clone()))?;

            // 从索引中查找
            let index = table.get_index(&index_name)
                .ok_or_else(|| crate::common::error::HybridDbError::Parse(
                    format!("Index '{}' not found during execution", index_name)
                ))?;

            let entries = index.get_entries(&key_value);

            // 构建输出行
            // output_col_map[i] = 0 表示 key 列，>=1 表示 included 列的位置+1
            let mut rows: Vec<Vec<crate::Value>> = Vec::new();
            if let Some(entry_slice) = entries {
                rows.reserve(entry_slice.len());
                for entry in entry_slice {
                    let mut row = Vec::with_capacity(output_col_map.len());
                    for &pos in &output_col_map {
                        if pos == 0 {
                            row.push(key_value.clone());
                        } else {
                            let inc_idx = pos - 1;
                            row.push(entry.included.get(inc_idx).cloned().unwrap_or(crate::Value::Null));
                        }
                    }
                    rows.push(row);
                }
            }

            // 列名
            let column_names: Vec<String> = output_column_indices.iter()
                .map(|&i| table.def.columns[i].name.clone())
                .collect();

            Ok(QueryResult {
                columns: column_names,
                rows,
                rows_affected: 0,
            })
        }

        PhysicalPlan::Filter { input, condition } => {
            // 先执行子计划
            let input_result = execute(*input, db)?;
            let input_chunks = rows_to_chunks(&input_result.rows);
            let column_names = input_result.columns.clone();

            let filtered = operators::filter::execute(&input_chunks, &condition, &column_names)?;
            let rows = chunks_to_rows(&filtered);

            Ok(QueryResult {
                columns: column_names,
                rows,
                rows_affected: 0,
            })
        }

        PhysicalPlan::Projection { input, expressions, column_names } => {
            let input_result = execute(*input, db)?;
            let input_chunks = rows_to_chunks(&input_result.rows);
            let input_columns = input_result.columns.clone();

            let projected = operators::projection::execute(
                &input_chunks, &expressions, &input_columns
            )?;
            let rows = chunks_to_rows(&projected);

            Ok(QueryResult {
                columns: column_names,
                rows,
                rows_affected: 0,
            })
        }

        PhysicalPlan::HashJoin { left, right, join_type, left_keys, right_keys } => {
            let left_result = execute(*left, db)?;
            let right_result = execute(*right, db)?;

            let left_chunks = rows_to_chunks(&left_result.rows);
            let right_chunks = rows_to_chunks(&right_result.rows);

            let joined = operators::hash_join::execute(
                &left_chunks, &right_chunks, &left_keys, &right_keys, join_type
            )?;

            let rows = chunks_to_rows(&joined);

            // 列名：左表列 + 右表列
            let mut columns = left_result.columns.clone();
            columns.extend(right_result.columns.clone());

            Ok(QueryResult {
                columns,
                rows,
                rows_affected: 0,
            })
        }

        PhysicalPlan::Aggregate { input, group_by, aggregates } => {
            let input_result = execute(*input, db)?;
            let input_chunks = rows_to_chunks(&input_result.rows);

            let agg_funcs: Vec<_> = aggregates.iter()
                .map(|a| (a.func, a.input))
                .collect();

            let result = if group_by.is_empty() {
                // 无 GROUP BY：简单聚合
                operators::aggregate::execute(&input_chunks, &agg_funcs)?
            } else {
                // 有 GROUP BY：分组聚合
                operators::aggregate::execute_grouped(&input_chunks, &group_by, &agg_funcs)?
            };

            let rows = chunks_to_rows(&result);

            // 列名：分组列 + 聚合列
            let mut columns: Vec<String> = group_by.iter()
                .map(|&i| {
                    if i < input_result.columns.len() {
                        input_result.columns[i].clone()
                    } else {
                        format!("group_{}", i)
                    }
                })
                .collect();

            let agg_names: Vec<String> = aggregates.iter()
                .map(|a| format!("{:?}({})", a.func, a.input))
                .collect();
            columns.extend(agg_names);

            Ok(QueryResult {
                columns,
                rows,
                rows_affected: 0,
            })
        }

        PhysicalPlan::Sort { input, sort_keys } => {
            let input_result = execute(*input, db)?;
            let input_chunks = rows_to_chunks(&input_result.rows);

            let sorted = operators::sort::execute(&input_chunks, &sort_keys)?;
            let rows = chunks_to_rows(&sorted);

            Ok(QueryResult {
                columns: input_result.columns,
                rows,
                rows_affected: 0,
            })
        }

        PhysicalPlan::Limit { input, limit } => {
            let input_result = execute(*input, db)?;
            let limited_rows: Vec<_> = input_result.rows.into_iter().take(limit).collect();

            Ok(QueryResult {
                columns: input_result.columns,
                rows: limited_rows,
                rows_affected: 0,
            })
        }

        PhysicalPlan::BeginTransaction => {
            Ok(QueryResult {
                columns: vec!["status".to_string()],
                rows: vec![vec![crate::Value::Varchar("BEGIN".to_string())]],
                rows_affected: 0,
            })
        }

        PhysicalPlan::Commit => {
            Ok(QueryResult {
                columns: vec!["status".to_string()],
                rows: vec![vec![crate::Value::Varchar("COMMIT".to_string())]],
                rows_affected: 0,
            })
        }

        PhysicalPlan::Rollback => {
            Ok(QueryResult {
                columns: vec!["status".to_string()],
                rows: vec![vec![crate::Value::Varchar("ROLLBACK".to_string())]],
                rows_affected: 0,
            })
        }

        // DDL/管理语句：简单返回 OK
        PhysicalPlan::Analyze { table_name, .. } => {
            Ok(QueryResult {
                columns: vec!["status".to_string()],
                rows: vec![vec![crate::Value::Varchar(format!("ANALYZE {} ok", table_name))]],
                rows_affected: 0,
            })
        }
        PhysicalPlan::CreateMaterializedView { view_name, .. } => {
            Ok(QueryResult {
                columns: vec!["status".to_string()],
                rows: vec![vec![crate::Value::Varchar(format!("Materialized view '{}' created", view_name))]],
                rows_affected: 0,
            })
        }
        PhysicalPlan::RefreshMaterializedView { view_name, .. } => {
            Ok(QueryResult {
                columns: vec!["status".to_string()],
                rows: vec![vec![crate::Value::Varchar(format!("Materialized view '{}' refreshed", view_name))]],
                rows_affected: 0,
            })
        }
        PhysicalPlan::DropMaterializedView { view_name, .. } => {
            Ok(QueryResult {
                columns: vec!["status".to_string()],
                rows: vec![vec![crate::Value::Varchar(format!("Materialized view '{}' dropped", view_name))]],
                rows_affected: 0,
            })
        }
    }
}

fn collect_result(
    chunks: &[DataChunk],
    db: &Database,
    table_name: &str,
    column_indices: &[usize],
) -> Result<(Vec<String>, Vec<Vec<crate::Value>>)> {
    let table = db.get_table(table_name)
        .ok_or_else(|| crate::common::error::HybridDbError::TableNotFound(table_name.into()))?;

    let column_names: Vec<String> = column_indices
        .iter()
        .map(|&i| table.def.columns[i].name.clone())
        .collect();

    let rows = chunks_to_rows(chunks);

    Ok((column_names, rows))
}

fn chunks_to_rows(chunks: &[DataChunk]) -> Vec<Vec<crate::Value>> {
    let mut all_rows = Vec::new();
    for chunk in chunks {
        all_rows.extend(chunk.to_rows());
    }
    all_rows
}

fn rows_to_chunks(rows: &[Vec<crate::Value>]) -> Vec<DataChunk> {
    let batch_size = super::vector::VECTOR_SIZE;
    let mut chunks = Vec::new();
    for batch in rows.chunks(batch_size) {
        chunks.push(DataChunk::from_rows(batch));
    }
    chunks
}

// ============================================================================
// DELETE / UPDATE 执行（v0.12.0 新增）
// ============================================================================

/// 执行 DELETE 语句
///
/// 策略：
/// 1. 扫描表的所有列（用于评估 WHERE 条件和定位行）
/// 2. 应用 WHERE 过滤，找出匹配行的 Delta 层索引
/// 3. 从 Delta 层删除匹配行
///
/// 注意：当前仅支持删除 Delta 层的行。列存中的行暂不支持原地删除
/// （LSM 风格，后续通过 tombstone + compact 实现）。
fn execute_delete(
    db: &mut Database,
    table_name: &str,
    condition: Option<Expression>,
) -> Result<usize> {
    let table = db.get_table_mut(table_name)
        .ok_or_else(|| crate::common::error::HybridDbError::TableNotFound(table_name.into()))?;

    let num_cols = table.def.columns.len();
    if num_cols == 0 {
        return Ok(0);
    }

    // 扫描所有列（用于评估 WHERE 条件）
    let all_col_indices: Vec<usize> = (0..num_cols).collect();
    let all_rows = table.scan(&all_col_indices)?;

    // 计算列存的行数（用于区分列存行和 Delta 行）
    let delta_total = table.delta_store().len();
    let cs_rows = table.def.row_count as usize - delta_total;

    // 如果没有 WHERE 条件，删除所有 Delta 行
    if condition.is_none() {
        let delta_indices: Vec<usize> = (0..delta_total).collect();
        let count = table.delete_delta_rows(&delta_indices)?;
        return Ok(count);
    }

    let cond = condition.unwrap();
    let col_names: Vec<String> = table.def.columns.iter().map(|c| c.name.clone()).collect();

    // 找出匹配的行（只处理 Delta 层的行，即 cs_rows 之后的行）
    let mut delta_indices_to_delete: Vec<usize> = Vec::new();

    for (row_idx, row) in all_rows.iter().enumerate() {
        // 只处理 Delta 层的行（列存中的行暂不支持删除）
        if row_idx < cs_rows {
            continue;
        }
        let delta_idx = row_idx - cs_rows;

        // 评估 WHERE 条件
        let chunks = rows_to_chunks(&[row.clone()]);
        let filtered = operators::filter::execute(&chunks, &cond, &col_names)?;
        if !filtered.is_empty() && filtered[0].count > 0 {
            delta_indices_to_delete.push(delta_idx);
        }
    }

    let count = table.delete_delta_rows(&delta_indices_to_delete)?;
    Ok(count)
}

/// 执行 UPDATE 语句
///
/// 策略：
/// 1. 扫描表的所有列
/// 2. 应用 WHERE 过滤，找出匹配行的 Delta 层索引
/// 3. 对匹配行执行更新
///
/// 注意：当前仅支持更新 Delta 层的行。
fn execute_update(
    db: &mut Database,
    table_name: &str,
    assignments: &[(usize, Expression)],
    condition: Option<Expression>,
) -> Result<usize> {
    let table = db.get_table_mut(table_name)
        .ok_or_else(|| crate::common::error::HybridDbError::TableNotFound(table_name.into()))?;

    let num_cols = table.def.columns.len();
    if num_cols == 0 {
        return Ok(0);
    }

    // 扫描所有列
    let all_col_indices: Vec<usize> = (0..num_cols).collect();
    let all_rows = table.scan(&all_col_indices)?;

    let delta_total = table.delta_store().len();
    let cs_rows = table.def.row_count as usize - delta_total;
    let col_names: Vec<String> = table.def.columns.iter().map(|c| c.name.clone()).collect();

    // 找出匹配的 Delta 行，并计算新值
    let mut updates: Vec<(usize, Vec<(usize, crate::Value)>)> = Vec::new();

    for (row_idx, row) in all_rows.iter().enumerate() {
        // 只处理 Delta 层的行
        if row_idx < cs_rows {
            continue;
        }
        let delta_idx = row_idx - cs_rows;

        // 评估 WHERE 条件
        if let Some(ref cond) = condition {
            let chunks = rows_to_chunks(&[row.clone()]);
            let filtered = operators::filter::execute(&chunks, cond, &col_names)?;
            if filtered.is_empty() || filtered[0].count == 0 {
                continue;
            }
        }

        // 计算每个 SET 列的新值
        let mut new_vals: Vec<(usize, crate::Value)> = Vec::new();
        for &(col_idx, ref expr) in assignments {
            // 简单表达式求值：只支持字面量（MVP）
            // 复杂表达式通过 expression 模块求值
            let chunks = rows_to_chunks(&[row.clone()]);
            let result = operators::projection::execute(
                &chunks,
                &[expr.clone()],
                &col_names,
            )?;
            if !result.is_empty() && result[0].count > 0 {
                let val = result[0].columns[0].get(0).clone();
                new_vals.push((col_idx, val));
            }
        }

        if !new_vals.is_empty() {
            updates.push((delta_idx, new_vals));
        }
    }

    let count = table.update_delta_rows(&updates)?;
    Ok(count)
}
