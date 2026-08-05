//! v0.18 LogEngine 写入优化基准
//!
//! 运行：`cargo bench --bench v018_write_bench`
//!
//! 对比项：
//! - 批量落盘（巨型单 INSERT / import_columns）：列式直写路径（P0-1）
//! - 逐行 autocommit INSERT：Batcher 攒批（P0-2）vs 关闭 Batcher
//! - 组提交时间窗（P0-3）：低流量延迟有界（Sync 模式下持久化窗口）

use std::time::{Duration, Instant};
use engramdb::common::config::Config;
use engramdb::{Connection, Value};

const ITERS: usize = 3;

fn median(mut samples: Vec<Duration>) -> Duration {
    samples.sort();
    if samples.len() % 2 == 1 {
        samples[samples.len() / 2]
    } else {
        (samples[samples.len() / 2 - 1] + samples[samples.len() / 2]) / 2
    }
}

fn fmt_rate(d: Duration, rows: usize) -> String {
    format!("{:.0} 万行/秒", rows as f64 / d.as_secs_f64() / 10_000.0)
}

fn new_cfg() -> Config {
    let mut cfg = Config::default();
    cfg.enable_transaction = true;
    cfg
}

fn main() {
    println!("=== v0.18 LogEngine 写入优化基准 ===");
    main_plan_cache();

    // ---- 1. 批量落盘（import_columns 列式直写）----
    {
        let n = 1_000_000u64;
        let mut times = Vec::new();
        for _ in 0..ITERS {
            let path = format!("/tmp/v018_batch_{}.hdb", std::process::id());
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_file(format!("{}-wal", path));
            let mut conn = Connection::open_with_config(&path, new_cfg()).unwrap();
            conn.execute("CREATE TABLE t (ts INT64, v TEXT) ENGINE = Log").unwrap();
            let cols = vec![
                (0..n).map(|i| Value::Int64(i as i64)).collect(),
                (0..n).map(|i| Value::Varchar(format!("e{}", i))).collect(),
            ];
            let t0 = Instant::now();
            conn.import_columns("t", cols).unwrap();
            let d = t0.elapsed();
            times.push(d);
            println!("  批量 import_columns {} 行: {} ({})", n, fmt_rate(d, n as usize), "P0-1 列式直写");
            conn.close().unwrap();
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_file(format!("{}-wal", path));
        }
        println!("  [中位数] {}", fmt_rate(median(times), n as usize));
    }

    // ---- 2. 逐行 INSERT：Batcher 开 vs 关 ----
    for (name, enabled) in [("Batcher 开", true), ("Batcher 关", false)] {
        let n = 200_000usize;
        let mut times = Vec::new();
        for _ in 0..ITERS {
            let path = format!("/tmp/v018_row_{}.hdb", std::process::id());
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_file(format!("{}-wal", path));
            let mut cfg = new_cfg();
            cfg.wal_batch_insert = enabled;
            let mut conn = Connection::open_with_config(&path, cfg).unwrap();
            conn.execute("CREATE TABLE t (ts INT64, v TEXT) ENGINE = Log").unwrap();
            let t0 = Instant::now();
            for i in 0..n {
                conn.execute(&format!("INSERT INTO t VALUES ({}, 'e{}')", i, i)).unwrap();
            }
            conn.sync_wal().unwrap();
            let d = t0.elapsed();
            times.push(d);
            println!("  逐行 INSERT {} 行（{}）: {}", n, name, fmt_rate(d, n));
            conn.close().unwrap();
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_file(format!("{}-wal", path));
        }
        println!("  [中位数] {}", fmt_rate(median(times), n));
    }

    // ---- 3. 低流量持久化窗口（组提交时间窗）----
    {
        let path = format!("/tmp/v018_low_{}.hdb", std::process::id());
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{}-wal", path));
        let mut conn = Connection::open_with_config(&path, new_cfg()).unwrap();
        conn.execute("CREATE TABLE t (ts INT64, v TEXT) ENGINE = Log").unwrap();
        // 单条 INSERT 后等待 > 10ms，模拟低流量：时间窗应触发 fsync（而非等 16 次）
        let t0 = Instant::now();
        conn.execute("INSERT INTO t VALUES (1, 'a')").unwrap();
        std::thread::sleep(Duration::from_millis(15));
        conn.execute("INSERT INTO t VALUES (2, 'b')").unwrap();
        let d = t0.elapsed();
        // 第二条语句的 commit 应因时间窗强制 fsync；持久化窗口 ≈ 15ms 有界
        println!("  低流量两行间隔写入总耗时: {:.1} ms（时间窗有界，无 16 次等待）", d.as_millis());
        conn.close().unwrap();
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{}-wal", path));
    }
}

