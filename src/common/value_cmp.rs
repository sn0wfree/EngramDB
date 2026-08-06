//! Value 跨类型比较（canonical 语义）
//!
//! 历史问题：散落在 8+ 文件里的 Value 比较/相等性函数每份语义略有不同：
//! - NULL 处理：有的把 Null 视为最小值（与 SQLite/PostgreSQL 一致），有的用派生 `==`（Null == Null → true）
//! - 跨类型数值互通：column_store/sparse_index 支持 Int32↔Int64↔Float64↔Timestamp，
//!   sort/expression 还支持 Float64↔Timestamp；skiplist/aggregate/table 不支持
//! - Float64 NaN：partial_cmp().unwrap_or(Equal) vs to_bits() 比较
//! - Timestamp 与 Int64/Float64 互通：column_store/sparse_index 支持，其他不支持
//! - Boolean 排序：planner 用 `(!x).cmp(&!y)` 颠倒序（疑似 bug），skiplist 用标准序
//!
//! 本模块提供统一语义，所有调用方替换为本模块：
//!
//! ## `total_cmp`
//!
//! NULL-aware 跨类型比较。语义：
//! - NULL 最小（Null == Null → Equal，Null < 任何非空）
//! - 同类型按值（数值用同序；Float64 NaN → Equal；Varchar 字典序；Boolean false < true；
//!   Vector/VectorInt8/Blob 序列比；Timestamp 同 Int64）
//! - 跨类型数值互通：Int32↔Int64↔Timestamp（i64 拓宽），Int32/Int64/Timestamp↔Float64（f64 拓宽）
//! - 不相关类型：按 discriminant 兜底（保证全序，无 panic）
//!
//! ## `total_eq`
//!
//! 等价于 `total_cmp(a, b) == Equal`，但保留 NULL=NULL → true 语义
//! （用于 MinMax 探测、IN 列表等需要 NULL 自等的场景）。
//!
//! ## 不替换 `Value::cmp`（Ord impl）
//!
//! BTreeMap<Value, _> 依赖 Ord impl 的"按类型 rank 排序"语义保持不变。
//! `total_cmp` 是更宽松的"跨类型 NULL-aware 比较"，用于 WHERE/ORDER BY/MINMAX。
//!
//! ## 迁移清单
//!
//! | 调用方 | 原实现 | 替换为 |
//! |---|---|---|
//! | column_store::values_equal | 9-arm match + Null=Null→true | `total_eq` |
//! | column_store::value_less/greater | 18-arm + cross-type | `total_cmp` 比较 |
//! | sparse_index::value_gt | 22-arm + cross-type | `total_cmp` |
//! | expression::value_cmp | 24-arm + Float fallback | `total_cmp` |
//! | expression::value_eq | Null-aware partial eq | `total_eq` |
//! | sort::value_cmp | 30+ arm + cross-type | `total_cmp` |
//! | window::compare_values | 8-arm partial | `total_cmp` |
//! | planner::value_cmp_planner | 8-arm + Boolean 反 | `total_cmp` |
//! | skiplist::key_less | 6-arm Boolean `!x && y` | `total_cmp` |
//! | aggregate::value_less/greater | 4-arm | `total_cmp` |
//! | table::cmp_values_for_sort | Debug 字符串兜底 | `total_cmp` |

use std::cmp::Ordering;

use crate::Value as Value;

/// 类型 rank（用于"不相关类型兜底"，保证全序不 panic）
fn type_rank(v: &Value) -> u8 {
    match v {
        Value::Null => 0,
        Value::Boolean(_) => 1,
        Value::Int32(_) | Value::Int64(_) | Value::Timestamp(_) => 2,
        Value::Float32(_) | Value::Float64(_) => 3,
        Value::Varchar(_) => 4,
        Value::Json(_) => 5,
        Value::Vector(_) => 6,
        Value::VectorInt8(_) => 7,
        Value::Blob(_) => 8,
    }
}

/// 数值拓宽（i64 / f64），跨类型比较的统一通路
///
/// 返回 (rank, i64, f64)：rank 0 = 整数家族（Int32/Int64/Timestamp），
/// rank 1 = 浮点家族（Float32/Float64）。rank 不同 → 走 f64 拓宽。
fn numeric_widen(v: &Value) -> Option<(u8, Option<i64>, Option<f64>)> {
    match v {
        Value::Int32(x) => Some((0, Some(*x as i64), Some(*x as f64))),
        Value::Int64(x) => Some((0, Some(*x), Some(*x as f64))),
        Value::Timestamp(x) => Some((0, Some(*x), Some(*x as f64))),
        Value::Float32(x) => Some((1, None, Some(*x as f64))),
        Value::Float64(x) => Some((1, None, Some(*x))),
        _ => None,
    }
}

