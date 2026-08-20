//! Phase 2.5 P2：Auto 表分层迁移集成测试
//!
//! 验证：
//! 1. tick 返回正确决策
//! 2. 高频小表 → Memory
//! 3. 冷大表 → Log
//! 4. 不应迁移 Columnar 表
//! 5. 多 决策批量返回
//!
//! 运行：`cargo test --release --test tier_migration_basic -- --test-threads=1`

use engramdb::common::types::EngineType;
use engramdb::Connection;

#[test]
fn test_tier_migration_tick_empty() {
    let path = "/tmp/p25_tier_empty.hdb";
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{}-wal", path));

    let mut conn = Connection::open(path).unwrap();
    let decisions = conn.tier_migration_tick();
    assert_eq!(decisions.len(), 0);
}

#[test]
fn test_tier_migration_high_freq_small_to_memory() {
    let path = "/tmp/p25_tier_hot.hdb";
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{}-wal", path));

    let mut conn = Connection::open(path).unwrap();
    conn.execute("CREATE TABLE hot (id INT64 PRIMARY KEY, v TEXT) ENGINE = Auto")
        .unwrap();
    conn.execute("INSERT INTO hot VALUES (1, 'a')").unwrap();

    let id = conn.database_mut().table_id_by_name("hot").unwrap();
    for _ in 0..200 {
        conn.database_mut().record_access(id);
    }

    let decisions = conn.tier_migration_tick();
    assert_eq!(decisions.len(), 1);
    let d = &decisions[0];
    assert_eq!(d.to_engine, EngineType::Memory);
    assert_eq!(d.from_engine, EngineType::Columnar);
    assert!(d.heat_score > 100.0, "score should be high: {}", d.heat_score);
}

#[test]
fn test_tier_migration_cold_large_to_log() {
    let path = "/tmp/p25_tier_cold.hdb";
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{}-wal", path));

    let mut conn = Connection::open(path).unwrap();
    conn.execute("CREATE TABLE cold (id INT64 PRIMARY KEY, v TEXT) ENGINE = Auto")
        .unwrap();

    let id = conn.database_mut().table_id_by_name("cold").unwrap();
    {
        let table = conn.database_mut().get_engine_table_mut_by_id(id).unwrap();
        if let engramdb::storage::engine::EngineTable::Columnar(t) = table {
            t.def_mut().row_count = 200_000;
        }
    }

    let decisions = conn.tier_migration_tick();
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].to_engine, EngineType::Log);
}

#[test]
fn test_tier_migration_no_migration_for_explicit_columnar() {
    let path = "/tmp/p25_tier_fixed.hdb";
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{}-wal", path));

    let mut conn = Connection::open(path).unwrap();
    conn.execute("CREATE TABLE fixed (id INT64 PRIMARY KEY) ENGINE = Columnar")
        .unwrap();
    conn.execute("INSERT INTO fixed VALUES (1)").unwrap();

    let id = conn.database_mut().table_id_by_name("fixed").unwrap();
    for _ in 0..100 {
        conn.database_mut().record_access(id);
    }

    let decisions = conn.tier_migration_tick();
    assert_eq!(decisions.len(), 0, "Columnar 表不应触发迁移");
}

#[test]
fn test_tier_migration_mixed_tables() {
    let path = "/tmp/p25_tier_mixed.hdb";
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{}-wal", path));

    let mut conn = Connection::open(path).unwrap();
    // Auto 高频小表
    conn.execute("CREATE TABLE hot_auto (id INT64 PRIMARY KEY) ENGINE = Auto").unwrap();
    conn.execute("INSERT INTO hot_auto VALUES (1)").unwrap();
    // Auto 冷大表
    conn.execute("CREATE TABLE cold_auto (id INT64 PRIMARY KEY) ENGINE = Auto").unwrap();
    {
        let id = conn.database_mut().table_id_by_name("cold_auto").unwrap();
        let table = conn.database_mut().get_engine_table_mut_by_id(id).unwrap();
        if let engramdb::storage::engine::EngineTable::Columnar(t) = table {
            t.def_mut().row_count = 200_000;
        }
    }
    // Auto 温表（无访问 + 中等大小）
    conn.execute("CREATE TABLE warm_auto (id INT64 PRIMARY KEY) ENGINE = Auto").unwrap();
    conn.execute("INSERT INTO warm_auto VALUES (1), (2), (3)").unwrap();
    {
        let id = conn.database_mut().table_id_by_name("warm_auto").unwrap();
        let table = conn.database_mut().get_engine_table_mut_by_id(id).unwrap();
        if let engramdb::storage::engine::EngineTable::Columnar(t) = table {
            t.def_mut().row_count = 50_000;
        }
    }
    // 显式 Columnar 不应迁移
    conn.execute("CREATE TABLE fixed_col (id INT64 PRIMARY KEY) ENGINE = Columnar").unwrap();

    // 给 hot_auto 大量访问
    let hot_id = conn.database_mut().table_id_by_name("hot_auto").unwrap();
    for _ in 0..200 {
        conn.database_mut().record_access(hot_id);
    }

    let decisions = conn.tier_migration_tick();
    assert_eq!(decisions.len(), 2, "应只有 hot_auto + cold_auto 触发");

    let hot_decision = decisions.iter().find(|d| d.table_id == hot_id).unwrap();
    assert_eq!(hot_decision.to_engine, EngineType::Memory);

    let cold_id = conn.database_mut().table_id_by_name("cold_auto").unwrap();
    let cold_decision = decisions.iter().find(|d| d.table_id == cold_id).unwrap();
    assert_eq!(cold_decision.to_engine, EngineType::Log);
}