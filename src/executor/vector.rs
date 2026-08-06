//! 向量化数据结构
//!
//! DataChunk: 一批行数据，包含多个 Vector（每列一个）

use crate::Value;
use crate::common::column_data::ColumnData;

/// Vector 大小（每批行数）
pub const VECTOR_SIZE: usize = 2048;

/// 向量类型
///
/// S2-M2：新增 `Typed`（类型化列）——双路径并行：
/// - 扫描/存储层直接产出 Typed（零 Value 转换，内存连续）
/// - 表达式/算子逐步迁移到 Typed，未迁移路径经 `to_flat`/`get` 兼容
#[derive(Debug, Clone)]
pub enum Vector {
    Flat(Vec<Value>),
    Constant(Value, usize), // value, count
    Typed(ColumnData),
}

impl Vector {
    /// 创建空向量
    pub fn new() -> Self {
        Vector::Flat(Vec::new())
    }

    /// 从值列表创建向量
    pub fn from_values(values: Vec<Value>) -> Self {
        Vector::Flat(values)
    }

    /// 从类型化列创建（Typed variant）
    pub fn from_typed(data: ColumnData) -> Self {
        Vector::Typed(data)
    }

    /// 获取长度
    pub fn len(&self) -> usize {
        match self {
            Vector::Flat(v) => v.len(),
            Vector::Constant(_, n) => *n,
            Vector::Typed(d) => d.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 获取指定位置的值
    pub fn get(&self, idx: usize) -> Value {
        match self {
            Vector::Flat(v) => v[idx].clone(),
            Vector::Constant(val, _) => val.clone(),
            Vector::Typed(d) => d.get(idx),
        }
    }

    /// 追加值
    pub fn push(&mut self, val: Value) {
        match self {
            Vector::Flat(v) => v.push(val),
            Vector::Constant(_, n) => *n += 1,
            Vector::Typed(_) => unreachable!("不能向类型化向量 push 未知类型值"),
        }
    }

    /// 转换为 Flat 向量
    pub fn to_flat(&self) -> Vec<Value> {
        match self {
            Vector::Flat(v) => v.clone(),
            Vector::Constant(val, n) => vec![val.clone(); *n],
            Vector::Typed(d) => d.to_values(),
        }
    }

    /// 是否为类型化列（Typed variant）
    pub fn is_typed(&self) -> bool {
        matches!(self, Vector::Typed(_))
    }

    /// 尝试按引用访问类型化列
    pub fn as_typed(&self) -> Option<&ColumnData> {
        match self {
            Vector::Typed(d) => Some(d),
            _ => None,
        }
    }

    /// 尝试将 Flat 转为 Typed（纯类型列 → Typed；混合列保持 Flat）
    pub fn try_typed(self) -> Vector {
        match self {
            Vector::Flat(v) => match ColumnData::try_from_values(&v) {
                Some(d) => Vector::Typed(d),
                None => Vector::Flat(v),
            },
            other => other,
        }
    }
}

impl Default for Vector {
    fn default() -> Self {
        Self::new()
    }
}

/// 数据块（一批行）
#[derive(Debug, Clone)]
pub struct DataChunk {
    pub columns: Vec<Vector>,
    pub count: usize,
}

impl DataChunk {
    /// 创建空数据块
    pub fn new(num_columns: usize) -> Self {
        Self {
            columns: vec![Vector::new(); num_columns],
            count: 0,
        }
    }

    /// 从行数据创建
    pub fn from_rows(rows: &[Vec<Value>]) -> Self {
        if rows.is_empty() {
            return Self::new(0);
        }

        let num_cols = rows[0].len();
        let mut columns = vec![Vec::with_capacity(rows.len()); num_cols];

        for row in rows {
            for (i, val) in row.iter().enumerate() {
                columns[i].push(val.clone());
            }
        }

        let vectors = columns.into_iter().map(Vector::Flat).collect();

        Self {
            columns: vectors,
            count: rows.len(),
        }
    }

    /// 行数
    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// 列数
    pub fn num_columns(&self) -> usize {
        self.columns.len()
    }

    /// 转换为行向量
    pub fn to_rows(&self) -> Vec<Vec<Value>> {
        let mut rows = Vec::with_capacity(self.count);
        for i in 0..self.count {
            let mut row = Vec::with_capacity(self.columns.len());
            for col in &self.columns {
                row.push(col.get(i).clone());
            }
            rows.push(row);
        }
        rows
    }
}

/// 按 `VECTOR_SIZE` 批量将行转成 `DataChunk`（统一各算子边界转换逻辑）
///
/// 各算子（Filter/Projection/Sort/Aggregate 等）输入侧原本各写一份相同循环，
/// 现在统一调用此处。调用方拿到 `Vec<DataChunk>` 后再传入算子。
pub fn from_rows_batched(rows: &[Vec<Value>]) -> Vec<DataChunk> {
    if rows.is_empty() {
        return Vec::new();
    }
    rows.chunks(VECTOR_SIZE)
        .map(DataChunk::from_rows)
        .collect()
}

/// 将多个 `DataChunk` 扁平化为 `Vec<Vec<Value>>`（统一各算子输出边界）
///
/// 各算子输出侧原本各写一份相同循环，现在统一调用此处。
pub fn flatten_to_rows(chunks: &[DataChunk]) -> Vec<Vec<Value>> {
    let total: usize = chunks.iter().map(|c| c.count).sum();
    let mut all_rows = Vec::with_capacity(total);
    for chunk in chunks {
        all_rows.extend(chunk.to_rows());
    }
    all_rows
}


// ============================================================================
// 选择向量 (Selection Vector)
// 借鉴 ClickHouse：过滤时不拷贝数据，仅保留通过的行索引
// ============================================================================

/// 选择向量：记录通过过滤的行索引
///
/// 性能优化核心：避免过滤时逐行拷贝数据，仅维护索引数组。
/// 下游算子根据 selection 读取数据，实现零拷贝过滤。
#[derive(Debug, Clone)]
pub struct SelectionVector {
    /// 选中的行索引
    indices: Vec<usize>,
    /// 总数（等于 indices.len()，缓存避免重复计算）
    count: usize,
}

impl SelectionVector {
    /// 创建空的选择向量
    pub fn new() -> Self {
        Self {
            indices: Vec::new(),
            count: 0,
        }
    }

    /// 创建全选的选择向量（所有行都通过）
    pub fn all(count: usize) -> Self {
        let indices: Vec<usize> = (0..count).collect();
        let n = indices.len();
        Self {
            indices,
            count: n,
        }
    }

    /// 从索引列表创建
    pub fn from_indices(indices: Vec<usize>) -> Self {
        let count = indices.len();
        Self { indices, count }
    }

    /// 选中行数
    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// 添加一个选中的行索引
    pub fn push(&mut self, idx: usize) {
        self.indices.push(idx);
        self.count += 1;
    }

    /// 获取第 i 个选中行的原始索引
    pub fn index(&self, i: usize) -> usize {
        self.indices[i]
    }

    /// 获取所有索引的切片
    pub fn indices(&self) -> &[usize] {
        &self.indices
    }

    /// 应用选择向量到 Vector，返回过滤后的新 Vector
    pub fn apply_to_vector(&self, vector: &Vector) -> Vector {
        match vector {
            Vector::Flat(values) => {
                let mut result = Vec::with_capacity(self.count);
                for &idx in &self.indices {
                    result.push(values[idx].clone());
                }
                Vector::Flat(result)
            }
            Vector::Constant(val, _) => {
                Vector::Constant(val.clone(), self.count)
            }
            // S2-M2：Typed 列直接按索引 gather（类型数组零 Value 转换）
            Vector::Typed(data) => {
                Vector::Typed(data.gather(&self.indices))
            }
        }
    }

    /// 应用选择向量到 DataChunk，返回过滤后的新 DataChunk
    pub fn apply_to_chunk(&self, chunk: &DataChunk) -> DataChunk {
        if self.count == chunk.count {
            // 全选，直接克隆
            return chunk.clone();
        }

        let columns = chunk
            .columns
            .iter()
            .map(|col| self.apply_to_vector(col))
            .collect();

        DataChunk {
            columns,
            count: self.count,
        }
    }
}

impl Default for SelectionVector {
    fn default() -> Self {
        Self::new()
    }
}

/// 带选择向量的数据块（懒物化优化）
///
/// 借鉴 ClickHouse 懒物化思想：过滤阶段只计算 selection，
/// 不立即物化数据，直到真正需要时才物化。
/// 对于 Top N / LIMIT 查询，可大幅减少数据拷贝。
#[derive(Debug, Clone)]
pub struct LazyDataChunk {
    /// 原始数据块
    pub chunk: DataChunk,
    /// 选择向量（None 表示全选）
    pub selection: Option<SelectionVector>,
}

impl LazyDataChunk {
    /// 从 DataChunk 创建（默认全选）
    pub fn new(chunk: DataChunk) -> Self {
        Self {
            chunk,
            selection: None,
        }
    }

    /// 有效行数
    pub fn len(&self) -> usize {
        match &self.selection {
            Some(sel) => sel.len(),
            None => self.chunk.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 列数
    pub fn num_columns(&self) -> usize {
        self.chunk.num_columns()
    }

    /// 应用过滤：更新选择向量
    ///
    /// 这是懒物化的核心：过滤只改 selection，不碰数据
    pub fn filter<F>(&mut self, predicate: F)
    where
        F: Fn(usize) -> bool,
    {
        let total = self.chunk.len();

        match &mut self.selection {
            Some(sel) => {
                // 已有 selection，在此基础上进一步过滤
                let mut new_indices = Vec::with_capacity(sel.len());
                for i in 0..sel.len() {
                    let original_idx = sel.index(i);
                    if predicate(original_idx) {
                        new_indices.push(original_idx);
                    }
                }
                *sel = SelectionVector::from_indices(new_indices);
            }
            None => {
                // 第一次过滤，从全选开始
                let mut sel = SelectionVector::new();
                for i in 0..total {
                    if predicate(i) {
                        sel.push(i);
                    }
                }
                if sel.len() < total {
                    self.selection = Some(sel);
                }
                // 如果全通过，保持 None（全选优化）
            }
        }
    }

    /// 物化：将选择向量应用到数据，返回实际的 DataChunk
    pub fn materialize(self) -> DataChunk {
        match self.selection {
            Some(sel) => sel.apply_to_chunk(&self.chunk),
            None => self.chunk,
        }
    }

    /// 获取指定列的引用（考虑选择向量）
    ///
    /// 返回 (vector_ref, selection_opt)，下游算子可直接用 selection 索引访问
    pub fn column_with_selection(&self, col_idx: usize) -> (&Vector, Option<&SelectionVector>) {
        (&self.chunk.columns[col_idx], self.selection.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::column_data::ColumnData;
    use crate::common::types::DataType;

    fn typed_int64(vals: &[i64]) -> ColumnData {
        let v: Vec<Value> = vals.iter().map(|&x| Value::Int64(x)).collect();
        ColumnData::from_values_typed(&v, &DataType::Int64)
    }

    #[test]
    fn test_vector_flat_basics() {
        let mut v = Vector::from_values(vec![Value::Int64(1), Value::Int64(2)]);
        assert_eq!(v.len(), 2);
        assert!(!v.is_empty());
        assert_eq!(v.get(0), Value::Int64(1));
        v.push(Value::Int64(3));
        assert_eq!(v.len(), 3);
        assert_eq!(v.to_flat(), vec![Value::Int64(1), Value::Int64(2), Value::Int64(3)]);
        assert!(!v.is_typed());
        assert!(v.as_typed().is_none());
    }

    #[test]
    fn test_vector_constant() {
        let mut v = Vector::Constant(Value::Int64(7), 0);
        v.push(Value::Int64(9)); // push 对 Constant 只增计数
        assert_eq!(v.len(), 1);
        assert_eq!(v.get(0), Value::Int64(7), "Constant 任意位置返回同一值");
        assert_eq!(v.get(100), Value::Int64(7));
        assert_eq!(v.to_flat(), vec![Value::Int64(7)]);
        // all(5) 场景
        let c = Vector::Constant(Value::Varchar("x".into()), 5);
        assert_eq!(c.len(), 5);
        assert_eq!(c.to_flat().len(), 5);
    }

    #[test]
    fn test_vector_typed() {
        let d = typed_int64(&[10, 20, 30]);
        let v = Vector::from_typed(d);
        assert_eq!(v.len(), 3);
        assert!(v.is_typed());
        assert_eq!(v.get(1), Value::Int64(20));
        assert_eq!(v.to_flat(), vec![Value::Int64(10), Value::Int64(20), Value::Int64(30)]);
        assert!(v.as_typed().is_some());
    }

    #[test]
    fn test_try_typed_conversion() {
        // 纯类型列 → Typed
        let flat = Vector::from_values(vec![Value::Int64(1), Value::Int64(2)]);
        assert!(flat.try_typed().is_typed());
        // 混合列保持 Flat
        let mixed = Vector::from_values(vec![Value::Int64(1), Value::Varchar("a".into())]);
        assert!(!mixed.try_typed().is_typed());
        // Typed 幂等
        let typed = Vector::from_typed(typed_int64(&[1]));
        assert!(typed.try_typed().is_typed());
    }

    #[test]
    fn test_data_chunk_roundtrip() {
        let rows = vec![
            vec![Value::Int64(1), Value::Varchar("a".into())],
            vec![Value::Int64(2), Value::Varchar("b".into())],
        ];
        let chunk = DataChunk::from_rows(&rows);
        assert_eq!(chunk.len(), 2);
        assert_eq!(chunk.num_columns(), 2);
        assert_eq!(chunk.to_rows(), rows, "列式转置回行式必须无损");
        assert!(DataChunk::from_rows(&[]).is_empty());
        assert_eq!(DataChunk::new(3).num_columns(), 3);
    }

    #[test]
    fn test_selection_vector_basics() {
        let sel = SelectionVector::all(5);
        assert_eq!(sel.len(), 5);
        assert_eq!(sel.index(3), 3);
        assert_eq!(sel.indices(), &[0, 1, 2, 3, 4]);
        let sel2 = SelectionVector::from_indices(vec![2, 4]);
        assert_eq!(sel2.len(), 2);
        assert_eq!(sel2.index(0), 2);
        assert_eq!(sel2.index(1), 4);
        let mut s = SelectionVector::new();
        assert!(s.is_empty());
        s.push(9);
        s.push(11);
        assert_eq!(s.len(), 2);
        assert_eq!(s.index(1), 11);
    }

    #[test]
    fn test_selection_apply_flat_and_constant() {
        let flat = Vector::from_values(vec![Value::Int64(10), Value::Int64(20), Value::Int64(30)]);
        let sel = SelectionVector::from_indices(vec![0, 2]);
        let filtered = sel.apply_to_vector(&flat);
        assert_eq!(filtered.to_flat(), vec![Value::Int64(10), Value::Int64(30)]);

        let c = Vector::Constant(Value::Int64(5), 100);
        let fc = sel.apply_to_vector(&c);
        assert_eq!(fc.len(), 2, "Constant 过滤后只保留选中数");
        assert_eq!(fc.get(0), Value::Int64(5));
    }

    #[test]
    fn test_selection_apply_typed_gather() {
        let typed = Vector::from_typed(typed_int64(&[1, 2, 3, 4]));
        let sel = SelectionVector::from_indices(vec![3, 0]);
        let filtered = sel.apply_to_vector(&typed);
        assert!(filtered.is_typed(), "Typed gather 保持 Typed");
        assert_eq!(filtered.to_flat(), vec![Value::Int64(4), Value::Int64(1)]);
    }

    #[test]
    fn test_selection_apply_chunk_full_clone() {
        let chunk = DataChunk::from_rows(&[
            vec![Value::Int64(1), Value::Int64(2)],
            vec![Value::Int64(3), Value::Int64(4)],
        ]);
        let sel = SelectionVector::all(2);
        let out = sel.apply_to_chunk(&chunk);
        assert_eq!(out.len(), 2, "全选直接克隆");
        assert_eq!(out.to_rows(), chunk.to_rows());
    }

    #[test]
    fn test_lazy_chunk_filter_and_materialize() {
        let chunk = DataChunk::from_rows(&[
            vec![Value::Int64(1)],
            vec![Value::Int64(2)],
            vec![Value::Int64(3)],
            vec![Value::Int64(4)],
        ]);
        let mut lazy = LazyDataChunk::new(chunk.clone());
        assert_eq!(lazy.len(), 4, "无 selection 默认全选");
        lazy.filter(|i| i % 2 == 0);
        assert_eq!(lazy.len(), 2);
        // 二次过滤：在既有 selection 上累积
        lazy.filter(|i| i != 0);
        assert_eq!(lazy.len(), 1);
        let out = lazy.materialize();
        assert_eq!(out.to_rows(), vec![vec![Value::Int64(3)]]);
    }

    #[test]
    fn test_lazy_chunk_all_pass_keeps_none() {
        let chunk = DataChunk::from_rows(&[
            vec![Value::Int64(1)],
            vec![Value::Int64(2)],
        ]);
        let mut lazy = LazyDataChunk::new(chunk);
        lazy.filter(|_| true);
        assert!(lazy.selection.is_none(), "全通过保持 None（全选优化）");
        assert_eq!(lazy.len(), 2);
        let out = lazy.materialize();
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn test_lazy_chunk_column_with_selection() {
        let chunk = DataChunk::from_rows(&[
            vec![Value::Int64(1), Value::Varchar("a".into())],
            vec![Value::Int64(2), Value::Varchar("b".into())],
            vec![Value::Int64(3), Value::Varchar("c".into())],
        ]);
        let mut lazy = LazyDataChunk::new(chunk);
        lazy.filter(|i| i != 1);
        let (col, sel) = lazy.column_with_selection(0);
        assert_eq!(col.len(), 3);
        let sel = sel.unwrap();
        assert_eq!(sel.len(), 2);
        // 通过 selection 索引访问原列
        assert_eq!(col.get(sel.index(0)), Value::Int64(1));
        assert_eq!(col.get(sel.index(1)), Value::Int64(3));
    }
}
