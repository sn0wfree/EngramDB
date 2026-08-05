//! 向量化表达式求值
//!
//! 核心设计：输入 DataChunk，输出 Vector（整批计算，而非逐行）
//! 支持所有 AST 表达式类型，与优化器产生的计划完全兼容。
//!
//! 性能要点：
//! - 整列批量计算，减少函数调用开销
//! - Constant 向量短路优化（全常量直接返回 Constant）
//! - 空值传播（NULL 参与运算结果为 NULL）
//! - 布尔短路（AND/OR 提前终止）

use crate::common::error::{EngramDbError, Result};
use crate::Value;
use rand::Rng;
use crate::sql::ast::{Expression, BinaryOperator, UnaryOperator, DataType};

use super::vector::{DataChunk, Vector};

/// 向量化求值表达式
///
/// 对整个 DataChunk 批量计算，返回结果 Vector。
/// column_names 用于解析 ColumnRef。
pub fn eval_vectorized(
    expr: &Expression,
    chunk: &DataChunk,
    column_names: &[String],
) -> Result<Vector> {
    match expr {
        Expression::Literal(v) => {
            // 常量：直接返回 Constant 向量
            Ok(Vector::Constant(v.clone(), chunk.count))
        }

        Expression::ColumnRef { column, .. } => {
            let idx = column_names.iter().position(|c| c == column)
                .ok_or_else(|| EngramDbError::ColumnNotFound(column.clone()))?;
            if idx >= chunk.columns.len() {
                // 列不存在，返回全 NULL
                Ok(Vector::Constant(Value::Null, chunk.count))
            } else {
                Ok(chunk.columns[idx].clone())
            }
        }

        Expression::BinaryOp { left, op, right } => {
            let left_vec = eval_vectorized(left, chunk, column_names)?;
            let right_vec = eval_vectorized(right, chunk, column_names)?;
            eval_binary_vectorized(&left_vec, *op, &right_vec)
        }

        Expression::UnaryOp { op, expr } => {
            let vec = eval_vectorized(expr, chunk, column_names)?;
            eval_unary_vectorized(&vec, *op)
        }

        Expression::IsNull(expr) => {
            let vec = eval_vectorized(expr, chunk, column_names)?;
            Ok(eval_is_null(&vec, false))
        }

        Expression::IsNotNull(expr) => {
            let vec = eval_vectorized(expr, chunk, column_names)?;
            Ok(eval_is_null(&vec, true))
        }

        Expression::Cast { expr, data_type } => {
            let vec = eval_vectorized(expr, chunk, column_names)?;
            eval_cast(&vec, data_type)
        }

        Expression::InList { expr, list } => {
            let vec = eval_vectorized(expr, chunk, column_names)?;
            // 求值列表中的所有表达式（都应该是常量或列引用）
            let list_vecs: Result<Vec<Vector>> = list.iter()
                .map(|e| eval_vectorized(e, chunk, column_names))
                .collect();
            let list_vecs = list_vecs?;
            eval_in_list(&vec, &list_vecs)
        }

        Expression::Like { expr, pattern } => {
            let vec = eval_vectorized(expr, chunk, column_names)?;
            let pat_vec = eval_vectorized(pattern, chunk, column_names)?;
            eval_like(&vec, &pat_vec)
        }

        Expression::Case { when_then, else_expr } => {
            eval_case_vectorized(when_then, else_expr.as_deref(), chunk, column_names)
        }

        Expression::Function { name, args, .. } => {
            eval_function(name, args, chunk, column_names)
        }

        Expression::Placeholder(_) => {
            Err(EngramDbError::Internal(
                "Placeholder should be resolved before execution".into()
            ))
        }

        Expression::Subquery(_) | Expression::Exists { .. } | Expression::InSubquery { .. } => {
            Err(EngramDbError::Internal(
                "Subquery should be resolved before expression evaluation".into()
            ))
        }
    }
}

// ============================================================================
// 二元运算向量化
// ============================================================================

fn eval_binary_vectorized(left: &Vector, op: BinaryOperator, right: &Vector) -> Result<Vector> {
    use BinaryOperator::*;

    // 双 Constant 快速路径：直接计算单个值
    if let (Vector::Constant(l, n), Vector::Constant(r, _)) = (left, right) {
        let result = eval_binary_value(l, op, r)?;
        return Ok(Vector::Constant(result, *n));
    }

    // 展开为 flat 后逐元素计算（TODO: 可进一步按类型特化提升性能）
    let left_flat = left.to_flat();
    let right_flat = right.to_flat();
    let len = left_flat.len().min(right_flat.len());

    let mut result = Vec::with_capacity(len);

    match op {
        // 算术运算
        Plus | Minus | Multiply | Divide | Modulo => {
            for i in 0..len {
                result.push(eval_arith(&left_flat[i], op, &right_flat[i]));
            }
        }
        // 比较运算 → 布尔结果
        Eq | NotEq | Lt | LtEq | Gt | GtEq => {
            for i in 0..len {
                result.push(eval_compare(&left_flat[i], op, &right_flat[i]));
            }
        }
        // 逻辑运算 → 布尔结果，带短路
        And => {
            for i in 0..len {
                result.push(eval_logic_and(&left_flat[i], &right_flat[i]));
            }
        }
        Or => {
            for i in 0..len {
                result.push(eval_logic_or(&left_flat[i], &right_flat[i]));
            }
        }
        // 字符串拼接
        Concat => {
            for i in 0..len {
                result.push(eval_concat(&left_flat[i], &right_flat[i]));
            }
        }
    }

    Ok(Vector::Flat(result))
}

fn eval_binary_value(left: &Value, op: BinaryOperator, right: &Value) -> Result<Value> {
    use BinaryOperator::*;
    match op {
        Plus | Minus | Multiply | Divide | Modulo => Ok(eval_arith(left, op, right)),
        Eq | NotEq | Lt | LtEq | Gt | GtEq => Ok(eval_compare(left, op, right)),
        And => Ok(eval_logic_and(left, right)),
        Or => Ok(eval_logic_or(left, right)),
        Concat => Ok(eval_concat(left, right)),
    }
}

// --- 算术运算 ---

fn eval_arith(left: &Value, op: BinaryOperator, right: &Value) -> Value {
    use BinaryOperator::*;

    // NULL 传播
    if left.is_null() || right.is_null() {
        return Value::Null;
    }

    // 整数运算
    if let (Some(l), Some(r)) = (left.as_i64(), right.as_i64()) {
        let result = match op {
            Plus => l.checked_add(r),
            Minus => l.checked_sub(r),
            Multiply => l.checked_mul(r),
            Divide => if r == 0 { None } else { Some(l / r) },
            Modulo => if r == 0 { None } else { Some(l % r) },
            _ => unreachable!(),
        };
        return match result {
            Some(v) => Value::Int64(v),
            None => Value::Null, // 溢出或除零 → NULL
        };
    }

    // 浮点运算
    if let (Some(l), Some(r)) = (left.as_f64(), right.as_f64()) {
        let result = match op {
            Plus => l + r,
            Minus => l - r,
            Multiply => l * r,
            Divide => if r == 0.0 { f64::NAN } else { l / r },
            Modulo => if r == 0.0 { f64::NAN } else { l % r },
            _ => unreachable!(),
        };
        return Value::Float64(result);
    }

    Value::Null
}

// --- 比较运算 ---

fn eval_compare(left: &Value, op: BinaryOperator, right: &Value) -> Value {
    use BinaryOperator::*;

    // NULL 比较：任何与 NULL 的比较结果都是 NULL（SQL 三值逻辑）
    if left.is_null() || right.is_null() {
        return Value::Null;
    }

    let cmp = value_cmp(left, right);
    let result = match op {
        Eq => cmp == std::cmp::Ordering::Equal,
        NotEq => cmp != std::cmp::Ordering::Equal,
        Lt => cmp == std::cmp::Ordering::Less,
        LtEq => cmp <= std::cmp::Ordering::Equal,
        Gt => cmp == std::cmp::Ordering::Greater,
        GtEq => cmp >= std::cmp::Ordering::Equal,
        _ => unreachable!(),
    };

    Value::Boolean(result)
}

fn value_cmp(a: &Value, b: &Value) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    match (a, b) {
        (Value::Null, Value::Null) => Equal,
        (Value::Null, _) => Less,
        (_, Value::Null) => Greater,
        (Value::Boolean(x), Value::Boolean(y)) => x.cmp(y),
        (Value::Int32(x), Value::Int32(y)) => x.cmp(y),
        (Value::Int64(x), Value::Int64(y)) => x.cmp(y),
        (Value::Int32(x), Value::Int64(y)) => (*x as i64).cmp(y),
        (Value::Int64(x), Value::Int32(y)) => x.cmp(&(*y as i64)),
        (Value::Float64(x), Value::Float64(y)) => x.partial_cmp(y).unwrap_or(Equal),
        (Value::Varchar(x), Value::Varchar(y)) => x.cmp(y),
        // 跨类型比较：尝试数值比较
        _ => {
            if let (Some(x), Some(y)) = (a.as_f64(), b.as_f64()) {
                x.partial_cmp(&y).unwrap_or(Equal)
            } else {
                // 类型不兼容，按类型 discriminant 排序
                fn value_tag(v: &Value) -> u8 {
                    match v {
                        Value::Null => 0,
                        Value::Boolean(_) => 1,
                        Value::Int32(_) => 2,
                        Value::Int64(_) => 3,
                        Value::Float32(_) => 4,
                        Value::Float64(_) => 5,
                        Value::Varchar(_) => 6,
                        Value::Json(_) => 7,
                        Value::Vector(_) => 8,
                        Value::Blob(_) => 9,
                        Value::Timestamp(_) => 10,
                        Value::VectorInt8(_) => 11,
                    }
                }
                value_tag(a).cmp(&value_tag(b))
            }
        }
    }
}

// --- 逻辑运算（三值逻辑）---

fn eval_logic_and(left: &Value, right: &Value) -> Value {
    match (left, right) {
        (Value::Boolean(false), _) | (_, Value::Boolean(false)) => Value::Boolean(false),
        (Value::Boolean(true), Value::Boolean(true)) => Value::Boolean(true),
        // NULL AND true = NULL, true AND NULL = NULL
        (Value::Null, Value::Boolean(true)) | (Value::Boolean(true), Value::Null) => Value::Null,
        (Value::Null, Value::Null) => Value::Null,
        _ => Value::Null,
    }
}

fn eval_logic_or(left: &Value, right: &Value) -> Value {
    match (left, right) {
        (Value::Boolean(true), _) | (_, Value::Boolean(true)) => Value::Boolean(true),
        (Value::Boolean(false), Value::Boolean(false)) => Value::Boolean(false),
        // NULL OR false = NULL, false OR NULL = NULL
        (Value::Null, Value::Boolean(false)) | (Value::Boolean(false), Value::Null) => Value::Null,
        (Value::Null, Value::Null) => Value::Null,
        _ => Value::Null,
    }
}

// --- 字符串拼接 ---

fn eval_concat(left: &Value, right: &Value) -> Value {
    if left.is_null() || right.is_null() {
        return Value::Null;
    }
    let l = left.as_str().unwrap_or(&format!("{}", left)).to_string();
    let r = right.as_str().unwrap_or(&format!("{}", right)).to_string();
    Value::Varchar(format!("{}{}", l, r))
}

// ============================================================================
// 一元运算向量化
// ============================================================================

fn eval_unary_vectorized(vec: &Vector, op: UnaryOperator) -> Result<Vector> {
    match vec {
        Vector::Constant(v, n) => {
            let result = eval_unary_value(v, op);
            Ok(Vector::Constant(result, *n))
        }
        Vector::Flat(values) => {
            let result: Vec<Value> = values.iter()
                .map(|v| eval_unary_value(v, op))
                .collect();
            Ok(Vector::Flat(result))
        }
    }
}

fn eval_unary_value(v: &Value, op: UnaryOperator) -> Value {
    match op {
        UnaryOperator::Not => {
            match v {
                Value::Boolean(b) => Value::Boolean(!b),
                Value::Null => Value::Null,
                _ => Value::Null,
            }
        }
        UnaryOperator::Negate => {
            if v.is_null() {
                return Value::Null;
            }
            if let Some(i) = v.as_i64() {
                Value::Int64(-i)
            } else if let Some(f) = v.as_f64() {
                Value::Float64(-f)
            } else {
                Value::Null
            }
        }
    }
}

// ============================================================================
// IS NULL / IS NOT NULL
// ============================================================================

fn eval_is_null(vec: &Vector, negate: bool) -> Vector {
    match vec {
        Vector::Constant(v, n) => {
            let is_null = v.is_null();
            let result = if negate { !is_null } else { is_null };
            Vector::Constant(Value::Boolean(result), *n)
        }
        Vector::Flat(values) => {
            let result: Vec<Value> = values.iter()
                .map(|v| {
                    let is_null = v.is_null();
                    Value::Boolean(if negate { !is_null } else { is_null })
                })
                .collect();
            Vector::Flat(result)
        }
    }
}

// ============================================================================
// CAST 类型转换
// ============================================================================

fn eval_cast(vec: &Vector, target_type: &DataType) -> Result<Vector> {
    match vec {
        Vector::Constant(v, n) => {
            let result = cast_value(v, target_type)?;
            Ok(Vector::Constant(result, *n))
        }
        Vector::Flat(values) => {
            let mut result = Vec::with_capacity(values.len());
            for v in values {
                result.push(cast_value(v, target_type)?);
            }
            Ok(Vector::Flat(result))
        }
    }
}

fn cast_value(v: &Value, target: &DataType) -> Result<Value> {
    use crate::common::types::DataType::*;

    if v.is_null() {
        return Ok(Value::Null);
    }

    Ok(match target {
        Boolean => {
            match v {
                Value::Boolean(_) => v.clone(),
                Value::Int32(n) => Value::Boolean(*n != 0),
                Value::Int64(n) => Value::Boolean(*n != 0),
                Value::Float64(f) => Value::Boolean(*f != 0.0),
                Value::Varchar(s) => Value::Boolean(s.to_lowercase() == "true"),
                _ => Value::Null,
            }
        }
        Int32 => {
            if let Some(i) = v.as_i64() {
                Value::Int32(i as i32)
            } else if let Some(f) = v.as_f64() {
                Value::Int32(f as i32)
            } else if let Value::Varchar(s) = v {
                s.parse::<i32>().map(Value::Int32).unwrap_or(Value::Null)
            } else {
                Value::Null
            }
        }
        Int64 => {
            if let Some(i) = v.as_i64() {
                Value::Int64(i)
            } else if let Some(f) = v.as_f64() {
                Value::Int64(f as i64)
            } else if let Value::Varchar(s) = v {
                s.parse::<i64>().map(Value::Int64).unwrap_or(Value::Null)
            } else {
                Value::Null
            }
        }
        Float64 => {
            if let Some(f) = v.as_f64() {
                Value::Float64(f)
            } else if let Value::Varchar(s) = v {
                s.parse::<f64>().map(Value::Float64).unwrap_or(Value::Null)
            } else {
                Value::Null
            }
        }
        Varchar => {
            Value::Varchar(format!("{}", v))
        }
        _ => Value::Null,
    })
}

// ============================================================================
// IN 表达式
// ============================================================================

