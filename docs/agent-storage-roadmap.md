# Agent 后端存储引擎 — 升级路线图（v0.21 → v0.24）

> 更新：2026-08-07（v0.21 设计定稿：统一 Token 流压缩）
> 定位：嵌入式 Agent 数据引擎（单进程，无网络 server）
> 两大方向：① 会话内容存储（第一优先） ② 知识库存储（第二优先）
> 依据：opencode 实态调研 + 源码机制分析 + 业界分词压缩调研

---

## 一、方向总览

| 版本 | 主题 | 方向 | 核心交付 |
|---|---|---|---|
| **v0.21** | 会话存储引擎 | ① | 统一 Token 流（分词器）+ 分词压缩 codec + Log 事件流 + Columnar 物化 + 会话 API |
| **v0.22** | 知识库检索全链路 | ② | DAG 检索（复用 v0.21 分词器）+ SQL 混合查询 + SIMD + RRF |
| **v0.23** | 记忆原语（差异化） | ①+② | 分层记忆、重要性衰减、SearchTrace API、时间旅行 |
| **v0.24** | 生态与并发（按需） | — | Python 绑定、多进程只读共享、摄取管线、网络模式 |

**横切基础设施**：统一 Token 流（`Tokenizer` + `TokenStream`）是 v0.21-v0.24 的共用底座——压缩、FTS、DAG 检索、记忆分层全部消费同一 token 流。

---

## 二、v0.21 会话存储引擎（定稿）

### 架构（业务无感）

```
业务层（opencode 式 API，无感）:
  SessionStore::begin_message / append_part(全量快照) / update_part / finish_message
              / get_session / recent_messages / search_messages

引擎层（透明）:
  event_log（Log 引擎）: 完整事件流（全量快照），content 分词+delta 压缩 → 审计/回放
  sessions / messages（Columnar）: 聚合物化（finish 时单事务投影）→ 读取/分析/计费
```

### 任务清单（P0-1 已定稿：路线 B——离线 tokenizers 训练 + 运行时自研轻量编码器）

| # | 任务 | 内容 | 验收 |
|---|---|---|---|
| P0-1 | **统一 Tokenizer**（`src/common/tokenizer.rs`）| 离线 BPE 训练（tokenizers，混合语料 + 种子词表）+ 运行时自研零分配编码器（类别预分割 + trie 最长匹配 + byte_fallback + NFKC norm）+ TokenStream 三要素；差分测试锁定一致性 | 同文本恒同 token 流；无损往返；golden 与 tokenizers 逐 token 一致；v0.22 直接复用 |
| P0-2 | **微型压缩基准** | 四臂（TokenDelta+Huffman / TokenDelta+varint / zstd+CDict dev-dep / 不压缩）× 三场景（A 流式 / B 覆盖 / C 独立文档）× 词表三角（纯 BPE / 种子 BPE / 人工词表）| 数据定 codec 取舍（业界无先例，必须实测）|
| P0-3 | **TokenDelta codec** | 静态热词层（merges rank 前 N 短 ID）+ 块级动态字典 + 前缀 delta + 熵编码（Huffman 起步，rANS 候选）；接入 compression/mod.rs 分派 | 事件流压缩后 ≈ 1× 内容 |
| P0-4 | **Log 序列化接入 + Log TTL** | serialize_typed 按块走 TokenDelta；capabilities.rs 开启 + 块级 cutoff + checkpoint 物理释放 | 过期块零读取、物理释放 |
| P0-5 | **SessionStore API** | begin/append/update/finish → Log 事件（全量快照，业务无感）；读 API → Columnar | opencode 同款调用直接可用 |
| P0-6 | **finish 物化投影** | 单事务：Log finish 事件 + Columnar 聚合（message/part/session 计费累计）| 多表原子，强一致 |
| P0-7 | **FTS 切换统一 Tokenizer** | inverted_index 消费 norm（CJK 即刻可搜）| MATCH 中文命中正确 |
| P0-8 | **基准 A5** | 模拟 opencode 负载：写放大压缩率、拉取/搜索延迟、体积 vs 7.8GB 实测 | 文档记录对比 |

### 待办风险
- 分词确定性（词表版本化 + 未登录字 byte_fallback）
- 块字典膨胀（静态热词短 ID + 动态层增量编码）
- 压缩率退化场景（best-of 自动选型：TokenDelta vs Uncompressed 取小；
  代码/JSON 退化由 P0-2 数据判定是否引入运行时兜底，见 engram 文档 4.6）
