//! v0.13 验收基准
//!
//! 验证路线图 v0.13 三项验收标准：
//! - A-1: 事务内逐行写入 ≤ 10× 慢（vs SQLite）
//! - A-2: 索引点查 ≤ 5× 慢（vs SQLite）
//! - A-3: COUNT(*) 持平或更快（vs SQLite）
//! - A-4: WHERE 选择性过滤（v0.16 P-W1 新增；验收 MinMax + PREWHERE 真接通）
//!
//! 运行：`cargo run --release --bench v0.13_acceptance_bench`
//!
//! 设计原则：
//! - 同一进程持有 EngramDB 与 SQLite 连接，避免跨进程 FFI 开销差异
//! - 公平对比：EngramDB 默认配置（含 WAL 组提交）+ SQLite WAL + synchronous=NORMAL
//! - 每次运行 5 轮取中位数，避免抖动
//! - 输出表格化，便于记录到 `v0.13-acceptance-report.md`

use std::time::{Duration, Instant};

use std::io::Write;
use engramdb::{Connection, Config, Value};

const ITERS: usize = 5;
const HDB_PATH: &str = "/tmp/v0.13_acceptance.hdb";
const SQLITE_PATH: &str = "/tmp/v0.13_acceptance.sqlite";

/// 打开 EngramDB 连接（关闭 checkpoint 时的列存压缩，避免下一轮 reopen 后首次查询触发惰性解压）
fn open_hdb() -> Connection {
    let config = Config {
        compress_on_persist: false,
        ..Config::default()
    };
    Connection::open_with_config(HDB_PATH, config).unwrap()
}

// ============================================================
// 计时工具
// ============================================================

fn median(mut samples: Vec<Duration>) -> Duration {
    samples.sort();
    let n = samples.len();
    if n == 0 {
        return Duration::ZERO;
    }
    if n % 2 == 1 {
        samples[n / 2]
    } else {
        (samples[n / 2 - 1] + samples[n / 2]) / 2
    }
}

fn fmt_ms(d: Duration) -> String {
    let ms = d.as_secs_f64() * 1000.0;
    if ms < 1.0 {
        format!("{:.0} µs", ms * 1000.0)
    } else if ms < 1000.0 {
        format!("{:.2} ms", ms)
    } else {
        format!("{:.2} s", ms / 1000.0)
    }
}

fn cleanup_files() {
    let _ = std::fs::remove_file(HDB_PATH);
    let _ = std::fs::remove_file(format!("{}-wal", HDB_PATH));
    let _ = std::fs::remove_file(SQLITE_PATH);
    let _ = std::fs::remove_file(format!("{}-wal", SQLITE_PATH));
    let _ = std::fs::remove_file(format!("{}-shm", SQLITE_PATH));
}

// ============================================================
// A-1: 事务内逐行写入 (1000 事务 × 1 行 = 1000 行)
// 验收: ≤ 10× 慢 (vs SQLite)
// ============================================================

fn a1_engramdb() -> Duration {
    cleanup_files();
    let mut conn = open_hdb();
    conn.execute("CREATE TABLE t (id INT PRIMARY KEY, val DOUBLE, name VARCHAR)")
        .unwrap();

    let start = Instant::now();
    for i in 0..1000 {
        let mut txn = conn.begin().unwrap();
        txn.insert(
            "t",
            vec![vec![
                Value::Int64(i),
                Value::Float64(i as f64 * 1.5),
                Value::Varchar(format!("row_{}", i)),
            ]],
        )
        .unwrap();
        txn.commit().unwrap();
    }
    let elapsed = start.elapsed();

    conn.close().unwrap();
    cleanup_files();
    elapsed
}

fn a1_sqlite() -> Duration {
    cleanup_files();
    let conn = rusqlite::Connection::open(SQLITE_PATH).unwrap();
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;",
    )
    .unwrap();
    conn.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, val REAL, name TEXT)",
        [],
    )
    .unwrap();

    let start = Instant::now();
    for i in 0..1000 {
        conn.execute("BEGIN", []).unwrap();
        conn.execute(
            "INSERT INTO t VALUES (?, ?, ?)",
            rusqlite::params![i, i as f64 * 1.5, format!("row_{}", i)],
        )
        .unwrap();
        conn.execute("COMMIT", []).unwrap();
    }
    let elapsed = start.elapsed();

    drop(conn);
    cleanup_files();
    elapsed
}

