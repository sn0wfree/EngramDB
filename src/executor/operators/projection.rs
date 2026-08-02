//! 投影算子
//!
//! 向量化投影：支持任意表达式计算，每列独立向量化求值。
//! 与优化器的投影下推（Projection Pushdown）配合，减少 IO 和计算量。

use crate::common::error::Result;
use crate::sql::ast::Expression;

use super::super::vector::DataChunk;
use super::super::expression::eval_vectorized;

/// 执行投影（支持表达式计算）
///
/// 对每个表达式向量化求值，结果组成新的 DataChunk。
/// 输入列名用于解析 ColumnRef，输出列名由调用方指定。
pub fn execute(
    input: &[DataChunk],
    expressions: &[Expression],
    column_names: &[String],
) -> Result<Vec<DataChunk>> {
    let mut result = Vec::new();

    for chunk in input {
        let projected = project_chunk(chunk, expressions, column_names)?;
        if !projected.is_empty() {
            result.push(projected);
        }
    }

    Ok(result)
}

/// 对单个 DataChunk 做投影计算
fn project_chunk(
    chunk: &DataChunk,
    expressions: &[Expression],
    column_names: &[String],
) -> Result<DataChunk> {
    let mut columns = Vec::with_capacity(expressions.len());

    for expr in expressions {
        let vec = eval_vectorized(expr, chunk, column_names)?;
        columns.push(vec);
    }

    Ok(DataChunk {
        count: chunk.count,
        columns,
    })
}

// ============================================================================
// 测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::ast::{Expression, BinaryOperator};
    use crate::Value;
    use crate::executor::vector::{Vector, DataChunk};

    fn make_test_chunk() -> DataChunk {
        let a = Vector::Flat(vec![
            Value::Int64(10), Value::Int64(20), Value::Int64(30),
            Value::Int64(40), Value::Int64(50),
        ]);
        let b = Vector::Flat(vec![
            Value::Int64(1), Value::Int64(2), Value::Int64(3),
            Value::Int64(4), Value::Int64(5),
        ]);
        DataChunk {
            columns: vec![a, b],
            count: 5,
        }
    }

    #[test]
    fn test_project_columns() {
        let chunk = make_test_chunk();
        // 只选第一列
        let exprs = vec![
            Expression::ColumnRef { table: None, column: "a".to_string() },
        ];
        let result = project_chunk(&chunk, &exprs, &["a".to_string(), "b".to_string()]).unwrap();
        assert_eq!(result.num_columns(), 1);
        assert_eq!(result.count, 5);
    }

    #[test]
    fn test_project_arithmetic() {
        let chunk = make_test_chunk();
        // a + b
        let exprs = vec![
            Expression::BinaryOp {
                left: Box::new(Expression::ColumnRef { table: None, column: "a".to_string() }),
                op: BinaryOperator::Plus,
                right: Box::new(Expression::ColumnRef { table: None, column: "b".to_string() }),
            },
        ];
        let result = project_chunk(&chunk, &exprs, &["a".to_string(), "b".to_string()]).unwrap();
        assert_eq!(result.count, 5);
        let rows = result.to_rows();
        assert_eq!(rows[0][0], Value::Int64(11));
        assert_eq!(rows[1][0], Value::Int64(22));
        assert_eq!(rows[4][0], Value::Int64(55));
    }

    #[test]
    fn test_project_mixed() {
        let chunk = make_test_chunk();
        // 混合：列引用 + 计算 + 常量
        let exprs = vec![
            Expression::ColumnRef { table: None, column: "a".to_string() },
            Expression::BinaryOp {
                left: Box::new(Expression::ColumnRef { table: None, column: "a".to_string() }),
                op: BinaryOperator::Multiply,
                right: Box::new(Expression::Literal(Value::Int64(2))),
            },
            Expression::Literal(Value::Varchar("const".into())),
        ];
        let result = project_chunk(&chunk, &exprs, &["a".to_string(), "b".to_string()]).unwrap();
        assert_eq!(result.num_columns(), 3);
        assert_eq!(result.count, 5);
        let rows = result.to_rows();
        assert_eq!(rows[0][0], Value::Int64(10));
        assert_eq!(rows[0][1], Value::Int64(20));
        assert_eq!(rows[0][2], Value::Varchar("const".into()));
    }

    #[test]
    fn test_project_empty() {
        let chunk = DataChunk::new(2);
        let exprs = vec![
            Expression::ColumnRef { table: None, column: "a".to_string() },
        ];
        let result = project_chunk(&chunk, &exprs, &["a".to_string(), "b".to_string()]).unwrap();
        assert_eq!(result.count, 0);
    }
}
