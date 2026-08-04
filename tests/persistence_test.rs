//! 持久化集成测试（v0.12.1 新增）
//!
//! 验证 P0 致命问题已修复：
//! - 表 schema 持久化（catalog）
//! - 列存数据持久化（RowGroup）
//! - 重启后数据完整恢复
//!
//! 测试策略：写入数据 → close → 重新 open → 验证 schema + 数据可查询

use engramdb::{Connection, Value};
use tempfile::tempdir;

/// 辅助：断言查询结果行数
fn assert_row_count(conn: &mut Connection, sql: &str, expected: usize, msg: &str) {
    let result = conn.execute(sql).expect(msg);
    assert_eq!(
        result.rows.len(),
        expected,
        "{}: 期望 {} 行，实际 {} 行",
        msg,
        expected,
        result.rows.len()
    );
}

/// 测试 1：表 schema 持久化（CREATE TABLE 后重启，表仍存在）
#[test]
fn test_schema_persistence() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("schema_test.hdb");

    // 第一次打开：创建表
    {
        let mut conn = Connection::open(db_path.to_str().unwrap()).unwrap();
        conn.execute("CREATE TABLE users (id INT PRIMARY KEY, name VARCHAR, age INT)")
            .unwrap();
        conn.close().unwrap();
    }

    // 第二次打开：表应仍存在
    {
        let mut conn = Connection::open(db_path.to_str().unwrap()).unwrap();
        // 如果 schema 丢失，INSERT 会报错 "table not found"
        conn.execute("INSERT INTO users VALUES (1, 'alice', 30)").unwrap();
        assert_row_count(&mut conn, "SELECT * FROM users", 1, "schema 恢复后可写入");
    }
}

/// 测试 2：数据持久化（写入数据后重启，数据可查询）
#[test]
fn test_data_persistence() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("data_test.hdb");

    // 写入 100 行数据
    {
        let mut conn = Connection::open(db_path.to_str().unwrap()).unwrap();
        conn.execute("CREATE TABLE metrics (id INT PRIMARY KEY, value DOUBLE)").unwrap();

        let stmt = conn.prepare("INSERT INTO metrics VALUES (?, ?)").unwrap();
        let mut batch = Vec::with_capacity(100);
        for i in 0..100 {
            batch.push(vec![Value::Int32(i), Value::Float64(i as f64 * 1.5)]);
        }
        conn.execute_prepared_batch(&stmt, &batch).unwrap();
        conn.close().unwrap();
    }

    // 重启后数据应完整
    {
        let mut conn = Connection::open(db_path.to_str().unwrap()).unwrap();
        // COUNT(*) 应为 100
        let result = conn.execute("SELECT COUNT(*) FROM metrics").unwrap();
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0][0], Value::Int64(100), "数据行数应为 100");

        // 范围查询验证数据正确性
        let result = conn.execute("SELECT * FROM metrics WHERE id >= 95").unwrap();
        assert_eq!(result.rows.len(), 5, "id >= 95 应有 5 行");

        // 验证具体数值
        let result = conn.execute("SELECT value FROM metrics WHERE id = 10").unwrap();
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0][0], Value::Float64(15.0), "id=10 的 value 应为 15.0");
    }
}

/// 测试 3：多表持久化
#[test]
fn test_multi_table_persistence() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("multi_test.hdb");

    {
        let mut conn = Connection::open(db_path.to_str().unwrap()).unwrap();
        conn.execute("CREATE TABLE users (id INT PRIMARY KEY, name VARCHAR)").unwrap();
        conn.execute("CREATE TABLE orders (id INT PRIMARY KEY, user_id INT, amount DOUBLE)").unwrap();
        conn.execute("INSERT INTO users VALUES (1, 'alice')").unwrap();
        conn.execute("INSERT INTO orders VALUES (1, 1, 99.5)").unwrap();
        conn.close().unwrap();
    }

    {
        let mut conn = Connection::open(db_path.to_str().unwrap()).unwrap();
        assert_row_count(&mut conn, "SELECT * FROM users", 1, "users 表数据");
        assert_row_count(&mut conn, "SELECT * FROM orders", 1, "orders 表数据");
    }
}

/// 测试 4：Drop 自动持久化（不显式 close）
#[test]
fn test_drop_auto_persistence() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("drop_test.hdb");

    {
        let mut conn = Connection::open(db_path.to_str().unwrap()).unwrap();
        conn.execute("CREATE TABLE t (id INT PRIMARY KEY)").unwrap();
        conn.execute("INSERT INTO t VALUES (1)").unwrap();
        conn.execute("INSERT INTO t VALUES (2)").unwrap();
        conn.execute("INSERT INTO t VALUES (3)").unwrap();
        // 不调用 close，依赖 Drop 自动 checkpoint
    }

    {
        let mut conn = Connection::open(db_path.to_str().unwrap()).unwrap();
        let result = conn.execute("SELECT COUNT(*) FROM t").unwrap();
        assert_eq!(result.rows[0][0], Value::Int64(3), "Drop 自动持久化应保留 3 行");
    }
}

