# EngramDB 统一 Bench 报告（B3 · U1–U8）

> 由 `scripts/bench_unified.sh` 一键生成，每次运行整体重写；数字真源 = **WSL2 固定机**，勿手改。原始日志 `target/bench_unified.log`。

## 元数据头（强制字段）

| 字段 | 值 |
|---|---|
| git SHA | `e48a027`（branch `main`，工作区改动 10 处） |
| CPU | Intel(R) Core(TM) Ultra 5 338H × 12 vCPU |
| 内核/OS | `6.18.58.1-microsoft-standard-WSL2` / Ubuntu 26.04.1 LTS（WSL2） |
| cargo profile | bench（继承 release，opt-level 3）· rustc 1.99.1 (58cf8f9c3 2026-10-13) |
| 命令行 | `bash scripts/bench_unified.sh` |
| 种子 | 固定 seed=42（v0_13 A-2 查询、m1 装载 SimpleRng）；write_bench / WAL 组提交为顺序写，无随机种子 |
| 行数 | 按场景固定，见下表 |
| 迭代 | 按场景固定（bench 内 ITERS 取中位数/P50），见下表 |
| 冷/热 | **冷启动**：每场景独立进程，/tmp 下 DB 由 bench 每次自清理重建；场景内循环为热缓存；编译耗时单独计入阶段 1（229s），不计入场景耗时 |
| 运行时间 | 2026-11-19 22:53:18 UTC → 2026-11-19 23:19:52 UTC |
| 编译 | `cargo bench --no-run` rc=0，229s |

## 场景覆盖（E3 U1–U8）

| # | 场景 | bench 入口 | 固定参数（行数 · 迭代 · 种子） | 本次状态 |
|---|---|---|---|---|
| U1 | 事务逐行写 | `v0_13_acceptance_bench` | 1000 tx × 1 row；ITERS=5 取中位数（bench 内 A-1）；对照 SQLite | ✅ 已跑（1353s） |
| U2 | 批量/Prepared 写 | `write_bench` | bench 固定：单条 1k 行 / batch_sql 10k+100k(batch=100)+100k(batch=1000) / prepared 10k+100k(batch=100) / compact 50k | ✅ 已跑（4s） |
| U3 | 索引点查 | `v0_13_acceptance_bench` | 1M 行表 × 1000 随机等值；ITERS=5；查询 seed=42（SimpleRng）；bench 内 A-2 | ✅ 已跑（1353s） |
| U4 | 低选择性扫描 | `m1_acceptance_bench` | 10万行装载 seed=42；WHERE val>990 ≈ 1%；ITERS=5；bench 内 M1-1 | ✅ 已跑（4s） |
| U5 | ORDER BY | `m1_acceptance_bench` | 10万行整数列 ORDER BY k；ITERS=5；bench 内 M1-2 | ✅ 已跑（4s） |
| U6 | GROUP BY | `m1_acceptance_bench` | 10万行单列整数 GROUP BY k（1000 组）；ITERS=5；bench 内 M1-3 | ✅ 已跑（4s） |
| U7 | COUNT(*) | `v0_13_acceptance_bench` | 1M 行 COUNT(*) 无 WHERE；ITERS=5；bench 内 A-3 | ✅ 已跑（1353s） |
| U8 | WAL 组提交吞吐 | `wal_group_commit_bench` | 每轮 5000 autocommit INSERT × 10 样本取 P50；group=0/16/64/128（顺序写 id，无随机种子） | ✅ 已跑（4s） |

## 结果摘录（按 bench 运行，节选自原始日志）

### `v0_13_acceptance_bench` — 覆盖 U1 · U3 · U7 · rc=0 · 耗时 1353s

```text
==================================================================
  EngramDB v0.13 验收基准
  轮次: 5    数据规模: 1000000 行
==================================================================
--- A-1: 事务内逐行写入 (1000 事务 × 1 行) ---
  中位数  EngramDB:      2.44 ms  SQLite:      5.22 ms  比值:  0.47x  ✅ PASS
--- A-1b: 单事务批量 INSERT (1 事务 × 1000 行) ---
  中位数  EngramDB:       933 µs  SQLite:       490 µs  比值:  1.91x  ❌ FAIL
--- A-1c: 单事务批量 INSERT (1 事务 × 10000 行) ---
  中位数  EngramDB:     11.47 ms  SQLite:      4.27 ms  比值:  2.69x  ❌ FAIL
--- A-1d: 单事务批量 INSERT (1 事务 × 100000 行) ---
==========================================================
==========================================================
==================================================================
  EngramDB v0.13 验收基准
  轮次: 5    数据规模: 1000000 行
==================================================================
--- A-1: 事务内逐行写入 (1000 事务 × 1 行) ---
  中位数  EngramDB:      2.30 ms  SQLite:      5.02 ms  比值:  0.46x  ✅ PASS
--- A-1b: 单事务批量 INSERT (1 事务 × 1000 行) ---
  中位数  EngramDB:       752 µs  SQLite:       407 µs  比值:  1.85x  ❌ FAIL
--- A-1c: 单事务批量 INSERT (1 事务 × 10000 行) ---
  中位数  EngramDB:     11.29 ms  SQLite:      4.30 ms  比值:  2.63x  ❌ FAIL
--- A-1d: 单事务批量 INSERT (1 事务 × 100000 行) ---
  中位数  EngramDB:      39.65 s  SQLite:     42.62 ms  比值: 930.41x  ❌ FAIL
--- 设置 A-2/A-3 测试数据 (1M 行到 EngramDB + SQLite) ---
  EngramDB setup done in 681.24 ms
  SQLite setup done in 460.52 ms
--- A-2: 索引点查 (1M 行表, 1000 次随机等值) ---
  中位数  EngramDB:       2.57 s  SQLite:      2.38 ms  比值: 1080.68x  ❌ FAIL
--- A-3: COUNT(*) (1M 行, 无 WHERE) ---
  中位数  EngramDB:        77 µs  SQLite:      4.38 ms  比值:  0.02x  ✅ PASS
--- 设置 B-4 测试数据 (100000 行, 无序整数键) ---
  EngramDB setup done in 565.02 ms
  SQLite setup done in 46.42 ms
--- B-4: ORDER BY k 全排序 (100K 行, 单列整数) ---
  中位数  EngramDB:     16.35 ms  SQLite:     27.71 ms  比值:  0.59x  ✅ PASS
--- 设置 A-4 测试数据 (1M 行) ---
  EngramDB setup done in 265.81 ms
  SQLite setup done in 409.76 ms
--- A-4a: WHERE val > 990 (1% 选择性, 1M 行) ---
  中位数  EngramDB:      2.90 ms  SQLite:     26.48 ms  比值:  0.11x  ✅ PASS
--- A-4b: WHERE val > 900 (10% 选择性, 1M 行) ---
  中位数  EngramDB:      3.21 ms  SQLite:     25.81 ms  比值:  0.12x  ✅ PASS
--- A-4c: WHERE val > 500 (50% 选择性, 1M 行) ---
  中位数  EngramDB:      4.44 ms  SQLite:     26.84 ms  比值:  0.17x  ✅ PASS
==================================================================
==================================================================
```

