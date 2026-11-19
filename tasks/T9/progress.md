# T9 / A-Bench · W0 进度（B3 统一 bench + 可复现报告）

- 分支：`wave0/b3-bench-report`（自 main `0ddcb4d`，CI 43159799886 success）
- 状态：**脚本与报告框架已提交；WSL 全量数字未产出（见下「WSL 运行情况」）**

## 交付物

| 文件 | 内容 |
|---|---|
| `scripts/bench_unified.sh`（新） | 一键统一 bench：场景选择 → 编译门禁 → 逐 bench 重定向日志 → 生成强制元数据头报告 |
| `scripts/benchmarks/unified-report.md` | 报告输出位（本次未生成——脚本收口前未跑完；首次成功运行会自动写入） |
| 本文件 | progress |

报告选 `scripts/benchmarks/`（所有权 `scripts/benchmarks/**`，避免与 `docs/*bench*` 所有权交叉）。

## U1–U8 覆盖（E3 场景表落地，全部可映射到已有 bench，无新发明）

| # | 场景 | bench 入口 | 固定参数 |
|---|---|---|---|
| U1 | 事务逐行写 | `v0_13_acceptance_bench` A-1 | 1000 tx × 1 row，ITERS=5 |
| U2 | 批量/Prepared 写 | `write_bench` | bench 固定：1k 单条 / 10k+100k batch_sql / 10k+100k prepared / 50k compact |
| U3 | 索引点查 | `v0_13` A-2 | 1M 行 × 1000 随机等值，seed=42，ITERS=5 |
| U4 | 低选择性扫描 | `m1_acceptance_bench` M1-1 | 10 万行，WHERE val>990≈1%，seed=42 |
| U5 | ORDER BY | `m1` M1-2 | 10 万行整数列，ITERS=5 |
| U6 | GROUP BY | `m1` M1-3 | 10 万行单列 1000 组，ITERS=5 |
| U7 | COUNT(*) | `v0_13` A-3 | 1M 行，ITERS=5 |
| U8 | WAL 组提交 | `wal_group_commit_bench` | 5000 txn/轮 × 10 样本 P50，group=0/16/64/128 |

- U1/U3/U7 共用 `v0_13_acceptance_bench` 一次运行（附带 A-1b/c/d、B-4、A-4，报告一并摘录）；U4/U5/U6 共用 `m1_acceptance_bench`。去重后 4 个 bench 目标。
- 场景可选：`bash scripts/bench_unified.sh u4 u8`（同一次运行内去重）；无参数 = 全量并整体重写报告。
- 偏差：E3 建议 U2 batch=512，`write_bench` 实际固定 batch=100/1000，未改他人 bench，按实际口径记录。

## 元数据头（强制字段，脚本自动采集）

git SHA(+branch+工作区改动数) · CPU(/proc/cpuinfo model × nproc) · 内核/OS(uname + os-release，标注 WSL2) · cargo profile(bench=release, opt-level 3 + rustc -V) · 命令行(实际 invocation) · 种子(seed=42 SimpleRng / 顺序写标注) · 行数(逐场景表) · 迭代(逐场景 ITERS) · 冷/热(冷启动：独立进程、/tmp DB 每次自清理；编译耗时与场景耗时分离)。

## WSL 运行情况（如实记录）

- 脚本 `bash -n` 语法检查通过；WSL2（Ubuntu 26.04.1，kernel 6.18.58.1-microsoft-standard-WSL2，Intel Core Ultra 5 338H × 12 vCPU，rustc 1.99.1）冒烟调用已启动。
- **未跑完**：`cargo bench --no-run`（全量 bench 目标 release 编译）进行中被会话收口中断（日志停在 criterion/rand 编译，`target/bench_unified.log` ~62KB）；**8 个场景 0 个完成，无数字产出**。
- 后续跑法（任一 WSL 前台会话）：
  ```bash
  bash scripts/bench_unified.sh          # 全量 U1–U8
  bash scripts/bench_unified.sh u4 u8    # 分场景
  ```
  全部输出重定向 `target/bench_unified.log`，进度/摘要只回显每场景 `rc/耗时` 行；报告自动写 `scripts/benchmarks/unified-report.md`。
- 已知环境约束：本会话无法后台常驻进程（工具调用结束即被杀），长跑须前台长超时。

## 未动（所有权合规）

未改 `src/**`、`tests/**`、`.github/**`；工作区遗留他人的 `src/executor/operators/hash_join.rs`、`tests/differential_sqlite.rs` 改动未纳入本次 commit。
