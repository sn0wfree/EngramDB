//! EngramDB Catalog for DataFusion
//!
//! 实现 DataFusion 的 CatalogProvider / SchemaProvider trait，
//! 将 EngramDB 的表元数据暴露给查询引擎。

use std::any::Any;
use std::collections::HashMap;
use std::sync::Arc;

use datafusion::catalog::{CatalogProvider, SchemaProvider};
use datafusion_common::Result as DfResult;
use datafusion_expr::TableProvider;

use crate::common::types::DataType;
use crate::storage::Database;
use crate::Value;

use super::table_provider::EngramDBTable;

/// EngramDB Catalog (实现 DataFusion CatalogProvider)
pub struct EngramDBCatalog {
    schemas: HashMap<String, Arc<EngramDBSchema>>,
}

impl EngramDBCatalog {
    pub fn new() -> Self {
        let mut catalog = Self {
            schemas: HashMap::new(),
        };
        // 默认 public schema
        catalog.schemas.insert(
            "public".to_string(),
            Arc::new(EngramDBSchema::new()),
        );
        catalog
    }

    /// 从 Database 构建 catalog (注册所有表)
    pub fn from_database(db: &Database) -> Self {
        let schema = EngramDBSchema::from_database(db);
        let mut catalog = Self::new();
        catalog.schemas.insert(
            "public".to_string(),
            Arc::new(schema),
        );
        catalog
    }

    /// 注册一张表
    pub fn register_table(&self, name: &str, table: EngramDBTable) {
        if let Some(schema) = self.schemas.get("public") {
            // 内部用 Mutex 或直接重建，这里简化处理用内部可变性
            // MVP: 直接通过 unsafe 或新建方式
            // 实际生产中应使用 RwLock
        }
        // MVP 简化: 不支持运行时动态注册，初始化时一次性注册
        let _ = (name, table);
    }
}

impl Default for EngramDBCatalog {
    fn default() -> Self {
        Self::new()
    }
}

impl CatalogProvider for EngramDBCatalog {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn schema_names(&self) -> Vec<String> {
        self.schemas.keys().cloned().collect()
    }

    fn schema(&self, name: &str) -> Option<Arc<dyn SchemaProvider>> {
        self.schemas
            .get(name)
            .map(|s| s.clone() as Arc<dyn SchemaProvider>)
    }
}

/// EngramDB Schema (实现 DataFusion SchemaProvider)
pub struct EngramDBSchema {
    tables: std::sync::RwLock<HashMap<String, Arc<EngramDBTable>>>,
}

impl EngramDBSchema {
    pub fn new() -> Self {
        Self {
            tables: std::sync::RwLock::new(HashMap::new()),
        }
    }

    /// 从 Database 构建 schema
    pub fn from_database(db: &Database) -> Self {
        let mut tables = HashMap::new();

        // 遍历数据库中所有表
        // 注意: Database 没有暴露遍历所有表的接口，这里需要通过 table_names
        // MVP: 暂时通过已知方式获取
        // 实际需要在 Database 上添加一个方法
        // 这里先留空，后续补充

        let _ = db; // TODO: 从 db 加载所有表

        Self {
            tables: std::sync::RwLock::new(tables),
        }
    }

    /// 注册一张表
    pub fn register_table(&self, name: String, table: EngramDBTable) {
        let mut tables = self.tables.write().unwrap();
        tables.insert(name, Arc::new(table));
    }

    /// 获取表
    pub fn get_table(&self, name: &str) -> Option<Arc<EngramDBTable>> {
        let tables = self.tables.read().unwrap();
        tables.get(name).cloned()
    }
}

impl Default for EngramDBSchema {
    fn default() -> Self {
        Self::new()
    }
}

impl SchemaProvider for EngramDBSchema {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn table_names(&self) -> Vec<String> {
        let tables = self.tables.read().unwrap();
        tables.keys().cloned().collect()
    }

    fn table(&self, name: &str) -> DfResult<Option<Arc<dyn TableProvider>>> {
        let tables = self.tables.read().unwrap();
        Ok(tables
            .get(name)
            .map(|t| t.clone() as Arc<dyn TableProvider>))
    }

    fn table_exist(&self, name: &str) -> bool {
        let tables = self.tables.read().unwrap();
        tables.contains_key(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_table() -> EngramDBTable {
        let columns = vec![
            ("id".to_string(), DataType::Int64, false),
            ("name".to_string(), DataType::Varchar, true),
        ];
        let rows = vec![
            vec![Value::Int64(1), Value::Varchar("alice".into())],
            vec![Value::Int64(2), Value::Varchar("bob".into())],
        ];
        EngramDBTable::new("users".to_string(), columns, rows)
    }

    #[test]
    fn test_catalog_schema_list() {
        let catalog = EngramDBCatalog::new();
        let schemas = catalog.schema_names();
        assert!(schemas.contains(&"public".to_string()));
    }

    #[test]
    fn test_register_and_get_table() {
        let schema = EngramDBSchema::new();
        let table = make_test_table();
        schema.register_table("users".to_string(), table);

        assert!(schema.table_exist("users"));
        assert!(!schema.table_exist("nonexistent"));

        let names = schema.table_names();
        assert_eq!(names.len(), 1);
        assert_eq!(names[0], "users");

        let table = schema.table("users").unwrap();
        assert!(table.is_some());
    }
}
