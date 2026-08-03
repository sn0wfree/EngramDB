//! 查询规划器
//!
//! 将 AST 转换为物理执行计划。
//! 计划树结构：TableScan -> Filter -> Aggregate -> Projection -> OrderBy -> Limit

use crate::common::error::{EngramDbError, Result};
use crate::storage::Database;
use crate::Value;
use log::trace;

use super::ast::*;
use crate::executor::physical_plan::*;

/// 规划查询
pub fn plan(stmt: Statement, db: &Database) -> Result<PhysicalPlan> {
    match stmt {
        Statement::CreateTable(s) => plan_create_table(s),
        Statement::CreateIndex(s) => plan_create_index(s, db),
        Statement::Insert(s) => plan_insert(s, db, &[]),
        Statement::Select(s) => plan_select(s, db),
        Statement::Delete(s) => plan_delete(s, db),
        Statement::Update(s) => plan_update(s, db),
        Statement::BeginTransaction => Ok(PhysicalPlan::BeginTransaction),
        Statement::Commit => Ok(PhysicalPlan::Commit),
        Statement::Rollback => Ok(PhysicalPlan::Rollback),
        Statement::Analyze(s) => plan_analyze(s, db),
        Statement::CreateMaterializedView(s) => plan_create_mv(s, db),
        Statement::RefreshMaterializedView(s) => plan_refresh_mv(s),
        Statement::DropMaterializedView(s) => plan_drop_mv(s),
        Statement::AlterTable(s) => plan_alter_table(s),
        Statement::Pragma(s) => plan_pragma(s),
    }
}

fn plan_alter_table(stmt: AlterTableStmt) -> Result<PhysicalPlan> {
    Ok(PhysicalPlan::AlterTable(stmt))
}

fn plan_pragma(stmt: PragmaStmt) -> Result<PhysicalPlan> {
    Ok(PhysicalPlan::Pragma(stmt))
}

/// 带参数绑定的规划（用于 prepared statement）
pub fn plan_with_params(stmt: Statement, db: &Database, params: &[Value]) -> Result<PhysicalPlan> {
    match stmt {
        Statement::Insert(s) => plan_insert(s, db, params),
        // 其他语句暂不支持参数，走普通路径
        other => plan(other, db),
    }
}

fn plan_create_table(stmt: CreateTableStmt) -> Result<PhysicalPlan> {
    use crate::common::types::{ColumnDef, TableDef, DataType};

    let columns: Vec<ColumnDef> = stmt.columns
        .iter()
        .map(|c| {
            let mut col = ColumnDef::new(&c.name, c.data_type.clone());
            if c.primary_key {
                col = col.primary_key();
            }
            if !c.nullable {
                col = col.not_null();
            }
            col
        })
        .collect();

    let table_def = TableDef::new(0, &stmt.table_name, columns);

    Ok(PhysicalPlan::CreateTable { table_def })
}

/// 规划 CREATE INDEX（v0.12.0 新增，覆盖索引）
fn plan_create_index(stmt: CreateIndexStmt, db: &Database) -> Result<PhysicalPlan> {
    // 验证表存在
    let table = db.get_table(&stmt.table_name)
        .ok_or_else(|| EngramDbError::TableNotFound(stmt.table_name.clone()))?;

    // 解析键列 → 列索引
    let mut key_cols = Vec::with_capacity(stmt.key_columns.len());
    for col_name in &stmt.key_columns {
        let idx = table.def.column_index(col_name)
            .ok_or_else(|| EngramDbError::ColumnNotFound(format!(
                "index key column '{}' not found in table '{}'", col_name, stmt.table_name
            )))?;
        key_cols.push(idx);
    }

    // 解析覆盖列 → 列索引（INCLUDE 子句）
    let mut included_cols = Vec::with_capacity(stmt.included_columns.len());
    for col_name in &stmt.included_columns {
        let idx = table.def.column_index(col_name)
            .ok_or_else(|| EngramDbError::ColumnNotFound(format!(
                "included column '{}' not found in table '{}'", col_name, stmt.table_name
            )))?;
        // 覆盖列不能同时是键列
        if key_cols.contains(&idx) {
            return Err(EngramDbError::Parse(format!(
                "column '{}' cannot be both key and included in index '{}'",
                col_name, stmt.index_name
            )));
        }
        included_cols.push(idx);
    }

    Ok(PhysicalPlan::CreateIndex {
        table_name: stmt.table_name,
        index_name: stmt.index_name,
        key_columns: key_cols,
        included_columns: included_cols,
        unique: stmt.unique,
    })
}

