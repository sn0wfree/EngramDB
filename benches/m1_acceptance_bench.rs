//! M1 验收基准（文档 v1.0 三标准，10 万行）
//!
//! 运行：`cargo run --release --bench m1_acceptance_bench`
//!
//! 标准：
//! - M1-1: WHERE 1% 选择性 < 5ms
//! - M1-2: ORDER BY 整数列 < 30ms
//! - M1-3: 单列整数 GROUP BY < 5ms

use std::time::{Duration, Instant};
use engramdb::{Connection, Config, Value};

const ITERS: usize = 5;
const N: usize = 100_000;
const HDB_PATH: &str = "/tmp/m1_acceptance.hdb";

fn open_hdb() -> Connection {
    let config = Config {
        compress_on_persist: false,
        ..Config::default()
    };
    Connection::open_with_config(HDB_PATH, config).unwrap()
}

fn median(mut samples: Vec<Duration>) -> Duration {
    samples.sort();
    if samples.len() % 2 == 1 {
        samples[samples.len() / 2]
    } else {
        (samples[samples.len() / 2 - 1] + samples[samples.len() / 2]) / 2
    }
}

fn fmt_ms(d: Duration) -> String {
    let ms = d.as_secs_f64() * 1000.0;
    if ms < 1.0 {
        format!("{:.0} µs", ms * 1000.0)
    } else {
        format!("{:.2} ms", ms)
    }
}

struct SimpleRng {
    state: u64,
}

impl SimpleRng {
    fn new(seed: u64) -> Self {
        Self { state: seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407) }
    }
    fn gen_range(&mut self, range: std::ops::Range<i64>) -> i64 {
        self.state = self.state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let r = (self.state >> 32) as i64;
        let len = range.end - range.start;
        range.start + (r.rem_euclid(len))
    }
}

fn run_scenario(name: &str, target: Duration, mut f: impl FnMut() -> Duration) {
    let mut samples = Vec::with_capacity(ITERS);
    for _ in 0..ITERS {
        samples.push(f());
    }
    let m = median(samples);
    let pass = m <= target;
    let mark = if pass { "✅ PASS" } else { "❌ FAIL" };
    println!("  {name}: {} (目标 < {})  {mark}", fmt_ms(m), fmt_ms(target));
}

fn setup() {
    std::fs::remove_file(HDB_PATH).ok();
    let mut conn = open_hdb();
    conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, k INTEGER, val INTEGER, payload TEXT)").unwrap();
    let mut rng = SimpleRng::new(42);
    let mut sql = String::with_capacity(64 * N);
    for chunk_start in (0..N).step_by(1000) {
        let chunk_end = (chunk_start + 1000).min(N);
        sql.clear();
        sql.push_str("INSERT INTO t VALUES ");
        for i in chunk_start..chunk_end {
            if i > chunk_start {
                sql.push(',');
            }
            sql.push_str(&format!("({}, {}, {}, 'payload_{}')", i, rng.gen_range(0..1000), rng.gen_range(0..1000), i));
        }
        conn.execute(&sql).unwrap();
    }
    conn.close().unwrap();
}

fn m1_where() -> Duration {
    let mut conn = open_hdb();
    let start = Instant::now();
    let r = conn.execute("SELECT COUNT(*) FROM t WHERE val > 990").unwrap();
    let elapsed = start.elapsed();
    let count = match &r.rows[0][0] {
        Value::Int64(c) => *c,
        _ => panic!("expected Int64 count"),
    };
    assert!((800..1200).contains(&count), "1% 选择性应约 1000 行, got {count}");
    conn.close().unwrap();
    elapsed
}

fn m1_order_by() -> Duration {
    let mut conn = open_hdb();
    let start = Instant::now();
    let r = conn.execute("SELECT id, k, val, payload FROM t ORDER BY k").unwrap();
    let elapsed = start.elapsed();
    assert_eq!(r.rows.len(), N);
    for w in r.rows.windows(2) {
        match (&w[0][1], &w[1][1]) {
            (Value::Int64(a), Value::Int64(b)) => assert!(a <= b, "not sorted"),
            _ => panic!("expected Int64 key"),
        }
    }
    conn.close().unwrap();
    elapsed
}

fn m1_group_by() -> Duration {
    let mut conn = open_hdb();
    let start = Instant::now();
    let r = conn.execute("SELECT k, COUNT(*) FROM t GROUP BY k").unwrap();
    let elapsed = start.elapsed();
    assert_eq!(r.rows.len(), 1000, "k ∈ [0, 1000) 应 1000 组");
    conn.close().unwrap();
    elapsed
}

fn main() {
    println!("=== M1 验收复测 ({} 行, {} 轮取中位数) ===", N, ITERS);
    setup();
    println!("设置完成");

    run_scenario("M1-1 WHERE 1% 选择性", Duration::from_millis(5), m1_where);
    run_scenario("M1-2 ORDER BY 整数列", Duration::from_millis(30), m1_order_by);
    run_scenario("M1-3 GROUP BY 单列整数", Duration::from_millis(5), m1_group_by);
    std::fs::remove_file(HDB_PATH).ok();
    println!("=== M1 复测结束 ===");
}