/// 同类型比较（数值同家族同子类型 → 直接比；其他 → 类型内 `cmp`）
fn same_type_cmp(a: &Value, b: &Value) -> Ordering {
    use Value::*;
    match (a, b) {
        (Null, Null) => Ordering::Equal,
        (Boolean(x), Boolean(y)) => x.cmp(y),
        (Int32(x), Int32(y)) => x.cmp(y),
        (Int64(x), Int64(y)) => x.cmp(y),
        (Float32(x), Float32(y)) => x.partial_cmp(y).unwrap_or(Ordering::Equal),
        (Float64(x), Float64(y)) => x.partial_cmp(y).unwrap_or(Ordering::Equal),
        (Timestamp(x), Timestamp(y)) => x.cmp(y),
        (Varchar(x), Varchar(y)) => x.cmp(y),
        (Json(x), Json(y)) => x.cmp(y),
        // Vector/VectorInt8/Blob：序列长度优先，再按位比
        (Vector(x), Vector(y)) => {
            let len_ord = x.len().cmp(&y.len());
            if len_ord != Ordering::Equal {
                return len_ord;
            }
            for (xi, yi) in x.iter().zip(y.iter()) {
                let ord = xi.partial_cmp(yi).unwrap_or(Ordering::Equal);
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            Ordering::Equal
        }
        (VectorInt8(x), VectorInt8(y)) => {
            let len_ord = x.len().cmp(&y.len());
            if len_ord != Ordering::Equal {
                return len_ord;
            }
            for (xi, yi) in x.iter().zip(y.iter()) {
                let ord = xi.cmp(yi);
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            Ordering::Equal
        }
        (Blob(x), Blob(y)) => x.cmp(y),
        // 不应到达：调用方保证 same_type 但 match 未穷尽（混合 rank）→ fallback
        _ => type_rank(a).cmp(&type_rank(b)),
    }
}

/// 跨类型 NULL-aware 比较
///
/// 语义详见模块文档。
pub fn total_cmp(a: &Value, b: &Value) -> Ordering {
    use Value::*;
    // 1. NULL 最小
    match (a, b) {
        (Null, Null) => return Ordering::Equal,
        (Null, _) => return Ordering::Less,
        (_, Null) => return Ordering::Greater,
        _ => {}
    }

    // 2. 同类型 → 直接比较
    if std::mem::discriminant(a) == std::mem::discriminant(b) {
        return same_type_cmp(a, b);
    }

    // 3. 跨类型数值互通
    if let (Some((ra, ia, fa)), Some((rb, ib, fb))) = (numeric_widen(a), numeric_widen(b)) {
        if ra == rb {
            // 同家族：都走 i64（如 Int32 vs Int64）
            if let (Some(x), Some(y)) = (ia, ib) {
                return x.cmp(&y);
            }
            // 不应到达
        }
        // 跨家族：用 f64 拓宽
        return fa.partial_cmp(&fb).unwrap_or(Ordering::Equal);
    }

    // 4. 不相关类型：type-rank 兜底
    type_rank(a).cmp(&type_rank(b))
}

/// 跨类型等价（含 NULL 自等）
///
/// - `Null == Null` → true
/// - `Null == X`（非空） → false
/// - 其他：委托 `total_cmp == Equal`
pub fn total_eq(a: &Value, b: &Value) -> bool {
    if a.is_null() && b.is_null() {
        return true;
    }
    if a.is_null() || b.is_null() {
        return false;
    }
    total_cmp(a, b) == Ordering::Equal
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Value::*;

    #[test]
    fn test_null_is_smallest() {
        assert_eq!(total_cmp(&Null, &Null), Ordering::Equal);
        assert_eq!(total_cmp(&Null, &Int64(0)), Ordering::Less);
        assert_eq!(total_cmp(&Int64(0), &Null), Ordering::Greater);
        assert_eq!(total_cmp(&Null, &Varchar("z".into())), Ordering::Less);
    }

    #[test]
    fn test_same_int_types() {
        assert_eq!(total_cmp(&Int32(1), &Int32(2)), Ordering::Less);
        assert_eq!(total_cmp(&Int64(5), &Int64(5)), Ordering::Equal);
        assert!(total_cmp(&Int64(100), &Int32(50)) == Ordering::Greater);
    }

    #[test]
    fn test_cross_type_int_widening() {
        // Int32 ↔ Int64：i64 拓宽
        assert_eq!(total_cmp(&Int32(5), &Int64(5)), Ordering::Equal);
        assert_eq!(total_cmp(&Int32(5), &Int64(10)), Ordering::Less);
        // Timestamp ↔ Int64：i64 互通
        assert_eq!(total_cmp(&Timestamp(100), &Int64(100)), Ordering::Equal);
        assert_eq!(total_cmp(&Timestamp(50), &Int64(100)), Ordering::Less);
        // Int32 ↔ Timestamp
        assert_eq!(total_cmp(&Int32(100), &Timestamp(100)), Ordering::Equal);
    }

    #[test]
    fn test_cross_type_int_to_float() {
        // Int32 ↔ Float64：f64 拓宽
        assert_eq!(total_cmp(&Int32(5), &Float64(5.0)), Ordering::Equal);
        assert_eq!(total_cmp(&Int32(5), &Float64(5.5)), Ordering::Less);
        // Timestamp ↔ Float64
        assert_eq!(total_cmp(&Timestamp(1000), &Float64(1000.0)), Ordering::Equal);
        assert_eq!(total_cmp(&Timestamp(1000), &Float64(1000.5)), Ordering::Less);
        // Float64 NaN → Equal
        assert_eq!(total_cmp(&Float64(f64::NAN), &Float64(5.0)), Ordering::Equal);
    }

    #[test]
    fn test_boolean_standard_order() {
        // false < true（标准序；与 planner 旧实现的 `(!x).cmp(&!y)` 反序不同）
        assert_eq!(total_cmp(&Boolean(false), &Boolean(true)), Ordering::Less);
        assert_eq!(total_cmp(&Boolean(true), &Boolean(false)), Ordering::Greater);
        assert_eq!(total_cmp(&Boolean(true), &Boolean(true)), Ordering::Equal);
    }

    #[test]
    fn test_varchar_lexicographic() {
        assert_eq!(total_cmp(&Varchar("a".into()), &Varchar("b".into())), Ordering::Less);
        assert_eq!(total_cmp(&Varchar("abc".into()), &Varchar("ab".into())), Ordering::Greater);
    }

    #[test]
    fn test_unrelated_types_total_order() {
        // 不相关类型（Int64 vs Varchar）：按 type_rank 兜底，全序不 panic
        assert_eq!(total_cmp(&Int64(100), &Varchar("a".into())), Ordering::Less); // rank 2 < 4
        assert_eq!(total_cmp(&Varchar("a".into()), &Float64(1.0)), Ordering::Greater); // rank 4 > 3
        // Vector vs Blob：rank 6 < 8
        assert_eq!(total_cmp(&Vector(vec![1.0]), &Blob(vec![1])), Ordering::Less);
    }

    #[test]
    fn test_total_eq_null_semantics() {
        // NULL 自等
        assert!(total_eq(&Null, &Null));
        assert!(!total_eq(&Null, &Int64(0)));
        assert!(!total_eq(&Int64(0), &Null));
    }

    #[test]
    fn test_total_eq_cross_type() {
        assert!(total_eq(&Int32(5), &Int64(5)));
        assert!(total_eq(&Int64(100), &Timestamp(100)));
        assert!(!total_eq(&Int32(5), &Int64(6)));
        assert!(total_eq(&Varchar("a".into()), &Varchar("a".into())));
    }

    // 语义矩阵：枚举与历史实现的差异点，确保合并后行为统一
    #[test]
    fn test_semantic_matrix_no_cross_type_implicit() {
        // 旧 aggregate/key_less 不支持跨类型，返回 false/Equal
        // 新 total_cmp 互通 → Greater/Less
        // 这是一个**有意**的语义升级（aggregate 单列内通常单类型，多列或 schema mismatch 走兜底）
        assert_eq!(total_cmp(&Int32(5), &Int64(10)), Ordering::Less);
    }

    #[test]
    fn test_semantic_matrix_boolean_inverted_in_old_planner() {
        // planner::value_cmp_planner 旧实现：`(!x).cmp(&!y)` → true (1) vs false (0) 颠倒
        // 新 total_cmp 用标准序：false < true
        // 这是一个**有意**的语义修正（planner 的 Boolean 反序疑似 bug）
        assert_eq!(total_cmp(&Boolean(true), &Boolean(false)), Ordering::Greater);
        assert_ne!(total_cmp(&Boolean(true), &Boolean(false)), Ordering::Less);
    }

    #[test]
    fn test_semantic_matrix_timestamp_in_old_table_sort_falls_through() {
        // table::cmp_values_for_sort 旧实现：Timestamp 落到 `format!("{:?}", ...)` 兜底
        // → 两个 Timestamp 用 Debug 字符串比（不稳定）
        // 新 total_cmp 用 i64 数值比
        assert_eq!(total_cmp(&Timestamp(100), &Timestamp(200)), Ordering::Less);
        assert_eq!(total_cmp(&Timestamp(200), &Timestamp(100)), Ordering::Greater);
    }

    #[test]
    fn test_semantic_matrix_float_nan_unified() {
        // Float64 NaN：lib.rs::Ord 用 to_bits()（高位 1 表示 NaN，部分平台负向）；
        // 其他实现用 partial_cmp().unwrap_or(Equal) → 恒等
        // 新 total_cmp 统一为 unwrap_or(Equal)（与多数实现一致）
        assert_eq!(total_cmp(&Float64(f64::NAN), &Float64(5.0)), Ordering::Equal);
        assert_eq!(total_cmp(&Float64(5.0), &Float64(f64::NAN)), Ordering::Equal);
        assert_eq!(total_cmp(&Float64(f64::NAN), &Float64(f64::NAN)), Ordering::Equal);
    }
}