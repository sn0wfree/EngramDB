//! Compact 策略深度性能测试
//!
//! 1. 直接测量不同数据量下的 compact 耗时
//! 2. WAL 开启后的真实场景对比
//! 3. sync_wal 联动 compact 的开销

use engramdb::{Connection, CompactStrategy, WalFlushMode, Config, Value};
use std::time::Instant;

const TABLE_SQL: &str = "CREATE TABLE bench (id INT, name VARCHAR, age INT, score DOUBLE, active BOOLEAN)";

fn main() {
    println!("=== EngramDB v0.11.3 Compact 深度性能测试 ===\n");

    // 测试 1: compact 操作本身的耗时
    println!("【测试 1】Compact 操作直接耗时（不同数据量）\n");
    bench_compact_direct(1_000);
    bench_compact_direct(5_000);
    bench_compact_direct(10_000);
    bench_compact_direct(50_000);
    bench_compact_direct(100_000);
    bench_compact_direct(200_000);

    // 测试 2: WAL 开启后的策略对比
    println!("\n【测试 2】WAL Sync 模式下策略对比（10 万行）\n");
    bench_with_wal("Manual", CompactStrategy::manual(), WalFlushMode::Sync, 100_000);
    bench_with_wal("Full(10K)", CompactStrategy::full(10_000), WalFlushMode::Sync, 100_000);
    bench_with_wal("Incremental(10K/1K)", CompactStrategy::incremental(10_000, 1_000), WalFlushMode::Sync, 100_000);
    bench_with_wal("Adaptive(默认)", CompactStrategy::default_adaptive(122_880), WalFlushMode::Sync, 100_000);

    // 测试 3: Periodic + sync_wal 联动
    println!("\n【测试 3】Periodic WAL + sync_wal 联动 compact（10 万行）\n");
    bench_periodic_sync_wal(false, 100_000); // 不联动
    bench_periodic_sync_wal(true, 100_000);  // 联动

    println!("\n=== 测试完成 ===");
}

fn bench_compact_direct(rows: usize) {
    let path = format!("/tmp/bench_compact_direct_{}.db", rows);
    let _ = std::fs::remove_file(&path);

    let mut conn = Connection::open(&path).unwrap();
    conn.execute(TABLE_SQL).unwrap();
    conn.set_compact_strategy(CompactStrategy::manual());

    // 先写入数据到 Delta
    let stmt = conn.prepare("INSERT INTO bench VALUES (?, ?, ?, ?, ?)").unwrap();
    let batch_size = 1000;
    let mut offset = 0;
    while offset < rows {
        let end = std::cmp::min(offset + batch_size, rows);
        let mut batch = Vec::with_capacity(end - offset);
        for i in offset..end {
            batch.push(vec![
                Value::Int64(i as i64),
                Value::Varchar(format!("user_{}", i)),
                Value::Int64(20 + (i % 40) as i64),
                Value::Float64((i % 100) as f64),
                Value::Boolean(i % 2 == 0),
            ]);
        }
        conn.execute_prepared_batch(&stmt, &batch).unwrap();
        offset = end;
    }

    // 直接测量 compact 耗时
    let start = Instant::now();
    let merged = conn.compact("bench").unwrap();
    let elapsed = start.elapsed();

    let throughput = rows as f64 / elapsed.as_secs_f64();
    println!("  {:>7} 行: {:>8.3} ms  ({:>10.1} 万行/秒), 合并 {} 行",
        rows, elapsed.as_secs_f64() * 1000.0, throughput / 10000.0, merged);

    conn.close().unwrap();
    let _ = std::fs::remove_file(&path);
}

