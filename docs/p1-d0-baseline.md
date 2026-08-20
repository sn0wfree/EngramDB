# D0 — Phase 1 基线建立

> **日期**：2026-08-24（周一）
> **负责人**：（待分配）
> **目的**：在改造前冻结所有可量化指标，建立对比基线

---

## 一、目标

1. 跑通所有相关 production benchmarks，每项 **10 次 P50** 取中位数
2. 用 dhat 记录**查询执行期分配次数绝对值**（决策 A 主指标）
3. 用 heaptrack 记录**列扫描内存峰值**
4. 输出可机器解析的基线报告

---

## 二、基线矩阵

### 2.1 必须跑（核心 6 项）

| Bench | 覆盖指标 | 命令 | 期望产出 |
|---|---|---|---|
| `core_bench` | 综合：扫描/聚合/点查/写入混合 | `cargo bench --bench core_bench -- --sample-size 10` | 综合吞吐/延迟 |
| `m1_acceptance_bench` | 写入/事务（A-1） | `cargo bench --bench m1_acceptance_bench -- --sample-size 10` | 写入吞吐 |
| `m2_memory_bench` | 内存引擎点查 | `cargo bench --bench m2_memory_bench -- --sample-size 10` | 点查延迟 |
| `m3_log_bench` | 日志引擎写入/扫描 | `cargo bench --bench m3_log_bench -- --sample-size 10` | 写读吞吐 |
| `v018_write_bench` | 写入优化（LogEngine 路径） | `cargo bench --bench v018_write_bench -- --sample-size 10` | 批量/逐行对照 |
| `select_star_bench` | SELECT * 全扫 | `cargo bench --bench select_star_bench -- --sample-size 10` | 全扫吞吐 |

### 2.2 必须跑（专项，对应 Phase 1 KPI）

| Bench | 对应 KPI | 命令 |
|---|---|---|
| （新建）`wal_group_commit_bench` | M1：事务吞吐 | `cargo bench --bench wal_group_commit_bench -- --sample-size 10` |
| （新建）选择性扫描 bench | M2：1% 选择性查询延迟 | 见 §四 |

### 2.3 可选跑（参考）

- `bench_full`（综合运行时）
- `compression_bench` / `compression_bench_v2`
- `vector_bench` / `index_bench` / `limit_bench`
- `p35_bloom_bench` / `compact_strategy_*_bench` / `v0_13_acceptance_bench`

---

## 三、dhat 配置

### 3.1 启用 dhat 全局分配器

`Cargo.toml` 添加 dev-dependency：

```toml
[dev-dependencies]
dhat = "0.3"
```

`src/lib.rs` 末尾添加：

```rust
#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;
```

### 3.2 查询执行期 dhat 抽样

新建 `examples/dhat_query_trace.rs`：

```rust
//! dhat 抽样：典型查询场景的分配次数与总分配字节
//!
//! 运行：`cargo run --release --features dhat-heap --example dhat_query_trace`

#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

fn main() {
    #[cfg(feature = "dhat-heap")]
    let _profiler = dhat::Profiler::new_heap();

    let path = format!("/tmp/dhat_baseline_{}.hdb", std::process::id());
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}-wal", path));

    let mut conn = engramdb::Connection::open(&path).unwrap();

    // 1. 写入 100k 行
    conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY, v TEXT, ts INT64)").unwrap();
    for i in 0..100_000 {
        conn.execute(&format!("INSERT INTO t VALUES ({}, 'row-{}', {})", i, i, i)).unwrap();
    }

    // 2. 全表扫描（典型分析查询）
    conn.execute("SELECT COUNT(*), SUM(ts) FROM t").unwrap();
    conn.execute("SELECT * FROM t WHERE ts > 50000 LIMIT 1000").unwrap();

    // 3. 等值过滤（典型 OLTP 查询）
    conn.execute("SELECT * FROM t WHERE id = 50000").unwrap();
    for _ in 0..100 {
        conn.execute("SELECT * FROM t WHERE id = 12345").unwrap();
    }
}
```

### 3.3 解析 dhat 输出

```bash
cargo run --release --features dhat-heap --example dhat_query_trace
# dhat 在结束时打印 dhat-heap.json 到当前目录
python3 scripts/parse_dhat.py dhat-heap.json > dhat_baseline.txt
```

`scripts/parse_dhat.py` 提取：

- `total allocations`（分配次数绝对值）
- `total bytes`（分配字节数）
- `allocations by site`（按代码行号归类的热点）

---

## 四、选择性扫描 bench 模板

新建 `benches/selectivity_scan_bench.rs`：

```rust
//! 选择性扫描基准：1% / 5% / 50% / 100% 选择性下查询延迟
//!
//! 运行：`cargo bench --bench selectivity_scan_bench -- --sample-size 10`
//!
//! 对应 KPI：选择性 1% 查询延迟 ≥ -50%

use std::time::{Duration, Instant};
use engramdb::{Connection, Value};

const ROWS: usize = 100_000;

fn run(scenario: &str, conn: &mut Connection, n: usize) -> Duration {
    let t0 = Instant::now();
    for _ in 0..n {
        match scenario {
            "1pct" => { conn.execute("SELECT * FROM t WHERE id < 1000").unwrap(); }
            "5pct" => { conn.execute("SELECT * FROM t WHERE id < 5000").unwrap(); }
            "50pct" => { conn.execute("SELECT * FROM t WHERE id < 50000").unwrap(); }
            "all" => { conn.execute("SELECT * FROM t").unwrap(); }
            _ => unreachable!(),
        }
    }
    t0.elapsed() / n as u32
}

fn main() {
    let path = format!("/tmp/selectivity_{}.hdb", std::process::id());
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}-wal", path));
    let mut conn = Connection::open(&path).unwrap();
    conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY, v TEXT)").unwrap();
    for i in 0..ROWS {
        conn.execute(&format!("INSERT INTO t VALUES ({}, 'row-{}')", i, i)).unwrap();
    }

    let n = 1000; // 每个场景执行 1000 次
    for scenario in ["1pct", "5pct", "50pct", "all"] {
        // 跑 10 次取 P50
        let mut times: Vec<Duration> = (0..10).map(|_| run(scenario, &mut conn, n)).collect();
        times.sort();
        let p50 = times[5];
        println!("{}: {:?}/query", scenario, p50);
    }
}
```

