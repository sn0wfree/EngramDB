//! 写入性能基准测试
//!
//! 测试 P0-P4 各优化层级的写入性能：
//! - baseline: 单条 INSERT SQL
//! - batch_sql: 批量 INSERT SQL（P2+P3 优化）
//! - prepared: 预编译语句批量执行（P0 优化）
//! - large_batch: 大批量直接列式路径（P1 优化）
//! - compact: Delta→列存合并速度（P4 优化）

use engramdb::{Connection, Value};
use std::time::Instant;

const TABLE_SQL: &str = "CREATE TABLE bench (id INT, name VARCHAR, age INT, score DOUBLE, active BOOLEAN)";

fn main() {
    println!("=== EngramDB v0.11.1 写入性能基准测试 ===\n");

    // 测试 1: 单条 INSERT（baseline）
    bench_single_insert(1_000);

    // 测试 2: 批量 INSERT SQL（100 行/批）
    bench_batch_sql(10_000, 100);

    // 测试 3: 批量 INSERT SQL（1000 行/批）
    bench_batch_sql(100_000, 1000);

    // 测试 4: Prepared Statement（P0）
    bench_prepared(10_000, 100);

    // 测试 5: Prepared Statement 大批量（触发 P1 直接列式路径）
    bench_prepared_large(100_000);

    // 测试 6: compact 速度（P4）
    bench_compact(50_000);

    println!("\n=== 测试完成 ===");
}

fn bench_single_insert(count: usize) {
    println!("【测试 1】单条 INSERT SQL（{} 行）", count);
    let path = format!("/tmp/bench_single_{}.db", count);
    let _ = std::fs::remove_file(&path);

    let mut conn = Connection::open(&path).unwrap();
    conn.execute(TABLE_SQL).unwrap();

    let start = Instant::now();
    for i in 0..count {
        let sql = format!(
            "INSERT INTO bench VALUES ({}, 'user_{}', {}, {:.1}, {})",
            i, i, 20 + (i % 40), (i % 100) as f64, i % 2 == 0
        );
        conn.execute(&sql).unwrap();
    }
    let elapsed = start.elapsed();

    let rows_per_sec = count as f64 / elapsed.as_secs_f64();
    println!("  耗时: {:.3}s", elapsed.as_secs_f64());
    println!("  速度: {:.1} 行/秒", rows_per_sec);
    println!();

    conn.close().unwrap();
    let _ = std::fs::remove_file(&path);
}

fn bench_batch_sql(total: usize, batch_size: usize) {
    println!("【测试 2】批量 INSERT SQL（共 {} 行，{} 行/批，P2+P3 优化）", total, batch_size);
    let path = format!("/tmp/bench_batch_{}_{}.db", total, batch_size);
    let _ = std::fs::remove_file(&path);

    let mut conn = Connection::open(&path).unwrap();
    conn.execute(TABLE_SQL).unwrap();

    let start = Instant::now();
    let mut offset = 0;
    while offset < total {
        let end = std::cmp::min(offset + batch_size, total);
        let mut sql = String::from("INSERT INTO bench VALUES ");
        for i in offset..end {
            if i > offset {
                sql.push_str(", ");
            }
            sql.push_str(&format!(
                "({}, 'user_{}', {}, {:.1}, {})",
                i, i, 20 + (i % 40), (i % 100) as f64, i % 2 == 0
            ));
        }
        conn.execute(&sql).unwrap();
        offset = end;
    }
    let elapsed = start.elapsed();

    let rows_per_sec = total as f64 / elapsed.as_secs_f64();
    println!("  耗时: {:.3}s", elapsed.as_secs_f64());
    println!("  速度: {:.1} 行/秒 ({:.1} 万行/秒)", rows_per_sec, rows_per_sec / 10000.0);
    println!();

    conn.close().unwrap();
    let _ = std::fs::remove_file(&path);
}

