//! MemoryEngine（v0.17.0 M2）：全内存表，高频读写，不持久化
//!
//! 定位：Agent 推理中间状态、session 缓存、计数器等临时数据。
//! - 主键 BTreeMap 点查 O(log n)，行式存储（数据量小的内存场景）
//! - 无 WAL / 无磁盘写入（默认）：进程退出数据丢失（符合预期）
//! - 复用现有事务管线（MVCC 版本链全在内存，无 I/O 开销）
//! - 扫描输出 Typed DataChunk（对齐 Columnar 的列式管道）

use std::collections::BTreeMap;

use crate::common::column_data::ColumnData;
use crate::common::error::{Result, EngramDbError};
use crate::common::types::{EngineType, TableDef};
use crate::executor::vector::{DataChunk, Vector};
use crate::storage::column_store::{matches_predicate, PredicateOp};
use crate::Value;

/// 内存表
///
/// `data` 按 row_id 索引（`None` = 已删除的 tombstone 行，row_id 不复用，
/// 与 Columnar 的 rowid 语义对齐，保证事务 apply 路径的正确性）。
pub struct MemoryTable {
    pub def: TableDef,
    data: Vec<Option<Vec<Value>>>,
    /// 主键 → row_id（表定义含主键列时维护）
    primary: BTreeMap<Value, u32>,
    next_row_id: u32,
}

impl MemoryTable {
    pub fn new(def: TableDef) -> Self {
        Self {
            def,
            data: Vec::new(),
            primary: BTreeMap::new(),
            next_row_id: 0,
        }
    }

    pub fn row_count(&self) -> u64 {
        // O(1)：def.row_count 在 insert/delete/truncate 时同步维护
        self.def.row_count
    }

