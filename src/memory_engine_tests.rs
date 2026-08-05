
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
    // M5：planner 提前清晰报错（原为执行期 "Table not found"）
    assert!(err.to_string().contains("不支持"), "got: {err}");
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

#[test]
fn test_mark_index_persistence() {
    // Mark Index（M1-7）：主键索引持久化，重启后直接恢复（免全行重建）
    let path = format!("/tmp/engramdb_markidx_{}.hdb", std::process::id());
    std::fs::remove_file(&path).ok();
    std::fs::remove_file(format!("{}-wal", path)).ok();
    {
        let mut conn = super::Connection::open(&path).unwrap();
        conn.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)").unwrap();
        let mut sql = String::from("INSERT INTO t VALUES ");
        for i in 0..5000 {
            if i > 0 { sql.push(','); }
            sql.push_str(&format!("({}, {})", i, i * 2));
        }
        conn.execute(&sql).unwrap();
        conn.close().unwrap();
    }
    {
        let mut conn = super::Connection::open(&path).unwrap();
        // 重启后主键点查命中（索引从持久化段恢复）
        let r = conn.execute("SELECT v FROM t WHERE id = 4321").unwrap();
        assert_eq!(r.rows[0][0], Value::Int64(8642));
        // 持久化段恢复的索引应为完整大小（5000 条）
        let table = conn.database_mut().get_table("t").unwrap();
        let idx_len = table.primary_index().map(|i| i.len()).unwrap_or(0);
        assert_eq!(idx_len, 5000, "主键索引应从持久化段完整恢复");
        conn.close().unwrap();
    }
    std::fs::remove_file(&path).ok();
    std::fs::remove_file(format!("{}-wal", path)).ok();
}

#[test]
fn test_mark_index_rebuild_fallback() {
    // 无持久化主键段（旧文件）→ 全量重建兜底，点查仍可用
    let path = format!("/tmp/engramdb_markidx_rb_{}.hdb", std::process::id());
    std::fs::remove_file(&path).ok();
    std::fs::remove_file(format!("{}-wal", path)).ok();
    let (idx_count, point_hit) = {
        let mut conn = super::Connection::open(&path).unwrap();
        conn.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)").unwrap();
        conn.execute("INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)").unwrap();
        // 模拟旧文件：清空内存主键索引后重新加载索引段（索引段无主键段时触发兜底重建）
        {
            let db = conn.database_mut();
            let table = db.get_table_mut("t").unwrap();
            table.clear_primary_index_for_test();
        }
        let rebuilt = conn.database_mut().load_indexes().unwrap();
        assert!(rebuilt >= 0); // load_indexes 返回索引总数（rebuild 是内部兜底）
        let r = conn.execute("SELECT v FROM t WHERE id = 2").unwrap();
        let hit = r.rows[0][0].clone();
        let idx_count = conn.database_mut().get_table("t").unwrap()
            .primary_index().map(|i| i.len()).unwrap_or(0);
        (idx_count, hit)
    };
    assert_eq!(idx_count, 3, "兜底重建应恢复全部主键");
    assert_eq!(point_hit, Value::Int64(20));
    conn_cleanup(&path);
}

fn conn_cleanup(path: &str) {
    std::fs::remove_file(path).ok();
    std::fs::remove_file(format!("{}-wal", path)).ok();
}
