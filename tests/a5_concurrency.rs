//! A5 多连接并发闸门（llmRx `SetMaxOpenConns(16)`）
//!
//! 现状：`Database` 非线程安全，多 Connection 同文件不支持。
//! 本测试用 `Arc<Mutex<Connection>>` 模拟 16 并发客户端：
//! - 验证并发写入不丢数据、不串扰
//! - 钉死「真·多连接」缺口（需外部锁串行化）
//!
//! 运行：`cargo test --test a5_concurrency -- --nocapture`

use engramdb::Connection;
use std::sync::{Arc, Mutex};
use std::thread;

/// 16 线程 × 每线程 N 行，经共享连接写入，总行数必须精确
#[test]
fn a5_16_threads_shared_connection_no_lost_writes() {
    let conn = Connection::open(":memory:").unwrap();
    let conn = Arc::new(Mutex::new(conn));
    {
        let mut c = conn.lock().unwrap();
        c.execute("CREATE TABLE t (id INT PRIMARY KEY, tid INT, v TEXT)")
            .unwrap();
    }

    let n_threads = 16usize;
    let rows_per_thread = 200usize;
    let big = "x".repeat(64);

    let mut handles = Vec::with_capacity(n_threads);
    for tid in 0..n_threads {
        let conn = Arc::clone(&conn);
        let big = big.clone();
        handles.push(thread::spawn(move || {
            for i in 0..rows_per_thread {
                let id = (tid * rows_per_thread + i) as i64;
                let sql = format!("INSERT INTO t VALUES ({}, {}, '{}')", id, tid, big);
                let mut c = conn.lock().unwrap();
                c.execute(&sql).expect("insert failed");
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    let mut c = conn.lock().unwrap();
    let r = c.execute("SELECT COUNT(*) FROM t").unwrap();
    let count = match &r.rows[0][0] {
        engramdb::Value::Int64(i) => *i,
        engramdb::Value::Int32(i) => *i as i64,
        other => panic!("unexpected count type: {:?}", other),
    };
    let expected = (n_threads * rows_per_thread) as i64;
    assert_eq!(
        count, expected,
        "A5 FAIL: 期望 {} 行，实际 {} 行（丢写）",
        expected, count
    );

    // 每线程的行数也必须精确（不串扰）
    for tid in 0..n_threads as i64 {
        let r = c
            .execute(&format!("SELECT COUNT(*) FROM t WHERE tid = {}", tid))
            .unwrap();
        let n = match &r.rows[0][0] {
            engramdb::Value::Int64(i) => *i,
            engramdb::Value::Int32(i) => *i as i64,
            other => panic!("unexpected: {:?}", other),
        };
        assert_eq!(
            n,
            rows_per_thread as i64,
            "A5 FAIL: tid={} 行数 {} ≠ {}（串扰）",
            tid,
            n,
            rows_per_thread
        );
    }
    println!(
        "A5a PASS: 16 线程 × {} 行 = {} 行全落盘，无丢写无串扰",
        rows_per_thread, expected
    );
}

/// 16 线程并发事务（BEGIN/INSERT/COMMIT），验证事务边界不互相污染
///
/// 事务必须整段持锁：BEGIN..COMMIT 是一个临界区，
/// 中间放锁会让其他线程的 BEGIN 交错进来污染事务状态。
#[test]
fn a5_16_threads_concurrent_txns() {
    let conn = Connection::open(":memory:").unwrap();
    let conn = Arc::new(Mutex::new(conn));
    {
        let mut c = conn.lock().unwrap();
        c.execute("CREATE TABLE t (id INT PRIMARY KEY, tid INT)").unwrap();
    }

    let n_threads = 16usize;
    let rows_per_txn = 10usize;
    let txns_per_thread = 5usize;

    let mut handles = Vec::with_capacity(n_threads);
    for tid in 0..n_threads {
        let conn = Arc::clone(&conn);
        handles.push(thread::spawn(move || {
            for txn in 0..txns_per_thread {
                // 整段事务持锁
                let mut c = conn.lock().unwrap();
                c.execute("BEGIN").unwrap();
                for i in 0..rows_per_txn {
                    let id = ((tid * txns_per_thread + txn) * rows_per_txn + i) as i64;
                    c.execute(&format!("INSERT INTO t VALUES ({}, {})", id, tid))
                        .unwrap();
                }
                c.execute("COMMIT").unwrap();
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    let mut c = conn.lock().unwrap();
    let r = c.execute("SELECT COUNT(*) FROM t").unwrap();
    let count = match &r.rows[0][0] {
        engramdb::Value::Int64(i) => *i,
        engramdb::Value::Int32(i) => *i as i64,
        other => panic!("{:?}", other),
    };
    let expected = (n_threads * txns_per_thread * rows_per_txn) as i64;
    assert_eq!(count, expected, "A5 FAIL: 事务并发丢写 {} ≠ {}", count, expected);
    println!(
        "A5b PASS: 16 线程 × {} 事务 × {} 行 = {} 行全落盘",
        txns_per_thread, rows_per_txn, expected
    );
}

/// A5c：真·多 Connection 同文件共享数据（v0.22.5 Arc<Mutex> 注册表）
///
/// 同一 path 的多个 `Connection` 共享同一 `Database`。
/// `:memory:` 各自独立。`open_exclusive` 绕过注册表（崩溃恢复测试用）。
#[test]
fn a5c_true_multi_connection_shared() {
    let path = std::env::temp_dir().join("engramdb_a5c_shared.hdb");
    let path_str = path.to_string_lossy().to_string();
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}-wal", path_str));

    // 两个独立 Connection，同一文件
    let mut c1 = Connection::open(&path_str).unwrap();
    let mut c2 = Connection::open(&path_str).unwrap();

    c1.execute("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)").unwrap();
    // c2 看到同一张表
    c2.execute("INSERT INTO t VALUES (1, 'from_c2')").unwrap();
    // c1 读到 c2 写入的数据
    let r = c1.execute("SELECT COUNT(*) FROM t").unwrap();
    let n = match &r.rows[0][0] {
        engramdb::Value::Int64(i) => *i,
        engramdb::Value::Int32(i) => *i as i64,
        _ => -1,
    };
    assert_eq!(n, 1, "A5c FAIL: c1 未看到 c2 写入的数据");

    c1.execute("INSERT INTO t VALUES (2, 'from_c1')").unwrap();
    let r = c2.execute("SELECT COUNT(*) FROM t").unwrap();
    let n = match &r.rows[0][0] {
        engramdb::Value::Int64(i) => *i,
        engramdb::Value::Int32(i) => *i as i64,
        _ => -1,
    };
    assert_eq!(n, 2, "A5c FAIL: c2 未看到 c1 写入的数据");

    println!("A5c PASS: 同文件多 Connection 共享数据（Arc<Mutex> 注册表）");
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}-wal", path_str));
}

/// :memory: 连接各自独立（不共享）
#[test]
fn a5c_memory_connections_independent() {
    let mut c1 = Connection::open(":memory:").unwrap();
    let mut c2 = Connection::open(":memory:").unwrap();
    c1.execute("CREATE TABLE t (id INT PRIMARY KEY)").unwrap();
    c2.execute("CREATE TABLE t (id INT PRIMARY KEY)").unwrap();
    c1.execute("INSERT INTO t VALUES (1)").unwrap();
    let r1 = c1.execute("SELECT COUNT(*) FROM t").unwrap();
    let r2 = c2.execute("SELECT COUNT(*) FROM t").unwrap();
    let n1 = match &r1.rows[0][0] {
        engramdb::Value::Int64(i) => *i,
        _ => -1,
    };
    let n2 = match &r2.rows[0][0] {
        engramdb::Value::Int64(i) => *i,
        _ => -1,
    };
    assert_eq!(n1, 1);
    assert_eq!(n2, 0, ":memory: 不应共享");
    println!("A5c PASS: :memory: 连接各自独立");
}