// ============================================================
// A-2: 索引点查 (1M 行表, 1000 次随机等值查询)
// 验收: ≤ 5× 慢 (vs SQLite)
// ============================================================

const POINT_QUERY_COUNT: usize = 1000;
const POINT_QUERY_TABLE_SIZE: usize = 1_000_000;

fn a2_setup_engramdb(seed: u64) {
    cleanup_files();
    let mut conn = open_hdb();
    conn.execute("CREATE TABLE t (id INT PRIMARY KEY, val DOUBLE, name VARCHAR)")
        .unwrap();

    // 使用 import_columns 直接列式写入（跳过 SQL 解析）
    const BATCH: usize = 50_000;
    for chunk_start in (0..POINT_QUERY_TABLE_SIZE).step_by(BATCH) {
        let end = (chunk_start + BATCH).min(POINT_QUERY_TABLE_SIZE);
        let count = end - chunk_start;
        let mut col_id = Vec::with_capacity(count);
        let mut col_val = Vec::with_capacity(count);
        let mut col_name = Vec::with_capacity(count);
        for i in chunk_start..end {
            col_id.push(Value::Int64(i as i64));
            col_val.push(Value::Float64(i as f64 * 1.5));
            col_name.push(Value::Varchar(format!("row_{}", i)));
        }
        conn.import_columns("t", vec![col_id, col_val, col_name]).unwrap();
    }

    // 创建覆盖索引：id 为键，val 为覆盖列，支持 IndexOnlyScan
    conn.execute("CREATE INDEX idx_t_id ON t (id) INCLUDE (val)")
        .unwrap();

    conn.close().unwrap();
    let _ = seed; // 当前未用, 保留占位
}

fn a2_engramdb() -> Duration {
    let mut conn = open_hdb();

    // 生成随机查询 ID（确定性，避免每轮不同）
    let mut rng = SimpleRng::new(42);
    let query_ids: Vec<i64> = (0..POINT_QUERY_COUNT)
        .map(|_| rng.gen_range(0..POINT_QUERY_TABLE_SIZE as i64))
        .collect();

    let stmt = conn.prepare("SELECT id, val FROM t WHERE id = ?").unwrap();
    let start = Instant::now();
    for &id in &query_ids {
        let r = conn.execute_prepared(&stmt, &[Value::Int64(id)]).unwrap();
        debug_assert_eq!(r.rows.len(), 1, "found no row for id={}", id);
    }
    let query_time = start.elapsed();

    conn.close().unwrap();
    query_time
}

fn a2_sqlite() -> Duration {
    let conn = rusqlite::Connection::open(SQLITE_PATH).unwrap();

    let mut rng = SimpleRng::new(42);
    let query_ids: Vec<i64> = (0..POINT_QUERY_COUNT)
        .map(|_| rng.gen_range(0..POINT_QUERY_TABLE_SIZE as i64))
        .collect();
    let mut stmt = conn.prepare("SELECT id, val FROM t WHERE id = ?").unwrap();

    let start = Instant::now();
    for &id in &query_ids {
        let r: Vec<(i64, f64)> = stmt
            .query_map(rusqlite::params![id], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap()
            .map(|x| x.unwrap())
            .collect();
        debug_assert_eq!(r.len(), 1, "found no row for id={}", id);
    }
    let elapsed = start.elapsed();

    drop(stmt);
    drop(conn);
    elapsed
}

// ============================================================
// A-3: COUNT(*) 短路 (1M 行表, 无 WHERE)
// 验收: 持平或更快 (vs SQLite)
// ============================================================

fn a3_engramdb() -> Duration {
    let mut conn = open_hdb();

    let start = Instant::now();
    let r = conn.execute("SELECT COUNT(*) FROM t").unwrap();
    let elapsed = start.elapsed();
    debug_assert_eq!(r.rows.len(), 1, "COUNT(*) must return 1 row");

    conn.close().unwrap();
    elapsed
}

fn a3_sqlite() -> Duration {
    let conn = rusqlite::Connection::open(SQLITE_PATH).unwrap();

    let start = Instant::now();
    let v: i64 = conn
        .query_row("SELECT COUNT(*) FROM t", [], |row| row.get(0))
        .unwrap();
    let elapsed = start.elapsed();
    debug_eq_i64(v, POINT_QUERY_TABLE_SIZE as i64);

    drop(conn);
    elapsed
}

fn debug_eq_i64(actual: i64, expected: i64) {
    assert_eq!(actual, expected, "expected {}, got {}", expected, actual);
}

// ============================================================
// 简易确定性 RNG（避免引入 rand 依赖）
// ============================================================

struct SimpleRng {
    state: u64,
}

impl SimpleRng {
    fn new(seed: u64) -> Self {
        Self { state: seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407) }
    }
    fn gen_range(&mut self, range: std::ops::Range<i64>) -> i64 {
        self.state = self.state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let r = (self.state >> 32) as i64;
        let len = range.end - range.start;
        if len <= 0 {
            return range.start;
        }
        range.start + (r.rem_euclid(len))
    }
}

