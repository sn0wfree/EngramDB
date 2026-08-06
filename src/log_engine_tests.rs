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
    // 显式事务内 INSERT 走事务级攒批（P0-2）：未读过的写入段
    // ROLLBACK 可撤销（v0.18 前为语句级提交，ROLLBACK 无效）。
    let mut conn = super::Connection::open_with_config(":memory:", small_batcher_config()).unwrap();
    conn.execute("CREATE TABLE t (ts INT64, v TEXT) ENGINE = Log").unwrap();
    conn.execute("BEGIN").unwrap();
    for i in 0..3 {
        conn.execute(&format!("INSERT INTO t VALUES ({}, 'e{}')", i, i)).unwrap();
    }
    conn.execute("ROLLBACK").unwrap();
    let r = conn.execute("SELECT COUNT(*) FROM t").unwrap();
    assert_eq!(r.rows[0][0], Value::Int64(0), "ROLLBACK 撤销未读写入段（事务级攒批）");
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

// ---------------------------------------------------------------------------
// v0.18 P0-1 计划缓存测试
// ---------------------------------------------------------------------------

#[test]
fn test_plan_cache_same_sql() {
    // 同 SQL 重复执行：缓存命中，结果正确
    let mut conn = super::Connection::open(":memory:").unwrap();
    conn.execute("CREATE TABLE t (ts INT64, v TEXT) ENGINE = Log").unwrap();
    for i in 0..5 {
        let r = conn.execute("INSERT INTO t VALUES (1, 'a')").unwrap();
        assert_eq!(r.rows_affected, 1);
    }
    conn.sync_wal().unwrap();
    let r = conn.execute("SELECT COUNT(*) FROM t").unwrap();
    assert_eq!(r.rows[0][0], Value::Int64(5));
    conn.close().unwrap();
}

#[test]
fn test_plan_cache_invalidated_on_ddl() {
    // DDL 后缓存失效：同 SQL 重新规划（表结构已变）
    let mut conn = super::Connection::open(":memory:").unwrap();
    conn.execute("CREATE TABLE t (id INT64, v TEXT)").unwrap();
    // 缓存 INSERT + SELECT 计划
    conn.execute("INSERT INTO t VALUES (1, 'a')").unwrap();
    conn.execute("INSERT INTO t VALUES (2, 'b')").unwrap();
    // CREATE INDEX（DDL）→ 缓存清空；同 INSERT 语句走新计划仍正确
    conn.execute("CREATE INDEX idx_t_v ON t (v)").unwrap();
    conn.execute("INSERT INTO t VALUES (3, 'c')").unwrap();
    conn.sync_wal().unwrap();
    let r = conn.execute("SELECT COUNT(*) FROM t").unwrap();
    assert_eq!(r.rows[0][0], Value::Int64(3));
    // TRUNCATE 后计数正确（SELECT 计划缓存失效）
    conn.execute("TRUNCATE TABLE t").unwrap();
    conn.execute("INSERT INTO t VALUES (4, 'd')").unwrap();
    conn.sync_wal().unwrap();
    let r = conn.execute("SELECT COUNT(*) FROM t").unwrap();
    assert_eq!(r.rows[0][0], Value::Int64(1));
    conn.close().unwrap();
}

#[test]
fn test_plan_cache_countstar_not_cached() {
    // CountStar（行数快照）不缓存：插入后 COUNT 返回实时值
    let mut conn = super::Connection::open(":memory:").unwrap();
    conn.execute("CREATE TABLE t (id INT64, v TEXT)").unwrap();
    conn.execute("INSERT INTO t VALUES (1, 'a')").unwrap();
    let r = conn.execute("SELECT COUNT(*) FROM t").unwrap();
    assert_eq!(r.rows[0][0], Value::Int64(1));
    conn.execute("INSERT INTO t VALUES (2, 'b')").unwrap();
    conn.sync_wal().unwrap();
    let r = conn.execute("SELECT COUNT(*) FROM t").unwrap();
    assert_eq!(r.rows[0][0], Value::Int64(2), "CountStar 快照不得被缓存");
    conn.close().unwrap();
}

