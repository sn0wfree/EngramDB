// M1-8 Bloom Filter（P3.5）集成测试：等值跳读正确性（零假阴性）
// + 跨类型等值 + 重启后重建 + 写入后失效

#[test]
fn test_bloom_eq_skip_correctness() {
    let mut conn = super::Connection::open(":memory:").unwrap();
    // 稀疏 ID：1..=1000 但挖掉中间段（500 个缺值在 [min,max] 范围内）
    conn.execute("CREATE TABLE sparse (id INT PRIMARY KEY, v TEXT)").unwrap();
    let stmt = conn.prepare("INSERT INTO sparse VALUES (?, ?)").unwrap();
    let mut batch = Vec::new();
    for i in 0..1000i64 {
        if i >= 400 && i < 600 {
            continue; // 挖空 400-599
        }
        batch.push(vec![Value::Int64(i), Value::Varchar(format!("v{}", i))]);
    }
    conn.execute_prepared_batch(&stmt, &batch).unwrap();

    // 存在的值全部命中
    for id in [0i64, 399, 600, 999] {
        let r = conn.execute(&format!("SELECT v FROM sparse WHERE id = {}", id)).unwrap();
        assert_eq!(r.rows.len(), 1, "id={} 应命中", id);
        assert_eq!(r.rows[0][0], Value::Varchar(format!("v{}", id)));
    }
    // 范围内不存在的值 → 0 行（Bloom 整块跳过或行级筛选，结果必须正确）
    for id in [400i64, 500, 599] {
        let r = conn.execute(&format!("SELECT COUNT(*) FROM sparse WHERE id = {}", id)).unwrap();
        assert_eq!(r.rows[0][0], Value::Int64(0), "id={} 不应命中", id);
    }
    // 范围外的值 → 0 行
    let r = conn.execute("SELECT COUNT(*) FROM sparse WHERE id = 99999").unwrap();
    assert_eq!(r.rows[0][0], Value::Int64(0));
    // 全量 COUNT 校验（行级结果与 Bloom 跳过路径一致）
    let r = conn.execute("SELECT COUNT(*) FROM sparse").unwrap();
    assert_eq!(r.rows[0][0], Value::Int64(800));
}

#[test]
fn test_bloom_cross_type_eq() {
    let mut conn = super::Connection::open(":memory:").unwrap();
    // INT 列（Int32 语义）+ Int64 字面量等值（P2.5 归一化 + Bloom 探测）
    conn.execute("CREATE TABLE t (id INT, ts TIMESTAMP, v TEXT)").unwrap();
    for i in 0..100i32 {
        conn.execute(&format!("INSERT INTO t VALUES ({}, {}, 'x{}')", i, i * 1000, i)).unwrap();
    }
    // Int64 字面量查 Int32 列（既有归一化，Bloom 不假阴性）
    let r = conn.execute("SELECT v FROM t WHERE id = 42").unwrap();
    assert_eq!(r.rows[0][0], Value::Varchar("x42".into()));
    // Int64 字面量查 Timestamp 列
    let r = conn.execute("SELECT COUNT(*) FROM t WHERE ts = 42000").unwrap();
    assert_eq!(r.rows[0][0], Value::Int64(1));
    // 范围内不存在 → 0
    let r = conn.execute("SELECT COUNT(*) FROM t WHERE ts = 42001").unwrap();
    assert_eq!(r.rows[0][0], Value::Int64(0));
}

#[test]
fn test_bloom_after_restart_and_write() {
    let path = format!("/tmp/engramdb_bloom_{}.hdb", std::process::id());
    std::fs::remove_file(&path).ok();
    std::fs::remove_file(format!("{}-wal", path)).ok();
    {
        let mut conn = super::Connection::open(&path).unwrap();
        conn.execute("CREATE TABLE t (id INT, v TEXT)").unwrap();
        let stmt = conn.prepare("INSERT INTO t VALUES (?, ?)").unwrap();
        let mut batch = Vec::new();
        for i in 0..500i64 {
            if i == 250 {
                continue;
            }
            batch.push(vec![Value::Int64(i), Value::Varchar(format!("v{}", i))]);
        }
        conn.execute_prepared_batch(&stmt, &batch).unwrap();
        conn.close().unwrap();
    }
    {
        let mut conn = super::Connection::open(&path).unwrap();
        // 重启后（bloom 未落盘，惰性重建）等值查询正确
        let r = conn.execute("SELECT COUNT(*) FROM t WHERE id = 250").unwrap();
        assert_eq!(r.rows[0][0], Value::Int64(0), "重启后缺值仍为 0");
        let r = conn.execute("SELECT COUNT(*) FROM t WHERE id = 100").unwrap();
        assert_eq!(r.rows[0][0], Value::Int64(1), "重启后存在值命中");
        // 写入新值 → 可查（bloom 失效重建）
        conn.execute("INSERT INTO t VALUES (250, 'v250')").unwrap();
        let r = conn.execute("SELECT COUNT(*) FROM t WHERE id = 250").unwrap();
        assert_eq!(r.rows[0][0], Value::Int64(1), "写入后新值命中");
        conn.close().unwrap();
    }
    {
        let mut conn = super::Connection::open(&path).unwrap();
        // 再次重启：新值持久化后仍可查
        let r = conn.execute("SELECT COUNT(*) FROM t WHERE id = 250").unwrap();
        assert_eq!(r.rows[0][0], Value::Int64(1));
    }
    std::fs::remove_file(&path).ok();
    std::fs::remove_file(format!("{}-wal", path)).ok();
}
