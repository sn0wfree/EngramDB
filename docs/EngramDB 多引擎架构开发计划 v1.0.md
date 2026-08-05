# EngramDB 多引擎架构开发计划 v1\.0

# EngramDB 多引擎架构开发计划

> 项目代号：EngramDB（原 HybridDB）
> 文档版本：v1\.0
> 更新日期：2026\-08\-05
> 负责人：林璐1
> 
> 

---

## 一、项目愿景与定位

### 1\.1 我们在做什么

EngramDB 是**面向 AI Agent 场景的嵌入式多引擎单文件数据库**。

- **嵌入式**：作为库链接到应用进程，零配置、无后台进程

- **单文件**：所有持久化数据在一个文件里，方便部署和迁移

- **多引擎**：同一数据库内支持多种存储引擎，不同表选不同引擎

- **AI Agent 原生**：从设计上适配 Agent 的多种工作负载

### 1\.2 为什么现在做

**技术时机成熟**：

- Rust 生态成熟（Redb/Sled/Fjall 验证了 Rust 写存储引擎的可行性）

- DuckDB 验证了「嵌入式列存」的市场和技术可行性

- SQLite 验证了「单文件嵌入式数据库」的用户需求

**需求刚出现**：

- AI Agent 是第一个同时需要「嵌入式 \+ 单文件 \+ 多种工作负载」的场景

- 传统嵌入式数据库（SQLite）只有行存，不适合向量检索和分析查询

- 服务器型数据库（MySQL/ClickHouse）太重，不适合 Agent 嵌入式运行

**市场空白**：

- 没有成熟的「单文件 \+ 多存储引擎 \+ SQL \+ 嵌入式」数据库

- 这是 EngramDB 的差异化机会

### 1\.3 核心设计理念

> **不同工作负载用不同引擎，比一个引擎打天下更快。**
> 
> 

- 列存引擎 → 分析查询快（记忆检索、统计分析）

- 内存引擎 → 点查/写入快（临时状态、推理中间结果）

- 日志引擎 → 写入吞吐高（操作日志、trace、监控）

用户只需要选 `ENGINE = xxx`，剩下的交给数据库。

---

## 二、整体架构

### 2\.1 架构分层

```
┌─────────────────────────────────────────────────────────────┐
│                        SQL Interface                        │
│  SQL Parser → Planner → Optimizer → Executor (向量化)       │
├─────────────────────────────────────────────────────────────┤
│                    Transaction Manager                        │
│  MVCC · 事务隔离 · 快照读 · 跨引擎事务协调                   │
├─────────────────────────────────────────────────────────────┤
│                    WAL (Write-Ahead Log)                     │
│  统一 WAL · 按引擎类型分发 · 崩溃恢复 · Checkpoint           │
├─────────────────────────────────────────────────────────────┤
│                      Catalog (元数据)                        │
│  表定义 · 引擎类型 · 索引信息 · 列信息 · 根页指针            │
├─────────────────────────────────────────────────────────────┤
│                   Page Allocator (页分配器)                   │
│  空闲页链表 · 空间管理 · 文件增长 · 碎片整理                 │
├──────────────┬───────────────┬──────────────────────────────┤
│ Columnar     │    Memory     │          Log                 │
│ Engine       │    Engine     │          Engine              │
│              │               │                              │
│ 列存 + Delta │ 纯内存结构    │ 追加写列式                    │
│ MVCC 完整    │ MVCC 简化     │ 无 MVCC                      │
│ 向量索引     │ 可选持久化    │ 时间分区                     │
│ 二级索引     │               │ 高压缩比                     │
└──────────────┴───────────────┴──────────────────────────────┘
```

### 2\.2 单文件布局

