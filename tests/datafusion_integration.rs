//! DataFusion 集成测试
//!
//! 验证 SQL → DataFusion → HybridDB TableProvider → 存储引擎 的完整链路

use std::sync::Arc;

use datafusion::prelude::*;
use datafusion::arrow::util::pretty::print_batches;

use hybriddb::datafusion_ext::catalog::HybridDBSchema;
use hybriddb::datafusion_ext::table_provider::HybridDBTable;
use hybriddb::common::types::DataType;
use hybriddb::Value;

/// 创建测试表
fn create_test_table() -> HybridDBTable {
    let columns = vec![
        ("id".to_string(), DataType::Int64, false),
        ("name".to_string(), DataType::Varchar, true),
        ("age".to_string(), DataType::Int32, true),
        ("score".to_string(), DataType::Float64, true),
        ("active".to_string(), DataType::Boolean, false),
    ];

    let mut rows = Vec::new();
    let names = vec!["alice", "bob", "charlie", "diana", "eve", "frank", "grace", "henry"];
    for i in 0..8 {
        rows.push(vec![
            Value::Int64(i as i64 + 1),
            Value::Varchar(names[i].to_string()),
            Value::Int32(20 + i as i32 * 3),
            Value::Float64(60.0 + i as f64 * 5.5),
            Value::Boolean(i % 2 == 0),
        ]);
    }

    HybridDBTable::new("users".to_string(), columns, rows)
}

#[tokio::test]
async fn test_simple_select() {
    let ctx = create_context();

    let result = ctx.sql("SELECT id, name FROM users ORDER BY id").await.unwrap();
    let batches = result.collect().await.unwrap();

    assert_eq!(batches.len(), 1);
    assert_eq!(batches[0].num_columns(), 2);
    assert_eq!(batches[0].num_rows(), 8);

    print_batches(&batches).unwrap();
}

#[tokio::test]
async fn test_select_star() {
    let ctx = create_context();

    let result = ctx.sql("SELECT * FROM users").await.unwrap();
    let batches = result.collect().await.unwrap();

    assert_eq!(batches[0].num_columns(), 5);
    assert_eq!(batches[0].num_rows(), 8);
}

#[tokio::test]
async fn test_count() {
    let ctx = create_context();

    let result = ctx.sql("SELECT COUNT(*) as cnt FROM users").await.unwrap();
    let batches = result.collect().await.unwrap();

    assert_eq!(batches[0].num_rows(), 1);
    // COUNT(*) = 8
    let cnt = batches[0]
        .column(0)
        .as_any()
        .downcast_ref::<datafusion::arrow::array::Int64Array>()
        .unwrap()
        .value(0);
    assert_eq!(cnt, 8);
}

#[tokio::test]
async fn test_where_filter() {
    let ctx = create_context();

    let result = ctx.sql("SELECT name, age FROM users WHERE age > 30 ORDER BY age").await.unwrap();
    let batches = result.collect().await.unwrap();

    // age > 30: charlie(26)? 不对, 20+0*3=20, 20+1*3=23, 20+2*3=26, 20+3*3=29, 20+4*3=32...
    // age > 30 的有: eve(32), frank(35), grace(38), henry(41) = 4 人
    assert_eq!(batches[0].num_rows(), 4);
}

#[tokio::test]
async fn test_sum_avg() {
    let ctx = create_context();

    let result = ctx.sql("SELECT SUM(score), AVG(score) FROM users").await.unwrap();
    let batches = result.collect().await.unwrap();

    assert_eq!(batches[0].num_rows(), 1);

    // score: 60, 65.5, 71, 76.5, 82, 87.5, 93, 98.5
    // sum = 60+65.5+71+76.5+82+87.5+93+98.5 = 634
    let sum_val = batches[0]
        .column(0)
        .as_any()
        .downcast_ref::<datafusion::arrow::array::Float64Array>()
        .unwrap()
        .value(0);
    assert!((sum_val - 634.0).abs() < 0.01);
}

#[tokio::test]
async fn test_group_by() {
    let ctx = create_context();

    let result = ctx.sql("SELECT active, COUNT(*) as cnt FROM users GROUP BY active ORDER BY active").await.unwrap();
    let batches = result.collect().await.unwrap();

    // 2 组: active=true(4人), active=false(4人)
    assert_eq!(batches[0].num_rows(), 2);
}

#[tokio::test]
async fn test_order_by_limit() {
    let ctx = create_context();

    let result = ctx.sql("SELECT name, score FROM users ORDER BY score DESC LIMIT 3").await.unwrap();
    let batches = result.collect().await.unwrap();

    assert_eq!(batches[0].num_rows(), 3);

    // 最高分前三: henry(98.5), grace(93), frank(87.5)
    let names: Vec<&str> = batches[0]
        .column(0)
        .as_any()
        .downcast_ref::<datafusion::arrow::array::StringArray>()
        .unwrap()
        .iter()
        .map(|s| s.unwrap())
        .collect();
    assert_eq!(names, vec!["henry", "grace", "frank"]);
}

#[tokio::test]
async fn test_where_with_and() {
    let ctx = create_context();

    let result = ctx.sql("SELECT name FROM users WHERE active = true AND age > 25 ORDER BY name").await.unwrap();
    let batches = result.collect().await.unwrap();

    // active=true: id 1,3,5,7 → alice(20), charlie(26), eve(32), grace(38)
    // age > 25: charlie, eve, grace = 3 人
    assert_eq!(batches[0].num_rows(), 3);
}

fn create_context() -> SessionContext {
    let ctx = SessionContext::new();

    // 注册测试表
    let table = create_test_table();
    let schema = HybridDBSchema::new();
    schema.register_table("users".to_string(), table);

    ctx.register_catalog("hybriddb", Arc::new(HybridDBCatalog::from_schema(schema)));
    ctx.sql("USE hybriddb.public").unwrap(); // 切换到默认 schema

    ctx
}

/// 辅助: 从 Schema 构建 Catalog
struct HybridDBCatalog {
    schema: Arc<HybridDBSchema>,
}

impl HybridDBCatalog {
    fn from_schema(schema: HybridDBSchema) -> Self {
        Self {
            schema: Arc::new(schema),
        }
    }
}

impl datafusion::catalog::CatalogProvider for HybridDBCatalog {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn schema_names(&self) -> Vec<String> {
        vec!["public".to_string()]
    }

    fn schema(&self, _name: &str) -> Option<Arc<dyn datafusion::catalog::SchemaProvider>> {
        Some(self.schema.clone() as Arc<dyn datafusion::catalog::SchemaProvider>)
    }
}
