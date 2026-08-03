//! SQL 解析器
//!
//! 基于 sqlparser-rs (Apache DataFusion 同款解析器)，
//! 将外部 AST 转换为 EngramDB 内部精简 AST。
//!
//! 优势:
//! - 成熟稳定，社区活跃 (DataFusion / Polars / RisingWave 均使用)
//! - 支持 ANSI SQL:2016 + 多种方言
//! - 语法覆盖全面，无需手写边界 case
//! - 内部 AST 保持精简，只转换我们支持的语法

use crate::common::error::{EngramDbError, Result};
use crate::common::types::DataType;
use crate::Value;

use super::ast::*;

use sqlparser::ast as sqlast;
use sqlparser::dialect::GenericDialect;
use sqlparser::parser::Parser;

/// 解析 SQL 语句
pub fn parse(sql: &str) -> Result<Statement> {
    // P3 优化：先尝试轻量级 INSERT 解析器（快 15-25%）
    // 仅对简单 INSERT ... VALUES 有效，失败时回退到完整解析器
    if let Some(stmt) = crate::sql::fast_insert::try_parse_insert(sql) {
        return Ok(stmt);
    }

    // 处理 CREATE INDEX ... INCLUDE (...) 语法（v0.12.0 覆盖索引）
    // sqlparser 0.47 不原生支持 INCLUDE 子句，需要预处理
    if let Some((base_sql, included_cols)) = extract_include_clause(sql) {
        let dialect = GenericDialect {};
        let stmts = Parser::parse_sql(&dialect, &base_sql).map_err(|e| {
            EngramDbError::Parse(format!("SQL parse error: {}", e))
        })?;
        if stmts.is_empty() {
            return Err(EngramDbError::Parse("Empty SQL statement".into()));
        }
        let mut stmt = convert_statement(&stmts[0])?;
        // 注入 INCLUDE 列
        if let Statement::CreateIndex(ref mut idx_stmt) = stmt {
            idx_stmt.included_columns = included_cols;
        }
        return Ok(stmt);
    }

    let dialect = GenericDialect {};
    let stmts = Parser::parse_sql(&dialect, sql).map_err(|e| {
        EngramDbError::Parse(format!("SQL parse error: {}", e))
    })?;

    if stmts.is_empty() {
        return Err(EngramDbError::Parse("Empty SQL statement".into()));
    }

    // 只处理第一条语句
    convert_statement(&stmts[0])
}

/// 从 CREATE INDEX 语句中提取 INCLUDE 子句（v0.12.0 覆盖索引）
///
/// 返回 (去掉 INCLUDE 后的 SQL, INCLUDE 列名列表)。
/// 如果没有 INCLUDE 子句或不是 CREATE INDEX，返回 None。
fn extract_include_clause(sql: &str) -> Option<(String, Vec<String>)> {
    let upper = sql.to_uppercase();
    // 必须是 CREATE INDEX 语句
    if !upper.starts_with("CREATE") || !upper.contains("INDEX") {
        return None;
    }

    // 查找 INCLUDE 关键字（不区分大小写）
    let include_pos = upper.find("INCLUDE")?;
    let after_include = &sql[include_pos + "INCLUDE".len()..];

    // 跳过空白
    let bytes = after_include.as_bytes();
    let mut i = 0;
    while i < bytes.len() && (bytes[i] as char).is_whitespace() {
        i += 1;
    }
    if i >= bytes.len() || bytes[i] != b'(' {
        return None;
    }

    // 解析括号内的列名列表
    let paren_start = i + 1;
    let mut depth = 1;
    let mut j = paren_start;
    while j < bytes.len() && depth > 0 {
        match bytes[j] {
            b'(' => depth += 1,
            b')' => depth -= 1,
            _ => {}
        }
        j += 1;
    }
    if depth != 0 {
        return None;
    }
    let paren_end = j - 1; // 最后一个 ) 的位置

    let cols_str = &after_include[paren_start..paren_end];
    let columns: Vec<String> = cols_str
        .split(',')
        .map(|s| s.trim().trim_matches(|c: char| c == '"' || c == '\'' || c == '`').to_string())
        .filter(|s| !s.is_empty())
        .collect();

    if columns.is_empty() {
        return None;
    }

    // 构造去掉 INCLUDE 子句的基础 SQL
    let base_sql = format!("{}{}", &sql[..include_pos], &after_include[j..]);

    Some((base_sql, columns))
}

