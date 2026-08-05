//! ColumnData：类型化列数据（S2 核心，M0）
//!
//! 目标：替代执行器/存储层的 `Vec<Value>` 列表示，消除 Value enum 的
//! tag 间接层（32B/值 → 8B 连续数组），内存连续、cache 友好、可 SIMD。
//!
//! M0 范围：类型 + Value 双向转换 + 等价性测试（不接线）。
//!
//! 设计决策：
//! - 列内所有非 NULL 值必须同一种 Value 类型（混合列 → 转换返回 None，
//!   调用方保持 Flat 路径；保证 `get()` 与原 Value 完全一致，
//!   Group By / Join 的 Int32(1) ≠ Int64(1) 语义不变）
//! - NULL 用独立 BitVec（1 bit/row），None = 无 NULL（快路径）
//! - Varchar/Json 第一版用 Vec<String> 简化，offset+data 双数组后续优化
//! - Vector/VectorInt8/Blob 每行一个 Vec

use crate::Value;

/// 1 bit/row 的 NULL 位图（自实现，避免引入依赖）
#[derive(Debug, Clone, Default)]
pub struct BitVec {
    words: Vec<u64>,
    len: usize,
}

impl BitVec {
    /// 创建 len 位，初始全 false
    pub fn new(len: usize) -> Self {
        Self {
            words: vec![0; (len + 63) / 64],
            len,
        }
    }

    /// 创建全 true 位图（初始全 NULL）
    pub fn all_ones(len: usize) -> Self {
        let mut bv = Self::new(len);
        for i in 0..len {
            bv.set(i, true);
        }
        bv
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// 设置第 i 位
    #[inline]
    pub fn set(&mut self, i: usize, v: bool) {
        debug_assert!(i < self.len);
        let word = i / 64;
        let bit = i % 64;
        if v {
            self.words[word] |= 1u64 << bit;
        } else {
            self.words[word] &= !(1u64 << bit);
        }
    }

    /// 读取第 i 位
    #[inline]
    pub fn test(&self, i: usize) -> bool {
        debug_assert!(i < self.len);
        (self.words[i / 64] >> (i % 64)) & 1 == 1
    }

    /// NULL 数量
    pub fn count_ones(&self) -> usize {
        self.words.iter().map(|w| w.count_ones() as usize).sum()
    }
}

/// 类型化列值数组
#[derive(Debug, Clone)]
pub enum ColumnValue {
    Boolean(Vec<bool>),
    Int32(Vec<i32>),
    Int64(Vec<i64>),
    Float32(Vec<f32>),
    Float64(Vec<f64>),
    Varchar(Vec<String>),
    Json(Vec<String>),
    Blob(Vec<Vec<u8>>),
    Vector(Vec<Vec<f32>>),
    VectorInt8(Vec<Vec<i8>>),
    Timestamp(Vec<i64>),
}

/// 类型化列（值数组 + NULL 位图）
///
/// `nulls: None` 表示列中无 NULL（常见快路径，免位图检查）。
#[derive(Debug, Clone)]
pub struct ColumnData {
    pub values: ColumnValue,
    pub nulls: Option<BitVec>,
}

impl ColumnData {
    /// 行数
    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 无 NULL 快路径（nulls 为 None 或全 0）
    pub fn has_nulls(&self) -> bool {
        match &self.nulls {
            Some(n) => n.count_ones() > 0,
            None => false,
        }
    }

