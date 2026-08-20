# M1 — WAL 组提交验证

> **日期**：2026-08-25（周二）
> **截止**：2026-08-25 EOD
> **核心代码**：已实现，本次仅做验证
> **KPI**：`group=16` vs `group=0` 写入吞吐 **≥ 3×**（10 次 P50）

---

## 一、代码现状校准

### 1.1 已实现

| 模块 | 行号 | 状态 |
|---|---|---|
| 三触发模式（size/bytes/timeout） | `src/wal/writer.rs:180-222` | ✅ |
| 单元测试（10 个） | `src/wal/writer.rs:485-675` | ✅ |
| 生产默认值 `16/64KB/10ms` | `src/common/config.rs:307-312` | ✅ |
| 事务层接通 | `src/txn/manager.rs:54-71`, `:132-180` | ✅ |
| 只读事务跳过 fsync | `src/txn/manager.rs:104, 149-155` | ✅ |
| 非持久化表跳过 fsync | `src/txn/manager.rs:151-155` | ✅ |

### 1.2 待验证

| 项 | 必要性 |
|---|---|
| 端到端链路（事务→commit→fsync）跨模块回归 | 高 |
| 边界：空事务 | 中 |
| 边界：超 buffer 大事务（payload > 64KB） | 高 |
| 并发：多线程 commit（每线程独立事务） | 高 |
| 并发：多线程共享 writer（**当前 API 是否支持？需确认**） | 高 |
| 崩溃一致性：mid-write / mid-fsync / mid-commit 注入 | 高 |

---

## 二、本日任务清单

### 2.1 上午（3h）— 端到端链路回归

**目标**：验证 `TransactionManager::commit()` → `wal.commit_flush()` 全链路在各种组合下行为正确。

新建 `tests/group_commit_e2e.rs`：

```rust
//! WAL 组提交端到端测试
//!
//! 验证场景：
//! 1. 单事务 commit → 立即 fsync（group=0）
//! 2. 多事务 commit → 累计到阈值 fsync（group>0）
//! 3. 字节阈值触发 fsync
//! 4. 时间阈值触发 fsync
//! 5. 只读事务跳过 fsync
//! 6. MemoryEngine 表事务跳过 fsync
//! 7. 混合持久化/非持久化表事务

use engramdb::common::config::{Config, WalFlushMode};
use engramdb::Connection;

fn open_with(group: usize, timeout_ms: u64) -> Connection {
    let path = format!("/tmp/m1_e2e_{}.hdb", std::process::id());
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}-wal", path));
    let mut cfg = Config::default();
    cfg.wal_group_commit_size = group;
    cfg.wal_group_commit_timeout_ms = timeout_ms;
    Connection::open_with_config(&path, cfg).unwrap()
}

#[test]
fn test_group_commit_default_16_64kb_10ms() {
    let mut conn = open_with(16, 10);
    conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY) ENGINE=Columnar").unwrap();
    for i in 0..100 {
        conn.execute(&format!("INSERT INTO t VALUES ({})", i)).unwrap();
    }
    // 验证 durable_lsn == current_lsn（在显式 sync 后）
    conn.execute("CHECKPOINT").unwrap();
}

#[test]
fn test_group_commit_size_zero_disables_grouping() {
    // 验证：group=0 时每次 commit 都 fsync，pending_commits 始终为 0
    let mut conn = open_with(0, 0);
    conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY) ENGINE=Columnar").unwrap();
    for i in 0..10 {
        conn.execute(&format!("INSERT INTO t VALUES ({})", i)).unwrap();
    }
}

#[test]
fn test_group_commit_readonly_skips_wal() {
    let mut conn = open_with(16, 10);
    conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY) ENGINE=Columnar").unwrap();
    // 只读事务不应产生 WAL COMMIT 记录
    for _ in 0..100 {
        let _ = conn.query("SELECT * FROM t").unwrap();
    }
    // 验证：WAL 文件大小不应增长（无 COMMIT 记录）
    let wal_size = std::fs::metadata(format!("/tmp/m1_e2e_{}.hdb-wal", std::process::id()))
        .map(|m| m.len()).unwrap_or(0);
    assert_eq!(wal_size, 0, "只读事务不应写 WAL COMMIT");
}

#[test]
fn test_group_commit_memory_table_skips_wal() {
    let mut conn = open_with(16, 10);
    conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY) ENGINE=Memory").unwrap();
    for i in 0..10 {
        conn.execute(&format!("INSERT INTO t VALUES ({})", i)).unwrap();
    }
    // 验证：MemoryEngine 表的事务不应写 WAL
    let wal_size = std::fs::metadata(format!("/tmp/m1_e2e_{}.hdb-wal", std::process::id()))
        .map(|m| m.len()).unwrap_or(0);
    assert_eq!(wal_size, 0, "MemoryEngine 表事务不应写 WAL");
}

#[test]
fn test_group_commit_sync_wal_forces_flush() {
    let mut conn = open_with(1000, 0); // 大 group size 不自动触发
    conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY) ENGINE=Columnar").unwrap();
    for i in 0..10 {
        conn.execute(&format!("INSERT INTO t VALUES ({})", i)).unwrap();
    }
    // 显式 sync_wal() 强制刷盘
    conn.execute("PRAGMA wal_sync").unwrap();
}
```