/// 规划 DELETE 语句（v0.12.0 新增）
fn plan_delete(stmt: DeleteStmt, db: &Database) -> Result<PhysicalPlan> {
    // 验证表存在
    let _table = db.get_table(&stmt.table_name)
        .ok_or_else(|| EngramDbError::TableNotFound(stmt.table_name.clone()))?;

    Ok(PhysicalPlan::Delete {
        table_name: stmt.table_name,
        condition: stmt.where_clause,
    })
}

/// 规划 UPDATE 语句（v0.12.0 新增）
fn plan_update(stmt: UpdateStmt, db: &Database) -> Result<PhysicalPlan> {
    // 验证表存在
    let table = db.get_table(&stmt.table_name)
        .ok_or_else(|| EngramDbError::TableNotFound(stmt.table_name.clone()))?;

    // 解析 SET 子句中的列名 → 列索引
    let mut assignments = Vec::with_capacity(stmt.assignments.len());
    for (col_name, expr) in stmt.assignments {
        let col_idx = table.def.column_index(&col_name)
            .ok_or_else(|| EngramDbError::ColumnNotFound(format!(
                "update column '{}' not found in table '{}'", col_name, stmt.table_name
            )))?;
        assignments.push((col_idx, expr));
    }

    Ok(PhysicalPlan::Update {
        table_name: stmt.table_name,
        assignments,
        condition: stmt.where_clause,
    })
}

fn plan_insert(stmt: InsertStmt, db: &Database, params: &[Value]) -> Result<PhysicalPlan> {
    // 验证表存在
    let table = db.get_table(&stmt.table_name)
        .ok_or_else(|| EngramDbError::TableNotFound(stmt.table_name.clone()))?;

    let num_cols = table.def.columns.len();
    let num_rows = stmt.values.len();

    // 如果指定了列，预先计算列索引映射，避免逐行查找
    let col_map: Option<Vec<usize>> = stmt.columns.as_ref().map(|col_names| {
        col_names.iter()
            .filter_map(|name| table.def.column_index(name))
            .collect()
    });

    // 预分配 rows Vec
    let mut rows = Vec::with_capacity(num_rows);

    match &col_map {
        // 有列名重排：直接按目标列数构造行
        Some(indices) => {
            for value_row in &stmt.values {
                let mut full_row = vec![Value::Null; num_cols];
                for (i, expr) in value_row.iter().enumerate() {
                    if let Some(&idx) = indices.get(i) {
                        full_row[idx] = eval_constant_expr(expr, params)?;
                    }
                }
                rows.push(full_row);
            }
        }
        // 无列名重排：直接构造
        None => {
            for value_row in &stmt.values {
                let mut row = Vec::with_capacity(num_cols);
                for expr in value_row {
                    row.push(eval_constant_expr(expr, params)?);
                }
                rows.push(row);
            }
        }
    }

    Ok(PhysicalPlan::Insert {
        table_name: stmt.table_name,
        rows,
    })
}

