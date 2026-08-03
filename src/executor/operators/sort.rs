//! 排序算子
//!
//! 基于行的全内存排序（当前实现），支持多列排序、升/降序。
//! 后续可扩展为外部排序（外排）以支持超大数据集。

use crate::common::error::Result;
use crate::Value;

use super::super::physical_plan::{SortKey, SortDirection};
use super::super::vector::DataChunk;

/// 执行排序
///
/// 将所有输入 chunk 收集为行数组，按排序键排序后重新分块。
/// 当 limit 有值时，使用 Top-N 部分排序优化（堆排序），避免全排序。
pub fn execute(input: &[DataChunk], sort_keys: &[SortKey], limit: Option<usize>) -> Result<Vec<DataChunk>> {
    if input.is_empty() || sort_keys.is_empty() {
        return Ok(input.to_vec());
    }

    let num_columns = input[0].num_columns();

    // 收集所有行
    let mut all_rows: Vec<Vec<Value>> = Vec::new();
    for chunk in input {
        let rows = chunk.to_rows();
        all_rows.extend(rows);
    }

    let total_rows = all_rows.len();
    if total_rows <= 1 {
        return Ok(input.to_vec());
    }

    let keys = sort_keys.to_vec();

    if let Some(n) = limit {
        // Top-N 优化：使用 BinaryHeap 只保留前 N 个
        // 若 n >= total_rows，退化为全排序
        if n < total_rows {
            use std::collections::BinaryHeap;
            // 使用反向排序的 BinaryHeap（最小堆，pop 移除最小的）
            // 我们想要前 N 个最大的（按 sort_keys 排序），所以用反向比较
            let mut heap: BinaryHeap<Vec<Value>> = BinaryHeap::with_capacity(n + 1);
            for row in all_rows {
                heap.push(row);
                if heap.len() > n {
                    heap.pop();
                }
            }
            // 提取并反转（heap 输出的是从大到小）
            let mut top_n: Vec<Vec<Value>> = heap.into_iter().collect();
            top_n.sort_by(|a, b| {
                for key in &keys {
                    let cmp = value_cmp(&a[key.column_index], &b[key.column_index]);
                    match cmp {
                        std::cmp::Ordering::Equal => continue,
                        other => {
                            return match key.direction {
                                SortDirection::Asc => other,
                                SortDirection::Desc => other.reverse(),
                            };
                        }
                    }
                }
                std::cmp::Ordering::Equal
            });
            all_rows = top_n;
        } else {
            // limit >= total_rows，全排序
            all_rows.sort_by(|a, b| {
                for key in &keys {
                    let cmp = value_cmp(&a[key.column_index], &b[key.column_index]);
                    match cmp {
                        std::cmp::Ordering::Equal => continue,
                        other => {
                            return match key.direction {
                                SortDirection::Asc => other,
                                SortDirection::Desc => other.reverse(),
                            };
                        }
                    }
                }
                std::cmp::Ordering::Equal
            });
        }
    } else {
        // 无 limit：全排序
        all_rows.sort_by(|a, b| {
            for key in &keys {
                let cmp = value_cmp(&a[key.column_index], &b[key.column_index]);
                match cmp {
                    std::cmp::Ordering::Equal => continue,
                    other => {
                        return match key.direction {
                            SortDirection::Asc => other,
                            SortDirection::Desc => other.reverse(),
                        };
                    }
                }
            }
            std::cmp::Ordering::Equal
        });
    }

    // 重新分块
    let mut result = Vec::new();
    let chunk_size = crate::executor::vector::VECTOR_SIZE;
    let total = all_rows.len();
    for chunk_start in (0..total).step_by(chunk_size) {
        let chunk_end = std::cmp::min(chunk_start + chunk_size, total);
        let chunk_rows = &all_rows[chunk_start..chunk_end];

        let mut columns = Vec::with_capacity(num_columns);
        for col_idx in 0..num_columns {
            let col_values: Vec<Value> = chunk_rows.iter().map(|r| r[col_idx].clone()).collect();
            columns.push(crate::executor::vector::Vector::from_values(col_values));
        }

        result.push(DataChunk {
            columns,
            count: chunk_end - chunk_start,
        });
    }

    Ok(result)
}

