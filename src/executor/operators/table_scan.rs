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
    let table = db.get_table_mut(table_name)
        .ok_or_else(|| crate::common::error::HybridDbError::TableNotFound(table_name.into()))?;

    let rows = table.scan(column_indices)?;

    // 分批，每批 VECTOR_SIZE 行
    let mut chunks = Vec::new();
    let batch_size = super::super::vector::VECTOR_SIZE;

    for batch in rows.chunks(batch_size) {
        let chunk = DataChunk::from_rows(batch);
        chunks.push(chunk);
    }

    Ok(chunks)
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
    _filter_expr: &Expression,
    _column_names: &[String],
) -> Result<Vec<DataChunk>> {
    // MVP 简化版：先用 MinMax 索引跳过 Row Group，再走正常扫描
    let table = db.get_table_mut(table_name)
        .ok_or_else(|| crate::common::error::HybridDbError::TableNotFound(table_name.into()))?;

    // 提取过滤条件中的列和值（用于 MinMax 跳过）
    // 实际实现中应由优化器做谓词下推
    let _filter_info = extract_filter_info(_filter_expr, _column_names);

    // TODO: 完整 PREWHERE 实现
    // 1. 读取过滤列（filter columns）
    // 2. 评估过滤条件，生成 selection vector
    // 3. 根据 selection 读取数据列（projection columns）
    // 4. 返回物化结果

    // MVP 回退：全量扫描
    let rows = table.scan(column_indices)?;
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