fn plan_select(stmt: SelectStmt, db: &Database) -> Result<PhysicalPlan> {
    // ===== Perf01：COUNT(*) 元数据级短路 =====
    // 条件：单表、无 WHERE、无 GROUP BY、无 HAVING、无 ORDER BY、无 LIMIT
    // 且 SELECT 列表唯一一项为 COUNT(*) 或 COUNT(1) 等常量输入
    if stmt.where_clause.is_none()
        && stmt.group_by.is_empty()
        && stmt.having.is_none()
        && stmt.order_by.is_empty()
        && stmt.limit.is_none()
        && stmt.select_list.len() == 1
    {
        if let SelectItem::Expression(expr, alias) = &stmt.select_list[0] {
            if let Expression::Function { name, args, distinct: false, count_star } = expr {
                let is_count_star = *count_star
                    || (name.eq_ignore_ascii_case("COUNT")
                        && (args.is_empty()
                            || matches!(args.as_slice(), [Expression::Literal(_)])));
                if name.eq_ignore_ascii_case("COUNT") && is_count_star {
                    let table_name = stmt.from
                        .as_ref()
                        .map(|t| t.table_name.clone())
                        .ok_or_else(|| EngramDbError::Parse("SELECT without FROM not supported".into()))?;
                    let table = db.get_table(&table_name)
                        .ok_or_else(|| EngramDbError::TableNotFound(table_name.clone()))?;
                    let count = table.row_count() as i64;
                    let output_name = alias.clone().unwrap_or_else(|| "count(*)".to_string());
                    trace!("Perf01: COUNT(*) fast-path for '{}' => {}", table_name, count);
                    return Ok(PhysicalPlan::CountStar { output_name, count });
                }
            }
        }
    }

    let table_name = stmt.from
        .as_ref()
        .map(|t| t.table_name.clone())
        .ok_or_else(|| EngramDbError::Parse("SELECT without FROM not supported".into()))?;

    let table = db.get_table(&table_name)
        .ok_or_else(|| EngramDbError::TableNotFound(table_name.clone()))?;

    // ===== Perf03：主键点查短路（WHERE pk = Literal）=====
    // 条件：
    // 1. 表有 PRIMARY KEY 索引
    // 2. WHERE 唯一条件为 `pk_col = Literal`（BinaryEq）
    // 3. 无 GROUP BY / HAVING / ORDER BY / LIMIT（简化，后续可扩展）
    let mut pk_short_circuit: Option<crate::Value> = None;
    if table.has_primary_index()
        && stmt.group_by.is_empty()
        && stmt.having.is_none()
        && stmt.order_by.is_empty()
        && stmt.limit.is_none()
    {
        if let Some(ref where_expr) = stmt.where_clause {
            if let Expression::BinaryOp { left, op, right } = where_expr {
                if *op == BinaryOperator::Eq {
                    let pk_idx = table.def.primary_key_index().unwrap();
                    let pk_name = &table.def.columns[pk_idx].name;
                    let mut maybe_pk_value: Option<crate::Value> = None;
                    // 接受 (pk_col = literal) 或 (literal = pk_col)
                    match (left.as_ref(), right.as_ref()) {
                        (Expression::ColumnRef { column, .. }, Expression::Literal(v))
                            if column == pk_name =>
                        {
                            maybe_pk_value = Some(v.clone());
                        }
                        (Expression::Literal(v), Expression::ColumnRef { column, .. })
                            if column == pk_name =>
                        {
                            maybe_pk_value = Some(v.clone());
                        }
                        _ => {}
                    }
                    pk_short_circuit = maybe_pk_value;
                }
            }
        }
    }

    // 确定扫描的列（所有被引用的列）
    let all_referenced_cols = collect_referenced_columns(&stmt, &table.def.columns);
    let mut scan_column_indices: Vec<usize> = all_referenced_cols.iter()
        .filter_map(|name| table.def.column_index(name))
        .collect();

    // 纯聚合（如 COUNT(*)）不引用任何列时，至少扫描第一列用于计数
    let has_agg_in_select = select_list_has_aggregates(&stmt.select_list);
    if scan_column_indices.is_empty() && has_agg_in_select && !table.def.columns.is_empty() {
        scan_column_indices.push(0);
    }

    // ===== Perf03：主键短路时输出全列（Projection 裁剪列，避免列映射错乱）=====
    if pk_short_circuit.is_some() {
        scan_column_indices = (0..table.def.columns.len()).collect();
    }

    // 扫描阶段的列名映射（扫描输出的列名）
    let scan_column_names: Vec<String> = scan_column_indices.iter()
        .map(|&i| table.def.columns[i].name.clone())
        .collect();

    // ===== 覆盖索引优化（v0.12.0 新增）=====
    // 检测条件：
    // 1. WHERE 条件为单列等值比较（col = literal）
    // 2. 该列是某个索引的首键列
    // 3. 所有扫描列都在该索引的 key_columns + included_columns 中
    // 4. 无 GROUP BY / HAVING / ORDER BY（简化版，后续可扩展）
    let has_group_by = !stmt.group_by.is_empty();
    let has_having = stmt.having.is_some();
    let has_order_by = !stmt.order_by.is_empty();
    let has_agg = select_list_has_aggregates(&stmt.select_list);

    let can_use_index_only = !has_group_by && !has_having && !has_order_by && !has_agg;

    let had_pk = pk_short_circuit.is_some();
    let mut plan = if let Some(pk_val) = pk_short_circuit.take() {
        trace!("Perf03: PrimaryKeyLookup fast-path for '{}' pk={:?}", table_name, pk_val);
        PhysicalPlan::PrimaryKeyLookup {
            table_name: table_name.clone(),
            pk_value: pk_val,
        }
    } else if can_use_index_only {
        try_index_only_scan(&stmt, db, &table_name, &scan_column_indices)
            .unwrap_or_else(|| PhysicalPlan::TableScan {
                table_name: table_name.clone(),
                column_indices: scan_column_indices.clone(),
            })
    } else {
        PhysicalPlan::TableScan {
            table_name: table_name.clone(),
            column_indices: scan_column_indices.clone(),
        }
    };

    // Filter（WHERE）：主键短路已吸收 WHERE 条件，跳过
    if !had_pk {
        if let Some(where_expr) = stmt.where_clause {
            plan = PhysicalPlan::Filter {
                input: Box::new(plan),
                condition: where_expr,
            };
        }
    }

    // 检查是否需要聚合（GROUP BY 或 SELECT 中有聚合函数）
    let has_group_by = !stmt.group_by.is_empty();
    let has_agg_in_select = select_list_has_aggregates(&stmt.select_list);
    let has_having = stmt.having.is_some();
    let needs_aggregate = has_group_by || has_agg_in_select || has_having;

    // 聚合相关变量（在外部声明，供 ORDER BY 等后续阶段使用）
    let mut group_by_indices: Vec<usize> = Vec::new();
    let mut aggregates: Vec<AggregateExpr> = Vec::new();

    if needs_aggregate {
        // 解析 GROUP BY 列索引
        group_by_indices = stmt.group_by.iter()
            .filter_map(|expr| {
                if let Expression::ColumnRef { column, .. } = expr {
                    scan_column_names.iter().position(|c| c == column)
                } else {
                    None
                }
            })
            .collect();

        // 从 SELECT 列表中提取聚合表达式
        let (agg_exprs, _non_agg_exprs) = extract_aggregates_from_select(&stmt.select_list);

        aggregates = agg_exprs.iter()
            .filter_map(|(func_name, arg_expr, distinct)| {
                // 解析聚合函数的输入列索引
                let input_col = match arg_expr {
                    Expression::ColumnRef { column, .. } => {
                        scan_column_names.iter().position(|c| c == column)?
                    }
                    _ => 0, // 默认第 0 列（简化处理）
                };

                let func = match func_name.to_uppercase().as_str() {
                    "COUNT" => AggregateFunc::Count,
                    "SUM" => AggregateFunc::Sum,
                    "AVG" => AggregateFunc::Avg,
                    "MIN" => AggregateFunc::Min,
                    "MAX" => AggregateFunc::Max,
                    _ => return None,
                };

                Some(AggregateExpr { func, input: input_col, distinct: *distinct })
            })
            .collect();

        plan = PhysicalPlan::Aggregate {
            input: Box::new(plan),
            group_by: group_by_indices.clone(),
            aggregates: aggregates.clone(),
        };
    }

    // Projection（计算 SELECT 列表中的表达式）
    let (proj_expressions, proj_names) = plan_projection(
        &stmt.select_list,
        &scan_column_names,
        needs_aggregate,
        &table.def.columns,
    )?;

    if needs_aggregate {
        // 有聚合时，直接输出聚合结果（group_by 列 + 聚合列）
        // MVP 阶段简化：不做额外投影，列顺序由聚合执行器保证
    } else if !proj_expressions.is_empty() {
        // 无聚合时添加投影（确保列顺序与 SELECT 列表一致）
        plan = PhysicalPlan::Projection {
            input: Box::new(plan),
            expressions: proj_expressions.clone(),
            column_names: proj_names.clone(),
        };
    }

    // ORDER BY（v0.12.0 新增）
    if !stmt.order_by.is_empty() {
        // 确定当前计划的输出列名
        let output_columns = if needs_aggregate {
            // 聚合输出：group_by 列 + 聚合列
            let mut cols: Vec<String> = group_by_indices.iter()
                .map(|&i| {
                    if i < scan_column_names.len() {
                        scan_column_names[i].clone()
                    } else {
                        format!("group_{}", i)
                    }
                })
                .collect();
            // 聚合列名（简化处理）
            let agg_count = aggregates.len();
            for i in 0..agg_count {
                cols.push(format!("agg_{}", i));
            }
            cols
        } else {
            // 非聚合：用投影列名或扫描列名
            if !proj_expressions.is_empty() {
                proj_names.clone()
            } else {
                scan_column_names.clone()
            }
        };

        // 解析 ORDER BY 项 → 排序列索引
        let mut sort_keys = Vec::new();
        for item in &stmt.order_by {
            if let Expression::ColumnRef { column, .. } = &item.expr {
                if let Some(idx) = output_columns.iter().position(|c| c == column) {
                    sort_keys.push(crate::executor::physical_plan::SortKey {
                        column_index: idx,
                        direction: if item.ascending {
                            crate::executor::physical_plan::SortDirection::Asc
                        } else {
                            crate::executor::physical_plan::SortDirection::Desc
                        },
                    });
                }
            }
        }

        // 索引有序性优化（v0.12.0 新增）：
        // 如果扫描是 IndexOnlyScan 且 ORDER BY 列匹配索引键前缀，跳过 Sort
        let can_skip_sort = can_skip_sort_by_index(
            &plan,
            &sort_keys,
            &stmt.order_by,
            &table_name,
            db,
        );

        if !sort_keys.is_empty() && !can_skip_sort {
            plan = PhysicalPlan::Sort {
                input: Box::new(plan),
                sort_keys,
            };
        }
    }

    // Limit
    if let Some(limit) = stmt.limit {
        plan = PhysicalPlan::Limit {
            input: Box::new(plan),
            limit,
        };
    }

    Ok(plan)
}

