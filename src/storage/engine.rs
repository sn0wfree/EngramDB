//! 存储引擎抽象层（v0.17.0 多引擎架构 M0）
//!
//! - [`StorageEngine`]：引擎接口契约（文档 v1.0 定义），所有引擎实现它
//! - [`EngineTable`]：运行时持有与分派（ADR-1：枚举 + trait 契约双轨）
//!   分派 match 集中在枚举 impl，算子层通过 `Database` 便捷方法访问
//!
//! 当前仅有 Columnar 引擎（原 Table 列存）。M2 起加入 Memory/Log 变体，
//! 新增引擎只需实现 [`StorageEngine`] 并加入枚举。

use crate::common::error::{Result, EngramDbError};
use crate::common::types::{EngineType, TableDef};
use crate::executor::vector::{DataChunk, Vector};
use crate::storage::column_store::PredicateOp;
use crate::storage::log_engine::LogTable;
use crate::storage::memory_engine::MemoryTable;
use crate::storage::table::Table;
use crate::Value;

/// 扫描规格（引擎查询接口）
#[derive(Debug, Clone)]
pub struct ScanSpec {
    /// 输出列（表定义中的列索引）
    pub column_indices: Vec<usize>,
    /// 可下推谓词（列索引, 操作符, 目标值）
    pub skip_pred: Option<(usize, PredicateOp, Value)>,
    /// 最大行数（None = 不限）
    pub limit: Option<usize>,
}

impl ScanSpec {
    pub fn new(column_indices: Vec<usize>) -> Self {
        Self {
            column_indices,
            skip_pred: None,
            limit: None,
        }
    }
}

/// 存储引擎接口契约（多引擎架构 v1.0 文档定义）
///
/// 所有引擎（Columnar / Memory / Log）实现的统一接口。
/// 引擎专属能力（向量索引、FTS 等）不在此接口内，通过
/// [`EngineTable`] 具体变体访问。
pub trait StorageEngine {
    fn engine_type(&self) -> EngineType;

    // DDL
    fn create_table(&mut self, table_id: u32, schema: &TableDef) -> Result<()>;
    fn drop_table(&mut self, table_id: u32) -> Result<()>;

    // DML
    fn insert(&mut self, table_id: u32, rows: &[Vec<Value>]) -> Result<usize>;
    fn update(&mut self, table_id: u32, pk: &Value, updates: &[(usize, Value)]) -> Result<bool>;
    fn delete(&mut self, table_id: u32, pk: &Value) -> Result<bool>;

    // Query
    fn scan(&mut self, table_id: u32, spec: &ScanSpec) -> Result<Vec<DataChunk>>;
}

/// 表级操作契约（v0.17.0 M0 新增）
///
/// 各引擎表结构（Table / MemoryTable / LogTable）实现的统一表操作接口。
/// 引擎级接口见 [`StorageEngine`]（管理多表 + 事务，M4 由引擎管理器实现）。
pub trait EngineTableOps {
    fn engine_type(&self) -> EngineType;
    fn def(&self) -> &TableDef;

    // DML
    fn insert_rows(&mut self, rows: Vec<Vec<Value>>) -> Result<u64>;
    fn insert_row(&mut self, row_id: u32, row: &[Value]) -> Result<()>;
    fn update_row(&mut self, row_id: u32, new_row: &[Value]) -> Result<()>;
    fn delete_row(&mut self, row_id: u32) -> Result<()>;
    fn truncate(&mut self) -> Result<()>;

    // Query
    fn scan_to_chunks(
        &mut self,
        column_indices: &[usize],
        skip_pred: Option<(usize, PredicateOp, Value)>,
    ) -> Result<Vec<DataChunk>>;
    fn get_row_by_id(&mut self, row_id: u32) -> Result<Option<Vec<Value>>>;
    /// 主键点查（&mut：列存稀疏确认可能触发惰性解压）
    fn lookup_primary_key(&mut self, pk: &Value) -> Option<u32>;
}

/// 引擎表的运行时持有枚举（ADR-1）
///
/// 分派逻辑集中在每个方法内的一次 match；算子层大多只接触
/// Columnar 变体（M0 阶段），新引擎变体随里程碑逐步加入。
pub enum EngineTable {
    Columnar(Table),
    /// 全内存表（M2）：不持久化，进程退出数据丢失
    Memory(MemoryTable),
    /// 追加式时间序列表（M3）：块级 MinMax 跳读，禁 UPDATE/DELETE
    Log(LogTable),
}

