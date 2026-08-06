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

/// 窗口函数名列表
const WINDOW_FUNCTIONS: &[&str] = &[
    "ROW_NUMBER", "RANK", "DENSE_RANK", "LAG", "LEAD",
    "FIRST_VALUE", "LAST_VALUE", "NTH_VALUE",
];

/// 规划查询
pub fn plan(stmt: Statement, db: &Database) -> Result<PhysicalPlan> {
    match stmt {
        Statement::CreateTable(s) => plan_create_table(s, db),
        Statement::CreateIndex(s) => plan_create_index(s, db),
        Statement::Insert(s) => plan_insert(s, db, &[]),
        Statement::Select(s) => plan_select(s, db),
        Statement::Delete(s) => plan_delete(s, db),
        Statement::Update(s) => plan_update(s, db),
        Statement::BeginTransaction => Ok(PhysicalPlan::BeginTransaction),
        Statement::Commit => Ok(PhysicalPlan::Commit),
        Statement::Rollback => Ok(PhysicalPlan::Rollback),
        Statement::Savepoint { name } => Ok(PhysicalPlan::Savepoint { name }),
        Statement::ReleaseSavepoint { name } => Ok(PhysicalPlan::ReleaseSavepoint { name }),
        Statement::RollbackToSavepoint { name } => Ok(PhysicalPlan::RollbackToSavepoint { name }),
        Statement::Analyze(s) => plan_analyze(s, db),
        Statement::CreateMaterializedView(s) => plan_create_mv(s, db),
        Statement::RefreshMaterializedView(s) => plan_refresh_mv(s),
        Statement::DropMaterializedView(s) => plan_drop_mv(s),
        Statement::AlterTable(s) => plan_alter_table(s, db),
        Statement::Pragma(s) => plan_pragma(s),
        Statement::Explain(s) => plan_explain(s, db),
        Statement::TruncateTable { table_name } => Ok(PhysicalPlan::TruncateTable { table_name }),
    }
}

/// M5：引擎能力校验（planner 提前拦截，执行期不再深挖报错）
fn ensure_engine_capability(
    db: &Database,
    table_name: &str,
    capability: &str,
    supported: impl Fn(&crate::storage::capabilities::EngineCapabilities) -> bool,
) -> Result<()> {
    let Some(table) = db.get_engine_table(table_name) else {
        return Ok(()); // 表不存在由上层抛 TableNotFound
    };
    let caps = crate::storage::capabilities::EngineCapabilities::for_engine(table.def().engine);
    caps.ensure(capability, supported(&caps), table_name)
}

fn plan_alter_table(stmt: AlterTableStmt, db: &Database) -> Result<PhysicalPlan> {
    ensure_engine_capability(db, &stmt.table_name, "ALTER TABLE", |c| c.supports_alter)?;
    Ok(PhysicalPlan::AlterTable(stmt))
}

fn plan_pragma(stmt: PragmaStmt) -> Result<PhysicalPlan> {
    Ok(PhysicalPlan::Pragma(stmt))
}

fn plan_explain(stmt: ExplainStmt, db: &Database) -> Result<PhysicalPlan> {
    let inner_plan = plan(*stmt.statement, db)?;
    Ok(PhysicalPlan::Explain {
        analyze: stmt.analyze,
        plan: Box::new(inner_plan),
    })
}

/// CTE 内联：将 WITH 子句中的 CTE 定义递归内联到查询中
fn inline_ctes(stmt: SelectStmt, _db: &Database) -> SelectStmt {
    if stmt.ctes.is_empty() {
        return stmt;
    }

    // 构建 CTE 名 → 查询的映射
    let cte_map: std::collections::HashMap<String, SelectStmt> = stmt.ctes.iter()
        .map(|cte| (cte.alias.clone(), *cte.query.clone()))
        .collect();

    // 递归替换查询中的 CTE 表引用为内联的子查询
    fn replace_cte_refs(select: SelectStmt, cte_map: &std::collections::HashMap<String, SelectStmt>) -> SelectStmt {
        let from = select.from.map(|tr| match tr {
            TableRef::Table { table_name, alias } => {
                if let Some(cte_query) = cte_map.get(&table_name) {
                    TableRef::Derived {
                        query: Box::new(replace_cte_refs(cte_query.clone(), cte_map)),
                        alias: alias.unwrap_or_else(|| table_name.clone()),
                    }
                } else {
                    TableRef::Table { table_name, alias }
                }
            }
            other => other,
        });
        SelectStmt {
            from,
            ctes: vec![],
            ..select
        }
    }

    replace_cte_refs(stmt, &cte_map)
}

/// 检查表达式中是否包含窗口函数
fn expr_has_window(expr: &Expression) -> bool {
    match expr {
        Expression::Function { over, .. } => over.is_some(),
        _ => false,
    }
}

