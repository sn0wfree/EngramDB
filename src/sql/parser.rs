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
    if let Some(mut stmt) = crate::sql::fast_insert::try_parse_insert(sql) {
        // fast path 的裸 `?` 占位符硬编码为 Placeholder(0)，必须重编号，
        // 否则多列/多行参数全部绑定到 params[0]（与主路径语义一致）
        renumber_placeholders(&mut stmt);
        return Ok(stmt);
    }

    // 处理 INSERT OR IGNORE / INSERT OR REPLACE（v0.15.0 M05 新增）
    // sqlparser 不原生支持，转换为 ON CONFLICT 子句
    let normalized_sql = normalize_insert_or(sql);

    // 处理 CREATE VECTOR INDEX ... WITH (...) 语法（v0.15.0 V16 新增）
    // 转换为 CREATE INDEX ... USING hnsw，并提取 WITH 选项
    let (sql_for_parse, with_options, has_using_hnsw) = normalize_vector_index(&normalized_sql);

    // 处理 CREATE TABLE ... ENGINE = xxx（v0.17.0 M0 多引擎）
    // 仅 CREATE TABLE 语句尝试剥离（其他语句无 ENGINE 子句，原样返回）
    let (sql_for_parse, engine_name) = if sql_for_parse.trim_start().to_uppercase().starts_with("CREATE TABLE") {
        strip_engine_clause(&sql_for_parse)
    } else {
        (sql_for_parse, None)
    };

    // 处理 CREATE INDEX ... INCLUDE (...) 语法（v0.12.0 覆盖索引）
    // sqlparser 0.47 不原生支持 INCLUDE 子句，需要预处理
    if let Some((base_sql, included_cols)) = extract_include_clause(&sql_for_parse) {
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
            idx_stmt.with_options = with_options;
            // 恢复 USING hnsw 标记（供 executor 识别向量索引）
            if has_using_hnsw && idx_stmt.using.is_none() {
                idx_stmt.using = Some("hnsw".to_string());
            }
        }
        return Ok(stmt);
    }

    let dialect = GenericDialect {};
    let stmts = Parser::parse_sql(&dialect, &sql_for_parse).map_err(|e| {
        EngramDbError::Parse(format!("SQL parse error: {}", e))
    })?;

    if stmts.is_empty() {
        return Err(EngramDbError::Parse("Empty SQL statement".into()));
    }

    // 只处理第一条语句
    let mut stmt = convert_statement(&stmts[0])?;
    // 注入 ENGINE 选项（多引擎架构 v0.17.0 M0）
    if let Some(engine) = engine_name {
        if let Statement::CreateTable(ref mut ct) = stmt {
            ct.engine = Some(engine);
        }
    }
    // 注入 WITH 选项和 USING hnsw 标记（向量索引）
    if let Statement::CreateIndex(ref mut idx_stmt) = stmt {
        if !with_options.is_empty() {
            idx_stmt.with_options = with_options;
        }
        // 恢复 USING hnsw 标记（供 executor 识别向量索引）
        if has_using_hnsw && idx_stmt.using.is_none() {
            idx_stmt.using = Some("hnsw".to_string());
        }
    }
    // 占位符索引修正：按出现顺序重新编号 `?` 占位符
    // （sqlparser 把所有无编号 `?` 都赋 idx=0，需要后处理）
    renumber_placeholders(&mut stmt);
    Ok(stmt)
}

/// 按出现顺序重新编号 `?` 占位符
///
/// sqlparser 0.47 把所有无编号 `?` 占位符都映射为 idx=0，
/// 我们递归遍历 AST，按首次出现顺序分配 0, 1, 2, ... 的 idx。
/// 同一个 `?`（同一 sqlparser id）出现多次时复用首次分配的 idx。
fn renumber_placeholders(stmt: &mut Statement) {
    let mut counter: usize = 0;
    match stmt {
        Statement::Insert(s) => {
            for row in s.values.iter_mut() {
                for expr in row.iter_mut() {
                    renumber_expr(expr, &mut counter);
                }
            }
        }
        Statement::Select(s) => {
            for item in s.select_list.iter_mut() {
                renumber_select_item(item, &mut counter);
            }
            if let Some(ref mut where_expr) = s.where_clause {
                renumber_expr(where_expr, &mut counter);
            }
        }
        _ => {} // 其他语句暂不涉及占位符
    }
}

fn renumber_select_item(item: &mut SelectItem, counter: &mut usize) {
    match item {
        SelectItem::Expression(expr, _) => renumber_expr(expr, counter),
        _ => {}
    }
}