// ============================================================
// v0.18 P0-1 prepared 直通路径（免 plan 结构）测试
// ============================================================

#[test]
fn test_prepared_direct_plain_insert() {
    // 无列名裸 INSERT：直通分支（内联 eval + 直接索引），行/值正确
    let mut conn = super::Connection::open(":memory:").unwrap();
    conn.execute("CREATE TABLE t (ts INT64, v TEXT) ENGINE = Log").unwrap();
    let stmt = conn.prepare("INSERT INTO t VALUES (?, ?)").unwrap();
    let r = conn.execute_prepared(&stmt, &[Value::Int64(1), Value::Varchar("a".into())]).unwrap();
    assert_eq!(r.rows_affected, 1);
    conn.execute_prepared(&stmt, &[Value::Int64(2), Value::Varchar("b".into())]).unwrap();
    conn.sync_wal().unwrap();
    let r = conn.execute("SELECT ts, v FROM t ORDER BY ts").unwrap();
    assert_eq!(r.rows, vec![
        vec![Value::Int64(1), Value::Varchar("a".into())],
        vec![Value::Int64(2), Value::Varchar("b".into())],
    ]);
    conn.close().unwrap();
}

#[test]
fn test_prepared_direct_column_subset() {
    // 有列名（子集 + 重排）：走 eval_insert_rows 列映射 + Null 填充
    let mut conn = super::Connection::open(":memory:").unwrap();
    conn.execute("CREATE TABLE t (a INT64, b TEXT, c DOUBLE) ENGINE = Log").unwrap();
    let stmt = conn.prepare("INSERT INTO t (c, a) VALUES (?, ?)").unwrap();
    conn.execute_prepared(&stmt, &[Value::Float64(1.5), Value::Int64(7)]).unwrap();
    conn.sync_wal().unwrap();
    let r = conn.execute("SELECT a, b, c FROM t").unwrap();
    assert_eq!(r.rows, vec![vec![Value::Int64(7), Value::Null, Value::Float64(1.5)]]);
    conn.close().unwrap();
}

#[test]
fn test_prepared_direct_param_underflow() {
    // 参数个数不足：入口一次性校验报 Parse 错误
    let mut conn = super::Connection::open(":memory:").unwrap();
    conn.execute("CREATE TABLE t (ts INT64, v TEXT) ENGINE = Log").unwrap();
    let stmt = conn.prepare("INSERT INTO t VALUES (?, ?)").unwrap();
    let err = conn.execute_prepared(&stmt, &[Value::Int64(1)]).unwrap_err();
    assert!(matches!(err, crate::common::error::EngramDbError::Parse(_)));
    conn.close().unwrap();
}

#[test]
fn test_prepared_direct_fallback_returning() {
    // 非裸 INSERT（RETURNING）：跳过直通，走原计划路径行为不变
    let mut conn = super::Connection::open(":memory:").unwrap();
    conn.execute("CREATE TABLE t (ts INT64, v TEXT) ENGINE = Log").unwrap();
    let stmt = conn.prepare("INSERT INTO t VALUES (?, ?) RETURNING ts").unwrap();
    let r = conn.execute_prepared(&stmt, &[Value::Int64(42), Value::Varchar("x".into())]).unwrap();
    assert_eq!(r.rows, vec![vec![Value::Int64(42)]]);
    conn.close().unwrap();
}

#[test]
fn test_prepared_direct_batch() {
    // execute_prepared_batch 直通：批量参数逐行入批，末尾冲刷
    let mut conn = super::Connection::open(":memory:").unwrap();
    conn.execute("CREATE TABLE t (ts INT64, v TEXT) ENGINE = Log").unwrap();
    let stmt = conn.prepare("INSERT INTO t VALUES (?, ?)").unwrap();
    let batch: Vec<Vec<Value>> = (0..10)
        .map(|i| vec![Value::Int64(i as i64), Value::Varchar(format!("e{}", i))])
        .collect();
    let n = conn.execute_prepared_batch(&stmt, &batch).unwrap();
    assert_eq!(n, 10);
    conn.sync_wal().unwrap();
    let r = conn.execute("SELECT COUNT(*) FROM t").unwrap();
    assert_eq!(r.rows[0][0], Value::Int64(10));
    conn.close().unwrap();
}