---

## 五、wal_group_commit_bench 模板

新建 `benches/wal_group_commit_bench.rs`：

```rust
//! WAL 组提交基准：group=0/16/64 + timeout=10ms 四档对照
//!
//! 运行：`cargo bench --bench wal_group_commit_bench -- --sample-size 10`
//!
//! 对应 KPI：小事务写入吞吐 ≥ 3×（group=16 vs group=0）

use std::time::{Duration, Instant};
use engramdb::common::config::Config;
use engramdb::Connection;

const ITERS: usize = 10;
const TXNS_PER_ITER: usize = 10_000;

fn median(mut samples: Vec<Duration>) -> Duration {
    samples.sort();
    samples[samples.len() / 2]
}

fn fmt_rate(d: Duration, txns: usize) -> String {
    format!("{:.0} txn/s", txns as f64 / d.as_secs_f64())
}

fn bench(group_size: usize, timeout_ms: u64) -> Duration {
    let path = format!("/tmp/wal_gc_{}_{}.hdb", group_size, std::process::id());
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}-wal", path));

    let mut cfg = Config::default();
    cfg.wal_group_commit_size = group_size;
    cfg.wal_group_commit_timeout_ms = timeout_ms;

    let mut conn = Connection::open_with_config(&path, cfg).unwrap();
    conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY, v TEXT)").unwrap();

    let t0 = Instant::now();
    for i in 0..TXNS_PER_ITER {
        conn.execute(&format!("INSERT INTO t VALUES ({}, 'row')", i)).unwrap();
    }
    let d = t0.elapsed();
    conn.close().unwrap();
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(format!("{}-wal", path));
    d
}

fn main() {
    println!("=== WAL Group Commit Bench ===");

    let cases = [
        ("group=0",  0,  0),
        ("group=16", 16, 10),
        ("group=64", 64, 10),
    ];

    for (name, size, timeout) in cases {
        let mut samples = Vec::new();
        for _ in 0..ITERS {
            samples.push(bench(size, timeout));
        }
        println!("{}: median={:?}, rate={}",
                 name, median(samples.clone()),
                 fmt_rate(median(samples), TXNS_PER_ITER));
    }
}
```

---

## 六、输出文件

| 文件 | 内容 |
|---|---|
| `bench_baseline_<bench>.log` | 每个 bench 的 criterion 输出 |
| `dhat_baseline.txt` | dhat 解析后的分配次数与热点 |
| `heaptrack_baseline.txt` | heaptrack 摘要（峰值 RSS、分配字节） |
| `docs/p1-d0-baseline.md` | 本文档执行后填入数字 |

---

## 七、本文档（执行后填写模板）

```markdown
# D0 基线报告

**日期**：2026-08-24
**机器**：（CPU/内存/OS）
**jemalloc 版本**：（从 Cargo.lock）
**commit**：（git rev-parse HEAD）

## 1. 写入吞吐（10 次 P50）

| 场景 | 吞吐量 | 来源 |
|---|---|---|
| 批量 import_columns 100 万行 | _____ 万行/秒 | v018_write_bench |
| 逐行 INSERT Batcher 开 20 万行 | _____ 行/秒 | v018_write_bench |
| 逐行 INSERT Batcher 关 20 万行 | _____ 行/秒 | v018_write_bench |

## 2. 扫描吞吐（10 次 P50）

| 场景 | 吞吐量 | 来源 |
|---|---|---|
| 列存全扫 100k 行 | _____ 行/秒 | m3_log_bench |
| SELECT * 全扫 100k 行 | _____ 行/秒 | select_star_bench |
| 选择性 1% 查询 | _____ ms/query | selectivity_scan_bench |

## 3. 点查延迟（10 次 P50）

| 场景 | 延迟 | 来源 |
|---|---|---|
| MemoryEngine 主键点查 | _____ μs | m2_memory_bench |
| ColumnarEngine 主键点查 | _____ μs | m1_acceptance_bench |

## 4. WAL 组提交（10 次 P50）

| 配置 | 吞吐 | 提升 vs group=0 |
|---|---|---|
| group=0, timeout=0 | _____ txn/s | 1.0× |
| group=16, timeout=10ms | _____ txn/s | _____× |
| group=64, timeout=10ms | _____ txn/s | _____× |

## 5. 内存基线（dhat）

| 场景 | 分配次数 | 总分配字节 |
|---|---|---|
| 100k 行写入 | _____ | _____ MB |
| 全表扫描 | _____ | _____ MB |
| 选择性 1% 查询 | _____ | _____ KB |

## 6. 异常与备注

（任何 P50 之外的抖动、CI 失败、编译警告等）
```

---

## 八、D0 完成判定

- [ ] 6 个核心 bench 全跑，10 次 P50 数字记录
- [ ] dhat 基线（100k 写入 + 全扫 + 1% 选择性）记录分配次数
- [ ] heaptrack 内存峰值记录
- [ ] `docs/p1-d0-baseline.md` 填入数字
- [ ] 所有数字 commit 进 git（`phase1/d0-baseline-data`）
- [ ] 无新增 CI 警告（`cargo clippy -D warnings` 通过）