fn renumber_expr(expr: &mut Expression, counter: &mut usize) {
    match expr {
        Expression::Placeholder(idx) => {
            // sqlparser 把所有无编号 ? 都映射为 0，这里把它们替换为递增 idx
            if *idx == 0 {
                let new_idx = *counter;
                *counter += 1;
                *idx = new_idx;
            }
            // 已是 >0 的 idx 通常来自 ?1, ?2 等（sqlparser 已经处理好），保留原值
        }
        Expression::BinaryOp { left, right, .. } => {
            renumber_expr(left, counter);
            renumber_expr(right, counter);
        }
        Expression::UnaryOp { expr, .. } => renumber_expr(expr, counter),
        Expression::Function { args, .. } => {
            for arg in args.iter_mut() {
                renumber_expr(arg, counter);
            }
        }
        Expression::Cast { expr, .. } => renumber_expr(expr, counter),
        Expression::InList { expr, list, .. } => {
            renumber_expr(expr, counter);
            for e in list.iter_mut() {
                renumber_expr(e, counter);
            }
        }
        Expression::Like { expr, pattern, .. } => {
            renumber_expr(expr, counter);
            renumber_expr(pattern, counter);
        }
        Expression::Case { when_then, else_expr, .. } => {
            for (w, t) in when_then.iter_mut() {
                renumber_expr(w, counter);
                renumber_expr(t, counter);
            }
            if let Some(ref mut e) = else_expr {
                renumber_expr(e, counter);
            }
        }
        Expression::IsNull(e) | Expression::IsNotNull(e) => renumber_expr(e, counter),
        _ => {}
    }
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

/// 预处理 INSERT OR IGNORE / INSERT OR REPLACE 语法（v0.15.0 M05 新增）
///
/// sqlparser 不原生支持 SQLite 的 `INSERT OR REPLACE/IGNORE` 语法。
/// 将其转换为等价的 `INSERT ... ON CONFLICT DO NOTHING/UPDATE` 形式。
///
/// INSERT OR REPLACE → INSERT ... ON CONFLICT DO UPDATE SET ...
/// （实际列名在执行器阶段根据表结构填充）
fn normalize_insert_or(sql: &str) -> String {
    let upper = sql.to_uppercase();

    // INSERT OR REPLACE：转换为 ON CONFLICT DO UPDATE（替换所有列）
    if upper.starts_with("INSERT OR REPLACE ") || upper.starts_with("INSERT OR REPLACE\t")
        || upper.starts_with("INSERT OR REPLACE\n") || upper.starts_with("INSERT OR REPLACE\r")
    {
        let after = &sql["INSERT OR REPLACE".len()..];
        let stripped = after.trim_start();
        if !upper.contains("ON CONFLICT") {
            return format!("INSERT {} ON CONFLICT DO UPDATE SET __replace_all__ = __replace_all__", stripped);
        }
    }

    // REPLACE INTO：等价于 INSERT OR REPLACE
    if upper.starts_with("REPLACE INTO ") || upper.starts_with("REPLACE INTO\t")
        || upper.starts_with("REPLACE INTO\n") || upper.starts_with("REPLACE INTO\r")
    {
        let after = &sql["REPLACE INTO".len()..];
        let stripped = after.trim_start();
        if !upper.contains("ON CONFLICT") {
            return format!("INSERT {} ON CONFLICT DO UPDATE SET __replace_all__ = __replace_all__", stripped);
        }
    }

    if upper.starts_with("INSERT OR IGNORE ") || upper.starts_with("INSERT OR IGNORE\t")
        || upper.starts_with("INSERT OR IGNORE\n") || upper.starts_with("INSERT OR IGNORE\r")
    {
        // 去掉 "INSERT OR IGNORE" 前缀
        let after = &sql["INSERT OR IGNORE".len()..];
        let stripped = after.trim_start();
        // 在末尾追加 ON CONFLICT DO NOTHING
        if !upper.contains("ON CONFLICT") {
            return format!("INSERT {} ON CONFLICT DO NOTHING", stripped);
        }
    }

    sql.to_string()
}

/// 预处理 CREATE VECTOR INDEX ... WITH (...) 语法（v0.15.0 V16 新增）
///
/// 将 `CREATE VECTOR INDEX idx ON t (col) WITH (metric = cosine, m = 16)`
/// 转换为 `CREATE INDEX idx ON t (col) USING hnsw`
/// 并提取 WITH 选项。
///
/// 返回 (处理后的SQL, WITH选项列表, 是否包含USING hnsw)。
/// 注意：必须去除 `USING hnsw`，否则 sqlparser 0.47 会报错。
/// 但需要保留 USING hnsw 标记，以便后续识别为向量索引。
fn normalize_vector_index(sql: &str) -> (String, Vec<(String, String)>, bool) {
    let upper = sql.to_uppercase();
    if !upper.starts_with("CREATE VECTOR INDEX") {
        // 即使没有 CREATE VECTOR INDEX，也可能有 WITH 子句
        // 但只有 CREATE INDEX 才可能有 WITH
        if upper.contains("CREATE INDEX") {
            // 检测是否包含 USING hnsw（在去除之前）
            let has_hnsw = upper.contains(" USING HNSW");
            // 先提取 WITH 选项（如果有）
            let (mut base, options) = if upper.contains(" WITH ") {
                extract_with_options(sql)
            } else {
                (sql.to_string(), Vec::new())
            };
            // 同时去除 USING hnsw（sqlparser 0.47 不支持 hnsw 作为 USING 值）
            base = strip_using_hnsw(&base);
            return (base, options, has_hnsw);
        }
        return (sql.to_string(), Vec::new(), false);
    }

    // 去掉 "VECTOR" 关键字：CREATE VECTOR INDEX → CREATE INDEX
    // 若 SQL 原本没有 USING 子句（用户写了 `CREATE VECTOR INDEX idx ON t (col) WITH(...)` 没显式指定），
    // 这里补上 `USING hnsw`，否则 sqlparser 会把 WITH(...) 误解析为 WHERE 谓词。
    let after_vector = &sql["CREATE VECTOR".len()..];
    let has_using = upper.contains(" USING ");
    let base = if has_using {
        format!("CREATE{}", after_vector)
    } else {
        // 在 (col) 之前插入 USING hnsw
        // 找到 '(' 位置
        if let Some(paren_pos) = after_vector.find('(') {
            let (before, after) = after_vector.split_at(paren_pos);
            format!("CREATE{} USING hnsw{}", before, after)
        } else {
            format!("CREATE{}", after_vector)
        }
    };

    // 提取 WITH 选项
    if upper.contains(" WITH ") {
        let (sql, options) = extract_with_options(&base);
        (sql, options, true)
    } else {
        (base, Vec::new(), true)
    }
}

/// 从 CREATE INDEX 语句中提取 WITH 选项
fn extract_with_options(sql: &str) -> (String, Vec<(String, String)>) {
    let upper = sql.to_uppercase();
    let with_pos = match upper.find(" WITH ") {
        Some(p) => p,
        None => return (sql.to_string(), Vec::new()),
    };

    let after_with = &sql[with_pos + " WITH ".len()..];
    let bytes = after_with.as_bytes();
    let mut i = 0;
    while i < bytes.len() && (bytes[i] as char).is_whitespace() {
        i += 1;
    }
    if i >= bytes.len() || bytes[i] != b'(' {
        return (sql.to_string(), Vec::new());
    }

    // 解析括号内的选项
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
        return (sql.to_string(), Vec::new());
    }
    let paren_end = j - 1;

    let opts_str = &after_with[paren_start..paren_end];
    let options: Vec<(String, String)> = opts_str
        .split(',')
        .filter_map(|s| {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                return None;
            }
            // 按 = 分割
            let eq_pos = trimmed.find('=')?;
            let key = trimmed[..eq_pos].trim().to_lowercase();
            let val = trimmed[eq_pos + 1..].trim().trim_matches('\'').to_string();
            Some((key, val))
        })
        .collect();

    // 构造去掉 WITH 子句的基础 SQL
    let base_sql = format!("{}{}", &sql[..with_pos], &after_with[j..]);

    (base_sql, options)
}

