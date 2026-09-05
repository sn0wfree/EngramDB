//! Phase 3 P0：Auto 表数据迁移集成测试
//!
//! 验证：
//! 1. migrate_all_auto() 实际搬迁数据
//! 2. 迁移后数据完整
//! 3. 迁移后 engine 字段已更新
//! 4. on_commit 自动触发迁移
//! 5. 多次访问后热表 → Memory
//!
//! 运行：`cargo test --release --test migration_integration -- --test-threads=1`

use engramdb::common::types::EngineType;
use engramdb::Connection;

#[test]
fn test_migrate_all_auto_on_high_freq_small_table() {
    let path = "/tmp/p3_migrate_hot.hdb";
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

    let results = conn.migrate_all_auto();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].new_engine, EngineType::Memory);
    assert_eq!(results[0].stats.rows_migrated, 1);
    assert!(results[0].stats.duration_ms < 100);
}

#[test]
fn test_migrate_all_auto_on_cold_large_table() {
    let path = "/tmp/p3_migrate_cold.hdb";
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{}-wal", path));

    let mut conn = Connection::open(path).unwrap();
    conn.execute("CREATE TABLE cold (id INT64 PRIMARY KEY, v TEXT) ENGINE = Auto")
        .unwrap();

    let id = conn.database_mut().table_id_by_name("cold").unwrap();
    let table = conn.database_mut().get_engine_table_mut_by_id(id).unwrap();
    if let engramdb::storage::engine::EngineTable::Columnar(t) = table {
        t.def_mut().row_count = 200_000;
    }
    drop(table);

    let results = conn.migrate_all_auto();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].new_engine, EngineType::Log);
}

#[test]
fn test_migration_preserves_data() {
    let path = "/tmp/p3_migrate_data.hdb";
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{}-wal", path));

    let mut conn = Connection::open(path).unwrap();
    conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY, v TEXT) ENGINE = Auto")
        .unwrap();
    for i in 0..100 {
        conn.execute(&format!("INSERT INTO t VALUES ({}, 'r{}')", i, i)).unwrap();
    }

    let id = conn.database_mut().table_id_by_name("t").unwrap();
    for _ in 0..200 {
        conn.database_mut().record_access(id);
    }

    conn.migrate_all_auto();

    // 数据完整性验证
    let result = conn.execute("SELECT COUNT(*) FROM t").unwrap();
    assert_eq!(result.rows[0][0].as_i64(), Some(100));

    let result = conn.execute("SELECT * FROM t WHERE id = 50").unwrap();
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0][0].as_i64(), Some(50));
    assert_eq!(result.rows[0][1].as_str(), Some("r50"));
}

#[test]
fn test_migration_no_decision_no_action() {
    let path = "/tmp/p3_migrate_noop.hdb";
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{}-wal", path));

    let mut conn = Connection::open(path).unwrap();
    // 显式 Columnar（不应迁移）
    conn.execute("CREATE TABLE fixed (id INT64 PRIMARY KEY) ENGINE = Columnar")
        .unwrap();
    conn.execute("INSERT INTO fixed VALUES (1)").unwrap();

    let id = conn.database_mut().table_id_by_name("fixed").unwrap();
    for _ in 0..1000 {
        conn.database_mut().record_access(id);
    }

    let results = conn.migrate_all_auto();
    assert_eq!(results.len(), 0, "Columnar 表不应触发迁移");
}

#[test]
fn test_migration_loop_protection() {
    // 验证：迁移成功后 HeatTracker.reset() 阻止无限循环
    let path = "/tmp/p3_migrate_loop.hdb";
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{}-wal", path));

    let mut conn = Connection::open(path).unwrap();
    conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY) ENGINE = Auto").unwrap();
    conn.execute("INSERT INTO t VALUES (1)").unwrap();

    let id = conn.database_mut().table_id_by_name("t").unwrap();
    for _ in 0..200 {
        conn.database_mut().record_access(id);
    }

    // 第一次迁移
    let results1 = conn.migrate_all_auto();
    assert_eq!(results1.len(), 1);

    // 立即第二次：HeatTracker 已 reset，应不再决策
    let results2 = conn.migrate_all_auto();
    assert_eq!(results2.len(), 0, "迁移后立即 tick 应无决策（reset 生效）");
}

#[test]
fn test_migration_via_on_commit_tick() {
    // on_commit 每 1000 次触发一次；模拟 1000 次 commit 触发迁移
    let path = "/tmp/p3_migrate_oncommit.hdb";
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{}-wal", path));

    let mut conn = Connection::open(path).unwrap();
    conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY) ENGINE = Auto").unwrap();
    conn.execute("INSERT INTO t VALUES (1)").unwrap();

    let id = conn.database_mut().table_id_by_name("t").unwrap();
    for _ in 0..200 {
        conn.database_mut().record_access(id);
    }

    // 触发 1000 次 on_commit
    let mut triggered = 0;
    for _ in 0..1000 {
        if conn.database_mut().on_commit() {
            triggered += 1;
        }
    }
    assert!(triggered >= 1, "on_commit 应至少触发 1 次 tick");

    // 验证迁移已执行（通过查看表引擎）
    let table_engine = {
        let db = conn.database_mut();
        let table_id = db.table_id_by_name("t").unwrap();
        if let Some(et) = db.get_engine_table_mut_by_id(table_id) {
            et.engine_type()
        } else {
            panic!("table not found");
        }
    };
    // 应该是 Memory（已迁移）
    assert_eq!(table_engine, EngineType::Memory);
}
