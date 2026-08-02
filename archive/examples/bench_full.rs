//! HybridDB 完整性能基准测试

use hybriddb::Connection;
use std::time::Instant;
use rand::Rng;
use rand::SeedableRng;

struct BenchResult {
    name: String,
    rows: usize,
    duration_ms: f64,
    note: String,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let n_rows: usize = if args.len() > 1 {
        args[1].parse().unwrap_or(100_000)
    } else {
        100_000
    };

    println!("=== HybridDB v0.11.0 性能基准测试 ===");
    println!("数据规模: {} 行", n_rows);
    println!();

    let db_path = format!("/tmp/hybriddb_bench_{}.db", n_rows);
    let _ = std::fs::remove_file(&db_path);

    let mut conn = Connection::open(&db_path).unwrap();
    let mut results: Vec<BenchResult> = Vec::new();

    // 1. CREATE TABLE
    let start = Instant::now();
    conn.execute("CREATE TABLE t1 (id INT, category INT, value DOUBLE, name VARCHAR);").unwrap();
    let dur = start.elapsed().as_secs_f64() * 1000.0;
    results.push(BenchResult { name: "CREATE TABLE".into(), rows: 1, duration_ms: dur, note: "".into() });
    println!("{:<30} {:>10.2} ms", "CREATE TABLE", dur);

    // 2. 批量 INSERT
    let mut rng = rand::rngs::StdRng::seed_from_u64(42);
    let mut values = Vec::with_capacity(n_rows);
    for i in 0..n_rows {
        let cat = rng.gen_range(0..100);
        let val = rng.gen_range(0.0..1000.0);
        values.push(format!("({}, {}, {:.4}, 'item_{}')", i, cat, val, i));
    }

    let batch_size = 1000;
    let start = Instant::now();
    for chunk in values.chunks(batch_size) {
        let sql = format!("INSERT INTO t1 VALUES {};", chunk.join(", "));
        conn.execute(&sql).unwrap();
    }
    let dur = start.elapsed().as_secs_f64() * 1000.0;
    results.push(BenchResult { name: "INSERT (batch 1000)".into(), rows: n_rows, duration_ms: dur, note: "批量插入".into() });
    println!("{:<30} {:>10.2} ms  ({:>10.0} rows/s)", "INSERT (batch 1000)", dur, n_rows as f64 / (dur / 1000.0));

    // 3. SELECT * 全表扫描
    let start = Instant::now();
    let r = conn.execute("SELECT * FROM t1;").unwrap();
    let dur = start.elapsed().as_secs_f64() * 1000.0;
    results.push(BenchResult { name: "SELECT * (full scan)".into(), rows: r.rows.len(), duration_ms: dur, note: "全表扫描".into() });
    println!("{:<30} {:>10.2} ms  ({:>10.0} rows/s)", "SELECT * (full scan)", dur, r.rows.len() as f64 / (dur / 1000.0));

    // 4. SELECT 2 列投影扫描
    let start = Instant::now();
    let r = conn.execute("SELECT id, value FROM t1;").unwrap();
    let dur = start.elapsed().as_secs_f64() * 1000.0;
    results.push(BenchResult { name: "SELECT 2 cols".into(), rows: r.rows.len(), duration_ms: dur, note: "列裁剪".into() });
    println!("{:<30} {:>10.2} ms  ({:>10.0} rows/s)", "SELECT 2 cols", dur, r.rows.len() as f64 / (dur / 1000.0));

    // 5. WHERE 过滤（50%）
    let start = Instant::now();
    let r = conn.execute("SELECT id, value FROM t1 WHERE value > 500.0;").unwrap();
    let dur = start.elapsed().as_secs_f64() * 1000.0;
    let pct = r.rows.len() as f64 / n_rows as f64 * 100.0;
    results.push(BenchResult { name: "SELECT WHERE (50%)".into(), rows: r.rows.len(), duration_ms: dur, note: format!("返回{:.0}%行", pct) });
    println!("{:<30} {:>10.2} ms  ({:>10.0} rows/s, {} rows)", "SELECT WHERE (50%)", dur, r.rows.len() as f64 / (dur / 1000.0), r.rows.len());