/// 尝试生成 IndexOnlyScan 计划（v0.12.0 覆盖索引优化）
///
/// 成功返回 Some(plan)，失败返回 None（回退到全表扫描）。
fn try_index_only_scan(
    stmt: &SelectStmt,
    db: &Database,
    table_name: &str,
    scan_column_indices: &[usize],
) -> Option<PhysicalPlan> {
    // 必须有 WHERE 条件
    let where_expr = stmt.where_clause.as_ref()?;

    // 解析 WHERE 中的等值条件 col = value
    let (col_name, key_value) = extract_equality_condition(where_expr)?;

    // 查找表和匹配的索引
    let table = db.get_table(table_name)?;
    let col_idx = table.def.column_index(&col_name)?;

    for idx_def in &table.def.indexes {
        if idx_def.key_columns.first() == Some(&col_idx) {
            // 检查所有扫描列是否都在索引覆盖范围内（键列 + 覆盖列）
            let all_index_cols: std::collections::HashSet<usize> = idx_def.key_columns
                .iter()
                .chain(idx_def.included_columns.iter())
                .copied()
                .collect();

            let all_covered = scan_column_indices.iter().all(|c| all_index_cols.contains(c));
            if !all_covered {
                continue;
            }

            // 构建输出列到索引条目的位置映射
            let mut output_col_map = Vec::with_capacity(scan_column_indices.len());
            for &out_col in scan_column_indices {
                if out_col == idx_def.key_columns[0] {
                    output_col_map.push(0); // 0 表示 key 本身
                } else if let Some(pos) = idx_def.included_columns.iter().position(|&c| c == out_col) {
                    output_col_map.push(pos + 1); // 覆盖列：位置 = pos + 1
                } else {
                    return None;
                }
            }

            return Some(PhysicalPlan::IndexOnlyScan {
                table_name: table_name.to_string(),
                index_name: idx_def.name.clone(),
                key_value,
                output_column_indices: scan_column_indices.to_vec(),
                output_col_map,
            });
        }
    }

    None
}

