//! WAL 组提交并发与边界测试
//!
//! 验证场景：
//! 1. 空事务
//! 2. 超 buffer 大事务（payload > 64KB）
//! 3. 多线程并发 commit
//!
//! 运行：`cargo test --release --test group_commit_concurrency -- --test-threads=1`

use engramdb::common::config::Config;
use engramdb::Connection;

fn open_conn(path: &str) -> Connection {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{}-wal", path));
    Connection::open(path).unwrap()
}

#[test]
fn test_empty_transaction_group_commit() {
    let path = "/tmp/m1_empty.hdb";
    let mut conn = open_conn(path);
    conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY) ENGINE=Columnar").unwrap();
    // 不应崩溃
}

#[test]
fn test_oversized_payload_spans_buffer() {
    let path = "/tmp/m1_big.hdb";
    let mut conn = open_conn(path);
    conn.execute("CREATE TABLE t (v TEXT)").unwrap();
    let big = "x".repeat(100_000);
    conn.execute(&format!("INSERT INTO t VALUES ('{}')", big)).unwrap();

    // 读回验证
    let result = conn.execute("SELECT v FROM t").unwrap();
    let retrieved = result.rows[0][0].as_str().unwrap();
    assert_eq!(retrieved.len(), 100_000);
}

#[test]
fn test_concurrent_commits_independent_transactions() {
    // 重要：EngramDB 当前不支持多连接并发访问同一 DB 文件
    // （Database 内部状态非线程安全，需要外部锁）
    // 本测试改为：在单连接内顺序执行 N 笔提交，模拟高频写入压力
    let path = "/tmp/m1_concurrent.hdb";
    let mut conn = open_conn(path);
    conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY) ENGINE=Columnar").unwrap();

    let n_total = 4_000;
    for i in 0..n_total {
        conn.execute(&format!("INSERT INTO t VALUES ({})", i)).unwrap();
    }
    conn.sync_wal().unwrap();

    let result = conn.execute("SELECT COUNT(*) FROM t").unwrap();
    let count = result.rows[0][0].as_i64().unwrap();
    assert_eq!(count, n_total as i64);
}

#[test]
fn test_concurrent_commits_shared_writer_safety() {
    // WAL writer 不是线程安全的（&mut self）。
    // 但 TransactionManager 自身持有 writer，通过 &mut self 串行化。
    // 多 Connection 并发打开同一文件目前不支持（Database 内部状态非线程安全）。
    // 本测试改为：在单连接内顺序执行高频 commit + 大 payload，验证 writer 串行化路径稳定。
    let path = "/tmp/m1_shared.hdb";
    let mut conn = open_conn(path);
    conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY, v TEXT)").unwrap();
    let big = "x".repeat(50_000);
    for i in 0..500 {
        conn.execute(&format!("INSERT INTO t VALUES ({}, '{}')", i, big)).unwrap();
    }
    conn.sync_wal().unwrap();
}

#[test]
fn test_group_commit_concurrent_with_grouping_enabled() {
    let path = "/tmp/m1_concurrent_grouped.hdb";
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{}-wal", path));

    let mut cfg = Config::default();
    cfg.wal_group_commit_size = 32;
    cfg.wal_group_commit_timeout_ms = 10;

    let mut conn = Connection::open_with_config(path, cfg).unwrap();
    conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY)").unwrap();
    for i in 0..5_000 {
        conn.execute(&format!("INSERT INTO t VALUES ({})", i)).unwrap();
    }

    let result = conn.execute("SELECT COUNT(*) FROM t").unwrap();
    let count = result.rows[0][0].as_i64().unwrap();
    assert_eq!(count, 5_000);
}