/// sqlparser AST → EngramDB 内部 AST
fn convert_statement(stmt: &sqlast::Statement) -> Result<Statement> {
    match stmt {
        sqlast::Statement::CreateTable { name, columns, .. } => {
            let table_name = name.to_string();
            let mut cols = Vec::new();
            for col_def in columns {
                if let sqlast::ColumnDef { name, data_type, .. } = col_def {
                    let col_name = name.value.clone();
                    let dt = convert_data_type(data_type)?;
                    let nullable = !col_def
                        .options
                        .iter()
                        .any(|o| matches!(o.option, sqlast::ColumnOption::NotNull));
                    let primary_key = col_def.options.iter().any(|o| {
                        matches!(o.option, sqlast::ColumnOption::Unique { is_primary: true, .. })
                    });
                    cols.push(ColumnDef {
                        name: col_name,
                        data_type: dt,
                        nullable,
                        primary_key,
                    });
                }
            }
            Ok(Statement::CreateTable(CreateTableStmt {
                table_name,
                columns: cols,
            }))
        }

        sqlast::Statement::Insert(insert) => {
            let tbl_name = insert.table_name.to_string();
            let col_names = if insert.columns.is_empty() {
                None
            } else {
                Some(insert.columns.iter().map(|c| c.value.clone()).collect())
            };

            // 解析 VALUES
            let values = if let Some(source) = &insert.source {
                if let sqlast::SetExpr::Values(vals) = source.body.as_ref() {
                    let mut rows = Vec::new();
                    for row in &vals.rows {
                        let mut exprs = Vec::new();
                        for e in row {
                            exprs.push(convert_expression(e)?);
                        }
                        rows.push(exprs);
                    }
                    rows
                } else {
                    return Err(EngramDbError::Parse(
                        "INSERT with subquery not supported yet".into(),
                    ));
                }
            } else {
                return Err(EngramDbError::Parse(
                    "INSERT without source not supported".into(),
                ));
            };

            Ok(Statement::Insert(InsertStmt {
                table_name: tbl_name,
                columns: col_names,
                values,
                returning: None,
                on_conflict: None,
            }))
        }

        sqlast::Statement::Query(query) => {
            let select = convert_query(query)?;
            Ok(Statement::Select(select))
        }

        // 事务语句
        sqlast::Statement::StartTransaction { .. } => Ok(Statement::BeginTransaction),
        sqlast::Statement::Commit { .. } => Ok(Statement::Commit),
        sqlast::Statement::Rollback { .. } => Ok(Statement::Rollback),

        // ANALYZE：收集统计信息
        sqlast::Statement::Analyze { table_name, .. } => {
            let table = table_name.to_string();
            // 简化：默认分析所有列
            let cols = vec![];
            Ok(Statement::Analyze(AnalyzeStmt {
                table_name: table,
                columns: cols,
            }))
        }

        // CREATE MATERIALIZED VIEW
        sqlast::Statement::CreateView {
            name,
            query,
            materialized,
            ..
        } if *materialized => {
            let view_name = name.to_string();
            let select_stmt = convert_query(query)?;
            // 简化：默认 WITH DATA
            let with_data = true;
            Ok(Statement::CreateMaterializedView(CreateMaterializedViewStmt {
                view_name,
                query: Box::new(select_stmt),
                with_data,
            }))
        }

        // DROP MATERIALIZED VIEW
        sqlast::Statement::Drop {
            object_type,
            names,
            if_exists,
            ..
        } if matches!(object_type, sqlast::ObjectType::View) && names.len() == 1 => {
            // sqlparser 不区分普通 VIEW 和 MATERIALIZED VIEW
            // 这里我们假设 DROP VIEW 也可以删物化视图（简化处理）
            // 实际生产中应区分，这里统一走 DropMaterializedView
            let view_name = names[0].to_string();
            Ok(Statement::DropMaterializedView(DropMaterializedViewStmt {
                view_name,
                if_exists: *if_exists,
            }))
        }

        // CREATE INDEX（v0.12.0 新增，支持覆盖索引 INCLUDE 子句）
        sqlast::Statement::CreateIndex {
            name,
            table_name,
            columns,
            unique,
            ..
        } => {
            let index_name = name.as_ref()
                .map(|n| n.to_string())
                .unwrap_or_else(|| format!("idx_{}", table_name.to_string().replace('.', "_")));
            let tbl_name = table_name.to_string();
            let key_cols: Vec<String> = columns.iter()
                .map(|c| c.expr.to_string())
                .collect();

            // INCLUDE 子句：sqlparser 0.47 的 CreateIndex 不原生支持 INCLUDE
            // 从原始 SQL 中手动解析 INCLUDE (col1, col2, ...)
            let included_cols = Vec::new(); // 暂时为空，下面用自定义解析补充

            Ok(Statement::CreateIndex(CreateIndexStmt {
                index_name,
                table_name: tbl_name,
                key_columns: key_cols,
                included_columns: included_cols,
                unique: *unique,
            }))
        }

        // DELETE（v0.12.0 新增）
        sqlast::Statement::Delete(delete_stmt) => {
            let tables = match &delete_stmt.from {
                sqlast::FromTable::WithFromKeyword(t) => t,
                sqlast::FromTable::WithoutKeyword(t) => t,
            };
            let tbl_name = if tables.is_empty() {
                return Err(EngramDbError::Parse("DELETE requires a table name".into()));
            } else {
                match &tables[0].relation {
                    sqlast::TableFactor::Table { name, .. } => name.to_string(),
                    _ => return Err(EngramDbError::Parse("Unsupported DELETE table type".into())),
                }
            };
            let where_clause = match &delete_stmt.selection {
                Some(expr) => Some(convert_expression(expr)?),
                None => None,
            };
            Ok(Statement::Delete(DeleteStmt {
                table_name: tbl_name,
                where_clause,
            }))
        }

        // UPDATE（v0.12.0 新增）
        sqlast::Statement::Update { table, assignments, selection, .. } => {
            let tbl_name = match &table.relation {
                sqlast::TableFactor::Table { name, .. } => name.to_string(),
                _ => return Err(EngramDbError::Parse("Unsupported UPDATE table type".into())),
            };
            let mut assigns = Vec::new();
            for assign in assignments {
                let col_name = assign.id.iter().map(|i| i.value.clone()).collect::<Vec<_>>().join(".");
                let value_expr = convert_expression(&assign.value)?;
                assigns.push((col_name, value_expr));
            }
            let where_clause = match selection {
                Some(expr) => Some(convert_expression(expr)?),
                None => None,
            };
            Ok(Statement::Update(UpdateStmt {
                table_name: tbl_name,
                assignments: assigns,
                where_clause,
            }))
        }

        // 暂不支持的语句
        _ => Err(EngramDbError::Parse(format!(
            "Unsupported SQL statement: {}",
            stmt_to_str(stmt)
        ))),
    }
}

