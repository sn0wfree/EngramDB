# EngramDB 多 Agent 开发计划

> 日期：2026-10  
> 目标：用多 Agent 并行推进 **剩余工作**（默认范围 = 关账 v0.23 + 清 P0/基础设施到可 GO；可选波次延伸 v0.24–v0.25）  
> 硬约束：Windows SAC 本地不可编译 → **一切代码变更必须过 CI 或 WSL 脚本才算完成**

---

## 0. 范围与成功标准

### 本计划默认范围（Wave 0–2）

| 波次 | 内容 | 退出标准 |
|---|---|---|
| **W0 关账 v0.23** | A3、B2、B3、B4 收尾 | 路线图退出标准三条全绿；`docs` 更新版本叙事 |
| **W1 P0 正确性** | 列存 DELETE tombstone+compact；崩溃注入自动化；P0 修复 WSL/CI 复验 | 历史包袱 P0 清零；`cargo test` 含 crash 集成测 |
| **W2 基础设施** | CI 阻塞恢复、警告治理、统一 bench 报告管道、clippy 门禁 | CI 默认 feature 全阻塞绿；B3 报告可一键再生 |

### 可选后续（不在首轮并行）

- **W3 v0.24**：C1–C3 DataPoint、D1 Graph、E1 arena（先 profile）
- **W4 v0.25**：F1–F4（见 `v0.25-Agent记忆映射.md`）

### 全局成功定义（GO 候选）

1. v0.23 退出标准满足（含 **A1–A2 长跑不膨胀**）  
2. P0 挂账清零且在 CI/WSL 复验  
3. 崩溃注入（确定性 kill 点）+ SQLite 差分在 CI 默认路径运行  
4. 统一 bench 报告入库（固定种子/场景/口径 + **参考环境记录**）  
5. 无**未文档化**缺口；已文档化豁免（如 LogEngine 语义限制）可接受  
6. 门禁类测试不得「只 println 不 assert」（A4b 模式禁止）

---

## 0.1 评估修订记录（2026-10 复盘）

| 发现 | 处置 |
|---|---|
| 漏「长跑不膨胀」退出标准 | 新增 W0-P1 任务 |
| A4b 软断言假绿风险 | W0-P2：改为 assert 或显式 ignore+豁免 |
| 列存 DELETE 2–3 天低估 | 估 4–5 天，设计先行，W1 串行深工 |
| 随机 crash×100 必 flaky | 确定性 kill 点进 PR；随机压力 nightly |
| B3 无参考环境 | 报告强制记录机器/命令/种子 |
| A3 单开 Agent 开销过大 | 并入 A-Persist，不单开分支 |
| 测试文件单写过死 | 区域共写 + A-Test review |
| `-D warnings` 与在途分支冲突 | W0/W1 只「不新增」；批量清零在 W0 合并后 |
| clippy --all-features 含第三方冲突 | 只保证默认 feature 阻塞 |
| config.rs 未入热点清单 | 已列入单写者 |

---

## 1. 组织原则（防多 Agent 互踩）

### 1.1 文件所有权（写权限按 Agent 切分）

| 区域 | 主责 Agent | 禁止他 Agent 直接改 |
|---|---|---|
| `src/wal/**`, `src/txn/**` | **A-Persist** | 其他 |
| `src/storage/{table,column_store,delta_store,compact*}/**` | **A-Storage** | 其他 |
| `src/executor/operators/{hash_join,filter,projection}/**` | **A-SQL** | 其他 |
| `src/sql/{planner,optimizer}/**` | **A-SQL**（与执行器联调时开 PR） | 其他 |
| `tests/differential_sqlite.rs`, `tests/phase0*` | **A-Test** | 功能 Agent 只加 case 分支文件 |
| `benches/**`, `scripts/bench_*`, `docs/*bench*` | **A-Bench** | 其他 |
| `.github/workflows/**`, `scripts/wsl_*`, CI 相关 | **A-Infra** | 其他 |
| `docs/**` 路线图/清单 | **Lead（主会话）** | Agent 只写自己条目的 progress |

共享热点（`src/lib.rs`、`src/storage/mod.rs`、`src/common/config.rs`、`Cargo.toml`）：**仅 Lead 或当周指定单一 Agent 改**；他人提需求到 Lead 合并。

**测试文件**：按功能区共写（A-SQL 可加 `differential_sqlite` 中 JOIN case），**A-Test 任 review + 基线维护**；冲突以 A-Test 为裁定人。

### 1.2 分支与提交

