# opencode 使用 SQLite 作为存储后端 — 特性 / 表结构 / 利用 / 数据流分析

> 日期：2026-08-07
> 依据：① 本地实测（`~/.local/share/opencode/opencode.db`，7.8GB SQLite，292 会话 / 16 万消息）
> ② 源码（`/home/ll/Public/opencode_src`，dev 分支）
> 目的：完整拆解 opencode 如何用 SQLite 存会话，作为 EngramDB 会话存储引擎的
> 对照基准与借鉴来源

---

## 一、结论先行

opencode 采用**事件溯源（Event Sourcing）+ 事务内同步投影（Projector）**架构：
- 每次变更发布一个**持久化事件**（含全量数据快照）到 `event` 表
- 同一 SQLite 事务内，投影器（projector）把事件**物化**到 `message` / `part` / `session` 表
- 读取**只读物化表**；事件表用于回放/同步/审计

**实测痛点**：写放大 610×（流式全量快照）、无清理（5.5GB 事件冗余）、无压缩。
SQLite 本身没问题——问题在「事件 = 全量快照 + 每次更新都发布 + 不压缩」的用法。

---

## 二、特性需求清单（SQLite 为 opencode 提供的核心能力）

| # | 特性 | opencode 的利用 | 关键点 |
|---|---|---|---|
| 1 | **ACID 事务** | 事件 + 物化同事务提交（`db.transaction(immediate)`）| 强一致：事件与物化永不分裂 |
| 2 | UPSERT（`onConflictDoUpdate`）| message/part 每次更新覆盖物化行 | 流式更新 = 行覆盖，非追加 |
| 3 | 复合索引 + 排序 | `(session_id, time_created, id)` 复合索引 + `ORDER BY desc` + `LIMIT` 分页 | cursor 分页（time + id 双键）|
| 4 | 外键级联删除 | 删 session → 级联删 message/part | 表间引用完整性 |
| 5 | WAL 模式 | 读写并发（只读快照 + 写不阻塞）| 前端 UI 读 + 后端写并存 |
| 6 | JSON 列（`text mode=json`）| `data` 列存完整对象快照 | 灵活 schema，但**不压缩** |
| 7 | 事务内读改写 | projector 读旧行 → 计算 usage → 更新 session 计费 | 增量累计 tokens/cost |
| 8 | 单文件单进程 | 嵌入式，无需服务端 | 开箱即用 |

---

## 三、表结构（9 张核心表，源码 `packages/core/src/session/sql.ts`）

### 3.1 物化表（读路径）

```sql
-- 会话元数据（低频更新，含成本核算）
session (
    id TEXT PK, project_id TEXT NOT NULL, workspace_id TEXT, parent_id TEXT,
    slug TEXT NOT NULL, directory TEXT NOT NULL, path TEXT, title TEXT NOT NULL,
    version TEXT NOT NULL, share_url TEXT,
    summary_additions INT, summary_deletions INT, summary_files INT,
    summary_diffs JSON, metadata JSON,
    cost REAL DEFAULT 0, tokens_input INT DEFAULT 0, tokens_output INT DEFAULT 0,
    tokens_reasoning INT DEFAULT 0, tokens_cache_read INT DEFAULT 0, tokens_cache_write INT DEFAULT 0,
    revert JSON, permission JSON, agent TEXT, model JSON,
    time_created INT, time_updated INT, time_compacting INT, time_archived INT
    -- 索引: project / workspace / parent
)

-- 消息（每次更新覆盖 data 快照）
message (
    id TEXT PK, session_id TEXT NOT NULL REFERENCES session ON DELETE CASCADE,
    time_created INT, time_updated INT, data JSON NOT NULL        -- data = 消息全量对象
    -- 索引: (session_id, time_created, id)  ← cursor 分页键
)

-- 消息片段（part：text/tool/step-start/step-finish/reasoning/patch）
part (
    id TEXT PK, message_id TEXT NOT NULL REFERENCES message ON DELETE CASCADE,
    session_id TEXT NOT NULL,
    time_created INT, time_updated INT, data JSON NOT NULL        -- data = part 全量对象
    -- 索引: (message_id, id) / (session_id)
)

-- 任务列表（会话内 todo）
todo (session_id, content, status, priority, position, timestamps)
    -- PK: (session_id, position)
)
```

### 3.2 事件溯源表（写/回放路径）

```sql
event (
    id TEXT PK, aggregate_id TEXT NOT NULL,        -- = session_id（聚合根）
    seq INT NOT NULL,                              -- 聚合内单调递增
    type TEXT NOT NULL,                            -- message.updated / message.part.updated / ...
    data TEXT NOT NULL                             -- 事件 payload（全量快照，不压缩）
    -- UNIQUE (aggregate_id, seq)  ← 幂等重放锚点
    -- INDEX  (aggregate_id, type, seq)
)

event_sequence (aggregate_id TEXT PK, seq INT, owner_id TEXT)  -- 每聚合最新 seq
```

### 3.3 会话执行态（v2，会话状态机）

```sql
session_message (           -- v2 消息（事件驱动的会话状态，seq = 事件 seq）
    id TEXT PK, session_id TEXT NOT NULL, type TEXT NOT NULL,
    seq INT NOT NULL, time_created INT, data JSON NOT NULL
    -- UNIQUE (session_id, seq)  ← 与事件一一对应
    -- INDEX  (session_id, type, seq) / (session_id, time_created, id) / (time_created)
)

session_input (             -- 用户输入队列（pending → admitted → promoted）
    id TEXT PK, session_id TEXT NOT NULL, prompt JSON NOT NULL, delivery TEXT NOT NULL,
    admitted_seq INT NOT NULL, promoted_seq INT, time_created INT
    -- UNIQUE (session_id, admitted_seq) / (session_id, promoted_seq)
)

session_context_epoch (     -- 上下文基线（snapshot + baseline_seq，压缩/回滚锚点）
    session_id TEXT PK, baseline TEXT NOT NULL, snapshot JSON NOT NULL, baseline_seq INT NOT NULL
)
```