/// Value 比较（用于排序）
///
/// 排序规则：
/// - NULL 排在最前（ASC）或最后（DESC）
/// - 同类型按值比较
/// - 不同类型按类型优先级排序（Int64 < Float64 < Varchar < ...）
fn value_cmp(a: &Value, b: &Value) -> std::cmp::Ordering {
    use std::cmp::Ordering::*;
    use Value::*;

    match (a, b) {
        (Null, Null) => Equal,
        (Null, _) => Less,      // NULL 排在最前
        (_, Null) => Greater,

        (Int64(x), Int64(y)) => x.cmp(y),
        (Int64(x), Float64(y)) => (*x as f64).partial_cmp(y).unwrap_or(Equal),
        (Float64(x), Int64(y)) => x.partial_cmp(&(*y as f64)).unwrap_or(Equal),
        (Float64(x), Float64(y)) => x.partial_cmp(y).unwrap_or(Equal),

        (Varchar(x), Varchar(y)) => x.cmp(y),

        (Boolean(x), Boolean(y)) => x.cmp(y),

        (Int32(x), Int32(y)) => x.cmp(y),
        (Int32(x), Int64(y)) => (*x as i64).cmp(y),
        (Int64(x), Int32(y)) => x.cmp(&(*y as i64)),
        (Int32(x), Float64(y)) => (*x as f64).partial_cmp(y).unwrap_or(Equal),
        (Float64(x), Int32(y)) => x.partial_cmp(&(*y as f64)).unwrap_or(Equal),

        _ => {
            // 不同类型：按类型名排序（确定性）
            let type_a = std::mem::discriminant(a);
            let type_b = std::mem::discriminant(b);
            format!("{:?}", type_a).cmp(&format!("{:?}", type_b))
        }
    }
}

// ============================================================================
// 测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::vector::{Vector, DataChunk};

    fn make_test_chunk() -> DataChunk {
        // 三列：id (Int64), name (Varchar), score (Float64)
        let ids = Vector::Flat(vec![
            Value::Int64(3), Value::Int64(1), Value::Int64(2),
            Value::Int64(5), Value::Int64(4),
        ]);
        let names = Vector::Flat(vec![
            Value::Varchar("charlie".into()),
            Value::Varchar("alice".into()),
            Value::Varchar("bob".into()),
            Value::Varchar("eve".into()),
            Value::Varchar("dave".into()),
        ]);
        let scores = Vector::Flat(vec![
            Value::Float64(88.5),
            Value::Float64(95.0),
            Value::Float64(95.0),
            Value::Float64(72.3),
            Value::Float64(88.5),
        ]);
        DataChunk {
            columns: vec![ids, names, scores],
            count: 5,
        }
    }

    #[test]
    fn test_sort_single_asc() {
        let chunk = make_test_chunk();
        let keys = vec![SortKey { column_index: 0, direction: SortDirection::Asc }];
        let result = execute(&[chunk], &keys).unwrap();

        let rows: Vec<Vec<Value>> = result.iter().flat_map(|c| c.to_rows()).collect();
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[0][0], Value::Int64(1));
        assert_eq!(rows[1][0], Value::Int64(2));
        assert_eq!(rows[2][0], Value::Int64(3));
        assert_eq!(rows[3][0], Value::Int64(4));
        assert_eq!(rows[4][0], Value::Int64(5));
    }

    #[test]
    fn test_sort_single_desc() {
        let chunk = make_test_chunk();
        let keys = vec![SortKey { column_index: 0, direction: SortDirection::Desc }];
        let result = execute(&[chunk], &keys).unwrap();

        let rows: Vec<Vec<Value>> = result.iter().flat_map(|c| c.to_rows()).collect();
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[0][0], Value::Int64(5));
        assert_eq!(rows[4][0], Value::Int64(1));
    }

    #[test]
    fn test_sort_multi_key() {
        let chunk = make_test_chunk();
        // 先按 score 降序，再按 name 升序
        let keys = vec![
            SortKey { column_index: 2, direction: SortDirection::Desc },
            SortKey { column_index: 1, direction: SortDirection::Asc },
        ];
        let result = execute(&[chunk], &keys).unwrap();

        let rows: Vec<Vec<Value>> = result.iter().flat_map(|c| c.to_rows()).collect();
        assert_eq!(rows.len(), 5);
        // 95.0: alice, bob (按 name 升序)
        assert_eq!(rows[0][1], Value::Varchar("alice".into()));
        assert_eq!(rows[1][1], Value::Varchar("bob".into()));
        // 88.5: charlie, dave
        assert_eq!(rows[2][1], Value::Varchar("charlie".into()));
        assert_eq!(rows[3][1], Value::Varchar("dave".into()));
        // 72.3: eve
        assert_eq!(rows[4][1], Value::Varchar("eve".into()));
    }

    #[test]
    fn test_sort_varchar() {
        let chunk = make_test_chunk();
        let keys = vec![SortKey { column_index: 1, direction: SortDirection::Asc }];
        let result = execute(&[chunk], &keys).unwrap();

        let rows: Vec<Vec<Value>> = result.iter().flat_map(|c| c.to_rows()).collect();
        assert_eq!(rows[0][1], Value::Varchar("alice".into()));
        assert_eq!(rows[1][1], Value::Varchar("bob".into()));
        assert_eq!(rows[2][1], Value::Varchar("charlie".into()));
        assert_eq!(rows[3][1], Value::Varchar("dave".into()));
        assert_eq!(rows[4][1], Value::Varchar("eve".into()));
    }

    #[test]
    fn test_sort_empty() {
        let chunk = DataChunk::new(3);
        let keys = vec![SortKey { column_index: 0, direction: SortDirection::Asc }];
        let result = execute(&[chunk], &keys).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].count, 0);
    }
}