```
main ──────────────────────────────────────► 稳定
  ├── wave0/a3-checkpoint-trigger
  ├── wave0/b2-join-typed-keys
  ├── wave0/b3-bench-report
  ├── wave0/b4-diff-gaps
  ├── wave1/p0-columnar-delete
  ├── wave1/p0-crash-test
  └── wave2/ci-gates
```

- 一 Agent 一分支；**PR/合并只经 Lead 或串行 merge queue**
- Commit 约定：`area: 中文短句`（对齐现有 `feat:` / `test+docs:` 风格）
- 禁止 Agent 直接推 `main`；禁止互相 rebase 对方分支

### 1.3 完成定义（每个任务卡）

```
[ ] 实现 + 单元/集成测试
[ ] WSL 或 CI 绿（贴 job/日志摘要到 progress）
[ ] 文档：历史包袱/路线图对应行勾选或改状态
[ ] git commit（阶段完成时）
[ ] 写 tasks/<id>/progress.md（若用 task 绑定）
```

**没有可复现的测试/CI 证据 = 未完成。**

### 1.4 通信协议

| 事件 | 动作 |
|---|---|
| 需要改他人文件 | 停，向 Lead 发「阻塞：缺 X 权限」 |
| 发现依赖冲突 | Lead 裁定顺序，不并行改同一 hunk |
| 阶段完成 | Lead 更新路线图 + `历史包袱清单` + git commit |
| 连续 2 次 CI 红 | 该 Agent 降级为只写测试/复现，不继续堆功能 |

---

## 2. Agent 角色与任务包

### 角色一览

```
Lead（主会话）
  ├─ A-Persist   持久化/事务：A3、WAL、崩溃、文件头
  ├─ A-Storage   存储：列存 DELETE、compact、tombstone
  ├─ A-SQL       SQL/执行器：B2 JOIN、B4 剩余差分面
  ├─ A-Bench     性能：B3 统一报告
  ├─ A-Test      测试基建：崩溃自动化、差分 harness、闸门
  └─ A-Infra     CI/工具链：阻塞恢复、警告、WSL、clippy
```

首轮 **最多 4 路并行**（避免 CI 排队与 merge 冲突）；W1 起 A-Storage 与 A-Persist 串行点增多时降为 3 路。

---

### W0 — 关账 v0.23（**3 路**并行；A3 不单开）

#### A-Persist · A3 + WAL 长跑（P1/P0，~2d，同一分支 `wave0/persist-a3-longrun`）

- **交付 A3**：可配置 checkpoint 阈值（WAL 字节 / 时间）；`Config` 字段 + 自动触发  
- **交付 P0**：**长跑不膨胀**断言（循环写+checkpoint 后 WAL/段文件体积有上界），满足路线图退出标准第三条  
- **文件**：`src/wal/**`、`src/common/config.rs`（config 热点：本周归 A-Persist）  
- **测试**：阈值触发/不触发；长跑体积；恢复正确  
- **依赖**：A1/A2 已合入

#### A-SQL · B2 LEFT/RIGHT JOIN 类型化键（P1，~2d）

- **交付**：Hash Join 外连接热路径类型化键（对齐 P1-1 GROUP BY 模式）  
- **文件**：`src/executor/operators/hash_join.rs`；差分 case 可写入 `tests/differential_sqlite.rs`（A-Test review）  
- **测试**：差分外连接；可挂现有 join bench（**不改** A-Bench 新建的统一入口）  
- **依赖**：无

#### A-Bench · B3 统一 bench + 可复现报告（P1，~2d）

- **交付**：  
  1. 固定场景清单（种子、行数、查询集）  
  2. 一键脚本 `scripts/bench_unified.sh`（WSL）  
  3. 报告 `docs/benchmarks/unified-report.md`（或 `scripts/benchmarks/`）  
- **强制**：报告头记录 **CPU / OS / cargo profile / 命令 / 种子 / git SHA**；数字换环境即失效  
- **文件**：`benches/**` 新文件优先；**不改**功能源码  
- **依赖**：无

#### A-Test · B4 差分收尾 + 门禁硬化（P0，~1–2d）

- **交付**：补 REC CTE 收敛、DECIMAL 边界、更多 JOIN 变体  
- **交付**：**A4b 软断言硬化**——残留行必须 `assert`（或 `#[ignore]`+书面豁免），禁止只 println  
- **文件**：`tests/differential_sqlite.rs`、`tests/phase0_semantic_gate.rs`  
- **职责**：审核他人提交的差分 case，维持基线绿

**W0 合并顺序**：A-Test（门禁）→ A-SQL → A-Persist → A-Bench（报告最后出数）

---

### W1 — P0 正确性（2–3 路）

