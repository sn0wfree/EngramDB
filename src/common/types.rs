//! 数据类型定义

/// 列的数据类型

/// 列的数据类型
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum DataType {
    Boolean,
    Int32,
    Int64,
    /// 单精度浮点数（v0.14.0 新增）
    ///
    /// 4 字节存储，比 Float64 节省 50% 空间。适合 ML embedding、
    /// 科学计算等对精度要求不严格的场景。
    Float32,
    Float64,
    Varchar,
    /// JSON 类型（v0.12.0 新增）
    ///
    /// 存储半结构化 JSON 数据，支持路径查询。
    /// 适合 Agent 场景的工具参数、调用结果、状态元数据等。
    Json,
    /// 向量类型（v0.12.0 新增）
    ///
    /// 存储固定维度的 f32 向量，支持 HNSW 近似最近邻搜索。
    /// 维度在建表时指定（如 `VECTOR(1536)`），默认 0 表示动态维度。
    Vector { dim: usize },
    /// INT8 量化向量类型（v0.15.0 新增）
    ///
    /// 存储 INT8 量化后的向量，存储量减少 75%（4x 压缩）。
    /// 基于 MinMax 量化，每个向量独立存储 scale/offset 参数。
    /// 搜索时自动反量化回 f32 计算距离，精度损失约 1-5% 召回率。
    /// 适合 AI embedding 等对精度要求不苛刻的场景。
    VectorInt8 { dim: usize },
    /// BLOB 二进制数据（v0.13.0 新增）
    Blob,
    /// 时间戳（v0.14.0 新增）
    ///
    /// 内部存储为 Unix 毫秒（i64 UTC），适合 Agent 日志/记忆等时间序列场景。
    Timestamp,
}

impl DataType {
    pub fn name(&self) -> &'static str {
        match self {
            DataType::Boolean => "BOOLEAN",
            DataType::Int32 => "INT",
            DataType::Int64 => "BIGINT",
            DataType::Float32 => "FLOAT",
            DataType::Float64 => "DOUBLE",
            DataType::Varchar => "VARCHAR",
            DataType::Json => "JSON",
            DataType::Vector { .. } => "VECTOR",
            DataType::VectorInt8 { .. } => "VECTOR_INT8",
            DataType::Blob => "BLOB",
            DataType::Timestamp => "TIMESTAMP",
        }
    }

    pub fn fixed_size(&self) -> Option<usize> {
        match self {
            DataType::Boolean => Some(1),
            DataType::Int32 => Some(4),
            DataType::Int64 => Some(8),
            DataType::Float32 => Some(4),
            DataType::Float64 => Some(8),
            DataType::Varchar => None,
            DataType::Json => None,
            DataType::Vector { .. } => None,
            DataType::VectorInt8 { .. } => None,
            DataType::Blob => None,
            DataType::Timestamp => Some(8),
        }
    }
}

impl std::fmt::Display for DataType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name())
    }
}

/// 列定义
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ColumnDef {
    pub name: String,
    pub data_type: DataType,
    pub nullable: bool,
    pub is_primary_key: bool,
    pub default_value: Option<String>,
    pub auto_increment: bool,
}

impl ColumnDef {
    pub fn new(name: &str, data_type: DataType) -> Self {
        Self {
            name: name.to_string(),
            data_type,
            nullable: true,
            is_primary_key: false,
            default_value: None,
            auto_increment: false,
        }
    }

    pub fn primary_key(mut self) -> Self {
        self.is_primary_key = true;
        self.nullable = false;
        self
    }

    pub fn not_null(mut self) -> Self {
        self.nullable = false;
        self
    }

    pub fn default(mut self, val: &str) -> Self {
        self.default_value = Some(val.to_string());
        self
    }

    pub fn auto_inc(mut self) -> Self {
        self.auto_increment = true;
        self
    }
}

/// 索引定义（v0.12.0 新增，覆盖索引）
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct IndexDef {
    pub name: String,
    pub key_columns: Vec<usize>,
    pub included_columns: Vec<usize>,
    pub unique: bool,
    pub index_type: String,
}

/// 存储引擎类型（v0.17.0 多引擎架构 M0 新增）
///
/// 不同引擎面向不同工作负载，建表时通过 `ENGINE = xxx` 指定：
/// - Columnar：列存主力引擎（分析 / 向量混合查询），默认
/// - Memory：全内存高频读写（Agent 推理中间状态，不持久化）
/// - Log：追加写日志引擎（trace / 事件流，v0.18 规划）
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[repr(u8)]
pub enum EngineType {
    Columnar,
    Memory,
    Log,
}

impl EngineType {
    /// 磁盘/网络编码（WAL 记录头 engine 字节）
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0 => Some(EngineType::Columnar),
            1 => Some(EngineType::Memory),
            2 => Some(EngineType::Log),
            _ => None,
        }
    }
}

impl Default for EngineType {
    fn default() -> Self {
        EngineType::Columnar
    }
}

impl EngineType {
    pub fn from_str(name: &str) -> Option<Self> {
        match name.to_lowercase().as_str() {
            "columnar" => Some(EngineType::Columnar),
            "memory" => Some(EngineType::Memory),
            "log" => Some(EngineType::Log),
            _ => None,
        }
    }
}