/// 从 WHERE 表达式中提取单列等值条件（col = literal）
///
/// 成功返回 (列名, 值)，失败返回 None。
fn extract_equality_condition(expr: &Expression) -> Option<(String, Value)> {
    if let Expression::BinaryOp { left, op: BinaryOperator::Eq, right } = expr {
        match (left.as_ref(), right.as_ref()) {
            (Expression::ColumnRef { column, .. }, Expression::Literal(val)) => {
                Some((column.clone(), val.clone()))
            }
            (Expression::Literal(val), Expression::ColumnRef { column, .. }) => {
                Some((column.clone(), val.clone()))
            }
            _ => None,
        }
    } else {
        None
    }
}

/// 检查是否可以利用索引有序性跳过排序（v0.12.0 新增）
///
/// 当满足以下条件时，可以跳过 Sort 算子：
/// 1. 扫描使用了索引（IndexOnlyScan）
/// 2. ORDER BY 列的前缀与索引键列顺序一致
/// 3. 排序方向与索引有序方向一致（默认 ASC）
///
/// 目前索引为跳表，按键升序存储，因此 ASC 可直接利用，DESC 需反向扫描（暂不支持反向）。
fn can_skip_sort_by_index(
    plan: &PhysicalPlan,
    sort_keys: &[crate::executor::physical_plan::SortKey],
    _order_by_items: &[crate::sql::ast::OrderByItem],
    table_name: &str,
    db: &Database,
) -> bool {
    // 找到底层扫描节点（可能被 Filter / Projection 包裹）
    let scan_plan = find_scan_plan(plan);

    if let Some(PhysicalPlan::IndexOnlyScan { index_name, output_column_indices, .. }) = scan_plan {
        // 获取索引定义
        let table = match db.get_table(table_name) {
            Some(t) => t,
            None => return false,
        };
        let idx_def = match table.def.indexes.iter().find(|i| i.name == *index_name) {
            Some(idx) => idx,
            None => return false,
        };

        // 找到索引键列在输出列中的位置
        let key_col_in_output = output_column_indices.iter()
            .position(|&c| idx_def.key_columns.first() == Some(&c));

        if let Some(key_output_idx) = key_col_in_output {
            // 检查第一个排序键是否为索引键列且方向为 ASC
            // （跳表按键升序存储，仅 ASC 可直接利用）
            if !sort_keys.is_empty()
                && sort_keys[0].column_index == key_output_idx
                && sort_keys[0].direction == crate::executor::physical_plan::SortDirection::Asc
            {
                // 只有一个排序列 → 完全跳过排序
                if sort_keys.len() == 1 {
                    return true;
                }
                // 多个排序列：第一列有序但后续列仍需排序 → 不能完全跳过
                // （未来可优化为：保留 Sort 但利用第一列有序性做归并排序）
                return false;
            }
        }
    }

    false
}

