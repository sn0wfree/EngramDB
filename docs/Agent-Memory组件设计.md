# Agent Memory 组件设计（独立规格）

> 状态：设计规格 v1（不绑定单一实现）  
> 核对来源：`调研核对-Agent记忆.md`  
> 与 EngramDB 关系：可作为上层组件；引擎侧仅承载 F1–F3 所列底座能力

---

## 1. 目标与非目标

**目标**：进程/服务可插拔的「Agent 记忆组件」——写入蒸馏、分层存储、语义+过滤召回、生命周期与隔离，供任意 Agent 框架调用。

**非目标**：
- 不做 LLM 编排框架、不做对话 UI
- 不做跨组织知识市场 / 联邦学习
- 不内置「必须用某一家向量库」——后端可插拔

---

## 2. 概念模型

```
Owner (user/tenant)
  └── Namespace（可选，多 agent 共享边界）
        ├── SessionMemory     有序事件 · TTL · 摘要
        └── LongTermMemory    蒸馏事实 · 向量+结构化 · 取代/版本
```

**记忆条目（逻辑 schema）**

| 字段 | 说明 |
|---|---|
| id | 稳定 ID |
| owner_id / namespace_id / session_id? | 作用域 |
| kind | `session_event` \| `ltm_fact` \| `ltm_pref` \| `ltm_procedure` \| 自定义 |
| content | 自然语言正文 |
| embedding? | 向量 |
| structured | JSON：类别、实体、来源、置信度… |
| source | `user_stated` \| `agent_inferred` \| `tool` \| `import` |
| confidence | 0–1 |
| status | `active` \| `superseded` \| `expired` \| `withheld` |
| valid_until? / superseded_by? | 取代与有效期 |
| created_at / updated_at / event_time? | 双时间可选 |

---

## 3. 核心 API（语言中立）

```
// 写入
append_session_event(owner, session_id, role, content, meta?) -> event_id
put_long_term(owner, content, kind, structured?, source?, confidence?) -> memory_id
// 显式演化（应用层或策略引擎调用）
evict(memory_id, op: ADD|UPDATE|DELETE|NOOP|SUPERSEDE) -> result

// 召回
recall(query, {owner, namespace?, kinds?, limit, min_score?, mode: semantic|exact|hybrid}) -> [Hit]
recall_for_action(query, scope, policy) -> { allowed: [Hit], withheld: [Hit], reasons }

// 生命周期
expire_before(owner, ts) / purge_pii(owner, predicate)
summarize_session(session_id) -> summary   // 可回调外部 LLM，组件只定义钩子

// 管理
health() / stats() / export_审计日志
```

**设计原则**
1. **召回与行动分离**：`recall` 可返回低置信；`recall_for_action` 按 policy 扣留（借鉴 contextdb.ai 的 act|ask|stop，实现可简）。
2. **取代优于覆盖**：同槽位新事实 → 旧条目 `superseded`，保留血缘。
3. **写入默认异步蒸馏**：`append_session_event` 同步落盘；抽取进队列，接口暴露 `pending_extraction`。
4. **作用域一等公民**：所有读写带 owner；跨 owner 需显式 admin API。

---

## 4. 分层与后端

| 层 | 存什么 | 建议后端 | 生命周期 |
|---|---|---|---|
| Working | 当前任务白板（可由调用方维护，组件提供 KV/文档槽） | 内存 / Redis / 表 | 任务结束可丢 |
| Session | 有序事件 + 摘要 | 行存 / 文档 + 索引 | TTL + 滚动摘要 |
| Long-term | 蒸馏事实 | 向量索引 + 结构化过滤（同一事务更好） | 取代 / 衰减 / 手动删 |
| Archive | 冷 session / 被取代版本 | 对象存储 / 列存 | 按需水化 |

**StorageAdapter trait（示意）**

```
put / get / search(filter) / delete / scan_expired
+ VectorIndex: upsert(id, vec) / query(vec, filter, k)
+ 可选 Graph: link(a,b,type) / neighbors(id, type)
```

