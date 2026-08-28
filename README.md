# EngramDB

> 专用分析型嵌入 AI Agent 数据引擎
> **当前版本：v0.22.0 — VIEW/CHECK约束/标量子查询/递归CTE/ALTER TABLE**

兼具 **SQLite 的事务能力（ACID）** 与 **DuckDB 的列存压缩与分析性能**，单文件嵌入式部署，面向 AI Agent 工作负载优化。

## 特性

- 📦 **单文件**：整个数据库是一个 `.hdb` 文件，备份/迁移零成本
- ⚡ **列存主存储**：列式存储 + ClickHouse 风格分类型压缩（Delta/Gorilla/FOR/Bit-pack/RLE/Dict/TokenDelta/zstd），分析查询性能优异
- 🔄 **多引擎架构**：Columnar（列存）/ Memory（全内存）/ Log（追加式时序）三种引擎，Auto 引擎自动分层
- 🔒 **完整事务**：WAL + MVCC，支持快照隔离、崩溃恢复、SAVEPOINT
- 🔍 **多维度索引**：稀疏主索引 + 跳表二级索引 + 位图索引 + 布隆过滤器 + HNSW 向量索引 + 倒排索引（FTS）
- 🚀 **向量化执行**：基于 DataChunk 的向量化查询引擎（1024 行/chunk），查询计划缓存
- 🧠 **AI Agent 友好**：JSON 类型 + Vector 类型 + HNSW 语义检索 + 全文检索（BM25）+ 混合检索
- 📊 **丰富类型**：BOOLEAN/SMALLINT/INT32/INT64/FLOAT32/FLOAT64/DECIMAL/VARCHAR/JSON/JSONB/VECTOR/VECTOR_INT8/BLOB/TIMESTAMP/**DATE**/**TIME**/**ARRAY**/**UUID**/**ENUM**
- 📋 **完整 SQL**：VIEW/CHECK约束/标量子查询/递归CTE/ALTER TABLE
- 🔧 **类型转换**：DATE_TO_STRING/TIME_TO_STRING/UUID_TO_STRING 等内置函数
- 🦀 **Rust 实现**：内存安全、零成本抽象、jemalloc 全局分配器

## 架构概览

```
SQL Interface → Query Planner → Optimizer (RBO + CBO) → Vectorized Executor
       ↓
Transaction Manager (MVCC + WAL + Group Commit + SAVEPOINT)
       ↓
Storage Engine: [Columnar] [Memory] [Log] [Auto (自动分层)]
       ↓
Delta Store (行存) → Column Store (列存 + 压缩)
       ↓
Index Layer: [Sparse] [Skiplist] [Bitmap] [Bloom] [HNSW] [Inverted (FTS)]
       ↓
Buffer Pool → Single File Format (.hdb)
```

## 快速开始

### 构建

```bash
cd engramdb
cargo build --release
```

### 运行示例

```bash
cargo run --example basic
```

### 交互模式

```bash
cargo run --release -- test.hdb
```

### 运行测试

```bash
cargo test
```

### 运行基准测试

```bash
cargo bench
# 或独立基准项目
cd benches/native_bench && cargo run --release
```

## 代码示例

### 基础 CRUD

```rust
use engramdb::{Connection, Value};

let mut conn = Connection::open(":memory:")?;

// 建表
conn.execute("CREATE TABLE users (id INT PRIMARY KEY, name VARCHAR, age INT)")?;

// 插入
conn.execute("INSERT INTO users VALUES (1, 'Alice', 30), (2, 'Bob', 25)")?;

// 查询
let result = conn.execute("SELECT name, age FROM users WHERE age > 26")?;
for row in &result.rows {
    println!("{}: {}", row[0], row[1]);
}
```

### Prepared Statement 批量写入

```rust
let stmt = conn.prepare("INSERT INTO logs VALUES (?, ?, ?)")?;
let batch = vec![
    vec![Value::Int64(1), Value::Varchar("info".into()), Value::Int64(100)],
    vec![Value::Int64(2), Value::Varchar("warn".into()), Value::Int64(200)],
];
let n = conn.execute_prepared_batch(&stmt, &batch)?;
```

### 零拷贝列式导入

```rust
// 跳过 SQL 层，直接列式写入，适合大批量 ETL
let columns = vec![
    vec![Value::Int64(1), Value::Int64(2), Value::Int64(3)],  // id 列
    vec![Value::Varchar("a".into()), Value::Varchar("b".into()), Value::Varchar("c".into())],  // name 列
];
let rows = conn.import_columns("users", columns)?;
```

### JSON 类型（AI Agent 元数据）

```sql
CREATE TABLE agent_meta (id INT, data JSON);

INSERT INTO agent_meta VALUES
  (1, '{"name":"agent1","role":"analyst","tools":["search","calc"]}'),
  (2, '{"name":"agent2","role":"coder","tools":["editor"]}');

-- 路径提取
SELECT JSON_EXTRACT(data, '$.name') FROM agent_meta;

-- 数组包含判断
SELECT id FROM agent_meta WHERE JSON_CONTAINS(data, '"search"', '$.tools');
```

### Vector 类型（语义检索）

```rust
// 建表 + 构建 HNSW 索引
conn.execute("CREATE TABLE embeddings (id INT, vec VECTOR)")?;
conn.create_vector_index("embeddings", "idx_vec", "vec",
    engramdb::storage::vector_index::DistanceMetric::Cosine, 16, 200)?;

// 插入向量后检索最近邻
let neighbors = conn.vector_search("embeddings", "idx_vec", &query_vec, 10)?;
for n in neighbors {
    println!("row_id={}, distance={}", n.row_id, n.distance);
}
```

### 事务

```rust
let mut txn = conn.begin()?;
// ... txn 上的操作
txn.commit()?;
```

```sql
BEGIN;
INSERT INTO users VALUES (3, 'Charlie', 35);
COMMIT;
```

## 支持的 SQL 语法

### DDL
```sql
CREATE TABLE table_name (
    column_name TYPE [PRIMARY KEY] [NOT NULL] [CHECK (expr)],
    ...
);
```

**支持类型**：`INT`, `BIGINT`, `SMALLINT`, `DOUBLE`, `FLOAT32`, `DECIMAL`/`NUMERIC`, `VARCHAR`, `BOOLEAN`, `JSON`, `JSONB`, `VECTOR`, `VECTOR_INT8`, `BLOB`, `TIMESTAMP`, `DATE`, `TIME`, `ARRAY`, `UUID`, `ENUM`

### 视图 / ALTER TABLE（v0.22）
```sql
-- 视图（定义随数据库持久化）
CREATE VIEW [OR REPLACE] view_name AS SELECT ...;
DROP VIEW [IF EXISTS] view_name;

-- ALTER TABLE
ALTER TABLE t ADD COLUMN c TYPE;
ALTER TABLE t DROP COLUMN c;
ALTER TABLE t RENAME COLUMN old TO new;
ALTER TABLE t RENAME TO new_name;
```

### CTE / 子查询（v0.22）
```sql
-- 递归 CTE（最大迭代 1000 次，防无限递归）
WITH RECURSIVE cnt(x) AS (
    SELECT 1
    UNION ALL
    SELECT x + 1 FROM cnt WHERE x < 10
)
SELECT * FROM cnt;

-- 标量子查询 / IN / EXISTS 子查询
SELECT name, (SELECT MAX(amount) FROM orders o WHERE o.uid = users.id) FROM users;
SELECT * FROM users WHERE id IN (SELECT uid FROM orders);
SELECT * FROM users WHERE EXISTS (SELECT 1 FROM orders o WHERE o.uid = users.id);
```

### DML
```sql
INSERT INTO table_name VALUES (...), (...);
SELECT * FROM table_name [WHERE condition] [ORDER BY col] [LIMIT n];
SELECT col1, col2, agg(...) FROM table_name [WHERE ...] [GROUP BY ...] [HAVING ...];
UPDATE table_name SET col = value WHERE condition;
DELETE FROM table_name WHERE condition;
```

### Join
```sql
SELECT a.*, b.* FROM a JOIN b ON a.id = b.aid;
SELECT * FROM a LEFT JOIN b ON a.id = b.aid;
```

### 事务
```sql
BEGIN;
COMMIT;
ROLLBACK;
```

### JSON 函数
```sql
JSON_EXTRACT(json_col, '$.path.to.field')
JSON_CONTAINS(json_col, '"value"', '$.array_field')
```

### Vector 函数
```sql
VECTOR_DISTANCE(vec1, vec2, 'cosine')  -- 'l2' | 'inner' | 'cosine'
```

## 性能调优

### WAL 组提交
```rust
// Sync 模式下多条事务共享一次 fsync，吞吐提升 5-20x
conn.set_wal_group_commit_size(16);
conn.set_wal_flush_mode(engramdb::WalFlushMode::Sync);
```

### Delta 合并策略
```rust
use engramdb::CompactStrategy;

// 自适应分桶（默认推荐）
conn.set_compact_strategy(CompactStrategy::default_adaptive(100_000));

// 按 session_id 聚簇，加速会话范围查询
conn.set_cluster_key("agent_logs", "session_id")?;

// 手动合并
conn.compact_all()?;
```

## 项目结构

```
engramdb/
├── src/
│   ├── main.rs              # CLI 入口
│   ├── lib.rs               # 库入口，Connection / Value / QueryResult
│   ├── common/              # 通用工具
│   │   ├── types.rs         # 数据类型 / 列定义 / 表定义 / EngineType
│   │   ├── error.rs         # 错误类型
│   │   ├── config.rs        # 配置（页大小、压缩、合并策略、WAL 模式、Tokenizer）
│   │   ├── column_data.rs   # 列式数据结构（Typed 缓存）
│   │   ├── memory_pool.rs   # 内存池
│   │   ├── tokenizer.rs     # 统一 Tokenizer（v0.21 NFKC 归一化）
│   │   ├── pretokenize.rs   # 预 tokenize 缓存
│   │   ├── huffman.rs       # Huffman 编解码（TokenDelta 熵编码层）
│   │   ├── vocab_file.rs    # 词表文件加载
│   │   └── value_cmp.rs     # Value 跨类型比较
│   ├── storage/             # 存储引擎
│   │   ├── engine.rs        # 多引擎抽象层（StorageEngine trait + EngineTable 枚举）
│   │   ├── capabilities.rs  # 引擎能力矩阵（planner 前置校验：索引/FTS/ALTER 等）
│   │   ├── file_format.rs   # 单文件格式（文件头、页布局）
│   │   ├── buffer_pool.rs   # LRU 缓冲池
│   │   ├── column_store.rs  # 列存主存储（Row Group + Zone Map + Bloom）
│   │   ├── delta_store.rs   # 行存 Delta 层
│   │   ├── table.rs         # Columnar 引擎表（列存 + Delta + 索引）
│   │   ├── memory_engine.rs # Memory 引擎（全内存，Agent session 缓存）
│   │   ├── log_engine.rs    # Log 引擎（追加式时序，块级 MinMax 跳读）
│   │   ├── sparse_index.rs  # 稀疏主索引（ClickHouse 风格）
│   │   ├── vector_index.rs  # HNSW 向量索引
│   │   ├── bloom_filter.rs  # 列级 Bloom Filter（等值跳读）
│   │   ├── bloom.rs         # Bloom Filter 实现
│   │   ├── cache.rs         # KV 缓存引擎
│   │   ├── catalog.rs       # Catalog 持久化
│   │   ├── heat_tracker.rs  # 访问热度追踪（Auto 引擎调度）
│   │   ├── tier_migration.rs# 分层迁移决策
│   │   ├── migration.rs     # 引擎间数据迁移
│   │   ├── insert_batcher.rs# INSERT 攒批合并器
│   │   ├── rate_limiter.rs  # 滑动窗口限流器
│   │   ├── manifest.rs      # LSM 段文件清单
│   │   ├── segment.rs       # LSM 段文件
│   │   ├── async_compress.rs# 异步块压缩（rayon）
│   │   ├── mmap_reader.rs   # mmap 读路径（feature = "mmap-read"）
│   │   ├── mmap_writer.rs   # mmap 写路径
│   │   ├── mmap_integration.rs # mmap 集成
│   │   ├── compact_mmap.rs  # compact + mmap 集成
│   │   ├── compression/     # 压缩算法
│   │   │   ├── rle.rs       # RLE
│   │   │   ├── dictionary.rs# 字典编码
│   │   │   ├── bitpacking.rs# 位打包
│   │   │   ├── for_encoding.rs # Frame of Reference
│   │   │   ├── delta.rs     # Delta 编码
│   │   │   ├── double_delta.rs # DoubleDelta
│   │   │   ├── gorilla.rs   # Gorilla XOR（浮点）
│   │   │   ├── token_delta.rs # TokenDelta（v0.21 文本压缩）
│   │   │   ├── token_stream_cache.rs # 行级 token 流缓存
│   │   │   └── recommender.rs # 列级自动编码选择
│   │   └── index/           # 二级索引
│   │       ├── skiplist.rs  # 跳表索引
│   │       ├── bitmap.rs    # 位图索引
│   │       ├── bloom.rs     # 布隆过滤器
│   │       └── inverted_index.rs # 倒排索引
│   ├── wal/                 # WAL 预写日志 + ARIES 崩溃恢复
│   │   ├── writer.rs        # WAL 写入器（组提交）
│   │   ├── reader.rs        # WAL 读取器
│   │   └── recovery.rs      # 崩溃恢复（Redo + Undo）
│   ├── txn/                 # 事务管理
│   │   ├── manager.rs       # 事务管理器
│   │   ├── mvcc.rs          # MVCC 多版本并发控制
│   │   └── transaction.rs   # 事务生命周期
│   ├── sql/                 # SQL 子系统
│   │   ├── parser.rs        # SQL 解析器（sqlparser-rs）
│   │   ├── ast.rs           # 抽象语法树
│   │   ├── planner.rs       # AST → 物理计划
│   │   ├── optimizer.rs     # 查询优化器（RBO + CBO）
│   │   ├── cost_model.rs    # 成本模型
│   │   ├── statistics.rs    # 统计信息
│   │   ├── join_order.rs    # Join 顺序优化
│   │   ├── udf.rs           # 用户定义函数
│   │   ├── fast_insert.rs   # 快速 INSERT 路径
│   │   ├── ingestion.rs     # 文档摄入 API
│   │   ├── materialized_view.rs  # 物化视图
│   │   └── arrow_integration.rs  # Arrow 格式互转
│   ├── executor/            # 向量化执行引擎
│   │   ├── executor.rs      # 执行入口
│   │   ├── expression.rs    # 表达式求值
│   │   ├── physical_plan.rs # 物理计划枚举
│   │   ├── vector.rs        # DataChunk 向量数据块
│   │   ├── arena.rs         # 查询 Arena（bumpalo）
│   │   └── operators/       # 物理算子
│   │       ├── table_scan.rs# 表扫描
│   │       ├── filter.rs    # 过滤
│   │       ├── projection.rs# 投影
│   │       ├── insert.rs    # 插入（含攒批）
│   │       ├── update.rs    # 更新
│   │       ├── delete.rs    # 删除
│   │       ├── aggregate.rs # 聚合
│   │       ├── sort.rs      # 排序（Top-N 堆排序）
│   │       ├── hash_join.rs # Hash Join
│   │       ├── window.rs    # 窗口函数
│   │       ├── alter_table.rs # ALTER TABLE
│   │       └── pragma.rs    # PRAGMA
│   ├── search/              # 检索引擎（v0.21）
│   │   ├── sparse.rs        # Token 级倒排索引
│   │   ├── bm25.rs          # BM25 排序检索
│   │   ├── fuzzy.rs         # 模糊匹配（编辑距离/n-gram）
│   │   └── hybrid.rs        # RRF 混合检索（sparse + dense）
│   └── datafusion_ext/      # DataFusion TableProvider 集成（feature = "datafusion"）
├── tests/                   # 集成测试（19 个）
├── benches/                 # 基准测试（28 个 + native_bench 子项目）
├── examples/                # 示例（28 个）
├── docs/                    # 文档
├── scripts/                 # 构建脚本、Python 基准工具
│   └── benchmarks/          # Python 性能对比脚本
└── archive/                 # 归档的历史版本（不参与编译）
```

## 版本历史

完整变更记录见 [CHANGELOG.md](CHANGELOG.md)。

- **v0.22**：VIEW 支持 + CHECK 约束 + 标量子查询 + 递归 CTE + ALTER TABLE Parser
- **v0.21**：TokenDelta 检索引擎 + 统一 Tokenizer + BM25 + 混合检索
- **v0.20**：约束表攒批 + 批内自重复检测
- **v0.19**：分层索引（Delta 稀密 + 列存稀疏）
- **v0.18**：查询计划缓存 + INSERT 攒批合并 + jemalloc
- **v0.17**：多引擎架构 + Memory/Log 引擎 + HeatTracker + 分层迁移
- **v0.16**：Varchar → Vector/VectorInt8 类型强转
- **v0.15**：VectorInt8 + TTL + TRUNCATE + SAVEPOINT + PRAGMA + JSON 函数
- **v0.14**：FLOAT32/TIMESTAMP 类型 + AUTO_INCREMENT + UNIQUE 约束 + INSERT...RETURNING + UPSERT
- **v0.13**：事务写入 2.63x SQLite + 主键索引持久化 + COUNT(*) O(1) + ALTER TABLE + PRAGMA + Prepared Statement
- **v0.12**：JSON + Vector 类型支持（AI Agent 工作负载）
- **v0.11**：向量化写入 / 零拷贝导入 / WAL 组提交 / Compact 策略
- **v0.8-v0.10**：SQL 完善 + Join + 查询优化器（RBO + CBO）
- **v0.7**：WAL + MVCC 完整 ACID 事务 + 多维度索引 + 分类型压缩 + 性能基准
- **v0.6**：HNSW 向量检索引擎
- **v0.5**：轻量级压缩算法（RLE / Dictionary / Bit-packing / FOR）
- **v0.4**：ClickHouse 风格稀疏主索引 + 数据跳数索引
- **v0.3**：列存主存储 + 行存 Delta 层混合架构
- **v0.2**：SQL 解析器 + 向量化执行引擎 MVP
- **v0.1**：项目初始化 + 单文件格式设计

## SQL 路线图

完整 SQL 支持分 5 个阶段推进：

- **Phase 1 (v0.8.x)**：基础 SQL 完善 — 表达式系统、排序分页、聚合增强、UPDATE/DELETE、索引 DDL ✅
- **Phase 2 (v0.9.x)**：Join + 高级查询 — HashJoin、子查询、Set 操作、CTE ✅
- **Phase 3 (v0.10.x)**：查询优化器 — RBO 规则集、统计信息 + CBO、索引选择优化 ✅
- **Phase 4 (v0.11.x)**：高级特性 — 窗口函数、复杂类型 (ARRAY/JSON)、物化视图、向量 SQL ✅
- **Phase 5 (v0.12.x+)**：生产级完善 — 事务隔离级别、权限安全、兼容性增强 ✅

详细规划见 [docs/sql_roadmap.md](docs/sql_roadmap.md)

## 技术方案

详细技术方案见 [docs/01-technical-design.md](docs/01-technical-design.md)

## 许可证

[MIT](LICENSE)