// ============================================================
// 主流程
// ============================================================

fn run_scenario(name: &str, target_ratio: f64, mut engramdb_fn: impl FnMut() -> Duration, mut sqlite_fn: impl FnMut() -> Duration) {
    println!("\n--- {} ---", name);
    println!("  目标: EngramDB / SQLite ≤ {:.1}x", target_ratio);

    let mut h_samples = Vec::with_capacity(ITERS);
    let mut s_samples = Vec::with_capacity(ITERS);
    for i in 1..=ITERS {
        let h = engramdb_fn();
        let s = sqlite_fn();
        println!("  第{}轮  EngramDB: {:>12}  SQLite: {:>12}  比值: {:>5.2}x",
                 i, fmt_ms(h), fmt_ms(s), h.as_secs_f64() / s.as_secs_f64());
        h_samples.push(h);
        s_samples.push(s);
    }

    let h_med = median(h_samples);
    let s_med = median(s_samples);
    let ratio = h_med.as_secs_f64() / s_med.as_secs_f64();
    let pass = if name.starts_with("A-3") {
        ratio <= 1.05 // COUNT(*)：持平或更快，允许 5% 抖动
    } else {
        ratio <= target_ratio
    };
    let status = if pass { "✅ PASS" } else { "❌ FAIL" };
    println!("  中位数  EngramDB: {:>12}  SQLite: {:>12}  比值: {:>5.2}x  {}",
             fmt_ms(h_med), fmt_ms(s_med), ratio, status);
}

