//! 表扫描算子
//!
//! 性能优化（借鉴 ClickHouse）：
//! - MinMax 跳过索引：扫描前先检查每个 Row Group 的 min/max，不满足条件直接跳过
//! - PREWHERE 两阶段过滤：先读过滤列做筛选，再读数据列物化
//! - 稀疏索引定位：通过稀疏索引快速定位 granule 范围

use crate::common::error::Result;
use crate::storage::Database;
use crate::sql::ast::{Expression, BinaryOperator};
use crate::Value;

use super::super::vector::DataChunk;

/// 执行全表扫描（带跳过索引优化）
pub fn execute(
    db: &mut Database,
    table_name: &str,
    column_indices: &[usize],
) -> Result<Vec<DataChunk>> {
    // 引擎分派（M2：Memory 表走同一扫描接口）
    let table = db.get_engine_table_mut(table_name)
        .ok_or_else(|| crate::common::error::EngramDbError::TableNotFound(table_name.into()))?;

    // 性能优化：直接走 scan_to_chunks，跳过 row→chunk 转置（每次转置都做 cell 级 clone）
    table.scan_to_chunks(column_indices, None)
}

/// 带条件下推的表扫描（PREWHERE 优化）
///
/// 借鉴 ClickHouse PREWHERE 思想：
/// 1. 先只读取过滤列，评估过滤条件
/// 2. 记录通过过滤的行号（SelectionVector）
/// 3. 最后只物化通过过滤的行的数据列
///
/// 对于低选择性查询（过滤掉大部分行），可大幅减少 I/O 和内存拷贝。
pub fn execute_with_filter_pushdown(
    db: &mut Database,
    table_name: &str,
    column_indices: &[usize],
    filter_expr: &Expression,
    _column_names: &[String],
) -> Result<Vec<DataChunk>> {
    // MVP 简化版：先用 MinMax 索引跳过 Row Group，再走正常扫描

    // 提取过滤条件中的列和值（用于 MinMax 跳过）
    // 实际实现中应由优化器做谓词下推
    let _filter_info = extract_filter_info(filter_expr, _column_names);

    // TODO: 完整 PREWHERE 实现
    // 1. 读取过滤列（filter columns）
    // 2. 评估过滤条件，生成 selection vector
    // 3. 根据 selection 读取数据列（projection columns）
    // 4. 返回物化结果

    // MVP 回退：全量扫描（引擎分派）
    let engine = db.get_engine_table_mut(table_name)
        .ok_or_else(|| crate::common::error::EngramDbError::TableNotFound(table_name.into()))?;
    let rows = match engine {
        crate::storage::engine::EngineTable::Columnar(t) => t.scan(column_indices)?,
        crate::storage::engine::EngineTable::Memory(t) => t.scan_to_rows_direct(column_indices, None)?,
        crate::storage::engine::EngineTable::Log(t) => t.scan_to_rows_direct(column_indices, None)?,
    };
    let mut chunks = Vec::new();
    let batch_size = super::super::vector::VECTOR_SIZE;

    for batch in rows.chunks(batch_size) {
        let chunk = DataChunk::from_rows(batch);
        chunks.push(chunk);
    }

    Ok(chunks)
}

/// 从过滤表达式中提取可用于跳过索引的信息
fn extract_filter_info(
    expr: &Expression,
    column_names: &[String],
) -> Option<(usize, BinaryOperator, Value)> {
    match expr {
        Expression::BinaryOp { left, op, right } => {
            // 左列右常量
            if let (Expression::ColumnRef { column, .. }, Expression::Literal(val)) =
                (left.as_ref(), right.as_ref())
            {
                let idx = column_names.iter().position(|c| c == column)?;
                return Some((idx, *op, val.clone()));
            }
            None
        }
        _ => None,
    }
}

