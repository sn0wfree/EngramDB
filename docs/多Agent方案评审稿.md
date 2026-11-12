# 多 Agent 开发方案 · 评审与讨论稿

> 状态：**已决议**（2026-11-12 · 会话批注落印；草案 = `docs/多Agent开发计划.md` 46eb94b）  
> 日期：2026-10 · 决议日：2026-11-12  
> 参与：Lead（本会话拍板）  
> 目的：对「W0–W2 多 Agent 并行开发」方案做合理性评审，收敛待决事项后开跑  
> 后续：`W-1` **本轮不执行**（仅填表）；spawn 等 W-1 CI 全绿 run id

---

## 1. 背景与方案一页摘要

**问题**：EngramDB 剩余工作 = 关账 v0.23（A3/B2/B3/B4）+ P0 正确性（列存 DELETE、崩溃自动化、复验）+ 基础设施（CI/警告/bench 口径）。本地 Windows SAC 无法编译，验证依赖 CI/WSL。

**方案主张**：
- 三波推进：W0 关账 v0.23 → W1 P0 → W2 基建；v0.24/25 不进首轮
- 角色：Lead + A-Persist / A-Storage / A-SQL / A-Test / A-Bench / A-Infra
- 防踩踏：文件所有权表、热点单写者、一分支一 Agent、禁推 main
- 完成定义：测试 + **WSL/CI 绿** + 文档勾选 + commit（本地不算数）
- 首轮并行 **3 路**：A-Bench ∥ A-Test ∥ A-Persist；A-SQL 随后；DELETE 进 W1 串行

**详细文本**：`docs/多Agent开发计划.md`（含 0.1 修订记录）

---

## 2. 评审结论（初判）

| 维度 | 判定 | 说明 |
|---|---|---|
| 总体 | **有条件通过** | 骨架可执行；原稿有 5 处硬伤，修订已写入计划正文，**须在本次评审确认** |
| 分工模型 | 通过 | 所有权 + Lead 合并队列匹配本仓库现实 |
| 工期/并行 | 有条件通过 | 并行 4→3；DELETE 不可并行压缩 |
| 验证策略 | 通过 | 以 CI/WSL 为门禁是对 SAC 的正确应对 |
| 范围控制 | 通过 | W0–W2 冻结，v0.24/25 另议 |

---

## 3. 修订前发现的问题（已改入计划，评审需确认）

| # | 问题 | 风险 | 计划中的处置 | 待确认 |
|---|---|---|---|---|
| H1 | 漏「A1–A2 长跑不膨胀」退出标准 | v0.23 关不了账 | W0 增 WAL 长跑断言测试 | 测试形态（集成测 or 脚本+CI） |
| H2 | A4b 门禁只 `println!` 不 assert | 假绿 | assert 或 `#[ignore]`+豁免 | 豁免要不要留口头 KNOWN-GAP |
| H3 | 列存 DELETE 估 2–3 天 | 严重超期、拖垮 W1 | 4–5 天、设计半页先行、串行 | 设计评审门槛（谁签字） |
| H4 | crash「随机 kill×100」 | CI flaky 拖垮信任 | PR 确定性 kill 点；随机 nightly | kill 点清单谁定 |
| H5 | B3 无参考环境 | 报告不可比 | 强制记录 CPU/OS/命令/种子 | 以 WSL 机还是 CI 为口径 |

---

## 4. 结构性取舍（请拍板）

### Q1 · 并行度：3 路还是 2 路？

| 选项 | 优点 | 缺点 |
|---|---|---|
| **A. 3 路**（Bench ∥ Test ∥ Persist）〔计划现稿〕 | 关账快；文件零重叠 | CI 排队；merge 队列忙 |
| B. 2 路（Test+SQL ∥ Persist，Bench 后置） | 更少冲突；CI 压力小 | B3 报告晚出，关账慢 |
| C. 4 路（原稿） | 最快 | A3 独占 Agent 开销 > 收益 |

**倾向**：A。若 CI 当日排队 &gt;2h，降为 B。