    /// 读取第 i 行（NULL → Value::Null；类型与原 Value 完全一致）
    pub fn get(&self, i: usize) -> Value {
        if let Some(nulls) = &self.nulls {
            if nulls.test(i) {
                return Value::Null;
            }
        }
        match &self.values {
            ColumnValue::Boolean(v) => Value::Boolean(v[i]),
            ColumnValue::Int32(v) => Value::Int32(v[i]),
            ColumnValue::Int64(v) => Value::Int64(v[i]),
            ColumnValue::Float32(v) => Value::Float32(v[i]),
            ColumnValue::Float64(v) => Value::Float64(v[i]),
            ColumnValue::Varchar(v) => Value::Varchar(v[i].clone()),
            ColumnValue::Json(v) => Value::Json(v[i].clone()),
            ColumnValue::Blob(v) => Value::Blob(v[i].clone()),
            ColumnValue::Vector(v) => Value::Vector(v[i].clone()),
            ColumnValue::VectorInt8(v) => Value::VectorInt8(v[i].clone()),
            ColumnValue::Timestamp(v) => Value::Timestamp(v[i]),
        }
    }

    /// 物化为 Value 数组（边界转换用）
    pub fn to_values(&self) -> Vec<Value> {
        (0..self.len()).map(|i| self.get(i)).collect()
    }

    /// 从 Value 数组转换。
    ///
    /// 要求：所有非 NULL 值同一种 Value 类型（混合列返回 None，
    /// 调用方保持 Flat 路径——保证 Group By / Join 的变体相等性语义不变）。
    pub fn try_from_values(values: &[Value]) -> Option<ColumnData> {
        // 探测类型（首个非 NULL 值决定）
        let mut probe: Option<ValueType> = None;
        let mut nulls: Option<BitVec> = None;
        let mut has_null = false;

        for v in values {
            if v.is_null() {
                has_null = true;
                continue;
            }
            let t = value_type(v)?;
            match probe {
                None => probe = Some(t),
                Some(p) if p == t => {}
                Some(_) => return None, // 混合类型 → 不支持
            }
        }

        let t = match probe {
            Some(t) => t,
            None => return None, // 全 NULL 列：无类型信息，交调用方处理
        };

        let mut bitvec = if has_null { Some(BitVec::new(values.len())) } else { None };
        let mut values_vec: ColumnValue = match t {
            ValueType::Boolean => ColumnValue::Boolean(Vec::with_capacity(values.len())),
            ValueType::Int32 => ColumnValue::Int32(Vec::with_capacity(values.len())),
            ValueType::Int64 => ColumnValue::Int64(Vec::with_capacity(values.len())),
            ValueType::Float32 => ColumnValue::Float32(Vec::with_capacity(values.len())),
            ValueType::Float64 => ColumnValue::Float64(Vec::with_capacity(values.len())),
            ValueType::Varchar => ColumnValue::Varchar(Vec::with_capacity(values.len())),
            ValueType::Json => ColumnValue::Json(Vec::with_capacity(values.len())),
            ValueType::Blob => ColumnValue::Blob(Vec::with_capacity(values.len())),
            ValueType::Vector => ColumnValue::Vector(Vec::with_capacity(values.len())),
            ValueType::VectorInt8 => ColumnValue::VectorInt8(Vec::with_capacity(values.len())),
            ValueType::Timestamp => ColumnValue::Timestamp(Vec::with_capacity(values.len())),
        };

        for (i, v) in values.iter().enumerate() {
            if v.is_null() {
                // NULL 行占位（与 values_vec 行对齐，get(i) 直接索引）
                if let Some(bv) = &mut bitvec {
                    bv.set(i, true);
                }
                match &mut values_vec {
                    ColumnValue::Boolean(a) => a.push(false),
                    ColumnValue::Int32(a) => a.push(0),
                    ColumnValue::Int64(a) => a.push(0),
                    ColumnValue::Float32(a) => a.push(0.0),
                    ColumnValue::Float64(a) => a.push(0.0),
                    ColumnValue::Varchar(a) => a.push(String::new()),
                    ColumnValue::Json(a) => a.push(String::new()),
                    ColumnValue::Blob(a) => a.push(Vec::new()),
                    ColumnValue::Vector(a) => a.push(Vec::new()),
                    ColumnValue::VectorInt8(a) => a.push(Vec::new()),
                    ColumnValue::Timestamp(a) => a.push(0),
                }
                continue;
            }
            match (&mut values_vec, v) {
                (ColumnValue::Boolean(a), Value::Boolean(x)) => a.push(*x),
                (ColumnValue::Int32(a), Value::Int32(x)) => a.push(*x),
                (ColumnValue::Int64(a), Value::Int64(x)) => a.push(*x),
                (ColumnValue::Float32(a), Value::Float32(x)) => a.push(*x),
                (ColumnValue::Float64(a), Value::Float64(x)) => a.push(*x),
                (ColumnValue::Varchar(a), Value::Varchar(x)) => a.push(x.clone()),
                (ColumnValue::Json(a), Value::Json(x)) => a.push(x.clone()),
                (ColumnValue::Blob(a), Value::Blob(x)) => a.push(x.clone()),
                (ColumnValue::Vector(a), Value::Vector(x)) => a.push(x.clone()),
                (ColumnValue::VectorInt8(a), Value::VectorInt8(x)) => a.push(x.clone()),
                (ColumnValue::Timestamp(a), Value::Timestamp(x)) => a.push(*x),
                _ => unreachable!("probe 已保证类型一致"),
            }
        }

        Some(ColumnData {
            values: values_vec,
            nulls: if has_null { bitvec } else { None },
        })
    }
}

/// 支持的列值类型（探测用）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ValueType {
    Boolean,
    Int32,
    Int64,
    Float32,
    Float64,
    Varchar,
    Json,
    Blob,
    Vector,
    VectorInt8,
    Timestamp,
}