fn convert_query(query: &sqlast::Query) -> Result<SelectStmt> {
    // 处理 body (SELECT 主体)
    let (select_list, from, where_clause, group_by, having) = match query.body.as_ref() {
        sqlast::SetExpr::Select(select) => {
            // SELECT 列表
            let mut items = Vec::new();
            for item in &select.projection {
                match item {
                    sqlast::SelectItem::Wildcard(_) => {
                        items.push(SelectItem::Wildcard);
                    }
                    sqlast::SelectItem::UnnamedExpr(expr) => {
                        let e = convert_expression(expr)?;
                        items.push(SelectItem::Expression(e, None));
                    }
                    sqlast::SelectItem::ExprWithAlias { expr, alias } => {
                        let e = convert_expression(expr)?;
                        items.push(SelectItem::Expression(e, Some(alias.value.clone())));
                    }
                    _ => {}
                }
            }

            // FROM 子句
            let from = if select.from.is_empty() {
                None
            } else {
                let table = &select.from[0];
                match &table.relation {
                    sqlast::TableFactor::Table { name, alias, .. } => {
                        let alias = alias.as_ref().map(|a| a.name.value.clone());
                        Some(TableRef {
                            table_name: name.to_string(),
                            alias,
                        })
                    }
                    _ => None,
                }
            };

            // WHERE 子句
            let where_clause = match &select.selection {
                Some(expr) => Some(convert_expression(expr)?),
                None => None,
            };

            // GROUP BY
            let group_by: Vec<Expression> = match &select.group_by {
                sqlast::GroupByExpr::All => Vec::new(),
                sqlast::GroupByExpr::Expressions(exprs) => {
                    let mut result = Vec::new();
                    for e in exprs {
                        result.push(convert_expression(e)?);
                    }
                    result
                }
            };

            // HAVING
            let having = match &select.having {
                Some(expr) => Some(convert_expression(expr)?),
                None => None,
            };

            (items, from, where_clause, group_by, having)
        }
        _ => {
            return Err(EngramDbError::Parse(
                "Only SELECT queries are supported".into(),
            ));
        }
    };

    // ORDER BY
    let mut order_by = Vec::new();
    for item in &query.order_by {
        match item {
            sqlast::OrderByExpr { expr, asc, .. } => {
                let e = convert_expression(expr)?;
                let ascending = asc.unwrap_or(true);
                order_by.push(OrderByItem {
                    expr: e,
                    ascending,
                });
            }
        }
    }

    // LIMIT
    let limit = match &query.limit {
        Some(expr) => {
            if let sqlast::Expr::Value(sqlast::Value::Number(n, _)) = expr {
                Some(n.parse::<usize>().map_err(|_| {
                    EngramDbError::Parse("Invalid LIMIT value".into())
                })?)
            } else {
                None
            }
        }
        None => None,
    };

    Ok(SelectStmt {
        select_list,
        from,
        where_clause,
        group_by,
        having,
        order_by,
        limit,
    })
}

