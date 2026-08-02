use hybriddb::Connection;
use std::time::Instant;
use rand::Rng;
use rand::SeedableRng;

fn main() {
    let n_rows = 100_000;
    let db_path = "/tmp/bench_insert_prof.db";
    let _ = std::fs::remove_file(db_path);
    
    let mut conn = Connection::open(db_path).unwrap();
    conn.execute("CREATE TABLE t1 (id INT, category INT, value DOUBLE, name VARCHAR);").unwrap();
    
    let mut rng = rand::rngs::StdRng::seed_from_u64(42);
    let mut values = Vec::with_capacity(n_rows);
    for i in 0..n_rows {
        let cat = rng.gen_range(0..100);
        let val = rng.gen_range(0.0..1000.0);
        values.push(format!("({}, {}, {:.4}, 'item_{}')", i, cat, val, i));
    }
    
    // 测试不同 batch size
    for &batch_size in &[100, 500, 1000, 2000, 5000] {
        let _ = std::fs::remove_file(db_path);
        let mut conn = Connection::open(db_path).unwrap();
        conn.execute("CREATE TABLE t1 (id INT, category INT, value DOUBLE, name VARCHAR);").unwrap();
        
        let start = Instant::now();
        let mut total = 0;
        for chunk in values.chunks(batch_size) {
            let sql = format!("INSERT INTO t1 VALUES {};", chunk.join(", "));
            let r = conn.execute(&sql).unwrap();
            total += r.rows_affected;
        }
        let dur = start.elapsed().as_secs_f64() * 1000.0;
        println!("batch_size={:>5}: {:>8.2} ms  ({:>10.0} rows/s)", batch_size, dur, n_rows as f64 / (dur / 1000.0));
    }
    
    conn.close().unwrap();
    let _ = std::fs::remove_file(db_path);
}