impl EngineTable {
    pub fn engine_type(&self) -> EngineType {
        // 引擎类型来自表定义（各引擎表在 def 中记录引擎类型）
        self.def().engine
    }

    /// 表定义（跨引擎通用）
    pub fn def(&self) -> &TableDef {
        match self {
            EngineTable::Columnar(t) => &t.def,
            EngineTable::Memory(t) => &t.def,
            EngineTable::Log(t) => &t.def,
        }
    }

    pub fn def_mut(&mut self) -> &mut TableDef {
        match self {
            EngineTable::Columnar(t) => &mut t.def,
            EngineTable::Memory(t) => &mut t.def,
            EngineTable::Log(t) => &mut t.def,
        }
    }

    /// 解包 Columnar 引擎（非 Columnar 返回 None）
    pub fn as_columnar(&self) -> Option<&Table> {
        match self {
            EngineTable::Columnar(t) => Some(t),
            EngineTable::Memory(_) | EngineTable::Log(_) => None,
        }
    }

    pub fn as_columnar_mut(&mut self) -> Option<&mut Table> {
        match self {
            EngineTable::Columnar(t) => Some(t),
            EngineTable::Memory(_) | EngineTable::Log(_) => None,
        }
    }

    /// 解包 Memory 引擎（非 Memory 返回 None）
    pub fn as_memory(&self) -> Option<&MemoryTable> {
        match self {
            EngineTable::Columnar(_) | EngineTable::Log(_) => None,
            EngineTable::Memory(t) => Some(t),
        }
    }

    pub fn as_memory_mut(&mut self) -> Option<&mut MemoryTable> {
        match self {
            EngineTable::Columnar(_) | EngineTable::Log(_) => None,
            EngineTable::Memory(t) => Some(t),
        }
    }

    /// 解包 Log 引擎（非 Log 返回 None）
    pub fn as_log(&self) -> Option<&LogTable> {
        match self {
            EngineTable::Log(t) => Some(t),
            EngineTable::Columnar(_) | EngineTable::Memory(_) => None,
        }
    }

    pub fn as_log_mut(&mut self) -> Option<&mut LogTable> {
        match self {
            EngineTable::Log(t) => Some(t),
            EngineTable::Columnar(_) | EngineTable::Memory(_) => None,
        }
    }

    /// 扫描（引擎分派入口，M2/M3 加入新变体时扩展）
    pub fn scan_to_chunks(
        &mut self,
        column_indices: &[usize],
        skip_pred: Option<(usize, PredicateOp, Value)>,
    ) -> Result<Vec<DataChunk>> {
        match self {
            EngineTable::Columnar(t) => t.scan_to_chunks_with_skip(column_indices, skip_pred),
            EngineTable::Memory(t) => t.scan_to_chunks(column_indices, skip_pred),
            EngineTable::Log(t) => t.scan_to_chunks(column_indices, skip_pred),
        }
    }

    /// 插入（引擎分派入口）
    pub fn insert_rows(&mut self, rows: Vec<Vec<Value>>) -> Result<u64> {
        match self {
            EngineTable::Columnar(t) => t.insert(rows),
            EngineTable::Memory(t) => t.insert(rows),
            EngineTable::Log(t) => t.insert(rows),
        }
    }

    /// 按行号取行（引擎分派入口）
    /// 按 row_id + 列裁剪取行（单行，按 col_indices 顺序；不存在返回空）
    pub fn get_row_by_id_columns(
        &mut self,
        row_id: u32,
        col_indices: &[usize],
    ) -> Result<Vec<Vec<Value>>> {
        match self {
            EngineTable::Columnar(t) => t.get_row_by_id_columns(row_id, col_indices),
            EngineTable::Memory(t) => Ok(t
                .get_row_by_id_columns(row_id, col_indices)?
                .map(|row| vec![row])
                .unwrap_or_default()),
            EngineTable::Log(t) => Ok(t
                .get_row_by_id_columns(row_id, col_indices)?
                .map(|row| vec![row])
                .unwrap_or_default()),
        }
    }

    pub fn get_row_by_id(&mut self, row_id: u32) -> Result<Option<Vec<Value>>> {
        match self {
            EngineTable::Columnar(t) => t.get_row_by_id(row_id),
            EngineTable::Memory(t) => t.get_row_by_id(row_id),
            EngineTable::Log(t) => t.get_row_by_id(row_id),
        }
    }

