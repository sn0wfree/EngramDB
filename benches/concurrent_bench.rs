//! 多线程并发读写测试
//!
//! 测试多线程同时读写场景下的性能和正确性。
//!
//! 运行：`cargo bench --bench concurrent_bench`

use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};
use engramdb::Connection;

const N_ROWS: usize = 100_000;
const N_THREADS: usize = 4;
const N_OPS_PER_THREAD: usize = 1_000;
const ITERS: usize = 3;

fn median(mut samples: Vec<Duration>) -> Duration {
    samples.sort();
    samples[samples.len() / 2]
}

fn fmt_rate(d: Duration, n: usize) -> String {
    format!("{:.0} 行/秒 ({:.1} ms)", n as f64 / d.as_secs_f64(), d.as_secs_f64() * 1000.0)
}

fn main() {
    println!("=== 多线程并发读写测试 ===");
    println!("线程数: {}, 每线程操作数: {}, 数据规模: {} 行", N_THREADS, N_OPS_PER_THREAD, N_ROWS);
    println!();

    // ==================== 1. 并发写入测试 ====================
    println!("━━━ 1. 并发写入测试 ({:} 线程) ━━━", N_THREADS);

    let mut times = Vec::new();
    for _ in 0..ITERS {
        let path = format!("/tmp/concurrent_write_{}.hdb", std::process::id());
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{}-wal", path));

        // 先创建表
        {
            let mut conn = Connection::open(&path).unwrap();
            conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY, v TEXT)").unwrap();
        }

        // 启动线程
        let barrier = Arc::new(Barrier::new(N_THREADS));
        let path = Arc::new(path);

        let t0 = Instant::now();
        let handles: Vec<_> = (0..N_THREADS).map(|thread_id| {
            let path = Arc::clone(&path);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let mut conn = Connection::open(&*path).unwrap();
                barrier.wait();

                // 每个线程使用不同的 ID 范围避免冲突
                let base = thread_id * 1_000_000 + 1_000_000;
                for i in 0..N_OPS_PER_THREAD {
                    let id = base + i;
                    conn.execute(&format!("INSERT INTO t VALUES ({}, 'thread-{}-row-{}')", id, thread_id, i)).unwrap();
                }
            })
        }).collect();

        for h in handles {
            h.join().unwrap();
        }
        times.push(t0.elapsed());
    }
    let m = median(times);
    println!("  串行写入:  {}", fmt_rate(m, N_THREADS * N_OPS_PER_THREAD));

    println!();

    // ==================== 2. 并发读取测试 ====================
    println!("━━━ 2. 并发读取测试 ({:} 线程) ━━━", N_THREADS);

    // 准备数据
    let read_path = "/tmp/concurrent_read.hdb";
    let _ = std::fs::remove_file(read_path);
    let _ = std::fs::remove_file(format!("{}-wal", read_path));
    {
        let mut conn = Connection::open(read_path).unwrap();
        conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY, v TEXT)").unwrap();
        for i in 0..N_ROWS {
            conn.execute(&format!("INSERT INTO t VALUES ({}, 'row-{}')", i, i)).unwrap();
        }
    }

    let mut times = Vec::new();
    for _ in 0..ITERS {
        let read_path = read_path.to_string();
        let barrier = Arc::new(Barrier::new(N_THREADS));

        let t0 = Instant::now();
        let handles: Vec<_> = (0..N_THREADS).map(|_| {
            let path = read_path.clone();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let mut conn = Connection::open(&path).unwrap();
                barrier.wait();

                for _ in 0..N_OPS_PER_THREAD {
                    let _r = conn.execute("SELECT * FROM t").unwrap();
                }
            })
        }).collect();

        for h in handles {
            h.join().unwrap();
        }
        times.push(t0.elapsed());
    }
    let m = median(times);
    println!("  串行读取:  {}", fmt_rate(m, N_THREADS * N_OPS_PER_THREAD));

    println!();

    // ==================== 3. 并发混合读写测试 ====================
    println!("━━━ 3. 并发混合读写测试 (2 写 + 2 读) ━━━");

    // 准备数据
    let mixed_path = "/tmp/concurrent_mixed.hdb";
    let _ = std::fs::remove_file(mixed_path);
    let _ = std::fs::remove_file(format!("{}-wal", mixed_path));
    {
        let mut conn = Connection::open(mixed_path).unwrap();
        conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY, v TEXT)").unwrap();
        for i in 0..1000 {
            conn.execute(&format!("INSERT INTO t VALUES ({}, 'row-{}')", i, i)).unwrap();
        }
    }

    let mut times = Vec::new();
    for iteration in 0..ITERS {
        let mixed_path = mixed_path.to_string();
        let barrier = Arc::new(Barrier::new(N_THREADS));
        let iter_offset = iteration as u64 * 100_000_000; // 每次迭代偏移不同

        let t0 = Instant::now();
        let handles: Vec<_> = (0..N_THREADS).map(|thread_id| {
            let path = mixed_path.clone();
            let barrier = Arc::clone(&barrier);
            let iter = N_OPS_PER_THREAD;
            let base_id = thread_id as u64 * 50_000_000 + 100_000_000 + iter_offset;
            thread::spawn(move || {
                let mut conn = Connection::open(&path).unwrap();
                barrier.wait();

                if thread_id < 2 {
                    // 写线程
                    for i in 0..iter {
                        let id = base_id + i as u64;
                        conn.execute(&format!("INSERT INTO t VALUES ({}, 'thread-{}-row-{}')", id, thread_id, i)).unwrap();
                    }
                } else {
                    // 读线程
                    for _ in 0..iter {
                        let _r = conn.execute("SELECT * FROM t").unwrap();
                    }
                }
            })
        }).collect();

        for h in handles {
            h.join().unwrap();
        }
        times.push(t0.elapsed());
    }
    let m = median(times);
    println!("  混合读写:  {}", fmt_rate(m, N_THREADS * N_OPS_PER_THREAD));

    println!();

    // ==================== 4. 并发 WHERE 查询 ====================
    println!("━━━ 4. 并发 WHERE 查询测试 ({:} 线程) ━━━", N_THREADS);

    let where_path = "/tmp/concurrent_where.hdb";
    let _ = std::fs::remove_file(where_path);
    let _ = std::fs::remove_file(format!("{}-wal", where_path));
    {
        let mut conn = Connection::open(where_path).unwrap();
        conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY, v TEXT)").unwrap();
        for i in 0..N_ROWS {
            conn.execute(&format!("INSERT INTO t VALUES ({}, 'row-{}')", i, i)).unwrap();
        }
    }

    let mut times = Vec::new();
    for _ in 0..ITERS {
        let where_path = where_path.to_string();
        let barrier = Arc::new(Barrier::new(N_THREADS));

        let t0 = Instant::now();
        let handles: Vec<_> = (0..N_THREADS).map(|_| {
            let path = where_path.clone();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let mut conn = Connection::open(&path).unwrap();
                barrier.wait();

                for i in 0..N_OPS_PER_THREAD {
                    let id = i * (N_ROWS / N_OPS_PER_THREAD);
                    let _r = conn.execute(&format!("SELECT * FROM t WHERE id = {}", id)).unwrap();
                }
            })
        }).collect();

        for h in handles {
            h.join().unwrap();
        }
        times.push(t0.elapsed());
    }
    let m = median(times);
    println!("  WHERE 查询: {}", fmt_rate(m, N_THREADS * N_OPS_PER_THREAD));

    println!();

    // ==================== 5. 并发 COUNT(*) 测试 ====================
    println!("━━━ 5. 并发 COUNT(*) 测试 ({:} 线程) ━━━", N_THREADS);

    let count_path = "/tmp/concurrent_count.hdb";
    let _ = std::fs::remove_file(count_path);
    let _ = std::fs::remove_file(format!("{}-wal", count_path));
    {
        let mut conn = Connection::open(count_path).unwrap();
        conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY, v TEXT)").unwrap();
        for i in 0..N_ROWS {
            conn.execute(&format!("INSERT INTO t VALUES ({}, 'row-{}')", i, i)).unwrap();
        }
    }

    let mut times = Vec::new();
    for _ in 0..ITERS {
        let count_path = count_path.to_string();
        let barrier = Arc::new(Barrier::new(N_THREADS));

        let t0 = Instant::now();
        let handles: Vec<_> = (0..N_THREADS).map(|_| {
            let path = count_path.clone();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let mut conn = Connection::open(&path).unwrap();
                barrier.wait();

                for _ in 0..N_OPS_PER_THREAD {
                    let _r = conn.execute("SELECT COUNT(*) FROM t").unwrap();
                }
            })
        }).collect();

        for h in handles {
            h.join().unwrap();
        }
        times.push(t0.elapsed());
    }
    let m = median(times);
    println!("  COUNT(*):  {}", fmt_rate(m, N_THREADS * N_OPS_PER_THREAD));

    println!();

    // ==================== 总结 ====================
    println!("━━━ 总结 ━━━");
    println!("  并发写入: 每线程独立表，串行写入");
    println!("  并发读取: 共享表，串行读取");
    println!("  混合读写: 2 写 + 2 读，读写交错");
    println!("  WHERE 查询: 每线程独立主键查询");
    println!("  COUNT(*): 每线程独立全表计数");

    // 清理
    let _ = std::fs::remove_file(read_path);
    let _ = std::fs::remove_file(format!("{}-wal", read_path));
    let _ = std::fs::remove_file(mixed_path);
    let _ = std::fs::remove_file(format!("{}-wal", mixed_path));
    let _ = std::fs::remove_file(where_path);
    let _ = std::fs::remove_file(format!("{}-wal", where_path));
    let _ = std::fs::remove_file(count_path);
    let _ = std::fs::remove_file(format!("{}-wal", count_path));
}