/// 从计划树中找到底层扫描节点
fn find_scan_plan(plan: &PhysicalPlan) -> Option<&PhysicalPlan> {
    match plan {
        PhysicalPlan::TableScan { .. } | PhysicalPlan::IndexOnlyScan { .. } => Some(plan),
        PhysicalPlan::Filter { input, .. } => find_scan_plan(input),
        PhysicalPlan::Projection { input, .. } => find_scan_plan(input),
        PhysicalPlan::Aggregate { input, .. } => find_scan_plan(input),
        PhysicalPlan::Sort { input, .. } => find_scan_plan(input),
        PhysicalPlan::Limit { input, .. } => find_scan_plan(input),
        _ => None,
    }
}

/// 收集 SELECT 语句中所有被引用的列名
fn collect_referenced_columns(stmt: &SelectStmt, table_cols: &[crate::common::types::ColumnDef]) -> Vec<String> {
    let mut cols = std::collections::HashSet::new();

    // SELECT 列表
    for item in &stmt.select_list {
        match item {
            SelectItem::Wildcard => {
                for col in table_cols {
                    cols.insert(col.name.clone());
                }
            }
            SelectItem::Expression(expr, _) => {
                collect_expr_columns(expr, &mut cols);
            }
        }
    }

    // WHERE
    if let Some(expr) = &stmt.where_clause {
        collect_expr_columns(expr, &mut cols);
    }

    // GROUP BY
    for expr in &stmt.group_by {
        collect_expr_columns(expr, &mut cols);
    }

    // HAVING
    if let Some(expr) = &stmt.having {
        collect_expr_columns(expr, &mut cols);
    }

    // ORDER BY
    for item in &stmt.order_by {
        collect_expr_columns(&item.expr, &mut cols);
    }

    cols.into_iter().collect()
}

