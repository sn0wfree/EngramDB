// M5 优化器适配（P5）集成测试：
// - 引擎能力检测表：planner 提前清晰报错（索引/向量/ALTER/UPDATE/DELETE）
// - ANALYZE 真实统计收集（row_count/NDV/直方图缓存）
// - JOIN 代价引擎加权（Memory/Log 扫描便宜 → 驱动表倾向）

#[test]
fn test_p5_engine_capability_errors() {
    let mut conn = super::Connection::open(":memory:").unwrap();
    conn.execute("CREATE TABLE col (id INT PRIMARY KEY, v INT)").unwrap();
    conn.execute("CREATE TABLE mem (id INT PRIMARY KEY, v INT) ENGINE = Memory").unwrap();
    conn.execute("CREATE TABLE log (ts INT64, v INT) ENGINE = Log").unwrap();
    conn.execute("INSERT INTO col VALUES (1, 10)").unwrap();

    // Memory：无索引/向量/ALTER
    let err = conn.execute("CREATE INDEX idx ON mem (v)").unwrap_err();
    assert!(err.to_string().contains("不支持") && err.to_string().contains("索引"), "got: {err}");
    // Log：无索引/UPDATE/DELETE（planner 提前拦截）
    let err = conn.execute("CREATE INDEX idx ON log (ts)").unwrap_err();
    assert!(err.to_string().contains("不支持"), "got: {err}");
    let err = conn.execute("UPDATE log SET v = 1 WHERE ts = 1").unwrap_err();
    assert!(err.to_string().contains("UPDATE"), "got: {err}");
    let err = conn.execute("DELETE FROM log WHERE ts = 1").unwrap_err();
    assert!(err.to_string().contains("DELETE"), "got: {err}");
    // Columnar 全支持
    conn.execute("CREATE INDEX idx ON col (v)").unwrap();
    // Memory/Log 仍支持 INSERT/查询
    conn.execute("INSERT INTO mem VALUES (1, 1)").unwrap();
    conn.execute("INSERT INTO log VALUES (1, 1)").unwrap();
    let r = conn.execute("SELECT COUNT(*) FROM mem").unwrap();
    assert_eq!(r.rows[0][0], super::Value::Int64(1));
}

#[test]
fn test_p5_analyze_collects_stats() {
    let mut conn = super::Connection::open(":memory:").unwrap();
    conn.execute("CREATE TABLE t (id INT, v TEXT)").unwrap();
    let stmt = conn.prepare("INSERT INTO t VALUES (?, ?)").unwrap();
    let mut batch = Vec::new();
    for i in 0..1000i64 {
        batch.push(vec![super::Value::Int64(i % 100), super::Value::Varchar(format!("v{}", i % 50))]);
    }
    conn.execute_prepared_batch(&stmt, &batch).unwrap();

    let r = conn.execute("ANALYZE TABLE t").unwrap();
    assert_eq!(r.rows[0][0], super::Value::Varchar("ANALYZE t ok".into()));

    let stats = conn.database_mut().statistics_cache().get("t").cloned().unwrap();
    assert_eq!(stats.row_count, 1000);
    assert_eq!(stats.columns.len(), 2);
    // id 列：100 个不同值，直方图已构建
    assert_eq!(stats.columns[0].ndv, 100);
    assert!(stats.columns[0].histogram.is_some(), "id 列应有直方图");
    // v 列：50 个不同值
    assert_eq!(stats.columns[1].ndv, 50);
}

#[test]
fn test_p5_analyze_engine_stats() {
    let mut conn = super::Connection::open(":memory:").unwrap();
    conn.execute("CREATE TABLE col (id INT, v INT)").unwrap();
    conn.execute("CREATE TABLE mem (id INT, v INT) ENGINE = Memory").unwrap();
    conn.execute("CREATE TABLE log (ts INT64, v INT) ENGINE = Log").unwrap();
    conn.execute("INSERT INTO col VALUES (1, 1)").unwrap();
    conn.execute("INSERT INTO mem VALUES (1, 1)").unwrap();
    conn.execute("INSERT INTO log VALUES (1, 1)").unwrap();
    for t in ["col", "mem", "log"] {
        conn.execute(&format!("ANALYZE TABLE {}", t)).unwrap();
    }
    let cache = conn.database_mut().statistics_cache().clone();
    assert_eq!(cache["col"].engine, crate::common::types::EngineType::Columnar);
    assert_eq!(cache["mem"].engine, crate::common::types::EngineType::Memory);
    assert_eq!(cache["log"].engine, crate::common::types::EngineType::Log);
    assert_eq!(cache["col"].row_count, 1);
    assert_eq!(cache["mem"].row_count, 1);
    assert_eq!(cache["log"].row_count, 1);
}

#[test]
fn test_p5_join_cost_engine_weight() {
    use crate::sql::cost_model::CostModel;
    use crate::sql::statistics::{ColumnStatistics, TableStatistics};
    use crate::executor::physical_plan::PhysicalPlan;
    use crate::sql::ast::Expression;

    // Memory 表 1000 行 vs Columnar 表 1000 行：扫描代价 Memory 低 10x
    let mk_stats = |name: &str, engine: crate::common::types::EngineType, rows: u64| TableStatistics {
        table_name: name.to_string(),
        engine,
        row_count: rows,
        columns: vec![ColumnStatistics {
            column_name: "id".to_string(),
            ndv: rows,
            null_count: 0,
            min_value: None,
            max_value: None,
            histogram: None,
        }],
    };
    let stats = vec![
        mk_stats("col_t", crate::common::types::EngineType::Columnar, 1000),
        mk_stats("mem_t", crate::common::types::EngineType::Memory, 1000),
    ];
    let model = CostModel::new(&stats);
    let scan = |t: &str| PhysicalPlan::TableScan {
        table_name: t.to_string(),
        column_indices: vec![0],
    };
    let cost_col = model.calculate(&scan("col_t")).total;
    let cost_mem = model.calculate(&scan("mem_t")).total;
    // Memory 扫描代价应显著低于 Columnar（权重 0.1）
    assert!(
        cost_mem < cost_col * 0.5,
        "Memory 扫描应便宜：mem={cost_mem:.4} col={cost_col:.4}"
    );
    let _ = Expression::Literal(super::Value::Int64(1));
}