fn eval_in_list(expr_vec: &Vector, list_vecs: &[Vector]) -> Result<Vector> {
    if list_vecs.is_empty() {
        return Ok(Vector::Constant(Value::Boolean(false), expr_vec.len()));
    }

    // 提取列表中的常量值（优化：列表全是常量时用 HashSet）
    let list_values: Vec<Value> = list_vecs.iter()
        .filter_map(|v| match v {
            Vector::Constant(val, _) => Some(val.clone()),
            _ => None,
        })
        .collect();

    let all_constant = list_values.len() == list_vecs.len();

    match expr_vec {
        Vector::Constant(val, n) => {
            // 表达式是常量
            if all_constant {
                let found = list_values.iter().any(|lv| value_eq(val, lv));
                return Ok(Vector::Constant(Value::Boolean(found), *n));
            }
            // 列表不全是常量，退化到逐行
            let len = list_vecs[0].len();
            let mut result = Vec::with_capacity(len);
            for i in 0..len {
                let mut found = false;
                for lv in list_vecs {
                    if value_eq(val, lv.get(i)) {
                        found = true;
                        break;
                    }
                }
                result.push(Value::Boolean(found));
            }
            Ok(Vector::Flat(result))
        }
        Vector::Flat(values) => {
            if all_constant {
                // 列表全是常量，用 HashSet 加速
                use std::collections::HashSet;
                let set: HashSet<&Value> = list_values.iter().collect();
                let result: Vec<Value> = values.iter()
                    .map(|v| Value::Boolean(set.contains(v)))
                    .collect();
                Ok(Vector::Flat(result))
            } else {
                // 通用情况
                let mut result = Vec::with_capacity(values.len());
                for (i, val) in values.iter().enumerate() {
                    let mut found = false;
                    for lv in list_vecs {
                        if value_eq(val, lv.get(i)) {
                            found = true;
                            break;
                        }
                    }
                    result.push(Value::Boolean(found));
                }
                Ok(Vector::Flat(result))
            }
        }
    }
}

fn value_eq(a: &Value, b: &Value) -> bool {
    // NULL = NULL 为 true（IN 语义）
    if a.is_null() && b.is_null() {
        return true;
    }
    if a.is_null() || b.is_null() {
        return false;
    }
    value_cmp(a, b) == std::cmp::Ordering::Equal
}

// ============================================================================
// LIKE 表达式
// ============================================================================

fn eval_like(expr_vec: &Vector, pattern_vec: &Vector) -> Result<Vector> {
    match (expr_vec, pattern_vec) {
        (Vector::Constant(e, n), Vector::Constant(p, _)) => {
            let result = like_match(e, p);
            Ok(Vector::Constant(result, *n))
        }
        _ => {
            let e_flat = expr_vec.to_flat();
            let p_flat = pattern_vec.to_flat();
            let len = e_flat.len().min(p_flat.len());
            let mut result = Vec::with_capacity(len);
            for i in 0..len {
                result.push(like_match(&e_flat[i], &p_flat[i]));
            }
            Ok(Vector::Flat(result))
        }
    }
}

fn like_match(value: &Value, pattern: &Value) -> Value {
    if value.is_null() || pattern.is_null() {
        return Value::Null;
    }

    let s = match value.as_str() {
        Some(s) => s,
        None => return Value::Boolean(false),
    };
    let p = match pattern.as_str() {
        Some(p) => p,
        None => return Value::Boolean(false),
    };

    Value::Boolean(like_pattern_match(s, p))
}

/// LIKE 模式匹配：% 匹配任意序列，_ 匹配单个字符
fn like_pattern_match(s: &str, pattern: &str) -> bool {
    let s_chars: Vec<char> = s.chars().collect();
    let p_chars: Vec<char> = pattern.chars().collect();

    // 动态规划：dp[i][j] = s[0..i] 是否匹配 p[0..j]
    let (m, n) = (s_chars.len(), p_chars.len());
    let mut dp = vec![vec![false; n + 1]; m + 1];
    dp[0][0] = true;

    // 前导 % 可以匹配空串
    for j in 1..=n {
        if p_chars[j - 1] == '%' {
            dp[0][j] = dp[0][j - 1];
        } else {
            break;
        }
    }

    for i in 1..=m {
        for j in 1..=n {
            match p_chars[j - 1] {
                '%' => {
                    // % 匹配 0 个（dp[i][j-1]）或多个（dp[i-1][j]）
                    dp[i][j] = dp[i][j - 1] || dp[i - 1][j];
                }
                '_' => {
                    // _ 匹配任意单个字符
                    dp[i][j] = dp[i - 1][j - 1];
                }
                c => {
                    // 普通字符必须相等
                    dp[i][j] = s_chars[i - 1] == c && dp[i - 1][j - 1];
                }
            }
        }
    }

    dp[m][n]
}

// ============================================================================
// CASE 表达式
// ============================================================================

fn eval_case_vectorized(
    when_then: &[(Expression, Expression)],
    else_expr: Option<&Expression>,
    chunk: &DataChunk,
    column_names: &[String],
) -> Result<Vector> {
    let count = chunk.count;

    // 预计算所有 WHEN 条件和 THEN 结果
    let mut when_vecs = Vec::with_capacity(when_then.len());
    let mut then_vecs = Vec::with_capacity(when_then.len());

    for (when_expr, then_expr) in when_then {
        let when_vec = eval_vectorized(when_expr, chunk, column_names)?;
        let then_vec = eval_vectorized(then_expr, chunk, column_names)?;
        when_vecs.push(when_vec);
        then_vecs.push(then_vec);
    }

    let else_vec = match else_expr {
        Some(e) => eval_vectorized(e, chunk, column_names)?,
        None => Vector::Constant(Value::Null, count),
    };

    // 逐行选择第一个匹配的 THEN 结果
    let mut result = Vec::with_capacity(count);
    for i in 0..count {
        let mut matched = false;
        for (w, t) in when_vecs.iter().zip(then_vecs.iter()) {
            if let Value::Boolean(true) = w.get(i) {
                result.push(t.get(i).clone());
                matched = true;
                break;
            }
        }
        if !matched {
            result.push(else_vec.get(i).clone());
        }
    }

    Ok(Vector::Flat(result))
}

// ============================================================================
// 内置函数
// ============================================================================