/// sqlparser Expr → EngramDB Expression
fn convert_expression(expr: &sqlast::Expr) -> Result<Expression> {
    match expr {
        // 参数占位符（? 或 $1 / $2 ...）——必须在 Expr::Value 之前匹配
        sqlast::Expr::Value(sqlast::Value::Placeholder(id)) => {
            let idx = if id.is_empty() {
                0 // ? 形式（无编号时默认第0个）
            } else {
                // $1, $2 或 ?1, ?2 形式 → 转为 0-based 索引
                let num_str: String = id.chars().skip_while(|c| !c.is_ascii_digit()).collect();
                if num_str.is_empty() {
                    0 // 命名参数（:name/@name/$name）暂按 0 处理
                } else {
                    num_str.parse::<usize>().unwrap_or(0).saturating_sub(1)
                }
            };
            Ok(Expression::Placeholder(idx))
        }

        sqlast::Expr::Value(v) => Ok(Expression::Literal(convert_value(v)?)),

        sqlast::Expr::Identifier(ident) => Ok(Expression::ColumnRef {
            table: None,
            column: ident.value.clone(),
        }),

        sqlast::Expr::CompoundIdentifier(parts) => {
            if parts.len() == 2 {
                Ok(Expression::ColumnRef {
                    table: Some(parts[0].value.clone()),
                    column: parts[1].value.clone(),
                })
            } else {
                Err(EngramDbError::Parse(format!(
                    "Unsupported compound identifier with {} parts",
                    parts.len()
                )))
            }
        }

        sqlast::Expr::BinaryOp { left, op, right } => {
            let left_expr = convert_expression(left)?;
            let right_expr = convert_expression(right)?;
            let op = convert_binary_op(op)?;
            Ok(Expression::BinaryOp {
                left: Box::new(left_expr),
                op,
                right: Box::new(right_expr),
            })
        }

        sqlast::Expr::UnaryOp { op, expr } => {
            let inner = convert_expression(expr)?;
            match op {
                sqlast::UnaryOperator::Not => Ok(Expression::UnaryOp {
                    op: UnaryOperator::Not,
                    expr: Box::new(inner),
                }),
                sqlast::UnaryOperator::Minus => Ok(Expression::UnaryOp {
                    op: UnaryOperator::Negate,
                    expr: Box::new(inner),
                }),
                sqlast::UnaryOperator::Plus => Ok(inner),
                _ => Err(EngramDbError::Parse(format!(
                    "Unsupported unary operator: {:?}",
                    op
                ))),
            }
        }

        sqlast::Expr::Function(func) => {
            let func_name = func.name.to_string().to_uppercase();

            // 解析参数
            let (args, distinct) = match &func.args {
                sqlast::FunctionArguments::None => (Vec::new(), false),
                sqlast::FunctionArguments::Subquery(_) => {
                    return Err(EngramDbError::Parse(
                        "Subquery function arguments not supported".into(),
                    ));
                }
                sqlast::FunctionArguments::List(arg_list) => {
                    let mut args = Vec::new();
                    for arg in &arg_list.args {
                        if let sqlast::FunctionArg::Unnamed(sqlast::FunctionArgExpr::Expr(e)) = arg
                        {
                            args.push(convert_expression(e)?);
                        }
                    }
                    let distinct = matches!(
                        arg_list.duplicate_treatment,
                        Some(sqlast::DuplicateTreatment::Distinct)
                    );
                    (args, distinct)
                }
            };

            // 特殊处理 COUNT(*)
            if func_name == "COUNT" && args.is_empty() {
                return Ok(Expression::Function {
                    name: "COUNT".to_string(),
                    args: vec![Expression::Literal(Value::Int64(1))],
                    distinct: false,
                    count_star: true,
                });
            }

            Ok(Expression::Function {
                name: func_name,
                args,
                distinct,
                count_star: false,
            })
        }

        sqlast::Expr::Nested(e) => convert_expression(e),

        sqlast::Expr::IsNull(expr) => Ok(Expression::IsNull(Box::new(convert_expression(expr)?))),

        sqlast::Expr::IsNotNull(expr) => {
            Ok(Expression::IsNotNull(Box::new(convert_expression(expr)?)))
        }

        sqlast::Expr::InList { expr, list, negated } => {
            let inner = convert_expression(expr)?;
            let mut items = Vec::new();
            for e in list {
                items.push(convert_expression(e)?);
            }
            let in_list = Expression::InList {
                expr: Box::new(inner),
                list: items,
            };
            if *negated {
                Ok(Expression::UnaryOp {
                    op: UnaryOperator::Not,
                    expr: Box::new(in_list),
                })
            } else {
                Ok(in_list)
            }
        }

        sqlast::Expr::Between { expr, negated, low, high } => {
            let inner = convert_expression(expr)?;
            let low_expr = convert_expression(low)?;
            let high_expr = convert_expression(high)?;
            // a BETWEEN b AND c  =>  a >= b AND a <= c
            let between = Expression::BinaryOp {
                left: Box::new(Expression::BinaryOp {
                    left: Box::new(inner.clone()),
                    op: BinaryOperator::GtEq,
                    right: Box::new(low_expr),
                }),
                op: BinaryOperator::And,
                right: Box::new(Expression::BinaryOp {
                    left: Box::new(inner),
                    op: BinaryOperator::LtEq,
                    right: Box::new(high_expr),
                }),
            };
            if *negated {
                Ok(Expression::UnaryOp {
                    op: UnaryOperator::Not,
                    expr: Box::new(between),
                })
            } else {
                Ok(between)
            }
        }

        sqlast::Expr::Like {
            expr,
            pattern,
            negated,
            ..
        } => {
            let inner = convert_expression(expr)?;
            let pat = convert_expression(pattern)?;
            let like = Expression::Like {
                expr: Box::new(inner),
                pattern: Box::new(pat),
            };
            if *negated {
                Ok(Expression::UnaryOp {
                    op: UnaryOperator::Not,
                    expr: Box::new(like),
                })
            } else {
                Ok(like)
            }
        }

        sqlast::Expr::Case {
            operand,
            conditions,
            results,
            else_result,
            ..
        } => {
            // 两种形式: CASE x WHEN a THEN b ... END
            //         CASE WHEN cond THEN res ... END
            let mut when_then = Vec::new();
            for (cond, res) in conditions.iter().zip(results.iter()) {
                let cond_expr = if let Some(op) = operand {
                    // CASE x WHEN a => x = a
                    Expression::BinaryOp {
                        left: Box::new(convert_expression(op)?),
                        op: BinaryOperator::Eq,
                        right: Box::new(convert_expression(cond)?),
                    }
                } else {
                    convert_expression(cond)?
                };
                let then_expr = convert_expression(res)?;
                when_then.push((cond_expr, then_expr));
            }
            let else_expr = match else_result {
                Some(e) => Some(Box::new(convert_expression(e)?)),
                None => None,
            };
            Ok(Expression::Case {
                when_then,
                else_expr,
            })
        }

        sqlast::Expr::Cast {
            expr, data_type, ..
        } => {
            let inner = convert_expression(expr)?;
            let target_type = convert_data_type(data_type)?;
            Ok(Expression::Cast {
                expr: Box::new(inner),
                data_type: target_type,
            })
        }

        _ => Err(EngramDbError::Parse(format!(
            "Unsupported expression: {}",
            expr
        ))),
    }
}