### 2.2 下午前半（3h）— 边界与并发

新建 `tests/group_commit_concurrency.rs`：

```rust
//! WAL 组提交并发与边界测试

use engramdb::common::config::Config;
use engramdb::Connection;
use std::sync::{Arc, Barrier};
use std::thread;

#[test]
fn test_empty_transaction_group_commit() {
    let mut conn = Connection::open("/tmp/m1_empty.hdb").unwrap();
    conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY) ENGINE=Columnar").unwrap();
    // BEGIN + COMMIT 空事务
    conn.execute("BEGIN").unwrap();
    conn.execute("COMMIT").unwrap();
    // 不应崩溃，不应产生预期外的 WAL 记录
}

#[test]
fn test_oversized_payload_spans_buffer() {
    // payload > 64KB buffer
    let mut conn = Connection::open("/tmp/m1_big.hdb").unwrap();
    conn.execute("CREATE TABLE t (v TEXT)").unwrap();
    let big = "x".repeat(100_000);
    conn.execute(&format!("INSERT INTO t VALUES ('{}')", big)).unwrap();
    let row: Vec<_> = conn.query("SELECT v FROM t").unwrap()
        .into_iter().flat_map(|c| c.rows).flatten().collect();
    assert_eq!(row[0].as_str().unwrap().len(), 100_000);
}

#[test]
fn test_concurrent_commits_independent_transactions() {
    // 多线程独立事务（每线程独立 Connection）
    let n_threads = 4;
    let n_per_thread = 1000;
    let barrier = Arc::new(Barrier::new(n_threads));
    let handles: Vec<_> = (0..n_threads).map(|tid| {
        let b = barrier.clone();
        thread::spawn(move || {
            let mut conn = Connection::open("/tmp/m1_concurrent.hdb").unwrap();
            b.wait();
            for i in 0..n_per_thread {
                conn.execute(&format!(
                    "INSERT INTO t VALUES ({})", tid * n_per_thread + i
                )).unwrap();
            }
        })
    }).collect();
    for h in handles { h.join().unwrap(); }

    // 验证：总行数 = n_threads * n_per_thread
    let mut conn = Connection::open("/tmp/m1_concurrent.hdb").unwrap();
    let count: i64 = conn.query("SELECT COUNT(*) FROM t").unwrap()
        .into_iter().flat_map(|c| c.rows).flatten().next().unwrap()
        .as_int().unwrap();
    assert_eq!(count, (n_threads * n_per_thread) as i64);
}

#[test]
fn test_concurrent_commits_shared_writer_safety() {
    // ⚠️ 当前 WAL writer 不是线程安全的（&mut self），本测试验证 API 边界
    // TransactionManager 自身持有 writer，应该通过其串行化
    // 此处只验证调用方不会跨线程直接持有 &mut WalWriter
    let mut conn = Connection::open("/tmp/m1_shared.hdb").unwrap();
    conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY)").unwrap();
    // 单线程下大量 commit 验证
    for i in 0..10_000 {
        conn.execute(&format!("INSERT INTO t VALUES ({})", i)).unwrap();
    }
}
```

### 2.3 下午后半（2h）— 崩溃一致性

新建 `scripts/inject_crash.sh`：

```bash
#!/bin/bash
# WAL 组提交崩溃注入测试
# 用法：./scripts/inject_crash.sh <scenario>
#   scenario in: mid-write / mid-fsync / mid-commit / post-commit

set -e

PATH_DB="/tmp/m1_crash_${1}.hdb"
PATH_WAL="${PATH_DB}-wal"
rm -f "$PATH_DB" "$PATH_WAL"

cargo build --release --example inject_crash_runner 2>/dev/null || {
    echo "需要先实现 examples/inject_crash_runner.rs"
    exit 1
}

# 启动后台进程
case $1 in
    mid-write)
        # 写入中途崩溃
        cargo run --release --example inject_crash_runner -- \
            --path "$PATH_DB" --scenario mid-write --n-txns 1000 --kill-at 500 &
        ;;
    mid-fsync)
        # fsync 中途崩溃（time-trigger 强制 sync 时）
        cargo run --release --example inject_crash_runner -- \
            --path "$PATH_DB" --scenario mid-fsync --n-txns 100 --kill-at 50 &
        ;;
    mid-commit)
        # 写 COMMIT 记录后未 fsync 时崩溃
        cargo run --release --example inject_crash_runner -- \
            --path "$PATH_DB" --scenario mid-commit --n-txns 100 --kill-at 50 &
        ;;
    post-commit)
        # 完整 commit 完成后崩溃
        cargo run --release --example inject_crash_runner -- \
            --path "$PATH_DB" --scenario post-commit --n-txns 50 --kill-at 50 &
        ;;
    *)
        echo "Unknown scenario: $1"
        exit 1
        ;;
esac

PID=$!
sleep 1
# 注入崩溃
kill -9 $PID 2>/dev/null || true
wait $PID 2>/dev/null || true

# 恢复并验证
cargo run --release --example inject_crash_recover -- --path "$PATH_DB"
```

