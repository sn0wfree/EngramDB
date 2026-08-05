
#[test]
fn test_memory_engine_sql_full() {
    let mut conn = super::Connection::open(":memory:").unwrap();
    conn.execute("CREATE TABLE st (step_id INT PRIMARY KEY, state TEXT) ENGINE = Memory").unwrap();
    conn.execute("INSERT INTO st VALUES (1, 'thinking')").unwrap();
    conn.execute("INSERT INTO st VALUES (2, 'calling')").unwrap();
    conn.execute("INSERT INTO st VALUES (3, 'done')").unwrap();
    let r = conn.execute("SELECT * FROM st ORDER BY step_id").unwrap();
    assert_eq!(r.rows.len(), 3);
    let r = conn.execute("SELECT state FROM st WHERE step_id = 2").unwrap();
    assert_eq!(r.rows[0][0], Value::Varchar("calling".into()));
    let r = conn.execute("SELECT COUNT(*) FROM st WHERE step_id > 1").unwrap();
    assert_eq!(r.rows[0][0], Value::Int64(2));
    let r = conn.execute("SELECT state, COUNT(*) FROM st GROUP BY state").unwrap();
    assert_eq!(r.rows.len(), 3);
    conn.execute("UPDATE st SET state = 'running' WHERE step_id = 1").unwrap();
    let r = conn.execute("SELECT state FROM st WHERE step_id = 1").unwrap();
    assert_eq!(r.rows[0][0], Value::Varchar("running".into()));
    conn.execute("DELETE FROM st WHERE step_id = 3").unwrap();
    let r = conn.execute("SELECT COUNT(*) FROM st").unwrap();
    assert_eq!(r.rows[0][0], Value::Int64(2));
    conn.execute("TRUNCATE TABLE st").unwrap();
    let r = conn.execute("SELECT COUNT(*) FROM st").unwrap();
    assert_eq!(r.rows[0][0], Value::Int64(0));
    conn.execute("INSERT INTO st VALUES (1, 'x')").unwrap();
    let err = conn.execute("INSERT INTO st VALUES (1, 'y')").unwrap_err();
    assert!(err.to_string().contains("UNIQUE"), "got: {err}");
}

#[test]
fn test_memory_engine_restart_cleared() {
    let path = format!("/tmp/engramdb_mem_restart_{}.hdb", std::process::id());
    std::fs::remove_file(&path).ok();
    std::fs::remove_file(format!("{}-wal", path)).ok();
    {
        let mut conn = super::Connection::open(&path).unwrap();
        conn.execute("CREATE TABLE persistent (id INT PRIMARY KEY, v INT)").unwrap();
        conn.execute("CREATE TABLE transient (id INT PRIMARY KEY, v INT) ENGINE = Memory").unwrap();
        conn.execute("INSERT INTO persistent VALUES (1, 10)").unwrap();
        conn.execute("INSERT INTO transient VALUES (1, 20)").unwrap();
        conn.close().unwrap();
    }
    {
        let mut conn = super::Connection::open(&path).unwrap();
        let r = conn.execute("SELECT COUNT(*) FROM persistent").unwrap();
        assert_eq!(r.rows[0][0], Value::Int64(1), "Columnar 表应保留");
        let r = conn.execute("SELECT COUNT(*) FROM transient").unwrap();
        assert_eq!(r.rows[0][0], Value::Int64(0), "Memory 表重启后应为空");
        conn.close().unwrap();
    }
    std::fs::remove_file(&path).ok();
    std::fs::remove_file(format!("{}-wal", path)).ok();
}

#[test]
fn test_memory_engine_transaction() {
    let mut conn = super::Connection::open(":memory:").unwrap();
    conn.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT) ENGINE = Memory").unwrap();
    conn.execute("BEGIN").unwrap();
    conn.execute("INSERT INTO t VALUES (1, 100)").unwrap();
    conn.execute("INSERT INTO t VALUES (2, 200)").unwrap();
    conn.execute("COMMIT").unwrap();
    let r = conn.execute("SELECT COUNT(*) FROM t").unwrap();
    assert_eq!(r.rows[0][0], Value::Int64(2));

    // Transaction API：真正的回滚语义（txn_manager MVCC）
    {
        let mut tx = conn.begin().unwrap();
        tx.insert("t", vec![vec![Value::Int64(3), Value::Int64(300)]]).unwrap();
        tx.rollback().unwrap();
    }
    let r = conn.execute("SELECT COUNT(*) FROM t").unwrap();
    assert_eq!(r.rows[0][0], Value::Int64(2), "回滚后不应有 (3, 300)");

    // Transaction API：提交
    {
        let mut tx = conn.begin().unwrap();
        tx.insert("t", vec![vec![Value::Int64(9), Value::Int64(900)]]).unwrap();
        tx.commit().unwrap();
    }
    let r = conn.execute("SELECT COUNT(*) FROM t").unwrap();
    assert_eq!(r.rows[0][0], Value::Int64(3));

    // UPDATE/DELETE 非事务路径（Memory 引擎分派）
    conn.execute("UPDATE t SET v = 999 WHERE id = 1").unwrap();
    conn.execute("DELETE FROM t WHERE id = 2").unwrap();
    let r = conn.execute("SELECT id, v FROM t ORDER BY id").unwrap();
    assert_eq!(r.rows.len(), 2);
    assert_eq!(r.rows[0], vec![Value::Int64(1), Value::Int64(999)]);
}

#[test]
fn test_memory_engine_capability_errors() {
    let mut conn = super::Connection::open(":memory:").unwrap();
    conn.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT) ENGINE = Memory").unwrap();
    let err = conn.execute("CREATE INDEX idx_v ON t (v)").unwrap_err();
    assert!(err.to_string().contains("Table not found"), "got: {err}");
}

#[test]
fn test_memory_engine_mixed_engines() {
    // 同库混合引擎：Columnar + Memory 并存，JOIN 查询
    let mut conn = super::Connection::open(":memory:").unwrap();
    conn.execute("CREATE TABLE users (id INT PRIMARY KEY, name TEXT)").unwrap();
    conn.execute("CREATE TABLE session (uid INT, token TEXT) ENGINE = Memory").unwrap();
    conn.execute("INSERT INTO users VALUES (1, 'alice'), (2, 'bob')").unwrap();
    conn.execute("INSERT INTO session VALUES (1, 't1'), (2, 't2')").unwrap();
    let r = conn.execute("SELECT users.name, session.token FROM users JOIN session ON users.id = session.uid ORDER BY users.id").unwrap();
    assert_eq!(r.rows.len(), 2);
    assert_eq!(r.rows[0], vec![Value::Varchar("alice".into()), Value::Varchar("t1".into())]);
}