#### A-Storage · 列存 DELETE tombstone + compact（P0，**串行深工，估 4–5d**）

- **前置**：半页设计说明（tombstone 位图/标记列、compact 触发、与压缩段/MVCC 交互）→ Lead 点头再动码  
- **交付**：列存层逻辑删 + compact 物理回收；Delta 模式可参考但勿大重构  
- **文件**：`src/storage/{table,column_store,delta_store,compact*}.rs`、`operators/delete.rs`  
- **测试**：删后查不到；compact 后行数/空间；恢复不复活；旧文件读路径  
- **风险**：与 mmap/压缩交叉 → 最小 diff；格式变更须版本头。**W1 期间他人不碰 storage 核心**

#### A-Persist · 崩溃注入自动化 + P0 复验（P0，~2–3d）

- **交付**：  
  1. `tests/crash_injection.rs`：**确定性 kill 点**（写后/checkpoint 后/半写 header 等固定点位）+ 恢复断言，进 `cargo test`  
  2. 随机 kill 压力 → `#[ignore]` 或 nightly，**不进 PR 阻塞**  
  3. assessment P0（文件头原子性等）在 WSL 全量复验记录  
- **文件**：`tests/crash_injection.rs`；wal/recovery 仅 bugfix  
- **CI**：长跑 job 由 A-Infra 接线

#### A-Test · 差分看护 + DELETE 用例

- **交付**：W0 后保持绿；为 A-Storage 补 DELETE/UPDATE 差分面（可与 Storage 共写）

**W1 依赖**：A-Storage 与 A-Persist 都可能碰恢复语义 → **DELETE 功能先合，崩溃测随后扩 case**；或 Lead 指定同一周内串行 review。

---

### W2 — 基础设施（1–2 路，可与 W1 部分并行）

#### A-Infra · CI 与工具链

| 项 | 动作 |
|---|---|
| 警告规则 | W0/W1：**不新增**警告；W0 合并后一次性清 ~74 unused，再恢复 `-D warnings` |
| Clippy 阻塞 | **默认 feature** clippy 阻塞；`--all-features`/datafusion 不承诺绿（第三方 chrono/arrow 冲突，pin/隔离即可） |
| datafusion 测试 | 能绿则阻塞；不能则书面豁免 + allowlist |
| 崩溃/长跑 job | `workflow_dispatch` 或 nightly，不拖 PR |
| rust-toolchain pin | 增加 `rust-toolchain.toml` 与 CI 一致 |
| （可选）cargo-deny / audit | 许可证与漏洞 |

**文件**：`.github/workflows/**`、`rust-toolchain.toml`、根配置——**独占**

#### A-Bench · 报告管道固化（若 W0 已出首版）

- CI artifact 上传 bench 摘要（非阻塞性能回归可后置）  
- 与 A-Infra 约定 workflow 片段由 A-Infra 合入

---

## 3. 并行时序（甘特示意）

```
Week 1  W0（3 路）
  A-Test    ██████ B4 + A4b 硬断言
  A-SQL     ████████ B2
  A-Persist ████████ A3 + WAL 长跑不膨胀
  A-Bench   ████████ B3 ──► 报告（含参考环境）
  Lead      合并队列 + 文档

Week 2  W1（串行深工为主）+ W2
  A-Storage ██████████ 列存 DELETE（设计→实现，4–5d）
  A-Persist ████████ crash 确定性 kill + P0 复验
  A-Infra   ████████ 警告清零 + clippy 默认 feature
  A-Test    ████ 看护 + DELETE 差分

Week 3  收尾
  Lead      GO 检查表 → tag v0.23.x / 更新 assessment
  可选      W3（DataPoint / Graph，单线程先行）
```

---

## 4. Lead（主会话）职责

1. **建任务卡**：每个任务 `task create`，绑定子 Agent 时带 `task_id`  
2. **合并队列**：串行 merge；每次 merge 后本地无法编译则以 **CI 绿** 为准  
3. **文档真源**：只 Lead 改 `路线图` / `历史包袱` / `assessment` 状态列  
4. **阻塞升级**：同文件冲突、CI 连红、范围蔓延 → 停波次重排  
5. **阶段 commit**：每合并一个任务包 `git commit`（符合仓库中文 commit 风格）

---

## 5. 子 Agent 启动模板（可直接粘贴）