// ============================================================
// v0.18 P0-1 prepared 直通路径护栏测试（锁定内联 eval 与计划路径等价性）
// ============================================================

#[test]
fn test_prepared_direct_guard_nonexpr() {
    // 分叉点 1：非平凡表达式（BinaryOp）在直通 `_` 臂与计划路径报同一错误
    let mut conn = super::Connection::open(":memory:").unwrap();
    conn.execute("CREATE TABLE t (ts INT64, v TEXT) ENGINE = Log").unwrap();
    let sql = "INSERT INTO t VALUES (1+2, ?)";
    let direct_err = conn.execute_prepared(&conn.prepare(sql).unwrap(), &[Value::Varchar("x".into())]).unwrap_err();
    let plan_err = conn.execute(sql).unwrap_err();
    assert_eq!(direct_err.to_string(), plan_err.to_string(),
        "直通 `_` 臂与 eval_constant_expr 必须报同一错误");
    conn.close().unwrap();
}

#[test]
fn test_prepared_direct_guard_table_missing() {
    // 直通不掩盖表错误：表不存在时两条路径都报 TableNotFound
    let mut conn = super::Connection::open(":memory:").unwrap();
    let sql = "INSERT INTO missing VALUES (?, ?)";
    let direct_err = conn.execute_prepared(&conn.prepare(sql).unwrap(), &[Value::Int64(1), Value::Int64(2)]).unwrap_err();
    let plan_err = conn.execute("INSERT INTO missing VALUES (1, 2)").unwrap_err();
    assert!(matches!(direct_err, crate::common::error::EngramDbError::TableNotFound(_)));
    assert!(matches!(plan_err, crate::common::error::EngramDbError::TableNotFound(_)));
    conn.close().unwrap();
}

#[test]
fn test_prepared_direct_guard_jump_placeholder() {
    // 分叉点 2：跳跃占位符 $2/$1（param_count=2），入口校验不误报且映射正确
    let mut conn = super::Connection::open(":memory:").unwrap();
    conn.execute("CREATE TABLE t (ts INT64, v TEXT) ENGINE = Log").unwrap();
    let stmt = conn.prepare("INSERT INTO t VALUES ($2, $1)").unwrap();
    conn.execute_prepared(&stmt, &[Value::Varchar("v1".into()), Value::Int64(7)]).unwrap();
    conn.sync_wal().unwrap();
    let r = conn.execute("SELECT ts, v FROM t").unwrap();
    assert_eq!(r.rows, vec![vec![Value::Int64(7), Value::Varchar("v1".into())]],
        "$2 -> params[1]，$1 -> params[0]");
    conn.close().unwrap();
}

#[test]
fn test_prepared_direct_guard_multirow_mixed() {
    // 内联循环多行 + 混合字面量/占位符：3 行全部正确
    let mut conn = super::Connection::open(":memory:").unwrap();
    conn.execute("CREATE TABLE t (ts INT64, v TEXT) ENGINE = Log").unwrap();
    let stmt = conn.prepare("INSERT INTO t VALUES (1, ?), (2, ?), (3, ?)").unwrap();
    conn.execute_prepared(&stmt, &[
        Value::Varchar("a".into()), Value::Varchar("b".into()), Value::Varchar("c".into()),
    ]).unwrap();
    conn.sync_wal().unwrap();
    let r = conn.execute("SELECT ts, v FROM t ORDER BY ts").unwrap();
    assert_eq!(r.rows, vec![
        vec![Value::Int64(1), Value::Varchar("a".into())],
        vec![Value::Int64(2), Value::Varchar("b".into())],
        vec![Value::Int64(3), Value::Varchar("c".into())],
    ]);
    conn.close().unwrap();
}