fn value_type(v: &Value) -> Option<ValueType> {
    match v {
        Value::Null => None,
        Value::Boolean(_) => Some(ValueType::Boolean),
        Value::Int32(_) => Some(ValueType::Int32),
        Value::Int64(_) => Some(ValueType::Int64),
        Value::Float32(_) => Some(ValueType::Float32),
        Value::Float64(_) => Some(ValueType::Float64),
        Value::Varchar(_) => Some(ValueType::Varchar),
        Value::Json(_) => Some(ValueType::Json),
        Value::Blob(_) => Some(ValueType::Blob),
        Value::Vector(_) => Some(ValueType::Vector),
        Value::VectorInt8(_) => Some(ValueType::VectorInt8),
        Value::Timestamp(_) => Some(ValueType::Timestamp),
    }
}

impl ColumnValue {
    pub fn len(&self) -> usize {
        match self {
            ColumnValue::Boolean(v) => v.len(),
            ColumnValue::Int32(v) => v.len(),
            ColumnValue::Int64(v) => v.len(),
            ColumnValue::Float32(v) => v.len(),
            ColumnValue::Float64(v) => v.len(),
            ColumnValue::Varchar(v) => v.len(),
            ColumnValue::Json(v) => v.len(),
            ColumnValue::Blob(v) => v.len(),
            ColumnValue::Vector(v) => v.len(),
            ColumnValue::VectorInt8(v) => v.len(),
            ColumnValue::Timestamp(v) => v.len(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(values: Vec<Value>) {
        let col = ColumnData::try_from_values(&values).expect("pure column");
        assert_eq!(col.len(), values.len());
        let back: Vec<Value> = col.to_values();
        assert_eq!(back, values, "roundtrip mismatch");
        // 逐行 get 一致
        for (i, v) in values.iter().enumerate() {
            assert_eq!(&col.get(i), v);
        }
        // NULL 计数
        let expected_nulls = values.iter().filter(|v| v.is_null()).count();
        assert_eq!(col.has_nulls(), expected_nulls > 0);
    }

    #[test]
    fn test_bitvec() {
        let mut bv = BitVec::new(200);
        assert_eq!(bv.len(), 200);
        bv.set(0, true);
        bv.set(63, true);
        bv.set(64, true);
        bv.set(199, true);
        bv.set(100, true);
        assert!(bv.test(0));
        assert!(bv.test(63));
        assert!(bv.test(64));
        assert!(bv.test(199));
        assert!(bv.test(100));
        assert!(!bv.test(1));
        assert!(!bv.test(62));
        assert!(!bv.test(65));
        assert!(!bv.test(198));
        assert_eq!(bv.count_ones(), 5);
        bv.set(63, false);
        assert_eq!(bv.count_ones(), 4);
    }

    #[test]
    fn test_int64_roundtrip() {
        roundtrip(vec![
            Value::Int64(1), Value::Int64(-5), Value::Null, Value::Int64(i64::MAX),
        ]);
        roundtrip((0..300).map(|i| Value::Int64(i as i64)).collect());
    }

    #[test]
    fn test_int32_roundtrip() {
        roundtrip(vec![Value::Int32(1), Value::Int32(-5), Value::Null, Value::Int32(7)]);
    }

    #[test]
    fn test_float_roundtrip() {
        roundtrip(vec![Value::Float64(1.5), Value::Null, Value::Float64(-0.0)]);
        roundtrip(vec![Value::Float32(1.5), Value::Null, Value::Float32(-0.0)]);
    }

    #[test]
    fn test_varchar_roundtrip() {
        roundtrip(vec![
            Value::Varchar("hello".into()),
            Value::Null,
            Value::Varchar("".into()),
            Value::Varchar("中文测试".into()),
        ]);
    }

    #[test]
    fn test_json_blob_roundtrip() {
        roundtrip(vec![Value::Json("{\"a\":1}".into()), Value::Null, Value::Json("{}".into())]);
        roundtrip(vec![Value::Blob(vec![1, 2, 3]), Value::Null, Value::Blob(vec![])]);
    }

    #[test]
    fn test_vector_roundtrip() {
        roundtrip(vec![
            Value::Vector(vec![1.0, 2.0, 3.0]),
            Value::Null,
            Value::Vector(vec![]),
        ]);
        roundtrip(vec![
            Value::VectorInt8(vec![1, -1, 2]),
            Value::Null,
            Value::VectorInt8(vec![]),
        ]);
    }

    #[test]
    fn test_timestamp_boolean_roundtrip() {
        roundtrip(vec![Value::Timestamp(1718409600000), Value::Null, Value::Timestamp(0)]);
        roundtrip(vec![Value::Boolean(true), Value::Null, Value::Boolean(false)]);
    }

    #[test]
    fn test_no_nulls_fast_path() {
        let col = ColumnData::try_from_values(&vec![Value::Int64(1), Value::Int64(2)]).unwrap();
        assert!(col.nulls.is_none());
        assert!(!col.has_nulls());
    }

    #[test]
    fn test_all_null_rejected() {
        assert!(ColumnData::try_from_values(&[Value::Null, Value::Null]).is_none());
    }

    #[test]
    fn test_mixed_types_rejected() {
        // Int32 + Int64 混合 → None（保持 Flat 路径，Group By 变体相等性语义不变）
        assert!(ColumnData::try_from_values(&[Value::Int32(1), Value::Int64(2)]).is_none());
        assert!(ColumnData::try_from_values(&[Value::Int64(1), Value::Float64(2.0)]).is_none());
        assert!(ColumnData::try_from_values(&[Value::Int64(1), Value::Varchar("x".into())]).is_none());
        assert!(ColumnData::try_from_values(&[Value::Int64(1), Value::Null, Value::Boolean(true)]).is_none());
    }

    #[test]
    fn test_large_column() {
        // 跨多 word 的 NULL 位图
        let mut values = Vec::with_capacity(5000);
        for i in 0..5000 {
            if i % 7 == 0 {
                values.push(Value::Null);
            } else {
                values.push(Value::Int64(i as i64));
            }
        }
        roundtrip(values);
    }
}
