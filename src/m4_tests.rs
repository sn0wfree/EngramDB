// M4 跨引擎事务 + 统一 WAL（P4）集成测试：
// - WAL 崩溃恢复真实重放（Columnar / Log 表）
// - Memory 表不写 WAL、恢复后为空
// - 跨引擎同事务提交/回滚
// - 新格式记录头（engine_type）往返 + 旧 19B 格式兼容读取

use super::{Connection, Value};

fn tmp_db(name: &str) -> String {
    format!("/tmp/engramdb_m4_{}_{}.hdb", name, std::process::id())
}

fn cleanup(path: &str) {
    std::fs::remove_file(path).ok();
    std::fs::remove_file(format!("{}-wal", path)).ok();
}

/// 模拟崩溃：写入后不 close/不 checkpoint 直接泄漏连接
/// （commit 已 fsync WAL；主文件未 checkpoint = 崩溃现场）
fn simulate_crash(conn: Connection) {
    std::mem::forget(conn);
}

#[test]
fn test_m4_redo_recovery_columnar() {
    let path = tmp_db("redo_col");
    cleanup(&path);
    {
        let mut conn = Connection::open(&path).unwrap();
        conn.execute("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)").unwrap();
        conn.execute("INSERT INTO t VALUES (1, 'a')").unwrap();
        conn.execute("INSERT INTO t VALUES (2, 'b')").unwrap();
        conn.execute("UPDATE t SET v = 'A' WHERE id = 1").unwrap();
        // 崩溃：不 checkpoint
        simulate_crash(conn);
    }
    // 重新打开 → WAL 重放已提交事务
    let mut conn = Connection::open(&path).unwrap();
    let r = conn.execute("SELECT id, v FROM t ORDER BY id").unwrap();
    assert_eq!(r.rows.len(), 2, "重放后应有 2 行");
    assert_eq!(r.rows[0], vec![Value::Int64(1), Value::Varchar("A".into())], "UPDATE 应重放");
    assert_eq!(r.rows[1], vec![Value::Int64(2), Value::Varchar("b".into())]);
    conn.close().unwrap();
    cleanup(&path);
}

#[test]
fn test_m4_redo_recovery_log_engine() {
    let path = tmp_db("redo_log");
    cleanup(&path);
    {
        let mut conn = Connection::open(&path).unwrap();
        conn.execute("CREATE TABLE events (ts INT64, e TEXT) ENGINE = Log").unwrap();
        for i in 0..100 {
            conn.execute(&format!("INSERT INTO events VALUES ({}, 'e{}')", i, i)).unwrap();
        }
        simulate_crash(conn);
    }
    let mut conn = Connection::open(&path).unwrap();
    let r = conn.execute("SELECT COUNT(*) FROM events").unwrap();
    assert_eq!(r.rows[0][0], Value::Int64(100), "Log 表 WAL 重放恢复");
    // 时间范围查询仍正常（恢复后 MinMax 可用）
    let r = conn.execute("SELECT COUNT(*) FROM events WHERE ts >= 95").unwrap();
    assert_eq!(r.rows[0][0], Value::Int64(5));
    conn.close().unwrap();
    cleanup(&path);
}

#[test]
fn test_m4_redo_skips_memory_engine() {
    let path = tmp_db("redo_mem");
    cleanup(&path);
    {
        let mut conn = Connection::open(&path).unwrap();
        conn.execute("CREATE TABLE col (id INT, v TEXT)").unwrap();
        conn.execute("CREATE TABLE mem (id INT, v TEXT) ENGINE = Memory").unwrap();
        conn.execute("INSERT INTO col VALUES (1, 'persist')").unwrap();
        conn.execute("INSERT INTO mem VALUES (1, 'transient')").unwrap();
        simulate_crash(conn);
    }
    let mut conn = Connection::open(&path).unwrap();
    // Columnar 表恢复；Memory 表无 WAL 记录 → 为空（符合语义）
    let r = conn.execute("SELECT v FROM col WHERE id = 1").unwrap();
    assert_eq!(r.rows[0][0], Value::Varchar("persist".into()));
    let r = conn.execute("SELECT COUNT(*) FROM mem").unwrap();
    assert_eq!(r.rows[0][0], Value::Int64(0), "Memory 表恢复后应为空");
    conn.close().unwrap();
    cleanup(&path);
}

