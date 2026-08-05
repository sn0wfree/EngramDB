//! 用户定义函数（UDF - User-Defined Functions）
//!
//! 支持在 SQL 中调用外部注册的函数，扩展数据库能力。
//!
//! 设计要点：
//! - 函数注册表：运行时动态注册/注销函数
//! - 标量 UDF：输入一行 → 输出一个值
//! - 向量化执行：批量调用，避免逐行开销
//! - 类型安全：注册时声明参数类型和返回类型
//! - 多语言支持：Rust 原生 / Python / WASM（框架预留）
//!
//! 与内置函数的关系：
//! - 内置函数在 expression.rs 中硬编码实现
//! - UDF 通过注册表动态查找，走通用调用路径
//! - 性能上内置函数更优，UDF 更灵活

use crate::common::error::{EngramDbError as DbError, Result};
use crate::executor::vector::Vector;
use crate::Value;

/// UDF 参数类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UdfType {
    Boolean,
    Int32,
    Int64,
    Float64,
    Varchar,
    Any, // 接受任意类型
}

/// UDF 签名（参数类型 + 返回类型）
#[derive(Debug, Clone)]
pub struct UdfSignature {
    pub arg_types: Vec<UdfType>,
    pub return_type: UdfType,
    pub is_variadic: bool, // 是否可变参数（最后一个参数类型可重复）
}

/// UDF 类型（按执行方式分类）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UdfKind {
    /// 标量函数（一行输入 → 一个输出值）
    Scalar,
    /// 聚合函数（多行输入 → 一个输出值）
    Aggregate,
    /// 表函数（一行输入 → 多行输出）
    Table,
}

/// 标量 UDF 的执行函数签名
///
/// 接受一批参数向量（每个参数一个 Vector），返回一个结果 Vector。
/// 向量化接口：一次调用处理整个 batch，避免逐行调用开销。
pub type ScalarUdfFn = fn(&[Vector]) -> Result<Vector>;

/// UDF 定义
pub struct UserDefinedFunction {
    /// 函数名（SQL 中使用的名称，不区分大小写存储为小写）
    pub name: String,
    /// 函数类型
    pub kind: UdfKind,
    /// 函数签名
    pub signature: UdfSignature,
    /// 标量函数的执行体（kind=Scalar 时有效）
    pub scalar_fn: Option<ScalarUdfFn>,
    /// 描述（用于 EXPLAIN / 错误信息）
    pub description: String,
}

impl std::fmt::Debug for UserDefinedFunction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UserDefinedFunction")
            .field("name", &self.name)
            .field("kind", &self.kind)
            .field("signature", &self.signature)
            .field("description", &self.description)
            .finish()
    }
}

impl UserDefinedFunction {
    /// 创建标量 UDF
    pub fn scalar(
        name: &str,
        arg_types: Vec<UdfType>,
        return_type: UdfType,
        func: ScalarUdfFn,
        description: &str,
    ) -> Self {
        Self {
            name: name.to_lowercase(),
            kind: UdfKind::Scalar,
            signature: UdfSignature {
                arg_types,
                return_type,
                is_variadic: false,
            },
            scalar_fn: Some(func),
            description: description.to_string(),
        }
    }

    /// 创建可变参数标量 UDF
    pub fn scalar_variadic(
        name: &str,
        fixed_arg_types: Vec<UdfType>,
        variadic_type: UdfType,
        return_type: UdfType,
        func: ScalarUdfFn,
        description: &str,
    ) -> Self {
        let mut arg_types = fixed_arg_types;
        arg_types.push(variadic_type);
        Self {
            name: name.to_lowercase(),
            kind: UdfKind::Scalar,
            signature: UdfSignature {
                arg_types,
                return_type,
                is_variadic: true,
            },
            scalar_fn: Some(func),
            description: description.to_string(),
        }
    }

    /// 检查参数数量是否匹配
    pub fn check_arg_count(&self, num_args: usize) -> bool {
        if self.signature.is_variadic {
            num_args >= self.signature.arg_types.len().saturating_sub(1)
        } else {
            num_args == self.signature.arg_types.len()
        }
    }
}