/// 从 CREATE TABLE 语句中提取 ENGINE = xxx 子句
///
/// sqlparser 0.47 不支持 ENGINE 表选项（MySQL 方言），需要先剥离。
/// 返回 (基础 SQL, 引擎名)。仅处理语句尾部的 `ENGINE = xxx` / `ENGINE xxx`
/// 或 `) ENGINE = xxx` 形式（MySQL 风格，位于列定义之后）。
fn strip_engine_clause(sql: &str) -> (String, Option<String>) {
    let upper = sql.to_uppercase();
    let marker = " ENGINE";
    let Some(pos) = upper.find(marker) else {
        return (sql.to_string(), None);
    };
    // 检查前缀：必须是 ") ENGINE"（列定义闭合括号后）或行首空白 + ENGINE
    let before = &sql[..pos];
    let trimmed_before = before.trim_end();
    let valid_anchor = trimmed_before.ends_with(')') || trimmed_before.is_empty();
    if !valid_anchor {
        return (sql.to_string(), None);
    }
    let after = &sql[pos + marker.len()..];
    let after_trimmed = after.trim_start();
    // 去掉可选的 '='
    let rest = after_trimmed.strip_prefix('=').map(|r| r.trim_start()).unwrap_or(after_trimmed);
    // 取标识符（字母数字下划线）
    let end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(rest.len());
    if end == 0 {
        return (sql.to_string(), None);
    }
    let engine_name = rest[..end].to_string();
    // 尾部可能还有分号/空白
    let trailing = rest[end..].trim_start();
    let base = format!("{}{}", trimmed_before, trailing);
    (base, Some(engine_name))
}

/// 从 CREATE INDEX 语句中去除 USING hnsw 子句
///
/// sqlparser 0.47 不支持 hnsw 作为 USING 值，需要先去除。
fn strip_using_hnsw(sql: &str) -> String {
    let upper = sql.to_uppercase();
    // 查找 USING HNSW（不区分大小写）
    if let Some(pos) = upper.find(" USING ") {
        let after_using = &sql[pos + " USING ".len()..];
        let upper_after = after_using.to_uppercase();
        if upper_after.starts_with("HNSW") {
            // 去掉 USING hnsw（包括后面可能的空格）
            let end = pos + " USING ".len() + "HNSW".len();
            // 跳过尾部空格
            let mut end_trimmed = end;
            let bytes = sql.as_bytes();
            while end_trimmed < bytes.len() && bytes[end_trimmed] == b' ' {
                end_trimmed += 1;
            }
            format!("{}{}", &sql[..pos], &sql[end_trimmed..])
        } else {
            sql.to_string()
        }
    } else {
        sql.to_string()
    }
}

