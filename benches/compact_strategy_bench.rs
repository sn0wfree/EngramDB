//! Compact 策略性能对比基准测试
//!
//! 对比 4 种 Delta 合并策略在不同写入场景下的性能表现：
//! - Manual: 手动策略，写入路径零 compact 开销
//! - Full: 全量合并，达到阈值一次性合并
//! - Incremental: 增量式，分批合并
//! - Adaptive: 自适应分桶（默认策略）
//!
//! 测试维度：
//! 1. 平均写入吞吐（行/秒）
//! 2. 写入延迟分布（P50 / P99 / Max）
//! 3. 总耗时
//! 4. Compact 次数
//! 5. 查询性能（合并后 vs 合并前）

use engramdb::{Connection, CompactStrategy, Value};
use std::time::{Instant, Duration};

const TABLE_SQL: &str = "CREATE TABLE bench (id INT, name VARCHAR, age INT, score DOUBLE, active BOOLEAN)";
const TOTAL_ROWS: usize = 200_000;
const BATCH_SIZE: usize = 500; // 小批量写入，确保走 Delta 路径

fn main() {
    println!("=== EngramDB v0.11.3 Compact 策略性能对比测试 ===\n");
    println!("测试配置: {} 行，{} 行/批，5 列（Int/Varchar/Int/Double/Bool）\n", TOTAL_ROWS, BATCH_SIZE);

    // 测试各策略
    let results = vec![
        run_strategy_bench("Manual (手动)", CompactStrategy::manual()),
        run_strategy_bench("Full (全量, 阈值=10K)", CompactStrategy::full(10_000)),
        run_strategy_bench("Full (全量, 阈值=50K)", CompactStrategy::full(50_000)),
        run_strategy_bench("Incremental (阈值=10K, 批次=1K)", CompactStrategy::incremental(10_000, 1_000)),
        run_strategy_bench("Incremental (阈值=50K, 批次=10K)", CompactStrategy::incremental(50_000, 10_000)),
        run_strategy_bench("Incremental (阈值=50K, 批次=50K)", CompactStrategy::incremental(50_000, 50_000)),
        run_strategy_bench("Adaptive (默认, min=10K/max=120K/10%)",
            CompactStrategy::default_adaptive(122_880)),
        run_strategy_bench("Adaptive (激进, min=5K/max=60K/5%)",
            CompactStrategy::Adaptive {
                min_threshold: 5_000,
                max_threshold: 60_000,
                pct_of_table: 0.05,
                batch_size: 60_000,
            }),
    ];

    // 输出汇总表
    print_summary_table(&results);

    // 延迟分布对比
    print_latency_comparison(&results);

    println!("\n=== 测试完成 ===");
}

struct BenchResult {
    name: String,
    total_time_ms: f64,
    rows_per_sec: f64,
    p50_latency_us: f64,
    p99_latency_us: f64,
    max_latency_us: f64,
    compact_count: usize,
    query_after_ms: f64,
}