### Q2 · 列存 DELETE 要不要进 W1 首轮？

- **进**（现稿）：P0 真正清零，GO 可期；但是单点深工 4–5 天，且与 crash/恢复语义有交叉  
- **不进，W1.5**：先完成 crash + P0 复验 + CI，DELETE 下一波；GO 延后但风险隔离更好  

**倾向**：进 W1，但 **crash 确定性用例先合**（先有回归网再动 DELETE）。

### Q3 · clippy / `-D warnings` 目标

| 选项 | 说明 |
|---|---|
| **A. 仅默认 feature 阻塞**〔现稿〕 | datafusion/chrono 第三方冲突不碰 |
| B. 连 datafusion feature 一起修 | 可能被上游依赖拖住数日 |

**倾向**：A；B 记入 backlog 单独立项。

### Q4 · GO 标准里「缺口」怎么算

- **A. 允许文档化豁免**〔现稿〕：LogEngine 无 UPDATE 等产品决策可接受  
- B. 一切 KNOWN-GAP 必须清零  

**倾向**：A，但豁免必须挂在 `历史包袱清单` 有编号，禁止只存在于测试 println。

### Q5 · 多 Agent 执行载体

| 选项 | 说明 |
|---|---|
| **A. 本会话 actor 子 Agent**（explore/general spawn） | Lead 可直接合并；上下文同仓 |
| B. 外部 Claude Code / 多会话 | 独立会话更隔离，但同步成本高 |
| C. 混合：深工（DELETE）单会话，并行小项用 actor | 取两者优点 |

**倾向**：A 起步；DELETE 若需长窗口专注，改 C。

### Q6 · 分支策略

- **A. 短生命周期分支 + Lead 串行 merge**〔现稿〕  
- B. 干线开发（小步直接 main + 回滚）——与「禁推 main」冲突，不推荐  

**维持 A**；请确认是否允许 Agent 在 Lead 授权下对 main 直接 commit（当前仓库历史上有直接 main 提交习惯）。

---

## 5. 计划中仍偏弱、建议补强的点

1. **merge 冲突演练**：未规定两 Agent 同时改 `Cargo.toml`/`tests` 时的仲裁 SLA（谁等谁）。建议写死：后者 rebase 前必须先 re-read 对方 diff。  
2. **progress 证据格式**：要求贴 CI run URL / WSL 命令输出摘要，统一模板，避免「应该行」。  
3. **回滚预案**：DELETE 格式若破坏旧 `.hdb`，需 `git revert` 策略 + 文件版本头兜底——计划有版本头，缺 revert 决策人。  
4. **A-Test 与 A-SQL 共写 `differential_sqlite.rs`**：已改共写+review，但缺「冲突时以 A-Test 基线为准」的合并权限说明（已写裁定人，评审确认即可）。  
5. **B2 与 B4 耦合**：B4 窗口/JOIN case 可能直接暴露 B2 bug——是否允许 A-Test 在 B2 分支未合前先开失败用例（红灯驱动）？**建议允许，标 `issue` 注释**。  
6. **无「性能回归阈值」**：B3 只出报告不设回归线；是否 v0.23 只要求「有数」，回归门禁放 W2+？**建议是**。

---

## 6. 建议的评审议程（30–45 min）

| 时长 | 议题 | 输出 |
|---|---|---|
| 5' | §1 方案摘要对齐 | 无异议进入 |
| 10' | §3 五项硬伤处置确认 | H1–H5 关闭 |
| 15' | §4 Q1–Q6 拍板 | 记录选项 A/B |
| 10' | §5 补强项 1/2/5 | 写回计划 |
| 5' | 开跑条件 | 首批 spawn 名单 + 分支名 |

**开跑门槛（建议）**：H1–H5 全关闭 + Q1/Q2/Q5 有结论。

---

## 7. 决议记录（2026-11-12 落印）