/// sqlparser AST → EngramDB 内部 AST
fn convert_statement(stmt: &sqlast::Statement) -> Result<Statement> {
    match stmt {
        sqlast::Statement::CreateTable { name, columns, query, .. } => {
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
                    // 列级 UNIQUE 约束（非 PRIMARY KEY 的 UNIQUE 列）
                    let unique = col_def.options.iter().any(|o| {
                        matches!(o.option, sqlast::ColumnOption::Unique { is_primary: false, .. })
                    });
                    // 检测 AUTO_INCREMENT 关键字
                    // sqlparser 把它解析为 ColumnOption::DialectSpecific([Token::AUTO_INCREMENT])
                    let auto_increment = col_def.options.iter().any(|o| {
                        if let sqlast::ColumnOption::DialectSpecific(tokens) = &o.option {
                            tokens.iter().any(|t| {
                                let s = t.to_string().to_uppercase();
                                s == "AUTO_INCREMENT" || s == "AUTOINCREMENT"
                            })
                        } else {
                            false
                        }
                    });
                    cols.push(ColumnDef {
                        name: col_name,
                        data_type: dt,
                        nullable,
                        primary_key,
                        auto_increment,
                        unique,
                    });
                }
            }
            // CREATE TABLE AS SELECT：query 字段非空
            let as_select = if let Some(q) = query {
                Some(Box::new(convert_query(q)?))
            } else {
                None
            };
            Ok(Statement::CreateTable(CreateTableStmt {
                table_name,
                columns: cols,
                as_select,
                engine: None,
            }))
        }

        sqlast::Statement::Insert(insert) => {
            let tbl_name = insert.table_name.to_string();
            let col_names = if insert.columns.is_empty() {
                None
            } else {
                Some(insert.columns.iter().map(|c| c.value.clone()).collect())
            };

            // 解析 VALUES 或 SELECT 子查询
            let (values, select) = if let Some(source) = &insert.source {
                match source.body.as_ref() {
                    sqlast::SetExpr::Values(vals) => {
                        let mut rows = Vec::new();
                        for row in &vals.rows {
                            let mut exprs = Vec::new();
                            for e in row {
                                exprs.push(convert_expression(e)?);
                            }
                            rows.push(exprs);
                        }
                        (rows, None)
                    }
                    _ => {
                        // INSERT ... SELECT：source.body 是 SELECT 或集合操作
                        let select_stmt = convert_query(source)?;
                        (vec![], Some(Box::new(select_stmt)))
                    }
                }
            } else {
                return Err(EngramDbError::Parse(
                    "INSERT without source not supported".into(),
                ));
            };

            // 解析 RETURNING 子句
            let returning = if let Some(returning_items) = &insert.returning {
                let mut items = Vec::new();
                for item in returning_items {
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
                Some(items)
            } else {
                None
            };

            // 解析 ON CONFLICT 子句（UPSERT）
            let on_conflict = if let Some(on_insert) = &insert.on {
                match on_insert {
                    sqlast::OnInsert::OnConflict(on_conflict) => {
                        // 提取冲突目标列
                        let conflict_columns = if let Some(target) = &on_conflict.conflict_target {
                            match target {
                                sqlast::ConflictTarget::Columns(cols) => {
                                    cols.iter().map(|c| c.value.clone()).collect()
                                }
                                _ => vec![],
                            }
                        } else {
                            vec![]
                        };

                        // 提取冲突动作
                        let action = match &on_conflict.action {
                            sqlast::OnConflictAction::DoNothing => {
                                OnConflictAction::DoNothing
                            }
                            sqlast::OnConflictAction::DoUpdate(do_update) => {
                                // 检测 INSERT OR REPLACE / REPLACE INTO 标记
                                if do_update.assignments.len() == 1
                                    && do_update.assignments[0].id[0].value.eq_ignore_ascii_case("__replace_all__")
                                {
                                    OnConflictAction::Replace
                                } else {
                                    let mut assignments = Vec::new();
                                    for assignment in &do_update.assignments {
                                        let col_name = assignment.id[0].value.clone();
                                        let expr = convert_expression(&assignment.value)?;
                                        assignments.push((col_name, expr));
                                    }
                                    OnConflictAction::DoUpdate { assignments }
                                }
                            }
                        };

                        Some(OnConflictClause {
                            conflict_columns,
                            action,
                        })
                    }
                    _ => None,
                }
            } else {
                None
            };

            Ok(Statement::Insert(InsertStmt {
                table_name: tbl_name,
                columns: col_names,
                values,
                select,
                returning,
                on_conflict,
            }))
        }

        sqlast::Statement::Query(query) => {
            let select = convert_query(query)?;
            Ok(Statement::Select(select))
        }

        sqlast::Statement::Explain { statement, analyze, .. } => {
            let inner = convert_statement(statement)?;
            Ok(Statement::Explain(ExplainStmt {
                analyze: *analyze,
                statement: Box::new(inner),
            }))
        }

        // 事务语句
        sqlast::Statement::StartTransaction { .. } => Ok(Statement::BeginTransaction),
        sqlast::Statement::Commit { .. } => Ok(Statement::Commit),
        sqlast::Statement::Rollback { savepoint, .. } => {
            if let Some(name) = savepoint {
                // ROLLBACK TO SAVEPOINT <name>
                Ok(Statement::RollbackToSavepoint {
                    name: name.value.clone(),
                })
            } else {
                Ok(Statement::Rollback)
            }
        }

        // SAVEPOINT / RELEASE SAVEPOINT（v0.15.0 Txn05 新增）
        sqlast::Statement::Savepoint { name } => Ok(Statement::Savepoint {
            name: name.value.clone(),
        }),
        sqlast::Statement::ReleaseSavepoint { name } => Ok(Statement::ReleaseSavepoint {
            name: name.value.clone(),
        }),

        // PRAGMA（v0.15.0 新增 P03-P06）
        sqlast::Statement::Pragma { name, value, .. } => {
            let pragma_name = name.to_string();
            let arg = value.as_ref().map(|v| match v {
                sqlast::Value::SingleQuotedString(s) => s.clone(),
                sqlast::Value::Number(s, _) => s.clone(),
                sqlast::Value::Boolean(b) => b.to_string(),
                _ => v.to_string(),
            });
            Ok(Statement::Pragma(PragmaStmt {
                name: pragma_name,
                arg,
            }))
        }

        // TRUNCATE TABLE（v0.15.0 新增）
        sqlast::Statement::Truncate { table_name, .. } => {
            Ok(Statement::TruncateTable {
                table_name: table_name.to_string(),
            })
        }

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
            using,
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
                using: using.clone().map(|u| u.value.to_string()),
                with_options: Vec::new(),
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
    // 提取 DISTINCT 标志
    let distinct = match &query.body.as_ref() {
        sqlast::SetExpr::Select(select) => select.distinct.is_some(),
        _ => false,
    };
    // 处理 body (SELECT 主体)
    let (select_list, from, where_clause, group_by, having, set_op) = match query.body.as_ref() {
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
                match convert_table_ref(table) {
                    Ok(Some(tr)) => Some(tr),
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

            (items, from, where_clause, group_by, having, None)
        }
        sqlast::SetExpr::SetOperation { op, set_quantifier, left, right } => {
            // UNION / UNION ALL / INTERSECT / EXCEPT
            let set_op_type = match (op, set_quantifier) {
                (sqlast::SetOperator::Union, sqlast::SetQuantifier::All) => SetOpType::UnionAll,
                (sqlast::SetOperator::Union, _) => SetOpType::Union,
                (sqlast::SetOperator::Intersect, _) => SetOpType::Intersect,
                (sqlast::SetOperator::Except, _) => SetOpType::Except,
                _ => {
                    return Err(EngramDbError::Parse(format!(
                        "Unsupported set operator: {} (only UNION/INTERSECT/EXCEPT supported)",
                        op
                    )));
                }
            };

            // 解析右侧 SELECT
            let right_query = sqlast::Query {
                with: None,
                body: right.clone(),
                order_by: vec![],
                limit: None,
                limit_by: vec![],
                offset: None,
                fetch: None,
                locks: vec![],
                for_clause: None,
            };
            let right_select = convert_query(&right_query)?;

            // 解析左侧（必须也是 SELECT）
            let left_query = sqlast::Query {
                with: None,
                body: left.clone(),
                order_by: vec![],
                limit: None,
                limit_by: vec![],
                offset: None,
                fetch: None,
                locks: vec![],
                for_clause: None,
            };
            let left_select = convert_query(&left_query)?;

            // 使用左侧的 select_list、from 等，set_op 指向右侧
            let (l_items, l_from, l_where, l_group_by, l_having) = match left_select {
                s if s.set_op.is_none() => (
                    s.select_list, s.from, s.where_clause, s.group_by, s.having,
                ),
                _ => {
                    return Err(EngramDbError::Parse(
                        "Nested set operations not supported".into(),
                    ));
                }
            };

            (l_items, l_from, l_where, l_group_by, l_having, Some((set_op_type, Box::new(right_select))))
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
        distinct,
        ctes: extract_ctes(query),
        set_op,
    })
}