```
┌──────────────────────────────────────────────────────────┐
│  Page 0: 文件头 (File Header)                             │
│  ┌────────────────────────────────────────────────────┐  │
│  │  Magic: "ENGRAMDB"                                 │  │
│  │  Version: 1                                        │  │
│  │  Page Size: 8192                                   │  │
│  │  WAL head page pointer                             │  │
│  │  Catalog root page pointer                         │  │
│  │  Free page list head pointer                       │  │
│  │  Checkpoint LSN                                    │  │
│  └────────────────────────────────────────────────────┘  │
├──────────────────────────────────────────────────────────┤
│  Catalog 区 (元数据)                                     │
│  ┌────────────────────────────────────────────────────┐  │
│  │  Table 1: memory                                   │  │
│  │    - engine: columnar                              │  │
│  │    - root_page: 12                                 │  │
│  │    - columns: [...]                                │  │
│  │    - indexes: [...]                                │  │
│  ├────────────────────────────────────────────────────┤  │
│  │  Table 2: traces                                   │  │
│  │    - engine: log                                   │  │
│  │    - root_page: 45                                 │  │
│  │    - columns: [...]                                │  │
│  ├────────────────────────────────────────────────────┤  │
│  │  Table 3: state                                    │  │
│  │    - engine: memory                                │  │
│  │    - root_page: NULL (运行时在内存)                 │  │
│  │    - columns: [...]                                │  │
│  └────────────────────────────────────────────────────┘  │
├──────────────────────────────────────────────────────────┤
│  WAL 区 (环形/追加)                                       │
│  [txn_id][engine_type][table_id][op_type][payload]...    │
├──────────────────────────────────────────────────────────┤
│                                                          │
│  数据页区 (动态分配，各引擎共享)                            │
│                                                          │
│  ┌─────────┐ ┌─────────┐ ┌─────────┐ ┌─────────┐      │
│  │ Columnar│ │  Log    │ │ Columnar│ │  Log    │      │
│  │ Page 12 │ │ Page 45 │ │ Page 67 │ │ Page 89 │      │
│  └─────────┘ └─────────┘ └─────────┘ └─────────┘      │
│                                                          │
│  空闲页链表统一管理，各引擎按需申请/释放                   │
│                                                          │
└──────────────────────────────────────────────────────────┘
```

### 2\.3 关键基础设施

#### Page Allocator（统一页分配器）

所有引擎通过同一接口申请和释放页：

```rust
pub trait PageAllocator {
    fn alloc_page(&self) -> Result<PageId>;
    fn free_page(&self, page_id: PageId) -> Result<()>;
    fn alloc_pages(&self, n: usize) -> Result<Vec<PageId>>;
    fn read_page(&self, page_id: PageId) -> Result<Vec<u8>>;
    fn write_page(&self, page_id: PageId, data: &[u8]) -> Result<()>;
}
```

- 空闲页链表（free page list）管理空闲空间

- 新页从链表头部取，释放的页挂回链表头部

- 链表不够时 append 到文件末尾

- 类似 SQLite 的页分配机制，但跨引擎共享

#### 统一 WAL \+ 跨引擎事务

WAL 只有一个，所有引擎的写操作都写进同一个 WAL：

```
WAL 记录格式:
  [txn_id] [engine_type] [table_id] [op_type] [payload]

例：
  txn=123, engine=columnar, table=memory, op=insert_batch, payload=...
  txn=123, engine=log, table=traces, op=append, payload=...
  txn=123, engine=memory, table=state, op=put, payload=...
  txn=123, COMMIT
```

- 事务提交原子性：要么整个事务的所有 WAL 记录都落盘，要么都不落盘

- 崩溃恢复：按 txn\_id 回放，每个引擎自己解析自己的 payload

- Checkpoint：定期将 WAL 中的变更固化到数据页，回收 WAL 空间

#### StorageEngine Trait（引擎抽象）

```rust
pub trait StorageEngine {
    // DDL
    fn create_table(&self, table_id: u64, schema: &TableSchema) -> Result<()>;
    fn drop_table(&self, table_id: u64) -> Result<()>;

    // DML
    fn insert(&self, table_id: u64, rows: DataChunk) -> Result<usize>;
    fn update(&self, table_id: u64, pk: &Value, updates: &[(usize, Value)]) -> Result<bool>;
    fn delete(&self, table_id: u64, pk: &Value) -> Result<bool>;

    // Query
    fn scan(&self, table_id: u64, spec: &ScanSpec) -> Result<Vec<DataChunk>>;

    // 事务
    fn begin_txn(&self, txn_id: u64) -> Result<()>;
    fn commit_txn(&self, txn_id: u64) -> Result<()>;
    fn abort_txn(&self, txn_id: u64) -> Result<()>;

    // 元数据
    fn engine_type(&self) -> EngineType;
}
```