/// 递归收集表达式中的列引用
fn collect_expr_columns(expr: &Expression, cols: &mut std::collections::HashSet<String>) {
    match expr {
        Expression::ColumnRef { column, .. } => {
            cols.insert(column.clone());
        }
        Expression::BinaryOp { left, right, .. } => {
            collect_expr_columns(left, cols);
            collect_expr_columns(right, cols);
        }
        Expression::UnaryOp { expr, .. } => {
            collect_expr_columns(expr, cols);
        }
        Expression::Function { args, .. } => {
            for arg in args {
                collect_expr_columns(arg, cols);
            }
        }
        Expression::Cast { expr, .. } => {
            collect_expr_columns(expr, cols);
        }
        Expression::IsNull(expr) | Expression::IsNotNull(expr) => {
            collect_expr_columns(expr, cols);
        }
        Expression::InList { expr, list } => {
            collect_expr_columns(expr, cols);
            for item in list {
                collect_expr_columns(item, cols);
            }
        }
        Expression::Like { expr, pattern } => {
            collect_expr_columns(expr, cols);
            collect_expr_columns(pattern, cols);
        }
        Expression::Case { when_then, else_expr } => {
            for (when, then) in when_then {
                collect_expr_columns(when, cols);
                collect_expr_columns(then, cols);
            }
            if let Some(e) = else_expr {
                collect_expr_columns(e, cols);
            }
        }
        Expression::Literal(_) => {}
        Expression::Placeholder(_) => {}
    }
}

/// 检查 SELECT 列表中是否包含聚合函数
fn select_list_has_aggregates(items: &[SelectItem]) -> bool {
    items.iter().any(|item| {
        match item {
            SelectItem::Expression(expr, _) => expr_has_aggregate(expr),
            SelectItem::Wildcard => false,
        }
    })
}

/// 检查表达式中是否包含聚合函数
fn expr_has_aggregate(expr: &Expression) -> bool {
    match expr {
        Expression::Function { name, .. } => {
            matches!(
                name.to_uppercase().as_str(),
                "COUNT" | "SUM" | "AVG" | "MIN" | "MAX"
            )
        }
        Expression::BinaryOp { left, right, .. } => {
            expr_has_aggregate(left) || expr_has_aggregate(right)
        }
        Expression::UnaryOp { expr, .. } => expr_has_aggregate(expr),
        Expression::Cast { expr, .. } => expr_has_aggregate(expr),
        Expression::Case { when_then, else_expr } => {
            when_then.iter().any(|(w, t)| expr_has_aggregate(w) || expr_has_aggregate(t))
                || else_expr.as_ref().map(|e| expr_has_aggregate(e)).unwrap_or(false)
        }
        _ => false,
    }
}

/// 从 SELECT 列表中提取聚合函数
fn extract_aggregates_from_select(items: &[SelectItem]) -> (Vec<(String, Expression, bool)>, Vec<Expression>) {
    let mut aggs = Vec::new();
    let mut non_aggs = Vec::new();

    for item in items {
        match item {
            SelectItem::Expression(expr, _) => {
                if let Expression::Function { name, args, distinct, .. } = expr {
                    if matches!(
                        name.to_uppercase().as_str(),
                        "COUNT" | "SUM" | "AVG" | "MIN" | "MAX"
                    ) {
                        let arg = args.first().cloned().unwrap_or(Expression::Literal(Value::Null));
                        aggs.push((name.clone(), arg, *distinct));
                        continue;
                    }
                }
                non_aggs.push(expr.clone());
            }
            SelectItem::Wildcard => {}
        }
    }

    (aggs, non_aggs)
}

/// 规划投影表达式和列名
fn plan_projection(
    items: &[SelectItem],
    scan_column_names: &[String],
    _has_aggregate: bool,
    _table_cols: &[crate::common::types::ColumnDef],
) -> Result<(Vec<Expression>, Vec<String>)> {
    let mut expressions = Vec::new();
    let mut names = Vec::new();

    for item in items {
        match item {
            SelectItem::Wildcard => {
                // 展开为所有列引用
                for (i, col_name) in scan_column_names.iter().enumerate() {
                    expressions.push(Expression::ColumnRef {
                        table: None,
                        column: col_name.clone(),
                    });
                    names.push(col_name.clone());
                    let _ = i; // 避免未使用警告
                }
            }
            SelectItem::Expression(expr, alias) => {
                let name = alias.clone().unwrap_or_else(|| {
                    // 生成默认列名
                    match expr {
                        Expression::ColumnRef { column, .. } => column.clone(),
                        Expression::Function { name, .. } => name.clone(),
                        _ => "?column?".to_string(),
                    }
                });
                expressions.push(expr.clone());
                names.push(name);
            }
        }
    }

    Ok((expressions, names))
}

