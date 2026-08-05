//! M2 MemoryEngine 验收微基准
//!
//! 运行：`cargo bench --bench m2_memory_bench`
//!
//! 验收标准（文档 v1.0）：
//! - Memory 点查 < 1μs（对比 Columnar ~0.1ms）
//! - Memory 写入 < 1μs（对比 Columnar ~0.2ms）

use std::time::{Duration, Instant};
use engramdb::{Connection, Value};

const ITERS: usize = 7;
const N: usize = 100_000;

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

fn main() {
    println!("=== M2 MemoryEngine 验收微基准 ===");
    std::fs::remove_file("/tmp/m2_mem.hdb").ok();
    std::fs::remove_file("/tmp/m2_mem.hdb-wal").ok();

    // 建两张表：Columnar + Memory（同 schema）
    let mut conn = Connection::open("/tmp/m2_mem.hdb").unwrap();
    conn.execute("CREATE TABLE c (id INT PRIMARY KEY, v INT)").unwrap();
    conn.execute("CREATE TABLE m (id INT PRIMARY KEY, v INT) ENGINE = Memory").unwrap();

    // 批量灌入 N 行
    let mut sql = String::from("INSERT INTO m VALUES ");
    for i in 0..N {
        if i > 0 { sql.push(','); }
        sql.push_str(&format!("({}, {})", i, i));
    }
    conn.execute(&sql).unwrap();
    let mut sql = String::from("INSERT INTO c VALUES ");
    for i in 0..N {
        if i > 0 { sql.push(','); }
        sql.push_str(&format!("({}, {})", i, i));
    }
    conn.execute(&sql).unwrap();

    // 点查（主键等值，prepared 消除解析开销）
    let pm = conn.prepare("SELECT v FROM m WHERE id = ?").unwrap();
    let pc = conn.prepare("SELECT v FROM c WHERE id = ?").unwrap();
    let mut mem_samples = Vec::new();
    let mut col_samples = Vec::new();
    for _ in 0..ITERS {
        let start = Instant::now();
        for i in 0..2000 {
            conn.execute_prepared(&pm, &[Value::Int64(i)]).unwrap();
        }
        mem_samples.push(start.elapsed() / 2000);

        let start = Instant::now();
        for i in 0..2000 {
            conn.execute_prepared(&pc, &[Value::Int64(i)]).unwrap();
        }
        col_samples.push(start.elapsed() / 2000);
    }
    let mem_pt = median(mem_samples);
    let col_pt = median(col_samples);
    println!("点查 (主键等值):  Memory {}   Columnar {}   (目标 < 1µs)", fmt(mem_pt), fmt(col_pt));

    // 单行写入（非事务）
    let mut mem_samples = Vec::new();
    let mut col_samples = Vec::new();
    let mut next_id = N;
    for _ in 0..ITERS {
        let start = Instant::now();
        for _ in 0..2000 {
            conn.execute(&format!("INSERT INTO m VALUES ({}, 0)", next_id)).unwrap();
            next_id += 1;
        }
        mem_samples.push(start.elapsed() / 2000);

        let start = Instant::now();
        for _ in 0..2000 {
            conn.execute(&format!("INSERT INTO c VALUES ({}, 0)", next_id)).unwrap();
            next_id += 1;
        }
        col_samples.push(start.elapsed() / 2000);
    }
    let mem_w = median(mem_samples);
    let col_w = median(col_samples);
    println!("单行写入:        Memory {}   Columnar {}   (目标 < 1µs)", fmt(mem_w), fmt(col_w));

    // 全表扫描（N 行）
    let mut mem_samples = Vec::new();
    let mut col_samples = Vec::new();
    for _ in 0..ITERS {
        let start = Instant::now();
        let r = conn.execute("SELECT COUNT(*) FROM m").unwrap();
        let c = match &r.rows[0][0] { Value::Int64(v) => *v, _ => 0 };
        mem_samples.push(start.elapsed());
        assert!(c > 0);
        let start = Instant::now();
        let _ = conn.execute("SELECT COUNT(*) FROM c").unwrap();
        col_samples.push(start.elapsed());
    }
    println!("全表 COUNT({}):  Memory {}   Columnar {}", N, fmt(median(mem_samples)), fmt(median(col_samples)));

    // 引擎层点查（绕过 SQL 栈：lookup_primary_key + get_row_by_id）
    let mut mem_samples = Vec::new();
    let mut col_samples = Vec::new();
    for _ in 0..ITERS {
        let start = Instant::now();
        for i in 0..50_000 {
            let db = conn.database_mut();
            let rid = db.get_engine_table("m").unwrap().lookup_primary_key(&Value::Int64(i));
            let _ = db.get_engine_table_mut("m").unwrap().get_row_by_id(rid.unwrap()).unwrap();
        }
        mem_samples.push(start.elapsed() / 50_000);

        let start = Instant::now();
        for i in 0..50_000 {
            let db = conn.database_mut();
            let rid = db.get_engine_table("c").unwrap().lookup_primary_key(&Value::Int64(i));
            let _ = db.get_engine_table_mut("c").unwrap().get_row_by_id(rid.unwrap()).unwrap();
        }
        col_samples.push(start.elapsed() / 50_000);
    }
    println!("引擎层点查:      Memory {}   Columnar {}   (目标 < 1µs)",
        fmt(median(mem_samples)), fmt(median(col_samples)));

    conn.close().unwrap();
    std::fs::remove_file("/tmp/m2_mem.hdb").ok();
    std::fs::remove_file("/tmp/m2_mem.hdb-wal").ok();
    println!("=== M2 验收微基准结束 ===");
}
