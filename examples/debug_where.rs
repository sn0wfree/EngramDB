use engramdb::Connection;

fn main() {
    let mut conn = Connection::open("/tmp/debug_where.db").unwrap();
    conn.execute("CREATE TABLE t1 (id INT, category INT, value DOUBLE, name VARCHAR);").unwrap();
    conn.execute("INSERT INTO t1 VALUES (1, 10, 100.0, 'a'), (2, 20, 200.0, 'b');").unwrap();
    
    // 测试 SELECT *
    let r = conn.execute("SELECT * FROM t1;").unwrap();
    println!("SELECT * columns: {:?}", r.columns);
    println!("SELECT * rows: {}", r.rows.len());
    
    // 测试带 WHERE 的查询 - 用不同大小写
    let queries = vec![
        "SELECT id FROM t1 WHERE id > 0;",
        "SELECT id, value FROM t1 WHERE value > 50.0;",
        "SELECT id, VALUE FROM t1 WHERE VALUE > 50.0;",
    ];
    
    for q in queries {
        println!("\nQuery: {}", q);
        match conn.execute(q) {
            Ok(r) => println!("  OK: {} rows, cols: {:?}", r.rows.len(), r.columns),
            Err(e) => println!("  Error: {}", e),
        }
    }
    
    conn.close().unwrap();
}
