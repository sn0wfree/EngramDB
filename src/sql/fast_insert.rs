//! 轻量级 INSERT 语句解析器（P3 优化）
//!
//! 专门针对 `INSERT INTO ... VALUES (...)` 场景的手写解析器，
//! 绕过 sqlparser-rs 的完整 SQL 语法解析开销，预计提升 15-25%。
//!
//! 支持语法：
//! - INSERT INTO table_name VALUES (v1, v2, ...), (v3, v4, ...), ...
//! - INSERT INTO table_name (col1, col2, ...) VALUES (v1, v2, ...), ...
//!
//! 不支持时返回 None，由调用方回退到完整 sqlparser。

use crate::Value;
use super::ast::*;

/// 尝试快速解析 INSERT 语句
///
/// 成功返回 Some(Statement::Insert(...))，失败返回 None（回退到完整解析器）。
pub fn try_parse_insert(sql: &str) -> Option<Statement> {
    let bytes = sql.as_bytes();
    let mut pos = 0;

    // 跳过前导空白
    skip_whitespace(bytes, &mut pos);

    // 匹配 INSERT (大小写不敏感)
    if !eat_keyword(bytes, &mut pos, b"INSERT") {
        return None;
    }
    skip_whitespace(bytes, &mut pos);

    // 匹配 INTO
    if !eat_keyword(bytes, &mut pos, b"INTO") {
        return None;
    }
    skip_whitespace(bytes, &mut pos);

    // 解析表名
    let table_name = parse_identifier(bytes, &mut pos)?;
    skip_whitespace(bytes, &mut pos);

    // 解析可选的列名列表
    let columns = if pos < bytes.len() && bytes[pos] == b'(' {
        pos += 1; // skip '('
        let mut cols = Vec::new();
        loop {
            skip_whitespace(bytes, &mut pos);
            let col = parse_identifier(bytes, &mut pos)?;
            cols.push(col);
            skip_whitespace(bytes, &mut pos);
            if pos >= bytes.len() {
                return None;
            }
            if bytes[pos] == b')' {
                pos += 1;
                break;
            }
            if bytes[pos] == b',' {
                pos += 1;
                continue;
            }
            return None; // 语法错误
        }
        Some(cols)
    } else {
        None
    };
    skip_whitespace(bytes, &mut pos);

    // 匹配 VALUES
    if !eat_keyword(bytes, &mut pos, b"VALUES") {
        return None;
    }
    skip_whitespace(bytes, &mut pos);

    // 解析多行值
    let mut rows = Vec::new();
    loop {
        skip_whitespace(bytes, &mut pos);
        if pos >= bytes.len() || bytes[pos] != b'(' {
            break;
        }
        pos += 1; // skip '('
        let mut row = Vec::new();
        loop {
            skip_whitespace(bytes, &mut pos);
            let val = parse_value(bytes, &mut pos)?;
            row.push(val);
            skip_whitespace(bytes, &mut pos);
            if pos >= bytes.len() {
                return None;
            }
            if bytes[pos] == b')' {
                pos += 1;
                break;
            }
            if bytes[pos] == b',' {
                pos += 1;
                continue;
            }
            return None;
        }
        rows.push(row);
        skip_whitespace(bytes, &mut pos);

        // 检查是否还有下一行
        if pos < bytes.len() && bytes[pos] == b',' {
            pos += 1;
            continue;
        }
        // 检查是否到达语句末尾（分号或空白）
        skip_whitespace(bytes, &mut pos);
        if pos >= bytes.len() || bytes[pos] == b';' {
            break;
        }
        // 遇到其他字符，可能不是简单 INSERT，回退
        return None;
    }

    if rows.is_empty() {
        return None;
    }

    Some(Statement::Insert(InsertStmt {
        table_name,
        columns,
        values: rows,
    }))
}

/// 跳过空白字符
#[inline]
fn skip_whitespace(bytes: &[u8], pos: &mut usize) {
    while *pos < bytes.len() && bytes[*pos].is_ascii_whitespace() {
        *pos += 1;
    }
}

/// 匹配关键字（大小写不敏感）
#[inline]
fn eat_keyword(bytes: &[u8], pos: &mut usize, keyword: &[u8]) -> bool {
    if *pos + keyword.len() > bytes.len() {
        return false;
    }
    let slice = &bytes[*pos..*pos + keyword.len()];
    if slice.eq_ignore_ascii_case(keyword) {
        // 确保后面是空白或特殊字符（不是标识符的一部分）
        if *pos + keyword.len() >= bytes.len()
            || !bytes[*pos + keyword.len()].is_ascii_alphanumeric()
            && bytes[*pos + keyword.len()] != b'_'
        {
            *pos += keyword.len();
            return true;
        }
    }
    false
}