### `write_bench` — 覆盖 U2 · rc=0 · 耗时 4s

```text
【测试 1】单条 INSERT SQL（1000 行）
  耗时: 0.005s
  速度: 192363.5 行/秒
【测试 2】批量 INSERT SQL（共 10000 行，100 行/批，P2+P3 优化）
  耗时: 0.017s
  速度: 602919.7 行/秒 (60.3 万行/秒)
【测试 2】批量 INSERT SQL（共 100000 行，1000 行/批，P2+P3 优化）
  耗时: 0.125s
  速度: 802016.0 行/秒 (80.2 万行/秒)
【测试 3】Prepared Statement 批量（共 10000 行，100 行/批，P0 优化）
  耗时: 0.019s
  速度: 521173.0 行/秒 (52.1 万行/秒)
【测试 4】Prepared Statement 大批量（100000 行，触发 P1 直接列式路径）
  耗时: 0.174s
  速度: 574369.8 行/秒 (57.4 万行/秒)
【测试 5】Delta compact 速度（50000 行，P4 列式 Delta 优化）
  返回行数: 0
```

### `m1_acceptance_bench` — 覆盖 U4 · U5 · U6 · rc=0 · 耗时 4s

```text
  M1-1 WHERE 1% 选择性: 352 µs (目标 < 5.00 ms)  ✅ PASS
  M1-2 ORDER BY 整数列: 20.86 ms (目标 < 30.00 ms)  ✅ PASS
  M1-3 GROUP BY 单列整数: 3.30 ms (目标 < 5.00 ms)  ✅ PASS
  M1-1 WHERE 1% 选择性: 545 µs (目标 < 5.00 ms)  ✅ PASS
  M1-2 ORDER BY 整数列: 18.27 ms (目标 < 30.00 ms)  ✅ PASS
  M1-3 GROUP BY 单列整数: 2.57 ms (目标 < 5.00 ms)  ✅ PASS
```

### `wal_group_commit_bench` — 覆盖 U8 · rc=0 · 耗时 4s

```text
  group=0/0   (true baseline, 每次 fsync): median=503763 txn/s (9.93 ms total) (10 samples)
  group=16/64K/10ms  (Phase 1 默认): median=506496 txn/s (9.87 ms total) (10 samples)
  group=64/64K/10ms  (大组): median=503397 txn/s (9.93 ms total) (10 samples)
  group=128/64K/10ms (超大批): median=518145 txn/s (9.65 ms total) (10 samples)
  group=16/0/0   (仅 size 触发): median=508155 txn/s (9.84 ms total) (10 samples)
  group=0/64K/0  (仅 bytes 触发): median=508024 txn/s (9.84 ms total) (10 samples)
=== 总结 ===
  基线 (true group=0):  503763 txn/s
  group=16 default:     506496 txn/s (1.01×)
  group=64:             503397 txn/s (1.00×)
  group=128:            518144 txn/s (1.03×)
ℹ️  当前环境 fsync 接近零成本，组提交无明显收益 (1.01×)
```

## 缺失与偏差说明

- E3 草案建议 U2 batch=512；`write_bench` 固定 batch=100/1000，未改他人 bench，按实际口径记录。
- U1/U3/U7 共用 `v0_13_acceptance_bench` 一次运行，附带 A-1b/c/d（单事务批量，可视为 U2 参考）、B-4（ORDER BY 100K）、A-4a/b（WHERE 1%/10% 1M）——超出 U1–U8，一并摘录。
- U4/U5/U6 共用 `m1_acceptance_bench` 一次运行（10 万行，E3 建议口径）。
- 本报告数字为 **WSL2 release（profile=bench）**；与历史 Windows/debug 或 CI 数字不可比。
- 场景级筛选用法：`bash scripts/bench_unified.sh u4 u8`——报告只含本次选中场景，全量口径请不带参数重跑。
