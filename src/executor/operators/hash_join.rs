//! 哈希连接算子
//!
//! 经典 Hash Join 算法（Grace Hash Join 的简化版，单轮）：
//! 1. Build 阶段：将右表（较小的表）按连接键构建哈希表
//! 2. Probe 阶段：扫描左表，在哈希表中查找匹配
//!
//! 支持 INNER / LEFT / RIGHT / FULL 四种连接类型。
//!
//! 性能优化：
//! - 使用 fxhash 快速哈希（已在项目依赖中）
//! - 链式哈希：相同键的多行存储在同一个 bucket 的链表中
//! - 批量输出：每凑够 VECTOR_SIZE 行就输出一个 DataChunk
//! - NULL 键处理：NULL 不参与连接（SQL 标准语义）

use crate::common::error::Result;
use crate::Value;

use super::super::vector::{DataChunk, Vector, VECTOR_SIZE};
use super::super::physical_plan::JoinType;

use fxhash::FxHashMap;

/// 执行哈希连接
///
/// 输入：左表数据块、右表数据块、连接键列索引、连接类型
/// 输出：连接结果数据块（左表列 + 右表列）
pub fn execute(
    left_chunks: &[DataChunk],
    right_chunks: &[DataChunk],
    left_keys: &[usize],
    right_keys: &[usize],
    join_type: JoinType,
) -> Result<Vec<DataChunk>> {
    if left_chunks.is_empty() || right_chunks.is_empty() {
        return handle_empty_input(left_chunks, right_chunks, join_type);
    }

    let left_cols = left_chunks[0].num_columns();
    let right_cols = right_chunks[0].num_columns();

    match join_type {
        JoinType::Inner => {
            // 选择较小的表作为 build 端（右表）
            let build_rows: usize = right_chunks.iter().map(|c| c.count).sum();
            let probe_rows: usize = left_chunks.iter().map(|c| c.count).sum();

            if build_rows <= probe_rows {
                hash_join_inner(
                    left_chunks, right_chunks,
                    left_keys, right_keys,
                    left_cols, right_cols,
                )
            } else {
                // 左右交换，然后交换结果列顺序
                let mut result = hash_join_inner(
                    right_chunks, left_chunks,
                    right_keys, left_keys,
                    right_cols, left_cols,
                )?;
                // 交换列顺序：右表列在前，左表列在后 → 交换回来
                for chunk in &mut result {
                    swap_join_columns(chunk, right_cols);
                }
                Ok(result)
            }
        }
        JoinType::Left => {
            hash_join_left(
                left_chunks, right_chunks,
                left_keys, right_keys,
                left_cols, right_cols,
            )
        }
        JoinType::Right => {
            // RIGHT JOIN = 左右交换的 LEFT JOIN，然后交换列顺序
            let mut result = hash_join_left(
                right_chunks, left_chunks,
                right_keys, left_keys,
                right_cols, left_cols,
            )?;
            for chunk in &mut result {
                swap_join_columns(chunk, right_cols);
            }
            Ok(result)
        }
        JoinType::Full => {
            hash_join_full(
                left_chunks, right_chunks,
                left_keys, right_keys,
                left_cols, right_cols,
            )
        }
    }
}

// ============================================================================
// 核心：Build + Probe 哈希连接
// ============================================================================

/// 构建哈希表：右表（build 端）按键值分组
///
/// HashMap<键值组合, Vec<(chunk_idx, row_idx)>>
/// 键值组合用 Vec<Value> 表示多列连接键
fn build_hash_table(
    chunks: &[DataChunk],
    key_cols: &[usize],
) -> FxHashMap<Vec<Value>, Vec<(usize, usize)>> {
    let mut map: FxHashMap<Vec<Value>, Vec<(usize, usize)>> = FxHashMap::default();

    for (chunk_idx, chunk) in chunks.iter().enumerate() {
        for row_idx in 0..chunk.count {
            // 提取键值
            let key = extract_key(chunk, row_idx, key_cols);

            // NULL 键不参与连接（SQL 标准：NULL = NULL 为 unknown）
            if key.iter().any(|v| v.is_null()) {
                continue;
            }

            map.entry(key).or_default().push((chunk_idx, row_idx));
        }
    }

    map
}

/// 从行中提取连接键值
fn extract_key(chunk: &DataChunk, row_idx: usize, key_cols: &[usize]) -> Vec<Value> {
    key_cols.iter()
        .map(|&col| {
            if col < chunk.columns.len() {
                chunk.columns[col].get(row_idx).clone()
            } else {
                Value::Null
            }
        })
        .collect()
}

