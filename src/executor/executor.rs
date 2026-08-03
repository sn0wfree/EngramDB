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
use fxhash::FxHashSet;

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
            db.create_index(&table_name, &index_name, &key_columns, &included_columns, unique)?;
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
            let count = operators::delete::execute(db, &table_name, condition)?;
            Ok(QueryResult {
                columns: vec!["rows_deleted".to_string()],
                rows: vec![vec![crate::Value::Int64(count as i64)]],
                rows_affected: count as u64,
            })
        }

        PhysicalPlan::Update { table_name, assignments, condition } => {
            let count = operators::update::execute(db, &table_name, &assignments, condition)?;
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
                .ok_or_else(|| crate::common::error::EngramDbError::TableNotFound(table_name.clone()))?;

            // 从索引中查找
            let index = table.get_index(&index_name)
                .ok_or_else(|| crate::common::error::EngramDbError::Parse(
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
                .map(|a| (a.func, a.input, a.distinct))
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

        PhysicalPlan::Sort { input, sort_keys, limit } => {
            let input_result = execute(*input, db)?;
            let input_chunks = rows_to_chunks(&input_result.rows);

            let sorted = operators::sort::execute(&input_chunks, &sort_keys, limit)?;
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

        // Perf01：COUNT(*) 元数据级短路
        PhysicalPlan::CountStar { output_name, count } => {
            Ok(QueryResult {
                columns: vec![output_name],
                rows: vec![vec![crate::Value::Int64(count)]],
                rows_affected: 0,
            })
        }

        // Perf03：主键点查短路（WHERE pk = Literal）
        PhysicalPlan::PrimaryKeyLookup { table_name, pk_value } => {
            // Phase 1：不可变借 -> 查主键索引拿 row_id + 列名
            let (row_id_opt, columns): (Option<u32>, Vec<String>) = {
                let table = db.get_table(&table_name)
                    .ok_or_else(|| crate::common::error::EngramDbError::TableNotFound(table_name.clone()))?;
                (
                    table.lookup_primary_key(&pk_value),
                    table.def.columns.iter().map(|c| c.name.clone()).collect(),
                )
            };
            // Phase 2：可变借 -> 回表读全列
            let rows: Vec<Vec<crate::Value>> = match row_id_opt {
                Some(row_id) => {
                    let table = db.get_table_mut(&table_name)
                        .ok_or_else(|| crate::common::error::EngramDbError::TableNotFound(table_name.clone()))?;
                    table.get_row_by_id(row_id)?.into_iter().collect()
                }
                None => Vec::new(),
            };
            Ok(QueryResult {
                columns,
                rows,
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
        PhysicalPlan::AlterTable(stmt) => {
            crate::executor::operators::alter_table::execute(db, stmt)
        }
        PhysicalPlan::Pragma(stmt) => {
            crate::executor::operators::pragma::execute(db, stmt)
        }
        PhysicalPlan::Distinct { input } => {
            let input_result = execute(*input, db)?;
            let mut seen = FxHashSet::default();
            let mut deduped = Vec::with_capacity(input_result.rows.len());
            for row in input_result.rows {
                if seen.insert(row.clone()) {
                    deduped.push(row);
                }
            }
            Ok(QueryResult {
                columns: input_result.columns,
                rows: deduped,
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
        .ok_or_else(|| crate::common::error::EngramDbError::TableNotFound(table_name.into()))?;

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


