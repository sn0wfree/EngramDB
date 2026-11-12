//! H1：WAL 长跑不膨胀（v0.23 退出标准）
//!
//! 验证两点：
//! 1. **A3 自动 checkpoint 接线**：显式事务 commit 达到 Config 阈值
//!    （`wal_checkpoint_threshold` 字节 / `wal_checkpoint_timeout_ms` 时间）
//!    后自动触发完整 checkpoint（主存储落盘 + WAL 段滚动回收）。
//! 2. **H1 上界断言**：循环写入 + 自动 checkpoint 下，WAL 主文件 + `.seg*`
//!    段文件总大小有上界，不随写入量线性膨胀。
//!
//! 失败判别（assert 消息携带分项计量）：
//! - `seg字节` 大 → 段未回收（A2 `delete_rotated_segments` 失效）
//! - `主WAL` 大 → 段滚动未发生（A1 / 阈值触发接线失效）
//!
//! 迭代次数与磁盘预算均为用例常量，见下。

use std::path::{Path, PathBuf};

use engramdb::txn::{IsolationLevel, TransactionManager};
use engramdb::{Config, Connection, Value};
use tempfile::tempdir;

// ============================================================================
// 用例常量（迭代次数 / 磁盘预算）
// ============================================================================

/// 写入迭代次数（每次 = 1 个显式事务）
const ITERATIONS: usize = 2_000;
/// 每事务行数
const ROWS_PER_TXN: usize = 4;
/// A3 字节阈值：WAL 累计 ≥ 128KB 自动 checkpoint
const CHECKPOINT_THRESHOLD: u64 = 128 * 1024;
/// 预算余量：单事务越界 + checkpoint 记录 + 计量滞后（current_lsn→落盘）
const WAL_SLACK: u64 = 96 * 1024;
/// 每行 padding 字符数（撑大单事务 WAL 体积，减少迭代数）
const PAYLOAD_PAD: usize = 64;

/// 绝对预算 = 阈值 + 余量。任何时刻 WAL 总量不得超过此值
/// （首次 checkpoint 前会增长到阈值，之后被滚动回收）
fn wal_budget() -> u64 {
    CHECKPOINT_THRESHOLD + WAL_SLACK
}

// ============================================================================
// WAL 文件计量
// ============================================================================

#[derive(Debug, Clone, Copy)]
struct WalStats {
    /// 主 WAL 文件（`{db}-wal`）字节数
    main: u64,
    /// 已轮转段文件（`{db}-wal.seg*`）个数
    seg_count: usize,
    /// 已轮转段文件总字节数
    seg_bytes: u64,
    /// 主文件 + 段文件总字节
    total: u64,
}

fn wal_stats(db_path: &Path) -> WalStats {
    let wal_path = PathBuf::from(format!("{}-wal", db_path.to_string_lossy()));
    let mut s = WalStats {
        main: 0,
        seg_count: 0,
        seg_bytes: 0,
        total: 0,
    };
    if let Ok(m) = std::fs::metadata(&wal_path) {
        s.main = m.len();
    }
    if let (Some(dir), Some(name)) = (
        wal_path.parent(),
        wal_path.file_name().map(|n| n.to_string_lossy().into_owned()),
    ) {
        let prefix = format!("{name}.seg");
        if let Ok(rd) = std::fs::read_dir(dir) {
            for ent in rd.flatten() {
                let fname = ent.file_name().to_string_lossy().into_owned();
                if fname.starts_with(&prefix) {
                    s.seg_count += 1;
                    s.seg_bytes += ent.metadata().map(|m| m.len()).unwrap_or(0);
                }
            }
        }
    }
    s.total = s.main + s.seg_bytes;
    s
}

fn assert_wal_bounded(stats: &WalStats, ctx: &str) {
    let budget = wal_budget();
    assert!(
        stats.total <= budget,
        "{ctx}: WAL 超预算 total={} budget={}（阈值={} 余量={}）| \
         主WAL={} seg数={} seg字节={} | 判别：seg字节大=段未回收（A2 失效）；\
         主WAL大=段滚动未发生（A1/自动触发接线失效）",
        stats.total,
        budget,
        CHECKPOINT_THRESHOLD,
        WAL_SLACK,
        stats.main,
        stats.seg_count,
        stats.seg_bytes,
    );
}

