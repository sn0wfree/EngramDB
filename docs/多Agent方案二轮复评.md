# 多 Agent 方案 · 二轮复评（E1–E5 合入后）

> 日期：2026-11-12 · 依据：`评审证据包-E1-E5.md` + WSL 全量 `cargo test` 实测  
> 角色：技术评审主席 · 状态：**决议输入**

---

## 0. 结论（相对一轮的变化）

| | 一轮（证据前） | **二轮（证据后）** |
|---|---|---|
| 初判 | 有条件通过 | **有条件通过——且门槛收窄、可预期** |
| 最大风险 | 「单一可信验证链未闭合」（模糊） | **精确定位**：链断在「工程门面」，不在功能正确性 |
| 开跑阻塞 | 5 项决议 + 证据 | **1 个前置 W-1（修绿 CI）+ 3 项决议** |

**一句话**：功能底座比预想健康（WSL 全量 **1229+ 全绿**）；CI 红是 fmt 漂移 + **2 个 bench 文件借用错误** + 第三方 clippy——**W-1 一天量级**，修完即可 spawn。方案骨架 **仍然成立**，不需要推翻重做。

---

## 1. 关键实测（改变判断的事实）

### 1.1 功能健康度：优（已证实）

WSL `cargo test` 全量：

| 套件 | 结果 |
|---|---|
| lib 单测 | **1229 passed / 0 failed / 4 ignored** |
| 集成（a5/phase0/diff/group_commit/persistence/migration…） | **全部 ok** |
| `differential_sqlite` | **19/19 ok**（含原 known_bug 用例，现绿） |
| `phase0_semantic_gate`（含 A4b） | **8/8 ok** |
| ignored 项 | 仅 doc-test（arena/heat_tracker/transaction）+ 4 个 `bench_*` 微基准宏——**非缺口** |

→ **「正确性挂账」没有新增；测试资产可信。** E2（A4b PASS）与全量结果一致。

### 1.2 CI 红的精确解剖（已证实）——风险从「未知」变为「有限工作量」

| Job | 失败点 | 性质 | 工作量判断 |
|---|---|---|---|
| Formatting | 多文件 `cargo fmt` Diff（expression/planner/lib/insert…） | 机械 | **一次 `cargo fmt` 提交** |
| Clippy | `E0034` arrow-arith 52.2.0 × chrono 0.4.45（`temporal.rs`） | **第三方** | 不可修（符合 Q3=A） |
| Build & Test | `cargo test` ✅ 1229 绿 → **`cargo build --benches` 挂** | 机械+少量逻辑 | **2 个文件**：`auto_migrate_bench.rs`（E0716 临时值 / E0499 双 `&mut`）、`m2_memory_bench.rs`（E0596 未声明 mut） |

→ **W-1 清单闭合**：① fmt 全库 ② 修 2 个 bench 借用错误 ③ clippy/datafusion 保持 `continue-on-error`（Q3=A 维持）。  
→ **不存在「CI 基线未知」的无限风险**；预期 0.5–1.5 人日。

### 1.3 排队与合并（已证实）

- runner 排队 ≈ 0，全流程 4–6min → **并行度不受 CI 排队约束**（Q1 切换条件改为 merge 带宽，一轮修订正确）。
- 历史无 merge queue，单人直推 main → 多 Agent 合并协议是**新制度**，首周需 E5-bis 实测（保留）。

---

## 2. H1–H5 二轮状态

| ID | 一轮 | 二轮 | 变化 |
|---|---|---|---|
| H1 长跑不膨胀 | 成立 | **成立** | 无新证据推翻；仍缺任务承接 |
| H2 A4b 门禁 | 需实测 F16 | **降级**：已 PASS，仅硬化 assert | 工作量 −；直接 A 选项 |
| H3 DELETE 工期 | 低估 | **维持 4–5d + R1–R8** | E4 风险表补强设计门槛 |
| H4 crash flaky | 成立 | **成立** | 确定性 kill 点方案维持 |
| H5 bench 口径 | 成立 | **成立** | E3 八场景可直接拆卡 |

