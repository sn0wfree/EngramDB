//! M3 LogEngine 验收微基准
//!
//! 运行：`cargo bench --bench m3_log_bench`
//!
//! 验收标准（文档 v1.0）：
//! - 批量写入 > 50 万行/秒（对比列存 ~5 万行/秒）
//! - 时间范围扫描 1.5-2x（MinMax 块级跳读 vs 列存行级筛选）

use std::time::{Duration, Instant};
use engramdb::{Connection, Value};

const ITERS: usize = 5;
const N: usize = 1_000_000;

fn median(mut samples: Vec<Duration>) -> Duration {
    samples.sort();
    if samples.len() % 2 == 1 {
        samples[samples.len() / 2]
    } else {
        (samples[samples.len() / 2 - 1] + samples[samples.len() / 2]) / 2
    }
}

fn fmt(d: Duration) -> String {
    let us = d.as_secs_f64() * 1e6;
    if us < 1000.0 {
        format!("{:.1} µs", us)
    } else {
        format!("{:.2} ms", us / 1000.0)
    }
}

fn fmt_rate(d: Duration, rows: usize) -> String {
    let per_sec = rows as f64 / d.as_secs_f64();
    format!("{:.0} 万行/秒", per_sec / 10_000.0)
}

fn main() {
    println!("=== M3 LogEngine 验收微基准 ===");
    std::fs::remove_file("/tmp/m3_log.hdb").ok();
    std::fs::remove_file("/tmp/m3_log.hdb-wal").ok();

    let mut conn = Connection::open("/tmp/m3_log.hdb").unwrap();
    conn.execute("CREATE TABLE log_t (ts INT64, event VARCHAR) ENGINE = Log").unwrap();
    conn.execute("CREATE TABLE col_t (ts INT64, event VARCHAR)").unwrap();

    // 预生成 N 行（ts 递增时间戳，event 固定宽度字符串）
    // 与 M2 验收同方法论：巨型 INSERT 单语句批量写入（一次事务/一次 apply）
    let batch: Vec<Vec<Value>> = (0..N)
        .map(|i| {
            vec![
                Value::Int64(i as i64),
                Value::Varchar(format!("evt_{}", i % 10000)),
            ]
        })
        .collect();

    // ---- 写入吞吐 ----
    let mut samples = Vec::new();
    for _ in 0..ITERS {
        let mut sql = String::from("INSERT INTO log_t VALUES ");
        for (i, row) in batch.iter().enumerate() {
            if i > 0 { sql.push(','); }
            match &row[1] {
                Value::Varchar(s) => sql.push_str(&format!("({}, '{}')", row[0].as_i64().unwrap(), s)),
                _ => unreachable!(),
            }
        }
        let t0 = Instant::now();
        conn.execute(&sql).unwrap();
        samples.push(t0.elapsed());
    }
    let med = median(samples);
    println!("Log   批量写入 1M 行: {} / 批，{}", fmt(med), fmt_rate(med, N));

    let mut samples = Vec::new();
    for _ in 0..ITERS {
        let mut sql = String::from("INSERT INTO col_t VALUES ");
        for (i, row) in batch.iter().enumerate() {
            if i > 0 { sql.push(','); }
            match &row[1] {
                Value::Varchar(s) => sql.push_str(&format!("({}, '{}')", row[0].as_i64().unwrap(), s)),
                _ => unreachable!(),
            }
        }
        let t0 = Instant::now();
        conn.execute(&sql).unwrap();
        samples.push(t0.elapsed());
    }
    let med = median(samples);
    println!("Columnar 批量写入 1M 行: {} / 批，{}", fmt(med), fmt_rate(med, N));

    // ---- 时间范围扫描（命中最后 10 万行，跨 12 个块）----
    let mut samples = Vec::new();
    for _ in 0..ITERS {
        let t0 = Instant::now();
        let r = conn.execute("SELECT COUNT(*) FROM log_t WHERE ts >= 900000").unwrap();
        let _ = r;
        samples.push(t0.elapsed());
    }
    let log_scan = median(samples);

    let mut samples = Vec::new();
    for _ in 0..ITERS {
        let t0 = Instant::now();
        let r = conn.execute("SELECT COUNT(*) FROM col_t WHERE ts >= 900000").unwrap();
        let _ = r;
        samples.push(t0.elapsed());
    }
    let col_scan = median(samples);

    println!("Log   时间范围扫描 ts>=900000 (10% 命中): {}", fmt(log_scan));
    println!("Columnar 时间范围扫描 ts>=900000 (10% 命中): {}", fmt(col_scan));
    println!(
        "扫描加速: {:.2}x",
        col_scan.as_secs_f64() / log_scan.as_secs_f64()
    );

    // ---- 小时间窗（命中 1%，最不利块级跳读）----
    let mut samples = Vec::new();
    for _ in 0..ITERS {
        let t0 = Instant::now();
        let r = conn.execute("SELECT COUNT(*) FROM log_t WHERE ts >= 990000").unwrap();
        let _ = r;
        samples.push(t0.elapsed());
    }
    let log_narrow = median(samples);
    let mut samples = Vec::new();
    for _ in 0..ITERS {
        let t0 = Instant::now();
        let r = conn.execute("SELECT COUNT(*) FROM col_t WHERE ts >= 990000").unwrap();
        let _ = r;
        samples.push(t0.elapsed());
    }
    let col_narrow = median(samples);
    println!("Log   时间范围扫描 ts>=990000 (1% 命中): {}", fmt(log_narrow));
    println!("Columnar 时间范围扫描 ts>=990000 (1% 命中): {}", fmt(col_narrow));
    println!(
        "扫描加速: {:.2}x",
        col_narrow.as_secs_f64() / log_narrow.as_secs_f64()
    );
}
