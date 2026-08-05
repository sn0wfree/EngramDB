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

// ---------------------------------------------------------------------------
// P0-2 INSERT 攒批合并（Batcher）测试
// ---------------------------------------------------------------------------

fn small_batcher_config() -> super::Config {
    let mut cfg = super::Config::default();
    cfg.wal_batch_insert = true;
    cfg.insert_batch_rows = 16; // 小阈值便于触发
    cfg.insert_batch_bytes = 0; // 不按字节
    cfg.insert_batch_timeout_ms = 0; // 不按时间
    cfg
}

#[test]
fn test_batcher_flush_on_read() {
    // 逐行 INSERT 攒批：未达阈值时行已"接受"，SELECT 前强制落盘（读己之写）
    let mut conn = super::Connection::open_with_config(":memory:", small_batcher_config()).unwrap();
    conn.execute("CREATE TABLE t (ts INT64, v TEXT) ENGINE = Log").unwrap();
    for i in 0..5 {
        let r = conn.execute(&format!("INSERT INTO t VALUES ({}, 'e{}')", i, i)).unwrap();
        assert_eq!(r.rows_affected, 1, "攒批时 INSERT 仍返回行数");
    }
    // 未达阈值（5 < 16）：缓冲中，但 SELECT 触发 flush
    let r = conn.execute("SELECT COUNT(*) FROM t").unwrap();
    assert_eq!(r.rows[0][0], Value::Int64(5), "SELECT 前应自动冲刷攒批");
    conn.close().unwrap();
}

#[test]
fn test_batcher_threshold_flush() {
    let mut conn = super::Connection::open_with_config(":memory:", small_batcher_config()).unwrap();
    conn.execute("CREATE TABLE t (ts INT64, v TEXT) ENGINE = Log").unwrap();
    // 16 行触发一次 flush：20 行应产生 2 个事务（16 + 4）
    for i in 0..20 {
        conn.execute(&format!("INSERT INTO t VALUES ({}, 'e{}')", i, i)).unwrap();
    }
    conn.sync_wal().unwrap();
    let r = conn.execute("SELECT COUNT(*) FROM t").unwrap();
    assert_eq!(r.rows[0][0], Value::Int64(20));
    conn.close().unwrap();
}

#[test]
fn test_batcher_skips_constraint_tables() {
    // 有约束表绕过 batcher：NOT NULL 错误必须在该语句返回时暴露
    let mut conn = super::Connection::open_with_config(":memory:", small_batcher_config()).unwrap();
    conn.execute("CREATE TABLE t (id INT NOT NULL, v TEXT)").unwrap();
    let err = conn.execute("INSERT INTO t VALUES (NULL, 'x')").unwrap_err();
    assert!(err.to_string().contains("NOT NULL"), "got: {err}");
    // UNIQUE 表同样即时报错
    conn.execute("CREATE TABLE u (id INT UNIQUE)").unwrap();
    conn.execute("INSERT INTO u VALUES (1)").unwrap();
    let err = conn.execute("INSERT INTO u VALUES (1)").unwrap_err();
    assert!(err.to_string().contains("UNIQUE"), "got: {err}");
}

#[test]
fn test_batcher_skips_explicit_txn() {
    // 显式事务内 INSERT 绕过 batcher：保持原语义
    // （SQL 级 BEGIN 内 INSERT 为语句级提交：ROLLBACK 不撤回已落盘行；
    //   batcher 不得引入额外缓冲窗口，行为与关闭 batcher 时一致）
    let mut conn = super::Connection::open_with_config(":memory:", small_batcher_config()).unwrap();
    conn.execute("CREATE TABLE t (ts INT64, v TEXT) ENGINE = Log").unwrap();
    conn.execute("BEGIN").unwrap();
    for i in 0..3 {
        conn.execute(&format!("INSERT INTO t VALUES ({}, 'e{}')", i, i)).unwrap();
    }
    conn.execute("ROLLBACK").unwrap();
    let r = conn.execute("SELECT COUNT(*) FROM t").unwrap();
    assert_eq!(r.rows[0][0], Value::Int64(3), "事务内行语句级落盘（与关闭 batcher 一致）");
    conn.close().unwrap();
}

#[test]
fn test_batcher_close_flushes() {
    let db_path = format!("/tmp/engramdb_batcher_close_{}.hdb", std::process::id());
    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(format!("{}-wal", db_path));
    {
        let mut conn = super::Connection::open_with_config(&db_path, small_batcher_config()).unwrap();
        conn.execute("CREATE TABLE t (ts INT64, v TEXT) ENGINE = Log").unwrap();
        for i in 0..5 {
            conn.execute(&format!("INSERT INTO t VALUES ({}, 'e{}')", i, i)).unwrap();
        }
        // 不 flush 直接 close：close 兜底冲刷
    }
    let mut conn = super::Connection::open(&db_path).unwrap();
    let r = conn.execute("SELECT COUNT(*) FROM t").unwrap();
    assert_eq!(r.rows[0][0], Value::Int64(5), "close 应冲刷攒批");
    conn.close().unwrap();
    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(format!("{}-wal", db_path));
}

// ---------------------------------------------------------------------------
// P1-5 LogEngine 块行数可配置测试
// ---------------------------------------------------------------------------

#[test]
fn test_log_block_rows_configurable() {
    // 小块的持久化往返：切分、MinMax、typed 读取全路径
    let db_path = format!("/tmp/engramdb_blockrows_{}.hdb", std::process::id());
    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(format!("{}-wal", db_path));
    {
        let mut cfg = super::Config::default();
        cfg.log_block_rows = 4; // 4 行/块
        let mut conn = super::Connection::open_with_config(&db_path, cfg).unwrap();
        conn.execute("CREATE TABLE t (ts INT64, v TEXT) ENGINE = Log").unwrap();
        for i in 0..20 {
            conn.execute(&format!("INSERT INTO t VALUES ({}, 'e{}')", i, i)).unwrap();
        }
        conn.sync_wal().unwrap();
        conn.close().unwrap();
    }
    {
        let mut conn = super::Connection::open(&db_path).unwrap();
        // 20 行 → 5 个块（4 行/块）；范围查询跨块 MinMax 跳读仍正确
        let r = conn.execute("SELECT COUNT(*) FROM t").unwrap();
        assert_eq!(r.rows[0][0], Value::Int64(20));
        let r = conn.execute("SELECT COUNT(*) FROM t WHERE ts >= 18").unwrap();
        assert_eq!(r.rows[0][0], Value::Int64(2));
        // 点查（typed 读取路径）
        let table = conn.database_mut().get_engine_table_mut("t").unwrap();
        assert_eq!(table.def().row_count, 20);
        conn.close().unwrap();
    }
    let _ = std::fs::remove_file(&db_path);
    let _ = std::fs::remove_file(format!("{}-wal", db_path));

}
