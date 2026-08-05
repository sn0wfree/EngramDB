// LogEngine（M3）集成测试：ENGINE=Log 语法、追加写入、MinMax 跳读、
// 持久化往返、禁 UPDATE/DELETE/UPSERT、TRUNCATE、事务追加

#[test]
fn test_log_engine_sql_basic() {
    let mut conn = super::Connection::open(":memory:").unwrap();
    conn.execute("CREATE TABLE events (ts INT64, event TEXT) ENGINE = Log").unwrap();
    for i in 0..100 {
        conn.execute(&format!("INSERT INTO events VALUES ({}, 'e{}')", i, i)).unwrap();
    }
    let r = conn.execute("SELECT COUNT(*) FROM events").unwrap();
    assert_eq!(r.rows[0][0], Value::Int64(100));
    // 时间范围查询（Int64 字面量 + Int64 时间列）
    let r = conn.execute("SELECT * FROM events WHERE ts >= 95").unwrap();
    assert_eq!(r.rows.len(), 5);
    let r = conn.execute("SELECT * FROM events WHERE ts >= 95 AND ts < 98").unwrap();
    assert_eq!(r.rows.len(), 3);
    let r = conn.execute("SELECT * FROM events WHERE ts = 10").unwrap();
    assert_eq!(r.rows[0][1], Value::Varchar("e10".into()));
    // GROUP BY / ORDER BY 走上层算子
    let r = conn.execute("SELECT event, COUNT(*) FROM events WHERE ts < 10 GROUP BY event").unwrap();
    assert_eq!(r.rows.len(), 10);
    let r = conn.execute("SELECT ts FROM events ORDER BY ts DESC LIMIT 3").unwrap();
    assert_eq!(r.rows[0][0], Value::Int64(99));
}

#[test]
fn test_log_engine_blocks_and_skip() {
    let mut conn = super::Connection::open(":memory:").unwrap();
    conn.execute("CREATE TABLE t (ts INT64, v DOUBLE) ENGINE = Log").unwrap();
    // 超过 1 个块（LOG_BLOCK_ROWS = 8192）
    let n = 8192 + 4096;
    let mut batch = Vec::with_capacity(n);
    for i in 0..n {
        batch.push(vec![Value::Int64(i as i64), Value::Float64(i as f64)]);
    }
    conn.execute_prepared_batch(
        &conn.prepare("INSERT INTO t VALUES (?, ?)").unwrap(),
        &batch,
    ).unwrap();
    // 块间跳读：只命中最后一个块（时间范围命中最小块）
    let r = conn.execute("SELECT COUNT(*) FROM t WHERE ts >= 8192").unwrap();
    assert_eq!(r.rows[0][0], Value::Int64(4096));
    // 完全在首块内（行级筛选，不走块级跳读的极端）
    let r = conn.execute("SELECT COUNT(*) FROM t WHERE ts < 100").unwrap();
    assert_eq!(r.rows[0][0], Value::Int64(100));
    // 所有块全跳过 → 0 行
    let r = conn.execute("SELECT COUNT(*) FROM t WHERE ts > 100000").unwrap();
    assert_eq!(r.rows[0][0], Value::Int64(0));
    // 点查等值
    let r = conn.execute("SELECT v FROM t WHERE ts = 9000").unwrap();
    assert_eq!(r.rows[0][0], Value::Float64(9000.0));
}