fn eval_function(
    name: &str,
    args: &[Expression],
    chunk: &DataChunk,
    column_names: &[String],
) -> Result<Vector> {
    let func_name = name.to_uppercase();

    match func_name.as_str() {
        "ABS" => {
            if args.len() != 1 {
                return Err(EngramDbError::Parse("ABS requires 1 argument".into()));
            }
            let vec = eval_vectorized(&args[0], chunk, column_names)?;
            eval_abs(&vec)
        }
        "LENGTH" | "LEN" => {
            if args.len() != 1 {
                return Err(EngramDbError::Parse("LENGTH requires 1 argument".into()));
            }
            let vec = eval_vectorized(&args[0], chunk, column_names)?;
            eval_length(&vec)
        }
        "UPPER" | "UCASE" => {
            if args.len() != 1 {
                return Err(EngramDbError::Parse("UPPER requires 1 argument".into()));
            }
            let vec = eval_vectorized(&args[0], chunk, column_names)?;
            eval_upper(&vec)
        }
        "LOWER" | "LCASE" => {
            if args.len() != 1 {
                return Err(EngramDbError::Parse("LOWER requires 1 argument".into()));
            }
            let vec = eval_vectorized(&args[0], chunk, column_names)?;
            eval_lower(&vec)
        }
        "ROUND" => {
            if args.is_empty() || args.len() > 2 {
                return Err(EngramDbError::Parse("ROUND requires 1-2 arguments".into()));
            }
            let vec = eval_vectorized(&args[0], chunk, column_names)?;
            let decimals = if args.len() == 2 {
                if let Expression::Literal(Value::Int32(n)) = &args[1] {
                    *n
                } else if let Expression::Literal(Value::Int64(n)) = &args[1] {
                    *n as i32
                } else {
                    0
                }
            } else {
                0
            };
            eval_round(&vec, decimals)
        }
        "COALESCE" => {
            if args.is_empty() {
                return Err(EngramDbError::Parse("COALESCE requires at least 1 argument".into()));
            }
            let arg_vecs: Result<Vec<Vector>> = args.iter()
                .map(|a| eval_vectorized(a, chunk, column_names))
                .collect();
            let arg_vecs = arg_vecs?;
            eval_coalesce(&arg_vecs)
        }
        "SUBSTRING" | "SUBSTR" => {
            if args.len() < 2 || args.len() > 3 {
                return Err(EngramDbError::Parse("SUBSTRING requires 2-3 arguments".into()));
            }
            let str_vec = eval_vectorized(&args[0], chunk, column_names)?;
            let start_vec = eval_vectorized(&args[1], chunk, column_names)?;
            let len_vec = if args.len() == 3 {
                Some(eval_vectorized(&args[2], chunk, column_names)?)
            } else {
                None
            };
            eval_substring(&str_vec, &start_vec, len_vec.as_ref())
        }
        "CONCAT" => {
            if args.len() < 2 {
                return Err(EngramDbError::Parse("CONCAT requires at least 2 arguments".into()));
            }
            let arg_vecs: Result<Vec<Vector>> = args.iter()
                .map(|a| eval_vectorized(a, chunk, column_names))
                .collect();
            let arg_vecs = arg_vecs?;
            eval_concat_func(&arg_vecs)
        }
        "IFNULL" => {
            if args.len() != 2 {
                return Err(EngramDbError::Parse("IFNULL requires 2 arguments".into()));
            }
            let arg_vecs: Result<Vec<Vector>> = args.iter()
                .map(|a| eval_vectorized(a, chunk, column_names))
                .collect();
            let arg_vecs = arg_vecs?;
            eval_coalesce(&arg_vecs)
        }
        "NULLIF" => {
            if args.len() != 2 {
                return Err(EngramDbError::Parse("NULLIF requires 2 arguments".into()));
            }
            let a_vec = eval_vectorized(&args[0], chunk, column_names)?;
            let b_vec = eval_vectorized(&args[1], chunk, column_names)?;
            eval_nullif(&a_vec, &b_vec)
        }
        "IF" => {
            // IF(cond, true_val, false_val)：条件表达式
            if args.len() != 3 {
                return Err(EngramDbError::Parse("IF requires 3 arguments".into()));
            }
            let cond_vec = eval_vectorized(&args[0], chunk, column_names)?;
            let true_vec = eval_vectorized(&args[1], chunk, column_names)?;
            let false_vec = eval_vectorized(&args[2], chunk, column_names)?;
            eval_if(&cond_vec, &true_vec, &false_vec)
        }
        "TRIM" => {
            // TRIM(str [, chars]): 去除两端指定字符（默认空白）
            if args.is_empty() || args.len() > 2 {
                return Err(EngramDbError::Parse("TRIM requires 1 or 2 arguments".into()));
            }
            let str_vec = eval_vectorized(&args[0], chunk, column_names)?;
            let chars_vec = if args.len() == 2 {
                Some(eval_vectorized(&args[1], chunk, column_names)?)
            } else {
                None
            };
            eval_trim(&str_vec, chars_vec.as_ref(), TrimMode::Both)
        }
        "LTRIM" => {
            if args.is_empty() || args.len() > 2 {
                return Err(EngramDbError::Parse("LTRIM requires 1 or 2 arguments".into()));
            }
            let str_vec = eval_vectorized(&args[0], chunk, column_names)?;
            let chars_vec = if args.len() == 2 {
                Some(eval_vectorized(&args[1], chunk, column_names)?)
            } else {
                None
            };
            eval_trim(&str_vec, chars_vec.as_ref(), TrimMode::Left)
        }
        "RTRIM" => {
            if args.is_empty() || args.len() > 2 {
                return Err(EngramDbError::Parse("RTRIM requires 1 or 2 arguments".into()));
            }
            let str_vec = eval_vectorized(&args[0], chunk, column_names)?;
            let chars_vec = if args.len() == 2 {
                Some(eval_vectorized(&args[1], chunk, column_names)?)
            } else {
                None
            };
            eval_trim(&str_vec, chars_vec.as_ref(), TrimMode::Right)
        }
        "INSTR" | "POSITION" => {
            // INSTR(haystack, needle): 返回 needle 在 haystack 中第一次出现的位置（1-based），未找到返回 0
            if args.len() != 2 {
                return Err(EngramDbError::Parse("INSTR requires 2 arguments".into()));
            }
            let haystack_vec = eval_vectorized(&args[0], chunk, column_names)?;
            let needle_vec = eval_vectorized(&args[1], chunk, column_names)?;
            eval_instr(&haystack_vec, &needle_vec)
        }
        "SPLIT_PART" => {
            // SPLIT_PART(str, delimiter, part): 按 delimiter 分割字符串，返回第 part 段（1-based）
            if args.len() != 3 {
                return Err(EngramDbError::Parse("SPLIT_PART requires 3 arguments".into()));
            }
            let str_vec = eval_vectorized(&args[0], chunk, column_names)?;
            let delim_vec = eval_vectorized(&args[1], chunk, column_names)?;
            let part_vec = eval_vectorized(&args[2], chunk, column_names)?;
            eval_split_part(&str_vec, &delim_vec, &part_vec)
        }
        "CEIL" | "CEILING" => {
            if args.len() != 1 {
                return Err(EngramDbError::Parse("CEIL requires 1 argument".into()));
            }
            let vec = eval_vectorized(&args[0], chunk, column_names)?;
            eval_unary_numeric(&vec, |x| x.ceil())
        }
        "FLOOR" => {
            if args.len() != 1 {
                return Err(EngramDbError::Parse("FLOOR requires 1 argument".into()));
            }
            let vec = eval_vectorized(&args[0], chunk, column_names)?;
            eval_unary_numeric(&vec, |x| x.floor())
        }
        "TRUNC" | "TRUNCATE" => {
            // TRUNC(x): 向 0 取整（截断小数部分）
            if args.len() != 1 {
                return Err(EngramDbError::Parse("TRUNC requires 1 argument".into()));
            }
            let vec = eval_vectorized(&args[0], chunk, column_names)?;
            eval_unary_numeric(&vec, |x| x.trunc())
        }
        "POWER" | "POW" => {
            if args.len() != 2 {
                return Err(EngramDbError::Parse("POWER requires 2 arguments".into()));
            }
            let base_vec = eval_vectorized(&args[0], chunk, column_names)?;
            let exp_vec = eval_vectorized(&args[1], chunk, column_names)?;
            eval_binary_numeric(&base_vec, &exp_vec, |a, b| a.powf(b))
        }
        "SQRT" => {
            if args.len() != 1 {
                return Err(EngramDbError::Parse("SQRT requires 1 argument".into()));
            }
            let vec = eval_vectorized(&args[0], chunk, column_names)?;
            eval_unary_numeric(&vec, |x| x.sqrt())
        }
        "LN" => {
            if args.len() != 1 {
                return Err(EngramDbError::Parse("LN requires 1 argument".into()));
            }
            let vec = eval_vectorized(&args[0], chunk, column_names)?;
            eval_unary_numeric(&vec, |x| x.ln())
        }
        "LOG" | "LOG10" => {
            if args.len() != 1 {
                return Err(EngramDbError::Parse("LOG/LOG10 requires 1 argument".into()));
            }
            let vec = eval_vectorized(&args[0], chunk, column_names)?;
            eval_unary_numeric(&vec, |x| x.log10())
        }
        "LOG2" => {
            if args.len() != 1 {
                return Err(EngramDbError::Parse("LOG2 requires 1 argument".into()));
            }
            let vec = eval_vectorized(&args[0], chunk, column_names)?;
            eval_unary_numeric(&vec, |x| x.log2())
        }
        "RANDOM" | "RAND" => {
            if !args.is_empty() {
                return Err(EngramDbError::Parse("RANDOM/RAND takes 0 arguments".into()));
            }
            let mut rng = rand::thread_rng();
            Ok(Vector::Constant(Value::Float64(rng.gen::<f64>()), chunk.count))
        }
        "TYPEOF" => {
            if args.len() != 1 {
                return Err(EngramDbError::Parse("TYPEOF requires 1 argument".into()));
            }
            let vec = eval_vectorized(&args[0], chunk, column_names)?;
            eval_typeof(&vec)
        }
        "REPLACE" => {
            if args.len() != 3 {
                return Err(EngramDbError::Parse("REPLACE requires 3 arguments".into()));
            }
            let str_vec = eval_vectorized(&args[0], chunk, column_names)?;
            let from_vec = eval_vectorized(&args[1], chunk, column_names)?;
            let to_vec = eval_vectorized(&args[2], chunk, column_names)?;
            eval_replace(&str_vec, &from_vec, &to_vec)
        }
        "MOD" => {
            if args.len() != 2 {
                return Err(EngramDbError::Parse("MOD requires 2 arguments".into()));
            }
            let a_vec = eval_vectorized(&args[0], chunk, column_names)?;
            let b_vec = eval_vectorized(&args[1], chunk, column_names)?;
            eval_mod(&a_vec, &b_vec)
        }
        // JSON 函数（v0.12.0 新增，Agent 元数据场景）
        "JSON_EXTRACT" => {
            if args.len() != 2 {
                return Err(EngramDbError::Parse("JSON_EXTRACT requires 2 arguments".into()));
            }
            let json_vec = eval_vectorized(&args[0], chunk, column_names)?;
            let path_vec = eval_vectorized(&args[1], chunk, column_names)?;
            eval_json_extract(&json_vec, &path_vec)
        }
        "JSON_CONTAINS" => {
            if args.len() < 2 || args.len() > 3 {
                return Err(EngramDbError::Parse("JSON_CONTAINS requires 2-3 arguments".into()));
            }
            let json_vec = eval_vectorized(&args[0], chunk, column_names)?;
            let target_vec = eval_vectorized(&args[1], chunk, column_names)?;
            let path_vec = if args.len() == 3 {
                Some(eval_vectorized(&args[2], chunk, column_names)?)
            } else {
                None
            };
            eval_json_contains(&json_vec, &target_vec, path_vec.as_ref())
        }
        "JSON_ARRAY_LENGTH" => {
            if args.len() < 1 || args.len() > 2 {
                return Err(EngramDbError::Parse("JSON_ARRAY_LENGTH requires 1-2 arguments".into()));
            }
            let json_vec = eval_vectorized(&args[0], chunk, column_names)?;
            let path_vec = if args.len() == 2 {
                Some(eval_vectorized(&args[1], chunk, column_names)?)
            } else {
                None
            };
            eval_json_array_length(&json_vec, path_vec.as_ref())
        }
        // JSON_OBJECT(k1, v1, k2, v2, ...) — 构造 JSON 对象（v0.15.0 新增）
        "JSON_OBJECT" => {
            if args.len() % 2 != 0 {
                return Err(EngramDbError::Parse("JSON_OBJECT requires even number of arguments (key, value pairs)".into()));
            }
            // 每行都需要构造一个对象，但这里所有行共享同一个 JSON_OBJECT（无列引用作为参数）
            // 简化为对所有行应用同一个 JSON
            let key_vecs: Vec<Vector> = args.iter().step_by(2)
                .map(|a| eval_vectorized(a, chunk, column_names))
                .collect::<Result<Vec<_>>>()?;
            let val_vecs: Vec<Vector> = args.iter().skip(1).step_by(2)
                .map(|a| eval_vectorized(a, chunk, column_names))
                .collect::<Result<Vec<_>>>()?;
            eval_json_object(&key_vecs, &val_vecs)
        }
        // JSON_ARRAY(v1, v2, ...) — 构造 JSON 数组（v0.15.0 新增）
        "JSON_ARRAY" => {
            let val_vecs: Vec<Vector> = args.iter()
                .map(|a| eval_vectorized(a, chunk, column_names))
                .collect::<Result<Vec<_>>>()?;
            eval_json_array(&val_vecs)
        }
        // JSON_SET(json, path, value) — 设置/创建路径的值（v0.15.0 新增）
        "JSON_SET" => {
            if args.len() != 3 {
                return Err(EngramDbError::Parse("JSON_SET requires 3 arguments".into()));
            }
            let json_vec = eval_vectorized(&args[0], chunk, column_names)?;
            let path_vec = eval_vectorized(&args[1], chunk, column_names)?;
            let val_vec = eval_vectorized(&args[2], chunk, column_names)?;
            eval_json_set(&json_vec, &path_vec, &val_vec, false)
        }
        // JSON_INSERT(json, path, value) — 仅当路径不存在时设置（v0.15.0 新增）
        "JSON_INSERT" => {
            if args.len() != 3 {
                return Err(EngramDbError::Parse("JSON_INSERT requires 3 arguments".into()));
            }
            let json_vec = eval_vectorized(&args[0], chunk, column_names)?;
            let path_vec = eval_vectorized(&args[1], chunk, column_names)?;
            let val_vec = eval_vectorized(&args[2], chunk, column_names)?;
            eval_json_set(&json_vec, &path_vec, &val_vec, true)
        }
        // JSON_REPLACE(json, path, value) — 仅当路径存在时替换（v0.15.0 新增）
        "JSON_REPLACE" => {
            if args.len() != 3 {
                return Err(EngramDbError::Parse("JSON_REPLACE requires 3 arguments".into()));
            }
            let json_vec = eval_vectorized(&args[0], chunk, column_names)?;
            let path_vec = eval_vectorized(&args[1], chunk, column_names)?;
            let val_vec = eval_vectorized(&args[2], chunk, column_names)?;
            // JSON_REPLACE = JSON_SET 但只在路径存在时生效
            eval_json_replace(&json_vec, &path_vec, &val_vec)
        }
        // JSON_REMOVE(json, path) — 删除指定路径的字段（v0.15.0 新增）
        "JSON_REMOVE" => {
            if args.len() != 2 {
                return Err(EngramDbError::Parse("JSON_REMOVE requires 2 arguments".into()));
            }
            let json_vec = eval_vectorized(&args[0], chunk, column_names)?;
            let path_vec = eval_vectorized(&args[1], chunk, column_names)?;
            eval_json_remove(&json_vec, &path_vec)
        }
        // 向量函数（v0.12.0 新增，Agent 语义记忆 / RAG 场景）
        "VECTOR_DISTANCE" | "VEC_DISTANCE" => {
            if args.len() != 2 {
                return Err(EngramDbError::Parse("VECTOR_DISTANCE requires 2 arguments".into()));
            }
            let v1 = eval_vectorized(&args[0], chunk, column_names)?;
            let v2 = eval_vectorized(&args[1], chunk, column_names)?;
            eval_vector_distance(&v1, &v2)
        }
        "VECTOR_L2_DISTANCE" => {
            if args.len() != 2 {
                return Err(EngramDbError::Parse("VECTOR_L2_DISTANCE requires 2 arguments".into()));
            }
            let v1 = eval_vectorized(&args[0], chunk, column_names)?;
            let v2 = eval_vectorized(&args[1], chunk, column_names)?;
            eval_vector_distance(&v1, &v2)
        }
        "VECTOR_COSINE_SIMILARITY" => {
            if args.len() != 2 {
                return Err(EngramDbError::Parse("VECTOR_COSINE_SIMILARITY requires 2 arguments".into()));
            }
            let v1 = eval_vectorized(&args[0], chunk, column_names)?;
            let v2 = eval_vectorized(&args[1], chunk, column_names)?;
            eval_vector_cosine_similarity(&v1, &v2)
        }
        "VECTOR_NORM" => {
            if args.len() != 1 {
                return Err(EngramDbError::Parse("VECTOR_NORM requires 1 argument".into()));
            }
            let v = eval_vectorized(&args[0], chunk, column_names)?;
            eval_vector_norm(&v)
        }
        "NOW" | "CURRENT_TIMESTAMP" => {
            if !args.is_empty() {
                return Err(EngramDbError::Parse("NOW/CURRENT_TIMESTAMP takes 0 arguments".into()));
            }
            let now = now_ms();
            Ok(Vector::Constant(Value::Timestamp(now), chunk.count))
        }
        "DATE" => {
            if args.len() != 1 {
                return Err(EngramDbError::Parse("DATE requires 1 argument".into()));
            }
            let vec = eval_vectorized(&args[0], chunk, column_names)?;
            match &vec {
                Vector::Constant(v, n) => {
                    let val = date_value(v);
                    Ok(Vector::Constant(val, *n))
                }
                Vector::Flat(values) => {
                    let result: Vec<Value> = values.iter().map(date_value).collect();
                    Ok(Vector::Flat(result))
                }
            }
        }
        "STRFTIME" => {
            if args.len() != 2 {
                return Err(EngramDbError::Parse("STRFTIME requires 2 arguments (format, timestamp)".into()));
            }
            let fmt_vec = eval_vectorized(&args[0], chunk, column_names)?;
            let ts_vec = eval_vectorized(&args[1], chunk, column_names)?;
            let fmt = match &fmt_vec {
                Vector::Constant(Value::Varchar(s), _) => s.clone(),
                _ => return Err(EngramDbError::Parse("STRFTIME format must be a constant string".into())),
            };
            match &ts_vec {
                Vector::Constant(v, n) => {
                    let val = strftime_value(&fmt, v);
                    Ok(Vector::Constant(val, *n))
                }
                Vector::Flat(values) => {
                    let result: Vec<Value> = values.iter().map(|v| strftime_value(&fmt, v)).collect();
                    Ok(Vector::Flat(result))
                }
            }
        }
        "TIME" => {
            if args.len() != 1 {
                return Err(EngramDbError::Parse("TIME requires 1 argument".into()));
            }
            let vec = eval_vectorized(&args[0], chunk, column_names)?;
            match &vec {
                Vector::Constant(v, n) => {
                    let val = time_value(v);
                    Ok(Vector::Constant(val, *n))
                }
                Vector::Flat(values) => {
                    let result: Vec<Value> = values.iter().map(time_value).collect();
                    Ok(Vector::Flat(result))
                }
            }
        }
        "DATETIME" => {
            if args.len() != 1 {
                return Err(EngramDbError::Parse("DATETIME requires 1 argument".into()));
            }
            let vec = eval_vectorized(&args[0], chunk, column_names)?;
            match &vec {
                Vector::Constant(v, n) => {
                    let val = datetime_value(v);
                    Ok(Vector::Constant(val, *n))
                }
                Vector::Flat(values) => {
                    let result: Vec<Value> = values.iter().map(datetime_value).collect();
                    Ok(Vector::Flat(result))
                }
            }
        }
        "DATE_ADD" => {
            if args.len() != 3 {
                return Err(EngramDbError::Parse("DATE_ADD requires 3 arguments (timestamp, number, unit)".into()));
            }
            let ts_vec = eval_vectorized(&args[0], chunk, column_names)?;
            let n_vec = eval_vectorized(&args[1], chunk, column_names)?;
            let unit_vec = eval_vectorized(&args[2], chunk, column_names)?;
            let unit = match &unit_vec {
                Vector::Constant(Value::Varchar(s), _) => s.to_lowercase(),
                _ => return Err(EngramDbError::Parse("DATE_ADD unit must be a constant string".into())),
            };
            match (&ts_vec, &n_vec) {
                (Vector::Constant(ts, n), Vector::Constant(num, _)) => {
                    let val = date_add_value(ts, num, &unit);
                    Ok(Vector::Constant(val, *n))
                }
                (Vector::Flat(ts), Vector::Flat(num)) => {
                    let result: Vec<Value> = ts.iter().zip(num.iter()).map(|(t, n)| date_add_value(t, n, &unit)).collect();
                    Ok(Vector::Flat(result))
                }
                _ => Ok(Vector::Flat(vec![Value::Null; chunk.count])),
            }
        }
        "DATE_SUB" => {
            if args.len() != 3 {
                return Err(EngramDbError::Parse("DATE_SUB requires 3 arguments (timestamp, number, unit)".into()));
            }
            let ts_vec = eval_vectorized(&args[0], chunk, column_names)?;
            let n_vec = eval_vectorized(&args[1], chunk, column_names)?;
            let unit_vec = eval_vectorized(&args[2], chunk, column_names)?;
            let unit = match &unit_vec {
                Vector::Constant(Value::Varchar(s), _) => s.to_lowercase(),
                _ => return Err(EngramDbError::Parse("DATE_SUB unit must be a constant string".into())),
            };
            match (&ts_vec, &n_vec) {
                (Vector::Constant(ts, n), Vector::Constant(num, _)) => {
                    let val = date_sub_value(ts, num, &unit);
                    Ok(Vector::Constant(val, *n))
                }
                (Vector::Flat(ts), Vector::Flat(num)) => {
                    let result: Vec<Value> = ts.iter().zip(num.iter()).map(|(t, n)| date_sub_value(t, n, &unit)).collect();
                    Ok(Vector::Flat(result))
                }
                _ => Ok(Vector::Flat(vec![Value::Null; chunk.count])),
            }
        }
        "DATE_DIFF" => {
            if args.len() != 3 {
                return Err(EngramDbError::Parse("DATE_DIFF requires 3 arguments (timestamp1, timestamp2, unit)".into()));
            }
            let ts1_vec = eval_vectorized(&args[0], chunk, column_names)?;
            let ts2_vec = eval_vectorized(&args[1], chunk, column_names)?;
            let unit_vec = eval_vectorized(&args[2], chunk, column_names)?;
            let unit = match &unit_vec {
                Vector::Constant(Value::Varchar(s), _) => s.to_lowercase(),
                _ => return Err(EngramDbError::Parse("DATE_DIFF unit must be a constant string".into())),
            };
            match (&ts1_vec, &ts2_vec) {
                (Vector::Constant(ts1, n), Vector::Constant(ts2, _)) => {
                    let val = date_diff_value(ts1, ts2, &unit);
                    Ok(Vector::Constant(val, *n))
                }
                (Vector::Flat(ts1), Vector::Flat(ts2)) => {
                    let result: Vec<Value> = ts1.iter().zip(ts2.iter()).map(|(a, b)| date_diff_value(a, b, &unit)).collect();
                    Ok(Vector::Flat(result))
                }
                _ => Ok(Vector::Flat(vec![Value::Null; chunk.count])),
            }
        }
        "DATE_TRUNC" | "DATE_BIN" => {
            if args.len() != 2 {
                return Err(EngramDbError::Parse("DATE_TRUNC requires 2 arguments (unit, timestamp)".into()));
            }
            let unit_vec = eval_vectorized(&args[0], chunk, column_names)?;
            let ts_vec = eval_vectorized(&args[1], chunk, column_names)?;
            let unit = match &unit_vec {
                Vector::Constant(Value::Varchar(s), _) => s.to_lowercase(),
                _ => return Err(EngramDbError::Parse("DATE_TRUNC unit must be a constant string".into())),
            };
            match &ts_vec {
                Vector::Constant(v, n) => {
                    let val = date_trunc_value(v, &unit);
                    Ok(Vector::Constant(val, *n))
                }
                Vector::Flat(values) => {
                    let result: Vec<Value> = values.iter().map(|v| date_trunc_value(v, &unit)).collect();
                    Ok(Vector::Flat(result))
                }
            }
        }
        "STRPTIME" => {
            if args.len() != 2 {
                return Err(EngramDbError::Parse("STRPTIME requires 2 arguments (format, string)".into()));
            }
            let fmt_vec = eval_vectorized(&args[0], chunk, column_names)?;
            let s_vec = eval_vectorized(&args[1], chunk, column_names)?;
            let fmt = match &fmt_vec {
                Vector::Constant(Value::Varchar(s), _) => s.clone(),
                _ => return Err(EngramDbError::Parse("STRPTIME format must be a constant string".into())),
            };
            match &s_vec {
                Vector::Constant(v, n) => {
                    let val = strptime_value(&fmt, v);
                    Ok(Vector::Constant(val, *n))
                }
                Vector::Flat(values) => {
                    let result: Vec<Value> = values.iter().map(|v| strptime_value(&fmt, v)).collect();
                    Ok(Vector::Flat(result))
                }
            }
        }
        "MATCH" => {
            if args.len() != 2 {
                return Err(EngramDbError::Parse("MATCH requires 2 arguments (column, query)".into()));
            }
            let vec = eval_vectorized(&args[0], chunk, column_names)?;
            let query_vec = eval_vectorized(&args[1], chunk, column_names)?;
            let query = match &query_vec {
                Vector::Constant(Value::Varchar(s), _) => s.clone(),
                _ => return Err(EngramDbError::Parse("MATCH query must be a string literal".into())),
            };
            Ok(eval_match(&vec, &query))
        }
        _ => {
            Err(EngramDbError::Parse(format!("Unknown function: {}", name)))
        }
    }
}