#[test]
fn test_m4_cross_engine_txn() {
    let path = tmp_db("cross_engine");
    cleanup(&path);
    let mut conn = Connection::open(&path).unwrap();
    conn.execute("CREATE TABLE col (id INT PRIMARY KEY, v TEXT)").unwrap();
    conn.execute("CREATE TABLE mem (id INT PRIMARY KEY, v TEXT) ENGINE = Memory").unwrap();

    // 跨引擎同事务提交（SQL 级 BEGIN/COMMIT）
    conn.execute("BEGIN").unwrap();
    conn.execute("INSERT INTO col VALUES (1, 'c1')").unwrap();
    conn.execute("INSERT INTO mem VALUES (1, 'm1')").unwrap();
    conn.execute("COMMIT").unwrap();
    let r = conn.execute("SELECT COUNT(*) FROM col").unwrap();
    assert_eq!(r.rows[0][0], Value::Int64(1));
    let r = conn.execute("SELECT COUNT(*) FROM mem").unwrap();
    assert_eq!(r.rows[0][0], Value::Int64(1));

    // 跨引擎同事务回滚（Transaction API 真回滚）
    {
        let mut tx = conn.begin().unwrap();
        tx.insert("col", vec![vec![Value::Int64(2), Value::Varchar("c2".into())]]).unwrap();
        tx.insert("mem", vec![vec![Value::Int64(2), Value::Varchar("m2".into())]]).unwrap();
        tx.rollback().unwrap();
    }
    let r = conn.execute("SELECT COUNT(*) FROM col").unwrap();
    assert_eq!(r.rows[0][0], Value::Int64(1), "回滚后 col 无新行");
    let r = conn.execute("SELECT COUNT(*) FROM mem").unwrap();
    assert_eq!(r.rows[0][0], Value::Int64(1), "回滚后 mem 无新行");
    conn.close().unwrap();
    cleanup(&path);
}

#[test]
fn test_m4_cross_engine_txn_crash_recovery() {
    let path = tmp_db("cross_crash");
    cleanup(&path);
    {
        let mut conn = Connection::open(&path).unwrap();
        conn.execute("CREATE TABLE col (id INT PRIMARY KEY, v TEXT)").unwrap();
        conn.execute("CREATE TABLE mem (id INT PRIMARY KEY, v TEXT) ENGINE = Memory").unwrap();
        conn.execute("BEGIN").unwrap();
        conn.execute("INSERT INTO col VALUES (1, 'c1')").unwrap();
        conn.execute("INSERT INTO mem VALUES (1, 'm1')").unwrap();
        conn.execute("COMMIT").unwrap();
        simulate_crash(conn);
    }
    let mut conn = Connection::open(&path).unwrap();
    let r = conn.execute("SELECT COUNT(*) FROM col").unwrap();
    assert_eq!(r.rows[0][0], Value::Int64(1), "崩溃后 col 事务恢复");
    let r = conn.execute("SELECT COUNT(*) FROM mem").unwrap();
    assert_eq!(r.rows[0][0], Value::Int64(0), "崩溃后 mem 为空（无 WAL）");
    conn.close().unwrap();
    cleanup(&path);
}

#[test]
fn test_m4_wal_record_engine_roundtrip() {
    use crate::wal::{WalRecord, WalRecordType};
    use crate::common::types::EngineType;

    // 三种引擎的记录头往返（engine 字节写读一致）
    for engine in [EngineType::Columnar, EngineType::Memory, EngineType::Log] {
        let rec = WalRecord {
            lsn: 0,
            record_type: WalRecordType::Insert,
            txn_id: 7,
            table_id: 3,
            engine_type: engine,
            payload: vec![1, 2, 3],
        };
        let bytes = rec.to_bytes();
        let parsed = WalRecord::from_bytes(&bytes).unwrap();
        assert_eq!(parsed.engine_type, engine);
    }
}

#[test]
fn test_m4_wal_old_format_compat() {
    use crate::wal::{WalRecord, WalRecordType};
    use crate::common::types::EngineType;

    // 手工构造 v0.17.0 前旧格式（19B 头）：magic2 + type1 + txn4 + table4 + len4 + payload + crc4
    let payload: Vec<u8> = vec![10, 20, 30];
    let mut buf = Vec::new();
    buf.extend_from_slice(&0x5741u16.to_le_bytes());
    buf.push(WalRecordType::Insert as u8);
    buf.extend_from_slice(&42u32.to_le_bytes());
    buf.extend_from_slice(&1u32.to_le_bytes());
    buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    buf.extend_from_slice(&payload);
    let crc = crate::wal::crc32(&buf);
    buf.extend_from_slice(&crc.to_le_bytes());
    assert_eq!(buf.len(), 19 + 3);

    let parsed = WalRecord::from_bytes(&buf).unwrap();
    assert_eq!(parsed.record_type, WalRecordType::Insert);
    assert_eq!(parsed.txn_id, 42);
    assert_eq!(parsed.table_id, 1);
    assert_eq!(parsed.payload, payload);
    assert_eq!(parsed.engine_type, EngineType::Columnar, "旧格式回退 Columnar");
}