/// 提取窗口函数信息
fn extract_window_functions(
    expr: &Expression,
    alias: &Option<String>,
    column_names: &[String],
    table_columns: &[crate::common::types::ColumnDef],
    funcs: &mut Vec<WindowFunctionExpr>,
) {
    match expr {
        Expression::Function { name, args, over: Some(ws), .. } => {
            let func_name = name.to_uppercase();
            let func_type = match func_name.as_str() {
                "ROW_NUMBER" => Some(WindowFuncType::RowNumber),
                "RANK" => Some(WindowFuncType::Rank),
                "DENSE_RANK" => Some(WindowFuncType::DenseRank),
                "LAG" => Some(WindowFuncType::Lag(1)),
                "LEAD" => Some(WindowFuncType::Lead(1)),
                "FIRST_VALUE" => Some(WindowFuncType::FirstValue),
                "LAST_VALUE" => Some(WindowFuncType::LastValue),
                "NTH_VALUE" => Some(WindowFuncType::NthValue(1)),
                "COUNT" => Some(WindowFuncType::Count),
                "SUM" => Some(WindowFuncType::Sum),
                "AVG" => Some(WindowFuncType::Avg),
                "MIN" => Some(WindowFuncType::Min),
                "MAX" => Some(WindowFuncType::Max),
                _ => None,
            };
            if let Some(ft) = func_type {
                let input_column = if !args.is_empty() {
                    if let Some(arg) = args.first() {
                        if let Expression::ColumnRef { column, .. } = arg {
                            column_names.iter().position(|c| c == column)
                                .or_else(|| table_columns.iter().position(|c| c.name == *column))
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                } else {
                    None
                };
                let output_name = alias.clone().unwrap_or_else(|| func_name.to_lowercase());
                funcs.push(WindowFunctionExpr {
                    func: ft,
                    input_column,
                    window_spec: ws.clone(),
                    output_name,
                });
            }
        }
        _ => {}
    }
}

/// 带参数绑定的规划（用于 prepared statement）
pub fn plan_with_params(stmt: Statement, db: &Database, params: &[Value]) -> Result<PhysicalPlan> {
    if params.is_empty() {
        return plan(stmt, db);
    }
    match stmt {
        Statement::Insert(s) => plan_insert(s, db, params),
        Statement::Select(s) => plan(substitute_params_stmt(Statement::Select(s), params), db),
        Statement::Delete(s) => plan(substitute_params_stmt(Statement::Delete(s), params), db),
        Statement::Update(s) => plan(substitute_params_stmt(Statement::Update(s), params), db),
        other => plan(other, db),
    }
}

fn substitute_params_expr(expr: &Expression, params: &[Value]) -> Expression {
    match expr {
        Expression::Placeholder(idx) => {
            params.get(*idx)
                .map(|v| Expression::Literal(v.clone()))
                .unwrap_or_else(|| Expression::Placeholder(*idx))
        }
        Expression::BinaryOp { left, op, right } => Expression::BinaryOp {
            left: Box::new(substitute_params_expr(left, params)),
            op: *op,
            right: Box::new(substitute_params_expr(right, params)),
        },
        Expression::UnaryOp { op, expr } => Expression::UnaryOp {
            op: *op,
            expr: Box::new(substitute_params_expr(expr, params)),
        },
        Expression::Function { name, args, distinct, count_star, over } => Expression::Function {
            name: name.clone(),
            args: args.iter().map(|a| substitute_params_expr(a, params)).collect(),
            distinct: *distinct,
            count_star: *count_star,
            over: over.clone(),
        },
        Expression::Cast { expr, data_type } => Expression::Cast {
            expr: Box::new(substitute_params_expr(expr, params)),
            data_type: data_type.clone(),
        },
        Expression::InList { expr, list } => Expression::InList {
            expr: Box::new(substitute_params_expr(expr, params)),
            list: list.iter().map(|e| substitute_params_expr(e, params)).collect(),
        },
        Expression::Like { expr, pattern } => Expression::Like {
            expr: Box::new(substitute_params_expr(expr, params)),
            pattern: Box::new(substitute_params_expr(pattern, params)),
        },
        Expression::Case { when_then, else_expr } => Expression::Case {
            when_then: when_then.iter().map(|(w, t)| {
                (substitute_params_expr(w, params), substitute_params_expr(t, params))
            }).collect(),
            else_expr: else_expr.as_ref().map(|e| Box::new(substitute_params_expr(e, params))),
        },
        Expression::IsNull(expr) => Expression::IsNull(Box::new(substitute_params_expr(expr, params))),
        other => other.clone(),
    }
}

fn substitute_params_stmt(stmt: Statement, params: &[Value]) -> Statement {
    match stmt {
        Statement::Select(mut s) => {
            s.where_clause = s.where_clause.map(|w| substitute_params_expr(&w, params));
            s.group_by = s.group_by.iter().map(|e| substitute_params_expr(e, params)).collect();
            s.having = s.having.map(|h| substitute_params_expr(&h, params));
            for item in &mut s.select_list {
                if let SelectItem::Expression(ref mut expr, _) = item {
                    *expr = substitute_params_expr(expr, params);
                }
            }
            for item in &mut s.order_by {
                item.expr = substitute_params_expr(&item.expr, params);
            }
            if let Some((_, ref mut right)) = s.set_op {
                let right_sub = substitute_params_stmt(Statement::Select(*right.clone()), params);
                if let Statement::Select(new_right) = right_sub {
                    *right = Box::new(new_right);
                }
            }
            Statement::Select(s)
        }
        // DELETE / UPDATE：WHERE 与 SET 参数替换（此前被静默跳过）
        Statement::Delete(mut s) => {
            s.where_clause = s.where_clause.map(|w| substitute_params_expr(&w, params));
            Statement::Delete(s)
        }
        Statement::Update(mut s) => {
            s.where_clause = s.where_clause.map(|w| substitute_params_expr(&w, params));
            s.assignments = s.assignments.iter().map(|(c, e)| {
                (c.clone(), substitute_params_expr(e, params))
            }).collect();
            Statement::Update(s)
        }
        other => other,
    }
}

fn plan_create_table(stmt: CreateTableStmt, db: &Database) -> Result<PhysicalPlan> {
    use crate::common::types::{ColumnDef, TableDef, DataType};

    // CREATE TABLE AS SELECT：列名从查询结果的投影名推断
    let columns: Vec<ColumnDef> = if !stmt.columns.is_empty() {
        stmt.columns
            .iter()
            .map(|c| {
                let mut col = ColumnDef::new(&c.name, c.data_type.clone());
                if c.primary_key {
                    col = col.primary_key();
                }
                if !c.nullable {
                    col = col.not_null();
                }
                if c.auto_increment {
                    col = col.auto_inc();
                }
                col
            })
            .collect()
    } else if let Some(ref sel) = stmt.as_select {
        // CTAS：从 SELECT 列表推断列名和类型
        infer_columns_from_select(sel, db)?
    } else {
        return Err(EngramDbError::Parse(
            "CREATE TABLE requires column definitions or AS SELECT".into(),
        ));
    };

    let mut table_def = TableDef::new(0, &stmt.table_name, columns);

    // v0.17.0 M0：ENGINE 子句 → TableDef.engine（校验引擎名）
    if let Some(engine_name) = &stmt.engine {
        table_def.engine = crate::common::types::EngineType::from_str(engine_name)
            .ok_or_else(|| {
                EngramDbError::Parse(format!(
                    "unsupported ENGINE '{}' (supported: columnar, memory, log)",
                    engine_name
                ))
            })?;
    }

    // v0.14.0：为列级 UNIQUE 列自动创建 UniqueIndex
    for (i, c) in stmt.columns.iter().enumerate() {
        if c.unique && !c.primary_key {
            let idx_name = format!("uniq_{}_{}", stmt.table_name, c.name);
            table_def.indexes.push(crate::common::types::IndexDef {
                name: idx_name,
                key_columns: vec![i],
                included_columns: vec![],
                unique: true,
                index_type: "skiplist".to_string(),
            });
        }
    }

    // CREATE TABLE AS SELECT：返回 CreateTableAs 节点
    if let Some(sel) = stmt.as_select {
        let source_plan = plan_select(*sel, db)?;
        return Ok(PhysicalPlan::CreateTableAs {
            table_def,
            source: Box::new(source_plan),
        });
    }

    Ok(PhysicalPlan::CreateTable { table_def })
}

/// 从 SELECT 语句推断 CREATE TABLE AS SELECT 的列定义
///
/// 简单实现：
/// - 如果列是 ColumnRef，从表元数据获取类型
/// - 如果列是字面量，根据值类型推断
/// - 如果列是函数调用（如 COUNT, SUM），默认 Int64
fn infer_columns_from_select(
    sel: &SelectStmt,
    _db: &Database,
) -> Result<Vec<crate::common::types::ColumnDef>> {
    use crate::common::types::{ColumnDef, DataType};

    // 简单推断：所有列默认为 Varchar, nullable
    // 实际生产应分析每个表达式并推断具体类型
    let mut cols = Vec::new();
    for item in &sel.select_list {
        match item {
            crate::sql::ast::SelectItem::Wildcard => {
                // SELECT *: 无法推断（需要表元数据），暂时跳过
                return Err(EngramDbError::Parse(
                    "CREATE TABLE AS SELECT * requires explicit column list".into(),
                ));
            }
            crate::sql::ast::SelectItem::Expression(expr, alias) => {
                // 列名优先使用 alias，其次从表达式推断
                let col_name = alias.clone().or_else(|| {
                    match expr {
                        crate::sql::ast::Expression::ColumnRef { column, .. } => Some(column.clone()),
                        crate::sql::ast::Expression::Function { name, args, .. } => {
                            // func(col) 格式
                            let arg = args.first().map(|a| match a {
                                crate::sql::ast::Expression::ColumnRef { column, .. } => column.clone(),
                                _ => "?".to_string(),
                            }).unwrap_or_default();
                            Some(format!("{}({})", name, arg))
                        }
                        _ => None,
                    }
                }).unwrap_or_else(|| format!("col_{}", cols.len()));
                let data_type = match expr {
                    crate::sql::ast::Expression::Literal(v) => match v {
                        crate::Value::Null => DataType::Varchar,
                        crate::Value::Boolean(_) => DataType::Boolean,
                        crate::Value::Int32(_) | crate::Value::Int64(_) => DataType::Int64,
                        crate::Value::Float32(_) | crate::Value::Float64(_) => DataType::Float64,
                        crate::Value::Varchar(_) | crate::Value::Json(_) => DataType::Varchar,
                        crate::Value::Vector(_) | crate::Value::VectorInt8(_) => DataType::Vector { dim: 0 },
                        crate::Value::Blob(_) => DataType::Blob,
                        crate::Value::Timestamp(_) => DataType::Timestamp,
                    },
                    crate::sql::ast::Expression::ColumnRef { .. } => DataType::Varchar, // 简化：默认 Varchar
                    crate::sql::ast::Expression::Function { name, .. } => {
                        // 聚合函数默认返回 Int64/Float64
                        match name.to_uppercase().as_str() {
                            "SUM" | "AVG" => DataType::Float64,
                            "COUNT" => DataType::Int64,
                            _ => DataType::Varchar,
                        }
                    }
                    _ => DataType::Varchar,
                };
                cols.push(ColumnDef::new(&col_name, data_type));
            }
        }
    }
    Ok(cols)
}

/// 规划 CREATE INDEX（v0.12.0 新增，覆盖索引）
fn plan_create_index(stmt: CreateIndexStmt, db: &Database) -> Result<PhysicalPlan> {
    // M5：非 Columnar 引擎不支持索引（提前清晰报错）
    ensure_engine_capability(db, &stmt.table_name, "索引", |c| c.supports_index)?;
    // 验证表存在
    let table = db.get_engine_table(&stmt.table_name)
        .ok_or_else(|| EngramDbError::TableNotFound(stmt.table_name.clone()))?;

    // 解析键列 → 列索引
    let mut key_cols = Vec::with_capacity(stmt.key_columns.len());
    for col_name in &stmt.key_columns {
        let idx = table.def().column_index(col_name)
            .ok_or_else(|| EngramDbError::ColumnNotFound(format!(
                "index key column '{}' not found in table '{}'", col_name, stmt.table_name
            )))?;
        key_cols.push(idx);
    }

    // 解析覆盖列 → 列索引（INCLUDE 子句）
    let mut included_cols = Vec::with_capacity(stmt.included_columns.len());
    for col_name in &stmt.included_columns {
        let idx = table.def().column_index(col_name)
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
        using: stmt.using,
        with_options: stmt.with_options,
    })
}

/// 规划 vector_search 表值函数（v0.15.0 V16 新增）
fn plan_vector_search(args: &[Expression], db: &Database) -> Result<PhysicalPlan> {
    if args.len() < 4 {
        return Err(EngramDbError::Parse(
            "vector_search requires 4 arguments: table_name, index_name, query_vector, k".into()
        ));
    }

    // 所有参数必须是字面量
    let table_name = match &args[0] {
        Expression::Literal(Value::Varchar(s)) => s.clone(),
        _ => return Err(EngramDbError::Parse("vector_search: table_name must be a string literal".into())),
    };
    let index_name = match &args[1] {
        Expression::Literal(Value::Varchar(s)) => s.clone(),
        _ => return Err(EngramDbError::Parse("vector_search: index_name must be a string literal".into())),
    };
    let query_vector = match &args[2] {
        Expression::Literal(Value::Varchar(s)) => {
            // Parse JSON array of floats
            let v: Vec<f64> = serde_json::from_str(s)
                .map_err(|e| EngramDbError::Parse(format!("vector_search: invalid query vector JSON: {}", e)))?;
            v.into_iter().map(|x| x as f32).collect()
        }
        Expression::Literal(Value::Vector(v)) => v.clone(),
        _ => return Err(EngramDbError::Parse("vector_search: query_vector must be a string or vector literal".into())),
    };
    let k = match &args[3] {
        Expression::Literal(Value::Int64(n)) => *n as usize,
        Expression::Literal(Value::Int32(n)) => *n as usize,
        _ => return Err(EngramDbError::Parse("vector_search: k must be an integer literal".into())),
    };

    // M5：非 Columnar 引擎不支持向量搜索（提前清晰报错）
    ensure_engine_capability(db, &table_name, "向量索引", |c| c.supports_vector_index)?;
    // 验证表存在
    db.get_engine_table(&table_name)
        .ok_or_else(|| EngramDbError::TableNotFound(table_name.clone()))?;

    Ok(PhysicalPlan::VectorSearch {
        table_name,
        index_name,
        query_vector,
        k,
    })
}

/// 规划 DELETE 语句（v0.12.0 新增）
fn plan_delete(stmt: DeleteStmt, db: &Database) -> Result<PhysicalPlan> {
    // M5：Log 引擎不支持 DELETE（planner 提前拦截）
    ensure_engine_capability(db, &stmt.table_name, "DELETE", |c| c.supports_delete)?;
    // 验证表存在
    let _table = db.get_engine_table(&stmt.table_name)
        .ok_or_else(|| EngramDbError::TableNotFound(stmt.table_name.clone()))?;

    Ok(PhysicalPlan::Delete {
        table_name: stmt.table_name,
        condition: stmt.where_clause,
    })
}

/// 规划 UPDATE 语句（v0.12.0 新增）
fn plan_update(stmt: UpdateStmt, db: &Database) -> Result<PhysicalPlan> {
    // M5：Log 引擎不支持 UPDATE（planner 提前拦截）
    ensure_engine_capability(db, &stmt.table_name, "UPDATE", |c| c.supports_update)?;
    // 验证表存在
    let table = db.get_engine_table(&stmt.table_name)
        .ok_or_else(|| EngramDbError::TableNotFound(stmt.table_name.clone()))?;

    // 解析 SET 子句中的列名 → 列索引
    let mut assignments = Vec::with_capacity(stmt.assignments.len());
    for (col_name, expr) in stmt.assignments {
        let col_idx = table.def().column_index(&col_name)
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
    // INSERT ... SELECT：先规划 SELECT 子查询，再包装为 InsertSelect 节点
    if let Some(select) = stmt.select {
        let source_plan = plan_select(*select, db)?;
        return Ok(PhysicalPlan::InsertSelect {
            table_name: stmt.table_name,
            columns: stmt.columns,
            source: Box::new(source_plan),
        });
    }

    // 验证表存在
    let table = db.get_engine_table(&stmt.table_name)
        .ok_or_else(|| EngramDbError::TableNotFound(stmt.table_name.clone()))?;

    let num_cols = table.def().columns.len();
    let num_rows = stmt.values.len();

    // ③：批量 VALUES → 列式 InsertColumns 快速路径
    // 条件：非事务模式（列式直写不经 MVCC/WAL）、无列名重排（全列按表顺序）、
    // 无 returning/on_conflict、每行值数量与表列数一致（列式要求每列等长）
    // （事务模式下列式写入会转置回行式走 WAL，无收益，保持 Insert 路径）
    if !db.config().enable_transaction
        && num_rows > 0
        && stmt.columns.is_none()
        && stmt.returning.is_none()
        && stmt.on_conflict.is_none()
        && stmt.values.iter().all(|r| r.len() == num_cols)
    {
        let mut columns: Vec<Vec<Value>> = vec![Vec::with_capacity(num_rows); num_cols];
        for value_row in &stmt.values {
            for (i, expr) in value_row.iter().enumerate() {
                columns[i].push(eval_constant_expr(expr, params)?);
            }
        }
        return Ok(PhysicalPlan::InsertColumns {
            table_name: stmt.table_name,
            columns,
        });
    }

    // 如果指定了列，预先计算列索引映射，避免逐行查找
    let col_map: Option<Vec<usize>> = stmt.columns.as_ref().map(|col_names| {
        col_names.iter()
            .filter_map(|name| table.def().column_index(name))
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
        returning: stmt.returning,
        on_conflict: stmt.on_conflict,
    })
}

/// 直通路径：免计划结构，直接求值裸 INSERT 的绑定行（v0.18 P0-1 直通）
///
/// 语义与 `plan_insert` 的 Insert 分支完全等价（含列名映射、Null 填充、
/// 参数越界/非常量表达式错误），仅省去 PhysicalPlan 结构构造与调度。
/// 调用方（execute_prepared / execute_prepared_batch）确认 stmt 为裸 INSERT
/// （无 returning / on_conflict / select）后才可进入此路径。
#[inline]
pub fn eval_insert_rows(stmt: &InsertStmt, db: &Database, params: &[Value]) -> Result<Vec<Vec<Value>>> {
    // 无列名快速路径：免表访问（表存在性由 insert 算子内部校验，与计划路径错误等价；
    // 列数语义与 plan_insert 的 None 分支一致——不校验行宽）
    if stmt.columns.is_none() {
        let mut rows = Vec::with_capacity(stmt.values.len());
        for value_row in &stmt.values {
            let mut row = Vec::with_capacity(value_row.len());
            for expr in value_row {
                row.push(eval_constant_expr(expr, params)?);
            }
            rows.push(row);
        }
        return Ok(rows);
    }
    let table = db.get_engine_table(&stmt.table_name)
        .ok_or_else(|| EngramDbError::TableNotFound(stmt.table_name.clone()))?;
    let num_cols = table.def().columns.len();
    let col_names = stmt.columns.as_ref().unwrap();
    let indices: Vec<usize> = col_names.iter()
        .filter_map(|name| table.def().column_index(name))
        .collect();
    let mut rows = Vec::with_capacity(stmt.values.len());
    for value_row in &stmt.values {
        let mut full_row = vec![Value::Null; num_cols];
        for (i, expr) in value_row.iter().enumerate() {
            if let Some(&idx) = indices.get(i) {
                full_row[idx] = eval_constant_expr(expr, params)?;
            }
        }
        rows.push(full_row);
    }
    Ok(rows)
}

fn plan_select(stmt: SelectStmt, db: &Database) -> Result<PhysicalPlan> {
    // CTE 内联：将 WITH 子句中的 CTE 定义内联到查询中
    let stmt = inline_ctes(stmt, db);

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
            if let Expression::Function { name, args, distinct: false, count_star, .. } = expr {
                let is_count_star = *count_star
                    || (name.eq_ignore_ascii_case("COUNT")
                        && (args.is_empty()
                            || matches!(args.as_slice(), [Expression::Literal(_)])));
                if name.eq_ignore_ascii_case("COUNT") && is_count_star {
                    // 仅单表时走元数据短路；JOIN/CROSS JOIN 跳过（走连接计划）
                    if let Some(table_name) = stmt.from.as_ref().and_then(|t| match t {
                        TableRef::Table { table_name, .. } => Some(table_name.clone()),
                        _ => None,
                    }) {
                        let table = db.get_engine_table(&table_name)
                            .ok_or_else(|| EngramDbError::TableNotFound(table_name.clone()))?;
                        let count = table.row_count() as i64;
                        let output_name = alias.clone().unwrap_or_else(|| "count(*)".to_string());
                        trace!("Perf01: COUNT(*) fast-path for '{}' => {}", table_name, count);
                        return Ok(PhysicalPlan::CountStar { output_name, count });
                    }
                }
            }
        }
    }

    // ===== V16：表值函数 vector_search(...) =====
    if let Some(TableRef::TableFunction { name, args, .. }) = &stmt.from {
        if name.eq_ignore_ascii_case("vector_search") {
            return plan_vector_search(args, db);
        }
    }

    // ===== Q23：FROM 子查询（Derived Table）=====
    // SELECT * FROM (SELECT ...) AS sub:
    // 规划内层查询，通过 SubqueryScan 节点获取结果行。
    // ===== JOIN / CROSS JOIN（②）=====
    // 连接查询走完整流水线（WHERE/聚合/投影/排序/LIMIT）：
    // plan_join_tree 构建连接树（HashJoin/CrossJoin），随后与单表
    // 相同的通用阶段叠加。
    if matches!(&stmt.from, Some(TableRef::Join { .. }) | Some(TableRef::CrossJoin { .. })) {
        return plan_select_join(&stmt, db);
    }

    // ===== Q23：FROM 子查询（Derived Table）=====
    // 内层计划作为 SubqueryScan 起点，外层 WHERE/聚合/投影/排序/LIMIT
    // 等通用阶段继续叠加（此前直接返回导致外层条件被静默丢弃）。
    let mut derived_plan: Option<(PhysicalPlan, Vec<String>)> = None;
    if let Some(TableRef::Derived { query, .. }) = &stmt.from {
        let inner_plan = plan_select(*query.clone(), db)?;
        let inner_names = extract_column_names(&inner_plan);
        derived_plan = Some((
            PhysicalPlan::SubqueryScan { plan: Box::new(inner_plan) },
            inner_names,
        ));
    }

    // 单表名（Derived 时为 None，走 SubqueryScan 起点）
    let table_name: Option<String> = stmt.from.as_ref().and_then(|t| match t {
        TableRef::Table { table_name, .. } => Some(table_name.clone()),
        _ => None,
    });

    // 表 schema 列（Derived 时为空，窗口函数/投影仅回退使用）
    let window_table_cols: &[crate::common::types::ColumnDef] = table_name.as_ref()
        .and_then(|n| db.get_engine_table(n))
        .map(|t| t.def().columns.as_slice())
        .unwrap_or(&[]);

    let (mut plan, scan_column_names, had_pk) = if let Some((p, n)) = derived_plan {
        (p, n, false)
    } else {
        let table_name = table_name
            .clone()
            .ok_or_else(|| EngramDbError::Parse("SELECT without FROM not supported".into()))?;
        let table = db.get_engine_table(&table_name)
            .ok_or_else(|| EngramDbError::TableNotFound(table_name.clone()))?;

        // ===== Perf03：主键点查短路（WHERE pk = Literal）=====
        // 条件：
        // 1. 表有 PRIMARY KEY 索引
        // 2. WHERE 唯一条件为 `pk_col = Literal`（BinaryEq）
        // 3. 无 GROUP BY / HAVING / ORDER BY / LIMIT（简化，后续可扩展）
        let mut pk_short_circuit: Option<crate::Value> = None;
        if table.def().primary_key_index().is_some()
            && stmt.group_by.is_empty()
            && stmt.having.is_none()
            && stmt.order_by.is_empty()
            // P3.3：主键点查最多返回 1 行，LIMIT 天然满足，放开该限制
            //（LIMIT 0 由 Limit 算子的 take(0) 处理）
        {
            if let Some(ref where_expr) = stmt.where_clause {
                if let Expression::BinaryOp { left, op, right } = where_expr {
                    if *op == BinaryOperator::Eq {
                        let pk_idx = table.def().primary_key_index().unwrap();
                        let pk_name = &table.def().columns[pk_idx].name;
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
        // 注意：collect_referenced_columns 现在返回列名（Vec 而非 HashSet），保持 SELECT/WHERE 中出现的顺序
        let all_referenced_cols = collect_referenced_columns(&stmt, &table.def().columns);
        let mut scan_column_indices: Vec<usize> = all_referenced_cols.iter()
            .filter_map(|name| table.def().column_index(name))
            .collect();

        // 纯聚合（如 COUNT(*)）不引用任何列时，至少扫描第一列用于计数
        let has_agg_in_select = select_list_has_aggregates(&stmt.select_list);
        if scan_column_indices.is_empty() && has_agg_in_select && !table.def().columns.is_empty() {
            scan_column_indices.push(0);
        }

        // 扫描阶段的列名映射（扫描输出的列名）
        let scan_column_names: Vec<String> = scan_column_indices.iter()
            .map(|&i| table.def().columns[i].name.clone())
            .collect();

        // ===== 覆盖索引优化（v0.12.0 新增）=====
        // 检测条件：
        // 1. WHERE 条件为单列等值比较（col = literal）
        // 2. 该列是某个索引的首键列
        // 3. 所有扫描列都在该索引的 key_columns + included_columns 中
        // 4. 无 GROUP BY / HAVING / 聚合
        //    （ORDER BY 不阻断：排序可由 can_skip_sort_by_index 利用索引有序性跳过）
        let has_group_by = !stmt.group_by.is_empty();
        let has_having = stmt.having.is_some();
        let has_agg = select_list_has_aggregates(&stmt.select_list);

        let can_use_index_only = !has_group_by && !has_having && !has_agg;

        let had_pk = pk_short_circuit.is_some();
        let plan = if let Some(pk_val) = pk_short_circuit.take() {
            trace!("Perf03: PrimaryKeyLookup fast-path for '{}' pk={:?}", table_name, pk_val);
            PhysicalPlan::PrimaryKeyLookup {
                table_name: table_name.clone(),
                pk_value: pk_val,
                output_column_indices: scan_column_indices.clone(),
            }
        } else if can_use_index_only {
            try_index_only_scan(&stmt, db, &table_name, &scan_column_indices)
                .or_else(|| try_index_scan(&stmt, db, &table_name, &scan_column_indices))
                .or_else(|| try_index_range_scan(&stmt, db, &table_name, &scan_column_indices))
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
        (plan, scan_column_names, had_pk)
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
        // （HAVING 中的聚合也递归纳入：否则 HAVING 单独引用聚合时重写失败）
        let (mut agg_exprs, _non_agg_exprs) = extract_aggregates_from_select(&stmt.select_list);
        if let Some(having) = &stmt.having {
            collect_agg_exprs(having, &mut agg_exprs);
        }

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

    // HAVING：在聚合之上添加 Filter 节点（v0.15.0 新增）
    if let Some(having_expr) = &stmt.having {
        if needs_aggregate {
            // 将 HAVING 中的聚合函数调用替换为 ColumnRef（引用 Aggregate 输出列）
            let rewritten = rewrite_having_aggregates(
                having_expr,
                &group_by_indices,
                &aggregates,
                &scan_column_names,
            );
            plan = PhysicalPlan::Filter {
                input: Box::new(plan),
                condition: rewritten,
            };
        }
    }

    // 检测窗口函数
    let has_window = stmt.select_list.iter().any(|item| {
        if let SelectItem::Expression(expr, _) = item {
            expr_has_window(expr)
        } else {
            false
        }
    });

    // 提取窗口函数信息
    let window_funcs = if has_window {
        let mut funcs = Vec::new();
        for item in &stmt.select_list {
            if let SelectItem::Expression(expr, alias) = item {
                extract_window_functions(expr, alias, &scan_column_names, window_table_cols, &mut funcs);
            }
        }
        funcs
    } else {
        Vec::new()
    };

    // 如果有窗口函数，插入 Window 节点
    if !window_funcs.is_empty() {
        plan = PhysicalPlan::Window {
            input: Box::new(plan),
            window_functions: window_funcs.clone(),
            column_names: scan_column_names.clone(),
        };
    }

    // Projection（计算 SELECT 列表中的表达式）
    let (proj_expressions, proj_names) = plan_projection(
        &stmt.select_list,
        &scan_column_names,
        needs_aggregate,
        window_table_cols,
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
            table_name.as_deref().unwrap_or(""),
            db,
        );

        if !sort_keys.is_empty() && !can_skip_sort {
            plan = PhysicalPlan::Sort {
                input: Box::new(plan),
                sort_keys,
                limit: stmt.limit,
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

    // DISTINCT
    if stmt.distinct {
        plan = PhysicalPlan::Distinct {
            input: Box::new(plan),
        };
    }

    // 集合操作 UNION / UNION ALL / INTERSECT / EXCEPT（v0.15.0 新增）
    if let Some((op, right_stmt)) = stmt.set_op {
        let right_plan = plan_select(*right_stmt, db)?;
        let set_union_op = match op {
            SetOpType::Union => SetUnionOp::Union,
            SetOpType::UnionAll => SetUnionOp::UnionAll,
            SetOpType::Intersect => SetUnionOp::Intersect,
            SetOpType::Except => SetUnionOp::Except,
        };
        plan = PhysicalPlan::SetUnion {
            op: set_union_op,
            left: Box::new(plan),
            right: Box::new(right_plan),
        };
    }

    Ok(plan)
}

/// 规划连接查询（②：INNER / LEFT / RIGHT / FULL JOIN、CROSS JOIN）
///
/// 与单表路径的差异仅在于扫描层：`plan_join_tree` 构建连接树，
/// 随后叠加与单表相同的通用阶段（Filter / Aggregate / HAVING /
/// Projection / Sort / Limit）。
fn plan_select_join(stmt: &SelectStmt, db: &Database) -> Result<PhysicalPlan> {
    // 构建连接树计划 + 组合输出列名（左表列 ++ 右表列）
    let from_ref = stmt.from.as_ref()
        .ok_or_else(|| EngramDbError::Parse("JOIN without FROM".into()))?;
    let (mut plan, scan_column_names) = plan_join_tree(from_ref, db, stmt)?;

    // Filter（WHERE）
    // 列引用带表前缀时重写为前缀列名（匹配 join 输出的 "users.name" 列名）
    if let Some(where_expr) = &stmt.where_clause {
        plan = PhysicalPlan::Filter {
            input: Box::new(plan),
            condition: qualify_column_refs(where_expr),
        };
    }

    // 聚合（GROUP BY / SELECT 聚合函数 / HAVING）
    let has_group_by = !stmt.group_by.is_empty();
    let has_agg_in_select = select_list_has_aggregates(&stmt.select_list);
    let has_having = stmt.having.is_some();
    let needs_aggregate = has_group_by || has_agg_in_select || has_having;

    // SELECT 列表列引用重写为前缀列名（供聚合提取 / 投影共用）
    let qualified_select: Vec<SelectItem> = stmt.select_list.iter()
        .map(|item| match item {
            SelectItem::Wildcard => SelectItem::Wildcard,
            SelectItem::Expression(expr, alias) => {
                SelectItem::Expression(qualify_column_refs(expr), alias.clone())
            }
        })
        .collect();

    let mut group_by_indices: Vec<usize> = Vec::new();
    let mut aggregates: Vec<AggregateExpr> = Vec::new();

    if needs_aggregate {
        group_by_indices = stmt.group_by.iter()
            .filter_map(|expr| {
                if let Expression::ColumnRef { table, column } = expr {
                    find_join_column(&scan_column_names, table.as_deref(), column)
                } else {
                    None
                }
            })
            .collect();

        let (mut agg_exprs, _non_agg_exprs) = extract_aggregates_from_select(&qualified_select);
        // HAVING 中的聚合也递归纳入（HAVING 单独引用聚合时可被重写为输出列）
        if let Some(having_expr) = &stmt.having {
            let qualified_having = qualify_column_refs(having_expr);
            collect_agg_exprs(&qualified_having, &mut agg_exprs);
        }
        aggregates = agg_exprs.iter()
            .filter_map(|(func_name, arg_expr, distinct)| {
                let input_col = match arg_expr {
                    Expression::ColumnRef { column, .. } => {
                        scan_column_names.iter().position(|c| c == column)?
                    }
                    _ => 0,
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

    // HAVING（聚合之上 Filter）
    if let Some(having_expr) = &stmt.having {
        if needs_aggregate {
            let qualified = qualify_column_refs(having_expr);
            let rewritten = rewrite_having_aggregates(
                &qualified,
                &group_by_indices,
                &aggregates,
                &scan_column_names,
            );
            plan = PhysicalPlan::Filter {
                input: Box::new(plan),
                condition: rewritten,
            };
        }
    }

    // 窗口函数：JOIN 查询暂不支持
    let has_window = stmt.select_list.iter().any(|item| {
        if let SelectItem::Expression(expr, _) = item {
            expr_has_window(expr)
        } else {
            false
        }
    });
    if has_window {
        return Err(EngramDbError::Parse(
            "Window functions are not supported in JOIN queries".into()
        ));
    }

    // Projection（SELECT 列表已在聚合提取前重写为前缀列名）
    let (proj_expressions, proj_names) = plan_projection(
        &qualified_select,
        &scan_column_names,
        needs_aggregate,
        &[],
    )?;
    // 投影输出列名还原为裸名（用户可见 API；求值用表达式中的前缀名匹配输入列）
    let proj_names: Vec<String> = proj_names.iter()
        .map(|n| n.rsplit('.').next().unwrap_or(n).to_string())
        .collect();

    if needs_aggregate {
        // 聚合结果直接输出（列顺序由聚合执行器保证）
    } else if !proj_expressions.is_empty() {
        plan = PhysicalPlan::Projection {
            input: Box::new(plan),
            expressions: proj_expressions.clone(),
            column_names: proj_names.clone(),
        };
    }

    // ORDER BY
    if !stmt.order_by.is_empty() {
        let output_columns = if needs_aggregate {
            // group_by 列名（裸名）+ 聚合列输出名（SELECT 别名优先，与
            // Aggregate 输出列顺序一致：group_by 列在前，聚合列在后）
            let mut cols: Vec<String> = group_by_indices.iter()
                .map(|&i| scan_column_names.get(i)
                    .map(|c| c.rsplit('.').next().unwrap_or(c).to_string())
                    .unwrap_or_else(|| format!("group_{}", i)))
                .collect();
            let agg_out_names: Vec<String> = qualified_select.iter().filter_map(|item| {
                if let SelectItem::Expression(expr, alias) = item {
                    if let Expression::Function { name, .. } = expr {
                        if matches!(name.to_uppercase().as_str(),
                            "COUNT" | "SUM" | "AVG" | "MIN" | "MAX")
                        {
                            return Some(alias.clone().unwrap_or_else(|| name.to_lowercase()));
                        }
                    }
                }
                None
            }).collect();
            cols.extend(agg_out_names);
            cols
        } else if !proj_expressions.is_empty() {
            proj_names.clone()
        } else {
            scan_column_names.clone()
        };

        let mut sort_keys = Vec::new();
        for item in &stmt.order_by {
            if let Expression::ColumnRef { table, column } = &item.expr {
                // 前缀列名（"users.name"）或裸列名匹配输出列
                let idx = output_columns.iter().position(|c| {
                    let prefixed = table.as_ref()
                        .map(|t| c == &format!("{}.{}", t, column))
                        .unwrap_or(false);
                    prefixed || c == column
                });
                if let Some(idx) = idx {
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

        if !sort_keys.is_empty() {
            plan = PhysicalPlan::Sort {
                input: Box::new(plan),
                sort_keys,
                limit: stmt.limit,
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

    // DISTINCT
    if stmt.distinct {
        plan = PhysicalPlan::Distinct {
            input: Box::new(plan),
        };
    }

    Ok(plan)
}

/// 构建连接树物理计划（②）
///
/// 返回 (计划, 输出列名)。输出列名 = 左子树列名 ++ 右子树列名。
///
/// - `Table`：TableScan，扫描列 = stmt 引用的列 ∪ 整棵树 ON 条件引用的列
/// - `Join`：递归构建两侧 → 解析 ON 等值键 → HashJoin（无等值键时
///   CrossJoin + 残留 Filter；非等值 ON 且 LEFT/RIGHT/FULL 时报错）
/// - `CrossJoin`：递归构建两侧 → CrossJoin
fn plan_join_tree(
    table_ref: &TableRef,
    db: &Database,
    stmt: &SelectStmt,
) -> Result<(PhysicalPlan, Vec<String>)> {
    let all_on_columns = collect_join_on_columns(table_ref);
    plan_join_tree_inner(table_ref, db, stmt, &all_on_columns)
}

/// 连接树构建（内部递归）
fn plan_join_tree_inner(
    table_ref: &TableRef,
    db: &Database,
    stmt: &SelectStmt,
    all_on_columns: &[String],
) -> Result<(PhysicalPlan, Vec<String>)> {
    match table_ref {
        TableRef::Table { table_name, .. } => {
            let table = db.get_engine_table(table_name)
                .ok_or_else(|| EngramDbError::TableNotFound(table_name.clone()))?;

            // 扫描列 = stmt 引用列（仅本表）∪ 所有 ON 引用列（仅本表）
            // 注意：collect_referenced_columns 收集的是裸列名，JOIN 场景下
            // 包含属于其他表的列，必须按本表 schema 过滤后再用。
            // 输出列名带表前缀（"users.name"），消除跨表重名列歧义。
            let mut names: Vec<String> = Vec::new();
            let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
            for name in collect_referenced_columns(stmt, &table.def().columns) {
                if table.def().column_index(&name).is_some() && !seen.contains(&name) {
                    seen.insert(name.clone());
                    names.push(format!("{}.{}", table_name, name));
                }
            }
            for col in all_on_columns {
                if table.def().column_index(col).is_some() && !seen.contains(col) {
                    seen.insert(col.clone());
                    names.push(format!("{}.{}", table_name, col));
                }
            }

            let indices: Vec<usize> = names.iter()
                .filter_map(|n| table.def().column_index(n.rsplit('.').next().unwrap_or(n)))
                .collect();
            let indices = if indices.is_empty() && !table.def().columns.is_empty() {
                // 无引用列时扫描第一列（与单表路径一致）
                if !names.contains(&format!("{}.{}", table_name, table.def().columns[0].name)) {
                    names.push(format!("{}.{}", table_name, table.def().columns[0].name));
                }
                vec![0]
            } else {
                indices
            };

            // 列名规范化投影：TableScan 执行器输出裸名列名，JOIN 上下文
            // 需要前缀列名（"users.name"）供 ON/WHERE/SELECT 消歧解析。
            // 表达式用裸名列引用（匹配 TableScan 输出），输出列名用前缀名。
            // 注意：恒等投影消除要求 expressions[i].column == column_names[i]，
            // 此处二者不同（裸名 vs 前缀名），不会被消除。
            let expressions: Vec<Expression> = indices.iter()
                .map(|&i| Expression::ColumnRef {
                    table: None,
                    column: table.def().columns[i].name.clone(),
                })
                .collect();
            let plan = PhysicalPlan::Projection {
                input: Box::new(PhysicalPlan::TableScan {
                    table_name: table_name.clone(),
                    column_indices: indices,
                }),
                expressions,
                column_names: names.clone(),
            };

            Ok((plan, names))
        }
        TableRef::Join { left, right, join_type, on } => {
            let left_side = plan_join_tree_inner(left, db, stmt, all_on_columns)?;
            let right_side = plan_join_tree_inner(right, db, stmt, all_on_columns)?;

            let (mut plan, left_names) = if let Some(on_expr) = on {
                let (left_keys, right_keys, residual) =
                    resolve_join_on(on_expr, &left_side.1, &right_side.1)?;

                let mut p = if left_keys.is_empty() {
                    // 无等值键：INNER 用 CrossJoin + 残留 Filter
                    if *join_type != crate::executor::physical_plan::JoinType::Inner {
                        return Err(EngramDbError::Parse(format!(
                            "Non-equi {:?} JOIN is not supported (needs an equality condition in ON)",
                            join_type
                        )));
                    }
                    PhysicalPlan::CrossJoin {
                        left: Box::new(left_side.0),
                        right: Box::new(right_side.0),
                    }
                } else {
                    PhysicalPlan::HashJoin {
                        left: Box::new(left_side.0),
                        right: Box::new(right_side.0),
                        join_type: *join_type,
                        left_keys,
                        right_keys,
                    }
                };

                if let Some(residual_cond) = residual {
                    p = PhysicalPlan::Filter {
                        input: Box::new(p),
                        condition: qualify_column_refs(&residual_cond),
                    };
                }
                (p, left_side.1)
            } else {
                // 无 ON：CROSS JOIN
                (
                    PhysicalPlan::CrossJoin {
                        left: Box::new(left_side.0),
                        right: Box::new(right_side.0),
                    },
                    left_side.1,
                )
            };

            let mut columns = left_names;
            columns.extend(right_side.1);
            Ok((plan, columns))
        }
        TableRef::CrossJoin { left, right } => {
            let left_side = plan_join_tree_inner(left, db, stmt, all_on_columns)?;
            let right_side = plan_join_tree_inner(right, db, stmt, all_on_columns)?;

            let mut columns = left_side.1;
            columns.extend(right_side.1);
            Ok((
                PhysicalPlan::CrossJoin {
                    left: Box::new(left_side.0),
                    right: Box::new(right_side.0),
                },
                columns,
            ))
        }
        TableRef::Derived { .. } => Err(EngramDbError::Parse(
            "Derived tables are not supported in JOIN queries".into()
        )),
        TableRef::TableFunction { .. } => Err(EngramDbError::Parse(
            "Table functions are not supported in JOIN queries".into()
        )),
    }
}

/// 解析 ON 条件为等值连接键 + 残留条件（②）
///
/// 将 ON 拆分为顶层 AND 子句；形如 `col = col`（分属左右两侧）的子句
/// 提取为连接键，其余子句合并为残留 Filter 条件。
///
/// 返回 (left_keys, right_keys, residual)：
/// - left_keys[i] / right_keys[i]：键在左/右输出列名列表中的位置
/// - residual：未消费子句的 AND 合并（None 表示无残留）
fn resolve_join_on(
    on: &Expression,
    left_names: &[String],
    right_names: &[String],
) -> Result<(Vec<usize>, Vec<usize>, Option<Expression>)> {
    let conjuncts = split_and_conjuncts(on);
    let mut left_keys: Vec<usize> = Vec::new();
    let mut right_keys: Vec<usize> = Vec::new();
    let mut residual: Vec<Expression> = Vec::new();

    for conj in conjuncts {
        if let Expression::BinaryOp { left, op: BinaryOperator::Eq, right } = conj {
            let lref = column_ref_name(left.as_ref());
            let rref = column_ref_name(right.as_ref());
            if let (Some((lt, lc)), Some((rt, rc))) = (lref, rref) {
                let lpos = find_join_column(&left_names, lt, lc);
                let rpos = find_join_column(&right_names, rt, rc);
                let lpos2 = find_join_column(&right_names, lt, lc);
                let rpos2 = find_join_column(&left_names, rt, rc);
                if let (Some(l), Some(r)) = (lpos, rpos) {
                    left_keys.push(l);
                    right_keys.push(r);
                    continue;
                }
                if let (Some(l), Some(r)) = (lpos2, rpos2) {
                    left_keys.push(l);
                    right_keys.push(r);
                    continue;
                }
            }
        }
        residual.push(conj.clone());
    }

    let residual_expr = match residual.len() {
        0 => None,
        1 => Some(residual.pop().unwrap()),
        _ => Some(residual.into_iter().reduce(|acc, e| Expression::BinaryOp {
            left: Box::new(acc),
            op: BinaryOperator::And,
            right: Box::new(e),
        }).unwrap()),
    };

    Ok((left_keys, right_keys, residual_expr))
}

/// 若表达式是 ColumnRef，返回 (表前缀, 列名)（②）
fn column_ref_name(expr: &Expression) -> Option<(Option<&str>, &str)> {
    if let Expression::ColumnRef { table, column } = expr {
        Some((table.as_deref(), column.as_str()))
    } else {
        None
    }
}

/// 在 JOIN 侧输出列名列表中查找列（②）
///
/// 优先按表前缀精确匹配（"users.id"），再回退裸列名匹配
/// （无重名歧义时 WHERE/ON 中的裸列引用仍可用）。
fn find_join_column(names: &[String], table: Option<&str>, column: &str) -> Option<usize> {
    if let Some(t) = table {
        let prefixed = format!("{}.{}", t, column);
        if let Some(i) = names.iter().position(|c| c == &prefixed) {
            return Some(i);
        }
    }
    names.iter().position(|c| c == column)
}

/// 将表达式拆分为顶层 AND 子句
fn split_and_conjuncts(expr: &Expression) -> Vec<&Expression> {
    fn rec<'a>(expr: &'a Expression, out: &mut Vec<&'a Expression>) {
        if let Expression::BinaryOp { left, op: BinaryOperator::And, right } = expr {
            rec(left, out);
            rec(right, out);
        } else {
            out.push(expr);
        }
    }
    let mut out = Vec::new();
    rec(expr, &mut out);
    out
}

/// 收集整棵 FROM 树中所有 ON 条件引用的列名（②）
fn collect_join_on_columns(table_ref: &TableRef) -> Vec<String> {
    let mut cols: Vec<String> = Vec::new();
    fn rec(table_ref: &TableRef, out: &mut Vec<String>) {
        match table_ref {
            TableRef::Join { left, right, on, .. } => {
                if let Some(on) = on {
                    collect_expr_columns_ordered(
                        on, out,
                        &mut std::collections::HashSet::new(),
                    );
                }
                rec(left, out);
                rec(right, out);
            }
            TableRef::CrossJoin { left, right } => {
                rec(left, out);
                rec(right, out);
            }
            _ => {}
        }
    }
    rec(table_ref, &mut cols);
    cols
}

/// 将表达式中的表前缀列引用（ColumnRef{table: Some("t"), column: "c"}）
/// 重写为前缀列名（ColumnRef{table: None, column: "t.c"}），与 JOIN 输出
/// 列名（"t.c"）匹配，并消除跨表重名列歧义（②）。
fn qualify_column_refs(expr: &Expression) -> Expression {
    match expr {
        Expression::ColumnRef { table: Some(t), column } => Expression::ColumnRef {
            table: None,
            column: format!("{}.{}", t, column),
        },
        Expression::ColumnRef { table: None, column } => {
            Expression::ColumnRef { table: None, column: column.clone() }
        }
        Expression::Literal(_) | Expression::Placeholder(_) => expr.clone(),
        Expression::BinaryOp { left, op, right } => Expression::BinaryOp {
            left: Box::new(qualify_column_refs(left)),
            op: *op,
            right: Box::new(qualify_column_refs(right)),
        },
        Expression::UnaryOp { op, expr: inner } => Expression::UnaryOp {
            op: *op,
            expr: Box::new(qualify_column_refs(inner)),
        },
        Expression::Function { name, args, distinct, count_star, over } => Expression::Function {
            name: name.clone(),
            args: args.iter().map(qualify_column_refs).collect(),
            distinct: *distinct,
            count_star: *count_star,
            over: over.clone(),
        },
        Expression::Cast { expr: inner, data_type } => Expression::Cast {
            expr: Box::new(qualify_column_refs(inner)),
            data_type: data_type.clone(),
        },
        Expression::IsNull(inner) => Expression::IsNull(Box::new(qualify_column_refs(inner))),
        Expression::IsNotNull(inner) => Expression::IsNotNull(Box::new(qualify_column_refs(inner))),
        Expression::InList { expr: inner, list } => Expression::InList {
            expr: Box::new(qualify_column_refs(inner)),
            list: list.iter().map(qualify_column_refs).collect(),
        },
        Expression::Like { expr: inner, pattern } => Expression::Like {
            expr: Box::new(qualify_column_refs(inner)),
            pattern: Box::new(qualify_column_refs(pattern)),
        },
        Expression::Case { when_then, else_expr } => Expression::Case {
            when_then: when_then.iter()
                .map(|(w, t)| (qualify_column_refs(w), qualify_column_refs(t)))
                .collect(),
            else_expr: else_expr.as_ref().map(|e| Box::new(qualify_column_refs(e))),
        },
        Expression::Subquery(_) | Expression::Exists { .. } | Expression::InSubquery { .. } => {
            expr.clone()
        }
    }
}

/// 规划单个表引用（用于 CROSS JOIN 等场景）
fn plan_table_ref(table_ref: &TableRef, db: &Database, stmt: &SelectStmt) -> Result<PhysicalPlan> {
    match table_ref {
        TableRef::Join { left, right, on, .. } => {
            // ②：连接树（此函数不再被顶层 JOIN 路径使用，保留以支持嵌套引用）
            let (plan, _) = plan_join_tree(table_ref, db, stmt)?;
            let _ = (left, right, on);
            Ok(plan)
        }
        TableRef::Table { table_name, .. } => {
            let table = db.get_engine_table(table_name)
                .ok_or_else(|| EngramDbError::TableNotFound(table_name.clone()))?;
            let scan_columns = collect_referenced_columns(stmt, &table.def().columns);
            let scan_column_indices: Vec<usize> = scan_columns.iter()
                .filter_map(|name| table.def().column_index(name))
                .collect();
            let indices = if scan_column_indices.is_empty() && !table.def().columns.is_empty() {
                vec![0]
            } else {
                scan_column_indices
            };
            Ok(PhysicalPlan::TableScan {
                table_name: table_name.clone(),
                column_indices: indices,
            })
        }
        TableRef::Derived { query, .. } => {
            let inner = plan_select(*query.clone(), db)?;
            Ok(PhysicalPlan::SubqueryScan { plan: Box::new(inner) })
        }
        TableRef::CrossJoin { left, right } => {
            let left_plan = plan_table_ref(left, db, stmt)?;
            let right_plan = plan_table_ref(right, db, stmt)?;
            Ok(PhysicalPlan::CrossJoin {
                left: Box::new(left_plan),
                right: Box::new(right_plan),
            })
        }
        TableRef::TableFunction { name, args, .. } => {
            if name.eq_ignore_ascii_case("vector_search") {
                return plan_vector_search(args, db);
            }
            Err(EngramDbError::Parse(format!("Unsupported table function: {}", name)))
        }
    }
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
    let table = db.get_engine_table(table_name)?;
    let col_idx = table.def().column_index(&col_name)?;

    for idx_def in &table.def().indexes {
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

/// 尝试生成非覆盖索引点查计划（P2：IndexScan）
///
/// 与 `try_index_only_scan` 的区别：不要求输出列全部在索引覆盖范围内。
/// 条件：
/// 1. WHERE 为单列等值比较（col = literal）
/// 2. 该列是某个普通索引（SkipList）的首键列
/// 3. 无 GROUP BY / HAVING / ORDER BY / 聚合（由调用方保证）
///
/// 执行方式：索引 O(log n) 定位 row_id → 回表读取所需列。
fn try_index_scan(
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
    let table = db.get_engine_table(table_name)?;
    let col_idx = table.def().column_index(&col_name)?;

    for idx_def in &table.def().indexes {
        // 只考虑普通跳表索引（位图/布隆/向量不走回表路径）
        let is_skiplist = idx_def.index_type.is_empty()
            || idx_def.index_type.eq_ignore_ascii_case("skiplist")
            || idx_def.index_type.eq_ignore_ascii_case("btree")
            || idx_def.index_type.eq_ignore_ascii_case("default");
        if !is_skiplist {
            continue;
        }
        if idx_def.key_columns.first() == Some(&col_idx) {
            return Some(PhysicalPlan::IndexScan {
                table_name: table_name.to_string(),
                index_name: idx_def.name.clone(),
                key_value,
                output_column_indices: scan_column_indices.to_vec(),
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

/// 范围谓词（①：索引范围扫描）
///
/// 由 WHERE 中的 `col OP literal` 比较条件提取，可合并为单边/双边区间。
#[derive(Debug, Clone)]
struct RangePredicate {
    /// 索引键列名
    col_name: String,
    /// 下界：(值, 是否包含)，None 表示无下界
    low: Option<(Value, bool)>,
    /// 上界：(值, 是否包含)，None 表示无上界
    high: Option<(Value, bool)>,
}

/// 提取单边范围条件：`col OP literal`（OP ∈ Gt/GtEq/Lt/LtEq）
fn extract_single_bound(expr: &Expression) -> Option<RangePredicate> {
    let (col_name, op, lit) = match expr {
        Expression::BinaryOp { left, op, right } => {
            match (left.as_ref(), right.as_ref()) {
                (Expression::ColumnRef { column, .. }, Expression::Literal(v)) => {
                    (column.clone(), *op, v.clone())
                }
                (Expression::Literal(v), Expression::ColumnRef { column, .. }) => {
                    (column.clone(), *op, v.clone())
                }
                _ => return None,
            }
        }
        _ => return None,
    };

    match op {
        BinaryOperator::Gt => Some(RangePredicate {
            col_name,
            low: Some((lit, false)),
            high: None,
        }),
        BinaryOperator::GtEq => Some(RangePredicate {
            col_name,
            low: Some((lit, true)),
            high: None,
        }),
        BinaryOperator::Lt => Some(RangePredicate {
            col_name,
            high: Some((lit, false)),
            low: None,
        }),
        BinaryOperator::LtEq => Some(RangePredicate {
            col_name,
            high: Some((lit, true)),
            low: None,
        }),
        _ => None,
    }
}

/// 提取完整范围条件（①：索引范围扫描）
///
/// 支持：
/// - 单边：`col > x` / `col >= x` / `col < y` / `col <= y`
/// - 双边：`col > x AND col < y`（同一列的比较条件用 AND 合并）
///
/// 只有 WHERE 整体可被同一列的边界条件完全表示时才返回 Some
/// （否则范围扫描不能覆盖全部谓词，退回全表扫描 + Filter）。
fn extract_range_condition(expr: &Expression) -> Option<RangePredicate> {
    // 双边合并：AND 左右两侧各提取边界
    if let Expression::BinaryOp { left, op: BinaryOperator::And, right } = expr {
        let left_pred = extract_single_bound(left)?;
        let right_pred = extract_single_bound(right)?;
        if left_pred.col_name != right_pred.col_name {
            return None;
        }
        return merge_range_predicates(left_pred, right_pred);
    }

    extract_single_bound(expr)
}

/// 合并两个同列范围谓词（取更严格的边界）
fn merge_range_predicates(a: RangePredicate, b: RangePredicate) -> Option<RangePredicate> {
    // 下界：取更大的值；值相等时开区间（>）更严格
    let low = match (a.low, b.low) {
        (Some((v1, i1)), Some((v2, i2))) => {
            match value_cmp_planner(&v1, &v2) {
                std::cmp::Ordering::Greater => Some((v1, i1)),
                std::cmp::Ordering::Less => Some((v2, i2)),
                // 值相等：开区间更严格
                std::cmp::Ordering::Equal => Some((v1, i1 && i2)),
            }
        }
        (Some(x), None) | (None, Some(x)) => Some(x),
        (None, None) => None,
    };

    // 上界：取更小的值；值相等时开区间（<）更严格
    let high = match (a.high, b.high) {
        (Some((v1, i1)), Some((v2, i2))) => {
            match value_cmp_planner(&v1, &v2) {
                std::cmp::Ordering::Less => Some((v1, i1)),
                std::cmp::Ordering::Greater => Some((v2, i2)),
                std::cmp::Ordering::Equal => Some((v1, i1 && i2)),
            }
        }
        (Some(x), None) | (None, Some(x)) => Some(x),
        (None, None) => None,
    };

    // 边界冲突（下界 > 上界）：空结果集，无法用范围扫描表示，退回全表扫描
    if let (Some((lv, _)), Some((hv, _))) = (&low, &high) {
        if value_cmp_planner(lv, hv) == std::cmp::Ordering::Greater {
            return None;
        }
    }

    Some(RangePredicate {
        col_name: a.col_name,
        low,
        high,
    })
}

/// 规划器内的 Value 比较（用于边界合并，与跳表 key_less 语义一致）
fn value_cmp_planner(a: &Value, b: &Value) -> std::cmp::Ordering {
    match (a, b) {
        (Value::Null, Value::Null) => std::cmp::Ordering::Equal,
        (Value::Null, _) => std::cmp::Ordering::Less,
        (_, Value::Null) => std::cmp::Ordering::Greater,
        (Value::Int64(x), Value::Int64(y)) => x.cmp(y),
        (Value::Int32(x), Value::Int32(y)) => x.cmp(y),
        (Value::Float64(x), Value::Float64(y)) => x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal),
        (Value::Varchar(x), Value::Varchar(y)) => x.cmp(y),
        (Value::Boolean(x), Value::Boolean(y)) => (!x).cmp(&!y),
        _ => std::cmp::Ordering::Equal,
    }
}

/// 尝试生成索引范围扫描计划（①：IndexRangeScan）
///
/// 条件：
/// 1. WHERE 可被单列范围条件完全表示（col >/>=/</<= literal，AND 合并）
/// 2. 该列是某个普通索引（SkipList）的首键列
/// 3. 无 GROUP BY / HAVING / ORDER BY / 聚合（由调用方保证）
fn try_index_range_scan(
    stmt: &SelectStmt,
    db: &Database,
    table_name: &str,
    scan_column_indices: &[usize],
) -> Option<PhysicalPlan> {
    // 必须有 WHERE 条件
    let where_expr = stmt.where_clause.as_ref()?;

    // 提取范围条件
    let range = extract_range_condition(where_expr)?;
    if range.low.is_none() && range.high.is_none() {
        return None;
    }

    // 查找表和匹配的索引
    let table = db.get_engine_table(table_name)?;
    let col_idx = table.def().column_index(&range.col_name)?;

    for idx_def in &table.def().indexes {
        // 只考虑普通跳表索引（位图/布隆/向量不走回表路径）
        let is_skiplist = idx_def.index_type.is_empty()
            || idx_def.index_type.eq_ignore_ascii_case("skiplist")
            || idx_def.index_type.eq_ignore_ascii_case("btree")
            || idx_def.index_type.eq_ignore_ascii_case("default");
        if !is_skiplist {
            continue;
        }
        if idx_def.key_columns.first() == Some(&col_idx) {
            return Some(PhysicalPlan::IndexRangeScan {
                table_name: table_name.to_string(),
                index_name: idx_def.name.clone(),
                low: range.low.as_ref().map(|(v, _)| v.clone()),
                low_inclusive: range.low.map(|(_, inc)| inc).unwrap_or(false),
                high: range.high.as_ref().map(|(v, _)| v.clone()),
                high_inclusive: range.high.map(|(_, inc)| inc).unwrap_or(false),
                output_column_indices: scan_column_indices.to_vec(),
            });
        }
    }

    None
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
        let table = match db.get_engine_table(table_name) {
            Some(t) => t,
            None => return false,
        };
        let idx_def = match table.def().indexes.iter().find(|i| i.name == *index_name) {
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
        PhysicalPlan::Window { input, .. } => find_scan_plan(input),
        PhysicalPlan::SubqueryScan { plan } => find_scan_plan(plan),
        PhysicalPlan::SetUnion { left, .. } => find_scan_plan(left),
        PhysicalPlan::InsertSelect { source, .. } => find_scan_plan(source),
        _ => None,
    }
}

/// 收集 SELECT 语句中所有被引用的列名
fn collect_referenced_columns(stmt: &SelectStmt, table_cols: &[crate::common::types::ColumnDef]) -> Vec<String> {
    // 性能优化 + 确定性：用 Vec + 末尾检查代替 HashSet
    // - 保证列顺序：先出现在 SELECT 中的列排在前面，WHERE/GROUP BY 等引用的列追加在末尾
    // - 避免 HashSet 的非确定性迭代顺序（影响 IdentityProjection 消除的正确性）
    let mut cols: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut push_if_new = |name: &str, cols: &mut Vec<String>, seen: &mut std::collections::HashSet<String>| {
        if !seen.contains(name) {
            seen.insert(name.to_string());
            cols.push(name.to_string());
        }
    };

    // SELECT 列表
    for item in &stmt.select_list {
        match item {
            SelectItem::Wildcard => {
                // Wildcard：按 schema 顺序展开所有列
                for col in table_cols {
                    if !seen.contains(&col.name) {
                        seen.insert(col.name.clone());
                        cols.push(col.name.clone());
                    }
                }
            }
            SelectItem::Expression(expr, _) => {
                collect_expr_columns_ordered(expr, &mut cols, &mut seen);
            }
        }
    }

    // WHERE
    if let Some(expr) = &stmt.where_clause {
        collect_expr_columns_ordered(expr, &mut cols, &mut seen);
    }

    // GROUP BY
    for expr in &stmt.group_by {
        collect_expr_columns_ordered(expr, &mut cols, &mut seen);
    }

    // HAVING
    if let Some(expr) = &stmt.having {
        collect_expr_columns_ordered(expr, &mut cols, &mut seen);
    }

    // ORDER BY
    for item in &stmt.order_by {
        collect_expr_columns_ordered(&item.expr, &mut cols, &mut seen);
    }

    let _ = push_if_new; // 抑制未使用警告（用闭包风格保留接口一致性）
    cols
}

/// 收集表达式中的列引用，保持出现顺序 + 去重
fn collect_expr_columns_ordered(
    expr: &Expression,
    cols: &mut Vec<String>,
    seen: &mut std::collections::HashSet<String>,
) {
    match expr {
        Expression::ColumnRef { column, .. } => {
            if !seen.contains(column) {
                seen.insert(column.clone());
                cols.push(column.clone());
            }
        }
        Expression::BinaryOp { left, right, .. } => {
            collect_expr_columns_ordered(left, cols, seen);
            collect_expr_columns_ordered(right, cols, seen);
        }
        Expression::UnaryOp { expr, .. } => {
            collect_expr_columns_ordered(expr, cols, seen);
        }
        Expression::Function { args, .. } => {
            for arg in args {
                collect_expr_columns_ordered(arg, cols, seen);
            }
        }
        Expression::Cast { expr, .. } => {
            collect_expr_columns_ordered(expr, cols, seen);
        }
        Expression::InList { expr, list, .. } => {
            collect_expr_columns_ordered(expr, cols, seen);
            for e in list {
                collect_expr_columns_ordered(e, cols, seen);
            }
        }
        Expression::Like { expr, pattern, .. } => {
            collect_expr_columns_ordered(expr, cols, seen);
            collect_expr_columns_ordered(pattern, cols, seen);
        }
        Expression::Case { when_then, else_expr, .. } => {
            for (w, t) in when_then {
                collect_expr_columns_ordered(w, cols, seen);
                collect_expr_columns_ordered(t, cols, seen);
            }
            if let Some(e) = else_expr {
                collect_expr_columns_ordered(e, cols, seen);
            }
        }
        Expression::IsNull(e) | Expression::IsNotNull(e) => {
            collect_expr_columns_ordered(e, cols, seen);
        }
        _ => {} // Literal, Placeholder, Subquery, Wildcard: 无列引用
    }
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
        Expression::Literal(_) | Expression::Placeholder(_) | Expression::Subquery(_) => {}
        Expression::Exists { subquery, .. } | Expression::InSubquery { subquery, .. } => {
            if let Some(ref from) = subquery.from {
                if let TableRef::Table { table_name, .. } = from {
                    // 子查询列引用，暂时忽略
                }
            }
        }
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

/// 递归提取表达式中的聚合函数调用（HAVING 等嵌套场景）
fn collect_agg_exprs(expr: &Expression, out: &mut Vec<(String, Expression, bool)>) {
    match expr {
        Expression::Function { name, args, distinct, .. } => {
            if matches!(name.to_uppercase().as_str(),
                "COUNT" | "SUM" | "AVG" | "MIN" | "MAX")
            {
                let arg = args.first().cloned().unwrap_or(Expression::Literal(Value::Null));
                out.push((name.clone(), arg, *distinct));
                return;
            }
            for a in args {
                collect_agg_exprs(a, out);
            }
        }
        Expression::BinaryOp { left, right, .. } => {
            collect_agg_exprs(left, out);
            collect_agg_exprs(right, out);
        }
        Expression::UnaryOp { expr, .. } => collect_agg_exprs(expr, out),
        Expression::InList { expr, list } => {
            collect_agg_exprs(expr, out);
            for e in list { collect_agg_exprs(e, out); }
        }
        Expression::Like { expr, pattern } => {
            collect_agg_exprs(expr, out);
            collect_agg_exprs(pattern, out);
        }
        Expression::Case { when_then, else_expr } => {
            for (w, t) in when_then {
                collect_agg_exprs(w, out);
                collect_agg_exprs(t, out);
            }
            if let Some(e) = else_expr {
                collect_agg_exprs(e, out);
            }
        }
        Expression::IsNull(e) => collect_agg_exprs(e, out),
        _ => {}
    }
}

/// 将 HAVING 中的聚合函数调用替换为 ColumnRef
///
/// HAVING 条件中的聚合函数（如 SUM(x) > 100）应引用 Aggregate 节点的输出列，
/// 而不是重新执行聚合计算。该函数将聚合函数调用替换为对应的列引用。
///
/// 返回 (替换后的表达式, 聚合列索引列表)
fn rewrite_having_aggregates(
    expr: &Expression,
    group_by_indices: &[usize],
    aggregates: &[AggregateExpr],
    scan_column_names: &[String],
) -> Expression {
    match expr {
        Expression::Function { name, args, distinct, .. } => {
            let upper = name.to_uppercase();
            let is_agg = matches!(upper.as_str(), "COUNT" | "SUM" | "AVG" | "MIN" | "MAX");
            if is_agg {
                // 找到该聚合在 aggregates 列表中的索引
                let input_col = match args.first() {
                    Some(Expression::ColumnRef { column, .. }) => {
                        scan_column_names.iter().position(|c| c == column).unwrap_or(0)
                    }
                    _ => 0,
                };
                let agg_idx = aggregates.iter().position(|a| {
                    let func_name = format!("{:?}", a.func).to_uppercase();
                    func_name == upper && a.input == input_col
                });
                if let Some(idx) = agg_idx {
                    // 使用与 Aggregate 输出相同的列名格式
                    let col_name = format!("{:?}({})", aggregates[idx].func, aggregates[idx].input);
                    return Expression::ColumnRef {
                        table: None,
                        column: col_name,
                    };
                }
            }
            // 非聚合函数，递归处理参数
            let new_args: Vec<Expression> = args.iter()
                .map(|a| rewrite_having_aggregates(a, group_by_indices, aggregates, scan_column_names))
                .collect();
            Expression::Function {
                name: name.clone(),
                args: new_args,
                distinct: *distinct,
                count_star: false,
                over: None,
            }
        }
        Expression::BinaryOp { left, op, right } => {
            Expression::BinaryOp {
                left: Box::new(rewrite_having_aggregates(left, group_by_indices, aggregates, scan_column_names)),
                op: *op,
                right: Box::new(rewrite_having_aggregates(right, group_by_indices, aggregates, scan_column_names)),
            }
        }
        Expression::UnaryOp { op, expr } => {
            Expression::UnaryOp {
                op: *op,
                expr: Box::new(rewrite_having_aggregates(expr, group_by_indices, aggregates, scan_column_names)),
            }
        }
        Expression::Literal(_) | Expression::ColumnRef { .. } | Expression::Placeholder(_) => expr.clone(),
        Expression::Subquery(_) | Expression::Exists { .. } | Expression::InSubquery { .. } => expr.clone(),
        Expression::Like { expr, pattern } => {
            Expression::Like {
                expr: Box::new(rewrite_having_aggregates(expr, group_by_indices, aggregates, scan_column_names)),
                pattern: Box::new(rewrite_having_aggregates(pattern, group_by_indices, aggregates, scan_column_names)),
            }
        }
        Expression::Case { when_then, else_expr } => {
            Expression::Case {
                when_then: when_then.iter().map(|(w, t)| {
                    (rewrite_having_aggregates(w, group_by_indices, aggregates, scan_column_names),
                     rewrite_having_aggregates(t, group_by_indices, aggregates, scan_column_names))
                }).collect(),
                else_expr: else_expr.as_ref().map(|e| Box::new(rewrite_having_aggregates(e, group_by_indices, aggregates, scan_column_names))),
            }
        }
        Expression::InList { expr, list } => {
            Expression::InList {
                expr: Box::new(rewrite_having_aggregates(expr, group_by_indices, aggregates, scan_column_names)),
                list: list.iter().map(|e| rewrite_having_aggregates(e, group_by_indices, aggregates, scan_column_names)).collect(),
            }
        }
        _ => expr.clone(),
    }
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
#[inline]
pub(crate) fn eval_constant_expr(expr: &Expression, params: &[Value]) -> Result<Value> {
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
    let table = db.get_engine_table(&stmt.table_name)
        .ok_or_else(|| EngramDbError::TableNotFound(stmt.table_name.clone()))?;

    // 确定要分析的列索引
    let column_indices = if stmt.columns.is_empty() {
        // 所有列
        (0..table.def().columns.len()).collect()
    } else {
        let mut indices = Vec::new();
        for col_name in &stmt.columns {
            let idx = table.def().columns.iter()
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
        PhysicalPlan::CrossJoin { left, right } => {
            let mut left_names = extract_column_names(left);
            let right_names = extract_column_names(right);
            left_names.extend(right_names);
            left_names
        }
        PhysicalPlan::Limit { input, .. } => extract_column_names(input),
        PhysicalPlan::Window { input, window_functions, column_names } => {
            let mut names = column_names.clone();
            for wf in window_functions {
                names.push(wf.output_name.clone());
            }
            names
        }
        PhysicalPlan::SubqueryScan { plan } => extract_column_names(plan),
        PhysicalPlan::SetUnion { left, .. } => extract_column_names(left),
        PhysicalPlan::InsertSelect { source, .. } => extract_column_names(source),
        _ => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Connection;
    use crate::sql::parser::parse;
    use crate::common::types::EngineType;
    use crate::executor::physical_plan::{
        AggregateFunc, JoinType, PhysicalPlan, SetUnionOp, WindowFuncType,
    };

    fn setup() -> Connection {
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (id INT PRIMARY KEY, name TEXT, age INT, dept TEXT)").unwrap();
        conn.execute("CREATE TABLE u (id INT PRIMARY KEY, tid INT, score INT)").unwrap();
        conn.execute("CREATE TABLE v (id INT PRIMARY KEY, tag TEXT)").unwrap();
        conn.execute("CREATE TABLE log_t (ts INT64, v INT64) ENGINE = Log").unwrap();
        conn.execute("CREATE TABLE mem_t (id INT PRIMARY KEY, v INT) ENGINE = Memory").unwrap();
        let db = conn.database_mut();
        db.create_index("t", "idx_age", &[2], &[1], false).unwrap();
        db.create_index("t", "idx_dept", &[3], &[], false).unwrap();
        conn
    }

    fn plan_sql(conn: &mut Connection, sql: &str) -> Result<PhysicalPlan> {
        let stmt = parse(sql).unwrap();
        plan(stmt, conn.database_mut())
    }

    fn plan_ok(conn: &mut Connection, sql: &str) -> PhysicalPlan {
        plan_sql(conn, sql).unwrap_or_else(|e| panic!("plan failed for {sql:?}: {e}"))
    }

    fn assert_err(conn: &mut Connection, sql: &str) -> EngramDbError {
        match plan_sql(conn, sql) {
            Err(e) => e,
            Ok(p) => panic!("expected error for {sql:?}, got plan {p:?}"),
        }
    }

    fn node_name(plan: &PhysicalPlan) -> &'static str {
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

    fn tree_has(plan: &PhysicalPlan, name: &str) -> bool {
        if node_name(plan) == name {
            return true;
        }
        match plan {
            PhysicalPlan::Filter { input, .. } => tree_has(input, name),
            PhysicalPlan::Projection { input, .. } => tree_has(input, name),
            PhysicalPlan::Aggregate { input, .. } => tree_has(input, name),
            PhysicalPlan::Sort { input, .. } => tree_has(input, name),
            PhysicalPlan::Limit { input, .. } => tree_has(input, name),
            PhysicalPlan::Window { input, .. } => tree_has(input, name),
            PhysicalPlan::Distinct { input } => tree_has(input, name),
            PhysicalPlan::Explain { plan, .. } => tree_has(plan, name),
            PhysicalPlan::SubqueryScan { plan } => tree_has(plan, name),
            PhysicalPlan::HashJoin { left, right, .. } => tree_has(left, name) || tree_has(right, name),
            PhysicalPlan::CrossJoin { left, right } => tree_has(left, name) || tree_has(right, name),
            PhysicalPlan::SetUnion { left, right, .. } => tree_has(left, name) || tree_has(right, name),
            PhysicalPlan::CreateTableAs { source, .. } => tree_has(source, name),
            PhysicalPlan::InsertSelect { source, .. } => tree_has(source, name),
            _ => false,
        }
    }

    fn find_node<'a>(plan: &'a PhysicalPlan, name: &str) -> Option<&'a PhysicalPlan> {
        if node_name(plan) == name {
            return Some(plan);
        }
        match plan {
            PhysicalPlan::Filter { input, .. } => find_node(input, name),
            PhysicalPlan::Projection { input, .. } => find_node(input, name),
            PhysicalPlan::Aggregate { input, .. } => find_node(input, name),
            PhysicalPlan::Sort { input, .. } => find_node(input, name),
            PhysicalPlan::Limit { input, .. } => find_node(input, name),
            PhysicalPlan::Window { input, .. } => find_node(input, name),
            PhysicalPlan::Distinct { input } => find_node(input, name),
            PhysicalPlan::Explain { plan, .. } => find_node(plan, name),
            PhysicalPlan::SubqueryScan { plan } => find_node(plan, name),
            PhysicalPlan::HashJoin { left, right, .. } => find_node(left, name).or_else(|| find_node(right, name)),
            PhysicalPlan::CrossJoin { left, right } => find_node(left, name).or_else(|| find_node(right, name)),
            PhysicalPlan::SetUnion { left, right, .. } => find_node(left, name).or_else(|| find_node(right, name)),
            PhysicalPlan::CreateTableAs { source, .. } => find_node(source, name),
            PhysicalPlan::InsertSelect { source, .. } => find_node(source, name),
            _ => None,
        }
    }

    // ===== Perf01：COUNT(*) 元数据短路 =====
    #[test]
    fn test_count_star_shortcut() {
        let mut conn = setup();
        match plan_ok(&mut conn, "SELECT COUNT(*) FROM t") {
            PhysicalPlan::CountStar { output_name, count } => {
                assert_eq!(output_name, "count(*)");
                assert_eq!(count, 0);
            }
            other => panic!("expected CountStar, got {other:?}"),
        }
        match plan_ok(&mut conn, "SELECT COUNT(*) AS c FROM t") {
            PhysicalPlan::CountStar { output_name, count: 0 } => assert_eq!(output_name, "c"),
            other => panic!("expected CountStar alias, got {other:?}"),
        }
        match plan_ok(&mut conn, "SELECT COUNT(1) FROM t") {
            PhysicalPlan::CountStar { count: 0, .. } => {}
            other => panic!("expected CountStar for COUNT(1), got {other:?}"),
        }
        // COUNT(列) 不短路
        match plan_ok(&mut conn, "SELECT COUNT(id) FROM t") {
            PhysicalPlan::Aggregate { aggregates, .. } => {
                assert_eq!(aggregates.len(), 1);
                assert!(matches!(aggregates[0].func, AggregateFunc::Count));
            }
            other => panic!("expected Aggregate for COUNT(col), got {other:?}"),
        }
        // WHERE 阻断短路
        match plan_ok(&mut conn, "SELECT COUNT(*) FROM t WHERE age > 1") {
            PhysicalPlan::Aggregate { input, aggregates, .. } => {
                assert!(matches!(aggregates[0].func, AggregateFunc::Count));
                assert!(matches!(*input, PhysicalPlan::Filter { .. }));
            }
            other => panic!("expected Aggregate over Filter, got {other:?}"),
        }
        // GROUP BY 阻断短路
        assert!(matches!(
            plan_ok(&mut conn, "SELECT COUNT(*) FROM t GROUP BY dept"),
            PhysicalPlan::Aggregate { .. }
        ));
        // JOIN 不走短路
        assert!(tree_has(&plan_ok(&mut conn, "SELECT COUNT(*) FROM t JOIN u ON t.id = u.tid"),
            "HashJoin"));
    }

    // ===== Perf03：主键点查短路 =====
    #[test]
    fn test_pk_lookup_shortcut() {
        let mut conn = setup();
        match find_node(&plan_ok(&mut conn, "SELECT * FROM t WHERE id = 5"), "PrimaryKeyLookup") {
            Some(PhysicalPlan::PrimaryKeyLookup { pk_value, output_column_indices, .. }) => {
                assert_eq!(*pk_value, crate::Value::Int64(5));
                assert_eq!(*output_column_indices, vec![0, 1, 2, 3]);
            }
            other => panic!("expected PrimaryKeyLookup, got {other:?}"),
        }
        // 字面量在左
        match find_node(&plan_ok(&mut conn, "SELECT * FROM t WHERE 5 = id"), "PrimaryKeyLookup") {
            Some(PhysicalPlan::PrimaryKeyLookup { pk_value, .. }) => {
                assert_eq!(*pk_value, crate::Value::Int64(5));
            }
            other => panic!("expected PrimaryKeyLookup, got {other:?}"),
        }
        // 列裁剪
        match find_node(&plan_ok(&mut conn, "SELECT name FROM t WHERE id = 5"), "PrimaryKeyLookup") {
            Some(PhysicalPlan::PrimaryKeyLookup { output_column_indices, .. }) => {
                // 扫描列 = SELECT 列 + WHERE 引用列（pk 列）
                assert_eq!(*output_column_indices, vec![1, 0]);
            }
            other => panic!("expected column-pruned PK lookup, got {other:?}"),
        }
        // LIMIT 不阻断（P3.3）
        match plan_ok(&mut conn, "SELECT * FROM t WHERE id = 5 LIMIT 1") {
            PhysicalPlan::Limit { input, limit: 1 } => {
                assert!(tree_has(&input, "PrimaryKeyLookup"));
            }
            other => panic!("expected Limit over PK lookup, got {other:?}"),
        }
        // 列对列比较不短路
        assert!(tree_has(&plan_ok(&mut conn, "SELECT * FROM t WHERE id = age"), "Filter"));
        // AND 组合不短路
        assert!(tree_has(&plan_ok(&mut conn, "SELECT * FROM t WHERE id = 5 AND age = 1"),
            "Filter"));
        // ORDER BY 阻断主键短路
        assert!(tree_has(&plan_ok(&mut conn, "SELECT * FROM t WHERE id = 5 ORDER BY name"),
            "Filter"));
        // GROUP BY 阻断
        assert!(matches!(
            plan_ok(&mut conn, "SELECT * FROM t WHERE id = 5 GROUP BY name"),
            PhysicalPlan::Aggregate { .. }
        ));
    }

    // ===== 覆盖索引 / 索引点查 / 范围扫描 =====
    #[test]
    fn test_index_scan_variants() {
        let mut conn = setup();
        // 覆盖索引：name 在 INCLUDE 中 → IndexOnlyScan
        match find_node(&plan_ok(&mut conn, "SELECT name FROM t WHERE age = 3"), "IndexOnlyScan") {
            Some(PhysicalPlan::IndexOnlyScan { index_name, key_value, output_col_map, .. }) => {
                assert_eq!(index_name, "idx_age");
                assert_eq!(*key_value, crate::Value::Int64(3));
                // 扫描列 [name, age]（WHERE 列追加）→ name→included 1, age→key 0
                assert_eq!(*output_col_map, vec![1, 0]);
            }
            other => panic!("expected IndexOnlyScan, got {other:?}"),
        }
        // 键列 + 覆盖列都在索引内
        match find_node(&plan_ok(&mut conn, "SELECT age, name FROM t WHERE age = 3"), "IndexOnlyScan") {
            Some(PhysicalPlan::IndexOnlyScan { output_col_map, .. }) => {
                assert_eq!(*output_col_map, vec![0, 1]);
            }
            other => panic!("expected covering IndexOnlyScan, got {other:?}"),
        }
        // 键列单独覆盖
        match find_node(&plan_ok(&mut conn, "SELECT dept FROM t WHERE dept = 'x'"), "IndexOnlyScan") {
            Some(PhysicalPlan::IndexOnlyScan { index_name, output_col_map, .. }) => {
                assert_eq!(index_name, "idx_dept");
                assert_eq!(*output_col_map, vec![0]);
            }
            other => panic!("expected dept IndexOnlyScan, got {other:?}"),
        }
        // id 不在覆盖范围 → 回表 IndexScan
        match find_node(&plan_ok(&mut conn, "SELECT id FROM t WHERE age = 3"), "IndexScan") {
            Some(PhysicalPlan::IndexScan { index_name, key_value, output_column_indices, .. }) => {
                assert_eq!(index_name, "idx_age");
                assert_eq!(*key_value, crate::Value::Int64(3));
                // 扫描列 = SELECT id + WHERE age
                assert_eq!(*output_column_indices, vec![0, 2]);
            }
            other => panic!("expected IndexScan, got {other:?}"),
        }
        // SELECT * 混合列 → 回表
        assert!(tree_has(&plan_ok(&mut conn, "SELECT * FROM t WHERE age = 3"), "IndexScan"));
        // 无索引列等值 → 全表扫描 + Filter
        let p = plan_ok(&mut conn, "SELECT * FROM t WHERE name = 'x'");
        assert!(tree_has(&p, "Filter"), "{p:?}");
        assert!(tree_has(&p, "TableScan"), "{p:?}");
    }

    #[test]
    fn test_index_range_scan() {
        let mut conn = setup();
        match find_node(&plan_ok(&mut conn, "SELECT name FROM t WHERE age > 3"), "IndexRangeScan") {
            Some(PhysicalPlan::IndexRangeScan { index_name, low, low_inclusive, high, high_inclusive, .. }) => {
                assert_eq!(index_name, "idx_age");
                assert_eq!(*low, Some(crate::Value::Int64(3)));
                assert!(!low_inclusive);
                assert_eq!(*high, None);
                assert!(!high_inclusive);
            }
            other => panic!("expected IndexRangeScan gt, got {other:?}"),
        }
        match find_node(&plan_ok(&mut conn, "SELECT name FROM t WHERE age >= 3"), "IndexRangeScan") {
            Some(PhysicalPlan::IndexRangeScan { low_inclusive, .. }) => assert!(*low_inclusive),
            other => panic!("expected IndexRangeScan ge, got {other:?}"),
        }
        match find_node(&plan_ok(&mut conn, "SELECT name FROM t WHERE age < 5"), "IndexRangeScan") {
            Some(PhysicalPlan::IndexRangeScan { high, high_inclusive, .. }) => {
                assert_eq!(*high, Some(crate::Value::Int64(5)));
                assert!(!high_inclusive);
            }
            other => panic!("expected IndexRangeScan lt, got {other:?}"),
        }
        match find_node(&plan_ok(&mut conn, "SELECT name FROM t WHERE age <= 5"), "IndexRangeScan") {
            Some(PhysicalPlan::IndexRangeScan { high_inclusive, .. }) => assert!(*high_inclusive),
            other => panic!("expected IndexRangeScan le, got {other:?}"),
        }
        // 双边合并
        match find_node(&plan_ok(&mut conn, "SELECT name FROM t WHERE age >= 3 AND age < 10"), "IndexRangeScan") {
            Some(PhysicalPlan::IndexRangeScan { low, low_inclusive, high, high_inclusive, .. }) => {
                assert_eq!(*low, Some(crate::Value::Int64(3)));
                assert!(*low_inclusive);
                assert_eq!(*high, Some(crate::Value::Int64(10)));
                assert!(!high_inclusive);
            }
            other => panic!("expected two-sided IndexRangeScan, got {other:?}"),
        }
        // 边界冲突（下界 > 上界）→ 回退全表扫描
        let p = plan_ok(&mut conn, "SELECT name FROM t WHERE age > 5 AND age < 3");
        assert!(tree_has(&p, "TableScan"), "{p:?}");
        assert!(!tree_has(&p, "IndexRangeScan"), "{p:?}");
        // 三条件（AND 嵌套）无法完全表示 → 回退
        assert!(tree_has(
            &plan_ok(&mut conn, "SELECT name FROM t WHERE age > 3 AND age < 10 AND dept = 'x'"),
            "TableScan"
        ));
        // 不同列范围无法合并 → 回退
        assert!(tree_has(
            &plan_ok(&mut conn, "SELECT id FROM t WHERE age > 1 AND id > 1"),
            "TableScan"
        ));
        // 无索引列范围 → 回退全表扫描 + Filter
        let p = plan_ok(&mut conn, "SELECT id FROM t WHERE name > 'a'");
        assert!(tree_has(&p, "Filter"), "{p:?}");
        assert!(tree_has(&p, "TableScan"), "{p:?}");
    }

    #[test]
    fn test_sort_skip_via_index() {
        let mut conn = setup();
        // 覆盖索引 + ASC 顺序 → 跳过 Sort
        assert!(!tree_has(
            &plan_ok(&mut conn, "SELECT age, name FROM t WHERE age = 3 ORDER BY age"),
            "Sort"
        ));
        // 跳过后底层仍为索引扫描（修复前 ORDER BY 阻断索引优化）
        assert!(tree_has(
            &plan_ok(&mut conn, "SELECT age, name FROM t WHERE age = 3 ORDER BY age"),
            "IndexOnlyScan"
        ));
        // DESC → 保留 Sort
        assert!(tree_has(
            &plan_ok(&mut conn, "SELECT age, name FROM t WHERE age = 3 ORDER BY age DESC"),
            "Sort"
        ));
        // 排序列不在输出列 → sort_keys 解析为空 → 无 Sort 节点（排序列被省略）
        assert!(!tree_has(
            &plan_ok(&mut conn, "SELECT name FROM t WHERE age = 3 ORDER BY age"),
            "Sort"
        ));
        // 无索引普通排序
        match plan_ok(&mut conn, "SELECT id FROM t ORDER BY id DESC") {
            PhysicalPlan::Sort { sort_keys, .. } => {
                assert_eq!(sort_keys[0].column_index, 0);
                assert!(matches!(sort_keys[0].direction,
                    crate::executor::physical_plan::SortDirection::Desc));
            }
            other => panic!("expected Sort, got {other:?}"),
        }
        // LIMIT 随 Sort 传递（Sort 携带 limit，顶层仍叠加 Limit）
        match find_node(&plan_ok(&mut conn, "SELECT id FROM t ORDER BY id LIMIT 5"), "Sort") {
            Some(PhysicalPlan::Sort { limit: Some(5), .. }) => {}
            other => panic!("expected Sort with limit, got {other:?}"),
        }
    }

    // ===== 聚合 / GROUP BY / HAVING =====
    #[test]
    fn test_aggregate_plan() {
        let mut conn = setup();
        match plan_ok(&mut conn, "SELECT dept, COUNT(*) FROM t GROUP BY dept") {
            PhysicalPlan::Aggregate { group_by, aggregates, .. } => {
                assert_eq!(group_by, vec![0]);
                assert_eq!(aggregates.len(), 1);
                assert!(matches!(aggregates[0].func, AggregateFunc::Count));
            }
            other => panic!("expected Aggregate, got {other:?}"),
        }
        match plan_ok(&mut conn, "SELECT dept, SUM(age) FROM t GROUP BY dept") {
            PhysicalPlan::Aggregate { aggregates, .. } => {
                assert!(matches!(aggregates[0].func, AggregateFunc::Sum));
                assert_eq!(aggregates[0].input, 1);
                assert!(!aggregates[0].distinct);
            }
            other => panic!("expected SUM aggregate, got {other:?}"),
        }
        // MIN/MAX/AVG
        for (sql, f) in [
            ("SELECT MIN(age) FROM t", AggregateFunc::Min),
            ("SELECT MAX(age) FROM t", AggregateFunc::Max),
            ("SELECT AVG(age) FROM t", AggregateFunc::Avg),
        ] {
            match plan_ok(&mut conn, sql) {
                PhysicalPlan::Aggregate { aggregates, .. } => {
                    assert!(matches!(aggregates[0].func, f));
                }
                other => panic!("expected {f:?} aggregate, got {other:?}"),
            }
        }
        // HAVING：聚合重写为输出列引用（HAVING 聚合不在 SELECT 中也能重写）
        match plan_ok(&mut conn, "SELECT dept FROM t GROUP BY dept HAVING COUNT(*) > 1") {
            PhysicalPlan::Filter { input, condition } => {
                assert!(matches!(*input, PhysicalPlan::Aggregate { .. }));
                let rewritten = format!("{condition:?}");
                assert!(rewritten.contains("Count(0)"), "HAVING rewrite: {rewritten}");
            }
            other => panic!("expected HAVING Filter, got {other:?}"),
        }
        // HAVING 单独引用聚合：聚合节点必须包含该聚合（修复后）
        match plan_ok(&mut conn, "SELECT dept FROM t GROUP BY dept HAVING SUM(age) > 100") {
            PhysicalPlan::Filter { input, condition } => {
                match *input {
                    PhysicalPlan::Aggregate { aggregates, .. } => {
                        assert!(matches!(aggregates[0].func, AggregateFunc::Sum), "{aggregates:?}");
                    }
                    other => panic!("expected Aggregate, got {other:?}"),
                }
                assert!(format!("{condition:?}").contains("Sum("), "HAVING rewrite: {condition:?}");
            }
            other => panic!("expected HAVING Filter, got {other:?}"),
        }
        // GROUP BY + ORDER BY
        match plan_ok(&mut conn, "SELECT dept FROM t GROUP BY dept ORDER BY dept") {
            PhysicalPlan::Sort { input, .. } => {
                assert!(matches!(*input, PhysicalPlan::Aggregate { .. }));
            }
            other => panic!("expected Sort over Aggregate, got {other:?}"),
        }
        // DISTINCT
        match plan_ok(&mut conn, "SELECT DISTINCT dept FROM t") {
            PhysicalPlan::Distinct { input, .. } => {
                assert!(matches!(*input, PhysicalPlan::Projection { .. }));
            }
            other => panic!("expected Distinct, got {other:?}"),
        }
    }

    // ===== 窗口函数 =====
    #[test]
    fn test_window_plan() {
        let mut conn = setup();
        match find_node(&plan_ok(&mut conn, "SELECT ROW_NUMBER() OVER (ORDER BY id) FROM t"), "Window") {
            Some(PhysicalPlan::Window { window_functions, .. }) => {
                assert_eq!(window_functions.len(), 1);
                assert!(matches!(window_functions[0].func, WindowFuncType::RowNumber));
                assert!(window_functions[0].input_column.is_none());
            }
            other => panic!("expected Window ROW_NUMBER, got {other:?}"),
        }
        match find_node(&plan_ok(&mut conn, "SELECT name, LAG(age, 1) OVER (ORDER BY id) FROM t"), "Window") {
            Some(PhysicalPlan::Window { window_functions, .. }) => {
                assert!(matches!(window_functions[0].func, WindowFuncType::Lag(1)));
                assert_eq!(window_functions[0].input_column, Some(1));
                assert_eq!(window_functions[0].output_name, "lag");
            }
            other => panic!("expected Window LAG, got {other:?}"),
        }
        // 别名作为输出列名
        match find_node(&plan_ok(&mut conn, "SELECT RANK() OVER (ORDER BY id) AS r FROM t"), "Window") {
            Some(PhysicalPlan::Window { window_functions, .. }) => {
                assert!(matches!(window_functions[0].func, WindowFuncType::Rank));
                assert_eq!(window_functions[0].output_name, "r");
            }
            other => panic!("expected Window RANK, got {other:?}"),
        }
        // JOIN 查询窗口函数 → 明确报错
        assert!(matches!(
            assert_err(&mut conn, "SELECT ROW_NUMBER() OVER (ORDER BY t.id) FROM t JOIN u ON t.id = u.tid"),
            EngramDbError::Parse(_)
        ));
    }

    // ===== JOIN 复杂场景 =====
    #[test]
    fn test_join_types() {
        let mut conn = setup();
        for (sql, jt) in [
            ("SELECT * FROM t JOIN u ON t.id = u.tid", JoinType::Inner),
            ("SELECT * FROM t LEFT JOIN u ON t.id = u.tid", JoinType::Left),
            ("SELECT * FROM t RIGHT JOIN u ON t.id = u.tid", JoinType::Right),
            ("SELECT * FROM t FULL JOIN u ON t.id = u.tid", JoinType::Full),
        ] {
            match find_node(&plan_ok(&mut conn, sql), "HashJoin") {
                Some(PhysicalPlan::HashJoin { join_type, left_keys, right_keys, .. }) => {
                    assert_eq!(*join_type, jt);
                    assert_eq!(left_keys.len(), 1);
                    assert_eq!(right_keys.len(), 1);
                }
                other => panic!("expected {jt:?} HashJoin, got {other:?}"),
            }
        }
    }

    #[test]
    fn test_join_keys_and_structure() {
        let mut conn = setup();
        // 多等值键
        match find_node(&plan_ok(&mut conn, "SELECT * FROM t JOIN u ON t.id = u.tid AND t.age = u.score"), "HashJoin") {
            Some(PhysicalPlan::HashJoin { left_keys, right_keys, .. }) => {
                assert_eq!(*left_keys, vec![0, 2]);
                assert_eq!(*right_keys, vec![1, 2]);
            }
            other => panic!("expected multi-key HashJoin, got {other:?}"),
        }
        // 非等值 INNER → CrossJoin + 残留 Filter
        match find_node(&plan_ok(&mut conn, "SELECT * FROM t JOIN u ON t.age > u.score"), "Filter") {
            Some(PhysicalPlan::Filter { input, .. }) => {
                assert!(matches!(input.as_ref(), PhysicalPlan::CrossJoin { .. }));
            }
            other => panic!("expected residual Filter over CrossJoin, got {other:?}"),
        }
        // 非等值 LEFT → 报错
        assert!(matches!(
            assert_err(&mut conn, "SELECT * FROM t LEFT JOIN u ON t.age > u.score"),
            EngramDbError::Parse(_)
        ));
        // CROSS JOIN
        assert!(tree_has(&plan_ok(&mut conn, "SELECT * FROM t CROSS JOIN u"), "CrossJoin"));
        // 三表左深嵌套
        match find_node(&plan_ok(&mut conn, "SELECT * FROM t JOIN u ON t.id = u.tid JOIN v ON u.score = v.id"), "HashJoin") {
            Some(PhysicalPlan::HashJoin { left, .. }) => {
                assert!(matches!(left.as_ref(), PhysicalPlan::HashJoin { .. }));
            }
            other => panic!("expected nested HashJoin, got {other:?}"),
        }
    }

    #[test]
    fn test_join_full_pipeline() {
        let mut conn = setup();
        // JOIN + WHERE + ORDER BY + LIMIT 完整流水线
        match plan_ok(&mut conn,
            "SELECT t.id FROM t JOIN u ON t.id = u.tid WHERE t.age > 1 ORDER BY t.id LIMIT 3")
        {
            PhysicalPlan::Limit { input, limit: 3 } => match *input {
                PhysicalPlan::Sort { input, sort_keys, .. } => {
                    assert_eq!(sort_keys.len(), 1);
                    assert_eq!(sort_keys[0].column_index, 0);
                    match *input {
                        PhysicalPlan::Projection { input, column_names, .. } => {
                            assert_eq!(column_names, vec!["id"]);
                            match *input {
                                PhysicalPlan::Filter { input, .. } => {
                                    assert!(tree_has(&input, "HashJoin"));
                                }
                                other => panic!("expected Filter, got {other:?}"),
                            }
                        }
                        other => panic!("expected Projection, got {other:?}"),
                    }
                }
                other => panic!("expected Sort, got {other:?}"),
            },
            other => panic!("expected Limit pipeline, got {other:?}"),
        }
        // JOIN + GROUP BY 聚合
        match plan_ok(&mut conn, "SELECT u.tid, COUNT(*) FROM t JOIN u ON t.id = u.tid GROUP BY u.tid") {
            PhysicalPlan::Aggregate { group_by, aggregates, .. } => {
                assert_eq!(aggregates.len(), 1);
                assert_eq!(group_by.len(), 1);
            }
            other => panic!("expected join aggregate, got {other:?}"),
        }
        // JOIN 输出列带表前缀（消歧）
        match plan_ok(&mut conn, "SELECT t.id, u.id FROM t JOIN u ON t.id = u.tid") {
            PhysicalPlan::Projection { expressions, column_names, .. } => {
                assert_eq!(column_names, vec!["id", "id"]);
                assert_eq!(expressions.len(), 2);
            }
            other => panic!("expected join projection, got {other:?}"),
        }
        // 列名前缀带表名（消歧作用在表达式里）
        match plan_ok(&mut conn, "SELECT t.id, u.id FROM t JOIN u ON t.id = u.tid") {
            PhysicalPlan::Projection { expressions, .. } => {
                let s0 = format!("{:?}", expressions[0]);
                let s1 = format!("{:?}", expressions[1]);
                assert!(s0.contains("t.id"), "exp0: {s0}");
                assert!(s1.contains("u.id"), "exp1: {s1}");
            }
            other => panic!("expected join projection, got {other:?}"),
        }
    }

    // ===== CTE / 派生表 / 子查询 =====
    #[test]
    fn test_cte_and_derived_tables() {
        let mut conn = setup();
        // CTE 内联 → Derived → SubqueryScan（外层 SELECT 投影保留）
        match find_node(&plan_ok(&mut conn, "WITH c AS (SELECT id FROM t) SELECT * FROM c"),
            "SubqueryScan")
        {
            Some(PhysicalPlan::SubqueryScan { plan }) => {
                assert!(matches!(plan.as_ref(), PhysicalPlan::Projection { .. }));
            }
            other => panic!("expected SubqueryScan from CTE, got {other:?}"),
        }
        // 直接派生表（age 有索引 → 内层为范围扫描）
        match find_node(&plan_ok(&mut conn, "SELECT * FROM (SELECT id, age FROM t WHERE age > 1) AS s"),
            "SubqueryScan")
        {
            Some(PhysicalPlan::SubqueryScan { plan }) => {
                assert!(tree_has(&plan, "Filter"));
                assert!(tree_has(&plan, "IndexRangeScan"));
            }
            other => panic!("expected SubqueryScan, got {other:?}"),
        }
        // 派生表嵌套过滤（内层 Filter + 外层 Filter）
        match find_node(&plan_ok(&mut conn,
            "SELECT s.id FROM (SELECT id FROM t WHERE age > 1) s WHERE s.id > 2"), "Filter")
        {
            Some(PhysicalPlan::Filter { input, .. }) => {
                assert!(matches!(input.as_ref(), PhysicalPlan::SubqueryScan { .. }));
            }
            other => panic!("expected outer Filter over SubqueryScan, got {other:?}"),
        }
        // JOIN 中派生表 → 明确报错
        assert!(matches!(
            assert_err(&mut conn, "SELECT * FROM (SELECT id FROM t) s JOIN u ON s.id = u.tid"),
            EngramDbError::Parse(_)
        ));
    }

    // ===== 集合操作 =====
    #[test]
    fn test_set_operations() {
        let mut conn = setup();
        for (sql, op) in [
            ("SELECT id FROM t UNION SELECT id FROM t", SetUnionOp::Union),
            ("SELECT id FROM t UNION ALL SELECT id FROM t", SetUnionOp::UnionAll),
            ("SELECT id FROM t INTERSECT SELECT id FROM t", SetUnionOp::Intersect),
            ("SELECT id FROM t EXCEPT SELECT id FROM t", SetUnionOp::Except),
        ] {
            match plan_ok(&mut conn, sql) {
                PhysicalPlan::SetUnion { op: got, left, right } => {
                    assert_eq!(got, op);
                    assert!(tree_has(&left, "TableScan"));
                    assert!(tree_has(&right, "TableScan"));
                }
                other => panic!("expected SetUnion {op:?}, got {other:?}"),
            }
        }
    }

    // ===== INSERT 计划路径 =====
    #[test]
    fn test_insert_plan_paths() {
        let mut conn = setup();
        // 默认事务模式：行式 Insert + 字面量求值
        match plan_ok(&mut conn, "INSERT INTO t VALUES (1, 'a', 2, 'x')") {
            PhysicalPlan::Insert { rows, returning, on_conflict, .. } => {
                assert_eq!(rows, vec![vec![
                    crate::Value::Int64(1),
                    crate::Value::Varchar("a".into()),
                    crate::Value::Int64(2),
                    crate::Value::Varchar("x".into()),
                ]]);
                assert!(returning.is_none());
                assert!(on_conflict.is_none());
            }
            other => panic!("expected Insert, got {other:?}"),
        }
        // 多行
        match plan_ok(&mut conn, "INSERT INTO t VALUES (1, 'a', 2, 'x'), (2, 'b', 3, 'y')") {
            PhysicalPlan::Insert { rows, .. } => assert_eq!(rows.len(), 2),
            other => panic!("expected multi-row Insert, got {other:?}"),
        }
        // 列重排 + Null 填充
        match plan_ok(&mut conn, "INSERT INTO t (name, id) VALUES ('a', 1)") {
            PhysicalPlan::Insert { rows, .. } => {
                assert_eq!(rows, vec![vec![
                    crate::Value::Int64(1),
                    crate::Value::Varchar("a".into()),
                    crate::Value::Null,
                    crate::Value::Null,
                ]]);
            }
            other => panic!("expected column-reordered Insert, got {other:?}"),
        }
        // 列子集
        match plan_ok(&mut conn, "INSERT INTO t (id) VALUES (9)") {
            PhysicalPlan::Insert { rows, .. } => {
                assert_eq!(rows, vec![vec![
                    crate::Value::Int64(9),
                    crate::Value::Null,
                    crate::Value::Null,
                    crate::Value::Null,
                ]]);
            }
            other => panic!("expected subset Insert, got {other:?}"),
        }
        // INSERT ... SELECT
        match plan_ok(&mut conn, "INSERT INTO u (tid, score) SELECT age, dept FROM t") {
            PhysicalPlan::InsertSelect { table_name, columns, source } => {
                assert_eq!(table_name, "u");
                assert_eq!(columns, Some(vec!["tid".into(), "score".into()]));
                assert!(tree_has(&source, "TableScan"));
            }
            other => panic!("expected InsertSelect, got {other:?}"),
        }
        // ON CONFLICT DO NOTHING
        match plan_ok(&mut conn, "INSERT INTO t VALUES (1, 'a', 2, 'x') ON CONFLICT DO NOTHING") {
            PhysicalPlan::Insert { on_conflict, rows, .. } => {
                assert_eq!(rows.len(), 1);
                match on_conflict {
                    Some(crate::sql::ast::OnConflictClause {
                        action: crate::sql::ast::OnConflictAction::DoNothing,
                        ..
                    }) => {}
                    other => panic!("expected DoNothing, got {other:?}"),
                }
            }
            other => panic!("expected Insert on conflict, got {other:?}"),
        }
        // RETURNING
        assert!(matches!(
            plan_ok(&mut conn, "INSERT INTO t VALUES (1, 'a', 2, 'x') RETURNING id"),
            PhysicalPlan::Insert { returning: Some(_), .. }
        ));
        // 表不存在
        assert!(matches!(assert_err(&mut conn, "INSERT INTO nosuch VALUES (1)"),
            EngramDbError::TableNotFound(_)));
    }

    #[test]
    fn test_insert_columns_fast_path() {
        // 事务关闭：列式快路径
        let mut conn = Connection::open_with_config(":memory:",
            crate::common::config::Config { enable_transaction: false, ..Default::default() }).unwrap();
        conn.execute("CREATE TABLE t (id INT PRIMARY KEY, name TEXT)").unwrap();
        match plan_ok(&mut conn, "INSERT INTO t VALUES (1, 'a'), (2, 'b')") {
            PhysicalPlan::InsertColumns { table_name, columns } => {
                assert_eq!(table_name, "t");
                assert_eq!(columns, vec![
                    vec![crate::Value::Int64(1), crate::Value::Int64(2)],
                    vec![crate::Value::Varchar("a".into()), crate::Value::Varchar("b".into())],
                ]);
            }
            other => panic!("expected InsertColumns fast path, got {other:?}"),
        }
        // 列名重排阻断快路径
        assert!(matches!(plan_ok(&mut conn, "INSERT INTO t (name) VALUES ('a')"),
            PhysicalPlan::Insert { .. }));
        // 行宽不齐阻断快路径
        assert!(matches!(plan_ok(&mut conn, "INSERT INTO t VALUES (1, 'a', 2)"),
            PhysicalPlan::Insert { .. }));
        // 单行同样走快路径
        assert!(matches!(plan_ok(&mut conn, "INSERT INTO t VALUES (1, 'a')"),
            PhysicalPlan::InsertColumns { .. }));
    }

    // ===== UPDATE / DELETE 计划 =====
    #[test]
    fn test_update_delete_plan() {
        let mut conn = setup();
        match plan_ok(&mut conn, "DELETE FROM t") {
            PhysicalPlan::Delete { condition, .. } => assert!(condition.is_none()),
            other => panic!("expected full Delete, got {other:?}"),
        }
        match plan_ok(&mut conn, "DELETE FROM t WHERE id = 1") {
            PhysicalPlan::Delete { condition, .. } => {
                assert!(matches!(condition, Some(crate::sql::ast::Expression::BinaryOp {
                    op: crate::sql::ast::BinaryOperator::Eq, ..
                })));
            }
            other => panic!("expected conditional Delete, got {other:?}"),
        }
        // Memory 引擎支持 DELETE
        assert!(matches!(plan_ok(&mut conn, "DELETE FROM mem_t WHERE id = 1"),
            PhysicalPlan::Delete { .. }));
        // Log 引擎被 planner 拦截（能力不足 → NotSupported）
        assert!(matches!(assert_err(&mut conn, "DELETE FROM log_t"),
            EngramDbError::NotSupported(_)));
        // 表不存在
        assert!(matches!(assert_err(&mut conn, "DELETE FROM nosuch"),
            EngramDbError::TableNotFound(_)));

        match plan_ok(&mut conn, "UPDATE t SET age = 10") {
            PhysicalPlan::Update { assignments, condition, .. } => {
                assert_eq!(assignments.len(), 1);
                assert_eq!(assignments[0].0, 2);
                assert!(matches!(&assignments[0].1, crate::sql::ast::Expression::Literal(
                    crate::Value::Int64(10))));
                assert!(condition.is_none());
            }
            other => panic!("expected Update, got {other:?}"),
        }
        match plan_ok(&mut conn, "UPDATE t SET age = 10, name = 'z' WHERE id = 1") {
            PhysicalPlan::Update { assignments, condition, .. } => {
                assert_eq!(assignments.len(), 2);
                assert!(condition.is_some());
            }
            other => panic!("expected conditional Update, got {other:?}"),
        }
        // 列不存在 → 明确报错
        assert!(matches!(assert_err(&mut conn, "UPDATE t SET nope = 1"),
            EngramDbError::ColumnNotFound(_)));
        // Log 引擎被 planner 拦截（能力不足 → NotSupported）
        assert!(matches!(assert_err(&mut conn, "UPDATE log_t SET v = 1"),
            EngramDbError::NotSupported(_)));
    }

    // ===== CREATE TABLE / 引擎 =====
    #[test]
    fn test_create_table_plan() {
        let mut conn = setup();
        match plan_ok(&mut conn,
            "CREATE TABLE t2 (id INT PRIMARY KEY AUTO_INCREMENT, name TEXT NOT NULL, score FLOAT)") {
            PhysicalPlan::CreateTable { table_def } => {
                assert_eq!(table_def.columns.len(), 3);
                assert_eq!(table_def.columns[0].name, "id");
                assert!(table_def.columns[0].is_primary_key);
                assert!(table_def.columns[0].auto_increment);
                assert!(!table_def.columns[0].nullable);
                assert!(!table_def.columns[1].nullable);
                assert!(table_def.primary_key_index().is_some());
                assert_eq!(table_def.engine, EngineType::Columnar);
            }
            other => panic!("expected CreateTable, got {other:?}"),
        }
        // 列级 UNIQUE → 自动唯一索引
        match plan_ok(&mut conn, "CREATE TABLE t3 (id INT PRIMARY KEY, email TEXT UNIQUE)") {
            PhysicalPlan::CreateTable { table_def } => {
                assert_eq!(table_def.indexes.len(), 1);
                assert_eq!(table_def.indexes[0].name, "uniq_t3_email");
                assert!(table_def.indexes[0].unique);
            }
            other => panic!("expected auto unique index, got {other:?}"),
        }
        // ENGINE 指定
        match plan_ok(&mut conn, "CREATE TABLE m1 (id INT PRIMARY KEY) ENGINE = Memory") {
            PhysicalPlan::CreateTable { table_def } => {
                assert_eq!(table_def.engine, EngineType::Memory);
            }
            other => panic!("expected Memory CreateTable, got {other:?}"),
        }
        match plan_ok(&mut conn, "CREATE TABLE l1 (id INT PRIMARY KEY) ENGINE = Log") {
            PhysicalPlan::CreateTable { table_def } => {
                assert_eq!(table_def.engine, EngineType::Log);
            }
            other => panic!("expected Log CreateTable, got {other:?}"),
        }
        // 非法引擎
        assert!(matches!(assert_err(&mut conn, "CREATE TABLE b1 (id INT) ENGINE = Nope"),
            EngramDbError::Parse(_)));
        // CTAS
        match plan_ok(&mut conn, "CREATE TABLE t4 AS SELECT id, name FROM t") {
            PhysicalPlan::CreateTableAs { table_def, source } => {
                assert_eq!(table_def.columns.len(), 2);
                assert_eq!(table_def.columns[0].name, "id");
                assert_eq!(table_def.columns[1].name, "name");
                assert!(tree_has(&source, "TableScan"));
            }
            other => panic!("expected CreateTableAs, got {other:?}"),
        }
        // CTAS SELECT * → 报错
        assert!(matches!(
            assert_err(&mut conn, "CREATE TABLE t5 AS SELECT * FROM t"),
            EngramDbError::Parse(_)
        ));
    }

    #[test]
    fn test_create_index_plan() {
        let mut conn = setup();
        match plan_ok(&mut conn, "CREATE INDEX idx_name ON t (name) INCLUDE (age)") {
            PhysicalPlan::CreateIndex { index_name, key_columns, included_columns, unique, .. } => {
                assert_eq!(index_name, "idx_name");
                assert_eq!(key_columns, vec![1]);
                assert_eq!(included_columns, vec![2]);
                assert!(!unique);
            }
            other => panic!("expected CreateIndex, got {other:?}"),
        }
        // 列不存在
        assert!(matches!(assert_err(&mut conn, "CREATE INDEX bad ON t (nosuch)"),
            EngramDbError::ColumnNotFound(_)));
        // 键列重复为覆盖列
        assert!(matches!(
            assert_err(&mut conn, "CREATE INDEX bad2 ON t (age) INCLUDE (age)"),
            EngramDbError::Parse(_)
        ));
        // Log 引擎不支持索引
        assert!(matches!(assert_err(&mut conn, "CREATE INDEX bad3 ON log_t (ts)"),
            EngramDbError::NotSupported(_)));
        // 唯一索引
        match plan_ok(&mut conn, "CREATE UNIQUE INDEX idx_u ON u (tid)") {
            PhysicalPlan::CreateIndex { unique, key_columns, .. } => {
                assert!(unique);
                assert_eq!(key_columns, vec![1]);
            }
            other => panic!("expected unique CreateIndex, got {other:?}"),
        }
    }

    // ===== 杂项语句 =====
    #[test]
    fn test_misc_statement_plans() {
        let mut conn = setup();
        // TRUNCATE：AST 级直接规划（sqlparser 版本语法不兼容）
        match plan(crate::sql::ast::Statement::TruncateTable {
            table_name: "t".into(),
        }, conn.database_mut()).unwrap() {
            PhysicalPlan::TruncateTable { table_name } => assert_eq!(table_name, "t"),
            other => panic!("expected TruncateTable, got {other:?}"),
        }
        assert!(matches!(plan_ok(&mut conn, "BEGIN TRANSACTION"),
            PhysicalPlan::BeginTransaction));
        assert!(matches!(plan_ok(&mut conn, "COMMIT"), PhysicalPlan::Commit));
        assert!(matches!(plan_ok(&mut conn, "ROLLBACK"), PhysicalPlan::Rollback));
        assert!(matches!(plan_ok(&mut conn, "SAVEPOINT sp1"),
            PhysicalPlan::Savepoint { name } if name == "sp1"));
        assert!(matches!(plan_ok(&mut conn, "RELEASE SAVEPOINT sp1"),
            PhysicalPlan::ReleaseSavepoint { name } if name == "sp1"));
        assert!(matches!(plan_ok(&mut conn, "ROLLBACK TO SAVEPOINT sp1"),
            PhysicalPlan::RollbackToSavepoint { name } if name == "sp1"));

        match plan_ok(&mut conn, "EXPLAIN SELECT * FROM t") {
            PhysicalPlan::Explain { analyze: false, plan } => {
                assert!(tree_has(&plan, "TableScan"));
            }
            other => panic!("expected Explain, got {other:?}"),
        }
        assert!(matches!(plan_ok(&mut conn, "EXPLAIN ANALYZE SELECT * FROM t"),
            PhysicalPlan::Explain { analyze: true, .. }));
        assert!(matches!(plan_ok(&mut conn, "PRAGMA table_info = 't'"),
            PhysicalPlan::Pragma(crate::sql::ast::PragmaStmt { name, arg, .. })
                if name == "table_info" && arg.as_deref() == Some("t")));
        // ALTER TABLE：parser 不支持，AST 级直接规划
        let stmt = crate::sql::ast::Statement::AlterTable(crate::sql::ast::AlterTableStmt {
            table_name: "t".into(),
            operation: crate::sql::ast::AlterTableOp::RenameTable { new_name: "t9".into() },
        });
        match plan(stmt, conn.database_mut()).unwrap() {
            PhysicalPlan::AlterTable(crate::sql::ast::AlterTableStmt {
                operation: crate::sql::ast::AlterTableOp::RenameTable { new_name }, ..
            }) => assert_eq!(new_name, "t9"),
            other => panic!("expected AlterTable, got {other:?}"),
        }
        match plan_ok(&mut conn, "ANALYZE TABLE t") {
            PhysicalPlan::Analyze { column_indices, .. } => {
                assert_eq!(column_indices, vec![0, 1, 2, 3]);
            }
            other => panic!("expected Analyze, got {other:?}"),
        }
        // 列级 ANALYZE：AST 直接规划（sqlparser 不支持列列表）
        let stmt = crate::sql::ast::Statement::Analyze(crate::sql::ast::AnalyzeStmt {
            table_name: "t".into(),
            columns: vec!["name".into()],
        });
        match plan(stmt, conn.database_mut()).unwrap() {
            PhysicalPlan::Analyze { column_indices, .. } => assert_eq!(column_indices, vec![1]),
            other => panic!("expected Analyze column, got {other:?}"),
        }
    }

    #[test]
    fn test_materialized_view_plans() {
        let mut conn = setup();
        assert!(matches!(
            plan_ok(&mut conn, "CREATE MATERIALIZED VIEW mv1 AS SELECT id FROM t"),
            PhysicalPlan::CreateMaterializedView { view_name, with_data, .. }
                if view_name == "mv1" && with_data
        ));
        // REFRESH：parser 不支持（非标准语句），AST 级直接规划
        let stmt = crate::sql::ast::Statement::RefreshMaterializedView(
            crate::sql::ast::RefreshMaterializedViewStmt {
                view_name: "mv1".into(),
                concurrently: true,
            });
        match plan(stmt, conn.database_mut()).unwrap() {
            PhysicalPlan::RefreshMaterializedView { concurrently: true, .. } => {}
            other => panic!("expected RefreshMaterializedView, got {other:?}"),
        }
        // DROP：sqlparser 统一走 DROP VIEW
        assert!(matches!(plan_ok(&mut conn, "DROP VIEW mv1"),
            PhysicalPlan::DropMaterializedView { if_exists: false, .. }));
    }

    // ===== 错误路径 =====
    #[test]
    fn test_plan_error_paths() {
        let mut conn = setup();
        assert!(matches!(plan_sql(&mut conn, "SELECT * FROM nosuch"),
            Err(EngramDbError::TableNotFound(_))));
        assert!(matches!(plan_sql(&mut conn, "SELECT 1"),
            Err(EngramDbError::Parse(_))));
        assert!(matches!(plan_sql(&mut conn, "EXPLAIN SELECT * FROM nosuch"),
            Err(EngramDbError::TableNotFound(_))));
        assert!(matches!(plan_sql(&mut conn, "SELECT * FROM vector_search('nosuch', 'idx', '[1.0]', 5)"),
            Err(EngramDbError::TableNotFound(_))));
    }

    // ===== vector_search 表值函数 =====
    #[test]
    fn test_vector_search_plan() {
        let mut conn = setup();
        match plan_ok(&mut conn,
            "SELECT * FROM vector_search('t', 'idx_age', '[1.0, 2.0]', 5)")
        {
            PhysicalPlan::VectorSearch { table_name, index_name, query_vector, k } => {
                assert_eq!(table_name, "t");
                assert_eq!(index_name, "idx_age");
                assert_eq!(query_vector, vec![1.0, 2.0]);
                assert_eq!(k, 5);
            }
            other => panic!("expected VectorSearch, got {other:?}"),
        }
        // 参数不足
        assert!(matches!(
            assert_err(&mut conn, "SELECT * FROM vector_search('t', 'idx_age')"),
            EngramDbError::Parse(_)
        ));
        // k 非整数
        assert!(matches!(
            assert_err(&mut conn, "SELECT * FROM vector_search('t', 'idx', '[1.0]', 'x')"),
            EngramDbError::Parse(_)
        ));
        // 非法向量 JSON
        assert!(matches!(
            assert_err(&mut conn, "SELECT * FROM vector_search('t', 'idx', 'notjson', 5)"),
            EngramDbError::Parse(_)
        ));
        // Log 引擎无向量能力
        assert!(matches!(
            assert_err(&mut conn, "SELECT * FROM vector_search('log_t', 'idx', '[1.0]', 5)"),
            EngramDbError::NotSupported(_)
        ));
    }

    // ===== 参数化计划 =====
    #[test]
    fn test_plan_with_params() {
        let mut conn = setup();
        let stmt = parse("INSERT INTO t VALUES (?, ?, ?, ?)").unwrap();
        match plan_with_params(stmt, conn.database_mut(),
            &[crate::Value::Int64(1), crate::Value::Varchar("a".into()),
              crate::Value::Int64(2), crate::Value::Varchar("x".into())]).unwrap()
        {
            PhysicalPlan::Insert { rows, .. } => {
                assert_eq!(rows, vec![vec![
                    crate::Value::Int64(1),
                    crate::Value::Varchar("a".into()),
                    crate::Value::Int64(2),
                    crate::Value::Varchar("x".into()),
                ]]);
            }
            other => panic!("expected parameterized Insert, got {other:?}"),
        }
        // SELECT 参数替换后仍走主键短路
        let stmt = parse("SELECT * FROM t WHERE id = ?").unwrap();
        match find_node(&plan_with_params(stmt, conn.database_mut(),
            &[crate::Value::Int64(5)]).unwrap(), "PrimaryKeyLookup")
        {
            Some(PhysicalPlan::PrimaryKeyLookup { pk_value, .. }) => {
                assert_eq!(*pk_value, crate::Value::Int64(5));
            }
            other => panic!("expected PK lookup after param substitution, got {other:?}"),
        }
        // DELETE / UPDATE 参数替换
        let stmt = parse("DELETE FROM t WHERE id = ?").unwrap();
        match plan_with_params(stmt, conn.database_mut(), &[crate::Value::Int64(5)]).unwrap() {
            PhysicalPlan::Delete { condition, .. } => {
                // 参数替换后 WHERE id = 5 保持 Eq 表达式（参数已替换为字面量）
                assert!(matches!(condition, Some(crate::sql::ast::Expression::BinaryOp {
                    left, op: crate::sql::ast::BinaryOperator::Eq, right
                }) if matches!(*right, crate::sql::ast::Expression::Literal(crate::Value::Int64(5)))
                    && matches!(*left, crate::sql::ast::Expression::ColumnRef { .. })));
            }
            other => panic!("expected parameterized Delete, got {other:?}"),
        }
        let stmt = parse("UPDATE t SET age = ? WHERE id = ?").unwrap();
        match plan_with_params(stmt, conn.database_mut(),
            &[crate::Value::Int64(10), crate::Value::Int64(5)]).unwrap()
        {
            PhysicalPlan::Update { assignments, .. } => {
                assert_eq!(assignments[0].0, 2);
                assert!(matches!(&assignments[0].1, crate::sql::ast::Expression::Literal(
                    crate::Value::Int64(10))));
            }
            other => panic!("expected parameterized Update, got {other:?}"),
        }
        // 无参数：占位符保留 → 无法短路
        let stmt = parse("SELECT * FROM t WHERE id = ?").unwrap();
        match find_node(&plan_with_params(stmt, conn.database_mut(), &[]).unwrap(), "Filter") {
            Some(PhysicalPlan::Filter { condition, .. }) => {
                // WHERE id = ? → BinaryOp 右操作数为占位符
                assert!(matches!(condition, crate::sql::ast::Expression::BinaryOp { right, .. }
                    if matches!(right.as_ref(), crate::sql::ast::Expression::Placeholder(0))));
            }
            other => panic!("expected placeholder Filter, got {other:?}"),
        }
        // INSERT 参数越界 → 明确报错
        let stmt = parse("INSERT INTO t VALUES (?, ?, ?, ?)").unwrap();
        assert!(matches!(
            plan_with_params(stmt, conn.database_mut(), &[crate::Value::Int64(1)]),
            Err(EngramDbError::Parse(_))
        ));
    }

    // ===== 直通路径 eval_insert_rows =====
    #[test]
    fn test_eval_insert_rows() {
        let mut conn = setup();
        let db = conn.database_mut();
        // 无列名：直接求值
        let stmt = parse("INSERT INTO t VALUES (1, 'a', 2, 'x'), (3, 'b', 4, 'y')").unwrap();
        if let crate::sql::ast::Statement::Insert(s) = stmt {
            let rows = eval_insert_rows(&s, db, &[]).unwrap();
            assert_eq!(rows.len(), 2);
            assert_eq!(rows[0][0], crate::Value::Int64(1));
            assert_eq!(rows[1][2], crate::Value::Int64(4));
        } else {
            panic!("expected Insert stmt");
        }
        // 有列名：Null 填充 + 重排
        let stmt = parse("INSERT INTO t (name, id) VALUES ('a', 1)").unwrap();
        if let crate::sql::ast::Statement::Insert(s) = stmt {
            let rows = eval_insert_rows(&s, db, &[]).unwrap();
            assert_eq!(rows, vec![vec![
                crate::Value::Int64(1),
                crate::Value::Varchar("a".into()),
                crate::Value::Null,
                crate::Value::Null,
            ]]);
        } else {
            panic!("expected Insert stmt");
        }
        // 参数替换
        let stmt = parse("INSERT INTO t VALUES (?, ?, ?, ?)").unwrap();
        if let crate::sql::ast::Statement::Insert(s) = stmt {
            let rows = eval_insert_rows(&s, db,
                &[crate::Value::Int64(7), crate::Value::Varchar("p".into()),
                  crate::Value::Int64(8), crate::Value::Varchar("q".into())]).unwrap();
            assert_eq!(rows[0][3], crate::Value::Varchar("q".into()));
        } else {
            panic!("expected Insert stmt");
        }
        // 参数越界
        let stmt = parse("INSERT INTO t VALUES (?)").unwrap();
        if let crate::sql::ast::Statement::Insert(s) = stmt {
            assert!(matches!(eval_insert_rows(&s, db, &[]), Err(EngramDbError::Parse(_))));
        }
        // 非常量表达式
        let stmt = parse("INSERT INTO t VALUES (id)").unwrap();
        if let crate::sql::ast::Statement::Insert(s) = stmt {
            assert!(matches!(eval_insert_rows(&s, db, &[]), Err(EngramDbError::Parse(_))));
        }
        // 有列名但表不存在
        let stmt = parse("INSERT INTO nosuch (id) VALUES (1)").unwrap();
        if let crate::sql::ast::Statement::Insert(s) = stmt {
            assert!(matches!(eval_insert_rows(&s, db, &[]),
                Err(EngramDbError::TableNotFound(_))));
        }
        // 与 plan_insert 结果一致（无列名）
        let stmt = parse("INSERT INTO t VALUES (1, 'a', 2, 'x')").unwrap();
        if let crate::sql::ast::Statement::Insert(s) = stmt {
            let direct = eval_insert_rows(&s, db, &[]).unwrap();
            match plan_insert(s, db, &[]) {
                Ok(PhysicalPlan::Insert { rows, .. }) => assert_eq!(direct, rows),
                other => panic!("expected plan Insert, got {other:?}"),
            }
        }
    }

    // ===== 表达式提取辅助 =====
    #[test]
    fn test_extract_equality_condition() {
        let col = |n: &str| Expression::ColumnRef { table: None, column: n.to_string() };
        let lit = |v: i64| Expression::Literal(crate::Value::Int64(v));
        let eq = |l: Expression, r: Expression| Expression::BinaryOp {
            left: Box::new(l), op: BinaryOperator::Eq, right: Box::new(r),
        };
        assert_eq!(extract_equality_condition(&eq(col("id"), lit(5))),
            Some(("id".into(), crate::Value::Int64(5))));
        assert_eq!(extract_equality_condition(&eq(lit(5), col("id"))),
            Some(("id".into(), crate::Value::Int64(5))));
        assert_eq!(extract_equality_condition(&eq(col("id"), col("age"))), None);
        assert_eq!(extract_equality_condition(&Expression::Literal(crate::Value::Null)), None);
    }

    #[test]
    fn test_extract_single_bound() {
        let col = |n: &str| Expression::ColumnRef { table: None, column: n.to_string() };
        let lit = |v: i64| Expression::Literal(crate::Value::Int64(v));
        let cmp = |op: BinaryOperator| Expression::BinaryOp {
            left: Box::new(col("age")), op, right: Box::new(lit(3)),
        };
        let r = extract_single_bound(&cmp(BinaryOperator::Gt)).unwrap();
        assert_eq!(r.col_name, "age");
        assert_eq!(r.low, Some((crate::Value::Int64(3), false)));
        assert!(r.high.is_none());
        assert!(extract_single_bound(&cmp(BinaryOperator::GtEq)).unwrap().low
            == Some((crate::Value::Int64(3), true)));
        assert!(extract_single_bound(&cmp(BinaryOperator::Lt)).unwrap().high
            == Some((crate::Value::Int64(3), false)));
        assert!(extract_single_bound(&cmp(BinaryOperator::LtEq)).unwrap().high
            == Some((crate::Value::Int64(3), true)));
        // 反向：字面量在左
        let rev = Expression::BinaryOp {
            left: Box::new(lit(3)), op: BinaryOperator::Gt, right: Box::new(col("age")),
        };
        assert!(extract_single_bound(&rev).unwrap().low == Some((crate::Value::Int64(3), false)));
        // 非比较运算符
        assert!(extract_single_bound(&cmp(BinaryOperator::Eq)).is_none());
        // 列对列
        let colcol = Expression::BinaryOp {
            left: Box::new(col("age")), op: BinaryOperator::Gt, right: Box::new(col("id")),
        };
        assert!(extract_single_bound(&colcol).is_none());
    }

    #[test]
    fn test_extract_range_condition() {
        let col = |n: &str| Expression::ColumnRef { table: None, column: n.to_string() };
        let lit = |v: i64| Expression::Literal(crate::Value::Int64(v));
        let cmp = |op: BinaryOperator, c: &str, v: i64| Expression::BinaryOp {
            left: Box::new(col(c)), op, right: Box::new(lit(v)),
        };
        let and = |l: Expression, r: Expression| Expression::BinaryOp {
            left: Box::new(l), op: BinaryOperator::And, right: Box::new(r),
        };
        // 单边
        assert!(extract_range_condition(&cmp(BinaryOperator::Gt, "age", 3)).is_some());
        // 双边同列
        let two = extract_range_condition(&and(
            cmp(BinaryOperator::GtEq, "age", 3),
            cmp(BinaryOperator::Lt, "age", 10),
        )).unwrap();
        assert_eq!(two.low, Some((crate::Value::Int64(3), true)));
        assert_eq!(two.high, Some((crate::Value::Int64(10), false)));
        // 双边异列 → None
        assert!(extract_range_condition(&and(
            cmp(BinaryOperator::Gt, "age", 3),
            cmp(BinaryOperator::Lt, "id", 10),
        )).is_none());
        // AND 一侧非边界 → None
        assert!(extract_range_condition(&and(
            cmp(BinaryOperator::Gt, "age", 3),
            lit(1),
        )).is_none());
    }

    #[test]
    fn test_merge_range_predicates() {
        let p = |low: Option<(i64, bool)>, high: Option<(i64, bool)>| RangePredicate {
            col_name: "age".to_string(),
            low: low.map(|(v, i)| (crate::Value::Int64(v), i)),
            high: high.map(|(v, i)| (crate::Value::Int64(v), i)),
        };
        // 同值开闭合并：取更严格（开区间）
        let m = merge_range_predicates(p(Some((3, false)), None), p(Some((3, true)), None)).unwrap();
        assert_eq!(m.low, Some((crate::Value::Int64(3), false)));
        // 下界取更大值
        let m = merge_range_predicates(p(Some((3, true)), None), p(Some((5, true)), None)).unwrap();
        assert_eq!(m.low, Some((crate::Value::Int64(5), true)));
        // 上界取更小值
        let m = merge_range_predicates(p(None, Some((10, false))), p(None, Some((8, true)))).unwrap();
        assert_eq!(m.high, Some((crate::Value::Int64(8), true)));
        // 冲突（下界 > 上界）→ None
        assert!(merge_range_predicates(
            p(Some((10, true)), None), p(None, Some((3, true)))).is_none());
        // 值相等上界：开区间更严格
        let m = merge_range_predicates(p(None, Some((8, true))), p(None, Some((8, false)))).unwrap();
        assert_eq!(m.high, Some((crate::Value::Int64(8), false)));
    }

    #[test]
    fn test_value_cmp_planner() {
        use crate::Value;
        assert_eq!(value_cmp_planner(&Value::Int64(3), &Value::Int64(5)),
            std::cmp::Ordering::Less);
        assert_eq!(value_cmp_planner(&Value::Varchar("a".into()), &Value::Varchar("b".into())),
            std::cmp::Ordering::Less);
        // Boolean：false > true（实现语义：!x 比较）
        assert_eq!(value_cmp_planner(&Value::Boolean(false), &Value::Boolean(true)),
            std::cmp::Ordering::Greater);
        // Null 最小
        assert_eq!(value_cmp_planner(&Value::Null, &Value::Int64(0)),
            std::cmp::Ordering::Less);
        // 跨类型 → Equal
        assert_eq!(value_cmp_planner(&Value::Int64(1), &Value::Varchar("a".into())),
            std::cmp::Ordering::Equal);
    }

    #[test]
    fn test_eval_constant_expr() {
        assert_eq!(eval_constant_expr(&Expression::Literal(crate::Value::Int64(1)), &[]).unwrap(),
            crate::Value::Int64(1));
        assert_eq!(eval_constant_expr(
            &Expression::Placeholder(0), &[crate::Value::Int64(9)]).unwrap(),
            crate::Value::Int64(9));
        assert!(matches!(eval_constant_expr(&Expression::Placeholder(2), &[crate::Value::Int64(9)]),
            Err(EngramDbError::Parse(_))));
        assert!(matches!(eval_constant_expr(
            &Expression::ColumnRef { table: None, column: "id".into() }, &[]),
            Err(EngramDbError::Parse(_))));
    }

    #[test]
    fn test_rewrite_having_aggregates() {
        let col = |n: &str| Expression::ColumnRef { table: None, column: n.to_string() };
        let agg = AggregateExpr { func: AggregateFunc::Count, input: 0, distinct: false };
        let gt = Expression::BinaryOp {
            left: Box::new(Expression::Function {
                name: "COUNT".into(),
                args: vec![Expression::ColumnRef { table: None, column: "dept".into() }],
                distinct: false,
                count_star: true,
                over: None,
            }),
            op: BinaryOperator::Gt,
            right: Box::new(Expression::Literal(crate::Value::Int64(1))),
        };
        let rewritten = rewrite_having_aggregates(&gt, &[0], &[agg], &["dept".to_string()]);
        match &rewritten {
            Expression::BinaryOp { left, op: BinaryOperator::Gt, right } => {
                assert!(matches!(left.as_ref(),
                    Expression::ColumnRef { column, .. } if column == "Count(0)"));
                assert!(matches!(right.as_ref(), Expression::Literal(crate::Value::Int64(1))));
            }
            other => panic!("expected rewritten comparison, got {other:?}"),
        }
        // 非聚合函数递归保留
        let f = Expression::Function {
            name: "LOWER".into(),
            args: vec![Expression::Function {
                name: "SUM".into(),
                args: vec![col("age")],
                distinct: false,
                count_star: false,
                over: None,
            }],
            distinct: false,
            count_star: false,
            over: None,
        };
        let rewritten = rewrite_having_aggregates(&f, &[], &[AggregateExpr {
            func: AggregateFunc::Sum, input: 0, distinct: false,
        }], &["age".to_string()]);
        assert!(format!("{rewritten:?}").contains("Sum(0)"), "{rewritten:?}");
    }}
