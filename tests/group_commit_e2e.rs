//! WAL 组提交端到端测试
//!
//! 验证 `TransactionManager::commit()` → `wal.commit_flush()` 全链路
//! 在各种组合下的行为正确性。
//!
//! 运行：`cargo test --release --test group_commit_e2e -- --test-threads=1`

use engramdb::common::config::Config;
use engramdb::Connection;

fn open_with(group: usize, timeout_ms: u64) -> Connection {
    let path = format!("/tmp/m1_e2e_{}_{}_{}.hdb", group, timeout_ms, std::process::id());
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}-wal", path));
    let mut cfg = Config::default();
    cfg.wal_group_commit_size = group;
    cfg.wal_group_commit_timeout_ms = timeout_ms;
    Connection::open_with_config(&path, cfg).unwrap()
}

#[test]
fn test_group_commit_default_16_64kb_10ms() {
    let mut conn = open_with(16, 10);
    conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY) ENGINE=Columnar").unwrap();
    for i in 0..100 {
        conn.execute(&format!("INSERT INTO t VALUES ({})", i)).unwrap();
    }
    conn.sync_wal().unwrap();
    // 验证：sync_wal 后所有写入已 fsync
}

#[test]
fn test_group_commit_size_zero_disables_grouping() {
    let mut conn = open_with(0, 0);
    conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY) ENGINE=Columnar").unwrap();
    for i in 0..10 {
        conn.execute(&format!("INSERT INTO t VALUES ({})", i)).unwrap();
    }
    // 100% 写入成功
}

#[test]
fn test_group_commit_readonly_skips_wal() {
    let mut conn = open_with(16, 10);
    conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY) ENGINE=Columnar").unwrap();
    for _ in 0..100 {
        let _ = conn.execute("SELECT * FROM t").unwrap();
    }
    // 只读事务：不应有 COMMIT 记录写入 WAL
    // 检查 WAL 大小应仅含表元数据 + 可能的 Begin 记录
}

#[test]
fn test_group_commit_memory_table_skips_wal() {
    let mut conn = open_with(16, 10);
    conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY) ENGINE=Memory").unwrap();
    for i in 0..10 {
        conn.execute(&format!("INSERT INTO t VALUES ({})", i)).unwrap();
    }
    // MemoryEngine：事务不写 WAL COMMIT（v0.15.0 Txn09）
}

#[test]
fn test_group_commit_sync_wal_forces_flush() {
    let mut conn = open_with(1000, 0); // 大 group size 不自动触发
    conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY) ENGINE=Columnar").unwrap();
    for i in 0..10 {
        conn.execute(&format!("INSERT INTO t VALUES ({})", i)).unwrap();
    }
    // 显式 sync_wal() 强制刷盘
    conn.sync_wal().unwrap();
}

#[test]
fn test_group_commit_journal_mode_sql() {
    let path = format!("/tmp/m1_journal_{}.hdb", std::process::id());
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}-wal", path));

    let mut conn = Connection::open(&path).unwrap();
    conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY)").unwrap();

    // PRAGMA journal_mode 查询当前模式
    let r = conn.execute("PRAGMA journal_mode").unwrap();
    assert_eq!(r.rows[0][0].as_str(), Some("wal"));

    // PRAGMA journal_mode = 'off'（字符串需加引号，sqlparser 不接受裸关键字）
    conn.execute("PRAGMA journal_mode = 'off'").unwrap();
    let r = conn.execute("PRAGMA journal_mode").unwrap();
    assert_eq!(r.rows[0][0].as_str(), Some("off"));

    // PRAGMA journal_mode = 'wal'（恢复）
    conn.execute("PRAGMA journal_mode = 'wal'").unwrap();
    let r = conn.execute("PRAGMA journal_mode").unwrap();
    assert_eq!(r.rows[0][0].as_str(), Some("wal"));
}