    /// 按主键查 row_id（引擎分派入口）
    pub fn lookup_primary_key(&mut self, pk: &Value) -> Option<u32> {
        match self {
            EngineTable::Columnar(t) => t.lookup_primary_key(pk),
            EngineTable::Memory(t) => t.lookup_primary_key(pk),
            EngineTable::Log(_) => None,
        }
    }

    /// 删除一行（引擎分派入口）
    pub fn delete_row(&mut self, row_id: u32) -> Result<()> {
        match self {
            EngineTable::Columnar(t) => t.delete_row(row_id),
            EngineTable::Memory(t) => t.delete_row(row_id),
            EngineTable::Log(t) => t.delete_row(row_id),
        }
    }

    /// 更新一行（引擎分派入口）
    pub fn update_row(&mut self, row_id: u32, new_row: &[Value]) -> Result<()> {
        match self {
            EngineTable::Columnar(t) => t.update_row(row_id, new_row),
            EngineTable::Memory(t) => t.update_row(row_id, new_row),
            EngineTable::Log(t) => t.update_row(row_id, new_row),
        }
    }

    /// 清空表（引擎分派入口）
    pub fn truncate(&mut self) -> Result<()> {
        match self {
            EngineTable::Columnar(t) => t.truncate(),
            EngineTable::Memory(t) => t.truncate(),
            EngineTable::Log(t) => t.truncate(),
        }
    }

    /// 序列化索引段（引擎分派入口，供 Database::save_indexes 使用）
    /// 实际行数（引擎感知：Columnar = def.row_count，Memory = 内存实际存活行）
    pub fn row_count(&self) -> u64 {
        match self {
            EngineTable::Columnar(t) => t.def.row_count,
            EngineTable::Memory(t) => t.row_count(),
            EngineTable::Log(t) => t.row_count(),
        }
    }

    /// 收集可操作行：(row_id, row)。
    ///
    /// 引擎语义：Columnar = Delta 层行（现有 UPDATE/DELETE 支持范围），
    /// Memory = 全部存活行（内存表无列存/Delta 之分）。
    pub fn collect_mutable_rows(&mut self) -> Result<Vec<(u64, Vec<Value>)>> {
        match self {
            EngineTable::Columnar(t) => {
                let rows = t.delta_store().all_rows();
                Ok(rows.iter().map(|(rid, row)| (*rid, row.clone())).collect())
            }
            EngineTable::Memory(t) => t.all_rows_with_ids(),
            // Log 表无行级写语义：UPDATE/DELETE 在此明确拒绝
            EngineTable::Log(_) => Err(EngramDbError::NotSupported(
                "LogEngine 不支持 UPDATE/DELETE（追加式时间序列引擎）".into(),
            )),
        }
    }

    pub fn indexes_to_bytes(&self) -> Vec<u8> {
        match self {
            EngineTable::Columnar(t) => t.indexes_to_bytes(),
            EngineTable::Memory(_) | EngineTable::Log(_) => Vec::new(),
        }
    }

    /// 从索引段字节恢复（引擎分派入口，供 Database::load_indexes 使用）
    pub fn indexes_from_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        match self {
            EngineTable::Columnar(t) => t.indexes_from_bytes(bytes),
            EngineTable::Memory(_) | EngineTable::Log(_) => Ok(()),
        }
    }

    /// 列式批量插入（引擎分派入口）
    pub fn insert_columns(&mut self, columns: Vec<Vec<Value>>) -> Result<u64> {
        match self {
            EngineTable::Columnar(t) => t.insert_columns(columns),
            EngineTable::Memory(t) => t.insert_columns(columns),
            EngineTable::Log(t) => t.insert_columns(columns),
        }
    }

    /// 单行插入（引擎分派入口，事务 apply 路径）
    pub fn insert_row(&mut self, row_id: u32, row: &[Value]) -> Result<()> {
        match self {
            EngineTable::Columnar(t) => t.insert_row(row_id, row),
            EngineTable::Memory(t) => t.insert_row(row_id, row),
            EngineTable::Log(t) => t.insert_row(row_id, row),
        }
    }

    /// 批量行插入（引擎分派入口，事务 apply 路径）
    pub fn insert(&mut self, rows: Vec<Vec<Value>>) -> Result<u64> {
        match self {
            EngineTable::Columnar(t) => t.insert(rows),
            EngineTable::Memory(t) => t.insert(rows),
            EngineTable::Log(t) => t.insert(rows),
        }
    }
}

/// 校验引擎名，返回规范枚举
pub fn parse_engine_type(name: &str) -> Option<EngineType> {
    EngineType::from_str(name)
}