#[test]
fn test_prepared_direct_guard_column_vs_plain() {
    // 两条 eval 路径（无列名内联 / 有列名 eval_insert_rows）插入同值结果一致
    let mut conn = super::Connection::open(":memory:").unwrap();
    conn.execute("CREATE TABLE t (a INT64, b TEXT) ENGINE = Log").unwrap();
    let plain = conn.prepare("INSERT INTO t VALUES (?, ?)").unwrap();
    let columned = conn.prepare("INSERT INTO t (a, b) VALUES (?, ?)").unwrap();
    for i in 0..5 {
        conn.execute_prepared(&plain, &[Value::Int64(i as i64), Value::Varchar(format!("p{}", i))]).unwrap();
        conn.execute_prepared(&columned, &[Value::Int64(i as i64), Value::Varchar(format!("c{}", i))]).unwrap();
    }
    conn.sync_wal().unwrap();
    let r = conn.execute("SELECT a, b FROM t ORDER BY a").unwrap();
    assert_eq!(r.rows.len(), 10);
    for i in 0..5 {
        assert_eq!(r.rows[i * 2], vec![Value::Int64(i as i64), Value::Varchar(format!("p{}", i))]);
        assert_eq!(r.rows[i * 2 + 1], vec![Value::Int64(i as i64), Value::Varchar(format!("c{}", i))]);
    }
    conn.close().unwrap();
}

#[test]
fn test_prepared_direct_literal_only() {
    // 无占位符 VALUES（param_count=0）：入口校验 0<=0 通过，直通 eval 纯字面量
    let mut conn = super::Connection::open(":memory:").unwrap();
    conn.execute("CREATE TABLE t (ts INT64, v TEXT) ENGINE = Log").unwrap();
    let stmt = conn.prepare("INSERT INTO t VALUES (5, 'lit')").unwrap();
    conn.execute_prepared(&stmt, &[]).unwrap();
    conn.sync_wal().unwrap();
    let r = conn.execute("SELECT ts, v FROM t").unwrap();
    assert_eq!(r.rows, vec![vec![Value::Int64(5), Value::Varchar("lit".into())]]);
    conn.close().unwrap();
}

#[test]
fn test_prepared_direct_extra_params_ignored() {
    // 参数多于占位符：多余参数被忽略（语义与计划路径一致）
    let mut conn = super::Connection::open(":memory:").unwrap();
    conn.execute("CREATE TABLE t (ts INT64, v TEXT) ENGINE = Log").unwrap();
    let stmt = conn.prepare("INSERT INTO t VALUES (?, ?)").unwrap();
    conn.execute_prepared(&stmt, &[Value::Int64(3), Value::Varchar("ok".into()), Value::Int64(999)]).unwrap();
    conn.sync_wal().unwrap();
    let r = conn.execute("SELECT ts, v FROM t").unwrap();
    assert_eq!(r.rows, vec![vec![Value::Int64(3), Value::Varchar("ok".into())]]);
    conn.close().unwrap();
}

#[test]
fn test_prepared_direct_constraint_table() {
    // 有主键/约束的表绕过 batcher（约束即时暴露），直通路径仍正确落盘
    let mut conn = super::Connection::open(":memory:").unwrap();
    conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY, v TEXT) ENGINE = Log").unwrap();
    let stmt = conn.prepare("INSERT INTO t VALUES (?, ?)").unwrap();
    conn.execute_prepared(&stmt, &[Value::Int64(1), Value::Varchar("a".into())]).unwrap();
    conn.sync_wal().unwrap();
    let r = conn.execute("SELECT id, v FROM t").unwrap();
    assert_eq!(r.rows, vec![vec![Value::Int64(1), Value::Varchar("a".into())]]);
    conn.close().unwrap();
}

#[test]
fn test_prepared_batch_param_underflow() {
    // execute_prepared_batch：其中一行参数不足 → Parse 错误
    let mut conn = super::Connection::open(":memory:").unwrap();
    conn.execute("CREATE TABLE t (ts INT64, v TEXT) ENGINE = Log").unwrap();
    let stmt = conn.prepare("INSERT INTO t VALUES (?, ?)").unwrap();
    let batch = vec![
        vec![Value::Int64(1), Value::Varchar("a".into())],
        vec![Value::Int64(2)], // 不足
    ];
    let err = conn.execute_prepared_batch(&stmt, &batch).unwrap_err();
    assert!(matches!(err, crate::common::error::EngramDbError::Parse(_)));
    conn.close().unwrap();
}
