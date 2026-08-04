//! SELECT * 全表扫描基准 (EngramDB vs SQLite)
//!
//! 验证 `SELECT *` 路径的优化效果：
//! - 阶段 0: 基线（IdentityProjection 未消除 + 7 次 cell 克隆）
//! - 阶段 1: IdentityProjection 消除 + ColumnRef 零拷贝
//! - 阶段 2: 消除 rows↔chunks 转置
//!
//! 运行：`cargo run --release --bench select_star_bench`

use std::time::{Duration, Instant};

use std::io::Write;
use engramdb::{Connection, Config};

const ITERS: usize = 5;
const HDB_PATH: &str = "/tmp/select_star.hdb";
const SQLITE_PATH: &str = "/tmp/select_star.sqlite";
const N: usize = 1_000_000;

fn open_hdb() -> Connection {
    let config = Config {
        compress_on_persist: false,
        ..Config::default()
    };
    Connection::open_with_config(HDB_PATH, config).unwrap()
}

fn median(mut samples: Vec<Duration>) -> Duration {
    samples.sort();
    let n = samples.len();
    if n == 0 { return Duration::ZERO; }
    if n % 2 == 1 { samples[n / 2] } else { (samples[n / 2 - 1] + samples[n / 2]) / 2 }
}

fn fmt_ms(d: Duration) -> String {
    let us = d.as_micros();
    if us < 1000 { format!("{} µs", us) }
    else { format!("{:.2} ms", us as f64 / 1000.0) }
}

fn cleanup_files() {
    let _ = std::fs::remove_file(HDB_PATH);
    let _ = std::fs::remove_file(format!("{}-wal", HDB_PATH));
    let _ = std::fs::remove_file(SQLITE_PATH);
    let _ = std::fs::remove_file(format!("{}-wal", SQLITE_PATH));
    let _ = std::fs::remove_file(format!("{}-shm", SQLITE_PATH));
}

fn setup_engramdb() {
    cleanup_files();
    let mut conn = open_hdb();
    conn.execute("CREATE TABLE t (id INT PRIMARY KEY, val DOUBLE, name VARCHAR)").unwrap();

    const BATCH: usize = 50_000;
    for chunk_start in (0..N).step_by(BATCH) {
        let end = (chunk_start + BATCH).min(N);
        let count = end - chunk_start;
        let mut col_id = Vec::with_capacity(count);
        let mut col_val = Vec::with_capacity(count);
        let mut col_name = Vec::with_capacity(count);
        for i in chunk_start..end {
            col_id.push(engramdb::Value::Int64(i as i64));
            col_val.push(engramdb::Value::Float64(i as f64 * 1.5));
            col_name.push(engramdb::Value::Varchar(format!("row_{}", i)));
        }
        conn.import_columns("t", vec![col_id, col_val, col_name]).unwrap();
    }
    conn.close().unwrap();
}

fn setup_sqlite() {
    let _ = std::fs::remove_file(SQLITE_PATH);
    let _ = std::fs::remove_file(format!("{}-wal", SQLITE_PATH));
    let _ = std::fs::remove_file(format!("{}-shm", SQLITE_PATH));
    let conn = rusqlite::Connection::open(SQLITE_PATH).unwrap();
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA cache_size = -20000;
         PRAGMA temp_store = MEMORY;",
    ).unwrap();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, val REAL, name TEXT)", []).unwrap();
    const BATCH: usize = 50_000;
    let tx = conn.unchecked_transaction().unwrap();
    for chunk_start in (0..N).step_by(BATCH) {
        let end = (chunk_start + BATCH).min(N);
        let mut sql = String::with_capacity(end - chunk_start);
        for i in chunk_start..end {
            if i > chunk_start { sql.push_str(", "); }
            sql.push_str(&format!("({}, {}, 'row_{}')", i, i as f64 * 1.5, i));
        }
        tx.execute(&format!("INSERT INTO t VALUES {}", sql), []).unwrap();
    }
    tx.commit().unwrap();
    drop(conn);
}

// 全列扫描
fn engramdb_star() -> Duration {
    let mut conn = open_hdb();
    let start = Instant::now();
    let r = conn.execute("SELECT * FROM t").unwrap();
    let dur = start.elapsed();
    assert_eq!(r.rows.len(), N, "expected {} rows, got {}", N, r.rows.len());
    conn.close().unwrap();
    dur
}

fn sqlite_star() -> Duration {
    let conn = rusqlite::Connection::open(SQLITE_PATH).unwrap();
    let start = Instant::now();
    let mut stmt = conn.prepare("SELECT * FROM t").unwrap();
    let mut rows = stmt.query([]).unwrap();
    let mut count = 0;
    while let Some(_) = rows.next().unwrap() { count += 1; }
    let dur = start.elapsed();
    assert_eq!(count, N, "expected {} rows, got {}", N, count);
    drop(rows);
    drop(stmt);
    drop(conn);
    dur
}

// 窄列扫描（对比基线）
fn engramdb_narrow() -> Duration {
    let mut conn = open_hdb();
    let start = Instant::now();
    let r = conn.execute("SELECT id, val FROM t").unwrap();
    let dur = start.elapsed();
    assert_eq!(r.rows.len(), N);
    conn.close().unwrap();
    dur
}

fn sqlite_narrow() -> Duration {
    let conn = rusqlite::Connection::open(SQLITE_PATH).unwrap();
    let start = Instant::now();
    let mut stmt = conn.prepare("SELECT id, val FROM t").unwrap();
    let mut rows = stmt.query([]).unwrap();
    let mut count = 0;
    while let Some(_) = rows.next().unwrap() { count += 1; }
    let dur = start.elapsed();
    assert_eq!(count, N);
    drop(rows);
    drop(stmt);
    drop(conn);
    dur
}

fn run(name: &str, mut e: impl FnMut() -> Duration, mut s: impl FnMut() -> Duration) {
    println!("\n--- {} ---", name);
    let mut eh = Vec::with_capacity(ITERS);
    let mut sh = Vec::with_capacity(ITERS);
    for i in 1..=ITERS {
        let h = e(); let st = s();
        println!("  第{}轮  EngramDB: {:>12}  SQLite: {:>12}  比值: {:>5.2}x",
            i, fmt_ms(h), fmt_ms(st), h.as_secs_f64() / st.as_secs_f64());
        eh.push(h); sh.push(st);
    }
    let hm = median(eh); let sm = median(sh);
    println!("  中位数  EngramDB: {:>12}  SQLite: {:>12}  比值: {:>5.2}x",
        fmt_ms(hm), fmt_ms(sm), hm.as_secs_f64() / sm.as_secs_f64());
}

fn main() {
    println!("==================================================================");
    println!("  SELECT * 全表扫描基准 (EngramDB vs SQLite)");
    println!("  轮次: {}    数据规模: {} 行", ITERS, N);
    println!("==================================================================");

    println!("\n设置 EngramDB 1M 行...");
    let t0 = Instant::now();
    setup_engramdb();
    println!("  done in {}", fmt_ms(t0.elapsed()));

    println!("设置 SQLite 1M 行...");
    let t0 = Instant::now();
    setup_sqlite();
    println!("  done in {}", fmt_ms(t0.elapsed()));

    run("SELECT * (全列扫描)", engramdb_star, sqlite_star);
    run("SELECT id, val (窄列扫描, 对照组)", engramdb_narrow, sqlite_narrow);

    cleanup_files();
}