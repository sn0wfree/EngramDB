//! 执行器
//!
//! 遍历物理计划树，执行每个算子。
//! 向量化执行：算子间以 DataChunk 为单位传递数据，整批计算。

use crate::common::error::{Result, EngramDbError};
use crate::storage::Database;
use crate::QueryResult;

use super::physical_plan::{PhysicalPlan, JoinType, SetUnionOp};
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

        PhysicalPlan::CreateTableAs { table_def, source } => {
            // 1. 创建表
            let name = table_def.name.clone();
            db.create_table(table_def)?;

            // 2. 执行 SELECT 子查询，将结果插入新表
            let source_result = execute(*source, db)?;
            let num_cols = source_result.rows.first().map(|r| r.len()).unwrap_or(0);
            let rows: Vec<Vec<crate::Value>> = source_result.rows.iter()
                .map(|r| {
                    let mut row = r.clone();
                    row.resize(num_cols.max(0), crate::Value::Null);
                    row
                })
                .collect();
            let count = crate::executor::operators::insert::execute(db, &name, rows)?;

            Ok(QueryResult {
                columns: vec!["status".to_string()],
                rows: vec![vec![crate::Value::Varchar(format!(
                    "Table '{}' created with {} rows", name, count
                ))]],
                rows_affected: count,
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

        PhysicalPlan::Insert { table_name, rows, returning, on_conflict } => {
            // UPSERT 路径：INSERT...ON CONFLICT DO UPDATE/NOTHING
            if let Some(conflict_clause) = on_conflict {
                return execute_upsert(db, &table_name, rows, conflict_clause, returning);
            }

            // 记录插入前的 row_count，用于 RETURNING 读取实际行
            let base_row_id = if returning.is_some() {
                let t = db.get_table(&table_name)
                    .ok_or_else(|| EngramDbError::TableNotFound(table_name.clone()))?;
                t.def.row_count as u32
            } else {
                0
            };
            let num_rows = rows.len();
            let count = operators::insert::execute(db, &table_name, rows)?;

            // INSERT...RETURNING: 从表中读取实际插入的行（含 AUTO_INCREMENT 值）
            if let Some(returning_items) = returning {
                let table = db.get_table_mut(&table_name)
                    .ok_or_else(|| EngramDbError::TableNotFound(table_name.clone()))?;

                let mut result_rows = Vec::with_capacity(num_rows);
                for rid in base_row_id..base_row_id + num_rows as u32 {
                    let row = table.get_row_by_id(rid)?
                        .ok_or_else(|| EngramDbError::Internal(
                            format!("RETURNING: row_id {} not found after insert", rid)
                        ))?;
                    let mut result_row = Vec::with_capacity(returning_items.len());
                    for item in &returning_items {
                        match item {
                            crate::sql::ast::SelectItem::Wildcard => {
                                result_row.extend(row.iter().cloned());
                            }
                            crate::sql::ast::SelectItem::Expression(expr, _alias) => {
                                let val = evaluate_returning_expr(expr, &row, &table.def)?;
                                result_row.push(val);
                            }
                        }
                    }
                    result_rows.push(result_row);
                }

                // 构造列名
                let columns: Vec<String> = returning_items.iter().enumerate().map(|(i, item)| {
                    match item {
                        crate::sql::ast::SelectItem::Wildcard => format!("*"),
                        crate::sql::ast::SelectItem::Expression(_expr, alias) => {
                            alias.clone().unwrap_or_else(|| format!("col_{}", i))
                        }
                    }
                }).collect();

                return Ok(QueryResult {
                    columns,
                    rows: result_rows,
                    rows_affected: count,
                });
            }

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

        PhysicalPlan::InsertSelect { table_name, columns, source } => {
            // INSERT ... SELECT：先执行 source 计划，将结果行插入目标表
            let source_result = execute(*source, db)?;

            // 验证表存在
            let table = db.get_table(&table_name)
                .ok_or_else(|| EngramDbError::TableNotFound(table_name.clone()))?
                .def.clone();
            let num_cols = table.columns.len();

            // 计算列索引映射
            let col_map: Vec<usize> = if let Some(col_names) = columns {
                col_names.iter()
                    .filter_map(|name| table.column_index(name))
                    .collect()
            } else {
                // 无列名时，按源结果的列顺序填入
                (0..source_result.rows.first().map(|r| r.len()).unwrap_or(num_cols)).collect()
            };

            // 将 source 行映射到目标表行格式
            let mut rows = Vec::with_capacity(source_result.rows.len());
            for source_row in source_result.rows {
                let mut full_row = vec![crate::Value::Null; num_cols];
                for (i, &target_idx) in col_map.iter().enumerate() {
                    if let Some(val) = source_row.get(i) {
                        if target_idx < num_cols {
                            full_row[target_idx] = val.clone();
                        }
                    }
                }
                rows.push(full_row);
            }

            let count = operators::insert::execute(db, &table_name, rows)?;

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

        PhysicalPlan::SetUnion { op, left, right } => {
            // UNION / UNION ALL：合并两个子计划的行
            let left_result = execute(*left, db)?;
            let right_result = execute(*right, db)?;

            // 列名必须一致（否则无法合并）
            if left_result.columns != right_result.columns {
                return Err(EngramDbError::Parse(format!(
                    "UNION columns mismatch: {:?} vs {:?}",
                    left_result.columns, right_result.columns
                )));
            }

            let columns = left_result.columns.clone();
            let mut rows = left_result.rows;

            match op {
                SetUnionOp::UnionAll => {
                    // UNION ALL：直接拼接
                    rows.extend(right_result.rows);
                }
                SetUnionOp::Union => {
                    // UNION：拼接后去重（基于行内容比较）
                    rows.extend(right_result.rows);
                    rows.dedup();
                }
            }

            Ok(QueryResult {
                columns,
                rows,
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
        PhysicalPlan::TruncateTable { table_name } => {
            // TRUNCATE TABLE：清空表数据，保留表结构
            let table_id = db.table_names().get(&table_name).copied()
                .ok_or_else(|| EngramDbError::TableNotFound(table_name.clone()))?;
            {
                let table = db.get_table_mut(&table_name)
                    .ok_or_else(|| EngramDbError::TableNotFound(table_name.clone()))?;
                table.truncate()?;
            }
            // 清空 MVCC 版本（避免后续 INSERT 被误判为 UPDATE）
            db.txn_manager_mut().clear_table_mvcc(table_id);
            Ok(QueryResult {
                columns: vec!["status".to_string()],
                rows: vec![vec![crate::Value::Varchar(format!("TRUNCATE {}", table_name))]],
                rows_affected: 0,
            })
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
        PhysicalPlan::Explain { analyze, plan } => {
            if analyze {
                execute_explain_analyze(*plan, db)
            } else {
                execute_explain(*plan, db)
            }
        }
        PhysicalPlan::Window { input, window_functions, column_names } => {
            let input_result = execute(*input, db)?;
            let input_chunks = rows_to_chunks(&input_result.rows);
            let result = operators::window::execute(&input_chunks, &window_functions, &column_names)?;
            let mut columns = input_result.columns.clone();
            for wf in &window_functions {
                columns.push(wf.output_name.clone());
            }
            let rows = chunks_to_rows(&result);
            Ok(QueryResult {
                columns,
                rows,
                rows_affected: 0,
            })
        }
        PhysicalPlan::SubqueryScan { plan } => {
            let result = execute(*plan, db)?;
            Ok(result)
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

// ============================================================================
// EXPLAIN / EXPLAIN ANALYZE 实现
// ============================================================================

fn plan_node_name(plan: &PhysicalPlan) -> &'static str {
    match plan {
        PhysicalPlan::TableScan { .. } => "TableScan",
        PhysicalPlan::IndexOnlyScan { .. } => "IndexOnlyScan",
        PhysicalPlan::Filter { .. } => "Filter",
        PhysicalPlan::Projection { .. } => "Projection",
        PhysicalPlan::Aggregate { .. } => "Aggregate",
        PhysicalPlan::Insert { .. } => "Insert",
        PhysicalPlan::InsertColumns { .. } => "InsertColumns",
        PhysicalPlan::InsertSelect { .. } => "InsertSelect",
        PhysicalPlan::CreateTable { .. } => "CreateTable",
        PhysicalPlan::CreateIndex { .. } => "CreateIndex",
        PhysicalPlan::Delete { .. } => "Delete",
        PhysicalPlan::Update { .. } => "Update",
        PhysicalPlan::Sort { .. } => "Sort",
        PhysicalPlan::HashJoin { .. } => "HashJoin",
        PhysicalPlan::Limit { .. } => "Limit",
        PhysicalPlan::Analyze { .. } => "Analyze",
        PhysicalPlan::CreateMaterializedView { .. } => "CreateMaterializedView",
        PhysicalPlan::RefreshMaterializedView { .. } => "RefreshMaterializedView",
        PhysicalPlan::DropMaterializedView { .. } => "DropMaterializedView",
        PhysicalPlan::CountStar { .. } => "CountStar",
        PhysicalPlan::PrimaryKeyLookup { .. } => "PrimaryKeyLookup",
        PhysicalPlan::BeginTransaction => "BeginTransaction",
        PhysicalPlan::Commit => "Commit",
        PhysicalPlan::Rollback => "Rollback",
        PhysicalPlan::AlterTable(_) => "AlterTable",
        PhysicalPlan::Pragma(_) => "Pragma",
        PhysicalPlan::Distinct { .. } => "Distinct",
        PhysicalPlan::Explain { .. } => "Explain",
        PhysicalPlan::Window { .. } => "Window",
        PhysicalPlan::SubqueryScan { .. } => "SubqueryScan",
        PhysicalPlan::SetUnion { .. } => "SetUnion",
        PhysicalPlan::TruncateTable { .. } => "TruncateTable",
        PhysicalPlan::CreateTableAs { .. } => "CreateTableAs",
    }
}

/// 构建计划树的可视化文本（缩进格式）
fn format_plan_tree(plan: &PhysicalPlan, indent: usize) -> String {
    let prefix = "  ".repeat(indent);
    let name = plan_node_name(plan);
    let detail = match plan {
        PhysicalPlan::TableScan { table_name, column_indices } => {
            format!(" [{}.{} cols]", table_name, column_indices.len())
        }
        PhysicalPlan::Filter { .. } => String::new(),
        PhysicalPlan::Projection { column_names, .. } => {
            format!(" [{}]", column_names.join(", "))
        }
        PhysicalPlan::Aggregate { group_by, aggregates, .. } => {
            let gb: Vec<String> = group_by.iter().map(|i| format!("col{}", i)).collect();
            let agg: Vec<String> = aggregates.iter().map(|a| format!("{:?}", a.func)).collect();
            format!(" [group_by: {}, agg: {}]", gb.join(","), agg.join(","))
        }
        PhysicalPlan::Sort { sort_keys, .. } => {
            let sk: Vec<String> = sort_keys.iter().map(|k| format!("col{}", k.column_index)).collect();
            format!(" [sort: {}]", sk.join(","))
        }
        PhysicalPlan::HashJoin { join_type, .. } => {
            format!(" [{:?}]", join_type)
        }
        PhysicalPlan::Limit { limit, .. } => {
            format!(" [limit: {}]", limit)
        }
        PhysicalPlan::Insert { table_name, .. } => {
            format!(" [table: {}]", table_name)
        }
        PhysicalPlan::Delete { table_name, .. } => {
            format!(" [table: {}]", table_name)
        }
        PhysicalPlan::Update { table_name, .. } => {
            format!(" [table: {}]", table_name)
        }
        PhysicalPlan::PrimaryKeyLookup { table_name, .. } => {
            format!(" [table: {}]", table_name)
        }
        PhysicalPlan::CountStar { output_name, count } => {
            format!(" [{}: {}]", output_name, count)
        }
        _ => String::new(),
    };
    let mut result = format!("{}{}{}\n", prefix, name, detail);

    // 递归处理子节点
    match plan {
        PhysicalPlan::Filter { input, .. }
        | PhysicalPlan::Projection { input, .. }
        | PhysicalPlan::Aggregate { input, .. }
        | PhysicalPlan::Sort { input, .. }
        | PhysicalPlan::Limit { input, .. }
        | PhysicalPlan::Distinct { input, .. } => {
            result.push_str(&format_plan_tree(input, indent + 1));
        }
        PhysicalPlan::HashJoin { left, right, .. } => {
            result.push_str(&format_plan_tree(left, indent + 1));
            result.push_str(&format_plan_tree(right, indent + 1));
        }
        PhysicalPlan::Window { input, .. } => {
            result.push_str(&format_plan_tree(input, indent + 1));
        }
        PhysicalPlan::SubqueryScan { plan } => {
            result.push_str(&format_plan_tree(plan, indent + 1));
        }
        PhysicalPlan::SetUnion { left, right, .. } => {
            result.push_str(&format_plan_tree(left, indent + 1));
            result.push_str(&format_plan_tree(right, indent + 1));
        }
        PhysicalPlan::InsertSelect { source, .. } => {
            result.push_str(&format_plan_tree(source, indent + 1));
        }
        _ => {}
    }

    result
}

/// EXPLAIN（不执行，只显示计划树）
fn execute_explain(plan: PhysicalPlan, _db: &mut Database) -> Result<QueryResult> {
    let plan_tree = format_plan_tree(&plan, 0);
    Ok(QueryResult {
        columns: vec!["QUERY PLAN".to_string()],
        rows: vec![vec![crate::Value::Varchar(plan_tree)]],
        rows_affected: 0,
    })
}

/// EXPLAIN ANALYZE（执行并收集统计信息）
fn execute_explain_analyze(plan: PhysicalPlan, db: &mut Database) -> Result<QueryResult> {
    use std::time::Instant;

    let total_start = Instant::now();

    // 先执行一次获取实际结果
    let result = execute(plan.clone(), db)?;
    let total_elapsed = total_start.elapsed().as_micros();

    // 构建计划树文本
    let plan_tree = format_plan_tree(&plan, 0);

    let output = format!(
        "Execution Time: {} us\nTotal Rows: {}\n\nPlan:\n{}",
        total_elapsed,
        result.rows.len(),
        plan_tree,
    );

    Ok(QueryResult {
        columns: vec!["QUERY PLAN".to_string()],
        rows: vec![vec![crate::Value::Varchar(output)]],
        rows_affected: 0,
    })
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

/// 评估 INSERT...RETURNING 表达式
fn evaluate_returning_expr(
    expr: &crate::sql::ast::Expression,
    row: &[crate::Value],
    table_def: &crate::common::types::TableDef,
) -> Result<crate::Value> {
    use crate::sql::ast::Expression;
    match expr {
        Expression::ColumnRef { column, .. } => {
            let idx = table_def.column_index(column)
                .ok_or_else(|| EngramDbError::Internal(
                    format!("RETURNING column '{}' not found", column)
                ))?;
            Ok(row[idx].clone())
        }
        Expression::Literal(v) => Ok(v.clone()),
        _ => Err(EngramDbError::Parse(
            format!("Unsupported RETURNING expression: {:?}", expr)
        )),
    }
}

/// 执行 INSERT...ON CONFLICT DO UPDATE/NOTHING（UPSERT）
fn execute_upsert(
    db: &mut Database,
    table_name: &str,
    rows: Vec<Vec<crate::Value>>,
    conflict_clause: crate::sql::ast::OnConflictClause,
    returning: Option<Vec<crate::sql::ast::SelectItem>>,
) -> Result<QueryResult> {
    use crate::sql::ast::OnConflictAction;

    let table = db.get_table(table_name)
        .ok_or_else(|| EngramDbError::TableNotFound(table_name.to_string()))?;
    let table_def = table.def.clone();

    let mut conflict_col_indices: Vec<usize> = conflict_clause.conflict_columns.iter()
        .filter_map(|col_name| table_def.column_index(col_name))
        .collect();

    // INSERT OR IGNORE / DO NOTHING：未指定冲突列时默认使用 PRIMARY KEY
    if conflict_col_indices.is_empty() {
        if let Some(pk_idx) = table_def.primary_key_index() {
            conflict_col_indices.push(pk_idx);
        }
    }

    let mut rows_affected: u64 = 0;
    let mut result_rows = Vec::new();

    for row in &rows {
        let mut conflicting_row_id: Option<u32> = None;

        if !conflict_col_indices.is_empty() {
            let table = db.get_table_mut(table_name)
                .ok_or_else(|| EngramDbError::TableNotFound(table_name.to_string()))?;

            conflicting_row_id = find_conflicting_row(
                table, &table_def, &conflict_col_indices, row,
            )?;
        }

        match conflicting_row_id {
            Some(rid) => {
                match &conflict_clause.action {
                    OnConflictAction::DoNothing => {}
                    OnConflictAction::DoUpdate { assignments } => {
                        let table = db.get_table_mut(table_name)
                            .ok_or_else(|| EngramDbError::TableNotFound(table_name.to_string()))?;
                        let mut existing_row = table.get_row_by_id(rid)?
                            .ok_or_else(|| EngramDbError::Internal(
                                format!("Row {} not found during UPSERT update", rid)
                            ))?;

                        for (col_name, expr) in assignments {
                            let col_idx = table_def.column_index(col_name)
                                .ok_or_else(|| EngramDbError::Internal(
                                    format!("Column '{}' not found in UPDATE assignments", col_name)
                                ))?;
                            if col_idx < existing_row.len() {
                                existing_row[col_idx] = evaluate_returning_expr(expr, &row, &table_def)?;
                            }
                        }

                        table.update_row(rid, &existing_row)?;
                        rows_affected += 1;
                    }
                }
            }
            None => {
                let table = db.get_table_mut(table_name)
                    .ok_or_else(|| EngramDbError::TableNotFound(table_name.to_string()))?;
                table.insert(vec![row.clone()])?;
                rows_affected += 1;
            }
        }

        if let Some(ref returning_items) = returning {
            let table = db.get_table_mut(table_name)
                .ok_or_else(|| EngramDbError::TableNotFound(table_name.to_string()))?;
            let rid = conflicting_row_id.unwrap_or(table.def.row_count as u32 - 1);
            let actual_row = table.get_row_by_id(rid)?
                .ok_or_else(|| EngramDbError::Internal(
                    format!("Row {} not found for RETURNING", rid)
                ))?;
            let mut result_row = Vec::new();
            for item in returning_items {
                match item {
                    crate::sql::ast::SelectItem::Wildcard => {
                        result_row.extend(actual_row.iter().cloned());
                    }
                    crate::sql::ast::SelectItem::Expression(expr, _alias) => {
                        let val = evaluate_returning_expr(expr, &actual_row, &table.def)?;
                        result_row.push(val);
                    }
                }
            }
            result_rows.push(result_row);
        }
    }

    if returning.is_some() {
        let returning_items = returning.unwrap();
        let columns: Vec<String> = returning_items.iter().enumerate().map(|(i, item)| {
            match item {
                crate::sql::ast::SelectItem::Wildcard => format!("*"),
                crate::sql::ast::SelectItem::Expression(_expr, alias) => {
                    alias.clone().unwrap_or_else(|| format!("col_{}", i))
                }
            }
        }).collect();
        Ok(QueryResult {
            columns,
            rows: result_rows,
            rows_affected,
        })
    } else {
        Ok(QueryResult {
            columns: vec!["rows_affected".to_string()],
            rows: vec![vec![crate::Value::Int64(rows_affected as i64)]],
            rows_affected,
        })
    }
}

/// 通过索引查找冲突行，避免全表扫描
///
/// 查找优先级：
/// 1. 主键索引（BTreeMap, O(log n)）— 单列冲突 + 该列是 PK
/// 2. 唯一二级索引（SkipList, O(log n)）— 单列冲突 + 存在唯一索引
/// 3. 全表扫描（O(n)）— 兜底
fn find_conflicting_row(
    table: &mut crate::storage::table::Table,
    table_def: &crate::common::types::TableDef,
    conflict_col_indices: &[usize],
    row: &[crate::Value],
) -> Result<Option<u32>> {
    let conflict_key = &row[conflict_col_indices[0]];

    // 1. 主键索引查找（单列冲突 + 该列是 PK）
    if conflict_col_indices.len() == 1 {
        if table_def.primary_key_index() == Some(conflict_col_indices[0]) {
            if let Some(rid) = table.lookup_primary_key(conflict_key) {
                return Ok(Some(rid));
            }
            return Ok(None);
        }
    }

    // 2. 唯一二级索引查找（单列冲突 + 匹配唯一索引）
    if conflict_col_indices.len() == 1 {
        for idx_def in &table_def.indexes {
            if idx_def.unique && idx_def.key_columns == *conflict_col_indices {
                if let Some(index) = table.get_index(&idx_def.name) {
                    if let Some(row_ids) = index.get(conflict_key) {
                        if let Some(&rid) = row_ids.first() {
                            return Ok(Some(rid));
                        }
                    }
                    return Ok(None);
                }
            }
        }
    }

    // 3. 兜底：全表扫描
    let row_count = table.def.row_count;
    'scan: for rid in 0..row_count as u32 {
        if let Some(existing_row) = table.get_row_by_id(rid)? {
            let mut conflict = true;
            for &col_idx in conflict_col_indices {
                if col_idx >= existing_row.len() || col_idx >= row.len() {
                    conflict = false;
                    break;
                }
                if existing_row[col_idx] != row[col_idx] {
                    conflict = false;
                    break;
                }
            }
            if conflict {
                return Ok(Some(rid));
            }
        }
    }

    Ok(None)
}