- 孤儿事件（崩溃）→ TTL 兜底 + 后续回放重建（nice-to-have）

---

## 三、v0.22 知识库检索全链路（第二优先）

| # | 任务 | 现状 | 改动 | 验收 |
|---|---|---|---|---|
| B1 | **DAG 检索**（复用 v0.21 分词器）| CJK 整串单 token | 同一 Tokenizer 的 text/offset 构建词图；norm 进倒排；BM25 排序 | 中文检索命中正确、可排序 |
| B2 | **SQL 混合查询** + ef_search 可配置 | 仅 Rust hybrid_search；ef 硬编码 50 | `CREATE VECTOR INDEX ... ef_search`；SQL 表值函数带 WHERE 下推 | 一条 SQL 完成向量+标量过滤 |
| B3 | 向量点积 SIMD | 逐元素 f32 | f32x16 距离层 | 检索 ≥2x |
| B4 | RRF 混合排序（V15/Ag06）| 无 | 全文（同一 token 流）+ 向量两路召回 → RRF | 排序可解释、命中互补 |

依赖：B1 依赖 v0.21 P0-1（分词器同源）；B4 依赖 B1。

---

## 四、v0.23 记忆原语（差异化护城河）

| # | 任务 | 说明 |
|---|---|---|
| C1 | 分层记忆接口（Ag04）| working（Memory 引擎）/ short（TTL 表）/ long（Columnar+向量）；统一 save/recall/forget |
| C2 | 记忆重要性评分 + 衰减（Ag05）| importance × time_decay 排序，可配置衰减曲线 |
| C3 | SearchTrace 导出 API | 命中节点/分数/索引类型，Agent 引用溯源（已内部实现，转公共）|
| C4 | 会话分支 / 时间旅行 | 事件溯源红利：从 event_log 重放重建历史物化 |

---

## 五、v0.24 生态与并发（按需）

| # | 任务 | 触发条件 |
|---|---|---|
| D1 | Python 绑定（pyo3，Eco03）| LangChain/生态需求 |
| D2 | 多进程只读共享（mmap）| 多 agent 进程读同一库 |
| D3 | 摄取管线（Ag13/Ag16）| 知识库文档摄入（复用统一 Tokenizer）|
| D4 | 网络 server 模式 | 多 agent 共享库成为硬需求（当前架构不支持，需大改）|

---

## 六、四维优先级总表

| 维度 | P0（v0.21） | P1（v0.22） | P2（v0.23+） |
|---|---|---|---|
| **性能** | 分词压缩（写放大 610×→~1×）、Log TTL 块淘汰 | 向量 SIMD | 并行查询 |
| **功能** | 会话 API、事件流+物化、FTS 中文 | DAG 检索、SQL 混合检索、RRF | 记忆分层、时间旅行 |
| **交互** | Rust 会话 API（opencode 式）+ SQL 模板 | SQL 向量语法 | Python 绑定 |
| **差异化** | 统一 Token 流（压缩+检索同源）、零维护会话存储 | 统一混合检索 | 记忆原语、SearchTrace |

---

## 七、决策记录

| 日期 | 决策 | 依据 |
|---|---|---|
| 2026-08-07 | 会话规模：单机个人/小团队（<10 万会话）| 用户确认 |
| 2026-08-07 | TTL 形态：表级 | 用户确认 |
| 2026-08-07 | 交互：Rust API 优先 | 用户确认 |
| 2026-08-07 | 架构：Log 完整事件流（全量快照）+ Columnar 物化 | 用户确认：业务无感、引擎消除冗余 |
| 2026-08-07 | 压缩：分词 + 静态热词 + 块字典 + 前缀 delta（TokenDelta）| 业界组件均有先例、组合无先例 → 基准先行验证 |
| 2026-08-07 | 统一 Token 流：text/norm/offset 三要素，压缩/FTS/DAG 三方消费 | 用户确认：分词器一次建设多处复用 |
| 2026-08-07 | 审计/回放为 nice-to-have（后续实现），Log 仍完整记录 | 用户确认 |
| 2026-08-07 | DAG 分词器：本轮建设（含中文词表，v0.22 复用）| 用户确认 |