---

## 三、引擎详细设计

### 3\.1 ColumnarEngine（列存引擎）

**定位**：主力分析引擎，结构化数据 \+ 向量混合查询

**适用场景**：

- 长期记忆存储（结构化字段 \+ 向量）

- 对话历史分析、Agent 行为统计

- 知识库 RAG（元数据过滤 \+ 向量检索）

**核心特性**：

- 列式存储 \+ Delta 层（LSM 风格）

- 完整 MVCC（Snapshot Isolation）

- 支持二级索引（B\-Tree）

- 支持 Zone Map / Mark Index（跳读索引）

- 支持 HNSW 向量索引

- 7 种压缩算法自动选择

- 支持事务、持久化

**文件内组织**：

- 每个列一个独立的列存文件逻辑上（物理上在同一文件的不同页）

- Row group 大小：默认 122,880 行

- 每个 row group 内按列连续存储

- Delta 层：内存 \+ 持久化双缓冲

**性能预期**：

|操作|性能|vs SQLite|
|---|---|---|
|单列扫描|极快|10\-20x 快|
|聚合查询|快|2\-5x 快|
|点查|中|1\-3x 慢|
|批量写入|中|1\-2x 慢|
|向量检索|极快|不支持|

---

### 3\.2 MemoryEngine（内存引擎）

**定位**：超高频读写的临时数据，Agent 思考过程中的中间状态

**适用场景**：

- Agent 推理链（Chain of Thought）中间结果

- 工具调用 session 级缓存

- 短期工作记忆（几秒到几分钟生命周期）

- 高频更新的计数器 / 状态机

**核心特性**：

- 全内存，不写磁盘（默认模式）

- 无 WAL 开销（默认模式）

- 点查 O\(1\)（HashMap 直接查）

- 进程退出数据丢失（默认模式）

- 可选持久化模式（SNAPSHOT / WAL）

**内存数据结构**：

- 主键索引：`BTreeMap<Value, RowIndex>`（支持范围查询）

- 数据存储：`Vec<Vec<Value>>` 或列式 `Vec<ColumnData>`

- 可选：二级索引（同内存 BTreeMap）

**持久化模式（可选）**：

- `PERSISTENCE = NONE`（默认）：纯内存，不持久化

- `PERSISTENCE = SNAPSHOT`：关闭时快照落盘，启动时加载

- `PERSISTENCE = WAL`：写入同步 WAL，崩溃可恢复

**性能预期**：

|操作|性能|vs ColumnarEngine|
|---|---|---|
|点查|\~1μs|100x 快|
|单行写入|\~1μs|200x 快|
|范围扫描|快|5\-10x 快|
|持久化|❌（默认）|—|

**建表语法**：

```sql
CREATE TABLE thought_state (
    step_id INTEGER PRIMARY KEY,
    state TEXT,
    parent_step INTEGER
) ENGINE = Memory;
-- 或带持久化
CREATE TABLE session_cache (
    key VARCHAR PRIMARY KEY,
    value TEXT
) ENGINE = Memory WITH PERSISTENCE = SNAPSHOT;
```

---

### 3\.3 LogEngine（日志引擎）

**定位**：只追加写，极致写入吞吐，日志/trace/事件流

**适用场景**：

- Agent 操作日志（每一步都记）

- 工具调用 trace

- 用户交互流水

- 监控指标数据

- 时序数据

**核心特性**：

- 只追加（append\-only），不支持 UPDATE/DELETE

- 无 MVCC 开销

- 无 Delta 层，直接写列存块

- 按时间自动分区

- 写入时只做最轻量的索引（时间戳 MinMax）

- 后台异步压缩 \+ 建索引

- 高压缩比（时序数据专用编码）

**文件内组织**：

- 按时间分块（block），每块默认 64MB 或 100 万行

- 块内列式存储 \+ 压缩

- 每块存时间范围（MinMax），支持时间范围跳读

- 块索引存在文件头部，快速定位

**性能预期**：

|操作|性能|vs ColumnarEngine|
|---|---|---|
|批量写入|\~100 万行/秒|10x 快|
|存储空间|0\.5\-0\.7x|更高压缩比|
|时间范围扫描|快|1\.5\-2x 快|
|UPDATE/DELETE|❌|—|
|点查|❌（只能扫）|—|