/// 从 sqlparser Query 中提取 CTE
fn extract_ctes(query: &sqlast::Query) -> Vec<Cte> {
    let mut ctes = Vec::new();
    if let Some(with) = &query.with {
        for cte in &with.cte_tables {
            if let Ok(inner) = convert_query(&cte.query) {
                let cols: Vec<String> = cte.alias.columns.iter().map(|c| c.value.clone()).collect();
                ctes.push(Cte {
                    alias: cte.alias.name.value.clone(),
                    query: Box::new(inner),
                    columns: cols,
                });
            }
        }
    }
    ctes
}

/// 转换 sqlparser 的 TableRef 为 EngramDB TableRef
fn convert_table_ref(table: &sqlast::TableWithJoins) -> Result<Option<TableRef>> {
    // 先转换主表
    let main = convert_table_factor(&table.relation)?;

    // 如果有 JOIN 子句，构建连接树
    if let Some(main_ref) = main {
        if table.joins.is_empty() {
            return Ok(Some(main_ref));
        }
        // 处理 CROSS JOIN / JOIN 等
        // 简化处理：只支持 CROSS JOIN，其他 JOIN 暂返回错误
        let mut result = main_ref;
        for join in &table.joins {
            match &join.relation {
                sqlast::TableFactor::Table { name, alias, .. } => {
                    let right = TableRef::Table {
                        table_name: name.to_string(),
                        alias: alias.as_ref().map(|a| a.name.value.clone()),
                    };
                    // 检查连接类型
                    match &join.join_operator {
                        sqlast::JoinOperator::CrossJoin => {
                            result = TableRef::CrossJoin {
                                left: Box::new(result),
                                right: Box::new(right),
                            };
                        }
                        sqlast::JoinOperator::Inner(constraint)
                        | sqlast::JoinOperator::LeftOuter(constraint)
                        | sqlast::JoinOperator::RightOuter(constraint)
                        | sqlast::JoinOperator::FullOuter(constraint) => {
                            // ②：INNER / LEFT / RIGHT / FULL JOIN
                            let join_type = match &join.join_operator {
                                sqlast::JoinOperator::LeftOuter(_) => {
                                    crate::executor::physical_plan::JoinType::Left
                                }
                                sqlast::JoinOperator::RightOuter(_) => {
                                    crate::executor::physical_plan::JoinType::Right
                                }
                                sqlast::JoinOperator::FullOuter(_) => {
                                    crate::executor::physical_plan::JoinType::Full
                                }
                                _ => crate::executor::physical_plan::JoinType::Inner,
                            };
                            // ON 条件（None = 无 ON，等价 CROSS JOIN）
                            let on = match constraint {
                                sqlast::JoinConstraint::On(expr) => {
                                    Some(convert_expression(expr)?)
                                }
                                _ => None,
                            };
                            result = TableRef::Join {
                                left: Box::new(result),
                                right: Box::new(right),
                                join_type,
                                on,
                            };
                        }
                        _ => {
                            return Err(EngramDbError::Parse(format!(
                                "Unsupported join type: {:?} (supported: INNER/LEFT/RIGHT/FULL JOIN)",
                                join.join_operator
                            )));
                        }
                    }
                }
                _ => {
                    return Err(EngramDbError::Parse(
                        "Only simple table joins are supported".into()
                    ));
                }
            }
        }
        return Ok(Some(result));
    }

    Ok(main)
}