// ============================================================================
// 测试 1：显式事务长跑 → 自动 checkpoint → WAL 有上界 + 恢复正确
// ============================================================================

#[test]
fn wal_growth_auto_checkpoint_bounded() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("wal_growth.hdb");

    let mut cfg = Config::default();
    cfg.wal_checkpoint_threshold = CHECKPOINT_THRESHOLD;
    cfg.wal_checkpoint_timeout_ms = 0; // 本用例只验证字节阈值

    let mut conn = Connection::open_with_config(db_path.to_str().unwrap(), cfg).unwrap();
    conn.execute("CREATE TABLE t (id INT PRIMARY KEY, pad VARCHAR)").unwrap();

    let pad = "x".repeat(PAYLOAD_PAD);
    let mut max_total: u64 = 0;
    let mut first_ckpt_seen = false;
    let mut prev_total = wal_stats(&db_path).total;

    for i in 0..ITERATIONS {
        let mut txn = conn.begin().unwrap();
        let rows: Vec<Vec<Value>> = (0..ROWS_PER_TXN)
            .map(|j| vec![Value::Int64((i * ROWS_PER_TXN + j) as i64), Value::Varchar(pad.clone())])
            .collect();
        txn.insert("t", rows).unwrap();
        // A3：commit 后达到阈值 → 自动完整 checkpoint（落盘 + 段滚动回收）
        txn.commit().unwrap();

        let st = wal_stats(&db_path);
        max_total = max_total.max(st.total);

        // 首次自动 checkpoint 证据：total 出现下降。
        // 触发与观测在同一 commit 内——达阈值瞬间 size 已被滚动重置，
        // 永远看不到 ≥ 阈值的瞬间，故以「下降」证明滚动回收发生过。
        if !first_ckpt_seen && st.total < prev_total {
            first_ckpt_seen = true;
        }
        prev_total = st.total;

        assert_wal_bounded(&st, &format!("第 {i}/{ITERATIONS} 次迭代"));
    }

    // ① 自动触发确实发生过（否则上面的上界可能只是写太少）
    assert!(
        first_ckpt_seen,
        "自动 checkpoint 从未发生（total 从未下降）：阈值接线失效（Config→WalWriter→Transaction::commit）。\
         末态 {:?}",
        wal_stats(&db_path)
    );

    // ② 相对上界：峰值远小于累计写入量（线性膨胀时峰值 ≈ 全部写入量）
    let total_written_floor = (ITERATIONS as u64) * 150; // 粗下界：每事务 ≥150B WAL（峰值预算 224KB < 此 floor）
    assert!(
        max_total < total_written_floor,
        "峰值 WAL ({max_total}) 未被压到累计写入量 ({total_written_floor}+) 之下，疑似线性膨胀"
    );
    assert_wal_bounded(&wal_stats(&db_path), "循环结束");

    // ③ 恢复正确：close（checkpoint）→ 重开 → 行数/末行完整
    conn.close().unwrap();
    let final_stats = wal_stats(&db_path);
    assert_eq!(final_stats.seg_count, 0, "close 后段应全部回收，got {final_stats:?}");
    assert_wal_bounded(&final_stats, "close 后");

    let mut conn2 = Connection::open(db_path.to_str().unwrap()).unwrap();
    let expected = (ITERATIONS * ROWS_PER_TXN) as i64;
    let r = conn2.execute("SELECT COUNT(*) FROM t").unwrap();
    assert_eq!(
        r.rows[0][0],
        Value::Int64(expected),
        "长跑 + 自动 checkpoint 后行数应完整（checkpoint 不得丢已提交数据）"
    );
    let r = conn2.execute(&format!("SELECT id FROM t WHERE id = {}", expected - 1)).unwrap();
    assert_eq!(r.rows.len(), 1, "末行应可点查（恢复正确）");
}

