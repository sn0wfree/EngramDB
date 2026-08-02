//! 集成测试

#[cfg(test)]
mod tests {
    use hybriddb::{Connection, Value};
    use tempfile::tempdir;

    fn setup_db() -> (Connection, tempfile::TempDir) {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("test.hdb");
        let conn = Connection::open(db_path.to_str().unwrap()).unwrap();
        (conn, dir)
    }

    #[test]
    fn test_create_table_and_insert() {
        let (mut conn, _dir) = setup_db();

        // 创建表
        let result = conn.execute(
            "CREATE TABLE users (id INT PRIMARY KEY, name VARCHAR, age INT)"
        ).unwrap();
        assert!(result.rows_affected == 0);

        // 插入数据
        let result = conn.execute(
            "INSERT INTO users VALUES (1, 'Alice', 30), (2, 'Bob', 25), (3, 'Charlie', 35)"
        ).unwrap();
        assert_eq!(result.rows_affected, 3);

        // 查询
        let result = conn.execute("SELECT * FROM users").unwrap();
        assert_eq!(result.rows.len(), 3);
        assert_eq!(result.columns.len(), 3);
    }

    #[test]
    fn test_select_with_where() {
        let (mut conn, _dir) = setup_db();

        conn.execute("CREATE TABLE t (id INT, val VARCHAR)").unwrap();
        conn.execute("INSERT INTO t VALUES (1, 'a'), (2, 'b'), (3, 'a')").unwrap();

        let result = conn.execute("SELECT id, val FROM t WHERE id > 1").unwrap();
        assert_eq!(result.rows.len(), 2);
    }

    #[test]
    fn test_select_with_limit() {
        let (mut conn, _dir) = setup_db();

        conn.execute("CREATE TABLE t (id INT)").unwrap();
        conn.execute("INSERT INTO t VALUES (1), (2), (3), (4), (5)").unwrap();

        let result = conn.execute("SELECT * FROM t LIMIT 2").unwrap();
        assert_eq!(result.rows.len(), 2);
    }

    #[test]
    fn test_transaction_commands() {
        let (mut conn, _dir) = setup_db();

        let result = conn.execute("BEGIN").unwrap();
        assert!(!result.rows.is_empty());

        let result = conn.execute("COMMIT").unwrap();
        assert!(!result.rows.is_empty());
    }

    #[test]
    fn test_multiple_tables() {
        let (mut conn, _dir) = setup_db();

        conn.execute("CREATE TABLE t1 (id INT)").unwrap();
        conn.execute("CREATE TABLE t2 (name VARCHAR)").unwrap();

        conn.execute("INSERT INTO t1 VALUES (1), (2)").unwrap();
        conn.execute("INSERT INTO t2 VALUES ('a'), ('b'), ('c')").unwrap();

        let r1 = conn.execute("SELECT * FROM t1").unwrap();
        let r2 = conn.execute("SELECT * FROM t2").unwrap();

        assert_eq!(r1.rows.len(), 2);
        assert_eq!(r2.rows.len(), 3);
    }
}
