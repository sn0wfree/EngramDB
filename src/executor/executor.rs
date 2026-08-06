//! 执行器
//!
//! 遍历物理计划树，执行每个算子。
//! 向量化执行：算子间以 DataChunk 为单位传递数据，整批计算。

use crate::common::error::{Result, EngramDbError};
use crate::storage::Database;
use crate::QueryResult;
use crate::Value;

use super::physical_plan::{PhysicalPlan, JoinType, SetUnionOp};
use super::vector::{DataChunk, Vector};
use super::operators;
use crate::sql::ast::Expression;
use fxhash::FxHashSet;

/// 只读子计划的列式执行：返回 (列名, chunks)，全程保持 Typed 列式，不物化行。
///
/// M1-3：跳过「行→列→行」双重转置。表扫描直接产出 Typed chunks，
/// 过滤/投影/排序/聚合在列式管道内完成，最终结果只物化一次。
/// 不支持的节点返回 Ok(None)，调用方回退行式路径（行为与改造前一致）。
fn try_execute_chunks(
    plan: &PhysicalPlan,
    db: &mut Database,
) -> Result<Option<(Vec<String>, Vec<DataChunk>)>> {
    use crate::common::column_data::ColumnData;
    use crate::executor::vector::VECTOR_SIZE;

    match plan {
        PhysicalPlan::TableScan { table_name, column_indices } => {
            let table = db
                .get_engine_table_mut(table_name)
                .ok_or_else(|| EngramDbError::TableNotFound(table_name.clone()))?;
            let columns: Vec<String> = column_indices
                .iter()
                .map(|&i| table.def().columns[i].name.clone())
                .collect();
            let chunks = table.scan_to_chunks(column_indices, None)?;
            Ok(Some((columns, chunks)))
        }
        PhysicalPlan::Projection {
            input,
            expressions,
            column_names,
        } => {
            // 仅支持纯列引用投影（列子集 / 重排），其余表达式回退行式路径
            if !expressions
                .iter()
                .all(|e| matches!(e, Expression::ColumnRef { .. }))
            {
                return Ok(None);
            }
            let (cols, chunks) = match try_execute_chunks(input, db)? {
                Some(x) => x,
                None => return Ok(None),
            };
            // 表达式列名 → 输入列位置（兼容 "t.col" 前缀形式）
            let mut indices = Vec::with_capacity(expressions.len());
            for e in expressions {
                let Expression::ColumnRef { table, column } = e else {
                    return Ok(None);
                };
                let prefixed = table
                    .as_ref()
                    .map(|t| format!("{}.{}", t, column))
                    .unwrap_or_default();
                let pos = cols
                    .iter()
                    .position(|c| c == column || c == &prefixed)
                    .ok_or_else(|| {
                        EngramDbError::Parse(format!("unknown column: {}", column))
                    })?;
                indices.push(pos);
            }
            let out_chunks = chunks
                .iter()
                .map(|ch| {
                    let columns: Vec<Vector> =
                        indices.iter().map(|&i| ch.columns[i].clone()).collect();
                    DataChunk {
                        columns,
                        count: ch.count,
                    }
                })
                .collect();
            Ok(Some((column_names.clone(), out_chunks)))
        }
        PhysicalPlan::Filter { input, condition } => {
            // PREWHERE（M1-6）：TableScan + 简单谓词 → 列式跳读扫描
            // （row group MinMax 跳过 + batch 内 Typed 谓词直扫，只物化幸存行）
            if let PhysicalPlan::TableScan { table_name, column_indices } = input.as_ref() {
                if let Some((pred_col, pred_op, pred_val)) = extract_skip_predicate(condition) {
                    let (names, chunks, filtered) = {
                        let table = db
                            .get_engine_table_mut(table_name)
                            .ok_or_else(|| EngramDbError::TableNotFound(table_name.clone()))?;
                        let names: Vec<String> = column_indices
                            .iter()
                            .map(|&i| table.def().columns[i].name.clone())
                            .collect();
                        let pred_col_idx = table.def().column_index(&pred_col);
                        // 谓词列是否在输出列：在 → 扫描已精确筛选；不在 → 需 filter 兜底
                        let pred_pos = pred_col_idx
                            .and_then(|ci| column_indices.iter().position(|&c| c == ci));
                        let skip = pred_col_idx.map(|ci| (ci, pred_op, pred_val.clone()));
                        let chunks = table.scan_to_chunks(column_indices, skip)?;
                        (names, chunks, pred_pos.is_some())
                    };
                    if filtered {
                        return Ok(Some((names, chunks)));
                    }
                    // 谓词列不在输出列：扫描只做 row group 级粗裁，需精确过滤
                    let filtered_chunks = operators::filter::execute(&chunks, condition, &names)?;
                    return Ok(Some((names, filtered_chunks)));
                }
            }
            // 通用路径：列式向量化过滤
            let (cols, chunks) = match try_execute_chunks(input, db)? {
                Some(x) => x,
                None => return Ok(None),
            };
            let filtered = operators::filter::execute(&chunks, condition, &cols)?;
            Ok(Some((cols, filtered)))
        }
        PhysicalPlan::Sort {
            input,
            sort_keys,
            limit,
        } => {
            let (cols, chunks) = match try_execute_chunks(input, db)? {
                Some(x) => x,
                None => return Ok(None),
            };
            let sorted = operators::sort::execute(&chunks, sort_keys, *limit)?;
            Ok(Some((cols, sorted)))
        }
        PhysicalPlan::Limit { input, limit } => {
            let (cols, chunks) = match try_execute_chunks(input, db)? {
                Some(x) => x,
                None => return Ok(None),
            };
            let mut remaining = *limit;
            let mut out = Vec::new();
            for ch in chunks {
                if remaining == 0 {
                    break;
                }
                let take = ch.count.min(remaining);
                let columns: Vec<Vector> = ch
                    .columns
                    .iter()
                    .map(|c| match c {
                        Vector::Constant(v, _) => Vector::Constant(v.clone(), take),
                        Vector::Typed(d) => {
                            let mut d = d.clone();
                            Vector::Typed(d.take_front(take))
                        }
                        Vector::Flat(rows) => {
                            Vector::Flat(rows.iter().take(take).cloned().collect())
                        }
                    })
                    .collect();
                out.push(DataChunk {
                    columns,
                    count: take,
                });
                remaining -= take;
            }
            Ok(Some((cols, out)))
        }
        PhysicalPlan::Aggregate {
            input,
            group_by,
            aggregates,
        } => {
            let (cols, chunks) = match try_execute_chunks(input, db)? {
                Some(x) => x,
                None => return Ok(None),
            };
            let agg_funcs: Vec<_> = aggregates
                .iter()
                .map(|a| (a.func, a.input, a.distinct))
                .collect();
            let result = if group_by.is_empty() {
                operators::aggregate::execute(&chunks, &agg_funcs)?
            } else {
                operators::aggregate::execute_grouped(&chunks, group_by, &agg_funcs)?
                
            };
            let mut columns: Vec<String> = group_by
                .iter()
                .map(|&i| {
                    if i < cols.len() {
                        cols[i].clone()
                    } else {
                        format!("group_{}", i)
                    }
                })
                .collect();
            let agg_names: Vec<String> = aggregates
                .iter()
                .map(|a| format!("{:?}({})", a.func, a.input))
                .collect();
            columns.extend(agg_names);
            Ok(Some((columns, result)))
        }
        PhysicalPlan::CountStar { output_name, count } => {
            let chunk = DataChunk {
                columns: vec![Vector::Constant(Value::Int64(*count), 1)],
                count: 1,
            };
            Ok(Some((vec![output_name.clone()], vec![chunk])))
        }
        PhysicalPlan::SetUnion { op, left, right } => {
            if matches!(op, SetUnionOp::Union) {
                // UNION 去重需要行比较，回退行式路径
                return Ok(None);
            }
            let (lcols, mut chunks) = match try_execute_chunks(left, db)? {
                Some(x) => x,
                None => return Ok(None),
            };
            let (rcols, rchunks) = match try_execute_chunks(right, db)? {
                Some(x) => x,
                None => return Ok(None),
            };
            if lcols.len() != rcols.len() {
                return Err(EngramDbError::Parse(format!(
                    "UNION columns mismatch: {:?} vs {:?}",
                    lcols, rcols
                )));
            }
            chunks.extend(rchunks);
            Ok(Some((lcols, chunks)))
        }
        PhysicalPlan::SubqueryScan { plan } => try_execute_chunks(plan, db),
        _ => Ok(None),
    }
}

