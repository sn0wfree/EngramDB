//! LogEngine（v0.17.0 M3）：追加式时间序列引擎
//!
//! 定位：时序日志、事件流等只追加、按时间范围查询的场景。
//! - 追加块式列存：行追加到当前块，块满（`LOG_BLOCK_ROWS`）冻结新块
//! - 块头每列 MinMax：范围谓词块级跳读（时间范围扫描 1.5-2x）
//! - 复用 `ColumnData::serialize_typed/deserialize_typed` 落盘
//!   （块级序列化，数据进主文件 data 段，与 Columnar 同管线）
//! - UPDATE / DELETE / UPSERT 明确报错（追加语义，M3 验收项）
//! - 无主键索引（时序表无主键）；点查短路不触发，走扫描路径
//!
//! 持久化语义：块冻结即不可变（append-only），checkpoint 时全量序列化。

use crate::common::column_data::ColumnData;
use crate::common::error::{Result, EngramDbError};
use crate::common::types::{EngineType, TableDef};
use crate::executor::vector::{DataChunk, Vector};
use crate::storage::column_store::{matches_predicate_typed, value_greater, value_less, PredicateOp};
use crate::Value;

/// 块行数（块满冻结；MinMax 跳读粒度）
pub const LOG_BLOCK_ROWS: usize = 8192;

/// 追加块
///
/// 追加期：`columns` 为每列原始 `Vec<Value>`（零中间分配）；
/// 冻结后首次扫描/序列化时惰性构建 `typed`（列式 Typed 缓存，块级摊销一次）。
struct LogBlock {
    /// 块起始 row_id（全局 row_id 连续分配，行序 = 追加序）
    row_id_base: u32,
    /// 每列原始值缓冲（追加期）
    columns: Vec<Vec<Value>>,
    /// 冻结后列式缓存（惰性构建）
    typed: Option<Vec<ColumnData>>,
    /// 每列最小值（块头元数据，跨类型比较）
    min: Vec<Value>,
    /// 每列最大值
    max: Vec<Value>,
    /// 块内行数
    rows: usize,
}

impl LogBlock {
    fn ensure_typed(&mut self, def: &TableDef) {
        if self.typed.is_none() {
            let typed = self
                .columns
                .iter()
                .enumerate()
                .map(|(i, col)| ColumnData::from_values_typed(col, &def.columns[i].data_type))
                .collect();
            self.typed = Some(typed);
        }
    }
}

/// 时间序列表（LogEngine）
///
/// `row_count` 存于 `def.row_count`（跨引擎统一统计口径，插删时同步维护）。
pub struct LogTable {
    pub def: TableDef,
    blocks: Vec<LogBlock>,
    next_row_id: u32,
}

impl LogTable {
    pub fn new(def: TableDef) -> Self {
        Self {
            def,
            blocks: Vec::new(),
            next_row_id: 0,
        }
    }

    pub fn row_count(&self) -> u64 {
        self.def.row_count
    }