/// UDF 注册表
///
/// 管理所有已注册的用户定义函数。
/// 支持按名称查找、动态注册/注销。
#[derive(Debug, Default)]
pub struct UdfRegistry {
    functions: std::collections::HashMap<String, UserDefinedFunction>,
}

impl UdfRegistry {
    /// 创建空注册表
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册 UDF
    ///
    /// 如果同名函数已存在，返回错误（避免意外覆盖）。
    pub fn register(&mut self, udf: UserDefinedFunction) -> Result<()> {
        let name = udf.name.clone();
        if self.functions.contains_key(&name) {
            return Err(DbError::Internal(
                format!("function '{}' already registered", name)
            ));
        }
        self.functions.insert(name, udf);
        Ok(())
    }

    /// 注销 UDF
    pub fn unregister(&mut self, name: &str) -> Option<UserDefinedFunction> {
        self.functions.remove(&name.to_lowercase())
    }

    /// 查找 UDF（不区分大小写）
    pub fn get(&self, name: &str) -> Option<&UserDefinedFunction> {
        self.functions.get(&name.to_lowercase())
    }

    /// 列出所有已注册函数名
    pub fn list_names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.functions.keys().map(|s| s.as_str()).collect();
        names.sort();
        names
    }

    /// 执行标量 UDF
    ///
    /// # 参数
    /// - `name`: 函数名
    /// - `args`: 参数向量数组（每个参数一个 Vector）
    ///
    /// # 返回
    /// 结果向量
    pub fn call_scalar(&self, name: &str, args: &[Vector]) -> Result<Vector> {
        let udf = self.get(name)
            .ok_or_else(|| DbError::Internal(
                format!("function '{}' not found", name)
            ))?;

        if udf.kind != UdfKind::Scalar {
            return Err(DbError::Internal(
                format!("function '{}' is not a scalar function", name)
            ));
        }

        if !udf.check_arg_count(args.len()) {
            return Err(DbError::Internal(
                format!(
                    "function '{}' expects {} arguments, got {}",
                    name,
                    if udf.signature.is_variadic {
                        format!("at least {}", udf.signature.arg_types.len().saturating_sub(1))
                    } else {
                        udf.signature.arg_types.len().to_string()
                    },
                    args.len()
                )
            ));
        }

        let func = udf.scalar_fn
            .ok_or_else(|| DbError::Internal(
                format!("function '{}' has no implementation", name)
            ))?;

        func(args)
    }
}

// ============================================================
// 示例 UDF（演示如何注册和使用）
// ============================================================

/// 示例：字符串长度函数（演示 UDF 注册）
///
/// 接受一个 VARCHAR 参数，返回其长度（Int64）。
pub fn example_strlen(args: &[Vector]) -> Result<Vector> {
    if args.len() != 1 {
        return Err(DbError::Internal("strlen expects 1 argument".to_string()));
    }

    let input = &args[0];
    let mut result = Vector::new();
    for i in 0..input.len() {
        match input.get(i) {
            Value::Varchar(s) => result.push(Value::Int64(s.len() as i64)),
            Value::Null => result.push(Value::Null),
            _ => return Err(DbError::Internal("strlen expects VARCHAR argument".to_string())),
        }
    }

    Ok(result)
}

/// 示例：数值平方函数
pub fn example_square(args: &[Vector]) -> Result<Vector> {
    if args.len() != 1 {
        return Err(DbError::Internal("square expects 1 argument".to_string()));
    }

    let input = &args[0];
    let mut result = Vector::new();
    for i in 0..input.len() {
        match input.get(i) {
            Value::Int32(v) => result.push(Value::Int64(v as i64 * v as i64)),
            Value::Int64(v) => result.push(Value::Int64(v * v)),
            Value::Float64(v) => result.push(Value::Float64(v * v)),
            Value::Null => result.push(Value::Null),
            _ => return Err(DbError::Internal("square expects numeric argument".to_string())),
        }
    }

    Ok(result)
}

/// 注册内置示例 UDF 到注册表
pub fn register_example_udfs(registry: &mut UdfRegistry) -> Result<()> {
    registry.register(UserDefinedFunction::scalar(
        "strlen",
        vec![UdfType::Varchar],
        UdfType::Int64,
        example_strlen,
        "Returns the length of a string",
    ))?;

    registry.register(UserDefinedFunction::scalar(
        "square",
        vec![UdfType::Any],
        UdfType::Any,
        example_square,
        "Returns the square of a number",
    ))?;

    Ok(())
}