// ============================================================================
// INNER JOIN
// ============================================================================

fn hash_join_inner(
    probe_chunks: &[DataChunk],  // 左表（探测端）
    build_chunks: &[DataChunk],  // 右表（构建端）
    probe_keys: &[usize],
    build_keys: &[usize],
    probe_cols: usize,
    build_cols: usize,
) -> Result<Vec<DataChunk>> {
    // Build 阶段
    let hash_table = build_hash_table(build_chunks, build_keys);

    let mut result_chunks = Vec::new();
    let mut output_rows: Vec<Vec<Value>> = Vec::with_capacity(VECTOR_SIZE);

    // Probe 阶段
    for probe_chunk in probe_chunks {
        for probe_row in 0..probe_chunk.count {
            let key = extract_key(probe_chunk, probe_row, probe_keys);

            // NULL 键不匹配
            if key.iter().any(|v| v.is_null()) {
                continue;
            }

            if let Some(matches) = hash_table.get(&key) {
                // 找到匹配，为每个匹配生成一行输出
                for &(build_chunk_idx, build_row_idx) in matches {
                    let build_chunk = &build_chunks[build_chunk_idx];
                    let mut row = Vec::with_capacity(probe_cols + build_cols);

                    // 左表列
                    for c in 0..probe_cols {
                        row.push(probe_chunk.columns[c].get(probe_row).clone());
                    }
                    // 右表列
                    for c in 0..build_cols {
                        row.push(build_chunk.columns[c].get(build_row_idx).clone());
                    }

                    output_rows.push(row);
                    if output_rows.len() >= VECTOR_SIZE {
                        result_chunks.push(DataChunk::from_rows(&output_rows));
                        output_rows.clear();
                    }
                }
            }
        }
    }

    // 剩余行
    if !output_rows.is_empty() {
        result_chunks.push(DataChunk::from_rows(&output_rows));
    }

    Ok(result_chunks)
}

// ============================================================================
// LEFT JOIN
// ============================================================================

fn hash_join_left(
    probe_chunks: &[DataChunk],  // 左表（保留所有行）
    build_chunks: &[DataChunk],  // 右表
    probe_keys: &[usize],
    build_keys: &[usize],
    probe_cols: usize,
    build_cols: usize,
) -> Result<Vec<DataChunk>> {
    let hash_table = build_hash_table(build_chunks, build_keys);

    let mut result_chunks = Vec::new();
    let mut output_rows: Vec<Vec<Value>> = Vec::with_capacity(VECTOR_SIZE);

    for probe_chunk in probe_chunks {
        for probe_row in 0..probe_chunk.count {
            let key = extract_key(probe_chunk, probe_row, probe_keys);
            let has_null_key = key.iter().any(|v| v.is_null());

            let matches = if has_null_key {
                None
            } else {
                hash_table.get(&key)
            };

            match matches {
                Some(rows) => {
                    // 有匹配：每个匹配一行
                    for &(build_chunk_idx, build_row_idx) in rows {
                        let build_chunk = &build_chunks[build_chunk_idx];
                        let mut row = Vec::with_capacity(probe_cols + build_cols);

                        for c in 0..probe_cols {
                            row.push(probe_chunk.columns[c].get(probe_row).clone());
                        }
                        for c in 0..build_cols {
                            row.push(build_chunk.columns[c].get(build_row_idx).clone());
                        }

                        output_rows.push(row);
                        if output_rows.len() >= VECTOR_SIZE {
                            result_chunks.push(DataChunk::from_rows(&output_rows));
                            output_rows.clear();
                        }
                    }
                }
                None => {
                    // 无匹配：左表行 + NULL 右表列
                    let mut row = Vec::with_capacity(probe_cols + build_cols);

                    for c in 0..probe_cols {
                        row.push(probe_chunk.columns[c].get(probe_row).clone());
                    }
                    for _ in 0..build_cols {
                        row.push(Value::Null);
                    }

                    output_rows.push(row);
                    if output_rows.len() >= VECTOR_SIZE {
                        result_chunks.push(DataChunk::from_rows(&output_rows));
                        output_rows.clear();
                    }
                }
            }
        }
    }

    if !output_rows.is_empty() {
        result_chunks.push(DataChunk::from_rows(&output_rows));
    }

    Ok(result_chunks)
}

// ============================================================================
// FULL OUTER JOIN
// ============================================================================

