//! 数据类型定义

/// 列的数据类型
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum DataType {
    Boolean,
    Int32,
    Int64,
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
}

impl DataType {
    pub fn name(&self) -> &'static str {
        match self {
            DataType::Boolean => "BOOLEAN",
            DataType::Int32 => "INT",
            DataType::Int64 => "BIGINT",
            DataType::Float64 => "DOUBLE",
            DataType::Varchar => "VARCHAR",
            DataType::Json => "JSON",
            DataType::Vector { .. } => "VECTOR",
        }
    }

    pub fn fixed_size(&self) -> Option<usize> {
        match self {
            DataType::Boolean => Some(1),
            DataType::Int32 => Some(4),
            DataType::Int64 => Some(8),
            DataType::Float64 => Some(8),
            DataType::Varchar => None,
            DataType::Json => None,
            DataType::Vector { .. } => None,
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
}

impl ColumnDef {
    pub fn new(name: &str, data_type: DataType) -> Self {
        Self {
            name: name.to_string(),
            data_type,
            nullable: true,
            is_primary_key: false,
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
}

/// 索引定义（v0.12.0 新增，覆盖索引）
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct IndexDef {
    pub name: String,
    /// 索引键列的列索引（按顺序）
    pub key_columns: Vec<usize>,
    /// 覆盖列的列索引（INCLUDE 子句）
    pub included_columns: Vec<usize>,
    /// 是否唯一索引
    pub unique: bool,
}

/// 表定义
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TableDef {
    pub id: u32,
    pub name: String,
    pub columns: Vec<ColumnDef>,
    pub row_count: u64,
    /// 索引列表
    pub indexes: Vec<IndexDef>,
    /// 聚簇列索引（可选）
    ///
    /// 设置后，Delta 合并到列存时会按该列分组写入，
    /// 同值行物理上连续，提升按该列查询的性能。
    /// 典型场景：Agent 消息表按 session_id 聚簇。
    pub cluster_key: Option<usize>,
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
}