**建表语法**：

```sql
CREATE TABLE agent_trace (
    ts TIMESTAMP,
    agent_id VARCHAR,
    event_type VARCHAR,
    payload JSON
) ENGINE = Log;
```

---

## 四、开发路线图

### 里程碑总览

|里程碑|版本|核心目标|预计周期|
|---|---|---|---|
|M0|v0\.15|架构准备：StorageEngine trait 抽象|2 周|
|M1|v0\.16|ColumnarEngine 独立封装 \+ 性能优化|3 周|
|M2|v0\.17|MemoryEngine 上线|2 周|
|M3|v0\.18|LogEngine 上线|3 周|
|M4|v0\.19|跨引擎事务 \+ 统一 WAL|3 周|
|M5|v0\.20|查询优化器适配多引擎|2 周|

**总计：约 15 周（3\.5 个月）**

---

### M0：架构准备 — StorageEngine Trait 抽象

**目标**：将现有列存代码封装成 ColumnarEngine，抽象出 StorageEngine trait

**任务清单**：

|编号|任务|工作量|优先级|
|---|---|---|---|
|M0\-1|定义 StorageEngine trait|1 天|P0|
|M0\-2|定义 EngineType 枚举 \+ Catalog 扩展|1 天|P0|
|M0\-3|将现有 table\.rs / column\_store\.rs / delta\_store\.rs 封装进 ColumnarEngine|3 天|P0|
|M0\-4|Connection 层通过引擎类型分派到对应引擎|2 天|P0|
|M0\-5|PageAllocator 从 ColumnarEngine 中抽离，成为共享基础设施|3 天|P0|
|M0\-6|WAL 层增加 engine\_type 字段，支持多引擎 WAL 记录|2 天|P0|
|M0\-7|现有测试全部迁移通过|2 天|P0|

**验收标准**：

- 所有现有功能正常工作（测试全绿）

- 性能不退化（对比基准）

- StorageEngine trait 定义清晰，新增引擎只需实现 trait

---

### M1：ColumnarEngine 性能优化

**目标**：在多引擎架构基础上，把 ColumnarEngine 的查询性能做上去

**任务清单**：

|编号|任务|工作量|优先级|
|---|---|---|---|
|M1\-1|整数列排序换 Radix Sort|2 天|P0|
|M1\-2|Group By 单列整数键直连哈希|2 天|P0|
|M1\-3|SELECT \* 跳过行→列→行双重转置|1 天|P0|
|M1\-4|表达式求值：整数/浮点类型特化 \+ SIMD|3 天|P1|
|M1\-5|PREWHERE 列级懒读取（真正的列级跳读）|3 天|P1|
|M1\-6|Page 级 Zone Map（row group 内跳读）|5 天|P1|
|M1\-7|主键 Mark Index（二分精确定位）|3 天|P1|
|M1\-8|Bloom Filter Index（row group 级）|3 天|P2|
|M1\-9|Delta 层空扫描短路 \+ 连续 rowid 映射优化|1 天|P2|

**验收标准**：

- 10 万行基准：WHERE 1% 选择性 \< 5ms（当前 \~12ms）

- 10 万行基准：ORDER BY 整数列 \< 30ms（当前 \~70ms）

- 10 万行基准：单列整数 Group By \< 5ms（当前 \~10ms）

---

### M2：MemoryEngine 上线

**目标**：第一个新引擎，验证多引擎架构可行性，快速出价值

**任务清单**：

|编号|任务|工作量|优先级|
|---|---|---|---|
|M2\-1|MemoryEngine 核心实现（内存 BTreeMap \+ 行存储）|3 天|P0|
|M2\-2|实现 StorageEngine trait（insert/delete/scan）|2 天|P0|
|M2\-3|主键点查 O\(1\) 优化|1 天|P0|
|M2\-4|范围扫描支持|1 天|P1|
|M2\-5|简化版 MVCC（快照读，无 WAL）|2 天|P1|
|M2\-6|SQL 层 `ENGINE = Memory` 语法支持|1 天|P0|
|M2\-7|Catalog 中 MemoryEngine 表元数据管理|1 天|P0|
|M2\-8|单元测试 \+ 集成测试|2 天|P0|
|M2\-9|SNAPSHOT 持久化模式（可选）|3 天|P2|

