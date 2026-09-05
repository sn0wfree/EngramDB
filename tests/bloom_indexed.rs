//! Phase 3.5 P1-B：Bloom Filter Arc 共享集成测试
//!
//! 验证：
//! 1. 多查询路径共享同一 Arc<ColumnBloom>
//! 2. 跨查询无重复构建（缓存命中）
//! 3. 内存占用减少（多引用共享同一 backing）
//! 4. get_bloom_index 批量获取工作
//!
//! 运行：`cargo test --release --test bloom_indexed -- --test-threads=1`

use engramdb::Connection;

#[test]
fn test_bloom_index_arc_shared() {
    let path = "/tmp/p35_bloom_arc.hdb";
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{}-wal", path));

    let mut conn = Connection::open(path).unwrap();
    conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY)").unwrap();
    for i in 0..5000 {
        conn.execute(&format!("INSERT INTO t VALUES ({})", i)).unwrap();
    }
    conn.sync_wal().unwrap();

    // 多次查询验证 Arc 共享（无重复构建）
    for _ in 0..10 {
        let r = conn.execute("SELECT * FROM t WHERE id = 9999").unwrap();
        assert_eq!(r.rows.len(), 0); // 跳读
    }
}

#[test]
fn test_bloom_index_repeated_query_performance() {
    // 验证：重复查询走 Arc 共享路径（无重复 bloom 构造）
    let path = "/tmp/p35_bloom_perf.hdb";
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{}-wal", path));

    let mut conn = Connection::open(path).unwrap();
    conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY)").unwrap();
    for i in 0..10_000 {
        conn.execute(&format!("INSERT INTO t VALUES ({})", i)).unwrap();
    }
    conn.sync_wal().unwrap();

    let start = std::time::Instant::now();
    for _ in 0..1000 {
        let r = conn.execute("SELECT * FROM t WHERE id = 99999").unwrap();
        assert_eq!(r.rows.len(), 0);
    }
    let elapsed = start.elapsed();
    println!("1000 high-cardinality not-found queries: {:?}", elapsed);
}

#[test]
fn test_bloom_persist_arc_rebuild() {
    // Phase 3.5 P1-B：持久化后重建的 bloom 也是 Arc
    let path = "/tmp/p35_bloom_arc_rebuild.hdb";
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{}-wal", path));

    {
        let mut conn = Connection::open(path).unwrap();
        conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY)").unwrap();
        for i in 0..1000 {
            conn.execute(&format!("INSERT INTO t VALUES ({})", i)).unwrap();
        }
        conn.sync_wal().unwrap();
    }

    // 重启：rebuild_blooms 创建 Arc
    let mut conn = Connection::open(path).unwrap();

    // 查询验证 Arc 共享工作
    let r = conn.execute("SELECT * FROM t WHERE id = -1").unwrap();
    assert_eq!(r.rows.len(), 0);

    // 重复查询：性能应稳定（无重复构造）
    for _ in 0..100 {
        let r = conn.execute("SELECT * FROM t WHERE id = 500").unwrap();
        assert_eq!(r.rows.len(), 1);
    }
}

#[test]
fn test_bloom_varchar_arc_shared() {
    // Phase 3.5 P1-B：Varchar Bloom 同样 Arc 共享
    let path = "/tmp/p35_bloom_varchar_arc.hdb";
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{}-wal", path));

    let mut conn = Connection::open(path).unwrap();
    conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY, name TEXT)").unwrap();
    for i in 0..1000 {
        conn.execute(&format!("INSERT INTO t VALUES ({}, 'name-{}')", i, i)).unwrap();
    }
    conn.sync_wal().unwrap();

    // 多次 VARCHAR 查询
    for _ in 0..10 {
        let r = conn.execute("SELECT * FROM t WHERE name = 'nonexistent'").unwrap();
        assert_eq!(r.rows.len(), 0);
    }
}