// v0.18 P0-1 计划缓存基准：同 SQL 重复场景 parse+plan 占比实测
// 对比组（均走 Batcher，仅差 parse+plan）：
//  - 变值 SQL（缓存必然 miss）
//  - 同值 SQL（execute 缓存命中）
//  - 参数化 prepared（PreparedStatement 省 parse，但每次重跑 plan）
fn main_plan_cache() {
    println!("=== v0.18 P0-1 计划缓存基准（同 SQL 重复 20 万次）===");
    let n = 200_000usize;
    let mut miss_times = Vec::new();
    let mut hit_times = Vec::new();
    let mut prep_times = Vec::new();
    for _ in 0..ITERS {
        // 变值 SQL：缓存 miss
        {
            let path = format!("/tmp/v018_pc_miss_{}.hdb", std::process::id());
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_file(format!("{}-wal", path));
            let mut conn = Connection::open_with_config(&path, new_cfg()).unwrap();
            conn.execute("CREATE TABLE t (ts INT64, v TEXT) ENGINE = Log").unwrap();
            let t0 = Instant::now();
            for i in 0..n {
                conn.execute(&format!("INSERT INTO t VALUES ({}, 'e{}')", i, i)).unwrap();
            }
            conn.sync_wal().unwrap();
            miss_times.push(t0.elapsed());
            println!("  变值 SQL（缓存 miss）: {}", fmt_rate(t0.elapsed(), n));
            conn.close().unwrap();
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_file(format!("{}-wal", path));
        }
        // 同值 SQL：execute 缓存命中
        {
            let path = format!("/tmp/v018_pc_hit_{}.hdb", std::process::id());
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_file(format!("{}-wal", path));
            let mut conn = Connection::open_with_config(&path, new_cfg()).unwrap();
            conn.execute("CREATE TABLE t (ts INT64, v TEXT) ENGINE = Log").unwrap();
            let t0 = Instant::now();
            for _ in 0..n {
                conn.execute("INSERT INTO t VALUES (1, 'a')").unwrap();
            }
            conn.sync_wal().unwrap();
            hit_times.push(t0.elapsed());
            println!("  同值 SQL（缓存命中）: {}", fmt_rate(t0.elapsed(), n));
            conn.close().unwrap();
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_file(format!("{}-wal", path));
        }
        // 参数化 prepared：省 parse，每次重跑 plan
        {
            let path = format!("/tmp/v018_pc_prep_{}.hdb", std::process::id());
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_file(format!("{}-wal", path));
            let mut conn = Connection::open_with_config(&path, new_cfg()).unwrap();
            conn.execute("CREATE TABLE t (ts INT64, v TEXT) ENGINE = Log").unwrap();
            let stmt = conn.prepare("INSERT INTO t VALUES (?, ?)").unwrap();
            let t0 = Instant::now();
            for i in 0..n {
                conn.execute_prepared(&stmt, &[Value::Int64(i as i64), Value::Varchar(format!("e{}", i))]).unwrap();
            }
            conn.sync_wal().unwrap();
            prep_times.push(t0.elapsed());
            println!("  参数化 prepared（重跑 plan）: {}", fmt_rate(t0.elapsed(), n));
            conn.close().unwrap();
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_file(format!("{}-wal", path));
        }
    }
    let miss = median(miss_times);
    let hit = median(hit_times);
    let prep = median(prep_times);
    println!("  [中位数] miss {} / hit {} / prepared {}", fmt_rate(miss, n), fmt_rate(hit, n), fmt_rate(prep, n));
    println!("  缓存收益: {:.1}x（同 SQL 场景）", miss.as_secs_f64() / hit.as_secs_f64());
    println!("  prepared 重跑 plan 成本: {:.1}% 相对 miss 全解析", (prep.as_secs_f64() / miss.as_secs_f64()) * 100.0);
}