| 议题 | 决议 | 责任人 | 日期 |
|---|---|---|---|
| Q1 并行度 | **B · 2 路**（Test+SQL ∥ Persist；Bench 后置）。切换条件按 E1：看 merge 冲突>1h×2，**不看** CI 排队 | 评审主席+Lead | 2026-11-12 |
| Q2 DELETE 进否 W1 | **C · 分阶段进 W1**（crash 网先合 → 阶段1 逻辑 tombstone → 阶段2 compact；设计半页 Lead 签字） | Lead | 2026-11-12 |
| Q3 clippy 范围 | **A · 仅默认 feature 阻塞**；E0034 第三方维持 continue-on-error / 书面豁免 | Lead+A-Infra | 2026-11-12 |
| Q4 GO 豁免 | **A · 允许文档化豁免**（GAP- 编号挂 `历史包袱清单`，禁止仅 println） | 评审主席 | 2026-11-12 |
| Q5 执行载体 | **A · 本会话 actor 子 Agent**；DELETE 若需长窗可临时升 C | Lead | 2026-11-12 |
| Q6 main 直提授权 | **C · 仅 Lead 直提 main**；Agent 一分支经 Lead 合并 | Lead | 2026-11-12 |
| H1 长跑测试形态 | **A · 集成测试进默认集**（按二轮推荐默认；任务卡由 A-Persist 承接） | A-Persist | 2026-11-12 |
| H2 A4b 豁免形式 | **A · assert 硬化**（E2 已 PASS，无过渡双用例） | A-Test | 2026-11-12 |
| H3 DELETE 设计签字 | **C · 分阶段 + 半页设计 Lead 签字**（与 Q2=C 一致；R1–R8 必答） | A-Storage / Lead | 2026-11-12 |
| H4 crash kill 点清单 | **A · PR 确定性 kill 点 + nightly 随机**（清单 A-Persist 草案 → Lead 核准） | A-Persist | 2026-11-12 |
| H5 bench 参考环境 | **A · WSL 固定机 + 报告头元数据**（E3 U1–U8） | A-Bench | 2026-11-12 |
| 补强 2 证据模板 | **启用**：progress 必贴 CI run URL 或 WSL 命令输出摘要 | Lead | 2026-11-12 |
| 补强 5 红灯驱动 | **允许**：A-Test 可在 B2 未合前先落失败 case（标 `issue` 注释） | A-Test | 2026-11-12 |
| W-1 执行时机 | **本轮只填表，不执行**；代码侧见 `1dde714`（fmt+bench 修复），**全绿 run id 仍待补** 后才 spawn | Lead / A-Infra | 2026-11-12 |

**Q1=B 对首批名单的修正**（相对二轮「三路」推荐）：

| 波次 | 名单 |
|---|---|
| 首批（2 路） | **A-Test**（B4+H2）∥ **A-Persist**（A3+H1） |
| 随后 | **A-SQL**（B2，差分基线绿后）∥ **A-Bench**（B3 后置，E3 U1–U8） |
| W1 | crash 确定性网 → DELETE 阶段1/2（Q2=C）∥ A-Infra 警告/clippy（Q3=A） |

---

## 8. 附：当前方案文件与基线

| 文件 | 说明 |
|---|---|
| `docs/多Agent开发计划.md` | 可执行计划（含 0.1 修订记录、§10 复盘结论） |
| `docs/历史包袱清单.md` | P0/P1/P2 挂账真源 |
| `docs/EngramDB 后续路线图 v0.23+.md` | 退出标准真源 |
| `docs/assessment-v0.22.md` | GO/NO-GO 判断与 P0 原始描述 |
| `docs/代码梳理.md` | 架构与模块规模 |

**基线**：`main @ 46eb94b`（计划修订）；工作区无未提交代码变更。

---

## 9. 评审后动作

1. 决议表填完 → 更新 `多Agent开发计划.md` 与本稿状态（待评审 → 已决议）  
2. Lead `git commit` 文档  
3. 按 Q5 载体 spawn 首批 Agent（建议 A-Bench / A-Test / A-Persist）  
4. 本稿保留在 `docs/` 作为决策痕迹，不归档删除
