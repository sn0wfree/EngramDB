//! 投影算子
//!
//! 向量化投影：支持任意表达式计算，每列独立向量化求值。
//! 与优化器的投影下推（Projection Pushdown）配合，减少 IO 和计算量。

use crate::common::error::Result;
use crate::sql::ast::Expression;

use super::super::expression::{contains_subquery, eval_vectorized, eval_vectorized_with_db};
use super::super::vector::{DataChunk, Vector};

/// 执行投影（支持表达式计算）
///
/// 对每个表达式向量化求值，结果组成新的 DataChunk。
/// `input_columns` 是输入 chunk 的列名（用于 ColumnRef 解析）；
/// `column_names` 是输出列名（当前 DataChunk 不存储，仅保留接口一致）。
pub fn execute(
    input: &[DataChunk],
    expressions: &[Expression],
    input_columns: &[String],
    column_names: &[String],
) -> Result<Vec<DataChunk>> {
    let mut result = Vec::new();

    for chunk in input {
        let projected = project_chunk(chunk, expressions, input_columns)?;
        if !projected.is_empty() {
            result.push(projected);
        }
    }
    let _ = column_names;
    Ok(result)
}

/// 执行投影（支持数据库上下文，处理含子查询的表达式）
///
/// 表达式含关联子查询时逐行求值：每行构造外层上下文并独立求解。
pub fn execute_with_db(
    input: &[DataChunk],
    expressions: &[Expression],
    input_columns: &[String],
    column_names: &[String],
    db: &mut crate::storage::Database,
) -> Result<Vec<DataChunk>> {
    let per_row = input.iter().any(|c| c.count > 0) && expressions.iter().any(contains_subquery);

    let mut result = Vec::new();

    if !per_row {
        // 无延迟子查询：整批求值，仅传入 db 供非关联子查询使用
        for chunk in input {
            let mut columns = Vec::with_capacity(expressions.len());
            for expr in expressions {
                columns.push(eval_vectorized_with_db(expr, chunk, input_columns, Some(db), None)?);
            }
            result.push(DataChunk {
                count: chunk.count,
                columns,
            });
        }
        let _ = column_names;
        return Ok(result);
    }

    // 关联路径：逐行求值
    for chunk in input {
        let n_out = expressions.len();
        let mut out_cols: Vec<Vec<crate::Value>> = vec![Vec::with_capacity(chunk.count); n_out];
        for i in 0..chunk.count {
            let outer_row: Vec<(String, crate::Value)> = input_columns
                .iter()
                .zip(chunk.columns.iter())
                .map(|(name, col)| (name.clone(), col.get(i)))
                .collect();
            let single = DataChunk {
                count: 1,
                columns: chunk.columns.iter().map(|c| Vector::Flat(vec![c.get(i)])).collect(),
            };
            for (j, expr) in expressions.iter().enumerate() {
                let vec = eval_vectorized_with_db(expr, &single, input_columns, Some(db), Some(&outer_row))?;
                out_cols[j].push(vec.get(0));
            }
        }
        let columns: Vec<Vector> = out_cols.into_iter().map(Vector::Flat).collect();
        result.push(DataChunk {
            count: chunk.count,
            columns,
        });
    }
    let _ = column_names;
    Ok(result)
}

/// 对单个 DataChunk 做投影计算
fn project_chunk(chunk: &DataChunk, expressions: &[Expression], input_columns: &[String]) -> Result<DataChunk> {
    let mut columns = Vec::with_capacity(expressions.len());

    for expr in expressions {
        let vec = eval_vectorized(expr, chunk, input_columns)?;
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
    use crate::executor::vector::{DataChunk, Vector};
    use crate::sql::ast::{BinaryOperator, Expression};
    use crate::Value;

    fn make_test_chunk() -> DataChunk {
        let a = Vector::Flat(vec![
            Value::Int64(10),
            Value::Int64(20),
            Value::Int64(30),
            Value::Int64(40),
            Value::Int64(50),
        ]);
        let b = Vector::Flat(vec![
            Value::Int64(1),
            Value::Int64(2),
            Value::Int64(3),
            Value::Int64(4),
            Value::Int64(5),
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
        let exprs = vec![Expression::ColumnRef {
            table: None,
            column: "a".to_string(),
        }];
        let result = project_chunk(&chunk, &exprs, &["a".to_string(), "b".to_string()]).unwrap();
        assert_eq!(result.num_columns(), 1);
        assert_eq!(result.count, 5);
    }

    #[test]
    fn test_project_arithmetic() {
        let chunk = make_test_chunk();
        // a + b
        let exprs = vec![Expression::BinaryOp {
            left: Box::new(Expression::ColumnRef {
                table: None,
                column: "a".to_string(),
            }),
            op: BinaryOperator::Plus,
            right: Box::new(Expression::ColumnRef {
                table: None,
                column: "b".to_string(),
            }),
        }];
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
            Expression::ColumnRef {
                table: None,
                column: "a".to_string(),
            },
            Expression::BinaryOp {
                left: Box::new(Expression::ColumnRef {
                    table: None,
                    column: "a".to_string(),
                }),
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
        let exprs = vec![Expression::ColumnRef {
            table: None,
            column: "a".to_string(),
        }];
        let result = project_chunk(&chunk, &exprs, &["a".to_string(), "b".to_string()]).unwrap();
        assert_eq!(result.count, 0);
    }
}