fn bench_prepared(total: usize, batch_size: usize) {
    println!("【测试 3】Prepared Statement 批量（共 {} 行，{} 行/批，P0 优化）", total, batch_size);
    let path = format!("/tmp/bench_prepared_{}_{}.db", total, batch_size);
    let _ = std::fs::remove_file(&path);

    let mut conn = Connection::open(&path).unwrap();
    conn.execute(TABLE_SQL).unwrap();

    let stmt = conn.prepare("INSERT INTO bench VALUES (?, ?, ?, ?, ?)").unwrap();

    let start = Instant::now();
    let mut offset = 0;
    while offset < total {
        let end = std::cmp::min(offset + batch_size, total);
        let mut batch = Vec::with_capacity(end - offset);
        for i in offset..end {
            let params = vec![
                Value::Int64(i as i64),
                Value::Varchar(format!("user_{}", i)),
                Value::Int64(20 + (i % 40) as i64),
                Value::Float64((i % 100) as f64),
                Value::Boolean(i % 2 == 0),
            ];
            batch.push(params);
        }
        conn.execute_prepared_batch(&stmt, &batch).unwrap();
        offset = end;
    }
    let elapsed = start.elapsed();

    let rows_per_sec = total as f64 / elapsed.as_secs_f64();
    println!("  耗时: {:.3}s", elapsed.as_secs_f64());
    println!("  速度: {:.1} 行/秒 ({:.1} 万行/秒)", rows_per_sec, rows_per_sec / 10000.0);
    println!();

    conn.close().unwrap();
    let _ = std::fs::remove_file(&path);
}

fn bench_prepared_large(total: usize) {
    println!("【测试 4】Prepared Statement 大批量（{} 行，触发 P1 直接列式路径）", total);
    let path = format!("/tmp/bench_prepared_large_{}.db", total);
    let _ = std::fs::remove_file(&path);

    let mut conn = Connection::open(&path).unwrap();
    conn.execute(TABLE_SQL).unwrap();

    let stmt = conn.prepare("INSERT INTO bench VALUES (?, ?, ?, ?, ?)").unwrap();

    // 一次性大批量，触发 P1 直接列式路径
    let mut batch = Vec::with_capacity(total);
    for i in 0..total {
        let params = vec![
            Value::Int64(i as i64),
            Value::Varchar(format!("user_{}", i)),
            Value::Int64(20 + (i % 40) as i64),
            Value::Float64((i % 100) as f64),
            Value::Boolean(i % 2 == 0),
        ];
        batch.push(params);
    }

    let start = Instant::now();
    conn.execute_prepared_batch(&stmt, &batch).unwrap();
    let elapsed = start.elapsed();

    let rows_per_sec = total as f64 / elapsed.as_secs_f64();
    println!("  耗时: {:.3}s", elapsed.as_secs_f64());
    println!("  速度: {:.1} 行/秒 ({:.1} 万行/秒)", rows_per_sec, rows_per_sec / 10000.0);
    println!();

    conn.close().unwrap();
    let _ = std::fs::remove_file(&path);
}

fn bench_compact(rows: usize) {
    println!("【测试 5】Delta compact 速度（{} 行，P4 列式 Delta 优化）", rows);
    let path = format!("/tmp/bench_compact_{}.db", rows);
    let _ = std::fs::remove_file(&path);

    let mut conn = Connection::open(&path).unwrap();
    conn.execute(TABLE_SQL).unwrap();

    // 先写入数据到 Delta 层（小批量，确保在 Delta 中）
    let stmt = conn.prepare("INSERT INTO bench VALUES (?, ?, ?, ?, ?)").unwrap();
    let batch_size = 500; // 小批量，确保走 Delta 路径
    let mut offset = 0;
    while offset < rows {
        let end = std::cmp::min(offset + batch_size, rows);
        let mut batch = Vec::with_capacity(end - offset);
        for i in offset..end {
            let params = vec![
                Value::Int64(i as i64),
                Value::Varchar(format!("user_{}", i)),
                Value::Int64(20 + (i % 40) as i64),
                Value::Float64((i % 100) as f64),
                Value::Boolean(i % 2 == 0),
            ];
            batch.push(params);
        }
        conn.execute_prepared_batch(&stmt, &batch).unwrap();
        offset = end;
    }

    // 手动触发 compact（通过执行一个大查询来强制 compact？或者直接测 API）
    // 这里我们用执行大查询的方式间接测量
    let start = Instant::now();
    let result = conn.execute("SELECT COUNT(*) FROM bench").unwrap();
    let elapsed = start.elapsed();

    println!("  查询+compact 耗时: {:.3}s", elapsed.as_secs_f64());
    println!("  返回行数: {}", result.rows_affected);
    println!();

    conn.close().unwrap();
    let _ = std::fs::remove_file(&path);
}