/// 解析标识符（表名、列名）
fn parse_identifier(bytes: &[u8], pos: &mut usize) -> Option<String> {
    skip_whitespace(bytes, pos);
    if *pos >= bytes.len() {
        return None;
    }

    let start = *pos;

    // 处理带引号的标识符
    if bytes[*pos] == b'"' {
        *pos += 1;
        let content_start = *pos;
        while *pos < bytes.len() && bytes[*pos] != b'"' {
            *pos += 1;
        }
        if *pos >= bytes.len() {
            return None;
        }
        let s = std::str::from_utf8(&bytes[content_start..*pos]).ok()?.to_string();
        *pos += 1; // skip closing quote
        return Some(s);
    }

    // 普通标识符：字母开头，后续字母数字下划线
    if !bytes[*pos].is_ascii_alphabetic() && bytes[*pos] != b'_' {
        return None;
    }
    while *pos < bytes.len()
        && (bytes[*pos].is_ascii_alphanumeric() || bytes[*pos] == b'_')
    {
        *pos += 1;
    }

    if *pos == start {
        return None;
    }

    let s = std::str::from_utf8(&bytes[start..*pos]).ok()?.to_string();
    Some(s)
}

/// 解析值字面量
fn parse_value(bytes: &[u8], pos: &mut usize) -> Option<Expression> {
    skip_whitespace(bytes, pos);
    if *pos >= bytes.len() {
        return None;
    }

    let b = bytes[*pos];

    // NULL
    if b == b'N' || b == b'n' {
        if eat_keyword(bytes, pos, b"NULL") {
            return Some(Expression::Literal(Value::Null));
        }
    }

    // TRUE / FALSE
    if b == b'T' || b == b't' {
        if eat_keyword(bytes, pos, b"TRUE") {
            return Some(Expression::Literal(Value::Boolean(true)));
        }
    }
    if b == b'F' || b == b'f' {
        if eat_keyword(bytes, pos, b"FALSE") {
            return Some(Expression::Literal(Value::Boolean(false)));
        }
    }

    // 字符串（单引号）
    if b == b'\'' {
        *pos += 1;
        let content_start = *pos;
        let mut result = Vec::new();
        while *pos < bytes.len() {
            if bytes[*pos] == b'\'' {
                // 检查是否是转义的单引号 ''
                if *pos + 1 < bytes.len() && bytes[*pos + 1] == b'\'' {
                    result.push(b'\'');
                    *pos += 2;
                    continue;
                }
                break;
            }
            result.push(bytes[*pos]);
            *pos += 1;
        }
        if *pos >= bytes.len() {
            return None;
        }
        *pos += 1; // skip closing quote
        let s = String::from_utf8(result).ok()?;
        return Some(Expression::Literal(Value::Varchar(s)));
    }

    // 数字（整数或浮点数）
    if b.is_ascii_digit() || (b == b'-' && *pos + 1 < bytes.len() && bytes[*pos + 1].is_ascii_digit())
        || (b == b'+' && *pos + 1 < bytes.len() && bytes[*pos + 1].is_ascii_digit())
    {
        let start = *pos;
        if b == b'-' || b == b'+' {
            *pos += 1;
        }
        let mut is_float = false;

        while *pos < bytes.len() && bytes[*pos].is_ascii_digit() {
            *pos += 1;
        }

        // 小数点
        if *pos < bytes.len() && bytes[*pos] == b'.' {
            is_float = true;
            *pos += 1;
            while *pos < bytes.len() && bytes[*pos].is_ascii_digit() {
                *pos += 1;
            }
        }

        // 科学计数法
        if *pos < bytes.len() && (bytes[*pos] == b'e' || bytes[*pos] == b'E') {
            is_float = true;
            *pos += 1;
            if *pos < bytes.len() && (bytes[*pos] == b'+' || bytes[*pos] == b'-') {
                *pos += 1;
            }
            while *pos < bytes.len() && bytes[*pos].is_ascii_digit() {
                *pos += 1;
            }
        }

        let num_str = std::str::from_utf8(&bytes[start..*pos]).ok()?;

        if is_float {
            let f = num_str.parse::<f64>().ok()?;
            return Some(Expression::Literal(Value::Float64(f)));
        } else {
            let i = num_str.parse::<i64>().ok()?;
            return Some(Expression::Literal(Value::Int64(i)));
        }
    }

    // 参数占位符 ? 或 $1
    if b == b'?' {
        *pos += 1;
        // 检查 ?NNN 形式
        let num_start = *pos;
        while *pos < bytes.len() && bytes[*pos].is_ascii_digit() {
            *pos += 1;
        }
        if *pos > num_start {
            let num_str = std::str::from_utf8(&bytes[num_start..*pos]).ok()?;
            let idx = num_str.parse::<usize>().ok()?.saturating_sub(1);
            return Some(Expression::Placeholder(idx));
        }
        return Some(Expression::Placeholder(0));
    }

    if b == b'$' {
        *pos += 1;
        let num_start = *pos;
        while *pos < bytes.len() && bytes[*pos].is_ascii_digit() {
            *pos += 1;
        }
        if *pos > num_start {
            let num_str = std::str::from_utf8(&bytes[num_start..*pos]).ok()?;
            let idx = num_str.parse::<usize>().ok()?.saturating_sub(1);
            return Some(Expression::Placeholder(idx));
        }
        // 命名参数暂不支持，回退
        return None;
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_simple_insert() {
        let sql = "INSERT INTO users VALUES (1, 'alice', 25)";
        let result = try_parse_insert(sql);
        assert!(result.is_some());
        if let Some(Statement::Insert(stmt)) = result {
            assert_eq!(stmt.table_name, "users");
            assert!(stmt.columns.is_none());
            assert_eq!(stmt.values.len(), 1);
            assert_eq!(stmt.values[0].len(), 3);
        } else {
            panic!("Expected Insert");
        }
    }

    #[test]
    fn test_insert_with_columns() {
        let sql = "INSERT INTO users (id, name, age) VALUES (1, 'bob', 30)";
        let result = try_parse_insert(sql);
        assert!(result.is_some());
        if let Some(Statement::Insert(stmt)) = result {
            assert_eq!(stmt.table_name, "users");
            assert_eq!(stmt.columns.unwrap(), vec!["id", "name", "age"]);
            assert_eq!(stmt.values.len(), 1);
        } else {
            panic!("Expected Insert");
        }
    }

    #[test]
    fn test_insert_multiple_rows() {
        let sql = "INSERT INTO t VALUES (1), (2), (3)";
        let result = try_parse_insert(sql);
        assert!(result.is_some());
        if let Some(Statement::Insert(stmt)) = result {
            assert_eq!(stmt.values.len(), 3);
        } else {
            panic!("Expected Insert");
        }
    }

    #[test]
    fn test_insert_with_placeholders() {
        let sql = "INSERT INTO t VALUES (?, ?)";
        let result = try_parse_insert(sql);
        assert!(result.is_some());
        if let Some(Statement::Insert(stmt)) = result {
            assert_eq!(stmt.values.len(), 1);
            assert!(matches!(stmt.values[0][0], Expression::Placeholder(0)));
            assert!(matches!(stmt.values[0][1], Expression::Placeholder(0)));
        } else {
            panic!("Expected Insert");
        }
    }

    #[test]
    fn test_insert_with_numbered_placeholders() {
        let sql = "INSERT INTO t VALUES ($1, $2)";
        let result = try_parse_insert(sql);
        assert!(result.is_some());
        if let Some(Statement::Insert(stmt)) = result {
            assert!(matches!(stmt.values[0][0], Expression::Placeholder(0)));
            assert!(matches!(stmt.values[0][1], Expression::Placeholder(1)));
        } else {
            panic!("Expected Insert");
        }
    }

    #[test]
    fn test_non_insert_returns_none() {
        assert!(try_parse_insert("SELECT * FROM users").is_none());
        assert!(try_parse_insert("CREATE TABLE t (id INT)").is_none());
    }

    #[test]
    fn test_null_boolean_values() {
        let sql = "INSERT INTO t VALUES (NULL, TRUE, FALSE)";
        let result = try_parse_insert(sql);
        assert!(result.is_some());
        if let Some(Statement::Insert(stmt)) = result {
            assert!(matches!(stmt.values[0][0], Expression::Literal(Value::Null)));
            assert!(matches!(stmt.values[0][1], Expression::Literal(Value::Boolean(true))));
            assert!(matches!(stmt.values[0][2], Expression::Literal(Value::Boolean(false))));
        } else {
            panic!("Expected Insert");
        }
    }

    #[test]
    fn test_float_values() {
        let sql = "INSERT INTO t VALUES (3.14, -2.5e10)";
        let result = try_parse_insert(sql);
        assert!(result.is_some());
        if let Some(Statement::Insert(stmt)) = result {
            assert!(matches!(stmt.values[0][0], Expression::Literal(Value::Float64(_))));
            assert!(matches!(stmt.values[0][1], Expression::Literal(Value::Float64(_))));
        } else {
            panic!("Expected Insert");
        }
    }
}
