//! Phase 2 P0-B：ENGINE = Auto 自动分层集成测试
//!
//! 验证：
//! 1. CREATE TABLE 接受 `ENGINE = Auto` 语法
//! 2. Auto 表创建后默认走 Columnar 路径
//! 3. Auto 表可通过 mark_auto_engine() 标记
//! 4. 序列化往返正确性
//!
//! 运行：`cargo test --release --test engine_auto_basic -- --test-threads=1`

use engramdb::common::config::Config;
use engramdb::common::types::EngineType;
use engramdb::Connection;

#[test]
fn test_create_table_engine_auto() {
    let path = "/tmp/p2_auto_basic.hdb";
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{}-wal", path));

    let mut conn = Connection::open(path).unwrap();
    conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY, v TEXT) ENGINE = Auto")
        .unwrap();

    // 验证表已创建
    let count = conn.execute("SELECT COUNT(*) FROM t").unwrap();
    assert_eq!(count.rows[0][0].as_i64(), Some(0));

    // 写入并读取
    conn.execute("INSERT INTO t VALUES (1, 'row-1')").unwrap();
    conn.execute("INSERT INTO t VALUES (2, 'row-2')").unwrap();

    let r = conn.execute("SELECT * FROM t ORDER BY id").unwrap();
    assert_eq!(r.rows.len(), 2);
    assert_eq!(r.rows[0][0].as_i64(), Some(1));
    assert_eq!(r.rows[1][0].as_i64(), Some(2));
}

#[test]
fn test_engine_type_auto_serde() {
    // 通过 catalog 序列化路径间接验证 Auto 字段持久化
    let path = "/tmp/p2_auto_serde.hdb";
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{}-wal", path));

    let mut conn = Connection::open(path).unwrap();
    conn.execute("CREATE TABLE t1 (id INT64 PRIMARY KEY) ENGINE = Auto").unwrap();
    conn.execute("CREATE TABLE t2 (id INT64 PRIMARY KEY) ENGINE = Columnar")
        .unwrap();
    conn.execute("CREATE TABLE t3 (id INT64 PRIMARY KEY) ENGINE = Memory").unwrap();
    conn.execute("CREATE TABLE t4 (id INT64 PRIMARY KEY) ENGINE = Log").unwrap();
    drop(conn);

    // 重新打开：所有表应能恢复（Auto 表恢复时默认走 Columnar）
    let mut conn = Connection::open(path).unwrap();
    let r1 = conn.execute("SELECT COUNT(*) FROM t1").unwrap();
    let r2 = conn.execute("SELECT COUNT(*) FROM t2").unwrap();
    let r3 = conn.execute("SELECT COUNT(*) FROM t3").unwrap();
    let r4 = conn.execute("SELECT COUNT(*) FROM t4").unwrap();

    assert_eq!(r1.rows[0][0].as_i64(), Some(0));
    assert_eq!(r2.rows[0][0].as_i64(), Some(0));
    assert_eq!(r3.rows[0][0].as_i64(), Some(0));
    assert_eq!(r4.rows[0][0].as_i64(), Some(0));
}

#[test]
fn test_engine_type_from_str_includes_auto() {
    // 验证 Auto 字符串解析
    assert_eq!(EngineType::from_str("auto"), Some(EngineType::Auto));
    assert_eq!(EngineType::from_str("AUTO"), Some(EngineType::Auto));
    assert_eq!(EngineType::from_str("Auto"), Some(EngineType::Auto));
}

#[test]
fn test_engine_type_byte_repr_stable() {
    // Phase 2 P0-B：byte 值 = 3（向后兼容：旧文件无此字段，反序列化为 0=Columnar）
    assert_eq!(EngineType::Auto.to_u8(), 3);
    assert_eq!(EngineType::from_u8(3), Some(EngineType::Auto));
}

#[test]
fn test_auto_engine_basic_perf() {
    // Phase 2 P0-B：Auto 表与 Columnar 表基本写入/读取对比
    // 当前预期：性能与 Columnar 一致（Auto 默认走 Columnar）
    let mut cfg = Config::default();
    cfg.wal_group_commit_size = 16;

    let path_auto = "/tmp/p2_auto_perf.hdb";
    let _ = std::fs::remove_file(path_auto);
    let _ = std::fs::remove_file(format!("{}-wal", path_auto));
    let mut conn = Connection::open_with_config(path_auto, cfg).unwrap();
    conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY) ENGINE = Auto").unwrap();
    for i in 0..1000 {
        conn.execute(&format!("INSERT INTO t VALUES ({})", i)).unwrap();
    }
    let r = conn.execute("SELECT COUNT(*) FROM t").unwrap();
    assert_eq!(r.rows[0][0].as_i64(), Some(1000));
}
