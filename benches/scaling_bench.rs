//! 数据规模扩展性能测试
//!
//! 测试不同数据规模下的性能变化曲线。
//!
//! 运行：`cargo bench --bench scaling_bench`

use engramdb::Connection;
use std::time::{Duration, Instant};

const SCALES: &[usize] = &[1_000, 10_000, 100_000, 1_000_000, 5_000_000];
const ITERS: usize = 3;

fn median(mut samples: Vec<Duration>) -> Duration {
    samples.sort();
    samples[samples.len() / 2]
}

fn fmt_rate(d: Duration, n: usize) -> String {
    format!(
        "{:.0} 行/秒 ({:.1} ms)",
        n as f64 / d.as_secs_f64(),
        d.as_secs_f64() * 1000.0
    )
}

fn fmt_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{} B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

fn main() {
    println!("=== 数据规模扩展性能测试 ===");
    println!("规模: {:?} 行, {} 轮取中位数", SCALES, ITERS);
    println!();

    // ==================== 1. 写入扩展性 ====================
    println!("━━━ 1. 写入扩展性 (逐行 INSERT) ━━━");
    for &n in SCALES {
        let mut times = Vec::new();
        for _ in 0..ITERS {
            let path = format!("/tmp/scaling_write_{}_{}.hdb", n, std::process::id());
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_file(format!("{}-wal", path));

            let mut conn = Connection::open(&path).unwrap();
            conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY, v TEXT)").unwrap();

            let t0 = Instant::now();
            for i in 0..n {
                conn.execute(&format!("INSERT INTO t VALUES ({}, 'row-{}')", i, i)).unwrap();
            }
            times.push(t0.elapsed());
            conn.close().unwrap();
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_file(format!("{}-wal", path));
        }
        let m = median(times);
        println!("  {} 行: {}", fmt_rate(m, n), fmt_rate(m, n));
    }

    println!();

    // ==================== 2. 批量导入扩展性 ====================
    println!("━━━ 2. 批量导入扩展性 (import_columns) ━━━");
    for &n in SCALES {
        let mut times = Vec::new();
        for _ in 0..ITERS {
            let path = format!("/tmp/scaling_import_{}_{}.hdb", n, std::process::id());
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_file(format!("{}-wal", path));

            let mut conn = Connection::open(&path).unwrap();
            conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY, v TEXT)").unwrap();

            let cols = vec![
                (0..n as i64).map(|i| engramdb::Value::Int64(i)).collect(),
                (0..n as i64).map(|i| engramdb::Value::Varchar(format!("row-{}", i))).collect(),
            ];

            let t0 = Instant::now();
            conn.import_columns("t", cols).unwrap();
            times.push(t0.elapsed());
            conn.close().unwrap();
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_file(format!("{}-wal", path));
        }
        let m = median(times);
        println!("  {} 行: {}", fmt_rate(m, n), fmt_rate(m, n));
    }

    println!();

    // ==================== 3. SELECT * 扫描扩展性 ====================
    println!("━━━ 3. SELECT * 扫描扩展性 ━━━");

    // 准备数据
    let paths: Vec<String> = SCALES
        .iter()
        .map(|&n| {
            let path = format!("/tmp/scaling_scan_{}.hdb", n);
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_file(format!("{}-wal", path));
            let mut conn = Connection::open(&path).unwrap();
            conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY, v TEXT)").unwrap();
            for i in 0..n {
                conn.execute(&format!("INSERT INTO t VALUES ({}, 'row-{}')", i, i)).unwrap();
            }
            path
        })
        .collect();

    for (&n, path) in SCALES.iter().zip(&paths) {
        let mut times = Vec::new();
        for _ in 0..ITERS {
            let mut conn = Connection::open(path).unwrap();
            let t0 = Instant::now();
            let _r = conn.execute("SELECT * FROM t").unwrap();
            times.push(t0.elapsed());
        }
        let m = median(times);
        println!("  {} 行: {}", fmt_rate(m, n), fmt_rate(m, n));
    }

    println!();

    // ==================== 4. WHERE 等值查询扩展性 ====================
    println!("━━━ 4. WHERE 等值查询扩展性 (100 次查询) ━━━");
    for (&n, path) in SCALES.iter().zip(&paths) {
        let mut times = Vec::new();
        for _ in 0..ITERS {
            let mut conn = Connection::open(path).unwrap();
            let t0 = Instant::now();
            for i in 0..100 {
                let _r = conn.execute(&format!("SELECT * FROM t WHERE id = {}", i * (n / 100))).unwrap();
            }
            times.push(t0.elapsed());
        }
        let m = median(times) / 100;
        println!("  {} 行: {:?}/次", fmt_rate(m, 1), m);
    }

    println!();

    // ==================== 5. COUNT(*) 扩展性 ====================
    println!("━━━ 5. COUNT(*) 扩展性 ━━━");
    for (&n, path) in SCALES.iter().zip(&paths) {
        let mut times = Vec::new();
        for _ in 0..ITERS {
            let mut conn = Connection::open(path).unwrap();
            let t0 = Instant::now();
            let _r = conn.execute("SELECT COUNT(*) FROM t").unwrap();
            times.push(t0.elapsed());
        }
        let m = median(times);
        println!("  {} 行: {:?}", n, m);
    }

    println!();

    // ==================== 6. 文件大小对比 ====================
    println!("━━━ 6. 文件大小对比 ━━━");
    for (&n, path) in SCALES.iter().zip(&paths) {
        let meta = std::fs::metadata(path).unwrap();
        println!(
            "  {} 行: {}",
            fmt_size(meta.len() as usize),
            fmt_size(meta.len() as usize)
        );
    }

    println!();

    // ==================== 7. 内存使用 ====================
    println!("━━━ 7. 内存使用 (估算) ━━━");
    for &n in SCALES {
        let row_size = 8 + 10 + 8; // INT64 + TEXT("row-NNN") + overhead
        let memory = n * row_size;
        println!("  {} 行: {}", fmt_size(memory), fmt_size(memory));
    }

    // 清理
    for path in &paths {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(format!("{}-wal", path));
    }
}