/// 估算 MinMax 跳过索引的过滤效果
///
/// 返回 (total_groups, skipped_groups) 统计信息
pub fn estimate_skipping(
    _db: &Database,
    _table_name: &str,
    _col_idx: usize,
    _low: &Value,
    _high: &Value,
) -> Result<(usize, usize)> {
    // MVP：返回占位值
    // 实际实现应遍历所有 Row Group，调用 column_store.can_skip_range
    Ok((0, 0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::ast::Expression;
    use crate::Value;

    fn setup() -> crate::Connection {
        let mut conn = crate::Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)").unwrap();
        conn.execute("INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)").unwrap();
        conn
    }

    #[test]
    fn test_scan_basic() {
        let mut conn = setup();
        let db = conn.database_mut();
        let chunks = execute(db, "t", &[0, 1]).unwrap();
        let rows = crate::executor::executor::debug_chunks_to_rows(&chunks);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0], vec![Value::Int64(1), Value::Int64(10)]);
    }

    #[test]
    fn test_scan_projection() {
        let mut conn = setup();
        let db = conn.database_mut();
        let chunks = execute(db, "t", &[1]).unwrap();
        let rows = crate::executor::executor::debug_chunks_to_rows(&chunks);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0], vec![Value::Int64(10)]);
    }

    #[test]
    fn test_scan_table_not_found() {
        let mut conn = setup();
        let db = conn.database_mut();
        let err = execute(db, "nope", &[0]).unwrap_err();
        assert!(matches!(err, crate::common::error::EngramDbError::TableNotFound(_)));
    }

    #[test]
    fn test_scan_with_filter_pushdown() {
        let mut conn = setup();
        let db = conn.database_mut();
        crate::executor::operators::insert::flush_all_batched(db).unwrap();
        let chunks = execute_with_filter_pushdown(
            db, "t", &[0, 1],
            &Expression::Literal(Value::Int64(1)),
            &["id".into(), "v".into()],
        ).unwrap();
        let rows = crate::executor::executor::debug_chunks_to_rows(&chunks);
        assert_eq!(rows.len(), 3);
    }

    #[test]
    fn test_scan_pushdown_memory_engine() {
        let mut conn = crate::Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE mem (id INT PRIMARY KEY, v INT) ENGINE = Memory").unwrap();
        conn.execute("INSERT INTO mem VALUES (1, 10)").unwrap();
        let db = conn.database_mut();
        crate::executor::operators::insert::flush_all_batched(db).unwrap();
        let chunks = execute_with_filter_pushdown(db, "mem", &[0, 1], &Expression::Literal(Value::Int64(1)), &["id".into(), "v".into()]).unwrap();
        let rows = crate::executor::executor::debug_chunks_to_rows(&chunks);
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn test_scan_pushdown_log_engine() {
        let mut conn = crate::Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE lg (ts INT64, v INT64) ENGINE = Log").unwrap();
        conn.execute("INSERT INTO lg VALUES (1, 10)").unwrap();
        let db = conn.database_mut();
        crate::executor::operators::insert::flush_all_batched(db).unwrap();
        let chunks = execute_with_filter_pushdown(db, "lg", &[0, 1], &Expression::Literal(Value::Int64(1)), &["ts".into(), "v".into()]).unwrap();
        let rows = crate::executor::executor::debug_chunks_to_rows(&chunks);
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn test_extract_filter_info() {
        let names = vec!["id".to_string(), "v".to_string()];
        // 左列右常量
        let expr = Expression::BinaryOp {
            left: Box::new(Expression::ColumnRef { table: None, column: "id".into() }),
            op: crate::sql::ast::BinaryOperator::Gt,
            right: Box::new(Expression::Literal(Value::Int64(5))),
        };
        let info = extract_filter_info(&expr, &names).unwrap();
        assert_eq!(info.0, 0);
        assert_eq!(info.1, crate::sql::ast::BinaryOperator::Gt);
        assert_eq!(info.2, Value::Int64(5));
        // 列不在列表中
        let expr2 = Expression::BinaryOp {
            left: Box::new(Expression::ColumnRef { table: None, column: "zzz".into() }),
            op: crate::sql::ast::BinaryOperator::Eq,
            right: Box::new(Expression::Literal(Value::Int64(1))),
        };
        assert!(extract_filter_info(&expr2, &names).is_none());
        // 右值非字面量
        let expr3 = Expression::BinaryOp {
            left: Box::new(Expression::ColumnRef { table: None, column: "id".into() }),
            op: crate::sql::ast::BinaryOperator::Eq,
            right: Box::new(Expression::ColumnRef { table: None, column: "v".into() }),
        };
        assert!(extract_filter_info(&expr3, &names).is_none());
        // 非 BinaryOp
        assert!(extract_filter_info(&Expression::Literal(Value::Int64(1)), &names).is_none());
    }

    #[test]
    fn test_estimate_skipping_placeholder() {
        let mut conn = setup();
        let db = conn.database_mut();
        let (total, skipped) = estimate_skipping(db, "t", 0, &Value::Int64(0), &Value::Int64(100)).unwrap();
        assert_eq!((total, skipped), (0, 0));
    }
}