// ============================================================
// 测试
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_udf_registry_register_and_get() {
        let mut registry = UdfRegistry::new();
        assert_eq!(registry.list_names().len(), 0);

        register_example_udfs(&mut registry).unwrap();
        assert_eq!(registry.list_names().len(), 2);

        assert!(registry.get("strlen").is_some());
        assert!(registry.get("STRLEN").is_some()); // 大小写不敏感
        assert!(registry.get("square").is_some());
        assert!(registry.get("nonexistent").is_none());
    }

    #[test]
    fn test_udf_registry_duplicate() {
        let mut registry = UdfRegistry::new();
        register_example_udfs(&mut registry).unwrap();

        let result = registry.register(UserDefinedFunction::scalar(
            "strlen",
            vec![UdfType::Varchar],
            UdfType::Int64,
            example_strlen,
            "duplicate",
        ));
        assert!(result.is_err());
    }

    #[test]
    fn test_udf_call_strlen() {
        let mut registry = UdfRegistry::new();
        register_example_udfs(&mut registry).unwrap();

        let input = Vector::from_values(vec![
            Value::Varchar("hello".to_string()),
            Value::Varchar("".to_string()),
            Value::Null,
            Value::Varchar("world!".to_string()),
        ]);

        let result = registry.call_scalar("strlen", &[input]).unwrap();
        assert_eq!(result.len(), 4);
        assert_eq!(result.get(0), Value::Int64(5));
        assert_eq!(result.get(1), Value::Int64(0));
        assert_eq!(result.get(2), Value::Null);
        assert_eq!(result.get(3), Value::Int64(6));
    }

    #[test]
    fn test_udf_call_square() {
        let mut registry = UdfRegistry::new();
        register_example_udfs(&mut registry).unwrap();

        let input = Vector::from_values(vec![
            Value::Int32(3),
            Value::Int64(5),
            Value::Float64(2.5),
            Value::Null,
        ]);

        let result = registry.call_scalar("square", &[input]).unwrap();
        assert_eq!(result.len(), 4);
        assert_eq!(result.get(0), Value::Int64(9));
        assert_eq!(result.get(1), Value::Int64(25));
        match result.get(2) {
            Value::Float64(v) => assert!((v - 6.25).abs() < 0.001),
            _ => panic!("expected float64"),
        }
        assert_eq!(result.get(3), Value::Null);
    }

    #[test]
    fn test_udf_arg_count_check() {
        let udf = UserDefinedFunction::scalar(
            "add",
            vec![UdfType::Int64, UdfType::Int64],
            UdfType::Int64,
            |_| unimplemented!(),
            "add two numbers",
        );
        assert!(udf.check_arg_count(2));
        assert!(!udf.check_arg_count(1));
        assert!(!udf.check_arg_count(3));
    }

    #[test]
    fn test_udf_variadic() {
        let udf = UserDefinedFunction::scalar_variadic(
            "concat",
            vec![],
            UdfType::Varchar,
            UdfType::Varchar,
            |_| unimplemented!(),
            "concatenate strings",
        );
        assert!(udf.check_arg_count(0));
        assert!(udf.check_arg_count(1));
        assert!(udf.check_arg_count(5));
        assert!(udf.signature.is_variadic);
    }

    #[test]
    fn test_udf_unregister() {
        let mut registry = UdfRegistry::new();
        register_example_udfs(&mut registry).unwrap();
        assert_eq!(registry.list_names().len(), 2);

        let removed = registry.unregister("strlen");
        assert!(removed.is_some());
        assert_eq!(registry.list_names().len(), 1);
        assert!(registry.get("strlen").is_none());

        let removed_again = registry.unregister("strlen");
        assert!(removed_again.is_none());
    }

    #[test]
    fn test_udf_kind_check() {
        let mut registry = UdfRegistry::new();
        register_example_udfs(&mut registry).unwrap();

        let udf = registry.get("square").unwrap();
        assert_eq!(udf.kind, UdfKind::Scalar);
        assert!(udf.scalar_fn.is_some());
        assert!(!udf.description.is_empty());
    }
}
