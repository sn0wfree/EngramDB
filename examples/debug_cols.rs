use engramdb::Connection;

fn main() {
    let mut conn = Connection::open("/tmp/debug_cols.db").unwrap();
    conn.execute("CREATE TABLE t1 (id INT, category INT, value DOUBLE, name VARCHAR);").unwrap();
    conn.execute("INSERT INTO t1 VALUES (1, 10, 100.0, 'a'), (2, 20, 200.0, 'b');").unwrap();
    
    // 测试 EXPLAIN
    match conn.explain("SELECT * FROM t1;") {
        Ok(s) => println!("EXPLAIN SELECT *:\n{}\n", s),
        Err(e) => println!("EXPLAIN error: {}\n", e),
    }
    
    match conn.explain("SELECT id FROM t1 WHERE id > 0;") {
        Ok(s) => println!("EXPLAIN SELECT WHERE:\n{}\n", s),
        Err(e) => println!("EXPLAIN error: {}\n", e),
    }
    
    // SELECT * 结果详情
    let r = conn.execute("SELECT * FROM t1;").unwrap();
    println!("SELECT * columns: {:?}", r.columns);
    println!("SELECT * rows: {}", r.rows.len());
    for (i, row) in r.rows.iter().enumerate() {
        println!("  row[{}]: {:?}", i, row.iter().map(|v| format!("{}", v)).collect::<Vec<_>>());
    }
    
    conn.close().unwrap();
}