// ============================================================================
// 测试 2：A3 阈值语义——(A) 不达不触发 / (B) due 翻转+复位 / (C) 数据完整
//
// (B) 必须直接用 TransactionManager：Connection 路径上 due=true 会被同一
// commit 的自动 checkpoint 立即消费，循环顶部永远观测不到 true。
// ============================================================================

#[test]
fn auto_checkpoint_due_semantics() {
    // ---- (A) Connection：不达不触发（默认 16MB 阈值）----
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("due_semantics.hdb");

    let mut cfg = Config::default(); // wal_checkpoint_threshold = 16MB
    cfg.wal_checkpoint_timeout_ms = 0;
    let mut conn = Connection::open_with_config(db_path.to_str().unwrap(), cfg).unwrap();
    conn.execute("CREATE TABLE t (id INT PRIMARY KEY, pad VARCHAR)").unwrap();

    let write_txns = |conn: &mut Connection, n: usize, base: usize| {
        for k in 0..n {
            let mut txn = conn.begin().unwrap();
            txn.insert(
                "t",
                vec![vec![
                    Value::Int64((base + k) as i64),
                    Value::Varchar("y".repeat(PAYLOAD_PAD)),
                ]],
            )
            .unwrap();
            txn.commit().unwrap();
        }
    };

    write_txns(&mut conn, 20, 0);
    assert!(
        !conn.database_mut().txn_manager().auto_checkpoint_due(),
        "(A) 不达字节阈值不应触发自动 checkpoint"
    );

    // ---- (B) 直接 TransactionManager：无自动消费，测 due 翻转 + checkpoint 复位 ----
    let dir_b = tempdir().unwrap();
    let tm_path = dir_b.path().join("due_tm.hdb");
    let mut tm = TransactionManager::new(tm_path.to_str().unwrap(), &Config::default()).unwrap();

    // 不达：默认 16MB 阈值，少量事务远低于阈值
    for _ in 0..10 {
        let id = tm.begin(IsolationLevel::default()).unwrap();
        tm.commit(id).unwrap();
    }
    assert!(!tm.auto_checkpoint_due(), "(B) 不达默认阈值不应 due");

    // 调小阈值 → 写到达到 → due 翻转（无自动消费，顶部可观测 true）
    tm.set_wal_checkpoint_policy(4096, 0);
    let mut guard = 0;
    while !tm.auto_checkpoint_due() {
        let id = tm.begin(IsolationLevel::default()).unwrap();
        tm.commit(id).unwrap();
        guard += 1;
        assert!(guard < 2000, "阈值 4KB 下 2000 事务内应达到触发条件");
    }
    assert!(
        tm.auto_checkpoint_due(),
        "(B) 达到字节阈值应触发且保持 due（无自动消费）"
    );

    // 完整 checkpoint → 段滚动 → 复位为不触发
    tm.checkpoint().unwrap();
    assert!(!tm.auto_checkpoint_due(), "(B) checkpoint（段滚动）后应复位为不触发");

    // ---- (C) Connection 小阈值：自动 checkpoint 静默消费，数据完整 ----
    conn.database_mut().txn_manager_mut().set_wal_checkpoint_policy(4096, 0);
    let mut prev_total = wal_stats(&db_path).total;
    let mut ckpt_drop = false;
    for k in 0..500 {
        write_txns(&mut conn, 1, 1000 + k);
        let st = wal_stats(&db_path);
        if st.total < prev_total {
            ckpt_drop = true;
        }
        prev_total = st.total;
    }
    assert!(
        ckpt_drop,
        "(C) 小阈值下 Connection 自动 checkpoint 应发生（total 应出现下降）"
    );

    // 恢复正确：全部已提交数据在自动 checkpoint 后可读
    let r = conn.execute("SELECT COUNT(*) FROM t").unwrap();
    assert_eq!(
        r.rows[0][0],
        Value::Int64(20 + 500),
        "(C) 自动 checkpoint 不得丢已提交数据"
    );
}
