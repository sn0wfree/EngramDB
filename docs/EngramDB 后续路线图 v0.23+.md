# EngramDB 后续路线图（v0.23+）

> 制定日期：2026-10-21  
> 基线：v0.22.4（本合并后）  
> 依据：`assessment-v0.22.md`、`high-priority-features-plan.md`、历史路线图 `EngramDB 后续路线图 v0.18-v0.22.md`（已标为历史文档）

---

## 一、相对 v0.18 规划的校准

| v0.18 规划项 | 当前状态 | 本版 / 后续 |
|---|---|---|
| P0-1 扫描 slice 替代 take_front | 已用 `from_range` 等价落地 | 关闭 |
| P0-3 表达式 Typed 快路径 | 已接入 `try_eval_specialized` | 关闭 |
| P1-1 类型化 GROUP BY | **本次合入**（零 Value 热路径 + 类型保持） | 关闭 |
| WAL checkpoint 临时修复 | **本次合入**（只写标记，不 truncate） | 关闭 |
| P0-2 Varchar offset+data 双数组 | 未做 | v0.23 P0 |
| P1-2 LEFT/RIGHT JOIN 类型化键 | 未做 | v0.23 P1 |
| P1-3 bumpalo arena | 未做 | v0.24 |
| WAL segment rolling | 未做（checkpoint 仍不丢旧段） | v0.23 P0 |
| P2-2 统一 bench 套件 | 部分 bench 已有，缺统一报告 | v0.23 P1 |

v0.19–v0.22 实际交付超前于原路线图（新类型、VIEW/CHECK/递归 CTE、搜索、mmap、多引擎等），版本号以 **v0.22.4** 为准，不再回填 v0.18 叙事。

---

## 二、路线图总览

```
v0.23  ──►  v0.24  ──►  v0.25  ──►  v0.26+
正确性/性能补完   架构升级     Agent 记忆     生态
  ~2-3 周         ~4 周        ~4-5 周       持续
```

| 版本 | 主题 | 核心交付 |
|------|------|---------|
| **v0.23** | 正确性 + 性能补完 | WAL segment rolling、Varchar 双数组、JOIN 类型化键、统一 bench |
| **v0.24** | 架构升级 | DataPoint 溯源模型、GraphEngine v1、bumpalo/执行期分配 |
| **v0.25** | Agent 记忆 | RRF 混合检索算子化、分层检索入口、Session 记忆 |
| **v0.26+** | 生态 | Python 绑定、MCP、并行查询、可观测性 |

---

## 三、v0.23 — 正确性 + 性能补完（优先）

**目标**：消除 checkpoint 不能回收 WAL 的持久化缺口；扫描/连接路径关键优化收尾；给性能结论一个可信基线。

| 编号 | 功能点 | 优先级 | 工作量 | 说明 |
|------|--------|--------|--------|------|
| A1 | WAL 段文件 rolling | P0 | 3 天 | 多 segment 轮换；checkpoint 后删除旧段，不是 `set_len` |
| A2 | Checkpoint 丢弃头部旧段 | P0 | 2 天 | 依赖 A1；恢复只回放最后 checkpoint 之后 |
| A3 | Checkpoint 触发策略（大小/时间） | P1 | 1 天 | 可配置阈值 |
| B1 | Varchar offset+data 双数组 | P0 | 3–5 天 | 消除 String 堆分配与比较开销；先做列内布局，再扩接口 |
| B2 | LEFT/RIGHT JOIN 类型化键 | P1 | 2 天 | Hash Join 外连接热路径 |
| B3 | 统一 bench + 可复现报告 | P1 | 2 天 | 固定场景/数据/统计口径，输出对比表 |
| B4 | SQLite 差分测试扩大面 | P0 | 2–3 天 | 覆盖聚合 NULL、JOIN、窗口；与 `assessment-v0.22` GO 条件对齐 |

**退出标准**：A1–A2 合入后长跑不膨胀；B4 差分无未解释分歧；B3 报告入库。

---

## 四、v0.24 — 架构升级

| 编号 | 功能点 | 优先级 | 工作量 | 说明 |
|------|--------|--------|--------|------|
| C1 | DataPoint 核心模型 | P0 | 2 天 | id + hash + payload + metadata + source_chain |
| C2 | 文档摄入 API | P0 | 3 天 | 文件/文本、hash 去重 |
| C3 | 溯源链查询 | P1 | 2 天 | source_chain 遍历 |
| D1 | GraphEngine v1（邻接 + BFS/DFS） | P1 | 5 天 | 基于 ColumnarEngine；SQL `GRAPH TRAVERSE` |
| E1 | 执行期 arena（bumpalo） | P2 | 3–4 天 | 先 profile 再决定是否上 |

---

## 五、v0.25 — Agent 记忆能力

| 编号 | 功能点 | 优先级 | 工作量 | 说明 |
|------|--------|--------|--------|------|
| F1 | RRF 混合排序算子化 | P0 | 2 天 | 现有纯函数接到 SQL/执行器入口 |
| F2 | 统一 SEARCH 入口 | P0 | 2 天 | 精确 / 向量 / 过滤+向量 三模式 |
| F3 | Session 记忆存储约定 | P1 | 3 天 | 会话级 TTL + 摘要列 |
| F4 | 分层检索 v2（图+向量） | P1 | 4 天 | 依赖 D1 |

---

## 六、v0.26+ — 生态（持续）

- Python 绑定（pyo3）
- MCP / 本地服务封装
- 并行扫描与聚合
- 可观测性（EXPLAIN ANALYZE 增强、指标导出）

---

## 七、明确不做（本阶段）

- Cognee 应用层能力（记忆演化流水线）——保持「底层引擎」定位，见 `EngramDB 全景梳理 v0.14-Cognee对标.md`
- 完整 SQL 标准 / OLTP 替换 SQLite
- 分布式
