//! M1-8 Bloom Filter（P3.5）验收微基准
//!
//! 运行：`cargo bench --bench p35_bloom_bench`
//!
//! 场景：等值查询命中"范围内但不存在"的值（MinMax 无法跳过，
//! 无 Bloom 时需全扫逐行匹配）——Bloom 将整组判定为 O(1) 跳过。

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

fn main() {
    println!("=== M1-8 Bloom Filter 验收微基准 ===");
    std::fs::remove_file("/tmp/p35_bloom.hdb").ok();
    std::fs::remove_file("/tmp/p35_bloom.hdb-wal").ok();

    let mut conn = Connection::open("/tmp/p35_bloom.hdb").unwrap();
    conn.execute("CREATE TABLE t (id INT, v TEXT)").unwrap();

    // 1M 行稀疏 id（挖空 40-60% 段——范围内缺值）
    let batch: Vec<Vec<Value>> = (0..N)
        .filter(|i| !(N / 5 * 2..N / 5 * 3).contains(i))
        .map(|i| vec![Value::Int64(i as i64), Value::Varchar(format!("v{}", i % 1000))])
        .collect();
    let stmt = conn.prepare("INSERT INTO t VALUES (?, ?)").unwrap();
    conn.execute_prepared_batch(&stmt, &batch).unwrap();
    let inserted = batch.len();
    println!("插入 {} 行（挖空 {} 行）", inserted, N - inserted);

    // compact → 数据进列存 row group（Bloom 作用对象）
    conn.compact_all().unwrap();
    conn.close().unwrap();
    conn = Connection::open("/tmp/p35_bloom.hdb").unwrap();

    // 等值查询：范围内不存在的值（无 Bloom 需全扫逐行匹配）
    let mut samples = Vec::new();
    for _ in 0..ITERS {
        let t0 = Instant::now();
        let r = conn.execute("SELECT COUNT(*) FROM t WHERE id = 500000").unwrap();
        assert_eq!(r.rows[0][0], Value::Int64(0));
        samples.push(t0.elapsed());
    }
    let miss = median(samples);
    println!("等值 miss 查询（范围内不存在，{} 行）: {:?}", inserted, miss);

    // 等值查询：存在的值
    let mut samples = Vec::new();
    for _ in 0..ITERS {
        let t0 = Instant::now();
        let r = conn.execute("SELECT COUNT(*) FROM t WHERE id = 700000").unwrap();
        assert_eq!(r.rows[0][0], Value::Int64(1));
        samples.push(t0.elapsed());
    }
    let hit = median(samples);
    println!("等值 hit 查询（存在值）: {:?}", hit);
}