/// 对文本列执行全文检索匹配（MATCH 函数）
/// 第一个参数为列引用，第二个参数为查询字符串
fn eval_match(vec: &Vector, query: &str) -> Vector {
    let tokens: Vec<String> = query.split_whitespace()
        .map(|t| t.to_lowercase())
        .collect();
    if tokens.is_empty() {
        return Vector::Flat(vec![Value::Boolean(false); vec.len()]);
    }
    match vec {
        Vector::Constant(v, n) => {
            let matches = match v {
                Value::Varchar(s) => {
                    let s_lower = s.to_lowercase();
                    tokens.iter().all(|t| s_lower.contains(t))
                }
                _ => false,
            };
            Vector::Constant(Value::Boolean(matches), *n)
        }
        Vector::Flat(values) => {
            let result: Vec<Value> = values.iter().map(|v| {
                match v {
                    Value::Varchar(s) => {
                        let s_lower = s.to_lowercase();
                        Value::Boolean(tokens.iter().all(|t| s_lower.contains(t)))
                    }
                    _ => Value::Boolean(false),
                }
            }).collect();
            Vector::Flat(result)
        }
    }
}

/// 计算当前时间戳（Unix 毫秒）
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

/// 将 Unix 毫秒时间戳格式化为 YYYY-MM-DD
fn format_date(ts_ms: i64) -> String {
    let secs = ts_ms / 1000;
    // 处理负时间戳：Rust 整数除法向零截断，需要调整
    let (days, rem) = if secs >= 0 {
        (secs / 86400, secs % 86400)
    } else {
        // 对负数向下取整
        ((secs - 86399) / 86400, (secs % 86400 + 86400) % 86400)
    };
    let _ = rem; // 不需要
    // 从 Unix epoch (1970-01-01) 开始计算
    let mut y = 1970i64;
    let mut remaining_days = days;
    if remaining_days < 0 {
        loop {
            y -= 1;
            let days_in_year = if is_leap_year(y) { 366 } else { 365 };
            remaining_days += days_in_year;
            if remaining_days >= 0 {
                break;
            }
        }
    } else {
        loop {
            let days_in_year = if is_leap_year(y) { 366 } else { 365 };
            if remaining_days < days_in_year {
                break;
            }
            remaining_days -= days_in_year;
            y += 1;
        }
    }
    let month_days = if is_leap_year(y) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut m = 0usize;
    for (i, &md) in month_days.iter().enumerate() {
        if remaining_days < md as i64 {
            m = i;
            break;
        }
        remaining_days -= md as i64;
    }
    let d = remaining_days + 1;
    format!("{:04}-{:02}-{:02}", y, m + 1, d)
}

fn is_leap_year(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

/// 使用 STRFTIME 格式格式化时间戳
fn format_strftime(fmt: &str, ts_ms: i64) -> String {
    let secs = ts_ms / 1000;
    // 处理负时间戳
    let (days, day_secs) = if secs >= 0 {
        (secs / 86400, secs % 86400)
    } else {
        ((secs - 86399) / 86400, (secs % 86400 + 86400) % 86400)
    };
    let hours = day_secs / 3600;
    let minutes = (day_secs % 3600) / 60;
    let seconds = day_secs % 60;

    // 计算年月日
    let mut y = 1970i64;
    let mut remaining_days = days;
    if remaining_days < 0 {
        loop {
            y -= 1;
            let days_in_year = if is_leap_year(y) { 366 } else { 365 };
            remaining_days += days_in_year;
            if remaining_days >= 0 {
                break;
            }
        }
    } else {
        loop {
            let days_in_year = if is_leap_year(y) { 366 } else { 365 };
            if remaining_days < days_in_year {
                break;
            }
            remaining_days -= days_in_year;
            y += 1;
        }
    }
    let month_days = if is_leap_year(y) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut m = 0usize;
    for (i, &md) in month_days.iter().enumerate() {
        if remaining_days < md as i64 {
            m = i;
            break;
        }
        remaining_days -= md as i64;
    }
    let d = remaining_days + 1;
    let wday = (days + 4) % 7; // 1970-01-01 是星期四 (4)

    let mut result = String::new();
    let chars: Vec<char> = fmt.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '%' && i + 1 < chars.len() {
            match chars[i + 1] {
                'Y' => result.push_str(&format!("{:04}", y)),
                'y' => result.push_str(&format!("{:02}", y % 100)),
                'm' => result.push_str(&format!("{:02}", m + 1)),
                'd' => result.push_str(&format!("{:02}", d)),
                'H' => result.push_str(&format!("{:02}", hours)),
                'M' => result.push_str(&format!("{:02}", minutes)),
                'S' => result.push_str(&format!("{:02}", seconds)),
                'w' => result.push_str(&format!("{}", wday)),
                'j' => result.push_str(&format!("{:03}", remaining_days + 1)),
                'U' => result.push_str(&format!("{:02}", (days + 7 - wday) / 7)),
                '%' => result.push('%'),
                _ => {
                    result.push('%');
                    result.push(chars[i + 1]);
                }
            }
            i += 2;
        } else {
            result.push(chars[i]);
            i += 1;
        }
    }
    result
}

/// 提取日期字符串（YYYY-MM-DD）
fn date_value(v: &Value) -> Value {
    if v.is_null() {
        return Value::Null;
    }
    if let Some(ts) = v.as_i64() {
        Value::Varchar(format_date(ts))
    } else if let Value::Varchar(s) = v {
        // 传入 YYYY-MM-DD 格式字符串，直接返回
        if s.len() == 10 && s.chars().filter(|&c| c == '-').count() == 2 {
            return Value::Varchar(s.clone());
        }
        Value::Null
    } else {
        Value::Null
    }
}

/// STRFTIME 格式化
fn strftime_value(fmt: &str, v: &Value) -> Value {
    if v.is_null() {
        return Value::Null;
    }
    if let Some(ts) = v.as_i64() {
        Value::Varchar(format_strftime(fmt, ts))
    } else {
        Value::Null
    }
}

/// 提取时间部分（HH:MM:SS）
fn time_value(v: &Value) -> Value {
    if v.is_null() {
        return Value::Null;
    }
    if let Some(ts) = v.as_i64() {
        let secs = ts / 1000;
        let day_secs = if secs >= 0 { secs % 86400 } else { (secs % 86400 + 86400) % 86400 };
        let h = day_secs / 3600;
        let m = (day_secs % 3600) / 60;
        let s = day_secs % 60;
        Value::Varchar(format!("{:02}:{:02}:{:02}", h, m, s))
    } else {
        Value::Null
    }
}

/// 提取日期时间字符串（YYYY-MM-DD HH:MM:SS）
fn datetime_value(v: &Value) -> Value {
    if v.is_null() {
        return Value::Null;
    }
    if let Some(ts) = v.as_i64() {
        let date = format_date(ts);
        let secs = ts / 1000;
        let day_secs = if secs >= 0 { secs % 86400 } else { (secs % 86400 + 86400) % 86400 };
        let h = day_secs / 3600;
        let m = (day_secs % 3600) / 60;
        let s = day_secs % 60;
        Value::Varchar(format!("{} {:02}:{:02}:{:02}", date, h, m, s))
    } else {
        Value::Null
    }
}

/// DATE_ADD(ts, n, unit) — 给时间戳加 n 个单位
fn date_add_value(ts: &Value, n: &Value, unit: &str) -> Value {
    if ts.is_null() || n.is_null() {
        return Value::Null;
    }
    let ts_ms = match ts.as_i64() {
        Some(v) => v,
        None => return Value::Null,
    };
    let num = match n.as_i64() {
        Some(v) => v,
        None => return Value::Null,
    };
    Value::Timestamp(apply_date_arithmetic(ts_ms, num, unit))
}

/// DATE_SUB(ts, n, unit) — 给时间戳减 n 个单位
fn date_sub_value(ts: &Value, n: &Value, unit: &str) -> Value {
    if ts.is_null() || n.is_null() {
        return Value::Null;
    }
    let ts_ms = match ts.as_i64() {
        Some(v) => v,
        None => return Value::Null,
    };
    let num = match n.as_i64() {
        Some(v) => v,
        None => return Value::Null,
    };
    Value::Timestamp(apply_date_arithmetic(ts_ms, -num, unit))
}

/// DATE_DIFF(ts1, ts2, unit) — 计算两个时间戳的差值
fn date_diff_value(ts1: &Value, ts2: &Value, unit: &str) -> Value {
    if ts1.is_null() || ts2.is_null() {
        return Value::Null;
    }
    let a = match ts1.as_i64() {
        Some(v) => v,
        None => return Value::Null,
    };
    let b = match ts2.as_i64() {
        Some(v) => v,
        None => return Value::Null,
    };
    let diff_ms = a - b;
    let result = match unit {
        "millisecond" | "milliseconds" | "ms" => diff_ms,
        "second" | "seconds" | "sec" | "s" => diff_ms / 1000,
        "minute" | "minutes" | "min" => diff_ms / 60000,
        "hour" | "hours" | "h" => diff_ms / 3600000,
        "day" | "days" | "d" => diff_ms / 86400000,
        "week" | "weeks" | "w" => diff_ms / 604800000,
        _ => return Value::Null,
    };
    Value::Int64(result)
}

/// DATE_TRUNC(unit, timestamp) — 按单位截断时间戳
fn date_trunc_value(v: &Value, unit: &str) -> Value {
    if v.is_null() {
        return Value::Null;
    }
    let ts_ms = match v.as_i64() {
        Some(v) => v,
        None => return Value::Null,
    };
    let secs = ts_ms / 1000;
    let (days, day_secs) = if secs >= 0 {
        (secs / 86400, secs % 86400)
    } else {
        ((secs - 86399) / 86400, (secs % 86400 + 86400) % 86400)
    };
    let result_secs = match unit {
        "year" | "years" | "y" => {
            // 向前找最近一年的第一天
            let date_str = format_date(ts_ms);
            let y: i64 = date_str[..4].parse().unwrap_or(1970);
            // 当年 1 月 1 日 00:00:00 UTC
            let start_of_year = format!("{}-01-01", y);
            parse_date_to_epoch(&start_of_year)
        }
        "month" | "months" => {
            let date_str = format_date(ts_ms);
            let parts: Vec<&str> = date_str.split('-').collect();
            let y = parts[0].parse::<i64>().unwrap_or(1970);
            let m = parts[1].parse::<i64>().unwrap_or(1);
            let start_of_month = format!("{:04}-{:02}-01", y, m);
            parse_date_to_epoch(&start_of_month)
        }
        "week" | "weeks" | "w" => {
            // 截断到周一 00:00:00 UTC
            // 1970-01-01 是周四，day 0 = 周四
            let weekday = if days >= 0 { (days + 4) % 7 } else { ((days + 4) % 7 + 7) % 7 };
            let monday_days = days - weekday; // 向前到周一
            monday_days * 86400
        }
        "day" | "days" | "d" => days * 86400,
        "hour" | "hours" | "h" => days * 86400 + (day_secs / 3600) * 3600,
        "minute" | "minutes" | "min" => days * 86400 + (day_secs / 60) * 60,
        _ => secs, // 默认不截断
    };
    Value::Timestamp(result_secs * 1000)
}

