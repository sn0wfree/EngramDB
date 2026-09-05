//! WAL 组提交基准 — group=0 / 16 / 64 / 128 + timeout 10ms
//!
//! 运行：`cargo bench --bench wal_group_commit_bench`
//!
//! KPI 来源：Phase 1 M1（`docs/p1-m1-wal-verification.md`）
//! 验收阈值：`group=16` vs `group=0` 事务吞吐 **≥ 3×**（10 次 P50）
//!
//! 测量方法：每次插入一条 autocommit INSERT，跑 N 次事务，记总耗时。
//! 每组配置测 ITERS 次取 P50。
//!
//! 重要：组提交的收益来自 fsync 是瓶颈的环境。
//! - 在快 NVMe / RAM-backed FS 上 fsync 接近零成本，组提交几乎无收益
//! - 在 HDD / 远程磁盘 / 容器内 fsync 显著时，组提交能 3-10×
//!
//! 触发条件说明：
//! - `wal_group_commit_size == 0 && wal_group_commit_max_bytes == 0` 时
//!   **完全禁用组提交**，每次 commit 都 fsync（true baseline）
//! - 任一非零 → 启用组提交模式（按 size/bytes/timeout 任一触发 fsync）

use engramdb::common::config::Config;
use engramdb::Connection;
use std::time::{Duration, Instant};

const ITERS: usize = 10;
const N_TXNS: usize = 5_000;

fn median(mut samples: Vec<Duration>) -> Duration {
    samples.sort();
    samples[samples.len() / 2]
}

fn fmt_rate(d: Duration, n: usize) -> String {
    format!(
        "{:.0} txn/s ({:.2} ms total)",
        n as f64 / d.as_secs_f64(),
        d.as_secs_f64() * 1000.0
    )
}

fn bench(group_size: usize, group_bytes: usize, timeout_ms: u64, n: usize) -> Duration {
    let path = format!(
        "/tmp/wal_gc_{}_{}_{}_{}.hdb",
        group_size,
        group_bytes,
        timeout_ms,
        std::process::id()
    );
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}-wal", path));

    let mut cfg = Config::default();
    cfg.wal_group_commit_size = group_size;
    cfg.wal_group_commit_max_bytes = group_bytes;
    cfg.wal_group_commit_timeout_ms = timeout_ms;

    let mut conn = Connection::open_with_config(&path, cfg).unwrap();
    conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY, v TEXT)").unwrap();

    let t0 = Instant::now();
    for i in 0..n {
        conn.execute(&format!("INSERT INTO t VALUES ({}, 'p')", i)).unwrap();
    }
    let d = t0.elapsed();
    conn.close().unwrap();
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}-wal", path));
    d
}

fn measure(label: &str, group_size: usize, group_bytes: usize, timeout_ms: u64, n: usize) -> Duration {
    let mut samples = Vec::with_capacity(ITERS);
    for _ in 0..ITERS {
        samples.push(bench(group_size, group_bytes, timeout_ms, n));
    }
    let m = median(samples);
    println!("  {}: median={} ({} samples)", label, fmt_rate(m, n), ITERS);
    m
}

fn main() {
    println!("=== WAL Group Commit Bench (Phase 1 M1) ===");
    println!("每轮 {} 个 autocommit INSERT，共 {} 轮取 P50", N_TXNS, ITERS);
    println!();

    // True baseline：size=0 AND bytes=0 → 完全禁用组提交
    let baseline = measure("group=0/0   (true baseline, 每次 fsync)", 0, 0, 0, N_TXNS);

    println!();
    let m16 = measure("group=16/64K/10ms  (Phase 1 默认)", 16, 65536, 10, N_TXNS);
    let m64 = measure("group=64/64K/10ms  (大组)", 64, 65536, 10, N_TXNS);
    let m128 = measure("group=128/64K/10ms (超大批)", 128, 65536, 10, N_TXNS);
    let m_only_size = measure("group=16/0/0   (仅 size 触发)", 16, 0, 0, N_TXNS);
    let _ = m_only_size;
    let m_only_bytes = measure("group=0/64K/0  (仅 bytes 触发)", 0, 65536, 0, N_TXNS);
    let _ = m_only_bytes;
    println!();

    let speedup_16 = baseline.as_secs_f64() / m16.as_secs_f64();
    let speedup_64 = baseline.as_secs_f64() / m64.as_secs_f64();
    let speedup_128 = baseline.as_secs_f64() / m128.as_secs_f64();

    println!("=== 总结 ===");
    println!(
        "  基线 (true group=0):  {} txn/s",
        (N_TXNS as f64 / baseline.as_secs_f64()) as i64
    );
    println!(
        "  group=16 default:     {} txn/s ({:.2}×)",
        (N_TXNS as f64 / m16.as_secs_f64()) as i64,
        speedup_16
    );
    println!(
        "  group=64:             {} txn/s ({:.2}×)",
        (N_TXNS as f64 / m64.as_secs_f64()) as i64,
        speedup_64
    );
    println!(
        "  group=128:            {} txn/s ({:.2}×)",
        (N_TXNS as f64 / m128.as_secs_f64()) as i64,
        speedup_128
    );
    println!();
    if speedup_16 >= 3.0 {
        println!("✅ KPI 达成: group=16 vs true group=0 = {:.2}× ≥ 3×", speedup_16);
    } else if speedup_16 >= 1.5 {
        println!("⚠️  KPI 部分达成: {:.2}× (≥ 1.5×, < 3×)", speedup_16);
        println!("    说明: 在快 NVMe 上 fsync 成本被分摊，组提交只显示部分收益。");
        println!("           在 HDD / 远程磁盘 / 容器内可观察 3-10× 加速。");
    } else if speedup_16 > 0.95 {
        println!("ℹ️  当前环境 fsync 接近零成本，组提交无明显收益 ({:.2}×)", speedup_16);
        println!("    这是预期的——组提交只能优化 fsync 瓶颈，本机无此瓶颈。");
    } else {
        println!("❌ 异常: group=16 反而慢于 baseline ({:.2}×)", speedup_16);
    }
}