fn bench_with_wal(name: &str, strategy: CompactStrategy, wal_mode: WalFlushMode, total: usize) {
    let path = format!("/tmp/bench_wal_{}.db", name.replace(|c: char| !c.is_alphanumeric(), "_"));
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}-wal", path));

    let config = Config {
        compact_strategy: strategy,
        wal_flush_mode: wal_mode,
        ..Default::default()
    };

    let mut conn = Connection::open_with_config(&path, config).unwrap();
    conn.execute(TABLE_SQL).unwrap();

    let stmt = conn.prepare("INSERT INTO bench VALUES (?, ?, ?, ?, ?)").unwrap();
    let batch_size = 500;

    let mut latencies: Vec<f64> = Vec::new();

    let start = Instant::now();
    let mut offset = 0;
    while offset < total {
        let end = std::cmp::min(offset + batch_size, total);
        let mut batch = Vec::with_capacity(end - offset);
        for i in offset..end {
            batch.push(vec![
                Value::Int64(i as i64),
                Value::Varchar(format!("u{}", i)),
                Value::Int64(20 + (i % 40) as i64),
                Value::Float64((i % 100) as f64),
                Value::Boolean(i % 2 == 0),
            ]);
        }

        let batch_start = Instant::now();
        conn.execute_prepared_batch(&stmt, &batch).unwrap();
        latencies.push(batch_start.elapsed().as_secs_f64() * 1_000_000.0);

        offset = end;
    }
    let elapsed = start.elapsed();

    latencies.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p50 = latencies[latencies.len() / 2];
    let p99 = latencies[(latencies.len() as f64 * 0.99) as usize];
    let max = latencies.last().copied().unwrap_or(0.0);

    let rows_per_sec = total as f64 / elapsed.as_secs_f64();
    println!("  {:<22}  总耗时: {:>8.2} ms  吞吐: {:>7.1}万/s  P50: {:>7.1}µs  P99: {:>8.1}µs  Max: {:>8.2}ms",
        name,
        elapsed.as_secs_f64() * 1000.0,
        rows_per_sec / 10000.0,
        p50, p99, max / 1000.0);

    conn.close().unwrap();
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}-wal", path));
}

fn bench_periodic_sync_wal(link_compact: bool, total: usize) {
    let name = if link_compact { "Periodic+sync_wal联动" } else { "Periodic+sync_wal独立" };
    let path = format!("/tmp/bench_periodic_{}.db", if link_compact { "link" } else { "nolink" });
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}-wal", path));

    let config = Config {
        compact_strategy: CompactStrategy::incremental(10_000, 5_000),
        wal_flush_mode: WalFlushMode::Periodic,
        sync_wal_compact: link_compact,
        wal_buffer_size: 64 * 1024,
        ..Default::default()
    };

    let mut conn = Connection::open_with_config(&path, config).unwrap();
    conn.execute(TABLE_SQL).unwrap();

    let stmt = conn.prepare("INSERT INTO bench VALUES (?, ?, ?, ?, ?)").unwrap();
    let batch_size = 500;
    let sync_interval = 10_000; // 每写 1 万行 sync 一次

    let start = Instant::now();
    let mut offset = 0;
    while offset < total {
        let end = std::cmp::min(offset + batch_size, total);
        let mut batch = Vec::with_capacity(end - offset);
        for i in offset..end {
            batch.push(vec![
                Value::Int64(i as i64),
                Value::Varchar(format!("u{}", i)),
                Value::Int64(20 + (i % 40) as i64),
                Value::Float64((i % 100) as f64),
                Value::Boolean(i % 2 == 0),
            ]);
        }
        conn.execute_prepared_batch(&stmt, &batch).unwrap();
        offset = end;

        // 定期 sync_wal
        if offset % sync_interval < batch_size {
            conn.sync_wal().unwrap();
        }
    }
    // 最后再 sync 一次
    conn.sync_wal().unwrap();

    let elapsed = start.elapsed();
    let rows_per_sec = total as f64 / elapsed.as_secs_f64();

    // 写入后查询性能
    let q_start = Instant::now();
    let _ = conn.execute("SELECT COUNT(*) FROM bench WHERE age > 30").unwrap();
    let q_ms = q_start.elapsed().as_secs_f64() * 1000.0;

    println!("  {:<22}  总耗时: {:>8.2} ms  吞吐: {:>7.1}万/s  查询: {:>6.2} ms",
        name,
        elapsed.as_secs_f64() * 1000.0,
        rows_per_sec / 10000.0,
        q_ms);

    conn.close().unwrap();
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}-wal", path));
}