fn run_strategy_bench(name: &str, strategy: CompactStrategy) -> BenchResult {
    println!("────────────────────────────────────────");
    println!("策略: {}", name);

    let path = format!("/tmp/bench_strategy_{}.db", name.replace(|c: char| !c.is_alphanumeric(), "_"));
    let _ = std::fs::remove_file(&path);

    let mut conn = Connection::open(&path).unwrap();
    conn.execute(TABLE_SQL).unwrap();
    conn.set_compact_strategy(strategy);

    let stmt = conn.prepare("INSERT INTO bench VALUES (?, ?, ?, ?, ?)").unwrap();

    // 记录每批次的延迟
    let mut batch_latencies: Vec<f64> = Vec::new();
    let mut compact_count = 0usize;

    let start = Instant::now();
    let mut offset = 0;
    while offset < TOTAL_ROWS {
        let end = std::cmp::min(offset + BATCH_SIZE, TOTAL_ROWS);
        let mut batch = Vec::with_capacity(end - offset);
        for i in offset..end {
            let params = vec![
                Value::Int64(i as i64),
                Value::Varchar(format!("user_{}", i)),
                Value::Int64(20 + (i % 40) as i64),
                Value::Float64((i % 100) as f64),
                Value::Boolean(i % 2 == 0),
            ];
            batch.push(params);
        }

        let batch_start = Instant::now();
        conn.execute_prepared_batch(&stmt, &batch).unwrap();
        let batch_elapsed = batch_start.elapsed().as_secs_f64() * 1_000_000.0; // µs
        batch_latencies.push(batch_elapsed);

        // 粗略估计 compact 次数（通过延迟突增检测）
        // 单次正常写入 < 500µs，compact 通常 > 1ms
        if batch_elapsed > 1000.0 {
            compact_count += 1;
        }

        offset = end;
    }
    let total_elapsed = start.elapsed();
    let total_time_ms = total_elapsed.as_secs_f64() * 1000.0;
    let rows_per_sec = TOTAL_ROWS as f64 / total_elapsed.as_secs_f64();

    // 计算延迟分布
    batch_latencies.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p50 = percentile(&batch_latencies, 0.50);
    let p99 = percentile(&batch_latencies, 0.99);
    let max_lat = *batch_latencies.last().unwrap_or(&0.0);

    // 写入后的查询性能（含 Delta 扫描）
    let query_start = Instant::now();
    let result = conn.execute("SELECT COUNT(*) FROM bench WHERE age > 30").unwrap();
    let query_after_ms = query_start.elapsed().as_secs_f64() * 1000.0;
    let _ = result;

    println!("  总耗时: {:.2} ms", total_time_ms);
    println!("  吞吐: {:.1} 行/秒 ({:.1} 万行/秒)", rows_per_sec, rows_per_sec / 10000.0);
    println!("  延迟 P50: {:.1} µs", p50);
    println!("  延迟 P99: {:.1} µs", p99);
    println!("  延迟 Max: {:.1} µs ({:.2} ms)", max_lat, max_lat / 1000.0);
    println!("  预估 compact 次数: ~{}", compact_count);
    println!("  写入后查询耗时: {:.2} ms", query_after_ms);
    println!();

    conn.close().unwrap();
    let _ = std::fs::remove_file(&path);

    BenchResult {
        name: name.to_string(),
        total_time_ms,
        rows_per_sec,
        p50_latency_us: p50,
        p99_latency_us: p99,
        max_latency_us: max_lat,
        compact_count,
        query_after_ms,
    }
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = (sorted.len() as f64 * p) as usize;
    let idx = idx.min(sorted.len() - 1);
    sorted[idx]
}

fn print_summary_table(results: &[BenchResult]) {
    println!("\n=== 汇总对比表 ===\n");
    println!("{:<42} {:>12} {:>14} {:>12} {:>12} {:>10}",
        "策略", "总耗时(ms)", "吞吐(万行/s)", "P50(µs)", "P99(µs)", "Max(ms)");
    println!("{}", "-".repeat(105));

    // 找基准（Manual）
    let baseline = results.iter().find(|r| r.name.contains("Manual"));
    let baseline_throughput = baseline.map(|r| r.rows_per_sec).unwrap_or(1.0);

    for r in results {
        let speedup = r.rows_per_sec / baseline_throughput;
        let speedup_str = if speedup > 1.0 {
            format!(" (+{:.0}%)", (speedup - 1.0) * 100.0)
        } else if speedup < 1.0 {
            format!(" ({:.0}%)", (speedup - 1.0) * 100.0)
        } else {
            String::from(" (基准)")
        };

        println!("{:<42} {:>12.2} {:>10.2}{:<10} {:>12.1} {:>12.1} {:>10.2}",
            r.name,
            r.total_time_ms,
            r.rows_per_sec / 10000.0,
            speedup_str,
            r.p50_latency_us,
            r.p99_latency_us,
            r.max_latency_us / 1000.0,
        );
    }
}

fn print_latency_comparison(results: &[BenchResult]) {
    println!("\n=== 延迟稳定性对比 ===\n");
    println!("{:<42} {:>10} {:>10} {:>10} {:>12}",
        "策略", "P50(µs)", "P99(µs)", "Max(ms)", "P99/P50 倍数");
    println!("{}", "-".repeat(80));

    for r in results {
        let ratio = if r.p50_latency_us > 0.0 {
            r.p99_latency_us / r.p50_latency_us
        } else {
            0.0
        };
        println!("{:<42} {:>10.1} {:>10.1} {:>10.2} {:>12.1}x",
            r.name,
            r.p50_latency_us,
            r.p99_latency_us,
            r.max_latency_us / 1000.0,
            ratio,
        );
    }

    println!("\n说明:");
    println!("  - P50 越低越好（平均延迟）");
    println!("  - P99 越低越好（尾部延迟）");
    println!("  - P99/P50 倍数越接近 1 表示延迟越稳定");
    println!("  - Max 反映最坏情况下的阻塞时间");
}
