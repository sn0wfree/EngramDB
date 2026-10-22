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

/// 钉死缺口：真·多 Connection 同文件（当前不支持）
///
/// llmRx `SetMaxOpenConns(16)` 期望 16 个独立 DB 连接。
/// EngramDB `Database` 非线程安全 → 必须外部锁串行化。
/// 此测试记录契约：共享锁是当前唯一正确用法。
#[test]
fn a5_multi_connection_gap_documented() {
    // 模拟「真·多连接」：各自 open 同一 path —— 内存库各是独立实例
    // （文件库行为另测；此处记录语义差异）
    let mut c1 = Connection::open(":memory:").unwrap();
    let mut c2 = Connection::open(":memory:").unwrap();
    c1.execute("CREATE TABLE t (id INT PRIMARY KEY)").unwrap();
    c2.execute("CREATE TABLE t (id INT PRIMARY KEY)").unwrap();
    c1.execute("INSERT INTO t VALUES (1)").unwrap();
    c2.execute("INSERT INTO t VALUES (2)").unwrap();

    let r1 = c1.execute("SELECT COUNT(*) FROM t").unwrap();
    let r2 = c2.execute("SELECT COUNT(*) FROM t").unwrap();
    let n1 = match &r1.rows[0][0] {
        engramdb::Value::Int64(i) => *i,
        engramdb::Value::Int32(i) => *i as i64,
        _ => -1,
    };
    let n2 = match &r2.rows[0][0] {
        engramdb::Value::Int64(i) => *i,
        engramdb::Value::Int32(i) => *i as i64,
        _ => -1,
    };
    // 各自独立：n1=1, n2=1 —— 不是共享库
    assert_eq!(n1, 1);
    assert_eq!(n2, 1);
    println!(
        "A5c GAP: 真·多 Connection 同文件不支持（Database 非线程安全）\
         —— llmRx SetMaxOpenConns(16) 需外部 Arc<Mutex<>> 串行化；当前契约已钉死"
    );
}
