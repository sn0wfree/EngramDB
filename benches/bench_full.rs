//! EngramDB 完整性能基准测试
//! 对比日常使用场景：数据加载、扫描、过滤、聚合、Join、排序

use engramdb::Connection;
use std::time::Instant;
use rand::Rng;
use rand::SeedableRng;

struct BenchResult {
    name: &'static str,
    rows: usize,
    duration_ms: f64,
}

impl BenchResult {
    fn new(name: &'static str, rows: usize, duration_ms: f64) -> Self {
        Self { name, rows, duration_ms }
    }

    fn throughput(&self) -> f64 {
        if self.duration_ms > 0.0 {
            self.rows as f64 / (self.duration_ms / 1000.0)
        } else {
            0.0
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let n_rows: usize = if args.len() > 1 {
        args[1].parse().unwrap_or(100_000)
    } else {
        100_000
    };

    println!("=== EngramDB 完整性能基准测试 ===");
    println!("数据规模: {} 行", n_rows);
    println!();

    let db_path = format!("/tmp/engramdb_bench_{}.db", n_rows);
    let _ = std::fs::remove_file(&db_path);

    let mut conn = Connection::open(&db_path).unwrap();
    let mut results: Vec<BenchResult> = Vec::new();

    // --- 1. CREATE TABLE ---
    let start = Instant::now();
    conn.execute("CREATE TABLE t1 (id INT, category INT, value DOUBLE, name VARCHAR);").unwrap();
    let dur = start.elapsed().as_secs_f64() * 1000.0;
    results.push(BenchResult::new("CREATE TABLE", 1, dur));
    println!("{:<25} {:>10.2} ms", "CREATE TABLE", dur);

    // --- 2. 批量 INSERT ---
    // 生成测试数据
    let mut rng = rand::rngs::StdRng::seed_from_u64(42);
    let mut values = Vec::with_capacity(n_rows);
    for i in 0..n_rows {
        let cat = rng.gen_range(0..100);
        let val = rng.gen_range(0.0..1000.0);
        let name = format!("item_{}", i);
        values.push(format!("({}, {}, {:.4}, '{}')", i, cat, val, name));
    }

    // 分批插入，每批 1000 行
    let batch_size = 1000;
    let start = Instant::now();
    for chunk in values.chunks(batch_size) {
        let sql = format!("INSERT INTO t1 VALUES {};", chunk.join(", "));
        conn.execute(&sql).unwrap();
    }
    let dur = start.elapsed().as_secs_f64() * 1000.0;
    results.push(BenchResult::new("INSERT (batch)", n_rows, dur));
    println!("{:<25} {:>10.2} ms  ({:>10.0} rows/s)", "INSERT (batch)", dur, n_rows as f64 / (dur / 1000.0));

    // --- 3. 全表扫描 SELECT * ---
    let start = Instant::now();
    let r = conn.execute("SELECT * FROM t1;").unwrap();
    let dur = start.elapsed().as_secs_f64() * 1000.0;
    results.push(BenchResult::new("SELECT * (full scan)", r.rows.len(), dur));
    println!("{:<25} {:>10.2} ms  ({:>10.0} rows/s)", "SELECT * (full scan)", dur, r.rows.len() as f64 / (dur / 1000.0));

    // --- 4. 投影扫描 SELECT 部分列 ---
    let start = Instant::now();
    let r = conn.execute("SELECT id, value FROM t1;").unwrap();
    let dur = start.elapsed().as_secs_f64() * 1000.0;
    results.push(BenchResult::new("SELECT projection", r.rows.len(), dur));
    println!("{:<25} {:>10.2} ms  ({:>10.0} rows/s)", "SELECT projection", dur, r.rows.len() as f64 / (dur / 1000.0));

    // --- 5. 过滤查询 WHERE ---
    let start = Instant::now();
    let r = conn.execute("SELECT id, value FROM t1 WHERE value > 500.0;").unwrap();
    let dur = start.elapsed().as_secs_f64() * 1000.0;
    results.push(BenchResult::new("SELECT WHERE filter", r.rows.len(), dur));
    println!("{:<25} {:>10.2} ms  ({:>10.0} rows/s, {} rows)", "SELECT WHERE filter", dur, r.rows.len() as f64 / (dur / 1000.0), r.rows.len());

    // --- 6. 聚合 COUNT ---
    let start = Instant::now();
    let r = conn.execute("SELECT COUNT(*) as cnt FROM t1;").unwrap();
    let dur = start.elapsed().as_secs_f64() * 1000.0;
    results.push(BenchResult::new("COUNT(*)", 1, dur));
    println!("{:<25} {:>10.2} ms", "COUNT(*)", dur);

    // --- 7. 聚合 SUM + AVG ---
    let start = Instant::now();
    let r = conn.execute("SELECT SUM(value) as s, AVG(value) as a FROM t1;").unwrap();
    let dur = start.elapsed().as_secs_f64() * 1000.0;
    results.push(BenchResult::new("SUM + AVG", 1, dur));
    println!("{:<25} {:>10.2} ms", "SUM + AVG", dur);

    // --- 8. GROUP BY 聚合 ---
    let start = Instant::now();
    let r = conn.execute("SELECT category, COUNT(*) as cnt, AVG(value) as avg_val FROM t1 GROUP BY category;").unwrap();
    let dur = start.elapsed().as_secs_f64() * 1000.0;
    results.push(BenchResult::new("GROUP BY + AGG", r.rows.len(), dur));
    println!("{:<25} {:>10.2} ms  ({} groups)", "GROUP BY + AGG", dur, r.rows.len());

    // --- 9. ORDER BY 排序 ---
    let start = Instant::now();
    let r = conn.execute("SELECT id, value FROM t1 ORDER BY value DESC;").unwrap();
    let dur = start.elapsed().as_secs_f64() * 1000.0;
    results.push(BenchResult::new("ORDER BY (full sort)", r.rows.len(), dur));
    println!("{:<25} {:>10.2} ms  ({:>10.0} rows/s)", "ORDER BY (full sort)", dur, r.rows.len() as f64 / (dur / 1000.0));

    // --- 10. LIMIT ---
    let start = Instant::now();
    let r = conn.execute("SELECT id, value FROM t1 ORDER BY value DESC LIMIT 100;").unwrap();
    let dur = start.elapsed().as_secs_f64() * 1000.0;
    results.push(BenchResult::new("ORDER BY + LIMIT 100", r.rows.len(), dur));
    println!("{:<25} {:>10.2} ms", "ORDER BY + LIMIT 100", dur);

    // --- 11. CREATE TABLE 第二张表 ---
    conn.execute("CREATE TABLE t2 (cat_id INT, cat_name VARCHAR, cat_weight DOUBLE);").unwrap();
    let mut t2_values = Vec::new();
    for i in 0..100 {
        t2_values.push(format!("({}, 'category_{}', {:.2})", i, i, rng.gen_range(0.5..5.0)));
    }
    let sql = format!("INSERT INTO t2 VALUES {};", t2_values.join(", "));
    conn.execute(&sql).unwrap();

    // --- 12. JOIN 查询 ---
    let start = Instant::now();
    let r = conn.execute("SELECT t1.id, t1.value, t2.cat_name FROM t1 JOIN t2 ON t1.category = t2.cat_id WHERE t1.value > 800.0;").unwrap();
    let dur = start.elapsed().as_secs_f64() * 1000.0;
    results.push(BenchResult::new("JOIN + filter", r.rows.len(), dur));
    println!("{:<25} {:>10.2} ms  ({} rows)", "JOIN + filter", dur, r.rows.len());

    // --- 13. 子查询 / 复杂查询 ---
    let start = Instant::now();
    let r = conn.execute("SELECT category, AVG(value) as avg_val FROM t1 GROUP BY category HAVING AVG(value) > 500.0;").unwrap();
    let dur = start.elapsed().as_secs_f64() * 1000.0;
    results.push(BenchResult::new("GROUP BY + HAVING", r.rows.len(), dur));
    println!("{:<25} {:>10.2} ms  ({} groups)", "GROUP BY + HAVING", dur, r.rows.len());

    // --- 输出汇总 ---
    println!();
    println!("=== 汇总 ({} 行) ===", n_rows);
    println!("{:<25} {:>12} {:>12}", "项目", "耗时(ms)", "吞吐(行/s)");
    println!("{}", "-".repeat(52));
    for r in &results {
        if r.rows > 1 {
            println!("{:<25} {:>12.2} {:>12.0}", r.name, r.duration_ms, r.throughput());
        } else {
            println!("{:<25} {:>12.2} {:>12}", r.name, r.duration_ms, "-");
        }
    }

    conn.close().unwrap();
    let _ = std::fs::remove_file(&db_path);
}