    pub fn len(&self) -> usize {
        self.row_count() as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 当前追加块（满则冻结新建）
    fn current_block_mut(&mut self) -> &mut LogBlock {
        let need_new = match self.blocks.last() {
            Some(b) => b.rows >= LOG_BLOCK_ROWS,
            None => true,
        };
        if need_new {
            self.blocks.push(LogBlock {
                row_id_base: self.next_row_id,
                columns: vec![Vec::with_capacity(LOG_BLOCK_ROWS); self.def.columns.len()],
                typed: None,
                min: Vec::new(),
                max: Vec::new(),
                rows: 0,
            });
        }
        self.blocks.last_mut().unwrap()
    }

    /// 追加单行到当前块（维护列缓冲 + MinMax）
    fn append_row(&mut self, row: &[Value]) {
        let block = self.current_block_mut();
        for (i, col) in block.columns.iter_mut().enumerate() {
            let v = row.get(i).cloned().unwrap_or(Value::Null);
            col.push(v.clone());
            if block.min.len() <= i {
                block.min.push(v.clone());
                block.max.push(v.clone());
            } else {
                if value_less(&v, &block.min[i]) {
                    block.min[i] = v.clone();
                }
                if value_greater(&v, &block.max[i]) {
                    block.max[i] = v.clone();
                }
            }
        }
        block.rows += 1;
    }

    /// 追加写入（LogEngine 唯一写路径）
    pub fn insert(&mut self, rows: Vec<Vec<Value>>) -> Result<u64> {
        let n = rows.len();
        if n == 0 {
            return Ok(0);
        }
        for row in &rows {
            self.append_row(row);
        }
        self.next_row_id += n as u32;
        self.def.row_count += n as u64;
        Ok(n as u64)
    }

    /// 列式批量追加（事务 apply 路径，InsertBatch）
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

    /// 按指定 row_id 追加（事务 apply 路径）
    ///
    /// LogEngine 仅支持追加：row_id 必须是下一个待分配 id。
    pub fn insert_row(&mut self, row_id: u32, row: &[Value]) -> Result<()> {
        if row_id != self.next_row_id {
            return Err(EngramDbError::NotSupported(format!(
                "LogEngine 仅支持追加写入：row_id {} != 下一追加位置 {}",
                row_id, self.next_row_id
            )));
        }
        self.append_row(row);
        self.next_row_id += 1;
        self.def.row_count += 1;
        Ok(())
    }

    /// 清空（TRUNCATE 允许：DDL 语义，非行级写）
    pub fn truncate(&mut self) -> Result<()> {
        self.blocks.clear();
        self.next_row_id = 0;
        self.def.row_count = 0;
        Ok(())
    }

    /// 按 row_id 取行（row_id 连续 = 块内偏移；块按 row_id_base 递增有序，二分定位）
    pub fn get_row_by_id(&mut self, row_id: u32) -> Result<Option<Vec<Value>>> {
        let idx = match self
            .blocks
            .binary_search_by(|b| {
                if row_id < b.row_id_base {
                    std::cmp::Ordering::Greater
                } else if row_id >= b.row_id_base + b.rows as u32 {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Equal
                }
            }) {
            Ok(i) => i,
            Err(_) => return Ok(None),
        };
        let block = &self.blocks[idx];
        let off = (row_id - block.row_id_base) as usize;
        let mut row = Vec::with_capacity(block.columns.len());
        for col in &block.columns {
            row.push(col.get(off).cloned().unwrap_or(Value::Null));
        }
        Ok(Some(row))
    }

    /// 按 row_id 取指定列
    pub fn get_row_by_id_columns(
        &mut self,
        row_id: u32,
        cols: &[usize],
    ) -> Result<Option<Vec<Value>>> {
        let row = match self.get_row_by_id(row_id)? {
            Some(r) => r,
            None => return Ok(None),
        };
        Ok(Some(
            cols.iter()
                .map(|&ci| row.get(ci).cloned().unwrap_or(Value::Null))
                .collect(),
        ))
    }

    /// 扫描（块级 MinMax 跳读 + 行级谓词筛选 + 列裁剪），输出 Typed DataChunk
    pub fn scan_to_chunks(
        &mut self,
        column_indices: &[usize],
        skip_pred: Option<(usize, PredicateOp, Value)>,
    ) -> Result<Vec<DataChunk>> {
        const VECTOR_SIZE: usize = crate::executor::vector::VECTOR_SIZE;

        // 1. 块级跳读 + 行级筛选，收集输出列
        let mut cols: Vec<Vec<Value>> = vec![Vec::new(); column_indices.len()];
        for block in &mut self.blocks {
            if let Some((ci, op, val)) = &skip_pred {
                if block_can_skip(block, *ci, *op, val) {
                    continue;
                }
            }
            block.ensure_typed(&self.def);
            let typed = block.typed.as_ref().unwrap();
            for r in 0..block.rows {
                if let Some((ci, op, val)) = &skip_pred {
                    if !matches_predicate_typed(&typed[*ci], r, *op, val) {
                        continue;
                    }
                }
                for (j, &ci) in column_indices.iter().enumerate() {
                    cols[j].push(typed[ci].get(r));
                }
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

    /// 序列化（进主文件 data 段）
    ///
    /// 格式：[u32 block_count] + per-block：
    ///   [u32 row_id_base][u32 rows][u32 col_count]
    ///   per-col: [u32 dt_len][dt bytes][u32 min_len][bincode min][u32 max_len][bincode max]
    ///            [u32 data_len][serialize_typed bytes]
    pub fn to_bytes(&mut self) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&(self.blocks.len() as u32).to_le_bytes());
        for b in &mut self.blocks {
            b.ensure_typed(&self.def);
            buf.extend_from_slice(&b.row_id_base.to_le_bytes());
            buf.extend_from_slice(&(b.rows as u32).to_le_bytes());
            buf.extend_from_slice(&(b.columns.len() as u32).to_le_bytes());
            let typed = b.typed.as_ref().unwrap();
            for (i, col) in typed.iter().enumerate() {
                let dt_bytes = bincode::serialize(&self.def.columns[i].data_type).unwrap_or_default();
                buf.extend_from_slice(&(dt_bytes.len() as u32).to_le_bytes());
                buf.extend_from_slice(&dt_bytes);
                let min_bytes = bincode::serialize(&b.min[i]).unwrap_or_default();
                buf.extend_from_slice(&(min_bytes.len() as u32).to_le_bytes());
                buf.extend_from_slice(&min_bytes);
                let max_bytes = bincode::serialize(&b.max[i]).unwrap_or_default();
                buf.extend_from_slice(&(max_bytes.len() as u32).to_le_bytes());
                buf.extend_from_slice(&max_bytes);
                let data = col.serialize_typed(&self.def.columns[i].data_type);
                buf.extend_from_slice(&(data.len() as u32).to_le_bytes());
                buf.extend_from_slice(&data);
            }
        }
        buf
    }

    /// 从字节恢复（load_data 分派路径）
    pub fn from_bytes(&mut self, data: &[u8]) -> Result<()> {
        let mut off = 0usize;
        let read_u32 = |off: &mut usize, data: &[u8]| -> Result<u32> {
            if *off + 4 > data.len() {
                return Err(EngramDbError::InvalidFormat(
                    "truncated log block header".into(),
                ));
            }
            let v = u32::from_le_bytes(data[*off..*off + 4].try_into().unwrap());
            *off += 4;
            Ok(v)
        };
        let block_count = read_u32(&mut off, data)? as usize;
        self.blocks.clear();
        self.next_row_id = 0;
        self.def.row_count = 0;
        for _ in 0..block_count {
            let row_id_base = read_u32(&mut off, data)?;
            let rows = read_u32(&mut off, data)? as usize;
            let col_count = read_u32(&mut off, data)? as usize;
            if col_count != self.def.columns.len() {
                return Err(EngramDbError::InvalidFormat(format!(
                    "log block column mismatch: {} != {}",
                    col_count,
                    self.def.columns.len()
                )));
            }
            let mut typed = Vec::with_capacity(col_count);
            let mut min = Vec::with_capacity(col_count);
            let mut max = Vec::with_capacity(col_count);
            for _ in 0..col_count {
                let dt_len = read_u32(&mut off, data)? as usize;
                if off + dt_len > data.len() {
                    return Err(EngramDbError::InvalidFormat(
                        "truncated log data_type".into(),
                    ));
                }
                let dt: crate::common::types::DataType =
                    bincode::deserialize(&data[off..off + dt_len])
                        .map_err(|_| EngramDbError::InvalidFormat("bad log data_type".into()))?;
                off += dt_len;
                let min_len = read_u32(&mut off, data)? as usize;
                if off + min_len > data.len() {
                    return Err(EngramDbError::InvalidFormat("truncated log min".into()));
                }
                min.push(
                    bincode::deserialize(&data[off..off + min_len])
                        .map_err(|_| EngramDbError::InvalidFormat("bad log min".into()))?,
                );
                off += min_len;
                let max_len = read_u32(&mut off, data)? as usize;
                if off + max_len > data.len() {
                    return Err(EngramDbError::InvalidFormat("truncated log max".into()));
                }
                max.push(
                    bincode::deserialize(&data[off..off + max_len])
                        .map_err(|_| EngramDbError::InvalidFormat("bad log max".into()))?,
                );
                off += max_len;
                let data_len = read_u32(&mut off, data)? as usize;
                if off + data_len > data.len() {
                    return Err(EngramDbError::InvalidFormat(
                        "truncated log column data".into(),
                    ));
                }
                typed.push(ColumnData::deserialize_typed(
                    &data[off..off + data_len],
                    &dt,
                    rows,
                ));
                off += data_len;
            }
            self.blocks.push(LogBlock {
                row_id_base,
                columns: Vec::new(),
                typed: Some(typed),
                min,
                max,
                rows,
            });
            if row_id_base + rows as u32 > self.next_row_id {
                self.next_row_id = row_id_base + rows as u32;
            }
            self.def.row_count += rows as u64;
        }
        Ok(())
    }
}

/// 块级 MinMax 跳读判定（与 Columnar 的 can_skip_predicate 同语义）
fn block_can_skip(block: &LogBlock, col_idx: usize, op: PredicateOp, val: &Value) -> bool {
    let (min, max) = match (block.min.get(col_idx), block.max.get(col_idx)) {
        (Some(m), Some(x)) => (m, x),
        _ => return false,
    };
    match op {
        PredicateOp::Eq => value_less(val, min) || value_greater(val, max),
        PredicateOp::Gt => !value_less(val, max),
        PredicateOp::GtEq => value_greater(val, max),
        PredicateOp::Lt => !value_greater(val, min),
        PredicateOp::LtEq => value_less(val, min),
    }
}

impl crate::storage::engine::EngineTableOps for LogTable {
    fn engine_type(&self) -> EngineType {
        EngineType::Log
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

    fn update_row(&mut self, _row_id: u32, _new_row: &[Value]) -> Result<()> {
        Err(EngramDbError::NotSupported(
            "LogEngine 不支持 UPDATE（追加式时间序列引擎）".into(),
        ))
    }

    fn delete_row(&mut self, _row_id: u32) -> Result<()> {
        Err(EngramDbError::NotSupported(
            "LogEngine 不支持 DELETE（追加式时间序列引擎）".into(),
        ))
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

    fn lookup_primary_key(&self, _pk: &Value) -> Option<u32> {
        None
    }
}