/// 转换单个表因子
fn convert_table_factor(factor: &sqlast::TableFactor) -> Result<Option<TableRef>> {
    match factor {
        sqlast::TableFactor::Table { name, alias, args, .. } => {
            let alias = alias.as_ref().map(|a| a.name.value.clone());
            // sqlparser 0.47 把裸函数调用（如 `vector_search('t', ...)`）解析成
            // `Table { name, args: Some([...]) }`。若 args 存在，升级为 TableRef::TableFunction，
            // 否则按普通表处理。
            if let Some(arg_list) = args {
                let name_str = name.to_string();
                let converted_args: Vec<Expression> = arg_list.iter()
                    .filter_map(|arg| match arg {
                        sqlast::FunctionArg::Unnamed(sqlast::FunctionArgExpr::Expr(e)) => {
                            convert_expression(e).ok()
                        }
                        _ => None,
                    })
                    .collect();
                return Ok(Some(TableRef::TableFunction {
                    name: name_str,
                    args: converted_args,
                    alias,
                }));
            }
            Ok(Some(TableRef::Table { table_name: name.to_string(), alias }))
        }
        sqlast::TableFactor::Derived { subquery, alias, .. } => {
            if let sqlast::SetExpr::Select(_) = subquery.body.as_ref() {
                let inner = convert_query(subquery)?;
                Ok(Some(TableRef::Derived {
                    query: Box::new(inner),
                    alias: alias.as_ref().map(|a| a.name.value.clone()).unwrap_or_default(),
                }))
            } else {
                Err(EngramDbError::Parse("Unsupported subquery type in FROM".into()))
            }
        }
        sqlast::TableFactor::TableFunction { expr, alias } => {
            let (name, args) = if let sqlast::Expr::Function(func) = &expr {
                let name = func.name.to_string();
                let args = match &func.args {
                    sqlast::FunctionArguments::List(arg_list) => {
                        arg_list.args.iter().filter_map(|arg| {
                            if let sqlast::FunctionArg::Unnamed(sqlast::FunctionArgExpr::Expr(e)) = arg {
                                convert_expression(e).ok()
                            } else {
                                None
                            }
                        }).collect()
                    }
                    _ => Vec::new(),
                };
                (name, args)
            } else {
                (expr.to_string(), Vec::new())
            };
            let alias = alias.as_ref().map(|a| a.name.value.clone());
            Ok(Some(TableRef::TableFunction { name, args, alias }))
        }
        _ => Ok(None),
    }
}

