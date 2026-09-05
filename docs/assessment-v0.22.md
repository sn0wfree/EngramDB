# EngramDB v0.22 可用性 / 稳健性 / 性能评估报告

> 日期：2026-08-30
> 方式：静态代码审计（存储/WAL/MVCC、SQL/执行器、测试体系三条线并行深查）+ 历史基准报告交叉核对
> 结论速览：**原型成熟度较高，但尚未达到"可放心投入使用"——存在 3 类必须先修的正确性/持久化缺陷**；性能证据方向性良好但缺 v0.22 统一基线。

---

## 一、总体结论

| 维度 | 评级 | 一句话结论 |
|---|---|---|
| 架构设计 | ★★★★☆ | 多引擎 + 列存/Delta 混合 + WAL/MVCC 骨架正确，分层清晰，注释密度高 |
| 稳健性 | ★★★☆☆ | 求值内核防御良好；**持久化原语（文件头原子性）和若干用户可触发的 panic 是主要短板** |
| SQL 正确性 | ★★★☆☆ | 三值逻辑/聚合 NULL 语义正确；但 **OFFSET 被静默忽略、无 DROP TABLE、计划缓存失效不完备** |
| 性能 | ★★★★☆（待验证） | 分析型负载方向性优异；点查弱于 SQLite；**缺 v0.22 统一基线，旧数字不可全信** |
| 测试体系 | ★★★☆☆ | ~810 个测试量大面广；**崩溃注入未自动化、无 SQL 语义回归、无模糊测试** |
| 投入使用判断 | **NO-GO（当前）** | 修复 P0 清单 + 完成 SQLite 差分测试与崩溃注入自动化后可达 GO |

---

## 二、必须先修的问题（P0）

> **状态更新（v0.22.1）**：以下 P0-1（文件头原子性）、P0-2（打开路径 panic）、P0-5（OFFSET/缓存失效）已修复并通过代码审查级自查；修复内容见 CHANGELOG 0.22.1 与 `test_header_torn_write_recovers_from_replica` 等回归测试。**尚未修复**：P0-3（裸 INSERT 攒批崩溃丢行）、P0-4（恢复半途失败固化）、P0-6（MVCC first-committer-wins）、P0-7~10（用户可触发 panic 面）。修复未经编译验证（本机 SAC 限制），需在可编译环境跑 `cargo test` 确认。

### 持久化与崩溃安全
1. **主文件头非原子更新**：`storage/mod.rs:1470-1479` 数据 append 后原地重写 offset 0 的文件头，仅 flush 无 fsync，且 FileHeader **无 checksum、无双头、无 rename 原子替换**（mmap 路径有，主路径没有）。崩溃在 header 写一半 → 库打不开且无法检测。**修法：双 header 交替写 + CRC + fsync 序。**
2. **损坏文件打开即 panic**：`column_store.rs:1286-1288` `data_from_bytes` 对 min/max 段只做了不对称的边界检查，文件尾部截断时 slice 越界 panic → 数据库打不开。**修法：全面改用 checked 读取 + 返回 EngramDbError::Corruption。**
3. **裸 INSERT 攒批崩溃丢行**：`executor/operators/insert.rs:96-123` — 语句返回成功后行仍驻留内存 batcher（异步窗口），进程崩溃丢"已确认"的行，违背 ACID 承诺。**修法：batcher 缓冲行落 WAL，或 flush 前不返回成功。**
4. **恢复半途失败固化**：`wal/recovery.rs:202-215` 重放中途失败仍继续 checkpoint，可能把半恢复状态持久化。**修法：重放失败时中止 checkpoint 并报错。**

### SQL 正确性
5. **OFFSET 静默忽略**：`parser.rs:1074-1085` 只读 limit，`LIMIT 5 OFFSET 3` 返回**错误的行**（比报错危险）。同时 `LIMIT 5+1` 非字面量被解析成"无限制"全表返回。**修法：解析 OFFSET；非字面量 LIMIT 报 Parse error。**
6. **MVCC 缺 first-committer-wins**：`mvcc.rs:100-109` 写冲突只检查未提交版本；T1 提交后 T2（快照早于 T1）再写同 key 无冲突检测 → 快照隔离下丢失更新。单连接下影响有限，但 `begin()` API 是公开的。**修法：write() 比较已提交链头 commit_ts 与本事务 start_ts。**
7. **计划缓存失效不完备**：`lib.rs:176-184` DDL 清单遗漏 `CreateView/DropView`；数据大批量变更后统计过期但缓存计划照旧复用 → 可能选出灾难计划。**修法：DROP VIEW 加入失效清单；写操作后使统计/计划降级。**