**验收标准**：

- MemoryEngine 点查 \< 1μs（对比 ColumnarEngine \~0\.1ms）

- MemoryEngine 写入 \< 1μs（对比 ColumnarEngine \~0\.2ms）

- 所有 SQL 操作在 MemoryEngine 表上正常工作

- 进程重启后 MemoryEngine 表为空（符合预期）

---

### M3：LogEngine 上线

**目标**：日志引擎，极致写入吞吐，验证 append\-only 引擎模式

**任务清单**：

|编号|任务|工作量|优先级|
|---|---|---|---|
|M3\-1|LogEngine 核心实现（列式追加写）|5 天|P0|
|M3\-2|块级压缩（自动选择编码）|3 天|P0|
|M3\-3|时间范围 MinMax 索引 \+ 跳读|2 天|P0|
|M3\-4|时间分区（按天/小时自动分块）|2 天|P1|
|M3\-5|后台异步索引构建（可选）|3 天|P2|
|M3\-6|SQL 层 `ENGINE = Log` 语法支持|1 天|P0|
|M3\-7|WAL 适配（LogEngine 可简化 WAL）|2 天|P1|
|M3\-8|单元测试 \+ 集成测试 \+ 性能测试|3 天|P0|

**验收标准**：

- 批量写入吞吐 \> 50 万行/秒（对比 ColumnarEngine \~5 万行/秒）

- 时间范围扫描 1\.5\-2x 快于 ColumnarEngine

- 不支持 UPDATE/DELETE（报错提示清晰）

- 不支持点查主键（走扫描，性能可接受即可）

---

### M4：跨引擎事务 \+ 统一 WAL

**目标**：完善事务体系，支持跨引擎事务

**任务清单**：

|编号|任务|工作量|优先级|
|---|---|---|---|
|M4\-1|WAL 记录格式标准化（带 engine\_type）|2 天|P0|
|M4\-2|事务管理器重构：协调多引擎提交/回滚|3 天|P0|
|M4\-3|两阶段提交（2PC）简化版：Prepare \+ Commit|3 天|P0|
|M4\-4|崩溃恢复：按引擎回放 WAL|3 天|P0|
|M4\-5|Checkpoint 机制：多引擎协调 checkpoint|2 天|P1|
|M4\-6|死锁检测（跨引擎）|2 天|P2|
|M4\-7|跨引擎事务集成测试|3 天|P0|

**验收标准**：

- 跨引擎事务 ACID 保证（原子性、一致性、隔离性、持久性）

- 崩溃后数据一致（断电测试）

- 单引擎事务性能不退化

- 跨引擎事务性能开销 \< 20%

---

### M5：查询优化器适配多引擎

**目标**：优化器知道不同引擎的代价模型，自动选择最优执行计划

**任务清单**：

|编号|任务|工作量|优先级|
|---|---|---|---|
|M5\-1|各引擎代价模型定义（scan\_cost / insert\_cost / join\_cost）|2 天|P0|
|M5\-2|优化器根据引擎代价选择 JOIN 顺序|3 天|P1|
|M5\-3|跨引擎 JOIN 策略（小表搬到大表引擎侧）|3 天|P1|
|M5\-4|下推优化：过滤/投影下推到各引擎|2 天|P0|
|M5\-5|引擎能力检测：某些引擎不支持的操作自动降级|2 天|P1|
|M5\-6|统计信息收集（各引擎独立收集）|3 天|P2|

**验收标准**：

- 跨引擎 JOIN 查询选择最优执行计划

- 各引擎发挥各自优势（列存做扫描聚合、内存做点查）

- 不支持的操作有清晰的降级路径

---

## 五、风险与应对

|风险|影响|概率|应对措施|
|---|---|---|---|
|PageAllocator 跨引擎共享产生碎片|中|中|后台 VACUUM \+ 按大小分类的 free list|
|跨引擎事务死锁|高|低|死锁检测 \+ 超时回滚|
|MemoryEngine 数据一致性（无持久化）|中|高|文档明确说明，提供持久化选项|
|多引擎导致代码复杂度爆炸|高|中|严格的 trait 边界 \+ 充分的单元测试|
|查询优化器多引擎代价模型不准|中|中|先做简单的规则优化，代价模型逐步迭代|
|单文件空间管理复杂|高|中|参考 SQLite 成熟方案，先做简单版再优化|