/// 截断 DataChunk 到前 n 行
#[allow(dead_code)]
fn truncate_chunk(chunk: &DataChunk, n: usize) -> DataChunk {
    let n = n.min(chunk.count);
    let columns: Vec<Vector> = chunk
        .columns
        .iter()
        .map(|c| match c {
            Vector::Constant(v, _) => Vector::Constant(v.clone(), n),
            Vector::Typed(d) => {
                let mut d = d.clone();
                Vector::Typed(d.take_front(n))
            }
            Vector::Flat(rows) => Vector::Flat(rows.iter().take(n).cloned().collect()),
        })
        .collect();
    DataChunk {
        columns,
        count: n,
    }
}

/// 执行物理计划
pub fn execute(plan: PhysicalPlan, db: &mut Database) -> Result<QueryResult> {
    // Q23：执行前解析计划树中的子查询表达式（IN/EXISTS/标量子查询）
    let plan = resolve_subqueries_in_plan(plan, db)?;
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
            let count = crate::executor::operators::insert::execute(db, &name, rows, false)?;

            Ok(QueryResult {
                columns: vec!["status".to_string()],
                rows: vec![vec![crate::Value::Varchar(format!(
                    "Table '{}' created with {} rows", name, count
                ))]],
                rows_affected: count,
            })
        }

        PhysicalPlan::CreateIndex { table_name, index_name, key_columns, included_columns, unique, using, with_options } => {
            if let Some(using_type) = using {
                if using_type.to_lowercase() == "hnsw" {
                    // 向量索引：解析 WITH 选项
                    let mut metric = crate::storage::vector_index::DistanceMetric::L2;
                    let mut m: usize = 16;
                    let mut ef_construction: usize = 100;
                    for (key, val) in with_options {
                        match key.as_str() {
                            "metric" => {
                                metric = match val.to_lowercase().as_str() {
                                    "l2" | "l2_distance" => crate::storage::vector_index::DistanceMetric::L2,
                                    "cosine" | "cosine_similarity" => crate::storage::vector_index::DistanceMetric::Cosine,
                                    "ip" | "inner_product" => crate::storage::vector_index::DistanceMetric::InnerProduct,
                                    _ => return Err(EngramDbError::Parse(format!("unknown metric: {}", val))),
                                };
                            }
                            "m" => {
                                m = val.parse().map_err(|_| EngramDbError::Parse(format!("invalid m: {}", val)))?;
                            }
                            "ef_construction" => {
                                ef_construction = val.parse().map_err(|_| EngramDbError::Parse(format!("invalid ef_construction: {}", val)))?;
                            }
                            _ => { /* ignore unknown options */ }
                        }
                    }
                    let col_name_str = {
                        let table = db.get_table(table_name.as_str())
                            .ok_or_else(|| EngramDbError::TableNotFound(table_name.clone()))?;
                        table.def.columns[key_columns[0]].name.clone()
                    };
                    db.create_vector_index(&table_name, &index_name, &col_name_str, metric, m, ef_construction)?;
                    Ok(QueryResult {
                        columns: vec!["status".to_string()],
                        rows: vec![vec![crate::Value::Varchar(format!(
                            "Vector index '{}' created on '{}' (metric={:?}, m={}, ef_construction={})",
                            index_name, table_name, metric, m, ef_construction
                        ))]],
                        rows_affected: 0,
                    })
                } else {
                    Err(EngramDbError::Parse(format!("unsupported index type: {}", using_type)))
                }
            } else {
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
                let t = db.get_engine_table(&table_name)
                    .ok_or_else(|| EngramDbError::TableNotFound(table_name.clone()))?;
                t.def().row_count as u32
            } else {
                0
            };
            let num_rows = rows.len();
            // RETURNING 需立即读回插入行：绕过攒批直接落盘
            let count = operators::insert::execute(db, &table_name, rows, returning.is_some())?;

            // INSERT...RETURNING: 从表中读取实际插入的行（含 AUTO_INCREMENT 值）
            if let Some(returning_items) = returning {
                let table = db.get_engine_table_mut(&table_name)
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
                                let val = evaluate_returning_expr(expr, &row, table.def())?;
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

            let count = operators::insert::execute(db, &table_name, rows, false)?;

            Ok(QueryResult {
                columns: vec!["rows_inserted".to_string()],
                rows: vec![vec![crate::Value::Int64(count as i64)]],
                rows_affected: count,
            })
        }

        PhysicalPlan::TableScan { table_name, column_indices } => {
            // 性能优化：直传路径（最常见场景 SELECT * / 简单 SELECT）
            // 跳过 DataChunk 中间层，直接产出行 Vec，避免 chunks_to_rows 的二次克隆
            // 引擎分派（M2：Memory 表走同语义扫描）
            let (columns, rows) = {
                let table = db.get_engine_table_mut(&table_name)
                    .ok_or_else(|| crate::common::error::EngramDbError::TableNotFound(table_name.clone()))?;
                let columns: Vec<String> = column_indices.iter()
                    .map(|&i| table.def().columns[i].name.clone())
                    .collect();
                let rows = match table {
                    crate::storage::engine::EngineTable::Columnar(t) => {
                        t.scan_to_rows_direct(&column_indices)?
                    }
                    crate::storage::engine::EngineTable::Memory(t) => {
                        t.scan_to_rows_direct(&column_indices, None)?
                    }
                    crate::storage::engine::EngineTable::Log(t) => {
                        t.scan_to_rows_direct(&column_indices, None)?
                    }
                };
                (columns, rows)
            };
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

        PhysicalPlan::IndexScan { table_name, index_name, key_value, output_column_indices } => {
            // P2：非覆盖索引点查 —— 索引拿 row_id，回表读列
            let (row_ids, columns): (Vec<u32>, Vec<String>) = {
                let table = db.get_table(&table_name)
                    .ok_or_else(|| crate::common::error::EngramDbError::TableNotFound(table_name.clone()))?;
                let cols: Vec<String> = output_column_indices.iter()
                    .map(|&i| table.def.columns[i].name.clone())
                    .collect();
                let index = table.get_index(&index_name)
                    .ok_or_else(|| crate::common::error::EngramDbError::Parse(
                        format!("Index '{}' not found during execution", index_name)
                    ))?;
                (index.get(&key_value).unwrap_or_default(), cols)
            };

            let mut rows: Vec<Vec<crate::Value>> = Vec::with_capacity(row_ids.len());
            {
                let table = db.get_table_mut(&table_name)
                    .ok_or_else(|| crate::common::error::EngramDbError::TableNotFound(table_name.clone()))?;
                for row_id in row_ids {
                    // 回表读取（列裁剪）
                    rows.extend(table.get_row_by_id_columns(row_id, &output_column_indices)?);
                }
            }

            Ok(QueryResult {
                columns,
                rows,
                rows_affected: 0,
            })
        }

        PhysicalPlan::IndexRangeScan {
            table_name, index_name,
            low, low_inclusive, high, high_inclusive,
            output_column_indices,
        } => {
            // ①：索引范围扫描 —— 跳表有序段取 row_id，回表读列
            let (row_ids, columns): (Vec<u32>, Vec<String>) = {
                let table = db.get_table(&table_name)
                    .ok_or_else(|| crate::common::error::EngramDbError::TableNotFound(table_name.clone()))?;
                let cols: Vec<String> = output_column_indices.iter()
                    .map(|&i| table.def.columns[i].name.clone())
                    .collect();
                let index = table.get_index(&index_name)
                    .ok_or_else(|| crate::common::error::EngramDbError::Parse(
                        format!("Index '{}' not found during execution", index_name)
                    ))?;
                let entries = index.range_bounded(
                    low.as_ref(), low_inclusive,
                    high.as_ref(), high_inclusive,
                );
                (entries.into_iter().map(|e| e.row_id).collect(), cols)
            };

            let mut rows: Vec<Vec<crate::Value>> = Vec::with_capacity(row_ids.len());
            {
                let table = db.get_table_mut(&table_name)
                    .ok_or_else(|| crate::common::error::EngramDbError::TableNotFound(table_name.clone()))?;
                for row_id in row_ids {
                    // 回表读取（列裁剪）
                    rows.extend(table.get_row_by_id_columns(row_id, &output_column_indices)?);
                }
            }

            Ok(QueryResult {
                columns,
                rows,
                rows_affected: 0,
            })
        }

        PhysicalPlan::Filter { input, condition } => {
            // P2.4/P3.2：若输入是 TableScan 且条件为简单比较谓词（col OP literal），
            // 用 MinMax 跳过索引扫描 + 逐行求值，跳过 DataChunk 中间层
            // （省去 rows→chunks→filter→rows 的整列克隆链）
            if let PhysicalPlan::TableScan { table_name, column_indices } = input.as_ref() {
                if let Some((pred_col, pred_op, pred_val)) = extract_skip_predicate(&condition) {
                    let (column_names, rows) = {
                        let table = db.get_engine_table_mut(table_name)
                            .ok_or_else(|| crate::common::error::EngramDbError::TableNotFound(table_name.clone()))?;
                        // 谓词列在表定义中的索引（MinMax 按表列索引判断）
                        let pred_col_idx = table.def().column_index(&pred_col);
                        // 谓词列在扫描输出中的位置（column_indices 顺序即输出顺序）
                        let pred_pos = pred_col_idx.and_then(|ci| column_indices.iter().position(|&c| c == ci));
                        let names: Vec<String> = column_indices.iter()
                            .map(|&i| table.def().columns[i].name.clone())
                            .collect();
                        let skip = pred_col_idx.map(|ci| (ci, pred_op, pred_val.clone()));
                        let scanned = match table {
                            crate::storage::engine::EngineTable::Columnar(t) => {
                                t.scan_to_rows_direct_with_skip(column_indices, skip)?
                            }
                            crate::storage::engine::EngineTable::Memory(t) => {
                                t.scan_to_rows_direct(column_indices, skip)?
                            }
                            crate::storage::engine::EngineTable::Log(t) => {
                                t.scan_to_rows_direct(column_indices, skip)?
                            }
                        };

                        // 逐行精确过滤（复用向量化求值的标量语义）
                        let rows: Vec<Vec<crate::Value>> = match pred_pos {
                            Some(pos) => {
                                // PredicateOp → BinaryOperator（eval_binary_value 的输入）
                                let bin_op = match pred_op {
                                    crate::storage::column_store::PredicateOp::Eq => crate::sql::ast::BinaryOperator::Eq,
                                    crate::storage::column_store::PredicateOp::Lt => crate::sql::ast::BinaryOperator::Lt,
                                    crate::storage::column_store::PredicateOp::LtEq => crate::sql::ast::BinaryOperator::LtEq,
                                    crate::storage::column_store::PredicateOp::Gt => crate::sql::ast::BinaryOperator::Gt,
                                    crate::storage::column_store::PredicateOp::GtEq => crate::sql::ast::BinaryOperator::GtEq,
                                };
                                let mut out = Vec::with_capacity(scanned.len());
                                for row in scanned {
                                    let keep = match super::expression::eval_binary_value(&row[pos], bin_op, &pred_val) {
                                        Ok(Value::Boolean(true)) => true,
                                        _ => false,
                                    };
                                    if keep {
                                        out.push(row);
                                    }
                                }
                                out
                            }
                            None => scanned,
                        };
                        (names, rows)
                    };

                    return Ok(QueryResult {
                        columns: column_names,
                        rows,
                        rows_affected: 0,
                    });
                }
            }

            // 常规路径：先执行子计划
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

            // ④：纯列引用投影 → rows 直排（跳过 DataChunk 往返 + 向量化求值）
            // 覆盖 SELECT a, b / SELECT b, a（列子集 + 重排，非恒等投影）等常见场景
            if !input_result.rows.is_empty()
                && expressions.iter().all(|e| matches!(e, crate::sql::ast::Expression::ColumnRef { .. }))
            {
                let col_indices: Vec<Option<usize>> = expressions.iter().map(|e| match e {
                    crate::sql::ast::Expression::ColumnRef { column, .. } => {
                        input_result.columns.iter().position(|c| c == column)
                    }
                    _ => None,
                }).collect();

                if col_indices.iter().all(|i| i.is_some()) {
                    let rows: Vec<Vec<crate::Value>> = input_result.rows.iter().map(|row| {
                        col_indices.iter()
                            .map(|&i| row[i.expect("checked above")].clone())
                            .collect()
                    }).collect();

                    return Ok(QueryResult {
                        columns: column_names,
                        rows,
                        rows_affected: 0,
                    });
                }
            }

            let input_chunks = rows_to_chunks(&input_result.rows);
            let input_columns = input_result.columns.clone();

            let projected = operators::projection::execute(
                &input_chunks, &expressions, &input_columns, &column_names
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

        PhysicalPlan::CrossJoin { left, right } => {
            let left_result = execute(*left, db)?;
            let right_result = execute(*right, db)?;

            // 笛卡尔积：左表每行 × 右表所有行
            let mut rows = Vec::new();
            let left_cols = left_result.columns.len();
            for lr in &left_result.rows {
                for rr in &right_result.rows {
                    let mut row = lr.clone();
                    row.extend(rr.clone());
                    rows.push(row);
                }
            }

            let mut columns = left_result.columns.clone();
            columns.extend(right_result.columns.clone());

            Ok(QueryResult {
                columns,
                rows,
                rows_affected: 0,
            })
        }

        PhysicalPlan::Aggregate { input, group_by, aggregates } => {
            // M1-3：列式路径（子计划列式执行 + 聚合），失败回退行式
            if let Some((sub_cols, chunks)) = try_execute_chunks(&input, db)? {
                let agg_funcs: Vec<_> = aggregates.iter()
                    .map(|a| (a.func, a.input, a.distinct))
                    .collect();

                let result = if group_by.is_empty() {
                    operators::aggregate::execute(&chunks, &agg_funcs)?
                } else {
                    operators::aggregate::execute_grouped(&chunks, &group_by, &agg_funcs)?
                };
                // 列名：分组列 + 聚合列
                let mut columns: Vec<String> = group_by.iter()
                    .map(|&i| {
                        if i < sub_cols.len() {
                            sub_cols[i].clone()
                        } else {
                            format!("group_{}", i)
                        }
                    })
                    .collect();
                let agg_names: Vec<String> = aggregates.iter()
                    .map(|a| format!("{:?}({})", a.func, a.input))
                    .collect();
                columns.extend(agg_names);
                let rows = chunks_to_rows(&result);
                return Ok(QueryResult {
                    columns,
                    rows,
                    rows_affected: 0,
                });
            }
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
            // M1-3：列式路径（跳过行→列→行双重转置），失败回退行式
            if let Some((columns, chunks)) = try_execute_chunks(&input, db)? {
                let sorted = operators::sort::execute(&chunks, &sort_keys, limit)?;
                let rows = chunks_to_rows(&sorted);
                return Ok(QueryResult {
                    columns,
                    rows,
                    rows_affected: 0,
                });
            }
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
                SetUnionOp::Intersect => {
                    // INTERSECT：返回两个结果集的交集（去重）
                    let right_set: std::collections::HashSet<Vec<Value>> =
                        right_result.rows.into_iter().collect();
                    rows.retain(|r| right_set.contains(r));
                    rows.dedup();
                }
                SetUnionOp::Except => {
                    // EXCEPT：返回左结果集减去右结果集（去重）
                    let right_set: std::collections::HashSet<Vec<Value>> =
                        right_result.rows.into_iter().collect();
                    rows.retain(|r| !right_set.contains(r));
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
            // 实际开启事务（v0.15.0 Txn05 新增）
            // 设置默认隔离级别为 SnapshotIsolation
            let txn_id = db.txn_manager_mut().begin(
                crate::common::config::IsolationLevel::SnapshotIsolation
            )?;
            // P0-2 事务级 Batcher：防御性清空（异常残留时避免跨事务串数据）
            db.discard_txn_buffer();
            db.set_current_txn_id(Some(txn_id));
            Ok(QueryResult {
                columns: vec!["status".to_string()],
                rows: vec![vec![crate::Value::Varchar(format!("BEGIN (txn={})", txn_id))]],
                rows_affected: 0,
            })
        }

        PhysicalPlan::Commit => {
            // 实际提交事务（v0.15.0 Txn05 新增）
            // P0-2 事务级 Batcher：先 flush 事务 buffer（读己之写 + 提交落盘）
            let result = if let Some(txn_id) = db.current_txn_id() {
                db.flush_txn_buffer()?;
                let r = db.txn_manager_mut().commit(txn_id)?;
                db.discard_txn_buffer();
                db.set_current_txn_id(None);
                Ok(r)
            } else {
                Err(crate::common::error::EngramDbError::Internal(
                    "No active transaction to COMMIT".into()
                ))
            };
            result.map(|_| QueryResult {
                columns: vec!["status".to_string()],
                rows: vec![vec![crate::Value::Varchar("COMMIT".to_string())]],
                rows_affected: 0,
            })
        }

        PhysicalPlan::Rollback => {
            // 实际回滚事务（v0.15.0 Txn05 新增）
            // P0-2 事务级 Batcher：丢弃 buffer（撤销未 flush 的写入段）
            let result = if let Some(txn_id) = db.current_txn_id() {
                db.txn_manager_mut().rollback(txn_id)?;
                db.discard_txn_buffer();
                db.set_current_txn_id(None);
                Ok(())
            } else {
                Err(crate::common::error::EngramDbError::Internal(
                    "No active transaction to ROLLBACK".into()
                ))
            };
            result.map(|_| QueryResult {
                columns: vec!["status".to_string()],
                rows: vec![vec![crate::Value::Varchar("ROLLBACK".to_string())]],
                rows_affected: 0,
            })
        }

        PhysicalPlan::Savepoint { name } => {
            // SAVEPOINT name（v0.15.0 Txn05 新增）
            // P0-2 事务级 Batcher：保存点前 flush（此后攒批归保存点后段，
            // ROLLBACK TO SAVEPOINT 丢弃之）
            let result = if let Some(txn_id) = db.current_txn_id() {
                db.flush_txn_buffer()?;
                db.txn_manager_mut().savepoint(txn_id, name.as_str())?;
                Ok(())
            } else {
                Err(crate::common::error::EngramDbError::Internal(
                    "No active transaction for SAVEPOINT".into()
                ))
            };
            result.map(|_| QueryResult {
                columns: vec!["status".to_string()],
                rows: vec![vec![crate::Value::Varchar(format!("SAVEPOINT {}", name))]],
                rows_affected: 0,
            })
        }

        PhysicalPlan::ReleaseSavepoint { name } => {
            // RELEASE SAVEPOINT name（v0.15.0 Txn05 新增）
            let result = if let Some(txn_id) = db.current_txn_id() {
                db.txn_manager_mut().release_savepoint(txn_id, name.as_str())?;
                Ok(())
            } else {
                Err(crate::common::error::EngramDbError::Internal(
                    "No active transaction for RELEASE SAVEPOINT".into()
                ))
            };
            result.map(|_| QueryResult {
                columns: vec!["status".to_string()],
                rows: vec![vec![crate::Value::Varchar(format!("RELEASE SAVEPOINT {}", name))]],
                rows_affected: 0,
            })
        }

        PhysicalPlan::RollbackToSavepoint { name } => {
            // ROLLBACK TO SAVEPOINT name（v0.15.0 Txn05 新增）
            // P0-2 事务级 Batcher：丢弃保存点后攒入 buffer 的写入段
            let result = if let Some(txn_id) = db.current_txn_id() {
                db.discard_txn_buffer();
                db.txn_manager_mut().rollback_to_savepoint(txn_id, name.as_str())?;
                Ok(())
            } else {
                Err(crate::common::error::EngramDbError::Internal(
                    "No active transaction for ROLLBACK TO SAVEPOINT".into()
                ))
            };
            result.map(|_| QueryResult {
                columns: vec!["status".to_string()],
                rows: vec![vec![crate::Value::Varchar(format!("ROLLBACK TO SAVEPOINT {}", name))]],
                rows_affected: 0,
            })
        }

        // V16: vector_search 表值函数
        PhysicalPlan::VectorSearch { table_name, index_name, query_vector, k } => {
            let neighbors = db.vector_search(&table_name, &index_name, query_vector.as_slice(), k)?;
            // 返回 (primary_key_value, distance) 格式
            let mut rows = Vec::with_capacity(neighbors.len());
            for n in &neighbors {
                let table = db.get_table_mut(&table_name)
                    .ok_or_else(|| crate::common::error::EngramDbError::TableNotFound(table_name.clone()))?;
                match table.get_row_by_id(n.id)? {
                    Some(row) => {
                        // 找到 PRIMARY KEY 列（第一列）的值
                        let pk_value = row.first().cloned().unwrap_or(crate::Value::Int32(n.id as i32));
                        rows.push(vec![
                            pk_value,
                            crate::Value::Float64(n.distance as f64),
                        ]);
                    }
                    None => {
                        rows.push(vec![
                            crate::Value::Int32(n.id as i32),
                            crate::Value::Float64(n.distance as f64),
                        ]);
                    }
                }
            }
            Ok(QueryResult {
                columns: vec!["row_id".into(), "distance".into()],
                rows,
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
        PhysicalPlan::PrimaryKeyLookup { table_name, pk_value, output_column_indices } => {
            // Phase 1：不可变借 -> 查主键索引拿 row_id + 列名（引擎分派）
            let (row_id_opt, columns): (Option<u32>, Vec<String>) = {
                let table = db.get_engine_table(&table_name)
                    .ok_or_else(|| crate::common::error::EngramDbError::TableNotFound(table_name.clone()))?;
                let cols: Vec<String> = if output_column_indices.is_empty() {
                    table.def().columns.iter().map(|c| c.name.clone()).collect()
                } else {
                    output_column_indices.iter().map(|&i| table.def().columns[i].name.clone()).collect()
                };
                (table.lookup_primary_key(&pk_value), cols)
            };
            // Phase 2：可变借 -> 回表读指定列（避免读无关列）
            let rows: Vec<Vec<crate::Value>> = match row_id_opt {
                Some(row_id) => {
                    let table = db.get_engine_table_mut(&table_name)
                        .ok_or_else(|| crate::common::error::EngramDbError::TableNotFound(table_name.clone()))?;
                    if output_column_indices.is_empty() {
                        match table.get_row_by_id(row_id)? {
                            Some(row) => vec![row],
                            None => Vec::new(),
                        }
                    } else {
                        table.get_row_by_id_columns(row_id, &output_column_indices)?
                    }
                }
                None => Vec::new(),
            };
            Ok(QueryResult {
                columns,
                rows,
                rows_affected: 0,
            })
        }

        // M5：ANALYZE 真实收集统计（引擎分派全表扫描 → 直方图/NDV 缓存）
        PhysicalPlan::Analyze { table_name, .. } => {
            use crate::sql::statistics::TableStatistics;
            let table = db.get_engine_table_mut(&table_name)
                .ok_or_else(|| EngramDbError::TableNotFound(table_name.clone()))?;
            let engine = table.def().engine;
            let col_names: Vec<String> = table.def().columns.iter().map(|c| c.name.clone()).collect();
            let col_indices: Vec<usize> = (0..col_names.len()).collect();
            let chunks = table.scan_to_chunks(&col_indices, None)?;
            let stats = TableStatistics::from_chunks(&table_name, engine, &col_names, &chunks, true);
            drop(table);
            db.statistics_cache_mut().insert(table_name.clone(), stats);
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
                let table = db.get_engine_table_mut(&table_name)
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
        PhysicalPlan::IndexScan { .. } => "IndexScan",
        PhysicalPlan::IndexRangeScan { .. } => "IndexRangeScan",
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
        PhysicalPlan::CrossJoin { .. } => "CrossJoin",
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
        PhysicalPlan::Savepoint { .. } => "Savepoint",
        PhysicalPlan::ReleaseSavepoint { .. } => "ReleaseSavepoint",
        PhysicalPlan::RollbackToSavepoint { .. } => "RollbackToSavepoint",
        PhysicalPlan::VectorSearch { .. } => "VectorSearch",
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
        PhysicalPlan::CrossJoin { left, right } => {
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

/// 测试辅助：chunks → 行（公开版，供集成测试使用）
pub fn debug_chunks_to_rows(chunks: &[DataChunk]) -> Vec<Vec<crate::Value>> {
    chunks_to_rows(chunks)
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

/// 从过滤表达式中提取可下推的比较谓词（P2.4 MinMax 跳过）
///
/// 仅支持 `col OP literal` 形式（OP ∈ {=, <, <=, >, >=}）。
/// 返回 `(列名, 谓词操作符, 比较值)`，列名由调用方映射为列索引。
fn extract_skip_predicate(
    condition: &Expression,
) -> Option<(String, crate::storage::column_store::PredicateOp, Value)> {
    use crate::sql::ast::BinaryOperator;

    let (left, op, right) = match condition {
        Expression::BinaryOp { left, op, right } => (left.as_ref(), *op, right.as_ref()),
        _ => return None,
    };

    let (col_name, val) = match (left, right) {
        (Expression::ColumnRef { column, .. }, Expression::Literal(v)) => (column, v),
        (Expression::Literal(v), Expression::ColumnRef { column, .. }) => (column, v),
        _ => return None,
    };

    let pred_op = match op {
        BinaryOperator::Eq => crate::storage::column_store::PredicateOp::Eq,
        BinaryOperator::Lt => crate::storage::column_store::PredicateOp::Lt,
        BinaryOperator::LtEq => crate::storage::column_store::PredicateOp::LtEq,
        BinaryOperator::Gt => crate::storage::column_store::PredicateOp::Gt,
        BinaryOperator::GtEq => crate::storage::column_store::PredicateOp::GtEq,
        _ => return None,
    };

    Some((col_name.clone(), pred_op, val.clone()))
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

    // M3：Log 引擎 —— 追加式引擎不支持 UPSERT（INSERT ... ON CONFLICT）
    if let Some(engine) = db.get_engine_table(table_name) {
        if matches!(engine, crate::storage::engine::EngineTable::Log(_)) {
            return Err(EngramDbError::NotSupported(
                "LogEngine 不支持 UPSERT（追加式时间序列引擎）".into(),
            ));
        }
    }

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
                    OnConflictAction::Replace => {
                        // INSERT OR REPLACE / REPLACE INTO：替换所有列
                        let table = db.get_table_mut(table_name)
                            .ok_or_else(|| EngramDbError::TableNotFound(table_name.to_string()))?;
                        table.update_row(rid, row.as_slice())?;
                        rows_affected += 1;
                    }
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

// ============================================================================
// Q23：子查询解析
// ============================================================================
// 在计划树中递归查找子查询表达式（Subquery/Exists/InSubquery），提前求值并
// 替换为字面量。子查询在执行前被求值，因为：
// 1. 简单子查询（IN/EXISTS/标量）不依赖外层查询的行
// 2. 避免在表达式执行器中引入子查询执行逻辑
// ============================================================================

/// 在计划树中递归解析所有子查询表达式
fn resolve_subqueries_in_plan(plan: PhysicalPlan, db: &mut Database) -> Result<PhysicalPlan> {
    match plan {
        PhysicalPlan::Filter { input, condition } => {
            let input = resolve_subqueries_in_plan(*input, db)?;
            let condition = resolve_subqueries_in_expr(condition, db)?;
            Ok(PhysicalPlan::Filter {
                input: Box::new(input),
                condition,
            })
        }
        PhysicalPlan::Projection { input, expressions, column_names } => {
            let input = resolve_subqueries_in_plan(*input, db)?;
            let mut resolved = Vec::with_capacity(expressions.len());
            for expr in expressions {
                resolved.push(resolve_subqueries_in_expr(expr, db)?);
            }
            Ok(PhysicalPlan::Projection {
                input: Box::new(input),
                expressions: resolved,
                column_names,
            })
        }
        PhysicalPlan::Aggregate { input, group_by, aggregates } => {
            let input = resolve_subqueries_in_plan(*input, db)?;
            Ok(PhysicalPlan::Aggregate { input: Box::new(input), group_by, aggregates })
        }
        PhysicalPlan::Sort { input, sort_keys, limit } => {
            let input = resolve_subqueries_in_plan(*input, db)?;
            Ok(PhysicalPlan::Sort { input: Box::new(input), sort_keys, limit })
        }
        PhysicalPlan::Limit { input, limit } => {
            let input = resolve_subqueries_in_plan(*input, db)?;
            Ok(PhysicalPlan::Limit { input: Box::new(input), limit })
        }
        PhysicalPlan::Window { input, window_functions, column_names } => {
            let input = resolve_subqueries_in_plan(*input, db)?;
            Ok(PhysicalPlan::Window { input: Box::new(input), window_functions, column_names })
        }
        PhysicalPlan::SubqueryScan { plan } => {
            let inner = resolve_subqueries_in_plan(*plan, db)?;
            Ok(PhysicalPlan::SubqueryScan { plan: Box::new(inner) })
        }
        PhysicalPlan::SetUnion { op, left, right } => {
            let left = resolve_subqueries_in_plan(*left, db)?;
            let right = resolve_subqueries_in_plan(*right, db)?;
            Ok(PhysicalPlan::SetUnion { op, left: Box::new(left), right: Box::new(right) })
        }
        PhysicalPlan::HashJoin { join_type, left, right, left_keys, right_keys } => {
            let left = resolve_subqueries_in_plan(*left, db)?;
            let right = resolve_subqueries_in_plan(*right, db)?;
            Ok(PhysicalPlan::HashJoin { join_type, left: Box::new(left), right: Box::new(right), left_keys, right_keys })
        }
        PhysicalPlan::CrossJoin { left, right } => {
            let left = resolve_subqueries_in_plan(*left, db)?;
            let right = resolve_subqueries_in_plan(*right, db)?;
            Ok(PhysicalPlan::CrossJoin { left: Box::new(left), right: Box::new(right) })
        }
        other => Ok(other),
    }
}

/// 在表达式中递归解析子查询节点
fn resolve_subqueries_in_expr(expr: Expression, db: &mut Database) -> Result<Expression> {
    match expr {
        Expression::Subquery(subquery) => {
            let plan = crate::sql::planner::plan(
                crate::sql::ast::Statement::Select(*subquery), db
            )?;
            let result = crate::executor::execute(plan, db)?;
            let val = result.rows.first()
                .and_then(|r| r.first().cloned())
                .unwrap_or(Value::Null);
            Ok(Expression::Literal(val))
        }
        Expression::Exists { subquery, negated } => {
            let plan = crate::sql::planner::plan(
                crate::sql::ast::Statement::Select(*subquery), db
            )?;
            let result = crate::executor::execute(plan, db)?;
            let exists = !result.rows.is_empty();
            let val = if negated { !exists } else { exists };
            Ok(Expression::Literal(Value::Boolean(val)))
        }
        Expression::InSubquery { expr, subquery, negated } => {
            let plan = crate::sql::planner::plan(
                crate::sql::ast::Statement::Select(*subquery), db
            )?;
            let result = crate::executor::execute(plan, db)?;
            let values: Vec<Expression> = result.rows.iter()
                .filter_map(|r| r.first().cloned())
                .map(Expression::Literal)
                .collect();
            let list = Expression::InList {
                expr,
                list: values,
            };
            if negated {
                Ok(Expression::UnaryOp {
                    op: crate::sql::ast::UnaryOperator::Not,
                    expr: Box::new(list),
                })
            } else {
                Ok(list)
            }
        }
        Expression::BinaryOp { left, op, right } => Ok(Expression::BinaryOp {
            left: Box::new(resolve_subqueries_in_expr(*left, db)?),
            op,
            right: Box::new(resolve_subqueries_in_expr(*right, db)?),
        }),
        Expression::UnaryOp { op, expr: inner } => Ok(Expression::UnaryOp {
            op,
            expr: Box::new(resolve_subqueries_in_expr(*inner, db)?),
        }),
        Expression::Function { name, args, distinct, count_star, over } => {
            let mut resolved = Vec::with_capacity(args.len());
            for arg in args {
                resolved.push(resolve_subqueries_in_expr(arg, db)?);
            }
            Ok(Expression::Function { name, args: resolved, distinct, count_star, over })
        }
        Expression::Like { expr, pattern } => Ok(Expression::Like {
            expr: Box::new(resolve_subqueries_in_expr(*expr, db)?),
            pattern: Box::new(resolve_subqueries_in_expr(*pattern, db)?),
        }),
        Expression::Case { when_then, else_expr } => {
            let mut resolved = Vec::with_capacity(when_then.len());
            for (w, t) in when_then {
                resolved.push((resolve_subqueries_in_expr(w, db)?, resolve_subqueries_in_expr(t, db)?));
            }
            let resolved_else = match else_expr {
                Some(e) => Some(Box::new(resolve_subqueries_in_expr(*e, db)?)),
                None => None,
            };
            Ok(Expression::Case { when_then: resolved, else_expr: resolved_else })
        }
        Expression::Cast { expr, data_type } => Ok(Expression::Cast {
            expr: Box::new(resolve_subqueries_in_expr(*expr, db)?),
            data_type,
        }),
        Expression::IsNull(inner) => Ok(Expression::IsNull(Box::new(resolve_subqueries_in_expr(*inner, db)?))),
        Expression::IsNotNull(inner) => Ok(Expression::IsNotNull(Box::new(resolve_subqueries_in_expr(*inner, db)?))),
        Expression::InList { expr, list } => {
            let mut resolved = Vec::with_capacity(list.len());
            for item in list {
                resolved.push(resolve_subqueries_in_expr(item, db)?);
            }
            Ok(Expression::InList { expr: Box::new(resolve_subqueries_in_expr(*expr, db)?), list: resolved })
        }
        other => Ok(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::physical_plan::PhysicalPlan;
    use crate::executor::vector::VECTOR_SIZE;
    use crate::sql::ast::{BinaryOperator, Expression, OnConflictClause};
    use crate::Value;

    fn col(name: &str) -> Expression {
        Expression::ColumnRef { table: None, column: name.into() }
    }

    fn lit(v: Value) -> Expression {
        Expression::Literal(v)
    }

    fn plan_scan() -> PhysicalPlan {
        PhysicalPlan::TableScan { table_name: "t".into(), column_indices: vec![0, 1] }
    }

    fn plan_filter() -> PhysicalPlan {
        PhysicalPlan::Filter {
            input: Box::new(plan_scan()),
            condition: Expression::BinaryOp {
                left: Box::new(col("id")),
                op: BinaryOperator::Gt,
                right: Box::new(lit(Value::Int64(1))),
            },
        }
    }

    #[test]
    fn test_plan_node_name() {
        assert_eq!(plan_node_name(&plan_scan()), "TableScan");
        assert_eq!(plan_node_name(&plan_filter()), "Filter");
        assert_eq!(plan_node_name(&PhysicalPlan::BeginTransaction), "BeginTransaction");
        assert_eq!(plan_node_name(&PhysicalPlan::Commit), "Commit");
        assert_eq!(plan_node_name(&PhysicalPlan::Rollback), "Rollback");
        assert_eq!(plan_node_name(&PhysicalPlan::Distinct { input: Box::new(plan_scan()) }), "Distinct");
        assert_eq!(plan_node_name(&PhysicalPlan::Window { input: Box::new(plan_scan()), window_functions: vec![], column_names: vec![] }), "Window");
        assert_eq!(plan_node_name(&PhysicalPlan::TruncateTable { table_name: "t".into() }), "TruncateTable");
        assert_eq!(plan_node_name(&PhysicalPlan::Savepoint { name: "sp".into() }), "Savepoint");
        assert_eq!(plan_node_name(&PhysicalPlan::ReleaseSavepoint { name: "sp".into() }), "ReleaseSavepoint");
        assert_eq!(plan_node_name(&PhysicalPlan::RollbackToSavepoint { name: "sp".into() }), "RollbackToSavepoint");
    }

    #[test]
    fn test_format_plan_tree_nested() {
        let plan = PhysicalPlan::Projection {
            input: Box::new(plan_filter()),
            expressions: vec![col("id")],
            column_names: vec!["id".into()],
        };
        let tree = format_plan_tree(&plan, 0);
        let lines: Vec<&str> = tree.trim().split('\n').collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "Projection [id]");
        assert_eq!(lines[1], "  Filter");
        assert_eq!(lines[2], "    TableScan [t.2 cols]");
    }

    #[test]
    fn test_format_plan_tree_detail_variants() {
        let cases: Vec<PhysicalPlan> = vec![
            PhysicalPlan::Insert { table_name: "t".into(), rows: vec![], returning: None, on_conflict: None },
            PhysicalPlan::InsertColumns { table_name: "t".into(), columns: vec![] },
            PhysicalPlan::Delete { table_name: "t".into(), condition: None },
            PhysicalPlan::Update { table_name: "t".into(), assignments: vec![], condition: None },
            PhysicalPlan::PrimaryKeyLookup { table_name: "t".into(), pk_value: Value::Int64(1), output_column_indices: vec![0] },
            PhysicalPlan::CountStar { output_name: "count(*)".into(), count: 42 },
            PhysicalPlan::Limit { input: Box::new(plan_scan()), limit: 5 },
            PhysicalPlan::Pragma(crate::sql::ast::PragmaStmt { name: "table_info".into(), arg: Some("t".into()) }),
            PhysicalPlan::AlterTable(crate::sql::ast::AlterTableStmt {
                table_name: "t".into(),
                operation: crate::sql::ast::AlterTableOp::RenameTable { new_name: "t2".into() },
            }),
            PhysicalPlan::BeginTransaction,
            PhysicalPlan::Commit,
            PhysicalPlan::Rollback,
        ];
        for plan in &cases {
            let tree = format_plan_tree(plan, 0);
            assert!(!tree.is_empty(), "{plan:?}");
        }
        let tree = format_plan_tree(&cases[0], 0);
        assert!(tree.contains("Insert [table: t]"), "{tree}");
        let tree = format_plan_tree(&cases[5], 0);
        assert!(tree.contains("CountStar [count(*): 42]"), "{tree}");
        let tree = format_plan_tree(&cases[6], 0);
        assert!(tree.contains("Limit [limit: 5]"), "{tree}");
    }

    #[test]
    fn test_format_plan_tree_joins_and_union() {
        let plan = PhysicalPlan::SetUnion {
            op: crate::executor::physical_plan::SetUnionOp::UnionAll,
            left: Box::new(plan_scan()),
            right: Box::new(plan_scan()),
        };
        let tree = format_plan_tree(&plan, 0);
        let lines: Vec<&str> = tree.trim().split('\n').collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "SetUnion");
        assert_eq!(lines[1], "  TableScan [t.2 cols]");
        assert_eq!(lines[2], "  TableScan [t.2 cols]");
    }

    #[test]
    fn test_chunks_roundtrip() {
        let rows: Vec<Vec<Value>> = (0..(VECTOR_SIZE + 3))
            .map(|i| vec![Value::Int64(i as i64), Value::Varchar(format!("v{i}"))])
            .collect();
        let chunks = rows_to_chunks(&rows);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].count, VECTOR_SIZE);
        assert_eq!(chunks[1].count, 3);
        let back = debug_chunks_to_rows(&chunks);
        assert_eq!(back.len(), rows.len());
        assert_eq!(back, rows);
        assert!(rows_to_chunks(&[]).is_empty());
        assert!(debug_chunks_to_rows(&[]).is_empty());
    }

    #[test]
    fn test_truncate_chunk() {
        use crate::executor::vector::{DataChunk, Vector};
        let chunk = DataChunk {
            columns: vec![
                Vector::Flat(vec![Value::Int64(1), Value::Int64(2), Value::Int64(3)]),
                Vector::Constant(Value::Int64(9), 3),
            ],
            count: 3,
        };
        let t = truncate_chunk(&chunk, 2);
        assert_eq!(t.count, 2);
        assert!(matches!(&t.columns[0], Vector::Flat(v) if v.len() == 2));
        let t = truncate_chunk(&chunk, 10);
        assert_eq!(t.count, 3);
        assert!(matches!(&t.columns[1], Vector::Constant(v, n) if *n == 3));
    }

    #[test]
    fn test_extract_skip_predicate() {
        use crate::storage::column_store::PredicateOp;
        let e = Expression::BinaryOp {
            left: Box::new(col("v")), op: BinaryOperator::Gt, right: Box::new(lit(Value::Int64(5))),
        };
        let (name, op, val) = extract_skip_predicate(&e).unwrap();
        assert_eq!(name, "v");
        assert_eq!(op, PredicateOp::Gt);
        assert_eq!(val, Value::Int64(5));
        let e = Expression::BinaryOp {
            left: Box::new(lit(Value::Int64(5))), op: BinaryOperator::Lt, right: Box::new(col("v")),
        };
        let (name, op, _) = extract_skip_predicate(&e).unwrap();
        assert_eq!(name, "v");
        assert_eq!(op, PredicateOp::Lt);
        let e = Expression::BinaryOp {
            left: Box::new(col("id")), op: BinaryOperator::Eq, right: Box::new(lit(Value::Int64(1))),
        };
        assert_eq!(extract_skip_predicate(&e).unwrap().1, PredicateOp::Eq);
        let e = Expression::BinaryOp {
            left: Box::new(col("v")), op: BinaryOperator::Plus, right: Box::new(lit(Value::Int64(1))),
        };
        assert!(extract_skip_predicate(&e).is_none());
        let e = Expression::BinaryOp {
            left: Box::new(col("a")), op: BinaryOperator::Eq, right: Box::new(col("b")),
        };
        assert!(extract_skip_predicate(&e).is_none());
        assert!(extract_skip_predicate(&lit(Value::Int64(1))).is_none());
    }

    #[test]
    fn test_execute_explain_plan() {
        let mut conn = crate::Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)").unwrap();
        let db = conn.database_mut();
        let r = execute_explain(plan_filter(), db).unwrap();
        assert_eq!(r.rows.len(), 1);
        let text = match &r.rows[0][0] { Value::Varchar(s) => s.clone(), other => panic!("{other:?}") };
        assert!(text.contains("Filter"), "{text}");
        assert!(text.contains("TableScan"), "{text}");
    }

    #[test]
    fn test_execute_create_table_plan() {
        let mut conn = crate::Connection::open(":memory:").unwrap();
        let db = conn.database_mut();
        let def = crate::common::types::TableDef {
            id: 0,
            name: "newt".into(),
            columns: vec![crate::common::types::ColumnDef {
                name: "id".into(), data_type: crate::common::types::DataType::Int64,
                nullable: false, is_primary_key: true, default_value: None, auto_increment: false,
            }],
            row_count: 0, indexes: vec![], cluster_key: None, foreign_keys: vec![],
            engine: crate::common::types::EngineType::Columnar,
            next_auto_increment_id: 0, ttl_seconds: None, ttl_column: None,
        };
        let r = execute(PhysicalPlan::CreateTable { table_def: def.clone() }, db).unwrap();
        assert!(r.rows[0][0] == Value::Varchar("Table 'newt' created".into()));
        assert!(db.get_table("newt").is_some());
        let err = execute(PhysicalPlan::CreateTable { table_def: def }, db).unwrap_err();
        assert!(matches!(err, crate::common::error::EngramDbError::ConstraintViolation(_)));
    }

    #[test]
    fn test_execute_insert_and_countstar() {
        let mut conn = crate::Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)").unwrap();
        let db = conn.database_mut();
        let r = execute(PhysicalPlan::Insert {
            table_name: "t".into(),
            rows: vec![vec![Value::Int64(1), Value::Int64(10)], vec![Value::Int64(2), Value::Int64(20)]],
            returning: None,
            on_conflict: None,
        }, db).unwrap();
        assert_eq!(r.rows_affected, 2);
        let r = execute(PhysicalPlan::CountStar { output_name: "count(*)".into(), count: 2 }, db).unwrap();
        assert_eq!(r.rows[0][0], Value::Int64(2));
        let err = execute(PhysicalPlan::Insert {
            table_name: "t".into(),
            rows: vec![vec![Value::Int64(1), Value::Int64(99)]],
            returning: None,
            on_conflict: None,
        }, db).unwrap_err();
        assert!(matches!(err, crate::common::error::EngramDbError::ConstraintViolation(_)));
    }

    #[test]
    fn test_execute_delete_update_plans() {
        let mut conn = crate::Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)").unwrap();
        conn.execute("INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)").unwrap();
        let db = conn.database_mut();
        let r = execute(PhysicalPlan::Update { table_name: "t".into(), assignments: vec![], condition: None }, db).unwrap();
        assert_eq!(r.rows_affected, 0);
        let r = execute(PhysicalPlan::Delete { table_name: "t".into(), condition: None }, db).unwrap();
        assert_eq!(r.rows_affected, 3);
        let rows = db.get_table_mut("t").unwrap().scan_to_rows_direct(&[0, 1]).unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn test_execute_begin_commit_rollback_plans() {
        let mut conn = crate::Connection::open(":memory:").unwrap();
        let db = conn.database_mut();
        execute(PhysicalPlan::BeginTransaction, db).unwrap();
        execute(PhysicalPlan::Commit, db).unwrap();
        execute(PhysicalPlan::BeginTransaction, db).unwrap();
        execute(PhysicalPlan::Rollback, db).unwrap();
    }

    #[test]
    fn test_execute_subquery_scan() {
        let mut conn = crate::Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)").unwrap();
        conn.execute("INSERT INTO t VALUES (1, 10), (2, 20)").unwrap();
        let db = conn.database_mut();
        let r = execute(PhysicalPlan::SubqueryScan { plan: Box::new(plan_scan()) }, db).unwrap();
        assert_eq!(r.rows.len(), 2);
    }

    #[test]
    fn test_upsert_on_conflict_do_nothing() {
        let mut conn = crate::Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)").unwrap();
        conn.execute("INSERT INTO t VALUES (1, 10)").unwrap();
        let db = conn.database_mut();
        let r = execute(PhysicalPlan::Insert {
            table_name: "t".into(),
            rows: vec![vec![Value::Int64(1), Value::Int64(99)], vec![Value::Int64(2), Value::Int64(20)]],
            returning: None,
            on_conflict: Some(OnConflictClause {
                conflict_columns: vec![],
                action: crate::sql::ast::OnConflictAction::DoNothing,
            }),
        }, db).unwrap();
        let rows = db.get_table_mut("t").unwrap().scan_to_rows_direct(&[0, 1]).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][1], Value::Int64(10));
    }
}