fn convert_value(v: &sqlast::Value) -> Result<Value> {
    match v {
        sqlast::Value::Boolean(b) => Ok(Value::Boolean(*b)),
        sqlast::Value::Number(n, _) => {
            // 尝试解析为整数，失败则为浮点数
            if let Ok(i) = n.parse::<i64>() {
                Ok(Value::Int64(i))
            } else if let Ok(f) = n.parse::<f64>() {
                Ok(Value::Float64(f))
            } else {
                Err(EngramDbError::Parse(format!("Invalid number: {}", n)))
            }
        }
        sqlast::Value::SingleQuotedString(s) => Ok(Value::Varchar(s.clone())),
        sqlast::Value::DollarQuotedString(s) => Ok(Value::Varchar(s.value.clone())),
        sqlast::Value::Null => Ok(Value::Null),
        _ => Err(EngramDbError::Parse(format!("Unsupported value type: {:?}", v))),
    }
}

fn convert_data_type(dt: &sqlast::DataType) -> Result<DataType> {
    match dt {
        sqlast::DataType::Boolean => Ok(DataType::Boolean),
        sqlast::DataType::Int(_)
        | sqlast::DataType::Integer(_)
        | sqlast::DataType::BigInt(_)
        | sqlast::DataType::Int64 => Ok(DataType::Int64),
        sqlast::DataType::SmallInt(_) => Ok(DataType::Int32),
        sqlast::DataType::TinyInt(_) => Ok(DataType::Int32),
        sqlast::DataType::Float(_) | sqlast::DataType::Double | sqlast::DataType::Float64 => {
            Ok(DataType::Float64)
        }
        sqlast::DataType::Varchar(_)
        | sqlast::DataType::Char(_)
        | sqlast::DataType::Text
        | sqlast::DataType::String(_) => Ok(DataType::Varchar),
        // JSON 类型（v0.12.0 新增）
        // sqlparser 0.47 有原生 JSON 变体，同时保留 Custom 兜底
        sqlast::DataType::JSON => Ok(DataType::Json),
        sqlast::DataType::Custom(name, _) if name.0.len() == 1 && {
            let n = name.0[0].value.to_uppercase();
            n == "JSON" || n == "JSONB"
        } => Ok(DataType::Json),
        // 向量类型 VECTOR(dim)（v0.12.0 新增）
        sqlast::DataType::Custom(name, _) if name.0.len() == 1 && name.0[0].value.to_uppercase() == "VECTOR" => {
            // 从类型名的完整字符串中解析维度，如 VECTOR(1536)
            // sqlparser 0.47 的 Custom 类型参数可能以不同形式出现
            // 先用 dim=0 占位，建表时再从列定义中获取
            Ok(DataType::Vector { dim: 0 })
        }
        _ => Err(EngramDbError::Parse(format!(
            "Unsupported data type: {:?}",
            dt
        ))),
    }
}

