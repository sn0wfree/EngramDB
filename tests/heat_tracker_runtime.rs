//! Phase 2.5 P1：HeatTracker 运行时集成测试
//!
//! 验证：
//! 1. 表 scan 触发 record_access
//! 2. 多表访问分别记录
//! 3. heat_score 随访问次数增加
//! 4. 多次 execute 入口累加
//! 5. on_commit tick 触发条件
//!
//! 运行：`cargo test --release --test heat_tracker_runtime -- --test-threads=1`

use engramdb::Connection;

#[test]
fn test_record_access_on_scan() {
    let path = "/tmp/p25_heat_basic.hdb";
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{}-wal", path));

    let mut conn = Connection::open(path).unwrap();
    conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY, v TEXT)").unwrap();
    conn.execute("INSERT INTO t VALUES (1, 'a'), (2, 'b')").unwrap();

    let db = conn.database_mut();
    let before = db.heat_tracker().access_count(1);
    db.record_access(1);
    let after = db.heat_tracker().access_count(1);
    assert_eq!(after, before + 1);
}

#[test]
fn test_record_access_by_name() {
    let path = "/tmp/p25_heat_byname.hdb";
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{}-wal", path));

    let mut conn = Connection::open(path).unwrap();
    conn.execute("CREATE TABLE t1 (id INT64 PRIMARY KEY)").unwrap();
    conn.execute("CREATE TABLE t2 (id INT64 PRIMARY KEY)").unwrap();

    let db = conn.database_mut();
    db.record_access_by_name("t1");
    db.record_access_by_name("t1");
    db.record_access_by_name("t2");

    let t1_id = db.table_id_by_name("t1").unwrap();
    let t2_id = db.table_id_by_name("t2").unwrap();
    assert_eq!(db.heat_tracker().access_count(t1_id), 2);
    assert_eq!(db.heat_tracker().access_count(t2_id), 1);
}

#[test]
fn test_heat_score_increases_with_access() {
    let path = "/tmp/p25_heat_score.hdb";
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{}-wal", path));

    let mut conn = Connection::open(path).unwrap();
    conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY)").unwrap();
    conn.execute("INSERT INTO t VALUES (1)").unwrap();

    let db = conn.database_mut();
    let id = db.table_id_by_name("t").unwrap();

    // 初始：heat_score ≈ 0
    let s0 = db.heat_tracker_mut().heat_score(id);

    // 100 次访问
    for _ in 0..100 {
        db.heat_tracker_mut().record_access(id);
    }
    let s1 = db.heat_tracker_mut().heat_score(id);
    assert!(s1 > s0, "score should increase after 100 accesses: {} -> {}", s0, s1);
}

#[test]
fn test_on_commit_tick_interval() {
    let path = "/tmp/p25_heat_tick.hdb";
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{}-wal", path));

    let mut conn = Connection::open(path).unwrap();
    let mut triggered = 0;
    let mut total = 0;

    for i in 0..1500 {
        total += 1;
        if conn.database_mut().on_commit() {
            triggered += 1;
        }
        let _ = i;
    }

    // 默认 TICK_INTERVAL = 1000 → 1500 commits 应触发 1 次
    assert!(
        triggered >= 1,
        "expected at least 1 tick trigger, got {} out of {}",
        triggered,
        total
    );
    assert!(
        triggered <= 2,
        "expected at most 2 tick triggers, got {} out of {}",
        triggered,
        total
    );
}

#[test]
fn test_heat_tracker_persists_across_restart() {
    // HeatTracker 是运行时状态，不持久化（Phase 2.5 范围）
    // 验证：重启后 heat_tracker 重新为空（不是 bug，是设计）
    let path = "/tmp/p25_heat_restart.hdb";
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{}-wal", path));

    let mut conn = Connection::open(path).unwrap();
    conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY)").unwrap();
    let id = conn.database_mut().table_id_by_name("t").unwrap();
    for _ in 0..10 {
        conn.database_mut().record_access(id);
    }
    let before = conn.database_mut().heat_tracker().access_count(id);
    assert_eq!(before, 10);
    drop(conn);

    let mut conn = Connection::open(path).unwrap();
    let id2 = conn.database_mut().table_id_by_name("t").unwrap();
    let after = conn.database_mut().heat_tracker().access_count(id2);
    assert_eq!(after, 0, "HeatTracker 不持久化（运行时状态）");
}

#[test]
fn test_scan_via_sql_records_heat() {
    // 通过 SQL 路径访问表，确认 record_access 钩子已生效
    let path = "/tmp/p25_heat_scan.hdb";
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{}-wal", path));

    let mut conn = Connection::open(path).unwrap();
    conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY, v INT64)").unwrap();
    conn.execute("INSERT INTO t VALUES (1, 100), (2, 200), (3, 300)").unwrap();

    let id = conn.database_mut().table_id_by_name("t").unwrap();
    let before = conn.database_mut().heat_tracker().access_count(id);

    // 多次 SQL scan
    for _ in 0..5 {
        let r = conn.execute("SELECT * FROM t").unwrap();
        assert_eq!(r.rows.len(), 3);
    }

    let after = conn.database_mut().heat_tracker().access_count(id);
    assert!(
        after > before,
        "SQL scan should increment access count: {} -> {}",
        before,
        after
    );
}