---

## 四、利用方式（关键机制）

### 4.1 事件溯源 + 事务内投影（核心，`packages/core/src/event.ts:205-353`）

```
publish(event) ──► db.transaction(immediate):
  1. 读 event_sequence 取最新 seq
  2. 执行该事件类型注册的 projector（写物化表）     ← 事务内同步！
  3. 更新 event_sequence（seq+1）
  4. 插入 event 行（含 data 快照）
  ──► 提交
```

- **强一致**：事件与物化同事务，不存在「事件有了物化没有」的窗口
- **幂等重放**：`(aggregate_id, seq)` 唯一约束 + 内容比对（`isDeepStrictEqual`），支持 replay/恢复
- **本地投影器**：`PublishOptions.commit` 可在同事务追加本地副作用（不计入事件流）

### 4.2 投影器注册（`packages/core/src/session/projector.ts:211-455`）

| 事件 | 投影动作 |
|---|---|
| `session.created` | INSERT session 表（onConflictDoNothing）|
| `session.updated` | UPDATE session 全行 |
| `message.updated` | **UPSERT** message（data 全量覆盖）|
| `message.removed` | 级联删 part + **usage 回退**（成本/令牌减）|
| `message.part.updated` | UPSERT part + **usage 增量累计**到 session（cost/tokens 加）|
| `step/tool/text/reasoning/compaction...` | `SessionMessageUpdater.update`（immer 局部改 → 整行写回）|

**计费模型**：`step-finish` part 携带 cost/tokens → projector 用 `applyUsage(sessionID, usage, ±1)` 增量累计（removed 时回退）——会话级 token/成本永远正确。

### 4.3 流式更新（写放大源头）

- 每次 LLM 输出增量 → 发布 `message.part.updated`（全量 part 快照）+ 阶段结束时 `message.updated`（全量消息快照）
- **实测**：平均每条消息被更新 **610 次**（最大 64,688 次），`message.updated` 事件平均 31.5KB
- event 表 5.5GB 中 4.5GB 是 `message.updated.1`——**全量快照 × 610 次** 的写放大

### 4.4 读取与分页（`packages/core/src/session/message-v2.ts`）

- **cursor 分页**：`(time_created DESC, id DESC)` + `LIMIT`，cursor = base64(id + time)
- **hydrate**：message 行批量 → `IN (message_ids)` 拉 part → 内存按 message 分组
- **读己之写**：message/part 表是唯一读源（非事件重放）

### 4.5 压缩 / 回滚（`session/compaction.ts` + `session_context_epoch`）

- 上下文超限 → compaction（摘要 + 裁剪），`compaction` part 类型 + `tail_start_id` 标记
- `RevertEvent` 投影：以 boundary message 的 seq 为界，删除其后 session_message（`seq > boundary`）→ 时间旅行回滚

---

## 五、数据流总览

```
LLM 流式输出
  │  每增量
  ▼
publish(part.updated / message.updated)         ──┐
  │ 事务(immediate)                                │ 事件溯源
  ├─► projector → UPSERT message/part            ──┤ 物化（同事务）
  ├─► applyUsage → UPDATE session 计费            ──┤
  └─► INSERT event + event_sequence              ──┘
  │
  ▼（内存 PubSub 通知 UI 实时刷新）
  │
读取: message/part 表（cursor 分页 + hydrate）
回放: event 表（readAggregate / durable 流）
恢复: replay(events, strictOwner) 幂等重放
```

---

## 六、问题诊断（EngramDB 要消除的）

| # | 问题 | 实测 | 根因 |
|---|---|---|---|
| 1 | **写放大 610×** | 平均 610 次更新/消息，event 累计 5.04GB vs 物化 1.5GB | 事件 = 全量快照 + 每次更新发布 + **无压缩** |
| 2 | **无限膨胀** | 292 会话全保留、无 TTL/归档，WAL 持续增长 | 无清理机制 |
| 3 | **体积爆炸** | 7.8GB 数据库（event 5.5GB / part 1.05GB / message 0.75GB）| 快照冗余 + JSON 不压缩 |
| 4 | 写路径慢 | 每次全量序列化 31KB | 无增量编码 |

---

## 七、对 EngramDB 的借鉴与对策映射

| opencode 机制 | 借鉴 | EngramDB 对策 |
|---|---|---|
| 事务内投影（强一致）| ✅ 保留思路 | finish 时单事务：Log 事件 + Columnar 物化 |
| 事件 = 全量快照 | ❌ 病根 | **统一 Token 流 + 分词压缩**（TokenDelta）：快照冗余压到 ~1× |
| 无清理 | ❌ | Log 表级 TTL + 块级过期淘汰 + checkpoint 物理释放 |
| JSON 列不压缩 | ❌ | content 用 Varchar + 分词压缩；metadata JSON 独立（小）|
| cursor 分页 | ✅ | `(time_created, id)` 复合索引 + 块级跳读 |
| usage 累计计费 | ✅ | finish 投影时累计到 sessions（Columnar）|
| 幂等重放 | ✅（nice-to-have）| event_log 完整保留，后续回放重建物化 |
| 引擎分工 | — | Log（快写、压缩、TTL）承载事件流；Columnar（索引/FTS/分析）承载物化 |