fn convert_binary_op(op: &sqlast::BinaryOperator) -> Result<BinaryOperator> {
    match op {
        sqlast::BinaryOperator::Plus => Ok(BinaryOperator::Plus),
        sqlast::BinaryOperator::Minus => Ok(BinaryOperator::Minus),
        sqlast::BinaryOperator::Multiply => Ok(BinaryOperator::Multiply),
        sqlast::BinaryOperator::Divide => Ok(BinaryOperator::Divide),
        sqlast::BinaryOperator::Modulo => Ok(BinaryOperator::Modulo),
        sqlast::BinaryOperator::Eq => Ok(BinaryOperator::Eq),
        sqlast::BinaryOperator::NotEq => Ok(BinaryOperator::NotEq),
        sqlast::BinaryOperator::Lt => Ok(BinaryOperator::Lt),
        sqlast::BinaryOperator::LtEq => Ok(BinaryOperator::LtEq),
        sqlast::BinaryOperator::Gt => Ok(BinaryOperator::Gt),
        sqlast::BinaryOperator::GtEq => Ok(BinaryOperator::GtEq),
        sqlast::BinaryOperator::And => Ok(BinaryOperator::And),
        sqlast::BinaryOperator::Or => Ok(BinaryOperator::Or),
        sqlast::BinaryOperator::StringConcat => Ok(BinaryOperator::Concat),
        _ => Err(EngramDbError::Parse(format!(
            "Unsupported binary operator: {:?}",
            op
        ))),
    }
}