/// 求值常量表达式（支持参数占位符替换）
fn eval_constant_expr(expr: &Expression, params: &[Value]) -> Result<Value> {
    match expr {
        Expression::Literal(v) => Ok(v.clone()),
        Expression::Placeholder(idx) => {
            params.get(*idx)
                .cloned()
                .ok_or_else(|| EngramDbError::Parse(
                    format!("Parameter index {} out of bounds ({} params provided)", idx, params.len())
                ))
        }
        _ => Err(EngramDbError::Parse(
            "Non-constant expression in VALUES not supported".into()
        )),
    }
}


// ============================================================
// ANALYZE 规划
// ============================================================

fn plan_analyze(stmt: AnalyzeStmt, db: &Database) -> Result<PhysicalPlan> {
    // 验证表存在
    let table = db.get_table(&stmt.table_name)
        .ok_or_else(|| EngramDbError::TableNotFound(stmt.table_name.clone()))?;

    // 确定要分析的列索引
    let column_indices = if stmt.columns.is_empty() {
        // 所有列
        (0..table.def.columns.len()).collect()
    } else {
        let mut indices = Vec::new();
        for col_name in &stmt.columns {
            let idx = table.def.columns.iter()
                .position(|c| c.name == *col_name)
                .ok_or_else(|| EngramDbError::Internal(
                    format!("column '{}' not found in table '{}'", col_name, stmt.table_name)
                ))?;
            indices.push(idx);
        }
        indices
    };

    Ok(PhysicalPlan::Analyze {
        table_name: stmt.table_name,
        column_indices,
    })
}

// ============================================================
// 物化视图规划
// ============================================================

fn plan_create_mv(stmt: CreateMaterializedViewStmt, db: &Database) -> Result<PhysicalPlan> {
    // 规划查询部分
    let query_plan = plan_select(*stmt.query, db)?;

    // 从查询计划中提取列名（用于物化视图的 schema）
    let column_names = extract_column_names(&query_plan);

    Ok(PhysicalPlan::CreateMaterializedView {
        view_name: stmt.view_name,
        query: Box::new(query_plan),
        column_names,
        with_data: stmt.with_data,
    })
}

fn plan_refresh_mv(stmt: RefreshMaterializedViewStmt) -> Result<PhysicalPlan> {
    Ok(PhysicalPlan::RefreshMaterializedView {
        view_name: stmt.view_name,
        concurrently: stmt.concurrently,
    })
}

fn plan_drop_mv(stmt: DropMaterializedViewStmt) -> Result<PhysicalPlan> {
    Ok(PhysicalPlan::DropMaterializedView {
        view_name: stmt.view_name,
        if_exists: stmt.if_exists,
    })
}

/// 从物理计划中提取输出列名
fn extract_column_names(plan: &PhysicalPlan) -> Vec<String> {
    match plan {
        PhysicalPlan::Projection { column_names, .. } => column_names.clone(),
        PhysicalPlan::TableScan { table_name, column_indices } => {
            // TableScan 没有列名信息，生成占位名
            column_indices.iter()
                .map(|i| format!("{}_col{}", table_name, i))
                .collect()
        }
        PhysicalPlan::Filter { input, .. } => extract_column_names(input),
        PhysicalPlan::Aggregate { input, aggregates, group_by } => {
            let input_names = extract_column_names(input);
            let mut names = Vec::new();
            // GROUP BY 列
            for &idx in group_by {
                if idx < input_names.len() {
                    names.push(input_names[idx].clone());
                } else {
                    names.push(format!("group_{}", idx));
                }
            }
            // 聚合列
            for (i, _agg) in aggregates.iter().enumerate() {
                names.push(format!("agg_{}", i));
            }
            names
        }
        PhysicalPlan::HashJoin { left, right, .. } => {
            let mut left_names = extract_column_names(left);
            let right_names = extract_column_names(right);
            left_names.extend(right_names);
            left_names
        }
        PhysicalPlan::Limit { input, .. } => extract_column_names(input),
        _ => vec![],
    }
}
