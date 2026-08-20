//! 崩溃注入 runner（写入侧）
//!
//! 命令行：`cargo run --release --example inject_crash_runner -- --path PATH --scenario X --n-txns N --kill-at K`
//!
//! scenario: mid-write | mid-fsync | mid-commit | post-commit
//!
//! 在 --kill-at 事务后，runner 会等待被父进程 kill。
//! 父进程通过 SIGKILL 模拟掉电/进程崩溃。

use std::env;
use std::thread;
use std::time::Duration;

fn main() {
    let args: Vec<String> = env::args().collect();
    let path = args.windows(2).find(|w| w[0] == "--path")
        .map(|w| w[1].clone()).expect("--path required");
    let scenario = args.windows(2).find(|w| w[0] == "--scenario")
        .map(|w| w[1].clone()).expect("--scenario required");
    let n_txns: usize = args.windows(2).find(|w| w[0] == "--n-txns")
        .map(|w| w[1].parse().unwrap()).expect("--n-txns required");
    let kill_at: usize = args.windows(2).find(|w| w[0] == "--kill-at")
        .map(|w| w[1].parse().unwrap()).expect("--kill-at required");

    let mut conn = engramdb::Connection::open(&path).unwrap();
    conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY, v TEXT)").unwrap();

    eprintln!("[runner] scenario={} n_txns={} kill_at={}", scenario, n_txns, kill_at);

    for i in 0..n_txns {
        let sql = format!("INSERT INTO t VALUES ({}, 'payload-{}')", i, i);
        conn.execute(&sql).unwrap();

        if i + 1 == kill_at {
            match scenario.as_str() {
                "post-commit" => {
                    // 完整 commit 完成后正常退出
                    eprintln!("[runner] post-commit at txn {} — exiting normally", i + 1);
                    return;
                }
                _ => {
                    // 让父进程通过 kill -9 终止我们
                    eprintln!("[runner] pausing at txn {} — awaiting SIGKILL", i + 1);
                    thread::sleep(Duration::from_secs(60));
                }
            }
        }
    }

    eprintln!("[runner] completed all {} transactions", n_txns);
}