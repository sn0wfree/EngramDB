//! 排序算子
//!
//! 基于行的全内存排序（当前实现），支持多列排序、升/降序。
//! 后续可扩展为外部排序（外排）以支持超大数据集。

use crate::common::error::Result;
use crate::Value;

use super::super::physical_plan::{SortKey, SortDirection};
use super::super::vector::{DataChunk, Vector};

/// 执行排序
///
/// 将所有输入 chunk 收集为行数组，按排序键排序后重新分块。
/// 当 limit 有值时，使用 Top-N 部分排序优化（堆排序），避免全排序。
pub fn execute(input: &[DataChunk], sort_keys: &[SortKey], limit: Option<usize>) -> Result<Vec<DataChunk>> {
    if input.is_empty() || sort_keys.is_empty() {
        return Ok(input.to_vec());
    }

    let num_columns = input[0].num_columns();

    // S2-M3：全 Typed 输入 → 索引排序 + gather（零行式物化）
    // 仅全排序场景（无 limit 或 limit >= 总行数）；Top-N 堆路径保持行式
    let full_sort = match limit {
        Some(n) => n >= input.iter().map(|c| c.len()).sum::<usize>(),
        None => true,
    };
    if full_sort && input.iter().all(|c| c.columns.iter().all(|v| v.is_typed())) {
        if let Some(result) = try_execute_typed(input, sort_keys) {
            return Ok(result);
        }
    }

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
            // P3.4：方向感知堆 —— ASC/DESC/混合方向统一走堆排序，
            // 不再让 DESC 退化为全排序。
            // 堆按「目标排序序」比较：堆溢出时 pop 掉序最大的行，
            // 保留序最小的 N 行（即 Top-N 结果集）。
            let mut heap: BinaryHeap<HeapRow> = BinaryHeap::with_capacity(n + 1);
            for row in all_rows {
                heap.push(HeapRow { row, keys: &keys });
                if heap.len() > n {
                    heap.pop();
                }
            }
            let mut top_n: Vec<Vec<Value>> = heap.into_iter().map(|h| h.row).collect();
            top_n.sort_by(|a, b| cmp_rows(a, b, &keys));
            all_rows = top_n;
        } else {
            // limit >= total_rows，全排序
            // S1.1：单列整数 → Radix Sort（O(N)）；否则退回比较排序
            if keys.len() != 1 || !try_radix_sort(&mut all_rows, &keys[0]) {
                all_rows.sort_by(|a, b| cmp_rows(a, b, &keys));
            }
        }
    } else {
        // 无 limit：全排序
        // S1.1：单列整数 → Radix Sort（O(N)）；否则退回比较排序
        if keys.len() != 1 || !try_radix_sort(&mut all_rows, &keys[0]) {
            all_rows.sort_by(|a, b| cmp_rows(a, b, &keys));
        }
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

/// S2-M3：全 Typed 输入的索引排序 + gather（零行式物化）
///
/// 1. 每列合并所有 chunk 的类型化数据（append，O(N) 数组扩展）
/// 2. 单列整数键 → radix 索引排序；否则索引比较排序（value_cmp 语义）
/// 3. 输出按 order 分块 gather → Vector::Typed
/// 不适用（类型不符）→ None → 回退行式路径
fn try_execute_typed(input: &[DataChunk], sort_keys: &[SortKey]) -> Option<Vec<DataChunk>> {
    use crate::common::column_data::ColumnData;
    use crate::common::column_data::ColumnValue;

    let num_columns = input[0].num_columns();
    let total: usize = input.iter().map(|c| c.len()).sum();
    if total <= 1 {
        return Some(input.to_vec());
    }

    // 1. 合并每列（类型一致性由 append 保证，不符 → None）
    let mut merged_cols: Vec<ColumnData> = Vec::with_capacity(num_columns);
    for col_idx in 0..num_columns {
        let mut acc: Option<ColumnData> = None;
        for chunk in input {
            let d = match &chunk.columns[col_idx] {
                Vector::Typed(d) => d,
                _ => return None,
            };
            match &mut acc {
                None => acc = Some(d.clone()),
                Some(a) => a.append(d),
            }
        }
        merged_cols.push(acc?);
    }

    // 2. 排序索引
    let mut order: Vec<usize> = Vec::with_capacity(total);
    if sort_keys.len() == 1 {
        let key = &sort_keys[0];
        let desc = matches!(key.direction, SortDirection::Desc);
        let col = &merged_cols[key.column_index];
        // 单列整数 → radix 索引排序
        let mut keys: Vec<(u64, usize)> = Vec::with_capacity(total);
        let mut nulls: Vec<usize> = Vec::new();
        for i in 0..total {
            if col.nulls.as_ref().map_or(false, |n| n.test(i)) {
                nulls.push(i);
                continue;
            }
            match &col.values {
                ColumnValue::Int64(v) => keys.push((int_to_key(v[i], desc), i)),
                ColumnValue::Timestamp(v) => keys.push((int_to_key(v[i], desc), i)),
                _ => return None,
            }
        }
        radix_sort_16bit(&mut keys);
        if desc {
            order.extend(keys.iter().map(|&(_, i)| i));
            order.extend(nulls);
        } else {
            order.extend(nulls);
            order.extend(keys.iter().map(|&(_, i)| i));
        }
    } else {
        // 多列：索引比较排序（value_cmp 语义，含 NULL 最前 / DESC）
        let mut idx: Vec<usize> = (0..total).collect();
        idx.sort_by(|&a, &b| {
            for key in sort_keys {
                let va = merged_cols[key.column_index].get(a);
                let vb = merged_cols[key.column_index].get(b);
                let c = value_cmp(&va, &vb);
                match c {
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
        order = idx;
    }

    // 3. 输出：order 分块 gather
    let mut result = Vec::new();
    let chunk_size = crate::executor::vector::VECTOR_SIZE;
    for chunk_start in (0..total).step_by(chunk_size) {
        let chunk_end = std::cmp::min(chunk_start + chunk_size, total);
        let sel = &order[chunk_start..chunk_end];
        let columns: Vec<Vector> = merged_cols
            .iter()
            .map(|col| Vector::Typed(col.gather(sel)))
            .collect();
        result.push(DataChunk {
            columns,
            count: chunk_end - chunk_start,
        });
    }

    Some(result)
}

// S1.1：单列整数 Radix Sort（LSD 16-bit × 4 pass，O(N) 非比较排序）//
// 适用条件：单排序键且列为 Int32/Int64/Timestamp（纯整数，无 NaN 语义问题）。
// 语义与 value_cmp 一致：NULL 在 ASC 最前 / DESC 最后；稳定（保持原相对顺序）。
// 不满足条件时返回 false，调用方退回 sort_by 比较排序。

/// 尝试用 Radix Sort 排序，成功返回 true（all_rows 原地重排）
fn try_radix_sort(all_rows: &mut Vec<Vec<Value>>, key: &SortKey) -> bool {
    let col = key.column_index;
    let desc = matches!(key.direction, SortDirection::Desc);

    // 1. 提取排序键：(u64 key, 原行号)；NULL 单独记录
    let mut keys: Vec<(u64, usize)> = Vec::with_capacity(all_rows.len());
    let mut nulls: Vec<usize> = Vec::new();
    for (i, row) in all_rows.iter().enumerate() {
        match &row[col] {
            Value::Null => nulls.push(i),
            Value::Int32(v) => keys.push((int_to_key(*v as i64, desc), i)),
            Value::Int64(v) => keys.push((int_to_key(*v, desc), i)),
            Value::Timestamp(v) => keys.push((int_to_key(*v, desc), i)),
            _ => return false, // 非整数列（Float/Varchar/…）→ 退回比较排序
        }
    }

    // 2. LSD 基数排序（16-bit × 4 pass，稳定）
    radix_sort_16bit(&mut keys);

    // 3. 按排序索引重排行（mem::take 移动，零深拷贝）
    let mut rows = std::mem::take(all_rows);
    let mut order: Vec<usize> = Vec::with_capacity(rows.len());
    if desc {
        order.extend(keys.iter().map(|&(_, i)| i));
        order.extend(nulls); // DESC：NULL 最后，保持原相对顺序
    } else {
        order.extend(nulls); // ASC：NULL 最前，保持原相对顺序
        order.extend(keys.iter().map(|&(_, i)| i));
    }
    for i in order {
        all_rows.push(std::mem::take(&mut rows[i]));
    }
    true
}

/// i64 → 单调 u64 键：ASC 时按位翻转符号位（负值映射到前半段），DESC 时整体取反
#[inline]
fn int_to_key(v: i64, desc: bool) -> u64 {
    let k = v as u64 ^ (1u64 << 63);
    if desc { !k } else { k }
}

/// LSD 基数排序：16-bit 分桶，4 趟（覆盖 u64 全范围），稳定
fn radix_sort_16bit(keys: &mut Vec<(u64, usize)>) {
    const PASSES: usize = 4;
    const BUCKETS: usize = 65536;
    let mut count = vec![0u32; BUCKETS];
    let mut out: Vec<(u64, usize)> = Vec::with_capacity(keys.len());

    for pass in 0..PASSES {
        let shift = pass * 16;
        count.iter_mut().for_each(|c| *c = 0);
        for &(k, _) in keys.iter() {
            count[((k >> shift) & 0xFFFF) as usize] += 1;
        }
        let mut sum = 0u32;
        for c in count.iter_mut() {
            let t = *c;
            *c = sum;
            sum += t;
        }
        out.clear();
        out.resize(keys.len(), (0, 0));
        for &(k, i) in keys.iter() {
            let bucket = ((k >> shift) & 0xFFFF) as usize;
            out[count[bucket] as usize] = (k, i);
            count[bucket] += 1;
        }
        std::mem::swap(keys, &mut out);
    }
}

/// 按排序键比较两行（方向感知：ASC / DESC）
fn cmp_rows(a: &[Value], b: &[Value], keys: &[SortKey]) -> std::cmp::Ordering {
    for key in keys {
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
}

/// Top-N 堆元素（P3.4）
///
/// 通过 `Ord` 实现方向感知比较，使 BinaryHeap 在 ASC 和 DESC 下
/// 都能在溢出时弹出「序最大」的行，保留 Top-N 最小序集合。
struct HeapRow<'a> {
    row: Vec<Value>,
    keys: &'a [SortKey],
}

impl<'a> PartialEq for HeapRow<'a> {
    fn eq(&self, other: &Self) -> bool {
        cmp_rows(&self.row, &other.row, self.keys) == std::cmp::Ordering::Equal
    }
}

impl<'a> Eq for HeapRow<'a> {}

impl<'a> PartialOrd for HeapRow<'a> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<'a> Ord for HeapRow<'a> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // 注意：BinaryHeap 是最大堆，弹出「序最大」的行。
        // 方向感知比较直接作用于堆排序，ASC/DESC 统一处理。
        cmp_rows(&self.row, &other.row, self.keys)
    }
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

        // S1.1：Timestamp 按 i64 数值语义比较（此前落 `_` 分支按类型名恒等，
        // 导致 Timestamp 列排序失效——保持原序）
        (Timestamp(x), Timestamp(y)) => x.cmp(y),
        (Timestamp(x), Int32(y)) => x.cmp(&(*y as i64)),
        (Int32(x), Timestamp(y)) => (*x as i64).cmp(y),
        (Timestamp(x), Int64(y)) => x.cmp(y),
        (Int64(x), Timestamp(y)) => x.cmp(y),
        (Timestamp(x), Float64(y)) => (*x as f64).partial_cmp(y).unwrap_or(Equal),
        (Float64(x), Timestamp(y)) => x.partial_cmp(&(*y as f64)).unwrap_or(Equal),

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
        let result = execute(&[chunk], &keys, None).unwrap();

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
        let result = execute(&[chunk], &keys, None).unwrap();

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
        let result = execute(&[chunk], &keys, None).unwrap();

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
        let result = execute(&[chunk], &keys, None).unwrap();

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
        let result = execute(&[chunk], &keys, None).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].count, 0);
    }

    // P3.4：DESC / 混合方向 Top-N（此前 DESC 退化为全排序，现走反向堆）

    #[test]
    fn test_sort_desc_top_n() {
        let chunk = make_test_chunk();
        let keys = vec![SortKey { column_index: 0, direction: SortDirection::Desc }];
        // Top-3（desc）：5, 4, 3
        let result = execute(&[chunk], &keys, Some(3)).unwrap();
        let rows: Vec<Vec<Value>> = result.iter().flat_map(|c| c.to_rows()).collect();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0][0], Value::Int64(5));
        assert_eq!(rows[1][0], Value::Int64(4));
        assert_eq!(rows[2][0], Value::Int64(3));
    }

    #[test]
    fn test_sort_mixed_direction_top_n() {
        let chunk = make_test_chunk();
        // score DESC, name ASC；Top-3 应为 95.0 的两行（alice, bob）+ 88.5 的 charlie
        let keys = vec![
            SortKey { column_index: 2, direction: SortDirection::Desc },
            SortKey { column_index: 1, direction: SortDirection::Asc },
        ];
        let result = execute(&[chunk], &keys, Some(3)).unwrap();
        let rows: Vec<Vec<Value>> = result.iter().flat_map(|c| c.to_rows()).collect();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0][1], Value::Varchar("alice".into()));
        assert_eq!(rows[1][1], Value::Varchar("bob".into()));
        assert_eq!(rows[2][1], Value::Varchar("charlie".into()));
    }

    #[test]
    fn test_sort_asc_top_n() {
        let chunk = make_test_chunk();
        let keys = vec![SortKey { column_index: 0, direction: SortDirection::Asc }];
        // Top-2（asc）：1, 2
        let result = execute(&[chunk], &keys, Some(2)).unwrap();
        let rows: Vec<Vec<Value>> = result.iter().flat_map(|c| c.to_rows()).collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0], Value::Int64(1));
        assert_eq!(rows[1][0], Value::Int64(2));
    }

    // ========================================================================
    // S1.1 Radix Sort 测试
    // ========================================================================

    /// 强制走比较排序路径（等价性测试对照，execute 本身会选 radix 路径）
    fn execute_force_compare(input: &[DataChunk], sort_keys: &[SortKey]) -> Result<Vec<DataChunk>> {
        let num_columns = input[0].num_columns();
        let mut all_rows: Vec<Vec<Value>> = Vec::new();
        for chunk in input {
            all_rows.extend(chunk.to_rows());
        }
        all_rows.sort_by(|a, b| cmp_rows(a, b, sort_keys));
        let mut result = Vec::new();
        let chunk_size = crate::executor::vector::VECTOR_SIZE;
        for chunk_start in (0..all_rows.len()).step_by(chunk_size) {
            let chunk_end = std::cmp::min(chunk_start + chunk_size, all_rows.len());
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

    /// 构造 N 行双列 chunk：col0 为排序键（含 NULL），col1 为原行号标记
    fn make_radix_chunk(keys: Vec<Value>) -> DataChunk {
        let n = keys.len();
        let col0 = Vector::Flat(keys);
        let col1 = Vector::Flat((0..n).map(|i| Value::Int64(i as i64)).collect());
        DataChunk {
            columns: vec![col0, col1],
            count: n,
        }
    }

    #[test]
    fn test_radix_matches_sort_by_random() {
        // 随机数据等价性：radix 结果 == sort_by 结果（ASC + DESC，含 NULL）
        use rand::Rng;
        let mut rng = rand::thread_rng();
        for trial in 0..20 {
            let n = 1 + (trial * 37) % 5000;
            let mut keys: Vec<Value> = Vec::with_capacity(n);
            for _ in 0..n {
                keys.push(match rng.gen_range(0..10) {
                    0 => Value::Null,
                    1 => Value::Int32(rng.gen_range(-1000..1000)),
                    2 => Value::Timestamp(rng.gen_range(-10000..10000)),
                    _ => Value::Int64(rng.gen_range(-1_000_000..1_000_000)),
                });
            }
            for desc in [false, true] {
                let dir = if desc { SortDirection::Desc } else { SortDirection::Asc };
                let keys_ref = vec![SortKey { column_index: 0, direction: dir }];

                let chunk_radix = make_radix_chunk(keys.clone());
                let r_radix = execute(&[chunk_radix], &keys_ref, None).unwrap();
                let rows_radix: Vec<Vec<Value>> = r_radix.iter().flat_map(|c| c.to_rows()).collect();

                let chunk_cmp = make_radix_chunk(keys.clone());
                let r_cmp = execute_force_compare(&[chunk_cmp], &keys_ref).unwrap();
                let rows_cmp: Vec<Vec<Value>> = r_cmp.iter().flat_map(|c| c.to_rows()).collect();

                assert_eq!(rows_radix.len(), rows_cmp.len(),
                    "trial {} desc {} radix={} cmp={}", trial, desc, rows_radix.len(), rows_cmp.len());
                for (a, b) in rows_radix.iter().zip(rows_cmp.iter()) {
                    assert_eq!(a[0], b[0], "key mismatch trial {} desc {}", trial, desc);
                    assert_eq!(a[1], b[1], "row order mismatch trial {} desc {}", trial, desc);
                }
            }
        }
    }

    #[test]
    fn test_radix_null_order() {
        // ASC：NULL 最前；DESC：NULL 最后
        let keys = vec![
            Value::Int64(5), Value::Null, Value::Int64(1), Value::Null, Value::Int64(3),
        ];
        let asc = vec![SortKey { column_index: 0, direction: SortDirection::Asc }];
        let rows: Vec<Vec<Value>> = execute(&[make_radix_chunk(keys.clone())], &asc, None)
            .unwrap().iter().flat_map(|c| c.to_rows()).collect();
        assert_eq!(rows[0][0], Value::Null);
        assert_eq!(rows[1][0], Value::Null);
        assert_eq!(rows[2][0], Value::Int64(1));
        assert_eq!(rows[3][0], Value::Int64(3));
        assert_eq!(rows[4][0], Value::Int64(5));

        let desc = vec![SortKey { column_index: 0, direction: SortDirection::Desc }];
        let rows: Vec<Vec<Value>> = execute(&[make_radix_chunk(keys)], &desc, None)
            .unwrap().iter().flat_map(|c| c.to_rows()).collect();
        assert_eq!(rows[0][0], Value::Int64(5));
        assert_eq!(rows[1][0], Value::Int64(3));
        assert_eq!(rows[2][0], Value::Int64(1));
        assert_eq!(rows[3][0], Value::Null);
        assert_eq!(rows[4][0], Value::Null);
    }

    #[test]
    fn test_radix_stability() {
        // 重复键保持原相对顺序（与 sort_by 稳定一致）
        let keys = vec![
            Value::Int64(2), Value::Int64(1), Value::Int64(2), Value::Int64(1), Value::Int64(2),
        ];
        let asc = vec![SortKey { column_index: 0, direction: SortDirection::Asc }];
        let rows: Vec<Vec<Value>> = execute(&[make_radix_chunk(keys)], &asc, None)
            .unwrap().iter().flat_map(|c| c.to_rows()).collect();
        // 原序号：key=1 的是行 1,3；key=2 的是行 0,2,4
        assert_eq!(rows[0][1], Value::Int64(1));
        assert_eq!(rows[1][1], Value::Int64(3));
        assert_eq!(rows[2][1], Value::Int64(0));
        assert_eq!(rows[3][1], Value::Int64(2));
        assert_eq!(rows[4][1], Value::Int64(4));
    }

    #[test]
    fn test_radix_fallback_non_integer() {
        // 非整数列（Float64）：radix 不可用，走 sort_by，结果仍正确
        let chunk = make_test_chunk();
        let keys = vec![SortKey { column_index: 2, direction: SortDirection::Asc }];
        let result = execute(&[chunk], &keys, None).unwrap();
        let rows: Vec<Vec<Value>> = result.iter().flat_map(|c| c.to_rows()).collect();
        assert_eq!(rows.len(), 5);
        assert_eq!(rows[0][0], Value::Int64(5)); // 72.3: eve（idx 3）
        assert_eq!(rows[4][0], Value::Int64(2)); // 95.0: bob（idx 2，稳定序最后）
    }

    #[test]
    fn test_radix_all_null() {
        // 全 NULL 列：保持原序
        let keys = vec![Value::Null, Value::Null, Value::Null];
        let asc = vec![SortKey { column_index: 0, direction: SortDirection::Asc }];
        let rows: Vec<Vec<Value>> = execute(&[make_radix_chunk(keys)], &asc, None)
            .unwrap().iter().flat_map(|c| c.to_rows()).collect();
        assert_eq!(rows[0][1], Value::Int64(0));
        assert_eq!(rows[1][1], Value::Int64(1));
        assert_eq!(rows[2][1], Value::Int64(2));
    }

    /// S2-M3：Typed 索引排序与行式路径等价性（单列 radix + 多列比较 + NULL + DESC）
    #[test]
    fn test_typed_sort_matches_rowwise() {
        use crate::common::column_data::ColumnData;
        use rand::Rng;
        let mut rng = rand::thread_rng();
        for trial in 0..20 {
            let n = 20 + rng.gen_range(0..300);
            let keys: Vec<Value> = (0..n)
                .map(|_| {
                    if rng.gen_bool(0.15) {
                        Value::Null
                    } else {
                        Value::Int64(rng.gen_range(-100000..100000))
                    }
                })
                .collect();
            let payload: Vec<Value> = (0..n).map(|i| Value::Int64(i as i64)).collect();
            let data = ColumnData::try_from_values(&keys).unwrap();
            let typed = DataChunk {
                columns: vec![Vector::Typed(data), Vector::Flat(payload.clone())],
                count: n,
            };
            let flat = DataChunk {
                columns: vec![Vector::Flat(keys), Vector::Flat(payload)],
                count: n,
            };
            for desc in [false, true] {
                let dir = if desc { SortDirection::Desc } else { SortDirection::Asc };
                let keys_ref = vec![SortKey { column_index: 0, direction: dir }];
                let typed_out = execute(&[typed.clone()], &keys_ref, None).unwrap();
                let flat_out = execute_force_compare(&[flat.clone()], &keys_ref).unwrap();
                let t: Vec<Vec<Value>> = typed_out.iter().flat_map(|c| c.to_rows()).collect();
                let f: Vec<Vec<Value>> = flat_out.iter().flat_map(|c| c.to_rows()).collect();
                assert_eq!(t, f, "trial {} desc {}", trial, desc);
            }
        }
    }

    #[test]
    fn test_typed_sort_multi_key() {
        use crate::common::column_data::ColumnData;
        // 多列：先 k1 降序再 k2 升序（索引比较路径）
        let k1 = ColumnData::try_from_values(&vec![Value::Int64(2), Value::Int64(1), Value::Int64(2)]).unwrap();
        let k2 = ColumnData::try_from_values(&vec![Value::Varchar("b".into()), Value::Varchar("a".into()), Value::Varchar("c".into())]).unwrap();
        let typed = DataChunk {
            columns: vec![Vector::Typed(k1), Vector::Typed(k2)],
            count: 3,
        };
        let keys = vec![
            SortKey { column_index: 0, direction: SortDirection::Desc },
            SortKey { column_index: 1, direction: SortDirection::Asc },
        ];
        let out = execute(&[typed], &keys, None).unwrap();
        let rows: Vec<Vec<Value>> = out.iter().flat_map(|c| c.to_rows()).collect();
        // (2,b) (2,c) (1,a)
        assert_eq!(rows[0], vec![Value::Int64(2), Value::Varchar("b".into())]);
        assert_eq!(rows[1], vec![Value::Int64(2), Value::Varchar("c".into())]);
        assert_eq!(rows[2], vec![Value::Int64(1), Value::Varchar("a".into())]);
    }

    /// 排序核心对比（radix vs sort_by），仅手动运行：cargo test --release --lib -- --ignored radix
    #[test]
    #[ignore]
    fn bench_radix_vs_compare() {
        use std::time::Instant;
        use rand::Rng;
        let mut rng = rand::thread_rng();
        let n = 1_000_000usize;
        let mut rows: Vec<Vec<Value>> = Vec::with_capacity(n);
        for i in 0..n {
            rows.push(vec![
                Value::Int64(rng.gen_range(-1_000_000..1_000_000)),
                Value::Int64(i as i64),
            ]);
        }
        let key = SortKey { column_index: 0, direction: SortDirection::Asc };

        let mut r1 = rows.clone();
        let t0 = Instant::now();
        assert!(try_radix_sort(&mut r1, &key));
        let radix_time = t0.elapsed();

        let mut r2 = rows;
        let t0 = Instant::now();
        r2.sort_by(|a, b| cmp_rows(a, b, &[key.clone()]));
        let cmp_time = t0.elapsed();

        println!("1M 行: radix = {:?}, sort_by = {:?}, radix 快 {}x",
                 radix_time, cmp_time,
                 cmp_time.as_nanos() as f64 / radix_time.as_nanos() as f64);
    }
}