fn hash_join_full(
    left_chunks: &[DataChunk],
    right_chunks: &[DataChunk],
    left_keys: &[usize],
    right_keys: &[usize],
    left_cols: usize,
    right_cols: usize,
) -> Result<Vec<DataChunk>> {
    // 先做 LEFT JOIN（包含所有左表行）
    let mut result = hash_join_left(
        left_chunks, right_chunks,
        left_keys, right_keys,
        left_cols, right_cols,
    )?;

    // 再追加右表中未匹配的行（右表独有）
    let hash_table_left = build_hash_table(left_chunks, left_keys);

    let mut unmatched_right: Vec<Vec<Value>> = Vec::new();

    for right_chunk in right_chunks {
        for right_row in 0..right_chunk.count {
            let key = extract_key(right_chunk, right_row, right_keys);

            if key.iter().any(|v| v.is_null()) {
                // NULL 键的右表行也要出现在 FULL JOIN 中
                let mut row = Vec::with_capacity(left_cols + right_cols);
                for _ in 0..left_cols {
                    row.push(Value::Null);
                }
                for c in 0..right_cols {
                    row.push(right_chunk.columns[c].get(right_row).clone());
                }
                unmatched_right.push(row);
                continue;
            }

            if !hash_table_left.contains_key(&key) {
                // 右表行在左表中无匹配
                let mut row = Vec::with_capacity(left_cols + right_cols);
                for _ in 0..left_cols {
                    row.push(Value::Null);
                }
                for c in 0..right_cols {
                    row.push(right_chunk.columns[c].get(right_row).clone());
                }
                unmatched_right.push(row);
            }
        }
    }

    if !unmatched_right.is_empty() {
        // 分批追加
        for batch in unmatched_right.chunks(VECTOR_SIZE) {
            result.push(DataChunk::from_rows(batch));
        }
    }

    Ok(result)
}

// ============================================================================
// 辅助函数
// ============================================================================

/// 处理空输入的情况
fn handle_empty_input(
    left: &[DataChunk],
    right: &[DataChunk],
    join_type: JoinType,
) -> Result<Vec<DataChunk>> {
    match join_type {
        JoinType::Inner => Ok(vec![]),
        JoinType::Left => {
            // 右表为空：左表行 + NULL 右表列
            if left.is_empty() {
                return Ok(vec![]);
            }
            let left_cols = left[0].num_columns();
            let result: Vec<DataChunk> = left.iter().map(|chunk| {
                let mut columns = chunk.columns.clone();
                // 追加 NULL 列
                columns.push(Vector::Constant(Value::Null, chunk.count));
                DataChunk { columns, count: chunk.count }
            }).collect();
            Ok(result)
        }
        JoinType::Right => {
            if right.is_empty() {
                return Ok(vec![]);
            }
            let right_cols = right[0].num_columns();
            let result: Vec<DataChunk> = right.iter().map(|chunk| {
                let mut columns = vec![Vector::Constant(Value::Null, chunk.count)];
                columns.extend(chunk.columns.clone());
                DataChunk { columns, count: chunk.count }
            }).collect();
            Ok(result)
        }
        JoinType::Full => {
            // 两边都空返回空；一边空则等价于对应的 LEFT/RIGHT
            if left.is_empty() && right.is_empty() {
                return Ok(vec![]);
            }
            if left.is_empty() {
                handle_empty_input(left, right, JoinType::Right)
            } else {
                handle_empty_input(left, right, JoinType::Left)
            }
        }
    }
}

/// 交换连接结果的列顺序（前 N 列和后 M 列互换）
fn swap_join_columns(chunk: &mut DataChunk, first_part_cols: usize) {
    let total = chunk.columns.len();
    if first_part_cols >= total {
        return;
    }

    let mut new_columns = Vec::with_capacity(total);
    // 第二部分移到前面
    for i in first_part_cols..total {
        new_columns.push(chunk.columns[i].clone());
    }
    // 第一部分移到后面
    for i in 0..first_part_cols {
        new_columns.push(chunk.columns[i].clone());
    }

    chunk.columns = new_columns;
}