---

## 六、为什么 AI Agent 场景需要多引擎

### 6\.1 Agent 的多种工作负载

|工作负载|特征|最佳引擎|
|---|---|---|
|长期记忆存储|读多写少，结构化 \+ 向量，需要分析|ColumnarEngine|
|推理中间状态|高频读写，数据量小，丢了没关系|MemoryEngine|
|操作日志 / trace|写多读少，只追加，时序数据|LogEngine|
|工具调用缓存|点查为主，高频读写|MemoryEngine|
|对话历史|时间序列，需要分析|ColumnarEngine 或 LogEngine|
|Agent 配置 / 设置|KV 模式，数据量小|MemoryEngine \(持久化\)|

### 6\.2 单引擎的困境

如果只有 ColumnarEngine：

- 推理状态写入要走 WAL \+ Delta \+ MVCC，太慢了（几百微秒 vs 几纳秒）

- 日志写入要维护索引和 MVCC，浪费写入性能

- 为了兼顾写入，查询优化不敢做太激进

多引擎后：

- 每个引擎在自己的赛道上做到极致

- 用户按需选择，不用在「写入快 vs 读取快」之间妥协

- 数据库整体能力 = 各引擎能力的并集

---

## 七、竞品对比

|项目|单文件|多引擎|嵌入式|SQL|向量|语言|成熟度|
|---|---|---|---|---|---|---|---|
|**EngramDB**|✅|✅ 规划中|✅|✅|✅|Rust|早期|
|SQLite|✅|❌（虚拟表）|✅|✅|❌（需扩展）|C|极高|
|DuckDB|✅|❌（扩展）|✅|✅|✅（vss）|C\+\+|高|
|libSQL/Turso|✅|❌（扩展）|✅|✅|✅|C\+Rust|中高|
|MariaDB|❌|✅ 10\+|❌|✅|❌|C|极高|
|SurrealDB|⚠️|✅ 多后端|⚠️|❌（SurrealQL）|✅|Rust|中|
|PluresDB|⚠️|✅ 3种|✅|⚠️（有限）|✅|Rust|低|
|GlareDB|✅|❌（多数据源）|✅|✅|❌|Rust|中|
|Redb|✅|❌|✅|❌（KV）|❌|Rust|中|

**EngramDB 的差异化定位**：

- 不是做「又一个 SQLite」或「又一个 DuckDB」

- 而是做 **AI Agent 场景的多引擎嵌入式数据库**

- 同一张单文件里，不同表选不同引擎，各取所长

- 目前没有直接竞品

---

## 八、附录

### A\. 术语表

|术语|说明|
|---|---|
|StorageEngine|存储引擎 trait，所有引擎实现的统一接口|
|PageAllocator|页分配器，管理单文件内的页空间|
|WAL|Write\-Ahead Log，预写日志，保证崩溃一致性|
|MVCC|Multi\-Version Concurrency Control，多版本并发控制|
|Catalog|元数据目录，存所有表的定义和引擎类型|
|Delta Layer|Delta 层，列存的写入缓冲（类似 LSM 的 memtable）|
|Zone Map|区域图，存每个数据块的 Min/Max，用于跳读|
|Mark Index|稀疏主键索引，每 N 行一条记录，用于精确点查|
|Compact|合并，将 Delta 层数据合并入主列存|
|Checkpoint|检查点，将 WAL 中的变更固化到数据页|

### B\. 参考资料

- SQLite 文件格式：https://www\.sqlite\.org/fileformat\.html

- DuckDB 存储格式：https://duckdb\.org/internals/storage

- ClickHouse MergeTree：https://clickhouse\.com/docs/en/engines/table\-engines/mergetree\-family/mergetree

- MariaDB 存储引擎：https://mariadb\.com/kb/en/storage\-engines/

- MySQL 插件式存储引擎架构

### C\. 版本历史

|版本|日期|说明|
|---|---|---|
|v1\.0|2026\-08\-05|初始版本，完整的多引擎架构设计与开发计划|

