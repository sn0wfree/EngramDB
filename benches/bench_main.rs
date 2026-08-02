//! 基准测试
//!
//! 运行：cargo bench

use criterion::{criterion_group, criterion_main, Criterion, BenchmarkId};
use hybriddb::Connection;
use tempfile::tempdir;

fn bench_insert(c: &mut Criterion) {
    let mut group = c.benchmark_group("insert");

    for &count in &[100, 1000, 10000] {
        group.bench_with_input(
            BenchmarkId::new("bulk_insert", count),
            &count,
            |b, &count| {
                b.iter(|| {
                    let dir = tempdir().unwrap();
                    let db_path = dir.path().join("bench.hdb");
                    let mut conn = Connection::open(db_path.to_str().unwrap()).unwrap();

                    conn.execute("CREATE TABLE t (id INT, val DOUBLE, name VARCHAR)").unwrap();

                    let values: Vec<String> = (0..count)
                        .map(|i| format!("({}, {}, 'row_{}')", i, i as f64 * 1.5, i))
                        .collect();
                    let sql = format!("INSERT INTO t VALUES {}", values.join(", "));
                    conn.execute(&sql).unwrap();

                    conn.close().unwrap();
                });
            },
        );
    }

    group.finish();
}

fn bench_select(c: &mut Criterion) {
    let mut group = c.benchmark_group("select");

    // 准备数据
    let dir = tempdir().unwrap();
    let db_path = dir.path().join("bench.hdb");
    let mut conn = Connection::open(db_path.to_str().unwrap()).unwrap();
    conn.execute("CREATE TABLE t (id INT, val DOUBLE, name VARCHAR)").unwrap();

    let values: Vec<String> = (0..10000)
        .map(|i| format!("({}, {}, 'row_{}')", i, i as f64 * 1.5, i))
        .collect();
    let sql = format!("INSERT INTO t VALUES {}", values.join(", "));
    conn.execute(&sql).unwrap();
    conn.close().unwrap();

    group.bench_function("full_scan_10k", |b| {
        b.iter(|| {
            let mut conn = Connection::open(db_path.to_str().unwrap()).unwrap();
            let result = conn.execute("SELECT * FROM t").unwrap();
            assert_eq!(result.rows.len(), 10000);
            conn.close().unwrap();
        });
    });

    group.bench_function("select_with_where", |b| {
        b.iter(|| {
            let mut conn = Connection::open(db_path.to_str().unwrap()).unwrap();
            let result = conn.execute("SELECT id, val FROM t WHERE id > 5000").unwrap();
            assert_eq!(result.rows.len(), 4999);
            conn.close().unwrap();
        });
    });

    group.finish();
}

criterion_group!(benches, bench_insert, bench_select);
criterion_main!(benches);