// ============================================================================
// 测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::vector::{Vector, DataChunk};

    fn make_left_chunk() -> DataChunk {
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
        DataChunk { columns: vec![ids, names], count: 5 }
    }

    fn make_right_chunk() -> DataChunk {
        let user_ids = Vector::Flat(vec![
            Value::Int64(1), Value::Int64(2), Value::Int64(2),
            Value::Int64(3), Value::Int64(6),
        ]);
        let orders = Vector::Flat(vec![
            Value::Varchar("order_a".into()),
            Value::Varchar("order_b1".into()),
            Value::Varchar("order_b2".into()),
            Value::Varchar("order_c".into()),
            Value::Varchar("order_f".into()),
        ]);
        DataChunk { columns: vec![user_ids, orders], count: 5 }
    }

    #[test]
    fn test_inner_join() {
        let left = vec![make_left_chunk()];
        let right = vec![make_right_chunk()];

        let result = execute(&left, &right, &[0], &[0], JoinType::Inner).unwrap();
        let total_rows: usize = result.iter().map(|c| c.count).sum();

        // 匹配：1, 2(2行), 3 → 共 4 行
        assert_eq!(total_rows, 4);
    }

    #[test]
    fn test_left_join() {
        let left = vec![make_left_chunk()];
        let right = vec![make_right_chunk()];

        let result = execute(&left, &right, &[0], &[0], JoinType::Left).unwrap();
        let total_rows: usize = result.iter().map(|c| c.count).sum();

        // 左表 5 行：1(1), 2(2), 3(1), 4(0→1行NULL), 5(0→1行NULL) = 6 行
        assert_eq!(total_rows, 6);
    }

    #[test]
    fn test_right_join() {
        let left = vec![make_left_chunk()];
        let right = vec![make_right_chunk()];

        let result = execute(&left, &right, &[0], &[0], JoinType::Right).unwrap();
        let total_rows: usize = result.iter().map(|c| c.count).sum();

        // 右表 5 行：1(1), 2(2), 2(2), 3(1), 6(0→1行NULL) = 5 行
        assert_eq!(total_rows, 5);
    }

    #[test]
    fn test_full_join() {
        let left = vec![make_left_chunk()];
        let right = vec![make_right_chunk()];

        let result = execute(&left, &right, &[0], &[0], JoinType::Full).unwrap();
        let total_rows: usize = result.iter().map(|c| c.count).sum();

        // LEFT(6) + 右表未匹配(6→1行) = 7 行
        assert_eq!(total_rows, 7);
    }

    #[test]
    fn test_inner_join_empty_right() {
        let left = vec![make_left_chunk()];
        let right = vec![];

        let result = execute(&left, &right, &[0], &[0], JoinType::Inner).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_left_join_empty_right() {
        let left = vec![make_left_chunk()];
        let right = vec![];

        let result = execute(&left, &right, &[0], &[0], JoinType::Left).unwrap();
        let total_rows: usize = result.iter().map(|c| c.count).sum();
        assert_eq!(total_rows, 5);
        // 右表列全为 NULL
        let all_rows: Vec<_> = result.iter().flat_map(|c| c.to_rows()).collect();
        for row in &all_rows {
            assert_eq!(row.len(), 3); // 左2 + 右1(NULL)
            assert_eq!(row[2], Value::Null);
        }
    }

    #[test]
    fn test_null_key_no_match() {
        let left = DataChunk {
            columns: vec![Vector::Flat(vec![
                Value::Int64(1), Value::Null, Value::Int64(3),
            ])],
            count: 3,
        };
        let right = DataChunk {
            columns: vec![Vector::Flat(vec![
                Value::Int64(1), Value::Int64(2), Value::Int64(3),
            ])],
            count: 3,
        };

        let result = execute(
            &[left], &[right], &[0], &[0], JoinType::Inner
        ).unwrap();
        let total_rows: usize = result.iter().map(|c| c.count).sum();
        // NULL 键不匹配，只有 1 和 3 匹配 → 2 行
        assert_eq!(total_rows, 2);
    }

    #[test]
    fn test_multi_key_join() {
        let left = DataChunk {
            columns: vec![
                Vector::Flat(vec![Value::Int64(1), Value::Int64(1), Value::Int64(2)]),
                Vector::Flat(vec![Value::Int64(10), Value::Int64(20), Value::Int64(10)]),
            ],
            count: 3,
        };
        let right = DataChunk {
            columns: vec![
                Vector::Flat(vec![Value::Int64(1), Value::Int64(1), Value::Int64(2)]),
                Vector::Flat(vec![Value::Int64(10), Value::Int64(30), Value::Int64(10)]),
                Vector::Flat(vec![
                    Value::Varchar("match1".into()),
                    Value::Varchar("nomatch".into()),
                    Value::Varchar("match2".into()),
                ]),
            ],
            count: 3,
        };

        let result = execute(
            &[left], &[right], &[0, 1], &[0, 1], JoinType::Inner
        ).unwrap();
        let total_rows: usize = result.iter().map(|c| c.count).sum();
        // 两列都匹配的：(1,10) 和 (2,10) → 2 行
        assert_eq!(total_rows, 2);
    }
}
