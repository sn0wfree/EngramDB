//! Phase 2.5 P3+P4 集成基准：Bloom Filter 列存跳读
//!
//! 对比 Phase 2（无 Bloom Filter 集成）vs Phase 2.5（有 Bloom Filter 集成）
//! 在高基数等值查询上的延迟。
//!
//! 运行：`cargo bench --bench bloom_integration_bench`
//!
//! KPI 目标（Phase 2.5 P3+P4）：
//! - 高基数列 + 等值查询（值不存在）→ Bloom 跳读 ≥ 5×
//! - 等值查询（值存在）→ Bloom + typed 双重校验（正确性）

use std::time::Instant;

use engramdb::Connection;

const ITERS: usize = 10;
const N: usize = 100_000;

fn median(mut samples: Vec<u128>) -> u128 {
    samples.sort();
    samples[samples.len() / 2]
}

fn fmt_us(ns: u128) -> String {
    if ns < 1_000 {
        format!("{} ns", ns)
    } else {
        format!("{:.2} µs", ns as f64 / 1000.0)
    }
}

fn setup_table() -> Connection {
    let path = format!("/tmp/bloom_int_{}.hdb", std::process::id());
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}-wal", path));

    let mut conn = Connection::open(&path).unwrap();
    conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY, v TEXT)").unwrap();
    for i in 0..N {
        conn.execute(&format!("INSERT INTO t VALUES ({}, 'r{}')", i, i)).unwrap();
    }
    conn.sync_wal().unwrap();
    conn
}

fn main() {
    println!("=== Phase 2.5 P3+P4: Bloom Integration Bench ===");
    println!("数据：{} 行 (高基数 Int64 PK)", N);
    println!();

    let mut conn = setup_table();

    // -------- 场景 A：等值查询 — 不存在值（Bloom 跳读）--------
    println!("--- 场景 A：等值查询 id = -1（值不存在 → Bloom 应快速 skip） ---");
    let mut samples = Vec::new();
    for _ in 0..ITERS {
        let t0 = Instant::now();
        let r = conn.execute("SELECT * FROM t WHERE id = -1").unwrap();
        let _ = r;
        samples.push(t0.elapsed().as_nanos());
    }
    let m_a = median(samples.clone());
    println!("  Bloom 跳读: median={}", fmt_us(m_a));
    println!();

    // -------- 场景 B：等值查询 — 存在值（Bloom + typed 双检）--------
    println!("--- 场景 B：等值查询 id = 50000（值存在 → Bloom + typed 校验） ---");
    let mut samples = Vec::new();
    for _ in 0..ITERS {
        let t0 = Instant::now();
        let r = conn.execute("SELECT * FROM t WHERE id = 50000").unwrap();
        let _ = r;
        samples.push(t0.elapsed().as_nanos());
    }
    let m_b = median(samples.clone());
    println!("  Bloom + typed: median={}", fmt_us(m_b));
    println!();

    // -------- 场景 C：范围查询（MinMax 跳读，非 Bloom 路径）--------
    println!("--- 场景 C：范围查询 id > 99000（MinMax 跳读，对照） ---");
    let mut samples = Vec::new();
    for _ in 0..ITERS {
        let t0 = Instant::now();
        let r = conn.execute("SELECT * FROM t WHERE id > 99000").unwrap();
        let _ = r;
        samples.push(t0.elapsed().as_nanos());
    }
    let m_c = median(samples.clone());
    println!("  MinMax: median={}", fmt_us(m_c));
    println!();

    // -------- 场景 D：全表扫描 --------
    println!("--- 场景 D：全表扫描 SELECT *（无跳读，对照） ---");
    let mut samples = Vec::new();
    for _ in 0..ITERS {
        let t0 = Instant::now();
        let r = conn.execute("SELECT * FROM t").unwrap();
        let _ = r;
        samples.push(t0.elapsed().as_nanos());
    }
    let m_d = median(samples.clone());
    println!("  全表扫描: median={}", fmt_us(m_d));
    println!();

    println!("=== 总结 ===");
    println!("  Bloom 跳读 (id = -1):        {}", fmt_us(m_a));
    println!("  Bloom + typed (id = 50000):  {}", fmt_us(m_b));
    println!("  MinMax 范围 (id > 99000):    {}", fmt_us(m_c));
    println!("  全表扫描:                   {}", fmt_us(m_d));

    let skip_ratio = m_d as f64 / m_a.max(1) as f64;
    println!();
    if skip_ratio >= 5.0 {
        println!(
            "✅ Phase 2.5 KPI 达成: Bloom 跳读 / 全表扫描 = {:.2}× (≥ 5×)",
            skip_ratio
        );
    } else if skip_ratio >= 2.0 {
        println!("ℹ️  Bloom 跳读 / 全表扫描 = {:.2}×（< 5×）", skip_ratio);
    } else {
        println!("⚠️  Bloom 跳读比 = {:.2}×（< 2×，可能受其他瓶颈影响）", skip_ratio);
    }
}