/// 表定义
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TableDef {
    pub id: u32,
    pub name: String,
    pub columns: Vec<ColumnDef>,
    pub row_count: u64,
    pub indexes: Vec<IndexDef>,
    pub cluster_key: Option<usize>,
    pub foreign_keys: Vec<ForeignKeyDef>,
    /// 存储引擎（v0.17.0 M0 新增）
    ///
    /// `#[serde(default)]`：旧版本文件无此字段时反序列化为 Columnar，
    /// 磁盘格式向后兼容（v0.16 及更早的 .hdb 文件可直接打开）。
    #[serde(default)]
    pub engine: EngineType,
    /// AUTO_INCREMENT 计数器（v0.14.0 新增）
    ///
    /// 下一个待分配的自增 ID。每次 INSERT 自增列时从该值分配并 +1。
    /// 持久化到 TableDef，自动通过 serde 处理。
    pub next_auto_increment_id: u64,
    /// TTL（秒），None 表示永不过期（v0.15.0 新增）
    ///
    /// 设置了 TTL 的表，写入时自动填充 `_created_at` 时间戳列，
    /// 读取时检查 `created_at + ttl < now()` 自动过滤过期行，
    /// compaction 时物理删除过期行。
    pub ttl_seconds: Option<u64>,
    /// TTL 参考列索引（v0.15.0 新增）
    ///
    /// 该列必须是 Timestamp 类型，用于判断 TTL 是否过期。
    /// 建表时由 `ttl_seconds` 自动指定，用户也可以显式设置。
    pub ttl_column: Option<usize>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ForeignKeyDef {
    pub local_columns: Vec<usize>,
    pub foreign_table: String,
    pub foreign_columns: Vec<usize>,
    pub on_delete: ForeignKeyAction,
    pub on_update: ForeignKeyAction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ForeignKeyAction {
    NoAction,
    Restrict,
    Cascade,
    SetNull,
    SetDefault,
}

impl TableDef {
    pub fn new(id: u32, name: &str, columns: Vec<ColumnDef>) -> Self {
        Self {
            id,
            name: name.to_string(),
            columns,
            row_count: 0,
            indexes: Vec::new(),
            cluster_key: None,
            foreign_keys: Vec::new(),
            engine: EngineType::Columnar,
            next_auto_increment_id: 1,
            ttl_seconds: None,
            ttl_column: None,
        }
    }

    pub fn primary_key_index(&self) -> Option<usize> {
        self.columns.iter().position(|c| c.is_primary_key)
    }

    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name == name)
    }

    /// 设置聚簇列（按列名）
    pub fn set_cluster_key(&mut self, column_name: &str) -> Result<(), String> {
        match self.column_index(column_name) {
            Some(idx) => {
                self.cluster_key = Some(idx);
                Ok(())
            }
            None => Err(format!("column '{}' not found", column_name)),
        }
    }

    /// 检查表是否有 TTL 配置
    pub fn has_ttl(&self) -> bool {
        self.ttl_seconds.is_some()
    }

    /// 获取 TTL 秒数
    pub fn ttl(&self) -> Option<u64> {
        self.ttl_seconds
    }

    /// TTL 截止时间（毫秒时间戳）。
    ///
    /// 与 `is_expired` 语义一致：`created_ms < cutoff` 视为过期。
    /// 一次扫描只调用一次，避免逐行 `SystemTime::now()` 系统调用。
    pub fn ttl_cutoff_ms(&self) -> Option<i64> {
        self.ttl_seconds.map(|ttl| {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as i64;
            now_ms.saturating_sub((ttl as i64).saturating_mul(1000))
        })
    }

    /// 判断指定行是否已过期（相对于当前时间）
    pub fn is_expired(&self, row: &[crate::Value]) -> bool {
        match self.ttl_seconds {
            Some(ttl) => {
                if let Some(ttl_col) = self.ttl_column {
                    if ttl_col < row.len() {
                        if let crate::Value::Timestamp(created_ms) = &row[ttl_col] {
                            let now_ms = std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_millis() as i64;
                            return now_ms - created_ms > (ttl as i64) * 1000;
                        }
                    }
                }
                false
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- DataType 测试 ---

    #[test]
    fn test_data_type_names() {
        assert_eq!(DataType::Boolean.name(), "BOOLEAN");
        assert_eq!(DataType::Int32.name(), "INT");
        assert_eq!(DataType::Int64.name(), "BIGINT");
        assert_eq!(DataType::Float64.name(), "DOUBLE");
        assert_eq!(DataType::Varchar.name(), "VARCHAR");
    }

    #[test]
    fn test_data_type_fixed_size() {
        assert_eq!(DataType::Boolean.fixed_size(), Some(1));
        assert_eq!(DataType::Int32.fixed_size(), Some(4));
        assert_eq!(DataType::Int64.fixed_size(), Some(8));
        assert_eq!(DataType::Float64.fixed_size(), Some(8));
        assert_eq!(DataType::Varchar.fixed_size(), None);
    }

    #[test]
    fn test_data_type_display() {
        assert_eq!(format!("{}", DataType::Boolean), "BOOLEAN");
        assert_eq!(format!("{}", DataType::Int32), "INT");
        assert_eq!(format!("{}", DataType::Int64), "BIGINT");
        assert_eq!(format!("{}", DataType::Float64), "DOUBLE");
        assert_eq!(format!("{}", DataType::Varchar), "VARCHAR");
    }

    #[test]
    fn test_data_type_equality() {
        assert_eq!(DataType::Int32, DataType::Int32);
        assert_ne!(DataType::Int32, DataType::Int64);
        assert_ne!(DataType::Boolean, DataType::Varchar);
    }

    #[test]
    fn test_data_type_clone() {
        let dt = DataType::Float64;
        let dt2 = dt.clone();
        assert_eq!(dt, dt2);
    }

    // --- ColumnDef 测试 ---

    #[test]
    fn test_column_def_new() {
        let col = ColumnDef::new("id", DataType::Int64);
        assert_eq!(col.name, "id");
        assert_eq!(col.data_type, DataType::Int64);
        assert!(col.nullable);
        assert!(!col.is_primary_key);
    }

    #[test]
    fn test_column_def_primary_key() {
        let col = ColumnDef::new("id", DataType::Int64).primary_key();
        assert!(col.is_primary_key);
        assert!(!col.nullable);
    }

    #[test]
    fn test_column_def_not_null() {
        let col = ColumnDef::new("name", DataType::Varchar).not_null();
        assert!(!col.nullable);
        assert!(!col.is_primary_key);
    }

    #[test]
    fn test_column_def_clone() {
        let col = ColumnDef::new("age", DataType::Int32).not_null();
        let col2 = col.clone();
        assert_eq!(col.name, col2.name);
        assert_eq!(col.data_type, col2.data_type);
        assert_eq!(col.nullable, col2.nullable);
    }

    // --- TableDef 测试 ---

    #[test]
    fn test_table_def_new() {
        let cols = vec![
            ColumnDef::new("id", DataType::Int64).primary_key(),
            ColumnDef::new("name", DataType::Varchar),
        ];
        let table = TableDef::new(1, "users", cols);
        assert_eq!(table.id, 1);
        assert_eq!(table.name, "users");
        assert_eq!(table.columns.len(), 2);
        assert_eq!(table.row_count, 0);
    }

    #[test]
    fn test_table_def_primary_key_index() {
        let cols = vec![
            ColumnDef::new("name", DataType::Varchar),
            ColumnDef::new("id", DataType::Int64).primary_key(),
        ];
        let table = TableDef::new(1, "t", cols);
        assert_eq!(table.primary_key_index(), Some(1));
    }

    #[test]
    fn test_table_def_no_primary_key() {
        let cols = vec![
            ColumnDef::new("a", DataType::Int32),
            ColumnDef::new("b", DataType::Int32),
        ];
        let table = TableDef::new(1, "t", cols);
        assert_eq!(table.primary_key_index(), None);
    }

    #[test]
    fn test_table_def_column_index() {
        let cols = vec![
            ColumnDef::new("id", DataType::Int64),
            ColumnDef::new("name", DataType::Varchar),
            ColumnDef::new("age", DataType::Int32),
        ];
        let table = TableDef::new(1, "t", cols);
        assert_eq!(table.column_index("id"), Some(0));
        assert_eq!(table.column_index("name"), Some(1));
        assert_eq!(table.column_index("age"), Some(2));
        assert_eq!(table.column_index("nonexistent"), None);
    }

    #[test]
    fn test_table_def_clone() {
        let cols = vec![ColumnDef::new("id", DataType::Int64).primary_key()];
        let table = TableDef::new(42, "test_table", cols);
        let table2 = table.clone();
        assert_eq!(table.id, table2.id);
        assert_eq!(table.name, table2.name);
        assert_eq!(table.columns.len(), table2.columns.len());
    }

    #[test]
    fn test_engine_type_from_str() {
        assert_eq!(EngineType::from_str("columnar"), Some(EngineType::Columnar));
        assert_eq!(EngineType::from_str("COLUMNAR"), Some(EngineType::Columnar), "大小写不敏感");
        assert_eq!(EngineType::from_str("memory"), Some(EngineType::Memory));
        assert_eq!(EngineType::from_str("log"), Some(EngineType::Log));
        assert_eq!(EngineType::from_str("unknown"), None);
        assert_eq!(EngineType::from_str(""), None);
    }

    #[test]
    fn test_engine_type_from_u8() {
        assert_eq!(EngineType::from_u8(0), Some(EngineType::Columnar));
        assert_eq!(EngineType::from_u8(1), Some(EngineType::Memory));
        assert_eq!(EngineType::from_u8(2), Some(EngineType::Log));
        assert_eq!(EngineType::from_u8(3), None);
        assert_eq!(EngineType::from_u8(255), None);
    }

    #[test]
    fn test_engine_type_default() {
        assert_eq!(EngineType::default(), EngineType::Columnar);
    }
}
