# Agent 记忆调研核对报告

> 日期：2026-10  
> 方法：官方文档 + GitHub + 站点直采（Bing 中文索引对 Redis Iris 极差，以一手源为准）  
> 对照：微信文《给 Agent 加长期记忆：30 行接上 Redis Iris》及本会话前序长文

---

## 一、结论速览

| 前序说法 | 核对结果 | 证据 |
|---|---|---|
| Redis Iris 存在，含 Agent Memory | **属实** | [redis.io/iris](https://redis.io/iris/) · [agent-memory](https://redis.io/agent-memory/) · [docs](https://redis.io/docs/latest/develop/ai/context-engine/agent-memory/) |
| Iris 六项能力 | **属实**（命名一致） | Intent Router · Agent Playbooks · Agent Memory · Context Gateway · Lens · Agent Mesh |
| Lens / Agent Mesh 仍 beta，勿上生产 | **未在官网证实** | 官网标 `[New]`，无 GA/beta 字样；成熟度以销售/文档为准 |
| 「11 月 4 日 GA」 | **未核实** | 官网活动为 **Nov 12, 2026**；无 11/4 GA 公告页 |
| 30 行本地 `redis-stack` + `from iris import AgentMemory` | **不属实（误导）** | 真实形态是 **托管服务**：`pip install redis-iris`，环境变量 `IRIS_AGENT_MEMORY_URL/STORE_ID/API_KEY`，非本地 stack 单机库 |
| MemoryETL、分层记忆、remember/recall | **方向属实，API 形态不同** | 官方：session 事件流 + 后台抽长期记忆 + `recall` 语义检索；非 WeChat 伪代码签名 |
| ContextDB = 学术「记忆操作系统」（四张正交图、RL Memory Manager、LOCOMO 79%） | **本轮未找到该论文** | 搜索仅命中三个同名商业/开源产品；Zenodo/arXiv 题名检索空 → **前序长文该段证据不足，勿当事实引用** |
| ContextDB 同名多义 | **属实，且至少 4 个** | 见下表 |

---

## 二、Redis Iris（已证实）

**定位**：Redis 的 Agent 实时上下文平台（非开源单文件库）。

**Agent Memory 官方能力**（docs + 产品页）：
- **Session memory**：有序对话事件、TTL、超阈值自动摘要（近期原文保留）
- **Long-term memory**：后台异步抽取、向量+元数据、跨 session 召回
- **MemoryETL**：从文档摄入、可查证据
- **自定义 memory type**、敏感信息排除、namespace、多 agent 作用域
- **接口**：`redis-iris` Python SDK · `iris` CLI · REST · MCP
- **部署**：Redis Cloud 或 K8s 自管；需 store URL + API Key

**接入形态（纠正微信文）**：

```bash
pip install 'redis-iris>=1.0.0'
# IRIS_AGENT_MEMORY_URL / STORE_ID / API_KEY
```

```python
from redis_iris.agent_memory import AgentMemory
memory = AgentMemory.from_env()
memory.conversation(owner=..., session_id=...).user("...")
hits = memory.recall("用户饮食偏好？", owner=...)
```

**生态旁证**：`redis/vercel-ai-sdk`、`coleam00/redis-iris-agent`（Context Retriever MCP + Agent Memory 工具）、topic `redis-iris` 多仓库。

**与 EngramDB 关系**：Iris 是**托管上下文服务**；EngramDB 是**嵌入式存储引擎**。不构成直接竞品，但 Session/长期分层、自动抽取、语义召回是同一问题域。

---

## 三、「ContextDB」同名拆分（关键纠偏）

| 名称 | 形态 | 要点 | URL |
|---|---|---|---|
| **contextdb.tech** | 开源嵌入式引擎（Rust） | 关系+图+向量，**单文件 MVCC 事务**；STATE MACHINE / IMMUTABLE 列；与 EngramDB **赛道最接近** | https://contextdb.tech · GitHub `context-graph-ai/contextdb` |
| **contextdb.ai** | SaaS（Atoms AI） | 客户记忆 + **行动前证据检查**（confidence / confirm / act\|ask\|stop） | https://contextdb.ai |
| **阿里云 RDS ContextDB** | 企业托管服务 | 事实-实体-记忆规则、自动遗忘、知识库、记忆晋升、MCP/CLI 接入 coding Agent | https://help.aliyun.com/zh/rds/apsaradb-rds-for-mysql/rds-contextdb-quick-start |
| **「Unified Context Layer」论文** | 未证实 | 前序文中的五层架构/四图/RL Manager/LOCOMO 数字 **本轮 0 命中** | — |

**contextdb.tech 与 EngramDB 对照（最值得盯）**：

| | EngramDB | contextdb.tech |
|---|---|---|
| 形态 | 单文件 `.hdb` 嵌入式 | 单文件嵌入式（redb） |
| 模型 | 列存分析 + 行 Delta + 多索引 | 关系 + 原生图遍历 + 向量，统一事务 |
| 特色 | 压缩、多引擎、SQL 分析 | 状态机约束、审计不可变列、LAN sync |
| Agent 记忆 | 路线图 v0.25 | 产品即 agentic memory |

---

## 四、前序长文中仍可保留的设计观点（与来源解耦）

以下不依赖可疑论文，可独立成立，供组件设计引用：

1. 记忆分层（工作 / 情景 / 语义 / 程序性）
2. 写入侧蒸馏：抽取 → 去重/取代 → 异步 Worker
3. 检索侧：查询路由 + 置信度升级 + 时间维 + 上下文打包
4. 生命周期：TTL / 主动遗忘 / 压缩 / 版本化
5. 非功能：独立持久化、异步写、PII 前置、租户隔离、可观测、可插拔后端
6. 信任/来源（user_stated vs agent_inferred、confirm 前不可行动）——**contextdb.ai 产品页有真实对应**

**应降级为「未验证」的数字**：LOCOMO 79%、Mem0 20%、p95&lt;100ms、90% token 节省、RL 150 样本、学习路由 +3.2 F1 等——除非补到可引用原始页。

---

## 五、证据清单

| 来源 | URL |
|---|---|
| Redis Iris 产品页 | https://redis.io/iris/ |
| Redis Agent Memory 产品页 | https://redis.io/agent-memory/ |
| Agent Memory 文档 | https://redis.io/docs/latest/develop/ai/context-engine/agent-memory/ |
| redis/vercel-ai-sdk | https://github.com/redis/vercel-ai-sdk |
| redis-iris-agent 示例 | https://github.com/coleam00/redis-iris-agent |
| contextdb.tech | https://contextdb.tech/ · /docs/ |
| contextdb.ai | https://contextdb.ai/ |
| 阿里云 RDS ContextDB | https://help.aliyun.com/zh/rds/apsaradb-rds-for-mysql/rds-contextdb-quick-start |

---

## 六、对后续文档的约束

1. 引用 Redis Iris → 以官方 docs 为准，**不用微信 30 行伪代码**。
2. 写 ContextDB → **必须带域名/产品前缀**，禁止裸称。
3. 学术四图/RL Manager → 标注 **未证实**，设计文档中改为「可选设计模式」而非「某论文已证」。
4. 性能/基准数字 → 无一手来源不写进路线图。
