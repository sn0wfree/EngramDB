//! Phase 3 P1-A：Float/Varchar Bloom Filter 集成测试
//!
//! 验证：
//! 1. Float64 列构建 Bloom
//! 2. Varchar 列构建 Bloom
//! 3. Float 等值查询走 Bloom 跳读
//! 4. Varchar 等值查询走 Bloom 跳读
//!
//! 运行：`cargo test --release --test bloom_extended -- --test-threads=1`

use std::time::Instant;

use engramdb::Connection;

#[test]
fn test_bloom_float_eq_skip() {
    let path = "/tmp/p3_bloom_float.hdb";
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{}-wal", path));

    let mut conn = Connection::open(path).unwrap();
    conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY, score FLOAT64)").unwrap();
    for i in 0..1000 {
        let score = i as f64 * 1.5;
        conn.execute(&format!("INSERT INTO t VALUES ({}, {})", i, score)).unwrap();
    }
    conn.sync_wal().unwrap();

    // 等值查询 — 不存在的 score
    let start = Instant::now();
    let r = conn.execute("SELECT * FROM t WHERE score = 99999.99").unwrap();
    let dur = start.elapsed();
    assert_eq!(r.rows.len(), 0);
    println!("Float64 not-found query: {:?}", dur);
}

#[test]
fn test_bloom_varchar_eq_skip() {
    let path = "/tmp/p3_bloom_varchar.hdb";
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{}-wal", path));

    let mut conn = Connection::open(path).unwrap();
    conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY, name TEXT)").unwrap();
    for i in 0..1000 {
        conn.execute(&format!("INSERT INTO t VALUES ({}, 'name-{}')", i, i)).unwrap();
    }
    conn.sync_wal().unwrap();

    // 等值查询 — 不存在的 name
    let start = Instant::now();
    let r = conn.execute("SELECT * FROM t WHERE name = 'nonexistent-99999'").unwrap();
    let dur = start.elapsed();
    assert_eq!(r.rows.len(), 0);
    println!("Varchar not-found query: {:?}", dur);

    // 等值查询 — 存在的 name
    let r = conn.execute("SELECT * FROM t WHERE name = 'name-500'").unwrap();
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0][0].as_i64(), Some(500));
}

#[test]
fn test_bloom_float_eq_find() {
    let path = "/tmp/p3_bloom_float_find.hdb";
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{}-wal", path));

    let mut conn = Connection::open(path).unwrap();
    conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY, score FLOAT64)").unwrap();
    for i in 0..1000 {
        let score = i as f64 * 1.5;
        conn.execute(&format!("INSERT INTO t VALUES ({}, {})", i, score)).unwrap();
    }
    conn.sync_wal().unwrap();

    // 等值查询 — 存在的 score（id=500 → score=750.0）
    let r = conn.execute("SELECT * FROM t WHERE score = 750.0").unwrap();
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0][0].as_i64(), Some(500));
}

#[test]
fn test_bloom_varchar_eq_find_with_persist() {
    let path = "/tmp/p3_bloom_varchar_persist.hdb";
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{}-wal", path));

    {
        let mut conn = Connection::open(path).unwrap();
        conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY, name TEXT)").unwrap();
        for i in 0..500 {
            conn.execute(&format!("INSERT INTO t VALUES ({}, 'item-{}')", i, i)).unwrap();
        }
        conn.sync_wal().unwrap();
    }

    // 重启后 Bloom 应从 typed 数据重建（Varchar 也支持）
    let mut conn = Connection::open(path).unwrap();
    let r = conn.execute("SELECT * FROM t WHERE name = 'nonexistent-xyz'").unwrap();
    assert_eq!(r.rows.len(), 0);

    let r = conn.execute("SELECT * FROM t WHERE name = 'item-100'").unwrap();
    assert_eq!(r.rows.len(), 1);
}
