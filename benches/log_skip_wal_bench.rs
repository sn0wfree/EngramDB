//! Phase 2 P1-C：LogEngine 写入基准（含 skip_wal 配置）
//!
//! 对比 LogEngine 在 WAL 开/关下的吞吐量。
//!
//! 运行：`cargo bench --bench log_skip_wal_bench`
//!
//! KPI 目标（Phase 2 P1-C）：
//! - LogEngine skip_wal=true 写入吞吐 ≥ +30% vs WAL 开
//! - 崩溃恢复（不在本期；待下个迭代验证）

use std::time::{Duration, Instant};

use engramdb::common::config::Config;
use engramdb::Connection;

const ITERS: usize = 10;
const N_ROWS: usize = 100_000;

fn median(mut samples: Vec<Duration>) -> Duration {
    samples.sort();
    samples[samples.len() / 2]
}

fn fmt_rate(d: Duration, n: usize) -> String {
    format!(
        "{:.0} 行/秒 ({:.1} ms total)",
        n as f64 / d.as_secs_f64(),
        d.as_secs_f64() * 1000.0
    )
}

fn bench(skip_wal: bool) -> Duration {
    let path = format!("/tmp/log_skip_{}_{}.hdb", if skip_wal { 1 } else { 0 }, std::process::id());
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}-wal", path));

    let mut cfg = Config::default();
    cfg.log_skip_wal = skip_wal;
    // 使用 Periodic 模式（更接近实际日志场景）
    cfg.wal_flush_mode = engramdb::common::config::WalFlushMode::Periodic;
    cfg.wal_group_commit_size = 64;

    let mut conn = Connection::open_with_config(&path, cfg).unwrap();
    conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY, v TEXT) ENGINE = Log")
        .unwrap();

    let t0 = Instant::now();
    for i in 0..N_ROWS {
        conn.execute(&format!("INSERT INTO t VALUES ({}, 'row-{}')", i, i))
            .unwrap();
    }
    let d = t0.elapsed();
    conn.close().unwrap();
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}-wal", path));
    d
}

fn main() {
    println!("=== Phase 2 P1-C: LogEngine Skip-WAL Bench ===");
    println!("数据：{} 行 autocommit INSERT，共 {} 轮取 P50", N_ROWS, ITERS);
    println!();

    let mut samples = Vec::with_capacity(ITERS);
    for _ in 0..ITERS {
        samples.push(bench(false));
    }
    let baseline = median(samples.clone());
    println!(
        "  baseline, log_skip_wal=false: median={} ({} samples)",
        fmt_rate(baseline, N_ROWS),
        ITERS
    );

    let mut samples = Vec::with_capacity(ITERS);
    for _ in 0..ITERS {
        samples.push(bench(true));
    }
    let skip_wal = median(samples.clone());
    println!(
        "  log_skip_wal=true: median={} ({} samples)",
        fmt_rate(skip_wal, N_ROWS),
        ITERS
    );

    let speedup = baseline.as_secs_f64() / skip_wal.as_secs_f64();
    println!();
    println!("=== 总结 ===");
    println!(
        "  WAL 开 (baseline):    {}",
        fmt_rate(baseline, N_ROWS)
    );
    println!(
        "  WAL 关 (skip_wal=true): {}",
        fmt_rate(skip_wal, N_ROWS)
    );
    println!("  加速比:                {:.2}×", speedup);
    println!();
    if speedup >= 1.3 {
        println!("✅ Phase 2 P1-C KPI 达成: skip_wal=true ≥ +30%");
    } else if speedup >= 1.1 {
        println!("ℹ️  skip_wal=true 提速 {:.1}%（>10% 但 <30%）", (speedup - 1.0) * 100.0);
    } else {
        println!("⚠️  skip_wal=true 提速仅 {:.1}%（<10%，可能受其他瓶颈限制）", (speedup - 1.0) * 100.0);
    }
}