    pub fn len(&self) -> usize {
        self.row_count() as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 主键列索引（None = 表无主键）
    fn pk_col(&self) -> Option<usize> {
        self.def.primary_key_index()
    }

    /// 批量插入：从 next_row_id 起分配 row_id
    pub fn insert(&mut self, rows: Vec<Vec<Value>>) -> Result<u64> {
        let n = rows.len();
        if n == 0 {
            return Ok(0);
        }
        // 主键冲突检查（含批量内重复）
        if let Some(pk) = self.pk_col() {
            for row in &rows {
                if let Some(cell) = row.get(pk) {
                    if self.primary.contains_key(cell) {
                        return Err(EngramDbError::ConstraintViolation(format!(
                            "UNIQUE constraint failed: {}={:?}",
                            self.def.columns[pk].name, cell
                        )));
                    }
                }
            }
        }
        let base = self.next_row_id;
        for (i, row) in rows.iter().enumerate() {
            let rid = base + i as u32;
            if let Some(pk) = self.pk_col() {
                if let Some(cell) = row.get(pk) {
                    self.primary.insert(cell.clone(), rid);
                }
            }
            self.data.push(Some(row.clone()));
        }
        self.next_row_id += n as u32;
        self.def.row_count += n as u64;
        Ok(n as u64)
    }

    /// 按指定 row_id 插入（事务 apply 路径，row_id 语义与 Columnar 对齐）
    pub fn insert_row(&mut self, row_id: u32, row: &[Value]) -> Result<()> {
        if let Some(pk) = self.pk_col() {
            if let Some(cell) = row.get(pk) {
                if self.primary.contains_key(cell) {
                    return Err(EngramDbError::ConstraintViolation(format!(
                        "UNIQUE constraint failed: {}={:?}",
                        self.def.columns[pk].name, cell
                    )));
                }
                self.primary.insert(cell.clone(), row_id);
            }
        }
        while (self.data.len() as u32) < row_id {
            self.data.push(None);
        }
        if (self.data.len() as u32) == row_id {
            self.data.push(Some(row.to_vec()));
        } else {
            self.data[row_id as usize] = Some(row.to_vec());
        }
        if row_id >= self.next_row_id {
            self.next_row_id = row_id + 1;
        }
        self.def.row_count += 1;
        Ok(())
    }

    /// 列式批量插入（事务 apply 路径）
    pub fn insert_columns(&mut self, columns: Vec<Vec<Value>>) -> Result<u64> {
        let num_rows = columns.first().map(|c| c.len()).unwrap_or(0);
        if num_rows == 0 {
            return Ok(0);
        }
        let num_cols = self.def.columns.len();
        let mut rows = Vec::with_capacity(num_rows);
        for i in 0..num_rows {
            let mut row = Vec::with_capacity(num_cols);
            for c in &columns {
                row.push(c.get(i).cloned().unwrap_or(Value::Null));
            }
            rows.push(row);
        }
        self.insert(rows)
    }

    /// 删除一行（标记 tombstone，row_id 不复用）
    pub fn delete_row(&mut self, row_id: u32) -> Result<()> {
        if let Some(Some(row)) = self.data.get(row_id as usize) {
            if let Some(pk) = self.pk_col() {
                if let Some(cell) = row.get(pk) {
                    self.primary.remove(cell);
                }
            }
        }
        if (row_id as usize) < self.data.len() {
            self.data[row_id as usize] = None;
            if self.def.row_count > 0 {
                self.def.row_count -= 1;
            }
        }
        Ok(())
    }

    /// 更新一行（保留 row_id）
    pub fn update_row(&mut self, row_id: u32, new_row: &[Value]) -> Result<()> {
        if let Some(Some(old)) = self.data.get(row_id as usize) {
            if let Some(pk) = self.pk_col() {
                if let Some(old_cell) = old.get(pk) {
                    self.primary.remove(old_cell);
                }
                if let Some(new_cell) = new_row.get(pk) {
                    if self.primary.contains_key(new_cell) {
                        return Err(EngramDbError::ConstraintViolation(format!(
                            "UNIQUE constraint failed: {}={:?}",
                            self.def.columns[pk].name, new_cell
                        )));
                    }
                    self.primary.insert(new_cell.clone(), row_id);
                }
            }
            self.data[row_id as usize] = Some(new_row.to_vec());
        }
        Ok(())
    }

    pub fn truncate(&mut self) -> Result<()> {
        self.data.clear();
        self.primary.clear();
        self.next_row_id = 0;
        self.def.row_count = 0;
        Ok(())
    }

    /// 按 row_id 取行
    pub fn get_row_by_id(&mut self, row_id: u32) -> Result<Option<Vec<Value>>> {
        Ok(self.data.get(row_id as usize).and_then(|r| r.clone()))
    }

    /// 按 row_id 取指定列
    pub fn get_row_by_id_columns(&mut self, row_id: u32, cols: &[usize]) -> Result<Option<Vec<Value>>> {
        let row = match self.data.get(row_id as usize) {
            Some(Some(r)) => r,
            _ => return Ok(None),
        };
        Ok(Some(
            cols.iter()
                .map(|&ci| row.get(ci).cloned().unwrap_or(Value::Null))
                .collect(),
        ))
    }

    /// 主键点查（O(log n)）
    pub fn lookup_primary_key(&self, key: &Value) -> Option<u32> {
        self.primary.get(key).copied()
    }

    /// 扫描（支持谓词筛选 + 列裁剪），输出 Typed DataChunk
    pub fn scan_to_chunks(
        &mut self,
        column_indices: &[usize],
        skip_pred: Option<(usize, PredicateOp, Value)>,
    ) -> Result<Vec<DataChunk>> {
        const VECTOR_SIZE: usize = crate::executor::vector::VECTOR_SIZE;

        // 1. 收集存活行（谓词筛选）
        let mut cols: Vec<Vec<Value>> = vec![Vec::new(); column_indices.len()];
        for row in self.data.iter().flatten() {
            if let Some((ci, op, val)) = &skip_pred {
                if row.get(*ci).map_or(true, |c| !matches_predicate(c, *op, val)) {
                    continue;
                }
            }
            for (j, &ci) in column_indices.iter().enumerate() {
                cols[j].push(row.get(ci).cloned().unwrap_or(Value::Null));
            }
        }
        let total = cols.first().map(|c| c.len()).unwrap_or(0);
        if total == 0 {
            return Ok(Vec::new());
        }

        // 2. 分块输出（Typed 保真，对齐列式管道）
        let mut chunks = Vec::new();
        let mut start = 0;
        while start < total {
            let len = VECTOR_SIZE.min(total - start);
            let mut columns = Vec::with_capacity(column_indices.len());
            for col in &cols {
                let slice: Vec<Value> = col[start..start + len].to_vec();
                match ColumnData::try_from_values(&slice) {
                    Some(d) => columns.push(Vector::Typed(d)),
                    None => columns.push(Vector::Flat(slice)),
                }
            }
            chunks.push(DataChunk {
                columns,
                count: len,
            });
            start += len;
        }
        Ok(chunks)
    }

    /// 按主键值定位 row_id（非事务 DELETE 用）
    pub fn pk_row_id(&self, row: &[Value]) -> Option<u32> {
        let pk = self.pk_col()?;
        let cell = row.get(pk)?;
        self.primary.get(cell).copied()
    }

    /// 全部存活行（带 row_id），供 UPDATE/DELETE 收集
    pub fn all_rows_with_ids(&self) -> Result<Vec<(u64, Vec<Value>)>> {
        let mut out = Vec::with_capacity(self.len());
        for (i, row) in self.data.iter().enumerate() {
            if let Some(r) = row {
                out.push((i as u64, r.clone()));
            }
        }
        Ok(out)
    }

    /// 直接输出行（TableScan 快捷路径）
    pub fn scan_to_rows_direct(
        &mut self,
        column_indices: &[usize],
        skip_pred: Option<(usize, PredicateOp, Value)>,
    ) -> Result<Vec<Vec<Value>>> {
        let chunks = self.scan_to_chunks(column_indices, skip_pred)?;
        let mut rows = Vec::new();
        for ch in &chunks {
            rows.extend(ch.to_rows());
        }
        Ok(rows)
    }
}

impl crate::storage::engine::EngineTableOps for MemoryTable {
    fn engine_type(&self) -> EngineType {
        EngineType::Memory
    }

    fn def(&self) -> &TableDef {
        &self.def
    }

    fn insert_rows(&mut self, rows: Vec<Vec<Value>>) -> Result<u64> {
        self.insert(rows)
    }

    fn insert_row(&mut self, row_id: u32, row: &[Value]) -> Result<()> {
        self.insert_row(row_id, row)
    }

    fn update_row(&mut self, row_id: u32, new_row: &[Value]) -> Result<()> {
        self.update_row(row_id, new_row)
    }

    fn delete_row(&mut self, row_id: u32) -> Result<()> {
        self.delete_row(row_id)
    }

    fn truncate(&mut self) -> Result<()> {
        self.truncate()
    }

    fn scan_to_chunks(
        &mut self,
        column_indices: &[usize],
        skip_pred: Option<(usize, PredicateOp, Value)>,
    ) -> Result<Vec<DataChunk>> {
        self.scan_to_chunks(column_indices, skip_pred)
    }

    fn get_row_by_id(&mut self, row_id: u32) -> Result<Option<Vec<Value>>> {
        self.get_row_by_id(row_id)
    }

    fn lookup_primary_key(&self, pk: &Value) -> Option<u32> {
        self.lookup_primary_key(pk)
    }
}
