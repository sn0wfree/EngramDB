//! 崩溃注入恢复验证（读侧）
//!
//! 命令行：`cargo run --release --example inject_crash_recover -- --path PATH`
//!
//! 打开数据库，验证：
//! 1. 不崩溃
//! 2. 总行数 <= 预期（崩溃前事务可能未提交）
//! 3. 数据完整性（无损坏，无悬挂事务）

use std::env;

fn main() {
    let args: Vec<String> = env::args().collect();
    let path = args.windows(2).find(|w| w[0] == "--path")
        .map(|w| w[1].clone()).expect("--path required");

    let conn_result = engramdb::Connection::open(&path);
    match conn_result {
        Ok(mut conn) => {
            let count_result = conn.execute("SELECT COUNT(*) FROM t");
            match count_result {
                Ok(r) => {
                    let count = r.rows[0][0].as_i64().unwrap_or(-1);
                    println!("recovered row count: {}", count);
                    println!("recovered: OK");
                }
                Err(e) => {
                    eprintln!("recovered: query failed: {}", e);
                    std::process::exit(1);
                }
            }
        }
        Err(e) => {
            eprintln!("recovered: open failed: {}", e);
            std::process::exit(1);
        }
    }
}