/// 将 YYYY-MM-DD 格式的日期字符串解析为 Unix 纪元秒
fn parse_date_to_epoch(date_str: &str) -> i64 {
    let parts: Vec<&str> = date_str.split('-').collect();
    if parts.len() < 3 {
        return 0;
    }
    let y = parts[0].parse::<i64>().unwrap_or(1970);
    let m = parts[1].parse::<i64>().unwrap_or(1);
    let d = parts[2].parse::<i64>().unwrap_or(1);
    // 计算从 1970-01-01 到 y-m-d 的天数
    let mut days = 0i64;
    if y >= 1970 {
        for year in 1970..y {
            days += if is_leap_year(year) { 366 } else { 365 };
        }
        let month_days = if is_leap_year(y) {
            [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
        } else {
            [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
        };
        for i in 0..(m - 1) as usize {
            days += month_days[i] as i64;
        }
        days += d - 1;
    } else {
        for year in y..1970 {
            days -= if is_leap_year(year) { 366 } else { 365 };
        }
        let month_days = if is_leap_year(y) {
            [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
        } else {
            [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
        };
        for i in 0..(m - 1) as usize {
            days += month_days[i] as i64;
        }
        days += d - 1;
    }
    days * 86400
}

/// STRPTIME(format, string) — 将字符串按格式解析为时间戳
fn strptime_value(fmt: &str, v: &Value) -> Value {
    if v.is_null() {
        return Value::Null;
    }
    let s = match v.as_str() {
        Some(s) => s,
        None => return Value::Null,
    };
    // 支持常见格式: %Y-%m-%d, %Y-%m-%d %H:%M:%S, %Y-%m-%dT%H:%M:%S
    if fmt.contains("%Y") && fmt.contains("%m") && fmt.contains("%d") {
        // 提取数字部分
        let digits: String = s.chars().filter(|c| c.is_ascii_digit() || *c == '-' || *c == 'T' || *c == ':' || *c == ' ').collect();
        let parts: Vec<&str> = digits.split(|c| c == '-' || c == ' ' || c == 'T' || c == ':').filter(|p| !p.is_empty()).collect();
        if parts.len() >= 3 {
            let y = parts[0].parse::<i64>().unwrap_or(1970);
            let m = parts[1].parse::<i64>().unwrap_or(1);
            let d = parts[2].parse::<i64>().unwrap_or(1);
            let h = if parts.len() > 3 { parts[3].parse::<i64>().unwrap_or(0) } else { 0 };
            let min = if parts.len() > 4 { parts[4].parse::<i64>().unwrap_or(0) } else { 0 };
            let sec = if parts.len() > 5 { parts[5].parse::<i64>().unwrap_or(0) } else { 0 };
            let epoch_secs = parse_date_to_epoch(&format!("{:04}-{:02}-{:02}", y, m, d));
            return Value::Timestamp((epoch_secs + h * 3600 + min * 60 + sec) * 1000);
        }
    }
    Value::Null
}

/// 日期算术运算（内部辅助）
fn apply_date_arithmetic(ts_ms: i64, delta: i64, unit: &str) -> i64 {
    match unit {
        "millisecond" | "milliseconds" | "ms" => ts_ms + delta,
        "second" | "seconds" | "sec" | "s" => ts_ms + delta * 1000,
        "minute" | "minutes" | "min" => ts_ms + delta * 60000,
        "hour" | "hours" | "h" => ts_ms + delta * 3600000,
        "day" | "days" | "d" => ts_ms + delta * 86400000,
        "week" | "weeks" | "w" => ts_ms + delta * 604800000,
        _ => ts_ms, // 未知单位，返回原值
    }
}

fn eval_abs(vec: &Vector) -> Result<Vector> {
    match vec {
        Vector::Constant(v, n) => Ok(Vector::Constant(abs_value(v), *n)),
        Vector::Flat(values) => {
            let result: Vec<Value> = values.iter().map(abs_value).collect();
            Ok(Vector::Flat(result))
        }
    }
}

fn abs_value(v: &Value) -> Value {
    if v.is_null() {
        return Value::Null;
    }
    if let Some(i) = v.as_i64() {
        Value::Int64(i.abs())
    } else if let Some(f) = v.as_f64() {
        Value::Float64(f.abs())
    } else {
        Value::Null
    }
}

fn eval_length(vec: &Vector) -> Result<Vector> {
    match vec {
        Vector::Constant(v, n) => Ok(Vector::Constant(length_value(v), *n)),
        Vector::Flat(values) => {
            let result: Vec<Value> = values.iter().map(length_value).collect();
            Ok(Vector::Flat(result))
        }
    }
}

fn length_value(v: &Value) -> Value {
    match v {
        Value::Null => Value::Null,
        Value::Varchar(s) => Value::Int64(s.chars().count() as i64),
        _ => Value::Null,
    }
}

fn eval_upper(vec: &Vector) -> Result<Vector> {
    match vec {
        Vector::Constant(v, n) => Ok(Vector::Constant(upper_value(v), *n)),
        Vector::Flat(values) => {
            let result: Vec<Value> = values.iter().map(upper_value).collect();
            Ok(Vector::Flat(result))
        }
    }
}

fn upper_value(v: &Value) -> Value {
    match v {
        Value::Null => Value::Null,
        Value::Varchar(s) => Value::Varchar(s.to_uppercase()),
        _ => v.clone(),
    }
}

fn eval_lower(vec: &Vector) -> Result<Vector> {
    match vec {
        Vector::Constant(v, n) => Ok(Vector::Constant(lower_value(v), *n)),
        Vector::Flat(values) => {
            let result: Vec<Value> = values.iter().map(lower_value).collect();
            Ok(Vector::Flat(result))
        }
    }
}

fn lower_value(v: &Value) -> Value {
    match v {
        Value::Null => Value::Null,
        Value::Varchar(s) => Value::Varchar(s.to_lowercase()),
        _ => v.clone(),
    }
}

fn eval_round(vec: &Vector, decimals: i32) -> Result<Vector> {
    match vec {
        Vector::Constant(v, n) => Ok(Vector::Constant(round_value(v, decimals), *n)),
        Vector::Flat(values) => {
            let result: Vec<Value> = values.iter().map(|v| round_value(v, decimals)).collect();
            Ok(Vector::Flat(result))
        }
    }
}

fn round_value(v: &Value, decimals: i32) -> Value {
    if v.is_null() {
        return Value::Null;
    }
    if let Some(f) = v.as_f64() {
        let factor = 10.0f64.powi(decimals);
        Value::Float64((f * factor).round() / factor)
    } else if let Some(i) = v.as_i64() {
        if decimals <= 0 {
            Value::Int64(i)
        } else {
            Value::Float64(i as f64)
        }
    } else {
        Value::Null
    }
}

fn eval_coalesce(vecs: &[Vector]) -> Result<Vector> {
    if vecs.is_empty() {
        return Ok(Vector::Constant(Value::Null, 0));
    }

    let len = vecs[0].len();
    let mut result = Vec::with_capacity(len);

    for i in 0..len {
        let mut val = Value::Null;
        for vec in vecs {
            let v = vec.get(i);
            if !v.is_null() {
                val = v.clone();
                break;
            }
        }
        result.push(val);
    }

    Ok(Vector::Flat(result))
}

fn eval_substring(str_vec: &Vector, start_vec: &Vector, len_vec: Option<&Vector>) -> Result<Vector> {
    let s_flat = str_vec.to_flat();
    let st_flat = start_vec.to_flat();
    let len_flat = len_vec.map(|v| v.to_flat());
    let count = s_flat.len();

    let mut result = Vec::with_capacity(count);
    for i in 0..count {
        let s = match &s_flat[i] {
            Value::Varchar(s) => s.clone(),
            Value::Null => { result.push(Value::Null); continue; }
            _ => { result.push(Value::Null); continue; }
        };

        let start = match st_flat[i].as_i64() {
            Some(v) => v,
            None => { result.push(Value::Null); continue; }
        };

        // SQL 中 SUBSTRING 起始位置从 1 开始
        let start_idx = if start > 0 {
            (start - 1) as usize
        } else {
            0
        };

        let chars: Vec<char> = s.chars().collect();
        if start_idx >= chars.len() {
            result.push(Value::Varchar(String::new()));
            continue;
        }

        let substr = match &len_flat {
            Some(lv) => {
                let l = match lv[i].as_i64() {
                    Some(v) => v.max(0) as usize,
                    None => { result.push(Value::Null); continue; }
                };
                let end = (start_idx + l).min(chars.len());
                chars[start_idx..end].iter().collect()
            }
            None => {
                chars[start_idx..].iter().collect()
            }
        };

        result.push(Value::Varchar(substr));
    }

    Ok(Vector::Flat(result))
}

fn eval_nullif(a_vec: &Vector, b_vec: &Vector) -> Result<Vector> {
    let len = a_vec.len();
    let mut result = Vec::with_capacity(len);
    for i in 0..len {
        let a = a_vec.get(i);
        let b = b_vec.get(i);
        // NULLIF(a, b): a == b 时返回 NULL，否则返回 a
        // NULL 与任何值比较都为 NULL（三值逻辑）
        if a.is_null() || b.is_null() {
            result.push(a.clone());
        } else if a == b {
            result.push(Value::Null);
        } else {
            result.push(a.clone());
        }
    }
    Ok(Vector::Flat(result))
}

fn eval_if(cond_vec: &Vector, true_vec: &Vector, false_vec: &Vector) -> Result<Vector> {
    let len = cond_vec.len();
    let mut result = Vec::with_capacity(len);
    for i in 0..len {
        let cond = cond_vec.get(i);
        // IF(cond, a, b): cond 为 TRUE 时返回 a，否则返回 b
        // 任何与 NULL 的判断都为 NULL（结果取决于实现，这里返回 false_val）
        if matches!(cond, Value::Boolean(true)) {
            result.push(true_vec.get(i).clone());
        } else {
            result.push(false_vec.get(i).clone());
        }
    }
    Ok(Vector::Flat(result))
}

#[derive(Debug, Clone, Copy)]
enum TrimMode { Both, Left, Right }

fn eval_trim(str_vec: &Vector, chars_vec: Option<&Vector>, mode: TrimMode) -> Result<Vector> {
    let len = str_vec.len();
    let mut result = Vec::with_capacity(len);
    for i in 0..len {
        let s = str_vec.get(i);
        if s.is_null() {
            result.push(Value::Null);
            continue;
        }
        let s_str = match s.as_str() {
            Some(s) => s.to_string(),
            None => {
                result.push(Value::Null);
                continue;
            }
        };
        // 字符集：默认空白
        let chars: Option<String> = if let Some(cv) = chars_vec {
            let c = cv.get(i);
            if c.is_null() {
                result.push(Value::Null);
                continue;
            }
            c.as_str().map(|x| x.to_string())
        } else {
            None
        };

        let trimmed = match chars {
            None => {
                // 默认去除空白
                let s_trimmed = match mode {
                    TrimMode::Both => s_str.trim().to_string(),
                    TrimMode::Left => s_str.trim_start().to_string(),
                    TrimMode::Right => s_str.trim_end().to_string(),
                };
                s_trimmed
            }
            Some(chars_str) => {
                let chars: Vec<char> = chars_str.chars().collect();
                let s_chars: Vec<char> = s_str.chars().collect();
                let (start, end) = match mode {
                    TrimMode::Both => {
                        let mut lo = 0usize;
                        let mut hi = s_chars.len();
                        while lo < hi && chars.contains(&s_chars[lo]) { lo += 1; }
                        while hi > lo && chars.contains(&s_chars[hi - 1]) { hi -= 1; }
                        (lo, hi)
                    }
                    TrimMode::Left => {
                        let mut lo = 0usize;
                        while lo < s_chars.len() && chars.contains(&s_chars[lo]) { lo += 1; }
                        (lo, s_chars.len())
                    }
                    TrimMode::Right => {
                        let mut hi = s_chars.len();
                        while hi > 0 && chars.contains(&s_chars[hi - 1]) { hi -= 1; }
                        (0, hi)
                    }
                };
                s_chars[start..end].iter().collect()
            }
        };
        result.push(Value::Varchar(trimmed));
    }
    Ok(Vector::Flat(result))
}

fn eval_instr(haystack_vec: &Vector, needle_vec: &Vector) -> Result<Vector> {
    let len = haystack_vec.len();
    let mut result = Vec::with_capacity(len);
    for i in 0..len {
        let h = haystack_vec.get(i);
        let n = needle_vec.get(i);
        if h.is_null() || n.is_null() {
            result.push(Value::Null);
            continue;
        }
        let h_str = match h.as_str() {
            Some(s) => s,
            None => {
                result.push(Value::Null);
                continue;
            }
        };
        let n_str = match n.as_str() {
            Some(s) => s,
            None => {
                result.push(Value::Null);
                continue;
            }
        };
        // 1-based position; 0 if not found
        let pos = h_str.find(&n_str)
            .map(|idx| {
                // 计算 1-based 字符位置（而不是字节位置）
                let prefix = &h_str[..idx];
                let prefix_chars = prefix.chars().count();
                (prefix_chars + 1) as i64
            })
            .unwrap_or(0);
        result.push(Value::Int64(pos));
    }
    Ok(Vector::Flat(result))
}

fn eval_split_part(str_vec: &Vector, delim_vec: &Vector, part_vec: &Vector) -> Result<Vector> {
    let len = str_vec.len();
    let mut result = Vec::with_capacity(len);
    for i in 0..len {
        let s = str_vec.get(i);
        let d = delim_vec.get(i);
        let p = part_vec.get(i);
        if s.is_null() || d.is_null() || p.is_null() {
            result.push(Value::Null);
            continue;
        }
        let s_str = match s.as_str() {
            Some(v) => v,
            None => { result.push(Value::Null); continue; }
        };
        let d_str = match d.as_str() {
            Some(v) => v,
            None => { result.push(Value::Null); continue; }
        };
        let part = match p.as_i64() {
            Some(v) => v,
            None => { result.push(Value::Null); continue; }
        };
        // 1-based part index; empty string if out of range
        if part < 1 {
            result.push(Value::Varchar("".into()));
            continue;
        }
        let parts: Vec<&str> = s_str.split(d_str).collect();
        let idx = (part - 1) as usize;
        if idx >= parts.len() {
            result.push(Value::Varchar("".into()));
        } else {
            result.push(Value::Varchar(parts[idx].to_string()));
        }
    }
    Ok(Vector::Flat(result))
}

/// TYPEOF(expr) — 返回表达式的类型名称
fn eval_typeof(vec: &Vector) -> Result<Vector> {
    let len = vec.len();
    let mut result = Vec::with_capacity(len);
    for i in 0..len {
        let v = vec.get(i);
        let type_name = match v {
            Value::Null => "null",
            Value::Boolean(_) => "boolean",
            Value::Int32(_) => "int32",
            Value::Int64(_) => "int64",
            Value::Float32(_) => "float32",
            Value::Float64(_) => "float64",
            Value::Varchar(_) => "varchar",
            Value::Timestamp(_) => "timestamp",
            Value::Json(_) => "json",
            Value::Vector(_) => "vector",
            Value::VectorInt8(_) => "vector_int8",
            Value::Blob(_) => "blob",
        };
        result.push(Value::Varchar(type_name.to_string()));
    }
    Ok(Vector::Flat(result))
}

/// 一元数值函数辅助：将 Value 转为 f64，应用函数 f，再转回 Value
fn eval_unary_numeric<F: Fn(f64) -> f64>(vec: &Vector, f: F) -> Result<Vector> {
    let len = vec.len();
    let mut result = Vec::with_capacity(len);
    for i in 0..len {
        let v = vec.get(i);
        let n = match v {
            Value::Null => {
                result.push(Value::Null);
                continue;
            }
            Value::Int32(x) => *x as f64,
            Value::Int64(x) => *x as f64,
            Value::Float32(x) => *x as f64,
            Value::Float64(x) => *x,
            _ => {
                result.push(Value::Null);
                continue;
            }
        };
        let r = f(n);
        // 保留整数（如果结果是整数）
        if r.fract() == 0.0 && r.is_finite() && r >= i64::MIN as f64 && r <= i64::MAX as f64 {
            result.push(Value::Int64(r as i64));
        } else {
            result.push(Value::Float64(r));
        }
    }
    Ok(Vector::Flat(result))
}

/// 二元数值函数辅助：两个 Value 转为 f64，应用函数 f
fn eval_binary_numeric<F: Fn(f64, f64) -> f64>(
    a_vec: &Vector,
    b_vec: &Vector,
    f: F,
) -> Result<Vector> {
    let len = a_vec.len();
    let mut result = Vec::with_capacity(len);
    for i in 0..len {
        let a = a_vec.get(i);
        let b = b_vec.get(i);
        if a.is_null() || b.is_null() {
            result.push(Value::Null);
            continue;
        }
        let x = match a {
            Value::Int32(x) => *x as f64,
            Value::Int64(x) => *x as f64,
            Value::Float32(x) => *x as f64,
            Value::Float64(x) => *x,
            _ => {
                result.push(Value::Null);
                continue;
            }
        };
        let y = match b {
            Value::Int32(y) => *y as f64,
            Value::Int64(y) => *y as f64,
            Value::Float32(y) => *y as f64,
            Value::Float64(y) => *y,
            _ => {
                result.push(Value::Null);
                continue;
            }
        };
        let r = f(x, y);
        if r.fract() == 0.0 && r.is_finite() && r >= i64::MIN as f64 && r <= i64::MAX as f64 {
            result.push(Value::Int64(r as i64));
        } else {
            result.push(Value::Float64(r));
        }
    }
    Ok(Vector::Flat(result))
}

fn eval_concat_func(vecs: &[Vector]) -> Result<Vector> {
    if vecs.is_empty() {
        return Ok(Vector::Constant(Value::Null, 0));
    }

    let len = vecs[0].len();
    let mut result = Vec::with_capacity(len);

    for i in 0..len {
        let mut has_null = false;
        let mut acc = String::new();
        for vec in vecs {
            let v = vec.get(i);
            if v.is_null() {
                has_null = true;
                break;
            }
            match v {
                Value::Varchar(s) => acc.push_str(s),
                _ => acc.push_str(&format!("{}", v)),
            }
        }
        if has_null {
            result.push(Value::Null);
        } else {
            result.push(Value::Varchar(acc));
        }
    }

    Ok(Vector::Flat(result))
}

fn eval_mod(a: &Vector, b: &Vector) -> Result<Vector> {
    let a_vals = a.to_flat();
    let b_vals = b.to_flat();
    let len = a_vals.len().min(b_vals.len());
    let mut result = Vec::with_capacity(len);
    for i in 0..len {
        let v = match (&a_vals[i], &b_vals[i]) {
            (Value::Int64(a), Value::Int64(b)) => {
                if *b == 0 { Value::Null } else { Value::Int64(a.rem_euclid(*b)) }
            }
            (Value::Int32(a), Value::Int32(b)) => {
                if *b == 0 { Value::Null } else { Value::Int32(a.rem_euclid(*b)) }
            }
            _ => Value::Null,
        };
        result.push(v);
    }
    Ok(Vector::Flat(result))
}

fn eval_replace(str_vec: &Vector, from_vec: &Vector, to_vec: &Vector) -> Result<Vector> {
    let str_vals = str_vec.to_flat();
    let from_vals = from_vec.to_flat();
    let to_vals = to_vec.to_flat();
    let len = str_vals.len().min(from_vals.len()).min(to_vals.len());
    let mut result = Vec::with_capacity(len);
    for i in 0..len {
        let r = match (&str_vals[i], &from_vals[i], &to_vals[i]) {
            (Value::Varchar(s), Value::Varchar(from), Value::Varchar(to)) => {
                Value::Varchar(s.replace(from.as_str(), to.as_str()))
            }
            _ => Value::Null,
        };
        result.push(r);
    }
    Ok(Vector::Flat(result))
}

// ============================================================================
// 工具函数：生成布尔选择向量
// ============================================================================

/// 从布尔 Vector 生成选择向量的索引列表
///
/// 用于 Filter 算子：将向量化条件求值结果转换为 SelectionVector。
pub fn boolean_to_selection(bool_vec: &Vector) -> Vec<usize> {
    match bool_vec {
        Vector::Constant(Value::Boolean(true), n) => {
            (0..*n).collect()
        }
        Vector::Constant(_, _) => {
            Vec::new() // false 或 NULL → 全不选
        }
        Vector::Flat(values) => {
            values.iter()
                .enumerate()
                .filter(|(_, v)| matches!(v, Value::Boolean(true)))
                .map(|(i, _)| i)
                .collect()
        }
    }
}

// ============================================================================
// JSON 函数（v0.12.0 新增）
// ============================================================================

/// 解析 JSON 路径表达式（简化版，支持 $.key1.key2 和 $.arr[0] 形式）
fn parse_json_path(path: &str) -> Vec<serde_json::Value> {
    let mut result = Vec::new();
    let path = path.trim_start_matches('$').trim_start_matches('.');
    if path.is_empty() {
        return result;
    }
    for segment in path.split('.') {
        // 检查是否包含数组索引，如 key[0]
        if let Some(bracket_pos) = segment.find('[') {
            let key = &segment[..bracket_pos];
            if !key.is_empty() {
                result.push(serde_json::Value::String(key.to_string()));
            }
            // 提取索引
            let rest = &segment[bracket_pos..];
            let end_bracket = rest.find(']').unwrap_or(rest.len());
            let idx_str = &rest[1..end_bracket];
            if let Ok(idx) = idx_str.parse::<usize>() {
                result.push(serde_json::Value::Number(idx.into()));
            }
        } else {
            result.push(serde_json::Value::String(segment.to_string()));
        }
    }
    result
}

/// 按路径从 JSON 值中提取子值
fn json_extract_path(json_val: &serde_json::Value, path: &[serde_json::Value]) -> Option<serde_json::Value> {
    let mut current = json_val;
    for segment in path {
        match segment {
            serde_json::Value::String(key) => {
                current = current.get(key)?;
            }
            serde_json::Value::Number(n) => {
                let idx = n.as_u64()? as usize;
                current = current.get(idx)?;
            }
            _ => return None,
        }
    }
    Some(current.clone())
}

fn eval_json_extract(json_vec: &Vector, path_vec: &Vector) -> Result<Vector> {
    match (json_vec, path_vec) {
        (Vector::Constant(jv, n), Vector::Constant(pv, _)) => {
            Ok(Vector::Constant(json_extract_value(jv, pv), *n))
        }
        _ => {
            let json_flat = json_vec.to_flat();
            let path_flat = path_vec.to_flat();
            let len = json_flat.len();
            let mut result = Vec::with_capacity(len);
            for i in 0..len {
                let path_val = if path_flat.len() == 1 {
                    &path_flat[0]
                } else if i < path_flat.len() {
                    &path_flat[i]
                } else {
                    result.push(Value::Null);
                    continue;
                };
                result.push(json_extract_value(&json_flat[i], path_val));
            }
            Ok(Vector::Flat(result))
        }
    }
}

fn json_extract_value(json_val: &Value, path_val: &Value) -> Value {
    let json_str = match json_val {
        Value::Json(s) => s.as_str(),
        Value::Varchar(s) => s.as_str(),
        Value::Null => return Value::Null,
        _ => return Value::Null,
    };
    let path_str = match path_val {
        Value::Varchar(s) => s.as_str(),
        Value::Json(s) => s.as_str(),
        Value::Null => return Value::Null,
        _ => return Value::Null,
    };
    let parsed = match serde_json::from_str::<serde_json::Value>(json_str) {
        Ok(v) => v,
        Err(_) => return Value::Null,
    };
    let path = parse_json_path(path_str);
    match json_extract_path(&parsed, &path) {
        Some(v) => Value::Json(v.to_string()),
        None => Value::Null,
    }
}

fn eval_json_contains(json_vec: &Vector, target_vec: &Vector, path_vec: Option<&Vector>) -> Result<Vector> {
    let json_flat = json_vec.to_flat();
    let target_flat = target_vec.to_flat();
    let path_flat = path_vec.map(|v| v.to_flat());
    let len = json_flat.len();
    let mut result = Vec::with_capacity(len);
    for i in 0..len {
        let jv = &json_flat[i];
        let tv = if target_flat.len() == 1 { &target_flat[0] } else { &target_flat[i] };
        let pv = path_flat.as_ref().map(|p| {
            if p.len() == 1 { &p[0] } else { &p[i] }
        });
        result.push(json_contains_value(jv, tv, pv));
    }
    Ok(Vector::Flat(result))
}

fn json_contains_value(json_val: &Value, target_val: &Value, path_val: Option<&Value>) -> Value {
    let json_str = match json_val {
        Value::Json(s) => s.as_str(),
        Value::Varchar(s) => s.as_str(),
        Value::Null => return Value::Null,
        _ => return Value::Null,
    };
    let target_str = match target_val {
        Value::Json(s) => s.as_str(),
        Value::Varchar(s) => s.as_str(),
        Value::Null => return Value::Null,
        _ => return Value::Null,
    };
    let parsed_json = match serde_json::from_str::<serde_json::Value>(json_str) {
        Ok(v) => v,
        Err(_) => return Value::Null,
    };
    let parsed_target = match serde_json::from_str::<serde_json::Value>(target_str) {
        Ok(v) => v,
        Err(_) => return Value::Null,
    };
    let current = if let Some(pv) = path_val {
        let path_str = match pv {
            Value::Varchar(s) => s.as_str(),
            Value::Json(s) => s.as_str(),
            _ => return Value::Null,
        };
        let path = parse_json_path(path_str);
        match json_extract_path(&parsed_json, &path) {
            Some(v) => v,
            None => return Value::Null,
        }
    } else {
        parsed_json
    };
    Value::Boolean(json_contains_recursive(&current, &parsed_target))
}

fn json_contains_recursive(container: &serde_json::Value, target: &serde_json::Value) -> bool {
    if container == target {
        return true;
    }
    match container {
        serde_json::Value::Object(map) => {
            map.values().any(|v| json_contains_recursive(v, target))
        }
        serde_json::Value::Array(arr) => {
            if target.is_array() {
                // 数组包含：target 的每个元素都在 container 数组中找到
                if let Some(target_arr) = target.as_array() {
                    return target_arr.iter().all(|t| {
                        arr.iter().any(|c| json_contains_recursive(c, t))
                    });
                }
            }
            arr.iter().any(|v| json_contains_recursive(v, target))
        }
        _ => false,
    }
}

fn eval_json_array_length(json_vec: &Vector, path_vec: Option<&Vector>) -> Result<Vector> {
    let json_flat = json_vec.to_flat();
    let path_flat = path_vec.map(|v| v.to_flat());
    let len = json_flat.len();
    let mut result = Vec::with_capacity(len);
    for i in 0..len {
        let jv = &json_flat[i];
        let pv = path_flat.as_ref().map(|p| {
            if p.len() == 1 { &p[0] } else { &p[i] }
        });
        result.push(json_array_length_value(jv, pv));
    }
    Ok(Vector::Flat(result))
}

fn json_array_length_value(json_val: &Value, path_val: Option<&Value>) -> Value {
    let json_str = match json_val {
        Value::Json(s) => s.as_str(),
        Value::Varchar(s) => s.as_str(),
        Value::Null => return Value::Null,
        _ => return Value::Null,
    };
    let parsed = match serde_json::from_str::<serde_json::Value>(json_str) {
        Ok(v) => v,
        Err(_) => return Value::Null,
    };
    let current = if let Some(pv) = path_val {
        let path_str = match pv {
            Value::Varchar(s) => s.as_str(),
            Value::Json(s) => s.as_str(),
            _ => return Value::Null,
        };
        let path = parse_json_path(path_str);
        match json_extract_path(&parsed, &path) {
            Some(v) => v,
            None => return Value::Null,
        }
    } else {
        parsed
    };
    match current.as_array() {
        Some(arr) => Value::Int64(arr.len() as i64),
        None => Value::Null,
    }
}

/// 将 Value 转为 serde_json::Value
fn value_to_json(v: &Value) -> serde_json::Value {
    match v {
        Value::Null => serde_json::Value::Null,
        Value::Boolean(b) => serde_json::Value::Bool(*b),
        Value::Int32(n) => serde_json::Value::from(*n),
        Value::Int64(n) => serde_json::Value::from(*n),
        Value::Float32(n) => serde_json::Value::from(*n as f64),
        Value::Float64(n) => serde_json::Value::from(*n),
        Value::Varchar(s) | Value::Json(s) => {
            // 先尝试解析为 JSON，否则当作字符串
            serde_json::from_str(s).unwrap_or_else(|_| serde_json::Value::String(s.clone()))
        }
        Value::Blob(_) => serde_json::Value::String("<blob>".to_string()),
        Value::Vector(_) | Value::VectorInt8(_) => serde_json::Value::String("<vector>".to_string()),
        Value::Timestamp(t) => serde_json::Value::from(*t),
    }
}

/// 将 serde_json::Value 转回 Value::Json 字符串
fn json_to_value(v: &serde_json::Value) -> Value {
    Value::Json(v.to_string())
}

/// JSON_OBJECT(k1, v1, k2, v2, ...) — 构造 JSON 对象
fn eval_json_object(key_vecs: &[Vector], val_vecs: &[Vector]) -> Result<Vector> {
    let count = if let Some(v) = key_vecs.first() { v.len() } else { 1 };
    let mut result = Vec::with_capacity(count);
    for i in 0..count {
        let mut obj = serde_json::Map::new();
        for (k_vec, v_vec) in key_vecs.iter().zip(val_vecs.iter()) {
            let k_val = k_vec.get(i);
            let v_val = v_vec.get(i);
            let key = match k_val {
                Value::Varchar(s) | Value::Json(s) => s.clone(),
                _ => format!("{}", k_val),
            };
            obj.insert(key, value_to_json(&v_val));
        }
        result.push(json_to_value(&serde_json::Value::Object(obj)));
    }
    Ok(Vector::Flat(result))
}

/// JSON_ARRAY(v1, v2, ...) — 构造 JSON 数组
fn eval_json_array(val_vecs: &[Vector]) -> Result<Vector> {
    let count = if let Some(v) = val_vecs.first() { v.len() } else { 1 };
    let mut result = Vec::with_capacity(count);
    for i in 0..count {
        let mut arr = Vec::new();
        for v_vec in val_vecs.iter() {
            arr.push(value_to_json(&v_vec.get(i)));
        }
        result.push(json_to_value(&serde_json::Value::Array(arr)));
    }
    Ok(Vector::Flat(result))
}

/// 在 serde_json::Value 上设置路径的值（递归）
///
/// path 是 JSONPath 片段列表（如 ["a", "b"] 表示 $.a.b）
/// insert_only=true: 仅当路径不存在时设置
fn json_set_path(
    root: &mut serde_json::Value,
    path: &[serde_json::Value],
    value: serde_json::Value,
    insert_only: bool,
) {
    if path.is_empty() {
        *root = value;
        return;
    }
    let key = &path[0];
    let rest = &path[1..];

    match root {
        serde_json::Value::Object(map) => {
            let key_str = match key {
                serde_json::Value::String(s) => s.clone(),
                _ => key.to_string(),
            };
            if rest.is_empty() {
                if insert_only && map.contains_key(&key_str) {
                    return;
                }
                map.insert(key_str, value);
            } else {
                let entry = map.entry(key_str.clone()).or_insert(serde_json::Value::Null);
                json_set_path(entry, rest, value, insert_only);
            }
        }
        serde_json::Value::Array(arr) => {
            // 数组索引路径
            if let Some(idx) = key.as_u64() {
                let idx = idx as usize;
                if idx <= arr.len() {
                    if rest.is_empty() {
                        if idx == arr.len() {
                            arr.push(value);
                        } else if !insert_only {
                            arr[idx] = value;
                        }
                    } else if idx < arr.len() {
                        json_set_path(&mut arr[idx], rest, value, insert_only);
                    }
                }
            }
        }
        _ => {
            // 非对象/数组：替换为对象
            if !insert_only {
                let mut map = serde_json::Map::new();
                let key_str = match key {
                    serde_json::Value::String(s) => s.clone(),
                    _ => key.to_string(),
                };
                if rest.is_empty() {
                    map.insert(key_str, value);
                } else {
                    let mut new_val = serde_json::Value::Null;
                    json_set_path(&mut new_val, rest, value, insert_only);
                    map.insert(key_str, new_val);
                }
                *root = serde_json::Value::Object(map);
            }
        }
    }
}

/// JSON_SET / JSON_INSERT：路径设置/创建值
fn eval_json_set(json_vec: &Vector, path_vec: &Vector, val_vec: &Vector, insert_only: bool) -> Result<Vector> {
    let json_flat = json_vec.to_flat();
    let path_flat = path_vec.to_flat();
    let val_flat = val_vec.to_flat();
    let len = json_flat.len();
    let mut result = Vec::with_capacity(len);
    for i in 0..len {
        let jv = &json_flat[i];
        let pv = &path_flat[i.min(path_flat.len() - 1)];
        let vv = &val_flat[i];
        if jv.is_null() || pv.is_null() {
            result.push(Value::Null);
            continue;
        }
        let json_str = match jv.as_str() {
            Some(s) => s,
            None => {
                result.push(Value::Null);
                continue;
            }
        };
        let path_str = match pv.as_str() {
            Some(s) => s,
            None => {
                result.push(Value::Null);
                continue;
            }
        };
        let mut parsed = match serde_json::from_str::<serde_json::Value>(json_str) {
            Ok(v) => v,
            Err(_) => {
                result.push(Value::Null);
                continue;
            }
        };
        let path = parse_json_path(path_str);
        json_set_path(&mut parsed, &path, value_to_json(vv), insert_only);
        result.push(json_to_value(&parsed));
    }
    Ok(Vector::Flat(result))
}

/// JSON_REPLACE：仅当路径存在时替换
fn eval_json_replace(json_vec: &Vector, path_vec: &Vector, val_vec: &Vector) -> Result<Vector> {
    let json_flat = json_vec.to_flat();
    let path_flat = path_vec.to_flat();
    let val_flat = val_vec.to_flat();
    let len = json_flat.len();
    let mut result = Vec::with_capacity(len);
    for i in 0..len {
        let jv = &json_flat[i];
        let pv = &path_flat[i.min(path_flat.len() - 1)];
        let vv = &val_flat[i];
        if jv.is_null() || pv.is_null() {
            result.push(Value::Null);
            continue;
        }
        let json_str = match jv.as_str() {
            Some(s) => s,
            None => {
                result.push(Value::Null);
                continue;
            }
        };
        let path_str = match pv.as_str() {
            Some(s) => s,
            None => {
                result.push(Value::Null);
                continue;
            }
        };
        let mut parsed = match serde_json::from_str::<serde_json::Value>(json_str) {
            Ok(v) => v,
            Err(_) => {
                result.push(Value::Null);
                continue;
            }
        };
        let path = parse_json_path(path_str);
        // 仅当路径存在时替换
        if json_path_exists(&parsed, &path) {
            json_set_path(&mut parsed, &path, value_to_json(vv), false);
        }
        result.push(json_to_value(&parsed));
    }
    Ok(Vector::Flat(result))
}

/// JSON_REMOVE(json, path) — 删除指定路径的字段
fn eval_json_remove(json_vec: &Vector, path_vec: &Vector) -> Result<Vector> {
    let json_flat = json_vec.to_flat();
    let path_flat = path_vec.to_flat();
    let len = json_flat.len();
    let mut result = Vec::with_capacity(len);
    for i in 0..len {
        let jv = &json_flat[i];
        let pv = &path_flat[i.min(path_flat.len() - 1)];
        if jv.is_null() || pv.is_null() {
            result.push(Value::Null);
            continue;
        }
        let json_str = match jv.as_str() {
            Some(s) => s,
            None => { result.push(Value::Null); continue; }
        };
        let path_str = match pv.as_str() {
            Some(s) => s,
            None => { result.push(Value::Null); continue; }
        };
        let mut parsed = match serde_json::from_str::<serde_json::Value>(json_str) {
            Ok(v) => v,
            Err(_) => { result.push(Value::Null); continue; }
        };
        let path = parse_json_path(path_str);
        json_remove_path(&mut parsed, &path);
        result.push(json_to_value(&parsed));
    }
    Ok(Vector::Flat(result))
}

/// JSON_REMOVE 辅助：删除指定路径的字段
fn json_remove_path(root: &mut serde_json::Value, path: &[serde_json::Value]) {
    if path.is_empty() {
        return;
    }
    let key = &path[0];
    let rest = &path[1..];
    if rest.is_empty() {
        // 最后一级：直接删除
        match root {
            serde_json::Value::Object(map) => {
                if let serde_json::Value::String(s) = key {
                    map.remove(s.as_str());
                }
            }
            serde_json::Value::Array(arr) => {
                if let Some(idx) = key.as_u64() {
                    let idx = idx as usize;
                    if idx < arr.len() {
                        arr.remove(idx);
                    }
                }
            }
            _ => {}
        }
    } else {
        // 递归到子节点
        match root {
            serde_json::Value::Object(map) => {
                let key_str = match key {
                    serde_json::Value::String(s) => s.clone(),
                    _ => key.to_string(),
                };
                if let Some(child) = map.get_mut(&key_str) {
                    json_remove_path(child, rest);
                }
            }
            serde_json::Value::Array(arr) => {
                if let Some(idx) = key.as_u64() {
                    let idx = idx as usize;
                    if idx < arr.len() {
                        json_remove_path(&mut arr[idx], rest);
                    }
                }
            }
            _ => {}
        }
    }
}
fn json_path_exists(root: &serde_json::Value, path: &[serde_json::Value]) -> bool {
    if path.is_empty() {
        return true;
    }
    let key = &path[0];
    let rest = &path[1..];
    match root {
        serde_json::Value::Object(map) => {
            let key_str = match key {
                serde_json::Value::String(s) => s.clone(),
                _ => return false,
            };
            if rest.is_empty() {
                map.contains_key(&key_str)
            } else {
                map.get(&key_str).map_or(false, |v| json_path_exists(v, rest))
            }
        }
        serde_json::Value::Array(arr) => {
            if let Some(idx) = key.as_u64() {
                let idx = idx as usize;
                arr.get(idx).map_or(false, |v| json_path_exists(v, rest))
            } else {
                false
            }
        }
        _ => false,
    }
}

// ============================================================================
// 向量函数（v0.12.0 新增，Agent 语义记忆 / RAG 场景）
// ============================================================================

fn eval_vector_distance(v1: &Vector, v2: &Vector) -> Result<Vector> {
    match (v1, v2) {
        (Vector::Constant(a, n), Vector::Constant(b, _)) => {
            Ok(Vector::Constant(vector_l2_distance(a, b), *n))
        }
        _ => {
            let f1 = v1.to_flat();
            let f2 = v2.to_flat();
            let len = f1.len();
            let mut result = Vec::with_capacity(len);
            for i in 0..len {
                let b = if f2.len() == 1 { &f2[0] } else { &f2[i] };
                result.push(vector_l2_distance(&f1[i], b));
            }
            Ok(Vector::Flat(result))
        }
    }
}

fn vector_l2_distance(a: &Value, b: &Value) -> Value {
    let va = match a {
        Value::Vector(v) => v.clone(),
        Value::VectorInt8(v) => v.iter().map(|x| *x as f32).collect(),
        Value::Null => return Value::Null,
        _ => return Value::Null,
    };
    let vb = match b {
        Value::Vector(v) => v.clone(),
        Value::VectorInt8(v) => v.iter().map(|x| *x as f32).collect(),
        Value::Null => return Value::Null,
        _ => return Value::Null,
    };
    if va.len() != vb.len() {
        return Value::Null;
    }
    let mut sum_sq = 0.0f64;
    for i in 0..va.len() {
        let d = va[i] as f64 - vb[i] as f64;
        sum_sq += d * d;
    }
    Value::Float64(sum_sq.sqrt())
}

fn eval_vector_cosine_similarity(v1: &Vector, v2: &Vector) -> Result<Vector> {
    match (v1, v2) {
        (Vector::Constant(a, n), Vector::Constant(b, _)) => {
            Ok(Vector::Constant(vector_cosine_sim(a, b), *n))
        }
        _ => {
            let f1 = v1.to_flat();
            let f2 = v2.to_flat();
            let len = f1.len();
            let mut result = Vec::with_capacity(len);
            for i in 0..len {
                let b = if f2.len() == 1 { &f2[0] } else { &f2[i] };
                result.push(vector_cosine_sim(&f1[i], b));
            }
            Ok(Vector::Flat(result))
        }
    }
}

fn vector_cosine_sim(a: &Value, b: &Value) -> Value {
    let va = match a {
        Value::Vector(v) => v.clone(),
        Value::VectorInt8(v) => v.iter().map(|x| *x as f32).collect(),
        Value::Null => return Value::Null,
        _ => return Value::Null,
    };
    let vb = match b {
        Value::Vector(v) => v.clone(),
        Value::VectorInt8(v) => v.iter().map(|x| *x as f32).collect(),
        Value::Null => return Value::Null,
        _ => return Value::Null,
    };
    if va.len() != vb.len() || va.is_empty() {
        return Value::Null;
    }
    let mut dot = 0.0f64;
    let mut norm_a = 0.0f64;
    let mut norm_b = 0.0f64;
    for i in 0..va.len() {
        let x = va[i] as f64;
        let y = vb[i] as f64;
        dot += x * y;
        norm_a += x * x;
        norm_b += y * y;
    }
    if norm_a == 0.0 || norm_b == 0.0 {
        return Value::Float64(0.0);
    }
    Value::Float64(dot / (norm_a.sqrt() * norm_b.sqrt()))
}

fn eval_vector_norm(v: &Vector) -> Result<Vector> {
    match v {
        Vector::Constant(val, n) => Ok(Vector::Constant(vector_norm_value(val), *n)),
        Vector::Flat(values) => {
            let result: Vec<Value> = values.iter().map(vector_norm_value).collect();
            Ok(Vector::Flat(result))
        }
    }
}

fn vector_norm_value(v: &Value) -> Value {
    match v {
        Value::Vector(vec) => {
            let mut sum_sq = 0.0f64;
            for x in vec {
                let f = *x as f64;
                sum_sq += f * f;
            }
            Value::Float64(sum_sq.sqrt())
        }
        Value::VectorInt8(vec) => {
            let mut sum_sq = 0.0f64;
            for x in vec {
                let f = *x as f64;
                sum_sq += f * f;
            }
            Value::Float64(sum_sq.sqrt())
        }
        Value::Null => Value::Null,
        _ => Value::Null,
    }
}

// ============================================================================
// 测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::ast::Expression;

    fn make_chunk(values: Vec<Value>) -> DataChunk {
        DataChunk {
            columns: vec![Vector::Flat(values)],
            count: 5,
        }
    }

    #[test]
    fn test_literal_eval() {
        let chunk = make_chunk(vec![
            Value::Int64(1), Value::Int64(2), Value::Int64(3),
            Value::Int64(4), Value::Int64(5),
        ]);
        let result = eval_vectorized(
            &Expression::Literal(Value::Int64(42)),
            &chunk,
            &["col1".to_string()],
        ).unwrap();
        assert_eq!(result.len(), 5);
        assert!(matches!(result, Vector::Constant(Value::Int64(42), 5)));
    }

    #[test]
    fn test_column_ref_eval() {
        let chunk = make_chunk(vec![
            Value::Int64(1), Value::Int64(2), Value::Int64(3),
            Value::Int64(4), Value::Int64(5),
        ]);
        let result = eval_vectorized(
            &Expression::ColumnRef { table: None, column: "col1".to_string() },
            &chunk,
            &["col1".to_string()],
        ).unwrap();
        assert_eq!(result.len(), 5);
    }

    #[test]
    fn test_binary_arith() {
        let chunk = make_chunk(vec![
            Value::Int64(10), Value::Int64(20), Value::Int64(30),
            Value::Int64(40), Value::Int64(50),
        ]);
        let expr = Expression::BinaryOp {
            left: Box::new(Expression::ColumnRef { table: None, column: "col1".to_string() }),
            op: BinaryOperator::Plus,
            right: Box::new(Expression::Literal(Value::Int64(5))),
        };
        let result = eval_vectorized(&expr, &chunk, &["col1".to_string()]).unwrap();
        let flat = result.to_flat();
        assert_eq!(flat[0], Value::Int64(15));
        assert_eq!(flat[1], Value::Int64(25));
        assert_eq!(flat[2], Value::Int64(35));
    }

    #[test]
    fn test_binary_compare() {
        let chunk = make_chunk(vec![
            Value::Int64(1), Value::Int64(5), Value::Int64(10),
            Value::Int64(15), Value::Int64(20),
        ]);
        let expr = Expression::BinaryOp {
            left: Box::new(Expression::ColumnRef { table: None, column: "col1".to_string() }),
            op: BinaryOperator::Gt,
            right: Box::new(Expression::Literal(Value::Int64(10))),
        };
        let result = eval_vectorized(&expr, &chunk, &["col1".to_string()]).unwrap();
        let flat = result.to_flat();
        assert_eq!(flat[0], Value::Boolean(false));
        assert_eq!(flat[2], Value::Boolean(false));
        assert_eq!(flat[3], Value::Boolean(true));
        assert_eq!(flat[4], Value::Boolean(true));
    }

    #[test]
    fn test_null_propagation() {
        let chunk = DataChunk {
            columns: vec![Vector::Flat(vec![
                Value::Int64(1), Value::Null, Value::Int64(3),
            ])],
            count: 3,
        };
        let expr = Expression::BinaryOp {
            left: Box::new(Expression::ColumnRef { table: None, column: "col1".to_string() }),
            op: BinaryOperator::Plus,
            right: Box::new(Expression::Literal(Value::Int64(10))),
        };
        let result = eval_vectorized(&expr, &chunk, &["col1".to_string()]).unwrap();
        let flat = result.to_flat();
        assert_eq!(flat[0], Value::Int64(11));
        assert_eq!(flat[1], Value::Null);
        assert_eq!(flat[2], Value::Int64(13));
    }

    #[test]
    fn test_is_null() {
        let chunk = DataChunk {
            columns: vec![Vector::Flat(vec![
                Value::Int64(1), Value::Null, Value::Varchar("hello".into()),
            ])],
            count: 3,
        };
        let expr = Expression::IsNull(Box::new(
            Expression::ColumnRef { table: None, column: "col1".to_string() }
        ));
        let result = eval_vectorized(&expr, &chunk, &["col1".to_string()]).unwrap();
        let flat = result.to_flat();
        assert_eq!(flat[0], Value::Boolean(false));
        assert_eq!(flat[1], Value::Boolean(true));
        assert_eq!(flat[2], Value::Boolean(false));
    }

    #[test]
    fn test_like_pattern() {
        assert!(like_pattern_match("hello", "%ell%"));
        assert!(like_pattern_match("hello", "h%o"));
        assert!(like_pattern_match("hello", "h_ll_"));
        assert!(!like_pattern_match("hello", "h%x"));
        assert!(like_pattern_match("abc", "%%"));
        assert!(like_pattern_match("", "%"));
    }

    #[test]
    fn test_boolean_to_selection() {
        let bool_vec = Vector::Flat(vec![
            Value::Boolean(true),
            Value::Boolean(false),
            Value::Boolean(true),
            Value::Null,
            Value::Boolean(true),
        ]);
        let selection = boolean_to_selection(&bool_vec);
        assert_eq!(selection, vec![0, 2, 4]);
    }

    // --- JSON 函数测试（v0.12.0 新增）---

    #[test]
    fn test_json_extract_simple() {
        let json = Value::Json(r#"{"name":"alice","age":30}"#.to_string());
        let path = Value::Varchar("$.name".to_string());
        let result = json_extract_value(&json, &path);
        assert_eq!(result, Value::Json("\"alice\"".to_string()));
    }

    #[test]
    fn test_json_extract_nested() {
        let json = Value::Json(r#"{"user":{"name":"bob","scores":[90,85,95]}}"#.to_string());
        let result = json_extract_value(&json, &Value::Varchar("$.user.name".to_string()));
        assert_eq!(result, Value::Json("\"bob\"".to_string()));
    }

    #[test]
    fn test_json_extract_array_index() {
        let json = Value::Json(r#"{"items":["a","b","c"]}"#.to_string());
        let result = json_extract_value(&json, &Value::Varchar("$.items[1]".to_string()));
        assert_eq!(result, Value::Json("\"b\"".to_string()));
    }

    #[test]
    fn test_json_extract_null() {
        let result = json_extract_value(&Value::Null, &Value::Varchar("$.x".to_string()));
        assert_eq!(result, Value::Null);
        let result = json_extract_value(&Value::Json("{}".to_string()), &Value::Varchar("$.nonexistent".to_string()));
        assert_eq!(result, Value::Null);
    }

    #[test]
    fn test_json_contains_basic() {
        let json = Value::Json(r#"{"a":1,"b":2}"#.to_string());
        let target = Value::Json("1".to_string());
        let result = json_contains_value(&json, &target, None);
        assert_eq!(result, Value::Boolean(true));
    }

    #[test]
    fn test_json_contains_with_path() {
        let json = Value::Json(r#"{"data":{"tags":["rust","db","ai"]}}"#.to_string());
        let target = Value::Json("\"ai\"".to_string());
        let result = json_contains_value(&json, &target, Some(&Value::Varchar("$.data.tags".to_string())));
        assert_eq!(result, Value::Boolean(true));
    }

    #[test]
    fn test_json_array_length() {
        let json = Value::Json(r#"[1,2,3,4,5]"#.to_string());
        let result = json_array_length_value(&json, None);
        assert_eq!(result, Value::Int64(5));
    }

    #[test]
    fn test_json_array_length_with_path() {
        let json = Value::Json(r#"{"items":[10,20,30]}"#.to_string());
        let result = json_array_length_value(&json, Some(&Value::Varchar("$.items".to_string())));
        assert_eq!(result, Value::Int64(3));
    }

    #[test]
    fn test_json_array_length_not_array() {
        let json = Value::Json(r#"{"key":"value"}"#.to_string());
        let result = json_array_length_value(&json, None);
        assert_eq!(result, Value::Null);
    }

    // --- 向量函数测试（v0.12.0 新增）---

    #[test]
    fn test_vector_l2_distance() {
        let v1 = Value::Vector(vec![1.0, 2.0, 3.0]);
        let v2 = Value::Vector(vec![4.0, 6.0, 8.0]);
        let result = vector_l2_distance(&v1, &v2);
        // distance = sqrt((3)^2 + (4)^2 + (5)^2) = sqrt(50) ≈ 7.071
        match result {
            Value::Float64(d) => assert!((d - 7.0710678).abs() < 0.001),
            _ => panic!("expected Float64"),
        }
    }

    #[test]
    fn test_vector_l2_distance_same() {
        let v = Value::Vector(vec![1.0, 2.0, 3.0]);
        let result = vector_l2_distance(&v, &v);
        match result {
            Value::Float64(d) => assert_eq!(d, 0.0),
            _ => panic!("expected Float64"),
        }
    }

    #[test]
    fn test_vector_cosine_similarity() {
        let v1 = Value::Vector(vec![1.0, 0.0]);
        let v2 = Value::Vector(vec![0.0, 1.0]);
        let result = vector_cosine_sim(&v1, &v2);
        match result {
            Value::Float64(s) => assert_eq!(s, 0.0),
            _ => panic!("expected Float64"),
        }
    }

    #[test]
    fn test_vector_cosine_similarity_same_direction() {
        let v1 = Value::Vector(vec![3.0, 4.0]);
        let v2 = Value::Vector(vec![6.0, 8.0]);
        let result = vector_cosine_sim(&v1, &v2);
        match result {
            Value::Float64(s) => assert!((s - 1.0).abs() < 0.0001),
            _ => panic!("expected Float64"),
        }
    }

    #[test]
    fn test_vector_norm() {
        let v = Value::Vector(vec![3.0, 4.0]);
        let result = vector_norm_value(&v);
        match result {
            Value::Float64(n) => assert_eq!(n, 5.0),
            _ => panic!("expected Float64"),
        }
    }

    #[test]
    fn test_vector_null_propagation() {
        let v = Value::Vector(vec![1.0, 2.0]);
        assert_eq!(vector_l2_distance(&Value::Null, &v), Value::Null);
        assert_eq!(vector_l2_distance(&v, &Value::Null), Value::Null);
        assert_eq!(vector_cosine_sim(&Value::Null, &v), Value::Null);
        assert_eq!(vector_norm_value(&Value::Null), Value::Null);
    }

    #[test]
    fn test_vector_dim_mismatch() {
        let v1 = Value::Vector(vec![1.0, 2.0]);
        let v2 = Value::Vector(vec![1.0, 2.0, 3.0]);
        assert_eq!(vector_l2_distance(&v1, &v2), Value::Null);
        assert_eq!(vector_cosine_sim(&v1, &v2), Value::Null);
    }

    // ============ v0.13.0 新增函数测试 ============

    fn func_expr(name: &str, args: Vec<Expression>) -> Expression {
        Expression::Function {
            name: name.to_string(),
            args,
            distinct: false,
            count_star: false,
            over: None,
        }
    }

    #[test]
    fn test_ifnull_first_non_null() {
        let chunk = DataChunk {
            columns: vec![Vector::Flat(vec![
                Value::Int64(1), Value::Null, Value::Int64(3),
            ])],
            count: 3,
        };
        let expr = func_expr("IFNULL", vec![
            Expression::ColumnRef { table: None, column: "c".to_string() },
            Expression::Literal(Value::Int64(99)),
        ]);
        let r = eval_vectorized(&expr, &chunk, &["c".to_string()]).unwrap();
        let flat = r.to_flat();
        assert_eq!(flat[0], Value::Int64(1));
        assert_eq!(flat[1], Value::Int64(99));
        assert_eq!(flat[2], Value::Int64(3));
    }

    #[test]
    fn test_replace_basic() {
        let chunk = DataChunk {
            columns: vec![Vector::Flat(vec![
                Value::Varchar("hello world".into()),
                Value::Varchar("foo bar".into()),
            ])],
            count: 2,
        };
        let expr = func_expr("REPLACE", vec![
            Expression::ColumnRef { table: None, column: "c".to_string() },
            Expression::Literal(Value::Varchar("world".into())),
            Expression::Literal(Value::Varchar("there".into())),
        ]);
        let r = eval_vectorized(&expr, &chunk, &["c".to_string()]).unwrap();
        let flat = r.to_flat();
        assert_eq!(flat[0], Value::Varchar("hello there".into()));
        assert_eq!(flat[1], Value::Varchar("foo bar".into())); // no match
    }

    #[test]
    fn test_mod_basic() {
        let chunk = DataChunk {
            columns: vec![Vector::Flat(vec![
                Value::Int64(10), Value::Int64(7), Value::Int64(100),
            ])],
            count: 3,
        };
        let expr = func_expr("MOD", vec![
            Expression::ColumnRef { table: None, column: "c".to_string() },
            Expression::Literal(Value::Int64(3)),
        ]);
        let r = eval_vectorized(&expr, &chunk, &["c".to_string()]).unwrap();
        let flat = r.to_flat();
        assert_eq!(flat[0], Value::Int64(1));
        assert_eq!(flat[1], Value::Int64(1));
        assert_eq!(flat[2], Value::Int64(1));
    }

    #[test]
    fn test_mod_by_zero_returns_null() {
        let chunk = DataChunk {
            columns: vec![Vector::Flat(vec![Value::Int64(10), Value::Int64(7)])],
            count: 2,
        };
        let expr = func_expr("MOD", vec![
            Expression::ColumnRef { table: None, column: "c".to_string() },
            Expression::Literal(Value::Int64(0)),
        ]);
        let r = eval_vectorized(&expr, &chunk, &["c".to_string()]).unwrap();
        let flat = r.to_flat();
        assert_eq!(flat[0], Value::Null);
        assert_eq!(flat[1], Value::Null);
    }

    #[test]
    fn test_now() {
        let chunk = DataChunk { columns: vec![], count: 1 };
        let expr = func_expr("NOW", vec![]);
        let r = eval_vectorized(&expr, &chunk, &["".to_string()]).unwrap();
        let flat = r.to_flat();
        // 应该是 Timestamp 且接近当前时间
        if let Value::Timestamp(ts) = flat[0] {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as i64;
            assert!((ts - now).abs() < 2000, "NOW should be within 2 seconds of actual time");
        } else {
            panic!("NOW should return Timestamp");
        }
    }

    #[test]
    fn test_current_timestamp_alias() {
        let chunk = DataChunk { columns: vec![], count: 1 };
        let expr = func_expr("CURRENT_TIMESTAMP", vec![]);
        let r = eval_vectorized(&expr, &chunk, &["".to_string()]).unwrap();
        let flat = r.to_flat();
        assert!(matches!(flat[0], Value::Timestamp(_)));
    }

    #[test]
    fn test_date() {
        let chunk = DataChunk {
            columns: vec![Vector::Flat(vec![
                Value::Timestamp(0),                           // 1970-01-01
                Value::Timestamp(86400000),                    // 1970-01-02
                Value::Timestamp(1735689600000),               // 2025-01-01
                Value::Timestamp(1759536000000),               // 2025-10-04
                Value::Timestamp(-86400000),                   // 1969-12-31
            ])],
            count: 5,
        };
        let expr = func_expr("DATE", vec![Expression::ColumnRef { table: None, column: "c".to_string() }]);
        let r = eval_vectorized(&expr, &chunk, &["c".to_string()]).unwrap();
        let flat = r.to_flat();
        assert_eq!(flat[0], Value::Varchar("1970-01-01".to_string()));
        assert_eq!(flat[1], Value::Varchar("1970-01-02".to_string()));
        assert_eq!(flat[2], Value::Varchar("2025-01-01".to_string()));
        assert_eq!(flat[3], Value::Varchar("2025-10-04".to_string()));
        assert_eq!(flat[4], Value::Varchar("1969-12-31".to_string()));
    }

    #[test]
    fn test_strftime() {
        let chunk = DataChunk {
            columns: vec![Vector::Flat(vec![
                Value::Timestamp(0),
                Value::Timestamp(1735689600000),
                Value::Timestamp(1759536000000),
            ])],
            count: 3,
        };
        let expr = func_expr("STRFTIME", vec![
            Expression::Literal(Value::Varchar("%Y-%m-%d %H:%M:%S".to_string())),
            Expression::ColumnRef { table: None, column: "c".to_string() },
        ]);
        let r = eval_vectorized(&expr, &chunk, &["c".to_string()]).unwrap();
        let flat = r.to_flat();
        assert_eq!(flat[0], Value::Varchar("1970-01-01 00:00:00".to_string()));
        assert_eq!(flat[2], Value::Varchar("2025-10-04 00:00:00".to_string()));
    }

    #[test]
    fn test_strftime_format_codes() {
        // 1735689600000 = 2025-01-01 00:00:00 UTC (星期三, wday=3)
        let ts = 1735689600000i64;
        let chunk = DataChunk {
            columns: vec![Vector::Constant(Value::Timestamp(ts), 1)],
            count: 1,
        };
        let test_cases = vec![
            ("%Y", "2025"),
            ("%y", "25"),
            ("%m", "01"),
            ("%d", "01"),
            ("%H", "00"),
            ("%M", "00"),
            ("%S", "00"),
        ];
        for (fmt, expected) in test_cases {
            let expr = func_expr("STRFTIME", vec![
                Expression::Literal(Value::Varchar(fmt.to_string())),
                Expression::ColumnRef { table: None, column: "c".to_string() },
            ]);
            let r = eval_vectorized(&expr, &chunk, &["c".to_string()]).unwrap();
            let flat = r.to_flat();
            assert_eq!(flat[0], Value::Varchar(expected.to_string()), "fmt={}", fmt);
        }
    }

    #[test]
    fn test_time() {
        let chunk = DataChunk {
            columns: vec![Vector::Flat(vec![
                Value::Timestamp(0),                           // 00:00:00
                Value::Timestamp(3723000),                     // 01:02:03
                Value::Timestamp(86399000),                    // 23:59:59
            ])],
            count: 3,
        };
        let expr = func_expr("TIME", vec![Expression::ColumnRef { table: None, column: "c".to_string() }]);
        let r = eval_vectorized(&expr, &chunk, &["c".to_string()]).unwrap();
        let flat = r.to_flat();
        assert_eq!(flat[0], Value::Varchar("00:00:00".to_string()));
        assert_eq!(flat[1], Value::Varchar("01:02:03".to_string()));
        assert_eq!(flat[2], Value::Varchar("23:59:59".to_string()));
    }

    #[test]
    fn test_datetime() {
        let chunk = DataChunk {
            columns: vec![Vector::Flat(vec![
                Value::Timestamp(0),
                Value::Timestamp(1735689600000),
                Value::Timestamp(1759536000000),
            ])],
            count: 3,
        };
        let expr = func_expr("DATETIME", vec![Expression::ColumnRef { table: None, column: "c".to_string() }]);
        let r = eval_vectorized(&expr, &chunk, &["c".to_string()]).unwrap();
        let flat = r.to_flat();
        assert_eq!(flat[0], Value::Varchar("1970-01-01 00:00:00".to_string()));
        assert_eq!(flat[1], Value::Varchar("2025-01-01 00:00:00".to_string()));
        assert_eq!(flat[2], Value::Varchar("2025-10-04 00:00:00".to_string()));
    }

    #[test]
    fn test_date_add() {
        let chunk = DataChunk { columns: vec![], count: 1 };
        let ts = Value::Timestamp(0);
        let expr = func_expr("DATE_ADD", vec![
            Expression::Literal(ts),
            Expression::Literal(Value::Int64(7)),
            Expression::Literal(Value::Varchar("day".to_string())),
        ]);
        let r = eval_vectorized(&expr, &chunk, &["".to_string()]).unwrap();
        let flat = r.to_flat();
        assert_eq!(flat[0], Value::Timestamp(7 * 86400000));
    }

    #[test]
    fn test_date_sub() {
        let chunk = DataChunk { columns: vec![], count: 1 };
        let ts = Value::Timestamp(10 * 86400000);
        let expr = func_expr("DATE_SUB", vec![
            Expression::Literal(ts),
            Expression::Literal(Value::Int64(3)),
            Expression::Literal(Value::Varchar("day".to_string())),
        ]);
        let r = eval_vectorized(&expr, &chunk, &["".to_string()]).unwrap();
        let flat = r.to_flat();
        assert_eq!(flat[0], Value::Timestamp(7 * 86400000));
    }

    #[test]
    fn test_date_diff() {
        let chunk = DataChunk { columns: vec![], count: 1 };
        let expr = func_expr("DATE_DIFF", vec![
            Expression::Literal(Value::Timestamp(10 * 86400000)),
            Expression::Literal(Value::Timestamp(0)),
            Expression::Literal(Value::Varchar("day".to_string())),
        ]);
        let r = eval_vectorized(&expr, &chunk, &["".to_string()]).unwrap();
        let flat = r.to_flat();
        assert_eq!(flat[0], Value::Int64(10));
    }

    #[test]
    fn test_date_add_hours() {
        let chunk = DataChunk { columns: vec![], count: 1 };
        let ts = Value::Timestamp(0);
        let expr = func_expr("DATE_ADD", vec![
            Expression::Literal(ts),
            Expression::Literal(Value::Int64(48)),
            Expression::Literal(Value::Varchar("hour".to_string())),
        ]);
        let r = eval_vectorized(&expr, &chunk, &["".to_string()]).unwrap();
        let flat = r.to_flat();
        assert_eq!(flat[0], Value::Timestamp(2 * 86400000));
    }

    #[test]
    fn test_date_trunc() {
        let chunk = DataChunk { columns: vec![], count: 1 };
        // 2024-06-15 14:30:45 UTC = 1718453445000 ms
        let ts = Value::Timestamp(1718453445000i64);
        // TRUNC to day: 2024-06-15 00:00:00 UTC
        let expr = func_expr("DATE_TRUNC", vec![
            Expression::Literal(Value::Varchar("day".to_string())),
            Expression::Literal(ts.clone()),
        ]);
        let r = eval_vectorized(&expr, &chunk, &["".to_string()]).unwrap();
        let flat = r.to_flat();
        // 2024-06-15 00:00:00 UTC = 1718409600000 ms (days since epoch * 86400000)
        let expected = (1718409600000i64 / 86400000) * 86400000;
        assert_eq!(flat[0], Value::Timestamp(expected));

        // TRUNC to month: 2024-06-01 00:00:00 UTC
        let expr = func_expr("DATE_TRUNC", vec![
            Expression::Literal(Value::Varchar("month".to_string())),
            Expression::Literal(ts.clone()),
        ]);
        let r = eval_vectorized(&expr, &chunk, &["".to_string()]).unwrap();
        let flat = r.to_flat();
        assert_eq!(flat[0], Value::Timestamp(1717200000000i64));
    }

    #[test]
    fn test_strptime() {
        let chunk = DataChunk { columns: vec![], count: 1 };
        // STRPTIME('%Y-%m-%d', '2024-06-15') → Timestamp
        let expr = func_expr("STRPTIME", vec![
            Expression::Literal(Value::Varchar("%Y-%m-%d".to_string())),
            Expression::Literal(Value::Varchar("2024-06-15".to_string())),
        ]);
        let r = eval_vectorized(&expr, &chunk, &["".to_string()]).unwrap();
        let flat = r.to_flat();
        // 2024-06-15 00:00:00 UTC = 1718409600000 ms
        assert_eq!(flat[0], Value::Timestamp(1718409600000i64));
    }
}