**新增 H6（本轮）**：CI 工程门面红——**不是 H 级缺陷，是 W-1 前置**；未完成前所有 Agent 的「CI 绿」DoD 无法执行。

---

## 3. Q1–Q6 二轮推荐（收敛）

| ID | 维持/变更 | 二轮推荐 | 依据 |
|---|---|---|---|
| Q1 | 变更切换条件 | **A（3 路）**；切换看 merge 冲突，不看 CI 排队 | E1 |
| Q2 | 维持 | **C（DELETE 分阶段进 W1）** | E4 R1–R8 |
| Q3 | **加强** | **A（默认 feature 阻塞）**；E0034 已证实第三方 | E1 |
| Q4 | 维持 | **A（GAP- 文档化豁免）** | — |
| Q5 | 维持 | **A 起步 / DELETE 可 C** | — |
| Q6 | 维持 | **C（仅 Lead 直提 main）** | E5 与历史一致 |

---

## 4. 开跑模型（二轮定稿）

```
[W-1 单线程 · A-Infra/Lead · 0.5–1.5d]
  fmt 全库 + 修 auto_migrate_bench/m2_memory_bench + 一次 CI 全绿 run
        │
        ├─ 门槛：run id 写入决议表
        ▼
[W0 三路并行]
  A-Bench(U1–U8) ∥ A-Test(B4+H2 assert) ∥ A-Persist(A3+H1 长跑)
        │
        ├─ A-SQL(B2)：差分基线绿后进
        ▼
[W1] crash 确定性网 → DELETE 阶段1 → 阶段2（E4 设计签字）
        ▼
[W2] 警告清零、clippy 默认 feature 阻塞、toolchain pin
```

**物理串行点（不变）**：W-1 门禁、Lead merge、DELETE 深工。  
**可并行（不变）**：W0 三路、W1 crash ∥ Infra 警告。

---

## 5. 开跑门槛（二轮 24h 清单——替换一轮清单）

**硬门槛（不过不 spawn）**

- [ ] **W-1 完成**：CI 一次全绿 run id（fmt + default test + benches 编译；clippy 可红但需与 E1 一致）
- [ ] Q1 / Q2 / Q5（及 Q6）决议勾选
- [ ] H2 = assert 硬化（无需再讨论过渡方案）
- [ ] H1 任务卡（A-Persist）含断言定义
- [ ] 三路分支名与所有权无交集
- [ ] spawn 模板（证据 = WSL 命令输出或 CI run URL）

**软门槛（首周内）**

- [ ] E5-bis：记录 3 次实际 merge 时延
- [ ] GAP- 豁免模板启用
- [ ] H4 kill 点清单草案

**已自动关闭**（E2/E3/E4 已补）：A4b 实测、bench 场景、DELETE 风险表、CI 排队假设。

---

## 6. 与一轮结论的差异摘要

1. **风险等级下调**：「验证链未知」→「验证链断点明确、修复量小」。  
2. **功能可信度上调**：WSL 全绿证明测试资产可用，多 Agent 不是在流沙上并行。  
3. **W-1 成为唯一新增阻塞**，且单线程即可，不消耗并行度预算。  
4. **H2 简化**、**Q3 加强依据**、**Q1 条件修正**——其余维持。  
5. **方案不必重写**；`多Agent开发计划.md` 只需增补 W-1 小节与 H2=A 收敛。

---

## 7. 最终建议（二轮）

**建议：批准方案进入「W-1 执行」；W-1 绿后立即按 W0 三路 spawn。**

| 若 | 则 |
|---|---|
| W-1 ≤2 天完成 | **开跑**（本建议默认路径） |
| W-1 超 3 天或出现新编译面 | 冻结 spawn，重评 H6 |
| 评审会决议 Q1/Q2/Q5 有保留 | 仅 W-1 与文档任务继续，功能 Agent 等待 |

**签署后动作**：Lead 增补计划 W-1 → A-Infra/Lead 执行 → run id 入决议表 → spawn A-Bench ∥ A-Test ∥ A-Persist。