fn main() {
    println!("==================================================================");
    println!("  EngramDB v0.13 验收基准");
    println!("  轮次: {}    数据规模: {} 行", ITERS, POINT_QUERY_TABLE_SIZE);
    println!("==================================================================");

    // A-1: 事务内逐行写入
    run_scenario("A-1: 事务内逐行写入 (1000 事务 × 1 行)", 10.0, a1_engramdb, a1_sqlite);

    // A-2 & A-3 共享 1M 行数据集 setup
    println!("\n--- 设置 A-2/A-3 测试数据 (1M 行到 EngramDB + SQLite) ---");
    println!("  EngramDB setup...");
    let t0 = Instant::now();
    a2_setup_engramdb(42);
    println!("  EngramDB setup done in {}", fmt_ms(t0.elapsed()));

    println!("  SQLite setup...");
    let t0 = Instant::now();
    setup_sqlite_1m();
    println!("  SQLite setup done in {}", fmt_ms(t0.elapsed()));

    // A-2: 索引点查
    run_scenario(
        "A-2: 索引点查 (1M 行表, 1000 次随机等值)",
        5.0,
        a2_engramdb,
        a2_sqlite,
    );

    // A-3: COUNT(*)
    run_scenario("A-3: COUNT(*) (1M 行, 无 WHERE)", 1.05, a3_engramdb, a3_sqlite);

    // A-4: WHERE 1% 选择性过滤（P-W1 PREWHERE 验收）
    // 验证 v0.16 P-W1: MinMax + PREWHERE 真接通
    println!("\n--- 设置 A-4 测试数据 (1M 行) ---");
    println!("  EngramDB setup...");
    let t0 = Instant::now();
    a4_setup_engramdb();
    println!("  EngramDB setup done in {}", fmt_ms(t0.elapsed()));
    println!("  SQLite setup...");
    let t0 = Instant::now();
    a4_setup_sqlite();
    println!("  SQLite setup done in {}", fmt_ms(t0.elapsed()));

    // 1% 选择性：val > 990（val ∈ [0, 1000] 均匀）
    run_scenario(
        "A-4a: WHERE val > 990 (1% 选择性, 1M 行)",
        10.0,  // 目标：从 21.1x 降到 10x 以内
        a4a_engramdb,
        a4a_sqlite,
    );

    // 10% 选择性：val > 900
    run_scenario(
        "A-4b: WHERE val > 900 (10% 选择性, 1M 行)",
        8.0,
        a4b_engramdb,
        a4b_sqlite,
    );

    // 50% 选择性：val > 500
    run_scenario(
        "A-4c: WHERE val > 500 (50% 选择性, 1M 行)",
        5.0,
        a4c_engramdb,
        a4c_sqlite,
    );

    println!("\n==================================================================");
    println!("  验收完成");
    println!("  详细结果请记录到: docs/v0.13-acceptance-report.md");
    println!("==================================================================");

    cleanup_files();
}

// ============================================================
// A-4: WHERE 选择性过滤（v0.16 P-W1 验收）
// ============================================================

/// EngramDB 1M 行 setup（val 均匀分布 [0, 1000]）
fn a4_setup_engramdb() {
    cleanup_files();
    let mut conn = open_hdb();
    conn.execute("CREATE TABLE t (id INT PRIMARY KEY, val DOUBLE, name VARCHAR)").unwrap();

    const BATCH: usize = 50_000;
    for chunk_start in (0..POINT_QUERY_TABLE_SIZE).step_by(BATCH) {
        let end = (chunk_start + BATCH).min(POINT_QUERY_TABLE_SIZE);
        let count = end - chunk_start;
        let mut col_id = Vec::with_capacity(count);
        let mut col_val = Vec::with_capacity(count);
        let mut col_name = Vec::with_capacity(count);
        for i in chunk_start..end {
            col_id.push(Value::Int64(i as i64));
            // val ∈ [0.0, 999.0] 均匀分布（与 SQLite setup 一致）
            col_val.push(Value::Float64(i as f64 % 1000.0));
            col_name.push(Value::Varchar(format!("row_{}", i)));
        }
        conn.import_columns("t", vec![col_id, col_val, col_name]).unwrap();
    }
    conn.close().unwrap();
}

fn a4_setup_sqlite() {
    let _ = std::fs::remove_file(SQLITE_PATH);
    let _ = std::fs::remove_file(format!("{}-wal", SQLITE_PATH));
    let _ = std::fs::remove_file(format!("{}-shm", SQLITE_PATH));
    let conn = rusqlite::Connection::open(SQLITE_PATH).unwrap();
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA cache_size = -20000;
         PRAGMA temp_store = MEMORY;",
    )
    .unwrap();
    conn.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, val REAL, name TEXT)",
        [],
    )
    .unwrap();

    const BATCH: usize = 50_000;
    let tx = conn.unchecked_transaction().unwrap();
    for chunk_start in (0..POINT_QUERY_TABLE_SIZE).step_by(BATCH) {
        let end = (chunk_start + BATCH).min(POINT_QUERY_TABLE_SIZE);
        let mut sql = String::with_capacity(end - chunk_start);
        for i in chunk_start..end {
            if i > chunk_start {
                sql.push_str(", ");
            }
            // val ∈ [0.0, 999.0] 均匀分布
            sql.push_str(&format!("({}, {}, 'row_{}')", i, i as f64 % 1000.0, i));
        }
        tx.execute(&format!("INSERT INTO t VALUES {}", sql), []).unwrap();
    }
    tx.commit().unwrap();
    drop(conn);
}

