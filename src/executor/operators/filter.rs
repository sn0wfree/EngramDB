//! 过滤算子
//!
//! 向量化过滤架构：
//! - 条件表达式通过向量化求值引擎整批计算
//! - 结果为布尔 Vector，转换为 SelectionVector 实现零拷贝过滤
//! - 支持任意复杂布尔表达式（AND/OR/NOT 任意嵌套）
//! - 懒物化：过滤只改 selection，数据拷贝推迟到物化阶段

use crate::common::error::Result;
use crate::sql::ast::Expression;

use super::super::vector::{DataChunk, LazyDataChunk, SelectionVector};
use super::super::expression::{eval_vectorized, boolean_to_selection};

/// 执行过滤（向量化 + SelectionVector 懒物化）
///
/// 对每个 DataChunk：
/// 1. 向量化求值条件表达式 → 布尔 Vector
/// 2. 布尔 Vector → SelectionVector（零拷贝过滤）
/// 3. 保留在 LazyDataChunk 中，下游按需物化
pub fn execute(
    input: &[DataChunk],
    condition: &Expression,
    column_names: &[String],
) -> Result<Vec<DataChunk>> {
    let mut result = Vec::new();

    for chunk in input {
        let filtered = filter_chunk(chunk, condition, column_names)?;
        if !filtered.is_empty() {
            result.push(filtered);
        }
    }

    Ok(result)
}

/// 过滤单个 DataChunk（内部使用懒物化）
fn filter_chunk(
    chunk: &DataChunk,
    condition: &Expression,
    column_names: &[String],
) -> Result<DataChunk> {
    // 步骤 1：向量化求值条件表达式
    let bool_vec = eval_vectorized(condition, chunk, column_names)?;

    // 步骤 2：布尔 Vector → 选择索引
    let selected_indices = boolean_to_selection(&bool_vec);

    // 步骤 3：应用选择向量
    if selected_indices.len() == chunk.count {
        // 全通过，直接返回原 chunk
        Ok(chunk.clone())
    } else if selected_indices.is_empty() {
        // 全过滤，返回空 chunk
        Ok(DataChunk::new(chunk.num_columns()))
    } else {
        let sel = SelectionVector::from_indices(selected_indices);
        Ok(sel.apply_to_chunk(chunk))
    }
}

/// 带懒物化的过滤（供执行器内部管道使用，避免不必要的物化）
///
/// 返回 LazyDataChunk，下游算子可直接基于 selection 继续计算，
/// 直到真正需要数据时才物化。这是 ClickHouse 风格的核心优化。
pub fn execute_lazy(
    chunk: DataChunk,
    condition: &Expression,
    column_names: &[String],
) -> Result<LazyDataChunk> {
    let mut lazy = LazyDataChunk::new(chunk);

    // 向量化求值条件
    let bool_vec = eval_vectorized(condition, &lazy.chunk, column_names)?;
    let selected_indices = boolean_to_selection(&bool_vec);

    if selected_indices.len() < lazy.chunk.count {
        // 部分过滤，设置 selection
        lazy.selection = Some(SelectionVector::from_indices(selected_indices));
    }
    // 全通过时 selection 保持 None（全选优化）

    Ok(lazy)
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
        // 两列：id (Int64), name (Varchar)
        let ids = Vector::Flat(vec![
            Value::Int64(1), Value::Int64(2), Value::Int64(3),
            Value::Int64(4), Value::Int64(5),
        ]);
        let names = Vector::Flat(vec![
            Value::Varchar("alice".into()),
            Value::Varchar("bob".into()),
            Value::Varchar("charlie".into()),
            Value::Varchar("dave".into()),
            Value::Varchar("eve".into()),
        ]);
        DataChunk {
            columns: vec![ids, names],
            count: 5,
        }
    }

    #[test]
    fn test_filter_gt() {
        let chunk = make_test_chunk();
        let condition = Expression::BinaryOp {
            left: Box::new(Expression::ColumnRef { table: None, column: "id".to_string() }),
            op: BinaryOperator::Gt,
            right: Box::new(Expression::Literal(Value::Int64(3))),
        };

        let result = filter_chunk(&chunk, &condition, &["id".to_string(), "name".to_string()]).unwrap();
        assert_eq!(result.count, 2);
        let rows = result.to_rows();
        assert_eq!(rows[0][0], Value::Int64(4));
        assert_eq!(rows[1][0], Value::Int64(5));
    }

    #[test]
    fn test_filter_eq() {
        let chunk = make_test_chunk();
        let condition = Expression::BinaryOp {
            left: Box::new(Expression::ColumnRef { table: None, column: "name".to_string() }),
            op: BinaryOperator::Eq,
            right: Box::new(Expression::Literal(Value::Varchar("bob".into()))),
        };

        let result = filter_chunk(&chunk, &condition, &["id".to_string(), "name".to_string()]).unwrap();
        assert_eq!(result.count, 1);
        let rows = result.to_rows();
        assert_eq!(rows[0][1], Value::Varchar("bob".into()));
    }

    #[test]
    fn test_filter_and() {
        let chunk = make_test_chunk();
        // id > 2 AND id < 5
        let condition = Expression::BinaryOp {
            left: Box::new(Expression::BinaryOp {
                left: Box::new(Expression::ColumnRef { table: None, column: "id".to_string() }),
                op: BinaryOperator::Gt,
                right: Box::new(Expression::Literal(Value::Int64(2))),
            }),
            op: BinaryOperator::And,
            right: Box::new(Expression::BinaryOp {
                left: Box::new(Expression::ColumnRef { table: None, column: "id".to_string() }),
                op: BinaryOperator::Lt,
                right: Box::new(Expression::Literal(Value::Int64(5))),
            }),
        };

        let result = filter_chunk(&chunk, &condition, &["id".to_string(), "name".to_string()]).unwrap();
        assert_eq!(result.count, 2);
        let rows = result.to_rows();
        assert_eq!(rows[0][0], Value::Int64(3));
        assert_eq!(rows[1][0], Value::Int64(4));
    }

    #[test]
    fn test_filter_all_pass() {
        let chunk = make_test_chunk();
        let condition = Expression::BinaryOp {
            left: Box::new(Expression::ColumnRef { table: None, column: "id".to_string() }),
            op: BinaryOperator::Gt,
            right: Box::new(Expression::Literal(Value::Int64(0))),
        };

        let result = filter_chunk(&chunk, &condition, &["id".to_string(), "name".to_string()]).unwrap();
        assert_eq!(result.count, 5);
    }

    #[test]
    fn test_filter_none_pass() {
        let chunk = make_test_chunk();
        let condition = Expression::BinaryOp {
            left: Box::new(Expression::ColumnRef { table: None, column: "id".to_string() }),
            op: BinaryOperator::Gt,
            right: Box::new(Expression::Literal(Value::Int64(100))),
        };

        let result = filter_chunk(&chunk, &condition, &["id".to_string(), "name".to_string()]).unwrap();
        assert_eq!(result.count, 0);
    }

    #[test]
    fn test_filter_like() {
        let chunk = make_test_chunk();
        let condition = Expression::Like {
            expr: Box::new(Expression::ColumnRef { table: None, column: "name".to_string() }),
            pattern: Box::new(Expression::Literal(Value::Varchar("%a%".into()))),
        };

        let result = filter_chunk(&chunk, &condition, &["id".to_string(), "name".to_string()]).unwrap();
        // alice, charlie, dave 包含 'a'
        assert_eq!(result.count, 3);
    }
}
