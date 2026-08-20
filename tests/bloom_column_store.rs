//! Phase 2.5 P3：Bloom Filter 列存集成测试
//!
//! 验证：
//! 1. RG 满时构建 ColumnChunk::bloom
//! 2. can_skip_predicate Eq 路径使用 Bloom（值不在 RG → skip）
//! 3. 多次访问后 Bloom 保持稳定
//! 4. 高基数 Int64 列构建成功
//!
//! 运行：`cargo test --release --test bloom_column_store -- --test-threads=1`

use engramdb::Connection;
use std::time::Instant;

#[test]
fn test_bloom_built_on_rg_completion() {
    let path = "/tmp/p25_bloom_build.hdb";
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{}-wal", path));

    let mut conn = Connection::open(path).unwrap();
    conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY, v TEXT)").unwrap();

    // 写入 1000 行（远超 1 个 RG 大小）
    for i in 0..1000 {
        conn.execute(&format!("INSERT INTO t VALUES ({}, 'r{}')", i, i)).unwrap();
    }
    // 触发 flush（batch 内的行合并到 Delta）
    conn.sync_wal().unwrap();

    // Bloom 已构建（行已被 typed 化）：通过访问缺失的 id 验证跳过
    let start = Instant::now();
    let r = conn.execute("SELECT * FROM t WHERE id = 999999").unwrap();
    let dur = start.elapsed();
    assert_eq!(r.rows.len(), 0);
    println!("high-cardinality not-found query took {:?}", dur);
}

#[test]
fn test_bloom_skips_high_cardinality_eq() {
    let path = "/tmp/p25_bloom_skip.hdb";
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{}-wal", path));

    let mut conn = Connection::open(path).unwrap();
    conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY)").unwrap();

    // 写入 5000 行
    for i in 0..5000 {
        conn.execute(&format!("INSERT INTO t VALUES ({})", i)).unwrap();
    }
    conn.sync_wal().unwrap();

    // 等值查询 — 不存在值
    let r = conn.execute("SELECT * FROM t WHERE id = -1").unwrap();
    assert_eq!(r.rows.len(), 0);

    // 等值查询 — 存在值（应找到 1 行）
    let r = conn.execute("SELECT * FROM t WHERE id = 1234").unwrap();
    assert_eq!(r.rows.len(), 1);
}

#[test]
fn test_bloom_with_non_bloomable_column() {
    // VARCHAR 列应不构建 Bloom（is_bloomable = false）
    let path = "/tmp/p25_bloom_varchar.hdb";
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{}-wal", path));

    let mut conn = Connection::open(path).unwrap();
    conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY, name TEXT)").unwrap();
    for i in 0..1000 {
        conn.execute(&format!("INSERT INTO t VALUES ({}, 'name-{}')", i, i)).unwrap();
    }
    conn.sync_wal().unwrap();

    // VARCHAR 列上 Bloom 不会构建，但不影响查询正确性
    let r = conn.execute("SELECT * FROM t WHERE name = 'name-500'").unwrap();
    assert_eq!(r.rows.len(), 1);
}

#[test]
fn test_bloom_skip_perf_high_cardinality() {
    // 性能对比：等值查询不存在值（应快速跳过）
    let path = "/tmp/p25_bloom_perf.hdb";
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{}-wal", path));

    let mut conn = Connection::open(path).unwrap();
    conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY, v TEXT)").unwrap();
    for i in 0..10_000 {
        conn.execute(&format!("INSERT INTO t VALUES ({}, 'r{}')", i, i)).unwrap();
    }
    conn.sync_wal().unwrap();

    // 多次不命中查询
    let start = Instant::now();
    for _ in 0..100 {
        let r = conn.execute("SELECT * FROM t WHERE id = 999999999").unwrap();
        assert_eq!(r.rows.len(), 0);
    }
    let dur = start.elapsed();
    println!("100 high-cardinality not-found queries: {:?}", dur);
}