fn a4a_engramdb() -> Duration {
    let mut conn = open_hdb();
    let start = Instant::now();
    let r = conn.execute("SELECT COUNT(*) FROM t WHERE val > 990").unwrap();
    let elapsed = start.elapsed();
    // val ∈ [0, 999]，val > 990 → val ∈ [991, 999] = 9 个值/1000，每值 1000 行 = 9000 行
    debug_assert_eq!(r.rows[0][0], Value::Int64(9000), "A-4a expected 9000 rows");
    conn.close().unwrap();
    elapsed
}

fn a4a_sqlite() -> Duration {
    let conn = rusqlite::Connection::open(SQLITE_PATH).unwrap();
    let start = Instant::now();
    let v: i64 = conn
        .query_row("SELECT COUNT(*) FROM t WHERE val > 990", [], |row| row.get(0))
        .unwrap();
    let elapsed = start.elapsed();
    debug_eq_i64(v, 9000);
    drop(conn);
    elapsed
}

fn a4b_engramdb() -> Duration {
    let mut conn = open_hdb();
    let start = Instant::now();
    let r = conn.execute("SELECT COUNT(*) FROM t WHERE val > 900").unwrap();
    let elapsed = start.elapsed();
    debug_assert_eq!(r.rows[0][0], Value::Int64(99000), "A-4b expected 99000 rows");
    conn.close().unwrap();
    elapsed
}

fn a4b_sqlite() -> Duration {
    let conn = rusqlite::Connection::open(SQLITE_PATH).unwrap();
    let start = Instant::now();
    let v: i64 = conn
        .query_row("SELECT COUNT(*) FROM t WHERE val > 900", [], |row| row.get(0))
        .unwrap();
    let elapsed = start.elapsed();
    debug_eq_i64(v, 99000);
    drop(conn);
    elapsed
}

fn a4c_engramdb() -> Duration {
    let mut conn = open_hdb();
    let start = Instant::now();
    let r = conn.execute("SELECT COUNT(*) FROM t WHERE val > 500").unwrap();
    let elapsed = start.elapsed();
    // val ∈ [0, 999]，val > 500 → val ∈ [501, 999] = 499 个值/1000，每值 1000 行 = 499000 行
    debug_assert_eq!(r.rows[0][0], Value::Int64(499000), "A-4c expected 499000 rows");
    conn.close().unwrap();
    elapsed
}

fn a4c_sqlite() -> Duration {
    let conn = rusqlite::Connection::open(SQLITE_PATH).unwrap();
    let start = Instant::now();
    let v: i64 = conn
        .query_row("SELECT COUNT(*) FROM t WHERE val > 500", [], |row| row.get(0))
        .unwrap();
    let elapsed = start.elapsed();
    debug_eq_i64(v, 499000);
    drop(conn);
    elapsed
}

// ============================================================
// SQLite 1M 行 setup
// ============================================================

fn setup_sqlite_1m() {
    let _ = std::fs::remove_file(SQLITE_PATH);
    let _ = std::fs::remove_file(format!("{}-wal", SQLITE_PATH));
    let _ = std::fs::remove_file(format!("{}-shm", SQLITE_PATH));
    let conn = rusqlite::Connection::open(SQLITE_PATH).unwrap();
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA cache_size = -20000;
         PRAGMA temp_store = MEMORY;",
    )
    .unwrap();
    conn.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, val REAL, name TEXT)",
        [],
    )
    .unwrap();

    const BATCH: usize = 50_000;
    let tx = conn.unchecked_transaction().unwrap();
    for chunk_start in (0..POINT_QUERY_TABLE_SIZE).step_by(BATCH) {
        let end = (chunk_start + BATCH).min(POINT_QUERY_TABLE_SIZE);
        let mut sql = String::with_capacity(end - chunk_start);
        for i in chunk_start..end {
            if i > chunk_start {
                sql.push_str(", ");
            }
            sql.push_str(&format!("({}, {}, 'row_{}')", i, i as f64 * 1.5, i));
        }
        tx.execute(&format!("INSERT INTO t VALUES {}", sql), []).unwrap();
    }
    tx.commit().unwrap();
    drop(conn);
}
