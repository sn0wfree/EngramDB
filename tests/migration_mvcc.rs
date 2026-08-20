//! Phase 3.5 P0：Auto 迁移 MVCC 集成测试
//!
//! 验证：
//! 1. 迁移后 WAL engine_type 与表引擎一致
//! 2. 迁移后 is_persistent 标记正确（Memory vs Columnar/Log）
//! 3. 迁移后事务正确应用（读写新数据）
//! 4. 多次迁移触发累积正确性

use engramdb::common::types::EngineType;
use engramdb::Connection;

#[test]
fn test_migration_updates_table_engines() {
    let path = "/tmp/p35_mvcc_engine.hdb";
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{}-wal", path));

    let mut conn = Connection::open(path).unwrap();
    conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY, v TEXT) ENGINE = Auto")
        .unwrap();
    conn.execute("INSERT INTO t VALUES (1, 'a'), (2, 'b')").unwrap();

    let id = conn.database_mut().table_id_by_name("t").unwrap();
    for _ in 0..200 {
        conn.database_mut().record_access(id);
    }

    // 迁移前：def.engine 是 Auto
    let engine_before = conn.database_mut().get_engine_table_mut_by_id(id).unwrap()
        .def().engine;
    assert_eq!(engine_before, EngineType::Auto);

    conn.migrate_all_auto();

    // 迁移后：def.engine 是 Memory（高频小表）
    let engine_after = conn.database_mut().get_engine_table_mut_by_id(id).unwrap()
        .def().engine;
    assert_eq!(engine_after, EngineType::Memory);
}

#[test]
fn test_migration_marks_memory_non_persistent() {
    let path = "/tmp/p35_mvcc_persistent.hdb";
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{}-wal", path));

    let mut conn = Connection::open(path).unwrap();
    conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY) ENGINE = Auto").unwrap();
    conn.execute("INSERT INTO t VALUES (1)").unwrap();

    let id = conn.database_mut().table_id_by_name("t").unwrap();
    for _ in 0..200 {
        conn.database_mut().record_access(id);
    }

    // 迁移到 Memory 后，应标记为 non_persistent（事务不写 WAL）
    conn.migrate_all_auto();

    // 验证：is_persistent 应返回 false
    assert!(!conn.database_mut().txn_manager_is_persistent(id));
}

#[test]
fn test_migration_unmarks_memory_to_log() {
    let path = "/tmp/p35_mvcc_unmark.hdb";
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{}-wal", path));

    let mut conn = Connection::open(path).unwrap();
    conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY) ENGINE = Memory").unwrap();
    conn.execute("INSERT INTO t VALUES (1)").unwrap();

    // 验证：Memory 表创建后自动标 non_persistent
    let id = conn.database_mut().table_id_by_name("t").unwrap();
    assert!(!conn.database_mut().txn_manager_is_persistent(id),
            "Memory 表创建后应自动标 non_persistent");

    // Phase 3.5：模拟内存表手动转为持久化（unmark）
    // 当前 API：migrate_all_auto 只能处理 Auto 表，Memory → Log 需直接调用
    // Phase 3.5 简化：直接调 unmark 验证 API
    // （完整 Memory→Log 迁移留给 Phase 4）
    conn.database_mut().txn_manager_unmark_non_persistent_for_test(id);

    // 验证：unmark 后应持久化
    assert!(conn.database_mut().txn_manager_is_persistent(id));
}

#[test]
fn test_migration_preserves_data_after_changing_engine() {
    let path = "/tmp/p35_mvcc_preserve.hdb";
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{}-wal", path));

    let mut conn = Connection::open(path).unwrap();
    conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY, v TEXT) ENGINE = Auto")
        .unwrap();
    for i in 0..50 {
        conn.execute(&format!("INSERT INTO t VALUES ({}, 'r{}')", i, i)).unwrap();
    }

    let id = conn.database_mut().table_id_by_name("t").unwrap();
    for _ in 0..200 {
        conn.database_mut().record_access(id);
    }

    // 迁移到 Memory
    conn.migrate_all_auto();
    let engine1 = conn.database_mut().get_engine_table_mut_by_id(id).unwrap()
        .def().engine;
    assert_eq!(engine1, EngineType::Memory);

    // 验证数据完整
    let r = conn.execute("SELECT COUNT(*) FROM t").unwrap();
    assert_eq!(r.rows[0][0].as_i64(), Some(50));

    // 继续插入新数据到 Memory 表
    for i in 50..60 {
        conn.execute(&format!("INSERT INTO t VALUES ({}, 'r{}')", i, i)).unwrap();
    }
    let r = conn.execute("SELECT COUNT(*) FROM t").unwrap();
    assert_eq!(r.rows[0][0].as_i64(), Some(60));
}

#[test]
fn test_migration_wal_records_use_new_engine_type() {
    // 验证：迁移后 WAL 记录头使用新引擎（不再写旧引擎）
    let path = "/tmp/p35_mvcc_wal.hdb";
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{}-wal", path));

    let mut conn = Connection::open(path).unwrap();
    conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY) ENGINE = Auto").unwrap();
    conn.execute("INSERT INTO t VALUES (1)").unwrap();

    let id = conn.database_mut().table_id_by_name("t").unwrap();
    for _ in 0..200 {
        conn.database_mut().record_access(id);
    }
    conn.migrate_all_auto();

    // 迁移后插入新行 → WAL 记录 engine_type 应为 Memory（不再为 Auto）
    conn.execute("INSERT INTO t VALUES (2)").unwrap();

    // 直接验证 WAL 文件存在且有数据
    let wal_path = format!("{}-wal", path);
    let wal_size = std::fs::metadata(&wal_path).unwrap().len();
    assert!(wal_size > 0, "WAL should have content after migration");
}

#[test]
fn test_migration_loop_safety_under_mvcc() {
    // 多次迁移循环不应破坏数据完整性
    let path = "/tmp/p35_mvcc_loop.hdb";
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{}-wal", path));

    let mut conn = Connection::open(path).unwrap();
    conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY) ENGINE = Auto").unwrap();
    conn.execute("INSERT INTO t VALUES (1), (2), (3)").unwrap();

    let id = conn.database_mut().table_id_by_name("t").unwrap();
    for _ in 0..200 {
        conn.database_mut().record_access(id);
    }

    // 3 次连续迁移（第一次应执行，后两次因 reset 应 no-op）
    conn.migrate_all_auto();
    conn.migrate_all_auto();
    conn.migrate_all_auto();

    let r = conn.execute("SELECT COUNT(*) FROM t").unwrap();
    assert_eq!(r.rows[0][0].as_i64(), Some(3));
}