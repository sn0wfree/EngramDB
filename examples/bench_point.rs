use std::time::Instant;
use engramdb::{Connection, Value};

fn main() {
    // Setup with 10K rows
    {
        let mut conn = Connection::open("/tmp/test_point2.hdb").unwrap();
        conn.execute("CREATE TABLE t (id INT PRIMARY KEY, val DOUBLE, name VARCHAR)").unwrap();
        let stmt = conn.prepare("INSERT INTO t VALUES (?, ?, ?)").unwrap();
        let mut batch: Vec<Vec<Value>> = Vec::with_capacity(10000);
        for i in 0..10000 {
            batch.push(vec![
                Value::Int64(i),
                Value::Float64(i as f64 * 1.5),
                Value::Varchar(format!("row_{}", i)),
            ]);
        }
        conn.execute_prepared_batch(&stmt, &batch).unwrap();
        conn.close().unwrap();
    }
    println!("Setup done (10K rows)");

    // Test open
    let start = Instant::now();
    let mut conn = Connection::open("/tmp/test_point2.hdb").unwrap();
    println!("open(): {:?}", start.elapsed());

    // Test prepare
    let start = Instant::now();
    let stmt = conn.prepare("SELECT id, val FROM t WHERE id = ?").unwrap();
    println!("prepare(): {:?}", start.elapsed());

    // Test first query
    let start = Instant::now();
    let r = conn.execute_prepared(&stmt, &[Value::Int64(5000)]).unwrap();
    println!("1 query: {} rows, {:?}", r.rows.len(), start.elapsed());

    // Test 10 queries
    let start = Instant::now();
    for i in 0..10 {
        let _r = conn.execute_prepared(&stmt, &[Value::Int64(i)]).unwrap();
    }
    println!("10 queries: {:?}", start.elapsed());

    // Test close
    let start = Instant::now();
    conn.close().unwrap();
    println!("close(): {:?}", start.elapsed());
}