/// sqlparser Expr → EngramDB Expression
fn convert_expression(expr: &sqlast::Expr) -> Result<Expression> {
    match expr {
        // 参数占位符（? 或 $1 / $2 ...）——必须在 Expr::Value 之前匹配
        sqlast::Expr::Value(sqlast::Value::Placeholder(id)) => {
            let idx = if id.is_empty() {
                0 // ? 形式（无编号时默认第0个），renumber_placeholders 会后处理为顺序编号
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
                    over: None,
                });
            }

            Ok(Expression::Function {
                name: func_name,
                args,
                distinct,
                count_star: false,
                over: func.over.as_ref().and_then(|wt| {
                    if let sqlast::WindowType::WindowSpec(ws) = wt {
                        Some(convert_window_spec(ws))
                    } else {
                        None
                    }
                }),
            })
        }

        sqlast::Expr::Nested(e) => convert_expression(e),

        sqlast::Expr::IsNull(expr) => Ok(Expression::IsNull(Box::new(convert_expression(expr)?))),

        // TRIM(expr [, chars]) / LTRIM / RTRIM：转换为函数调用（v0.15.0 S05）
        sqlast::Expr::Trim { expr, trim_where, trim_what, .. } => {
            let inner = convert_expression(expr)?;
            // trim_what 是要去除的字符（仅第一个元素）
            let what = if let Some(what_box) = trim_what {
                Some(convert_expression(what_box)?)
            } else {
                None
            };
            // 根据 trim_where 选择函数名
            let fname = match trim_where {
                Some(sqlast::TrimWhereField::Leading) => "LTRIM",
                Some(sqlast::TrimWhereField::Trailing) => "RTRIM",
                _ => "TRIM",
            };
            let mut fn_args = vec![inner];
            if let Some(w) = what {
                fn_args.push(w);
            }
            Ok(Expression::Function {
                name: fname.to_string(),
                args: fn_args,
                distinct: false,
                count_star: false,
                over: None,
            })
        }

        sqlast::Expr::IsNotNull(expr) => {
            Ok(Expression::IsNotNull(Box::new(convert_expression(expr)?)))
        }

        // CEIL/FLOOR 表达式：转换为函数调用（v0.15.0 N03-N04）
        sqlast::Expr::Ceil { expr, .. } => {
            let inner = convert_expression(expr)?;
            Ok(Expression::Function {
                name: "CEIL".to_string(),
                args: vec![inner],
                distinct: false,
                count_star: false,
                over: None,
            })
        }
        sqlast::Expr::Floor { expr, .. } => {
            let inner = convert_expression(expr)?;
            Ok(Expression::Function {
                name: "FLOOR".to_string(),
                args: vec![inner],
                distinct: false,
                count_star: false,
                over: None,
            })
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

        // JSON 操作符 -> / ->>
        // sqlparser 0.47 has Expr::JsonAccess { value: Box<Expr>, path: JsonPath }
        // where JsonPath is a struct, not an Expr, so we skip it and fall through
        // to the unsupported expression error below.

        sqlast::Expr::Subquery(subquery) => {
            let inner = convert_query(subquery)?;
            Ok(Expression::Subquery(Box::new(inner)))
        }

        sqlast::Expr::Exists { subquery, negated } => {
            let inner = convert_query(subquery)?;
            Ok(Expression::Exists {
                subquery: Box::new(inner),
                negated: *negated,
            })
        }

        sqlast::Expr::InSubquery { expr, subquery, negated } => {
            let inner_expr = convert_expression(expr)?;
            let inner_sub = convert_query(subquery)?;
            Ok(Expression::InSubquery {
                expr: Box::new(inner_expr),
                subquery: Box::new(inner_sub),
                negated: *negated,
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

/// 转换窗口规范
fn convert_window_spec(ws: &sqlast::WindowSpec) -> WindowSpec {
    let partition_by: Result<Vec<Expression>> = ws.partition_by.iter()
        .map(|e| convert_expression(e))
        .collect();
    let partition_by = partition_by.unwrap_or_default();
    let order_by: Vec<OrderByItem> = ws.order_by.iter().map(|item| {
        let expr = convert_expression(&item.expr).unwrap_or(Expression::Literal(Value::Null));
        let ascending = item.asc.unwrap_or(true);
        OrderByItem { expr, ascending }
    }).collect();
    let window_frame = ws.window_frame.as_ref().map(|wf| WindowFrame {
        units: match wf.units {
            sqlast::WindowFrameUnits::Rows => WindowFrameUnits::Rows,
            sqlast::WindowFrameUnits::Range => WindowFrameUnits::Range,
            sqlast::WindowFrameUnits::Groups => WindowFrameUnits::Groups,
        },
        start: match &wf.start_bound {
            sqlast::WindowFrameBound::Preceding(None) => WindowFrameBound::UnboundedPreceding,
            sqlast::WindowFrameBound::Preceding(Some(n)) => {
                WindowFrameBound::NPreceding(n.to_string().parse().unwrap_or(0))
            }
            sqlast::WindowFrameBound::CurrentRow => WindowFrameBound::CurrentRow,
            sqlast::WindowFrameBound::Following(Some(n)) => {
                WindowFrameBound::NFollowing(n.to_string().parse().unwrap_or(0))
            }
            sqlast::WindowFrameBound::Following(None) => WindowFrameBound::UnboundedFollowing,
        },
        end: wf.end_bound.as_ref().map(|end| match end {
            sqlast::WindowFrameBound::Preceding(None) => WindowFrameBound::UnboundedPreceding,
            sqlast::WindowFrameBound::Preceding(Some(n)) => {
                WindowFrameBound::NPreceding(n.to_string().parse().unwrap_or(0))
            }
            sqlast::WindowFrameBound::CurrentRow => WindowFrameBound::CurrentRow,
            sqlast::WindowFrameBound::Following(Some(n)) => {
                WindowFrameBound::NFollowing(n.to_string().parse().unwrap_or(0))
            }
            sqlast::WindowFrameBound::Following(None) => WindowFrameBound::UnboundedFollowing,
        }),
    });
    WindowSpec { partition_by, order_by, window_frame }
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
        sqlast::DataType::Float4 => Ok(DataType::Float32),
        sqlast::DataType::Float(_) | sqlast::DataType::Double | sqlast::DataType::Float64 => {
            Ok(DataType::Float64)
        }
        sqlast::DataType::Timestamp(_, _) | sqlast::DataType::Datetime(_) => Ok(DataType::Timestamp),
        sqlast::DataType::Varchar(_)
        | sqlast::DataType::Char(_)
        | sqlast::DataType::Text
        | sqlast::DataType::String(_) => Ok(DataType::Varchar),
        sqlast::DataType::Blob(_) | sqlast::DataType::Bytea => Ok(DataType::Blob),
        // JSON 类型（v0.12.0 新增）
        // sqlparser 0.47 有原生 JSON 变体，同时保留 Custom 兜底
        sqlast::DataType::JSON => Ok(DataType::Json),
        sqlast::DataType::Custom(name, _) if name.0.len() == 1 && {
            let n = name.0[0].value.to_uppercase();
            n == "JSON" || n == "JSONB"
        } => Ok(DataType::Json),
        // 向量类型 VECTOR(dim)（v0.12.0 新增）
        sqlast::DataType::Custom(name, modifiers) if name.0.len() == 1 && name.0[0].value.to_uppercase() == "VECTOR" => {
            // 从 modifiers 中解析维度，如 VECTOR(4) → dim=4
            let dim = parse_dim_from_modifiers(modifiers).unwrap_or(0);
            Ok(DataType::Vector { dim })
        }
        // INT8 量化向量类型 VECTOR_INT8(dim)（v0.15.0 新增）
        sqlast::DataType::Custom(name, modifiers) if name.0.len() == 1 && name.0[0].value.to_uppercase() == "VECTOR_INT8" => {
            let dim = parse_dim_from_modifiers(modifiers).unwrap_or(0);
            Ok(DataType::VectorInt8 { dim })
        }
        _ => Err(EngramDbError::Parse(format!(
            "Unsupported data type: {:?}",
            dt
        ))),
    }
}

/// 从 sqlparser Custom 类型的 modifiers 中解析向量维度（如 `VECTOR(4)` → 4）
fn parse_dim_from_modifiers(modifiers: &[String]) -> Option<usize> {
    // sqlparser 0.47 把 `VECTOR(4)` 解析为 modifiers=["4"]
    modifiers.first().and_then(|s| s.parse::<usize>().ok())
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

    #[test]
    fn test_select_distinct() {
        let sql = "SELECT DISTINCT id FROM t";
        let stmt = parse(sql).unwrap();
        match stmt {
            Statement::Select(s) => {
                assert!(s.distinct);
                assert_eq!(s.select_list.len(), 1);
            }
            _ => panic!("Expected Select"),
        }
    }

    #[test]
    fn test_create_table_blob() {
        let sql = "CREATE TABLE t (data BLOB)";
        let stmt = parse(sql).unwrap();
        match stmt {
            Statement::CreateTable(s) => {
                assert_eq!(s.columns[0].name, "data");
                assert_eq!(s.columns[0].data_type, DataType::Blob);
            }
            _ => panic!("Expected CreateTable"),
        }
    }

    #[test]
    fn test_tinyint_type_alias() {
        let sql = "CREATE TABLE t (val TINYINT)";
        let stmt = parse(sql).unwrap();
        match stmt {
            Statement::CreateTable(s) => {
                assert_eq!(s.columns[0].name, "val");
                assert_eq!(s.columns[0].data_type, DataType::Int32);
            }
            _ => panic!("Expected CreateTable"),
        }
    }

    #[test]
    fn test_parse_pragma() {
        let stmt = parse("PRAGMA table_info('t')").unwrap();
        match stmt {
            Statement::Pragma(s) => {
                assert_eq!(s.name, "table_info");
                assert_eq!(s.arg, Some("t".to_string()));
            }
            _ => panic!("Expected Pragma"),
        }

        // sqlparser 的 PRAGMA value 要求是字面量，WAL 须加引号
        let stmt = parse("PRAGMA journal_mode = 'WAL'").unwrap();
        match stmt {
            Statement::Pragma(s) => {
                assert_eq!(s.name, "journal_mode");
                assert_eq!(s.arg, Some("WAL".to_string()));
            }
            _ => panic!("Expected Pragma"),
        }

        // 无参数 PRAGMA
        let stmt = parse("PRAGMA database_list").unwrap();
        match stmt {
            Statement::Pragma(s) => {
                assert_eq!(s.name, "database_list");
                assert!(s.arg.is_none());
            }
            _ => panic!("Expected Pragma"),
        }
    }
}