### 用户可触发的 panic（DoS 面）
8. DECIMAL scale 无校验：`parser.rs:1678` `*s as u8` 截断 + `expression.rs:2146` `10i128.pow` → `DECIMAL(1,50)` 触发溢出。
9. 窗口帧边界无界加法/逆序切片：`window.rs:263-273`（`N FOLLOWING` 溢出、start>end 未归一化）。
10. `-i64::MIN` 未用 checked_neg：`expression.rs:2030`。

（上述均为 debug panic / release 静默错值，量小好修，建议一并处理。）

---

## 三、做得好的部分（不需要动）

- **WAL 链路**：CRC32 全覆盖 + 损坏逐字节重同步（`wal/reader.rs:85`）+ 幂等重放，torn write 处理正确，有测试。
- **MVCC GC**：`oldest_start_ts` 水位线设计正确，活跃快照所需版本不会被误删（`mvcc.rs:219-226,359`）。
- **表达式内核**：checked_add/checked_mul、除零返回 NULL、三值逻辑、聚合 NULL 语义全部正确（`expression.rs:1901-1981`、`aggregate.rs:62,193`）；生产路径 unwrap 干净（都在 `#[cfg(test)]`）。
- **谓词下推保守正确**：不穿越 JOIN/聚合，LEFT JOIN NULL 补行语义有回归测试。
- **攒批读己之写**：非裸 INSERT 前置 flush、ROLLBACK 豁免等语义经过有意识迭代（P0-1/P0-2 注释可循）。
- **零 unsafe 于核心路径**：9 处 unsafe 全在 mmap 模块，均为标准 memmap2 用法。

## 四、SQL 能力缺口（相对 SQLite，影响迁移使用）

无 `DROP TABLE` / `DROP INDEX`（仅 TRUNCATE）、无外键执行、无触发器、无 COLLATE、窗口帧仅 ROWS 且有缺陷、f64 除零返回 NaN（SQLite 返回 NULL）。**如果目标是从 SQLite 迁移 AI-Agent 存储工作负载，DROP TABLE 和 OFFSET 是硬阻塞。**

## 五、性能证据盘点

| 来源 | 日期/版本 | 关键数字 | 可信度 |
|---|---|---|---|
| v0.13 验收报告 | 2026-08-05 | 事务逐行写 2.63x 慢于 SQLite（达标线 10x）；批量导入 0.93x（反超）；1% 选择性 WHERE 快 4.3x；点查慢 3.39x；COUNT(*) 快 100x | 高（有完整验收流程） |
| DIAGNOSTIC_REPORT | 2026-08-06 | 写路径 fsync 主导，CPU 已最优；group_commit 16 可再提 p95 ~16x | 高 |
| performance-report.md | 标注"v0.29 Phase 6 / 2026-09-04"（日期异常） | 批量导入 2400 万行/秒；等值查询慢 16x；Bloom 跳读 157x | 中（版本号/日期疑误） |
| 02-benchmark-report.md | v0.6 模拟版 | 已严重过时 | 低 |

**结论**：分析型扫描/聚合/批量导入性能方向性良好；**点查与 SQLite 有 3-16x 差距**（不同报告口径不一）。**缺一份 v0.22 在同一台机器上的全量对比基线** —— 这正是下一步 SQLite 深入测试要补的。

## 六、下一步：SQLite 深入测试方案

1. **差分正确性测试**（tests/differential_sqlite.rs，已创建）：同一批 SQL 在 EngramDB 和 rusqlite 上执行、逐行比对结果 —— 相当于把 SQLite 当测试套件的 oracle。覆盖：DML、WHERE/ORDER/LIMIT、聚合/GROUP BY/HAVING、JOIN、子查询、IN/BETWEEN/LIKE、NULL 语义、类型往返。
2. **深度性能基准**（benches/deep_vs_sqlite_bench.rs，已创建）：在现有 4 场景外补充：二级索引范围扫、GROUP BY、UPDATE/DELETE、prepared 批量写、事务组提交、compact 前后查询影响、JSON 列、1M 行规模；统一 `PRAGMA journal_mode=WAL; synchronous=NORMAL` 口径与 v0_13 验收线对齐。
3. **崩溃注入自动化**：把 examples/inject_crash_runner 从手工工具升级为 `cargo test` 集成测试（随机 kill 点 × 100 次迭代 + 每次恢复后数据完整性断言）。
4. **执行环境**：本机 Smart App Control 拦截 cargo 产物，需在 WSL2（推荐）或其他 Linux 机器上运行；运行命令见报告末尾。

## 附：运行命令（WSL2/其他机器）

```bash
cd /mnt/d/Github/EngramDB   # 建议先 git clone 进 WSL 原生文件系统（ext4），IO 快 5-10x
cargo test --release --test-threads=1          # 全量功能测试
cargo test --release --test differential_sqlite -- --nocapture  # SQLite 差分正确性
cargo bench --bench deep_vs_sqlite_bench       # SQLite 深度性能对比
cargo bench --bench v0_13_acceptance_bench     # 验收线复核
```