单机嵌入式实现可把三者放进一个文件事务（EngramDB / contextdb.tech 路线）；分布式实现可拆。

---

## 5. 写入流水线

```
raw event → [async] segment → extract(原子事实/偏好/行动结果)
         → dedup(hash + 语义近邻) → resolve(ADD/UPDATE/NOOP/SUPERSEDE)
         → embed → index → audit
```

- **去重**：content hash + 近邻阈值；同 owner 同槽同值同源 → NOOP  
- **冲突**：同槽不同值 → 按 source 优先级 / 时间 / 显式 policy；不确定则双挂 `contested`（可选）  
- **PII**：extract 前规则/分类器；`exclude_patterns` 配置  
- **失败**：死信队列 + 可重放；不阻塞主 Agent 循环

---

## 6. 检索流水线

```
query → classify(fact|relation|recent) 
      → 路由: vector | filter | graph | hybrid
      → 各路 top-N → RRF/加权融合
      → rerank(可选) → budget 截断 → pack prompt block
```

- 时间维：`event_time` 参与排序（recency boost 可配置）  
- 置信度升级：热索引不足 → 冷档（可选 v2）  
- 返回结构：`{id, content, score, source, confidence, status}` 便于上层决定是否注入

---

## 7. 非功能

| 项 | 要求 |
|---|---|
| 持久化 | 组件外置存储；Agent 进程崩溃不丢已 ack 写入 |
| 一致性 | 单 owner 写串行或乐观锁；跨 owner 无共享写 |
| 性能 | 召回 p50 目标 &lt; 50ms（本地嵌入式，视规模）；写入 ack ≠ 蒸馏完成 |
| 安全 | owner 强制；API key/JWT；审计：who/what/when/op |
| 可观测 | 写入速率、抽取延迟、召回命中率、层容量、死信深度 |
| 可插拔 | Storage / Embedder / Extractor(LLM) / Reranker 均可换 |
| 多租户 | namespace 隔离默认开；共享需显式 grant |

---

## 8. 最小可用切片（MVP）

**两周可落地顺序：**

1. **Schema + Session append + TTL 扫描**（无 LLM）  
2. **Long-term 手动 put + 向量/过滤 recall**（Embedder 外置）  
3. **SUPERSEDE/DELETE 演化 + owner 隔离测试**  
4. **异步抽取钩子**（接口 + mock extractor）  
5. **hybrid RRF**（若底层已有两路分数）

**验收用例：**
- 跨 session 召回偏好  
- 同槽更新后旧事实不可 `recall_for_action`  
- 过期 session 不出现在默认 recall  
- 跨 owner 读拒绝  
- 抽取失败可重放且不阻塞 append

---

## 9. 与 Redis Iris / ContextDB* 的边界

| 能力 | 本组件 | Iris 类托管 | contextdb.tech 类引擎 |
|---|---|---|---|
| 会话事件 + TTL | ✅ | ✅ | 可建模 |
| 自动 LLM 抽取 | 钩子可选 | ✅ 内置 | ❌ 应用层 |
| 语义+过滤召回 | ✅ | ✅ | ✅ |
| 状态机/不可变审计列 | ❌（用 status 列） | ❌ | ✅ |
| 行动前信任策略 | 可选 `recall_for_action` | 弱 | 弱（.ai 产品有） |
| 单文件嵌入 | 取决于 Adapter | ❌ 托管 | ✅ |

---

## 10. 开放问题

1. 摘要与抽取的默认 Embedder/LLM 由谁提供（插件 vs 内置 HTTP）？  
2. `recall_for_action` 的默认 policy 是否要进 MVP，还是仅文档？  
3. 与 EngramDB F2 `SEARCH` 语法对齐到什么程度（库 API 优先 vs SQL 关键字）？  
4. 图关系是否 MVP 必需，还是 v2 跟 GraphEngine？
