use engramdb::Connection;
use engramdb::sql::{parser, planner};

fn main() {
    let mut conn = Connection::open("/tmp/debug_count.db").unwrap();
    conn.execute("CREATE TABLE t1 (id INT, value DOUBLE);").unwrap();
    conn.execute("INSERT INTO t1 VALUES (1, 10.0), (2, 20.0);").unwrap();
    
    // 测试不同的 COUNT 写法
    let queries = vec![
        "SELECT COUNT(*) FROM t1;",
        "SELECT COUNT(id) FROM t1;",
        "SELECT SUM(value) FROM t1;",
        "SELECT id, COUNT(*) FROM t1 GROUP BY id;",
    ];
    
    for q in queries {
        println!("\n=== Query: {} ===", q);
        match conn.execute(q) {
            Ok(r) => {
                println!("  OK: {} rows", r.rows.len());
                for row in &r.rows {
                    println!("    {:?}", row.iter().map(|v| format!("{}", v)).collect::<Vec<_>>());
                }
            }
            Err(e) => println!("  Error: {}", e),
        }
    }
    
    conn.close().unwrap();
}