```
角色：A-SQL（示例）
仓库：D:\Github\EngramDB
分支：wave0/b2-join-typed-keys
任务：B2 LEFT/RIGHT JOIN 类型化键
所有权：仅 src/executor/operators/hash_join.rs 及 tests 中新增 b2_* 文件
禁止：改 lib.rs / storage/mod.rs / workflows / 他人分支；禁止 push main
验证：WSL 脚本或 CI；完成前附测试命令与结果摘要
产出：代码 + 测试 + progress 笔记；commit 信息 style: `perf: B2 …`
阻塞规则：需改所有权外文件 → 停止并上报 Lead
```

通用补充（所有 Agent）：

> 读 `docs/历史包袱清单.md` 与本计划对应条目；不要顺手重构；不要改注释以外的无关格式；警告不新增。

---

## 6. 风险与缓解

| 风险 | 缓解 |
|---|---|
| Windows 无法本地编译 | 统一 WSL 脚本；PR 以 CI 为门禁；禁止「我本地没跑但应该行」 |
| `storage/mod.rs` / `lib.rs` 巨石冲突 | 热点文件单写者；拆分重构不进本波次 |
| CI 排队慢（并行 Agent 的**真实串行点**） | 首轮并行 **3** 路；bench/长跑不进 PR 阻塞路径 |
| DELETE 改格式破坏兼容 | 版本头/feature 开关；旧文件读路径测试 |
| 范围膨胀到 v0.24 | W0–W2 不做 DataPoint/Graph；需求进 backlog |
| 多 Agent 文档打架 | Lead 独占真源文档 |
| 崩溃测试 flaky 拖垮 CI 信任 | PR 只跑确定性 kill 点；随机压力 nightly |
| W0/W1 在途代码引入新警告 | 规则「不新增警告」；批量清零单独提交 |
| B3 数字不可比 | 报告锁参考环境；换机器必须重跑整表 |
| datafusion/chrono 依赖冲突 | 不承诺 `--all-features` clippy 绿；pin/隔离即可 |

---

## 7. 立即执行顺序（若现在开跑）

1. Lead：建任务卡（A3+长跑 / B2 / B3 / B4+A4b / 列存DELETE / crash / CI）并标依赖  
2. 并行 spawn **3 路**：**A-Bench、A-Test、A-Persist**（零文件重叠；A3 不单开）  
3. A-SQL(B2) 在 A-Test 第一版基线绿后加入；存储 DELETE 进 W1 再开  
4. W0 全部 merge + 退出标准三条勾选 → 开 W1  
5. 每波结束 Lead 打检查表

---

## 8. 检查表（复制用）

**W0 关账（含退出标准三条）**

- [ ] A3 阈值可配 + 测试  
- [ ] **WAL 长跑不膨胀** 断言测试  
- [ ] B2 外连接类型化 + 差分  
- [ ] B3 报告入库（含机器/命令/种子）+ 一键脚本  
- [ ] B4 无未解释分歧  
- [ ] A4b 门禁：assert 或书面豁免（禁止 println 假绿）  
- [ ] 路线图退出标准改「已满足」

**W1 GO 前置**

- [ ] 列存 DELETE 设计半页 → 实现 → 测试（含旧文件读路径）  
- [ ] crash **确定性 kill 点**进 `cargo test`（随机压力仅 nightly）  
- [ ] assessment P0 复验记录（WSL/CI 日志）  
- [ ] 历史包袱 P0 表清零  

**W2**

- [ ] 警告清零后 `-D warnings`（或等价）恢复  
- [ ] clippy **默认 feature** 阻塞（all-features 豁免须书面）  
- [ ] toolchain pin  
- [ ] （可选）nightly 长跑/随机 crash job  

---

## 9. 明确不做（本多 Agent 首轮）

- v0.24 DataPoint / GraphEngine / bumpalo  
- v0.25 SEARCH/RRF（映射已写好，另开波次）  
- 重构 `expression.rs` / `planner.rs` 拆文件  
- 分布式 / 完整 SQL 标准 / 应用层记忆运营  
- 修复 datafusion/chrono **上游**不兼容（只 pin/隔离）  
- 批量清理 73 篇历史 docs（W2 末尾可选）  

---

## 10. 合理性结论（2026-10 复盘）

**判定：修改后可执行。** 原稿若直接开跑会在五处翻车：长跑退出标准无任务、A4b println 假绿、DELETE 工期低估、crash 随机 kill 必 flaky、B3 无参考环境。

- 保留强点：所有权/合并协议、WSL/CI 完成定义、W0–W2 范围冻结  
- 已收紧：并行 4→3、A3 并入 Persist、crash 确定性 kill 点、clippy 只保默认 feature、GO 允许文档化豁免  
- 残留风险：CI 排队是物理串行点；列存 DELETE 无法靠加人压缩日历（串行深工）