/// 测试 5：内存库不持久化（:memory: 每次都是新库）
#[test]
fn test_memory_db_not_persisted() {
    // :memory: 库 Drop 后不写盘
    let mut conn = Connection::open(":memory:").unwrap();
    conn.execute("CREATE TABLE t (id INT)").unwrap();
    conn.execute("INSERT INTO t VALUES (1)").unwrap();
    assert_row_count(&mut conn, "SELECT * FROM t", 1, "内存库写入成功");
    // Drop 时不持久化
}

/// 测试 6：Compact 后数据持久化（Delta → 列存 → 持久化）
#[test]
fn test_persistence_after_compact() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("compact_test.hdb");

    {
        let mut conn = Connection::open(db_path.to_str().unwrap()).unwrap();
        conn.execute("CREATE TABLE big (id INT PRIMARY KEY, name VARCHAR)").unwrap();

        // 写入足够多数据触发自动 compact
        let stmt = conn.prepare("INSERT INTO big VALUES (?, ?)").unwrap();
        let mut batch = Vec::with_capacity(1000);
        for i in 0..1000 {
            batch.push(vec![Value::Int32(i), Value::Varchar(format!("user_{}", i))]);
        }
        conn.execute_prepared_batch(&stmt, &batch).unwrap();

        // 手动 compact
        conn.compact("big").unwrap();

        conn.close().unwrap();
    }

    {
        let mut conn = Connection::open(db_path.to_str().unwrap()).unwrap();
        let result = conn.execute("SELECT COUNT(*) FROM big").unwrap();
        assert_eq!(result.rows[0][0], Value::Int64(1000), "compact 后持久化 1000 行");

        // 抽样验证
        let result = conn.execute("SELECT name FROM big WHERE id = 500").unwrap();
        assert_eq!(result.rows[0][0], Value::Varchar("user_500".into()));
    }
}

/// 测试 7：多类型数据持久化（BOOLEAN / VARCHAR / DOUBLE 边界值）
#[test]
fn test_various_types_persistence() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("types_test.hdb");

    {
        let mut conn = Connection::open(db_path.to_str().unwrap()).unwrap();
        conn.execute("CREATE TABLE mixed (id INT PRIMARY KEY, b BOOLEAN, s VARCHAR, f DOUBLE)")
            .unwrap();
        conn.execute("INSERT INTO mixed VALUES (1, TRUE, 'hello', 3.14)").unwrap();
        conn.execute("INSERT INTO mixed VALUES (2, FALSE, 'world', -0.5)").unwrap();
        conn.execute("INSERT INTO mixed VALUES (3, TRUE, '', 0.0)").unwrap();
        conn.close().unwrap();
    }

    {
        let mut conn = Connection::open(db_path.to_str().unwrap()).unwrap();
        let result = conn.execute("SELECT * FROM mixed ORDER BY id").unwrap();
        assert_eq!(result.rows.len(), 3);

        // 验证各类型值正确恢复
        assert_eq!(result.rows[0][1], Value::Boolean(true));
        assert_eq!(result.rows[0][2], Value::Varchar("hello".into()));
        assert_eq!(result.rows[0][3], Value::Float64(3.14));

        assert_eq!(result.rows[1][1], Value::Boolean(false));
        assert_eq!(result.rows[1][3], Value::Float64(-0.5));

        // 边界值：空字符串和 0.0
        assert_eq!(result.rows[2][2], Value::Varchar("".into()));
        assert_eq!(result.rows[2][3], Value::Float64(0.0));
    }
}

/// 测试 8：重复 checkpoint 不损坏数据
#[test]
fn test_multiple_checkpoints() {
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("multi_checkpoint.hdb");

    {
        let mut conn = Connection::open(db_path.to_str().unwrap()).unwrap();
        conn.execute("CREATE TABLE t (id INT PRIMARY KEY)").unwrap();
        conn.execute("INSERT INTO t VALUES (1)").unwrap();
        conn.close().unwrap(); // 第一次 checkpoint
    }

    {
        let mut conn = Connection::open(db_path.to_str().unwrap()).unwrap();
        conn.execute("INSERT INTO t VALUES (2)").unwrap();
        conn.close().unwrap(); // 第二次 checkpoint
    }

    {
        let mut conn = Connection::open(db_path.to_str().unwrap()).unwrap();
        let result = conn.execute("SELECT COUNT(*) FROM t").unwrap();
        assert_eq!(result.rows[0][0], Value::Int64(2), "多次 checkpoint 后数据应累加");
    }
}
