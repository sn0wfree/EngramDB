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
use crate::common::types::DataType;

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

    /// 迭代值（按行，NULL → Value::Null）
    pub fn iter_values(&self) -> impl Iterator<Item = Value> + '_ {
        (0..self.len()).map(|i| self.get(i))
    }

    /// 追加另一列数据到尾部（两列类型必须一致）
    pub fn append(&mut self, other: &ColumnData) {
        match (&mut self.values, &other.values) {
            (ColumnValue::Boolean(a), ColumnValue::Boolean(b)) => a.extend_from_slice(b),
            (ColumnValue::Int32(a), ColumnValue::Int32(b)) => a.extend_from_slice(b),
            (ColumnValue::Int64(a), ColumnValue::Int64(b)) => a.extend_from_slice(b),
            (ColumnValue::Float32(a), ColumnValue::Float32(b)) => a.extend_from_slice(b),
            (ColumnValue::Float64(a), ColumnValue::Float64(b)) => a.extend_from_slice(b),
            (ColumnValue::Varchar(a), ColumnValue::Varchar(b)) => a.extend_from_slice(b),
            (ColumnValue::Json(a), ColumnValue::Json(b)) => a.extend_from_slice(b),
            (ColumnValue::Blob(a), ColumnValue::Blob(b)) => a.extend_from_slice(b),
            (ColumnValue::Vector(a), ColumnValue::Vector(b)) => a.extend_from_slice(b),
            (ColumnValue::VectorInt8(a), ColumnValue::VectorInt8(b)) => a.extend_from_slice(b),
            (ColumnValue::Timestamp(a), ColumnValue::Timestamp(b)) => a.extend_from_slice(b),
            _ => unreachable!("append: 列类型不一致"),
        }
        let old_len = self.values.len() - other.values.len();
        // NULL 位图合并（self 无 NULL 而 other 有 → 需重建，标记 other 段）
        match (&self.nulls, &other.nulls) {
            (None, None) => {}
            (None, Some(n)) => {
                let mut bv = BitVec::new(self.values.len());
                for i in 0..n.len() {
                    if n.test(i) {
                        bv.set(old_len + i, true);
                    }
                }
                self.nulls = Some(bv);
            }
            (Some(bv), None) => {
                let mut nb = BitVec::new(self.values.len());
                for i in 0..old_len {
                    if bv.test(i) {
                        nb.set(i, true);
                    }
                }
                self.nulls = Some(nb);
            }
            (Some(bv), Some(n)) => {
                let mut nb = BitVec::new(self.values.len());
                for i in 0..old_len {
                    if bv.test(i) {
                        nb.set(i, true);
                    }
                }
                for i in 0..n.len() {
                    if n.test(i) {
                        nb.set(old_len + i, true);
                    }
                }
                self.nulls = Some(nb);
            }
        }
    }

    /// 按表列 DataType 从 Value 数组构造（存储层用，M1）。
    ///
    /// 值转换规则与 `serialize_values` 完全一致：
    /// - 兼容变体数值转换（Int32 → Int64 列、Float32 → Float64 列、Timestamp ↔ Int64 等）
    /// - 不兼容变体 → NULL 占位（nulls 标记，位置精确——优于磁盘格式的数字 NULL 简化）
    pub fn from_values_typed(values: &[Value], data_type: &DataType) -> ColumnData {
        let (mut arr, mut nulls): (ColumnValue, Option<BitVec>) = match data_type {
            DataType::Boolean => (ColumnValue::Boolean(Vec::with_capacity(values.len())), None),
            DataType::Int32 => (ColumnValue::Int32(Vec::with_capacity(values.len())), None),
            DataType::Int64 => (ColumnValue::Int64(Vec::with_capacity(values.len())), None),
            DataType::Float32 => (ColumnValue::Float32(Vec::with_capacity(values.len())), None),
            DataType::Float64 => (ColumnValue::Float64(Vec::with_capacity(values.len())), None),
            DataType::Varchar => (ColumnValue::Varchar(Vec::with_capacity(values.len())), None),
            DataType::Json => (ColumnValue::Json(Vec::with_capacity(values.len())), None),
            DataType::Blob => (ColumnValue::Blob(Vec::with_capacity(values.len())), None),
            DataType::Vector { .. } => (ColumnValue::Vector(Vec::with_capacity(values.len())), None),
            DataType::VectorInt8 { .. } => (ColumnValue::VectorInt8(Vec::with_capacity(values.len())), None),
            DataType::Timestamp => (ColumnValue::Timestamp(Vec::with_capacity(values.len())), None),
        };

        for (i, v) in values.iter().enumerate() {
            if push_typed(&mut arr, v, data_type) {
                continue;
            }
            // NULL 或类型不兼容 → NULL 占位
            if nulls.is_none() {
                nulls = Some(BitVec::new(values.len()));
            }
            let bv = nulls.as_mut().unwrap();
            bv.set(i, true);
            push_null_placeholder(&mut arr);
        }

        ColumnData {
            values: arr,
            nulls,
        }
    }

    /// 序列化为磁盘字节（与 `serialize_values` 字节格式完全一致）
    ///
    /// 注意：数字类型的 NULL 序列化为 0（与既有格式一致，位置信息仅在
    /// 内存态 nulls 中保留；重新加载后数字 NULL 位置丢失——既有行为）。
    pub fn serialize_typed(&self, data_type: &DataType) -> Vec<u8> {
        let mut buf = Vec::with_capacity(self.len() * 8);
        for i in 0..self.len() {
            let is_null = match &self.nulls {
                Some(n) => n.test(i),
                None => false,
            };
            write_typed(&mut buf, &self.values, i, data_type, is_null);
        }
        buf
    }

    /// 从磁盘字节构造类型化列（与 `deserialize_values` 语义一致）
    ///
    /// 数字类型无 NULL 标记（既有格式限制）；Varchar/Blob/Json/Vector 零长度 → NULL。
    pub fn deserialize_typed(data: &[u8], data_type: &DataType, count: usize) -> ColumnData {
        let mut offset = 0usize;
        let mut nulls = BitVec::new(count);
        let mut any_null = false;

        let mut arr: ColumnValue = match data_type {
            DataType::Boolean => ColumnValue::Boolean(Vec::with_capacity(count)),
            DataType::Int32 => ColumnValue::Int32(Vec::with_capacity(count)),
            DataType::Int64 => ColumnValue::Int64(Vec::with_capacity(count)),
            DataType::Float32 => ColumnValue::Float32(Vec::with_capacity(count)),
            DataType::Float64 => ColumnValue::Float64(Vec::with_capacity(count)),
            DataType::Varchar => ColumnValue::Varchar(Vec::with_capacity(count)),
            DataType::Json => ColumnValue::Json(Vec::with_capacity(count)),
            DataType::Blob => ColumnValue::Blob(Vec::with_capacity(count)),
            DataType::Vector { .. } => ColumnValue::Vector(Vec::with_capacity(count)),
            DataType::VectorInt8 { .. } => ColumnValue::VectorInt8(Vec::with_capacity(count)),
            DataType::Timestamp => ColumnValue::Timestamp(Vec::with_capacity(count)),
        };

        for i in 0..count {
            let is_null = match &mut arr {
                ColumnValue::Boolean(a) => match data.get(offset) {
                    Some(0) => {
                        a.push(false);
                        offset += 1;
                        false
                    }
                    Some(1) => {
                        a.push(true);
                        offset += 1;
                        false
                    }
                    _ => {
                        offset += 1;
                        true
                    }
                },
                ColumnValue::Int32(a) => {
                    if offset + 4 <= data.len() {
                        a.push(i32::from_le_bytes(data[offset..offset + 4].try_into().unwrap()));
                        offset += 4;
                        false
                    } else {
                        true
                    }
                }
                ColumnValue::Int64(a) => {
                    if offset + 8 <= data.len() {
                        a.push(i64::from_le_bytes(data[offset..offset + 8].try_into().unwrap()));
                        offset += 8;
                        false
                    } else {
                        true
                    }
                }
                ColumnValue::Float32(a) => {
                    if offset + 4 <= data.len() {
                        a.push(f32::from_le_bytes(data[offset..offset + 4].try_into().unwrap()));
                        offset += 4;
                        false
                    } else {
                        true
                    }
                }
                ColumnValue::Float64(a) => {
                    if offset + 8 <= data.len() {
                        a.push(f64::from_le_bytes(data[offset..offset + 8].try_into().unwrap()));
                        offset += 8;
                        false
                    } else {
                        true
                    }
                }
                ColumnValue::Varchar(a) | ColumnValue::Json(a) => {
                    if offset + 4 <= data.len() {
                        let len = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
                        offset += 4;
                        if offset + len <= data.len() {
                            // 与 deserialize_values 一致：长度 0 → 空串（非 NULL）
                            a.push(String::from_utf8_lossy(&data[offset..offset + len]).to_string());
                            offset += len;
                            false
                        } else {
                            true
                        }
                    } else {
                        true
                    }
                }
                ColumnValue::Blob(a) => {
                    if offset + 4 <= data.len() {
                        let len = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
                        offset += 4;
                        if offset + len <= data.len() {
                            // 与 deserialize_values 一致：长度 0 → 空 Blob（非 NULL）
                            a.push(data[offset..offset + len].to_vec());
                            offset += len;
                            false
                        } else {
                            true
                        }
                    } else {
                        true
                    }
                }
                ColumnValue::Vector(a) => {
                    if offset + 4 <= data.len() {
                        let dim = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
                        offset += 4;
                        let byte_len = dim * 4;
                        // 与 deserialize_values 一致：dim == 0 → NULL
                        if dim > 0 && offset + byte_len <= data.len() {
                            let vec: Vec<f32> = data[offset..offset + byte_len]
                                .chunks_exact(4)
                                .map(|c| f32::from_le_bytes(c.try_into().unwrap()))
                                .collect();
                            a.push(vec);
                            offset += byte_len;
                            false
                        } else {
                            true
                        }
                    } else {
                        true
                    }
                }
                ColumnValue::VectorInt8(a) => {
                    if offset + 4 <= data.len() {
                        let dim = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
                        offset += 4;
                        if offset + dim <= data.len() {
                            // 与 deserialize_values 一致：dim 0 → 空向量（非 NULL）
                            a.push(data[offset..offset + dim].iter().map(|&b| b as i8).collect());
                            offset += dim;
                            false
                        } else {
                            true
                        }
                    } else {
                        true
                    }
                }
                ColumnValue::Timestamp(a) => {
                    if offset + 8 <= data.len() {
                        a.push(i64::from_le_bytes(data[offset..offset + 8].try_into().unwrap()));
                        offset += 8;
                        false
                    } else {
                        true
                    }
                }
            };
            if is_null {
                push_null_placeholder(&mut arr);
                nulls.set(i, true);
                any_null = true;
            }
        }

        ColumnData {
            values: arr,
            nulls: if any_null { Some(nulls) } else { None },
        }
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

// ============================================================================
// DataType 驱动的类型化转换辅助（M1）
// ============================================================================

/// 将 Value 按 DataType 推入类型数组；返回 false 表示 NULL / 类型不兼容（需占位）
fn push_typed(arr: &mut ColumnValue, v: &Value, data_type: &DataType) -> bool {
    match (data_type, v) {
        (DataType::Boolean, Value::Boolean(b)) => {
            if let ColumnValue::Boolean(a) = arr {
                a.push(*b);
            }
            true
        }
        (DataType::Int32, Value::Int32(i)) => {
            if let ColumnValue::Int32(a) = arr {
                a.push(*i);
            }
            true
        }
        (DataType::Int32, Value::Int64(i)) => {
            if let ColumnValue::Int32(a) = arr {
                a.push(*i as i32);
            }
            true
        }
        (DataType::Int64, Value::Int32(i)) => {
            if let ColumnValue::Int64(a) = arr {
                a.push(*i as i64);
            }
            true
        }
        (DataType::Int64, Value::Int64(i)) => {
            if let ColumnValue::Int64(a) = arr {
                a.push(*i);
            }
            true
        }
        (DataType::Float32, Value::Float32(f)) => {
            if let ColumnValue::Float32(a) = arr {
                a.push(*f);
            }
            true
        }
        (DataType::Float32, Value::Float64(f)) => {
            if let ColumnValue::Float32(a) = arr {
                a.push(*f as f32);
            }
            true
        }
        (DataType::Float32, Value::Int32(i)) => {
            if let ColumnValue::Float32(a) = arr {
                a.push(*i as f32);
            }
            true
        }
        (DataType::Float32, Value::Int64(i)) => {
            if let ColumnValue::Float32(a) = arr {
                a.push(*i as f32);
            }
            true
        }
        (DataType::Float64, Value::Float32(f)) => {
            if let ColumnValue::Float64(a) = arr {
                a.push(*f as f64);
            }
            true
        }
        (DataType::Float64, Value::Float64(f)) => {
            if let ColumnValue::Float64(a) = arr {
                a.push(*f);
            }
            true
        }
        (DataType::Float64, Value::Int32(i)) => {
            if let ColumnValue::Float64(a) = arr {
                a.push(*i as f64);
            }
            true
        }
        (DataType::Float64, Value::Int64(i)) => {
            if let ColumnValue::Float64(a) = arr {
                a.push(*i as f64);
            }
            true
        }
        (DataType::Varchar, Value::Varchar(s)) => {
            if let ColumnValue::Varchar(a) = arr {
                a.push(s.clone());
            }
            true
        }
        (DataType::Json, Value::Json(s)) => {
            if let ColumnValue::Json(a) = arr {
                a.push(s.clone());
            }
            true
        }
        (DataType::Blob, Value::Blob(b)) => {
            if let ColumnValue::Blob(a) = arr {
                a.push(b.clone());
            }
            true
        }
        (DataType::Vector { .. }, Value::Vector(vec)) => {
            if let ColumnValue::Vector(a) = arr {
                a.push(vec.clone());
            }
            true
        }
        (DataType::VectorInt8 { .. }, Value::VectorInt8(vec)) => {
            if let ColumnValue::VectorInt8(a) = arr {
                a.push(vec.clone());
            }
            true
        }
        (DataType::Timestamp, Value::Timestamp(t)) => {
            if let ColumnValue::Timestamp(a) = arr {
                a.push(*t);
            }
            true
        }
        (DataType::Timestamp, Value::Int64(i)) => {
            if let ColumnValue::Timestamp(a) = arr {
                a.push(*i);
            }
            true
        }
        (DataType::Timestamp, Value::Int32(i)) => {
            if let ColumnValue::Timestamp(a) = arr {
                a.push(*i as i64);
            }
            true
        }
        // NULL / 类型不兼容 → 占位
        _ => false,
    }
}

/// NULL 占位（保持数组与行对齐）
fn push_null_placeholder(arr: &mut ColumnValue) {
    match arr {
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
}

/// 将第 i 行按 DataType 写入字节（与 serialize_values 字节格式一致；NULL → 0/2/空串占位）
fn write_typed(buf: &mut Vec<u8>, values: &ColumnValue, i: usize, data_type: &DataType, is_null: bool) {
    match (data_type, values) {
        (DataType::Boolean, ColumnValue::Boolean(a)) => {
            buf.push(if is_null { 2 } else if a[i] { 1 } else { 0 });
        }
        (DataType::Int32, ColumnValue::Int32(a)) => {
            buf.extend_from_slice(&(if is_null { 0 } else { a[i] }).to_le_bytes());
        }
        (DataType::Int64, ColumnValue::Int64(a)) => {
            buf.extend_from_slice(&(if is_null { 0 } else { a[i] }).to_le_bytes());
        }
        (DataType::Float32, ColumnValue::Float32(a)) => {
            buf.extend_from_slice(&(if is_null { 0.0 } else { a[i] }).to_le_bytes());
        }
        (DataType::Float64, ColumnValue::Float64(a)) => {
            buf.extend_from_slice(&(if is_null { 0.0 } else { a[i] }).to_le_bytes());
        }
        (DataType::Varchar, ColumnValue::Varchar(a)) => {
            if is_null {
                buf.extend_from_slice(&0u32.to_le_bytes());
            } else {
                buf.extend_from_slice(&(a[i].len() as u32).to_le_bytes());
                buf.extend_from_slice(a[i].as_bytes());
            }
        }
        (DataType::Json, ColumnValue::Json(a)) => {
            if is_null {
                buf.extend_from_slice(&0u32.to_le_bytes());
            } else {
                buf.extend_from_slice(&(a[i].len() as u32).to_le_bytes());
                buf.extend_from_slice(a[i].as_bytes());
            }
        }
        (DataType::Blob, ColumnValue::Blob(a)) => {
            if is_null {
                buf.extend_from_slice(&0u32.to_le_bytes());
            } else {
                buf.extend_from_slice(&(a[i].len() as u32).to_le_bytes());
                buf.extend_from_slice(&a[i]);
            }
        }
        (DataType::Vector { .. }, ColumnValue::Vector(a)) => {
            if is_null {
                buf.extend_from_slice(&0u32.to_le_bytes());
            } else {
                buf.extend_from_slice(&(a[i].len() as u32).to_le_bytes());
                for f in &a[i] {
                    buf.extend_from_slice(&f.to_le_bytes());
                }
            }
        }
        (DataType::VectorInt8 { .. }, ColumnValue::VectorInt8(a)) => {
            if is_null {
                buf.extend_from_slice(&0u32.to_le_bytes());
            } else {
                buf.extend_from_slice(&(a[i].len() as u32).to_le_bytes());
                for b in &a[i] {
                    buf.push(*b as u8);
                }
            }
        }
        (DataType::Timestamp, ColumnValue::Timestamp(a)) => {
            buf.extend_from_slice(&(if is_null { 0 } else { a[i] }).to_le_bytes());
        }
        _ => unreachable!("write_typed: 类型不匹配"),
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

    // ========================================================================
    // M1：DataType 驱动的类型化转换
    // ========================================================================

    #[test]
    fn test_from_values_typed_int64() {
        let values = vec![
            Value::Int32(5),  // 兼容转换
            Value::Null,
            Value::Int64(7),
        ];
        let col = ColumnData::from_values_typed(&values, &DataType::Int64);
        assert_eq!(col.len(), 3);
        assert_eq!(col.get(0), Value::Int64(5));
        assert_eq!(col.get(1), Value::Null);
        assert_eq!(col.get(2), Value::Int64(7));
        assert!(col.has_nulls());
    }

    #[test]
    fn test_from_values_typed_incompatible_is_null() {
        // Varchar 值进 Int64 列 → NULL（与 serialize_values 的 NULL 简化一致）
        let values = vec![Value::Varchar("x".into()), Value::Int64(1)];
        let col = ColumnData::from_values_typed(&values, &DataType::Int64);
        assert_eq!(col.get(0), Value::Null);
        assert_eq!(col.get(1), Value::Int64(1));
    }

    #[test]
    fn test_from_values_typed_all_types() {
        // Boolean / Float32→Float64 / Timestamp↔Int64 / Varchar / Blob / Vector
        let col = ColumnData::from_values_typed(&[Value::Float32(1.5)], &DataType::Float64);
        assert_eq!(col.get(0), Value::Float64(1.5));
        let col = ColumnData::from_values_typed(&[Value::Int64(9)], &DataType::Timestamp);
        assert_eq!(col.get(0), Value::Timestamp(9));
        // Timestamp → Int64 列：serialize_values 不支持（NULL）
        let col = ColumnData::from_values_typed(&[Value::Timestamp(9)], &DataType::Int64);
        assert_eq!(col.get(0), Value::Null);
        let col = ColumnData::from_values_typed(&[Value::Boolean(true), Value::Null], &DataType::Boolean);
        assert_eq!(col.get(0), Value::Boolean(true));
        assert_eq!(col.get(1), Value::Null);
        let col = ColumnData::from_values_typed(&[Value::Varchar("s".into())], &DataType::Varchar);
        assert_eq!(col.get(0), Value::Varchar("s".into()));
        let col = ColumnData::from_values_typed(&[Value::Blob(vec![1, 2])], &DataType::Blob);
        assert_eq!(col.get(0), Value::Blob(vec![1, 2]));
        let col = ColumnData::from_values_typed(&[Value::Vector(vec![0.5])], &DataType::Vector { dim: 1 });
        assert_eq!(col.get(0), Value::Vector(vec![0.5]));
        let col = ColumnData::from_values_typed(&[Value::VectorInt8(vec![1])], &DataType::VectorInt8 { dim: 1 });
        assert_eq!(col.get(0), Value::VectorInt8(vec![1]));
        let col = ColumnData::from_values_typed(&[Value::Json("{}".into())], &DataType::Json);
        assert_eq!(col.get(0), Value::Json("{}".into()));
    }

    #[test]
    fn test_typed_serialize_matches_value_format() {
        // 交叉验证：ColumnData 序列化与 serialize_values 字节一致（磁盘格式兼容）
        use crate::storage::column_store::{serialize_values, deserialize_values};
        let cases: Vec<(Vec<Value>, DataType)> = vec![
            (vec![Value::Int64(1), Value::Null, Value::Int64(-3)], DataType::Int64),
            (vec![Value::Int32(1), Value::Null], DataType::Int32),
            (vec![Value::Float64(1.5), Value::Null], DataType::Float64),
            (vec![Value::Boolean(true), Value::Null, Value::Boolean(false)], DataType::Boolean),
            (vec![Value::Varchar("hi".into()), Value::Null], DataType::Varchar),
            (vec![Value::Timestamp(123), Value::Null], DataType::Timestamp),
            (vec![Value::Blob(vec![1, 2]), Value::Null], DataType::Blob),
        ];
        for (values, dt) in cases {
            let col = ColumnData::from_values_typed(&values, &dt);
            let typed_bytes = col.serialize_typed(&dt);
            let old_bytes = serialize_values(&values, &dt);
            assert_eq!(typed_bytes, old_bytes, "serialize mismatch for {:?}", dt);

            // 反序列化：typed 与 value 结果一致
            let back_col = ColumnData::deserialize_typed(&typed_bytes, &dt, values.len());
            let back_vals = deserialize_values(&typed_bytes, &dt, values.len());
            let col_vals: Vec<Value> = back_col.to_values();
            assert_eq!(col_vals, back_vals, "deserialize mismatch for {:?}", dt);
        }
    }

    #[test]
    fn test_typed_serialize_varchar_null_positions_lost() {
        // Varchar 的 NULL：内存态精确，序列化后位置丢失（0 长度 → 空串，既有行为）
        let values = vec![Value::Varchar("a".into()), Value::Null, Value::Varchar("b".into())];
        let col = ColumnData::from_values_typed(&values, &DataType::Varchar);
        assert!(col.has_nulls());
        let bytes = col.serialize_typed(&DataType::Varchar);
        let back = ColumnData::deserialize_typed(&bytes, &DataType::Varchar, 3);
        assert_eq!(back.get(0), Value::Varchar("a".into()));
        assert_eq!(back.get(1), Value::Varchar("".into())); // NULL → 空串（与 deserialize_values 一致）
        assert_eq!(back.get(2), Value::Varchar("b".into()));
        assert!(!back.has_nulls());
    }

    #[test]
    fn test_typed_serialize_int_null_positions_lost() {
        // 数字 NULL：序列化后位置丢失（既有磁盘格式限制）
        let values = vec![Value::Int64(5), Value::Null, Value::Int64(7)];
        let col = ColumnData::from_values_typed(&values, &DataType::Int64);
        let bytes = col.serialize_typed(&DataType::Int64);
        let back = ColumnData::deserialize_typed(&bytes, &DataType::Int64, 3);
        assert_eq!(back.get(0), Value::Int64(5));
        assert_eq!(back.get(1), Value::Int64(0)); // NULL → 0（既有行为）
        assert_eq!(back.get(2), Value::Int64(7));
        assert!(!back.has_nulls());
    }

    #[test]
    fn test_typed_append() {
        let mut col = ColumnData::from_values_typed(&[Value::Int64(1), Value::Null], &DataType::Int64);
        let other = ColumnData::from_values_typed(&[Value::Null, Value::Int64(4)], &DataType::Int64);
        col.append(&other);
        assert_eq!(col.len(), 4);
        assert_eq!(col.get(0), Value::Int64(1));
        assert_eq!(col.get(1), Value::Null);
        assert_eq!(col.get(2), Value::Null);
        assert_eq!(col.get(3), Value::Int64(4));
    }

    #[test]
    fn test_typed_append_no_nulls() {
        let mut col = ColumnData::from_values_typed(&[Value::Int64(1)], &DataType::Int64);
        let other = ColumnData::from_values_typed(&[Value::Int64(2)], &DataType::Int64);
        col.append(&other);
        assert_eq!(col.len(), 2);
        assert!(!col.has_nulls());
        assert!(col.nulls.is_none());
    }

    #[test]
    fn test_typed_iter_values() {
        let col = ColumnData::from_values_typed(
            &[Value::Int64(1), Value::Null, Value::Int64(3)],
            &DataType::Int64,
        );
        let vals: Vec<Value> = col.iter_values().collect();
        assert_eq!(vals, vec![Value::Int64(1), Value::Null, Value::Int64(3)]);
    }
}