#[test]
fn test_log_engine_persistence() {
    let path = format!("/tmp/engramdb_log_persist_{}.hdb", std::process::id());
    std::fs::remove_file(&path).ok();
    std::fs::remove_file(format!("{}-wal", path)).ok();
    {
        let mut conn = super::Connection::open(&path).unwrap();
        conn.execute("CREATE TABLE events (ts INT64, event TEXT) ENGINE = Log").unwrap();
        for i in 0..1000 {
            conn.execute(&format!("INSERT INTO events VALUES ({}, 'e{}')", i, i)).unwrap();
        }
        conn.close().unwrap();
    }
    {
        let mut conn = super::Connection::open(&path).unwrap();
        let r = conn.execute("SELECT COUNT(*) FROM events").unwrap();
        assert_eq!(r.rows[0][0], Value::Int64(1000), "Log 表重启后数据应完整");
        let r = conn.execute("SELECT * FROM events WHERE ts >= 995").unwrap();
        assert_eq!(r.rows.len(), 5);
        let r = conn.execute("SELECT event FROM events WHERE ts = 10").unwrap();
        assert_eq!(r.rows[0][0], Value::Varchar("e10".into()));
    }
    std::fs::remove_file(&path).ok();
    std::fs::remove_file(format!("{}-wal", path)).ok();
}

#[test]
fn test_log_engine_no_update_delete() {
    let mut conn = super::Connection::open(":memory:").unwrap();
    conn.execute("CREATE TABLE t (ts INT64, v TEXT) ENGINE = Log").unwrap();
    conn.execute("INSERT INTO t VALUES (1, 'a')").unwrap();
    // M5：planner 提前拦截（信息含引擎名与操作）
    let err = conn.execute("UPDATE t SET v = 'b' WHERE ts = 1").unwrap_err();
    assert!(err.to_string().contains("不支持 UPDATE"), "got: {err}");
    let err = conn.execute("DELETE FROM t WHERE ts = 1").unwrap_err();
    assert!(err.to_string().contains("不支持 DELETE"), "got: {err}");
    // UPSERT 同样拒绝
    let err = conn.execute(
        "INSERT INTO t VALUES (1, 'b') ON CONFLICT (ts) DO UPDATE SET v = 'b'",
    ).unwrap_err();
    assert!(err.to_string().contains("LogEngine"), "got: {err}");
    // TRUNCATE 允许（DDL 语义）
    conn.execute("TRUNCATE TABLE t").unwrap();
    let r = conn.execute("SELECT COUNT(*) FROM t").unwrap();
    assert_eq!(r.rows[0][0], Value::Int64(0));
    // 清空后可继续追加
    conn.execute("INSERT INTO t VALUES (1, 'a')").unwrap();
    let r = conn.execute("SELECT COUNT(*) FROM t").unwrap();
    assert_eq!(r.rows[0][0], Value::Int64(1));
}

#[test]
fn test_log_engine_txn_append() {
    let mut conn = super::Connection::open(":memory:").unwrap();
    conn.execute("CREATE TABLE t (ts INT64, v TEXT) ENGINE = Log").unwrap();
    // Transaction API：apply 延迟到 commit；rollback 不落盘（LogTable 零物理写）
    {
        let mut tx = conn.begin().unwrap();
        tx.insert("t", vec![vec![Value::Int64(1), Value::Varchar("a".into())]]).unwrap();
        tx.rollback().unwrap();
    }
    let r = conn.execute("SELECT COUNT(*) FROM t").unwrap();
    assert_eq!(r.rows[0][0], Value::Int64(0), "rollback 后不应有行");

    {
        let mut tx = conn.begin().unwrap();
        tx.insert("t", vec![vec![Value::Int64(1), Value::Varchar("a".into())]]).unwrap();
        tx.commit().unwrap();
    }
    let r = conn.execute("SELECT COUNT(*) FROM t").unwrap();
    assert_eq!(r.rows[0][0], Value::Int64(1), "commit 后应有 1 行");

    // SQL 级 BEGIN/COMMIT（自管理事务）同样只追加
    conn.execute("BEGIN").unwrap();
    conn.execute("INSERT INTO t VALUES (2, 'b')").unwrap();
    conn.execute("COMMIT").unwrap();
    let r = conn.execute("SELECT COUNT(*) FROM t").unwrap();
    assert_eq!(r.rows[0][0], Value::Int64(2));
}