    // 6. WHERE 过滤（1%）
    let start = Instant::now();
    let r = conn.execute("SELECT id, value FROM t1 WHERE value > 990.0;").unwrap();
    let dur = start.elapsed().as_secs_f64() * 1000.0;
    let pct = r.rows.len() as f64 / n_rows as f64 * 100.0;
    results.push(BenchResult { name: "SELECT WHERE (1%)".into(), rows: r.rows.len(), duration_ms: dur, note: format!("返回{:.1}%行", pct) });
    println!("{:<30} {:>10.2} ms  ({:>10.0} rows/s, {} rows)", "SELECT WHERE (1%)", dur, r.rows.len() as f64 / (dur / 1000.0), r.rows.len());

    // 7. COUNT(*)
    let start = Instant::now();
    let r = conn.execute("SELECT COUNT(*) FROM t1;").unwrap();
    let dur = start.elapsed().as_secs_f64() * 1000.0;
    results.push(BenchResult { name: "COUNT(*)".into(), rows: 1, duration_ms: dur, note: "全表计数".into() });
    println!("{:<30} {:>10.2} ms", "COUNT(*)", dur);

    // 8. SUM + AVG
    let start = Instant::now();
    let r = conn.execute("SELECT SUM(value) FROM t1;").unwrap();
    let dur = start.elapsed().as_secs_f64() * 1000.0;
    results.push(BenchResult { name: "SUM(value)".into(), rows: 1, duration_ms: dur, note: "全表聚合".into() });
    println!("{:<30} {:>10.2} ms", "SUM(value)", dur);

    // 9. GROUP BY + COUNT
    let start = Instant::now();
    let r = conn.execute("SELECT category, COUNT(*) FROM t1 GROUP BY category;").unwrap();
    let dur = start.elapsed().as_secs_f64() * 1000.0;
    results.push(BenchResult { name: "GROUP BY + COUNT".into(), rows: r.rows.len(), duration_ms: dur, note: format!("{}组", r.rows.len()) });
    println!("{:<30} {:>10.2} ms  ({} groups)", "GROUP BY + COUNT", dur, r.rows.len());

    // 10. ORDER BY 全排序
    let start = Instant::now();
    let r = conn.execute("SELECT id, value FROM t1 ORDER BY value DESC;").unwrap();
    let dur = start.elapsed().as_secs_f64() * 1000.0;
    results.push(BenchResult { name: "ORDER BY (full)".into(), rows: r.rows.len(), duration_ms: dur, note: "全量排序".into() });
    println!("{:<30} {:>10.2} ms  ({:>10.0} rows/s)", "ORDER BY (full)", dur, r.rows.len() as f64 / (dur / 1000.0));

    // 11. ORDER BY + LIMIT 100
    let start = Instant::now();
    let r = conn.execute("SELECT id, value FROM t1 ORDER BY value DESC LIMIT 100;").unwrap();
    let dur = start.elapsed().as_secs_f64() * 1000.0;
    results.push(BenchResult { name: "ORDER BY + LIMIT 100".into(), rows: r.rows.len(), duration_ms: dur, note: "Top-N".into() });
    println!("{:<30} {:>10.2} ms  ({} rows)", "ORDER BY + LIMIT 100", dur, r.rows.len());

    // 12. DISTINCT
    let start = Instant::now();
    let r = conn.execute("SELECT DISTINCT category FROM t1;").unwrap();
    let dur = start.elapsed().as_secs_f64() * 1000.0;
    results.push(BenchResult { name: "SELECT DISTINCT".into(), rows: r.rows.len(), duration_ms: dur, note: "去重".into() });
    println!("{:<30} {:>10.2} ms  ({} unique)", "SELECT DISTINCT", dur, r.rows.len());

    // JSON 输出
    println!();
    println!("JSON_RESULT_START");
    println!("{{\"engine\":\"HybridDB\",\"version\":\"0.11.0\",\"n_rows\":{},\"results\":[", n_rows);
    for (i, r) in results.iter().enumerate() {
        let comma = if i < results.len() - 1 { "," } else { "" };
        println!("  {{\"name\":\"{}\",\"rows\":{},\"duration_ms\":{:.3},\"note\":\"{}\"}}{}",
            r.name, r.rows, r.duration_ms, r.note, comma);
    }
    println!("]}}");
    println!("JSON_RESULT_END");

    conn.close().unwrap();
    let _ = std::fs::remove_file(&db_path);
}