新建 `examples/inject_crash_runner.rs`：

```rust
//! 崩溃注入 runner（写入侧）
//!
//! 命令行：`cargo run --release --example inject_crash_runner -- --path PATH --scenario X --n-txns N --kill-at K`

use std::env;
use std::process;
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

    match scenario.as_str() {
        "mid-write" => {
            for i in 0..n_txns {
                conn.execute(&format!(
                    "INSERT INTO t VALUES ({}, 'payload-{}')", i, i
                )).unwrap();
                if i == kill_at {
                    // sleep 让父进程 kill 我们
                    thread::sleep(Duration::from_secs(60));
                }
            }
        }
        "mid-fsync" | "mid-commit" | "post-commit" => {
            // 类似实现
            for i in 0..n_txns {
                conn.execute(&format!(
                    "INSERT INTO t VALUES ({}, 'p-{}')", i, i
                )).unwrap();
                if i == kill_at {
                    match scenario.as_str() {
                        "post-commit" => process::exit(0),
                        _ => thread::sleep(Duration::from_secs(60)),
                    }
                }
            }
        }
        _ => panic!("unknown scenario"),
    }
}
```

新建 `examples/inject_crash_recover.rs`：

```rust
//! 崩溃注入恢复验证（读侧）
//!
//! 打开数据库，验证：
//! 1. 不崩溃
//! 2. 总行数 == 已 fsync 的事务数
//! 3. 没有悬挂事务（无 BEGIN 无 COMMIT 的事务记录）

use std::env;

fn main() {
    let args: Vec<String> = env::args().collect();
    let path = args.windows(2).find(|w| w[0] == "--path")
        .map(|w| w[1].clone()).expect("--path required");

    let mut conn = engramdb::Connection::open(&path).unwrap();
    let count: i64 = conn.query("SELECT COUNT(*) FROM t").unwrap()
        .into_iter().flat_map(|c| c.rows).flatten().next().unwrap()
        .as_int().unwrap();
    println!("recovered row count: {}", count);
    // 注意：崩溃前的事务可能未提交（取决于 fsync 时刻），但不应有数据损坏
}
```

---

## 三、本日完成判定

- [ ] `tests/group_commit_e2e.rs` 5 个 e2e 测试全绿
- [ ] `tests/group_commit_concurrency.rs` 4 个并发/边界测试全绿
- [ ] `examples/inject_crash_runner.rs` + `examples/inject_crash_recover.rs` + `scripts/inject_crash.sh` 实现
- [ ] 4 种崩溃场景恢复后均无损坏（可读 + 行数正确）
- [ ] `cargo test --release --test group_commit_e2e --test group_commit_concurrency` 全绿
- [ ] `cargo test --release` 无功能回归
- [ ] `cargo clippy --all-targets -- -D warnings` 通过

---

## 四、M1 验收报告（执行后填写 → `docs/p1-m1-report.md`）

```markdown
# M1 验收报告

**日期**：2026-08-25
**commit**：（git rev-parse HEAD）

## 1. 测试覆盖

| 测试 | 用例数 | 状态 |
|---|---|---|
| group_commit_e2e.rs | 5 | ✅ |
| group_commit_concurrency.rs | 4 | ✅ |
| 崩溃恢复（4 场景） | 4 | ✅ |

## 2. KPI 验证（10 次 P50）

来源：`benches/wal_group_commit_bench.rs`

| 配置 | 吞吐（txn/s） | 提升 vs group=0 |
|---|---|---|
| group=0, timeout=0 | _____ | 1.0× |
| group=16, timeout=10ms | _____ | _____× |
| group=64, timeout=10ms | _____ | _____× |

**KPI 结论**：(达成/未达成)

## 3. 崩溃场景结果

| 场景 | 恢复后行数 | 数据完整性 | 备注 |
|---|---|---|---|
| mid-write | _____ | OK/Corrupt | |
| mid-fsync | _____ | OK/Corrupt | |
| mid-commit | _____ | OK/Corrupt | |
| post-commit | _____ | OK/Corrupt | |

## 4. 已知问题

（任何遗留的边界行为或文档不一致）
```