fn stmt_to_str(stmt: &sqlast::Statement) -> String {
    let s = format!("{:?}", stmt);
    if s.len() > 50 {
        format!("{}...", &s[..50])
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_create_table() {
        let sql = "CREATE TABLE users (id INT PRIMARY KEY, name VARCHAR NOT NULL, age INT, score DOUBLE)";
        let stmt = parse(sql).unwrap();
        match stmt {
            Statement::CreateTable(s) => {
                assert_eq!(s.table_name, "users");
                assert_eq!(s.columns.len(), 4);
                assert_eq!(s.columns[0].name, "id");
                assert!(s.columns[0].primary_key);
                assert!(!s.columns[1].nullable);
            }
            _ => panic!("Expected CreateTable"),
        }
    }

    #[test]
    fn test_parse_insert() {
        let sql = "INSERT INTO users VALUES (1, 'alice', 25, 95.5)";
        let stmt = parse(sql).unwrap();
        match stmt {
            Statement::Insert(s) => {
                assert_eq!(s.table_name, "users");
                assert_eq!(s.values.len(), 1);
                assert_eq!(s.values[0].len(), 4);
            }
            _ => panic!("Expected Insert"),
        }
    }

    #[test]
    fn test_parse_select_simple() {
        let sql = "SELECT id, name FROM users WHERE age > 18 ORDER BY name LIMIT 10";
        let stmt = parse(sql).unwrap();
        match stmt {
            Statement::Select(s) => {
                assert_eq!(s.select_list.len(), 2);
                assert!(s.from.is_some());
                assert!(s.where_clause.is_some());
                assert_eq!(s.order_by.len(), 1);
                assert_eq!(s.limit, Some(10));
            }
            _ => panic!("Expected Select"),
        }
    }

    #[test]
    fn test_parse_select_star() {
        let sql = "SELECT * FROM users";
        let stmt = parse(sql).unwrap();
        match stmt {
            Statement::Select(s) => {
                assert_eq!(s.select_list.len(), 1);
                match &s.select_list[0] {
                    SelectItem::Wildcard => {}
                    _ => panic!("Expected Wildcard"),
                }
            }
            _ => panic!("Expected Select"),
        }
    }

    #[test]
    fn test_parse_select_with_alias() {
        let sql = "SELECT id AS user_id, name AS user_name FROM users u";
        let stmt = parse(sql).unwrap();
        match stmt {
            Statement::Select(s) => {
                assert_eq!(s.select_list.len(), 2);
                match &s.select_list[0] {
                    SelectItem::Expression(_, alias) => {
                        assert_eq!(alias.as_deref(), Some("user_id"));
                    }
                    _ => panic!("Expected Expression with alias"),
                }
            }
            _ => panic!("Expected Select"),
        }
    }

    #[test]
    fn test_parse_count_star() {
        let sql = "SELECT COUNT(*) FROM users";
        let stmt = parse(sql).unwrap();
        match stmt {
            Statement::Select(s) => {
                match &s.select_list[0] {
                    SelectItem::Expression(Expression::Function { name, count_star, .. }, _) => {
                        assert_eq!(name, "COUNT");
                        assert!(*count_star);
                    }
                    _ => panic!("Expected COUNT(*) function"),
                }
            }
            _ => panic!("Expected Select"),
        }
    }

    #[test]
    fn test_parse_group_by() {
        let sql = "SELECT age, COUNT(*) FROM users GROUP BY age HAVING COUNT(*) > 5";
        let stmt = parse(sql).unwrap();
        match stmt {
            Statement::Select(s) => {
                assert_eq!(s.group_by.len(), 1);
                assert!(s.having.is_some());
            }
            _ => panic!("Expected Select"),
        }
    }

    #[test]
    fn test_parse_transaction() {
        assert!(matches!(parse("BEGIN").unwrap(), Statement::BeginTransaction));
        assert!(matches!(parse("COMMIT").unwrap(), Statement::Commit));
        assert!(matches!(parse("ROLLBACK").unwrap(), Statement::Rollback));
        assert!(matches!(parse("START TRANSACTION").unwrap(), Statement::BeginTransaction));
    }

    #[test]
    fn test_parse_and_or() {
        let sql = "SELECT * FROM users WHERE age > 18 AND score > 80 OR name = 'test'";
        let stmt = parse(sql).unwrap();
        assert!(matches!(stmt, Statement::Select(_)));
    }

    #[test]
    fn test_parse_not_null() {
        let sql = "SELECT * FROM users WHERE name IS NOT NULL";
        let stmt = parse(sql).unwrap();
        assert!(matches!(stmt, Statement::Select(_)));
    }

    #[test]
    fn test_parse_in_list() {
        let sql = "SELECT * FROM users WHERE age IN (18, 20, 25)";
        let stmt = parse(sql).unwrap();
        assert!(matches!(stmt, Statement::Select(_)));
    }

    #[test]
    fn test_parse_between() {
        let sql = "SELECT * FROM users WHERE age BETWEEN 18 AND 30";
        let stmt = parse(sql).unwrap();
        assert!(matches!(stmt, Statement::Select(_)));
    }

    #[test]
    fn test_parse_like() {
        let sql = "SELECT * FROM users WHERE name LIKE 'al%'";
        let stmt = parse(sql).unwrap();
        assert!(matches!(stmt, Statement::Select(_)));
    }

    #[test]
    fn test_parse_case() {
        let sql = "SELECT CASE WHEN score >= 90 THEN 'A' WHEN score >= 80 THEN 'B' ELSE 'C' END FROM users";
        let stmt = parse(sql).unwrap();
        assert!(matches!(stmt, Statement::Select(_)));
    }

    #[test]
    fn test_parse_cast() {
        let sql = "SELECT CAST(age AS DOUBLE) FROM users";
        let stmt = parse(sql).unwrap();
        assert!(matches!(stmt, Statement::Select(_)));
    }
}
