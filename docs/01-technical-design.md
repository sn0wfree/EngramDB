# HybridDB 技术方案文档

> 高性能压缩、支持事务的关系型单文件数据库
> 版本：v0.7 (WAL + MVCC 完整 ACID 事务) | 日期：2026-08-01

---

## 1. 项目定位与目标

### 1.1 核心目标

打造一个**单文件嵌入式关系型数据库**，兼具：
- **SQLite 的事务能力**：完整 ACID、WAL、MVCC、崩溃恢复
- **DuckDB 的列存压缩与分析性能**：列式存储、轻量级压缩、向量化执行

### 1.2 设计哲学

- **单文件**：整个数据库是一个文件（类似 `.db` / `.duckdb`），备份/迁移/分享零成本
- **嵌入式**：库形式存在，无独立进程，直接链接到应用
- **混合存储**：主存列存（分析友好）+ 行存 Delta 层（写入友好）
- **事务优先**：从第一天起就保证 ACID，而非事后补丁

### 1.3 适用场景

- 嵌入式数据分析：设备端/边缘端的本地数据仓库
- 应用内嵌数据库：需要同时支持事务写入和分析查询
- 数据科学原型：单文件、零配置、分析性能强
- 不适用：高并发 OLTP、分布式场景、TB 级以上数据

---

## 2. SQLite vs DuckDB 深度对比

### 2.1 架构对比

| 维度 | SQLite | DuckDB |
|------|--------|--------|
| 存储模型 | 行存 B+Tree | 列存 Row Group |
| 页/块大小 | 4KB (默认) | 256KB Block |
| 压缩 | 无（页内变长编码） | RLE/Bit-pack/Dict/FSST/Chimp |
| 执行模型 | VDBE 字节码（逐行） | 向量化执行（1024 行/chunk） |
| 事务 | WAL + 锁状态机 | WAL + Checkpoint |
| 并发 | 单写多读（WAL 模式 MVCC） | 单写多读 |
| 索引 | B-Tree 索引 | Art Tree 索引（可选） |
| 语言 | C | C++ |
| 代码量 | ~150K 行 | ~200K+ 行 |

### 2.2 各自优势

**SQLite 优势：**
- 极致的事务可靠性（经过 20 年工业验证）
- 小数据量点查极快（B-Tree O(log n)）
- 生态极其完善（几乎所有平台/语言绑定）
- 单文件格式极其稳定（2004 年至今向前兼容）

**DuckDB 优势：**
- 分析查询性能碾压（列存 + 向量化 + 并行）
- 压缩率极高（列存 + 轻量级编码，通常 3-10x）
- 现代查询优化器（基于成本、Join 重排等）
- 单文件 + 零依赖的嵌入式体验

### 2.3 各自短板

**SQLite 短板：**
- 行存导致分析查询慢（全表扫描时读所有列）
- 无压缩，磁盘占用大
- 无向量化执行，CPU 效率低
- 无原生并行查询

**DuckDB 短板：**
- 列存导致单行写入/更新慢（需重写整个 Row Group）
- 事务能力相对弱（MVCC 但优化点查不如行存）
- 存储格式 1.0 才稳定（2024 年），历史较短

---

## 3. 整体架构设计

### 3.1 架构总览

```
┌─────────────────────────────────────────────────────────┐
│                      SQL Interface                      │
│  (Parser → Planner → Optimizer → Executor)              │
│  Optimizer: RBO · 谓词下推 · PREWHERE 下推              │
├─────────────────────────────────────────────────────────┤
│              Vectorized Execution Engine                │
│  DataChunk · Vector · SelectionVector · LazyDataChunk   │
│  Operators: Scan · Filter · Projection · Aggregate      │
│  Optimizations: SIMD友好 · Partial+Merge · 零拷贝过滤   │
├─────────────────────────────────────────────────────────┤
│                   Transaction Manager                   │
│  (MVCC · Snapshot · Savepoint · Commit/Rollback)       │
├─────────────────────────────────────────────────────────┤
│                   Storage Engine                        │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐  │
│  │  Delta Store │  │ Column Store │  │ Sparse Index │  │
│  │  (行存, 热)  │  │ (列存, 冷)   │  │ (ClickHouse) │  │
│  └──────┬───────┘  └──────┬───────┘  └──────┬───────┘  │
│         │                 │                 │          │
│         │           MinMax / BF 跳过索引                │
│         └─────────────────┼─────────────────┘          │
│                           │                            │
│                    ┌──────▼──────┐                     │
│                    │ Buffer Pool │                     │
│                    │  (LRU-K)    │                     │
│                    └──────┬──────┘                     │
├───────────────────────────┼─────────────────────────────┤
│                           │                            │
│  ┌────────────────────────▼─────────────────────────┐  │
│  │              Write-Ahead Log (WAL)               │  │
│  │  (追加写 · 崩溃恢复 · Checkpoint 到主存储)        │  │
│  └────────────────────────┬─────────────────────────┘  │
├───────────────────────────┼─────────────────────────────┤
│                           │                            │
│  ┌────────────────────────▼─────────────────────────┐  │
│  │           Single File Format (.hdb)              │  │
│  │  Header · Metadata · Data Blocks · Free List     │  │
│  └──────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────┘
```

### 3.2 核心设计决策

#### 决策 1：混合存储架构（LSM 灵感）

**方案：主列存 + 行存 Delta 层**

```
写入路径:  SQL → WAL → Delta Store (行存, MemTable + 磁盘 Delta)
                      ↓ (定期 Compaction)
                 Column Store (列存, 主存储)

读取路径:  SQL → 合并 Delta + Column Store → 结果
```

**为什么不是纯列存？**
- 纯列存单行写入需要重写整个 Row Group (~120K 行)，写放大严重
- Delta 层吸收随机写，批量合并到列存，兼顾写入性能和分析性能
- 类似 LSM-tree 的思路，但底层是列存而非 SSTable

**为什么不是纯行存 + 列存索引？**
- 行存主存的压缩率远低于列存
- 分析查询仍需扫描全表（即使有索引也不如列存）
- 我们的目标是分析性能优先，事务写入够用即可

#### 决策 2：WAL + MVCC 事务模型

**借鉴 SQLite WAL 模式，做适配：**
- 写入只追加 WAL，不修改主文件 → 读写不阻塞
- Reader 通过快照读取（MVCC），Writer 不阻塞 Reader
- Checkpoint 定期将 WAL 合并到主文件
- 崩溃恢复通过 WAL 重放

**与 SQLite 的区别：**
- SQLite WAL 存的是完整页面（页级）
- 我们的 WAL 存的是逻辑变更（行级），因为列存页结构复杂
- 更接近 PostgreSQL 的逻辑 WAL 思路

#### 决策 3：语言选型 — Rust

| 候选 | 优势 | 劣势 |
|------|------|------|
| **Rust** | 内存安全、零成本抽象、现代工具链、cargo 生态 | 学习曲线陡、编译慢 |
| C | 最高性能、生态最成熟 | 内存不安全、构建工具原始 |
| C++ | 性能高、模板元编程强 | 内存不安全、历史包袱重 |
| Zig | 现代、手动内存管理 | 生态不成熟、稳定性存疑 |

**选择 Rust 的理由：**
1. 系统级性能（与 C/C++ 同量级）
2. 内存安全（无 UB，数据库这种长驻进程极其重要）
3. 现代工具链（cargo 构建、测试、依赖管理一体化）
4. 丰富的异步/并发生态
5. 社区活跃，数据库项目众多（TiKV、SurrealDB、GreptimeDB 等）

#### 决策 4：向量化执行引擎

- 采用向量化执行（Vectorized Execution），而非 Volcano 逐行或编译执行
- Vector 大小：2048 行（平衡缓存友好性和调度开销）
- 支持多种 Vector 表示：Flat / Constant / Dictionary / Sequence
- 执行器：基于 DataChunk 的流水线执行

---

## 4. 文件格式设计

### 4.1 总体布局

```
┌──────────────────────────────────────────────────────┐
│                   File Header (4KB)                  │
│  Magic · Version · Page Size · Meta Root · ...       │
├──────────────────────────────────────────────────────┤
│              Metadata Blocks (可变)                  │
│  Schema · Table Stats · Row Group Index · Free List  │
├──────────────────────────────────────────────────────┤
│                                                      │
│              Data Blocks (可变大小)                  │
│  ┌──────────────────────────────────────────────┐    │
│  │  Row Group 1 (122,880 rows)                  │    │
│  │  ┌─────┬─────┬─────┬─────┬─────┐            │    │
│  │  │ Col0│ Col1│ Col2│ ... │ ColN│            │    │
│  │  └─────┴─────┴─────┴─────┴─────┘            │    │
│  └──────────────────────────────────────────────┘    │
│  ┌──────────────────────────────────────────────┐    │
│  │  Delta Region (行存 B-Tree 页)               │    │
│  └──────────────────────────────────────────────┘    │
│                                                      │
├──────────────────────────────────────────────────────┤
│                  WAL Region (可选)                    │
│  (注：MVP 阶段 WAL 为独立文件，后续合并入单文件)       │
└──────────────────────────────────────────────────────┘
```

### 4.2 文件头 (4KB 对齐)

| 偏移 | 大小 | 字段 | 说明 |
|------|------|------|------|
| 0 | 16 | magic | "HYBRIDDB_FORMAT1\0" |
| 16 | 2 | version | 格式版本号 |
| 18 | 2 | page_size | 页大小（默认 4096） |
| 20 | 2 | block_size | 块大小（默认 262144 = 256KB） |
| 22 | 4 | meta_root | 元树根页号 |
| 26 | 8 | total_rows | 总行数（估算） |
| 34 | 8 | total_data_blocks | 数据块总数 |
| 42 | 4 | schema_cookie | Schema 版本号 |
| 46 | 4 | checkpoint_lsn | 最近 Checkpoint LSN |
| 50 | 16 | uuid | 数据库唯一标识 |
| 66 | 2 | compression_default | 默认压缩算法 |
| 68 | ... | 保留 | 对齐到 4KB |

### 4.3 列存数据格式

**Row Group 结构（默认 122,880 行）：**

```
Row Group Header
├── row_count (4B)
├── column_count (2B)
├── column_offsets [column_count] (各列 chunk 的偏移)
└── column_sizes [column_count] (各列 chunk 的大小)

Column Chunk 0
├── compression_type (1B)
├── uncompressed_size (4B)
├── compressed_size (4B)
├── min_value (变长)
├── max_value (变长)
├── null_count (4B)
└── compressed_data...

Column Chunk 1
...
```

### 4.4 Delta 层格式

Delta 层采用简化的 B+Tree 行存结构（类似 SQLite 但更简单）：
- 页大小：4KB
- 键：rowid (64-bit)
- 值：行数据（变长编码）
- 只存最新版本，历史版本在 WAL 中

### 4.5 压缩算法

| 算法 | 适用场景 | 压缩率 (实测) | 编码速度 | 解码速度 |
|------|----------|--------------|---------|---------|
| **Uncompressed** | 小数据/随机访问/随机数据 | 1x | 最快 | 最快 |
| **RLE** | 排序后高度重复列（状态/类别/分区键） | 100–10000x | ~16 亿行/秒 | ~17 亿行/秒 |
| **Dictionary** | 低基数列（< 65536 不同值） | 3–20x | ~1 亿行/秒 | ~13 亿行/秒 |
| **Bit-packing / FOR** | 数值列、值域有限（ID/时间戳/度量） | 2–10x | 0.2–3 亿行/秒 | 0.7–8 亿行/秒 |
| **Delta** (规划中) | 递增/递减序列 | 高 | 快 | 快 |
| **FSST** (规划中) | 高基数字符串 | 中-高 | 中 | 中 |
| **Chimp/Alp** (规划中) | 浮点/时间序列 | 中-高 | 中 | 中 |
| **Zstd** (规划中) | 通用冷数据/归档 | 2–10x | 中-慢 | 中 |

> 实测数据来源：`benches/compression_bench.rs`，10 万行 INT 列，Rust release 单核

**自动选择策略**：每个 Column Chunk 独立选择最优压缩算法：
1. 依次尝试 RLE → Dictionary → Bit-packing
2. 选择压缩率最高且 > 1.2x 的算法
3. 全部 < 1.2x 时标记为 None（不压缩）
4. Chunk 头部存储压缩类型标识，读取时自动解码

> 这也是 ClickHouse / Parquet / ORC 等主流列存系统的通用策略。

**轻量级 vs 重量级压缩**：互补而非替代
- **轻量级**（RLE/Dict/BitPack）：编解码极快（接近内存带宽），压缩率中等，适合热数据
- **重量级**（zstd/LZ4）：压缩率高，编解码慢，适合冷数据/归档

---

## 5. 性能优化体系（ClickHouse 借鉴）

> 本章整合 ClickHouse 经过工业验证的核心性能优化手段，从存储层到执行层全链路优化。

### 5.1 存储层优化

#### 5.1.1 稀疏主索引 (Sparse Primary Index)

**借鉴 ClickHouse MergeTree 稀疏索引思想**：每 N 行（granule）只记一条索引，索引体积极小，可全量缓存内存。

- **Granule 大小**：默认 8192 行/granule（与 ClickHouse 一致）
- **索引大小**：10 亿行仅需数 MB（vs B+Tree 数 GB）
- **索引结构**：每 granule 首行的主键值 + 该 granule 的起始偏移
- **查询时**：先通过稀疏索引定位到 granule 范围，再扫描对应数据块

```
primary_idx (sparse index, 每 8192 行一条):
┌──────────┬──────────┬──────────┬──────────┐
│ granule 0│ granule 1│ granule 2│ granule 3│
│ (key: 1) │ (key:8193│(key:16385│(key:24577│
└──────────┴──────────┴──────────┴──────────┘
       ↓         ↓          ↓          ↓
data: [0-8191] [8192-16383] [16384-24575] [24576-32767]
```

**vs B+Tree 稠密索引**：
- 索引体积减少 1000x+ → 全量缓存内存 → 无随机 IO
- 列存 + 顺序扫描 granule，带宽利用率高

#### 5.1.2 数据跳过索引 (Data Skipping Index)

在稀疏主索引之外，提供多种二级跳过索引，进一步减少扫描数据量：

| 索引类型 | 适用场景 | 原理 |
|----------|----------|------|
| **MinMax** | 数值/时间列 | 每 granule 存 min/max，过滤范围外的 granule |
| **Bloom Filter** | 等值查询 | 每 granule 建布隆过滤器，快速判断值不存在 |
| **Set** | 低基数列 | 每 granule 存值集合，精确判断 |
| **Ngram BF** | 字符串模糊查询 | n-gram 布隆过滤器，支持 LIKE 优化 |

**MinMax 索引**（默认启用，每列自动维护）：
- 列 chunk 头已包含 min/max 值（已有基础）
- 查询时先检查 min/max 与查询范围是否重叠
- 不重叠则跳过整个 granule，无需解压读取

#### 5.1.3 有序存储 + 压缩率最大化

**ORDER BY 决定物理布局**：数据按主键有序存储，相邻行值相近：
- **压缩率提升**：有序数据压缩率越高（RLE/FOR/Delta 效果越好）
- **索引效率提升**：有序数据的 min/max 区间不重叠，跳过索引效果好
- **范围查询加速**：连续 granule，顺序读

**排序键设计原则**：
1. 低基数列在前，高基数列在后
2. 常用过滤列优先
3. 时间维度列通常放第一

#### 5.1.4 延迟物化 (Lazy Materialization)

**核心思想**：列数据的读取推迟到真正需要时才进行，避免无用 I/O。

**Top N 查询优化流程**：
```
传统流程: 读所有列 → 过滤 → 排序 → LIMIT → 返回
延迟物化: 读排序列 → 排序 → LIMIT → 再读其他列
```

**效果**：Top N 查询可加速数十到上千倍（ClickHouse 实测 1576x 加速）。

**适用场景**：
- ORDER BY + LIMIT（Top N 查询）
- 高选择性 WHERE + 少量结果
- 宽表查询（只需要少数列）

**实现要点**：
- 先读过滤列 + 排序列，定位到行号
- 再读其他列，只读取需要的行
- 列存天然支持任意列独立读取

#### 5.1.5 PREWHERE 优化

**两阶段过滤**：
1. **第一阶段（PREWHERE）**：只读取过滤条件列，计算匹配行号
2. **第二阶段（WHERE）**：只读取匹配行的其他列

**效果**：
- 过滤条件列少 → 第一阶段 I/O 小
- 过滤率高时，第二阶段只需要读少量行
- 减少解压量

### 5.2 执行层优化

#### 5.2.1 向量化执行 + SIMD

**已有基础**：DataChunk 2048 行/批，列连续内存，SIMD 友好。

**SIMD 优化方向**：
- **过滤**：SIMD 比较，一次比较 8/16 个值
- **聚合**：SIMD 累加，一次算 4/8 个值
- **字符串**：SIMD 字符串比较

**编译时自动向量化**：
- 紧密循环，无函数调用，无分支
- 编译选项：`-O3 -march=native`
- Rust 依赖 LLVM 自动向量化

#### 5.2.2 多核并行执行

**Pipeline Stream 模型**：
- 每 CPU 核心一个 pipeline stream
- 每个 stream 独立扫描不同的数据范围
- 各自产生 partial 结果，最后合并

**聚合并行**：
- 每个 stream 建一个哈希表
- 最后合并所有哈希表
- 多种哈希表变体，自动选择

**并行度**：默认 = CPU 核心数

#### 5.2.3 部分聚合状态 (Partial Aggregation States)

**思想**：聚合分为两阶段：
1. **Partial 阶段**：每个 stream 局部聚合，产生 partial state
2. **Merge 阶段**：合并所有 partial state，得到最终结果

**可合并状态**：
- COUNT: count → sum(count)
- SUM: sum → sum(sum)
- AVG: (sum, count) → sum(sum)/sum(count)
- MIN: min → min(min)
- MAX: max → max(max)
- HLL: sketch → merge sketch

**好处**：
- 并行高效，无锁
- 可分布式扩展
- 内存占用小（大部分工作已在 partial 阶段完成）

#### 5.2.4 选择向量 (Selection Vector)

**过滤结果用选择向量而非复制**：
- 过滤后不复制数据，只记录行号索引
- 后续操作按选择向量读取
- 减少内存复制，减少 cache miss

**Selection Vector 大小**：N 行只需要 N * 2 字节（u16 行号）
- 过滤率高时节省大量内存操作

### 5.3 缓存与调度优化

#### 5.3.1 多级缓存

**缓存层次**：
- **L0**: 寄存器
- **L1**: L1/L2/L3 CPU 缓存
- **L2**: 缓冲池 (Buffer Pool)
- **L3**: 文件系统缓存
- **L4**: 磁盘

**缓存优化**：
- 热点数据缓存
- 预读：顺序读预读下一个 granule
- 缓存淘汰：LRU + 2Q 算法

#### 5.3.2 Merge 策略

**借鉴 MergeTree 合并策略**：
- 后台线程定期合并小 part
- 控制 part 数量在合理范围
- 合并时重新排序、重新压缩
- Merge 不阻塞读写（LSM 思想）

**Merge 调度**：
- 小 part 优先合并
- 避免大 merge 占用过多资源
- 可配置 merge 速度限制

---

## 6. 向量检索引擎（HNSW）

### 6.1 设计目标

作为 AI Agent 原生数据库的核心差异化能力，内置向量检索引擎支持：
- **近似最近邻搜索 (ANN)**：亚毫秒级检索亿级向量
- **多种距离度量**：L2 欧氏距离、内积 (IP)、余弦相似度
- **混合查询**：SQL 结构化过滤 + 向量相似度排序
- **嵌入式零依赖**：纯 Rust 实现，无外部引擎依赖

### 6.2 算法选型：HNSW

**为什么选 HNSW**：

| 算法 | 召回率 | 构建速度 | 查询速度 | 内存占用 | 实现复杂度 |
|------|--------|----------|----------|----------|------------|
| HNSW | 高 | 中 | 极快 | 中（图连接） | 中 |
| IVF | 中 | 快 | 快 | 低 | 低 |
| Annoy | 中 | 快 | 快 | 中 | 低 |
| ScaNN | 高 | 慢 | 极快 | 高 | 高 |

HNSW 综合性能最优，是当前工业界主流选择（FAISS、Milvus、Weaviate 均采用）。

### 6.3 HNSW 核心原理

**分层导航小世界图 (Hierarchical Navigable Small World)**：

- **多层图结构**：第 0 层包含所有节点，越往上节点越少（几何分布）
- **贪婪搜索**：从顶层入口点开始，逐层向下逼近目标
- **参数 M**：每层每个节点的最大连接数（控制图密度）
- **参数 ef**：搜索宽度（控制召回率与速度的权衡）

**层数分布**：
- 节点出现在第 l 层的概率：P(level ≥ l) = 1 / M^l
- 第 0 层：所有节点（N 个）
- 第 1 层：约 N/M 个节点
- 第 2 层：约 N/M² 个节点
- 总层数：log_M(N)

### 6.4 距离度量

**L2 欧氏距离平方**（默认，不影响排序）：
```
dist(a, b) = Σ(a_i - b_i)²
```

**内积**（归一化后等价于余弦相似度）：
```
dist(a, b) = -Σ(a_i × b_i)  // 取负，统一为"越小越近"
```

**余弦相似度**（自动归一化）：
```
sim(a, b) = Σ(a_i × b_i) / (||a|| × ||b||)
dist(a, b) = -sim(a, b)
```

### 6.5 核心数据结构

```rust
pub struct HnswIndex {
    config: HnswConfig,       // 配置（dim, M, efConstruction, efSearch, metric）
    nodes: Vec<HnswNode>,     // 节点数组
    enter_point: Option<u32>, // 顶层入口点
    max_level: i32,           // 最大层数
}

struct HnswNode {
    id: u32,
    vector: Vec<f32>,
    layers: Vec<Vec<u32>>,    // layers[level] = 该层邻居 ID 列表
}
```

### 6.6 关键操作

**插入流程**：
1. 随机生成新节点的最大层数（几何分布）
2. 从顶层入口点贪婪下降到新节点层 + 1
3. 从新节点层向下到第 0 层，每层：
   - 搜索 efConstruction 个最近邻
   - 连接到最近的 M 个邻居（双向连接）
   - 邻居已满时启发式替换（保留更近的）
4. 新节点层数超过当前最大层时，更新入口点

**搜索流程**：
1. 从顶层入口点贪婪下降到第 1 层
2. 在第 0 层做 best-first 搜索，维护 efSearch 个候选
3. 返回最近的 K 个结果

**提前终止**：当候选中最近的都比结果中最远的还远时，停止搜索。

### 6.7 性能指标（实测）

**测试环境**：128 维向量，10K 数据集，K=10，L2 距离，单核

| 配置 | 召回率 | 查询延迟 | QPS | 加速比（vs 暴力） |
|------|--------|----------|-----|-------------------|
| 暴力搜索 (baseline) | 100% | 1.41 ms | 708 | 1x |
| M=16, ef=50 | 63.0% | 0.21 ms | 4,791 | 6.7x |
| M=16, ef=100 | 76.5% | 0.34 ms | 2,949 | 4.2x |
| M=16, ef=200 | 84.1% | 0.54 ms | 1,863 | 2.6x |
| M=32, ef=100 | 89.2% | 0.52 ms | 1,938 | 2.7x |
| M=32, ef=200 | **94.5%** | 0.79 ms | 1,272 | 1.8x |

**构建性能**：
- M=16, efCon=100：~2,700 向量/秒
- M=32, efCon=200：~1,070 向量/秒
- 平均连接数/节点：M=16 时约 33，M=32 时约 65

### 6.8 与存储引擎的集成

向量列在列存中以独立结构存储：
- **向量数据**：按 Row Group 存储，float32 数组
- **HNSW 索引**：独立索引段，持久化到单文件
- **延迟物化**：先过滤行号，再取向量距离（类似 PREWHERE）
- **混合查询**：SQL WHERE 子句过滤 + 向量相似度排序

### 6.9 未来优化方向

- **启发式邻居选择**：从"最近 M 个"升级为论文算法 4（考虑多样性）
- **多线程构建**：并行插入，提升索引构建速度
- **PQ 量化**：乘积量化压缩向量，降低内存占用
- **动态删除**：支持向量删除与墓碑标记
- **IVF 混合索引**：大规模场景下的备选方案

---

## 7. 事务模型

### 7.1 WAL 格式

WAL 为独立文件（`.hdb-wal`），追加写入：

```
WAL Header
├── magic (8B)
├── version (2B)
├── db_uuid (16B)
└── initial_lsn (8B)

WAL Record (变长)
├── lsn (8B) — 日志序列号
├── type (1B) — INSERT/UPDATE/DELETE/COMMIT/ROLLBACK/CHECKPOINT
├── txn_id (4B)
├── table_id (4B)
├── payload_size (4B)
└── payload (变长)
    ├── INSERT: rowid + row_data
    ├── UPDATE: rowid + old_columns + new_columns
    ├── DELETE: rowid
    ├── COMMIT: commit_ts
    └── CHECKPOINT: checkpoint_lsn
```

### 7.2 MVCC 实现

**时间戳排序（Timestamp Ordering）：**
- 每个事务分配唯一的 start_ts 和 commit_ts
- 读取：只读取 commit_ts < start_ts 的版本（快照隔离）
- 写入冲突检测：写写冲突时，后到的事务回滚

**版本存储：**
- 当前版本在 Delta Store / Column Store 中
- 历史版本通过 WAL 回溯（MVP 阶段）
- 远期可考虑在 Delta Store 中保留多版本

### 7.3 事务隔离级别

- **MVP**：快照隔离（Snapshot Isolation）
- **后续**：可序列化（Serializable，通过乐观并发控制）

### 7.4 Checkpoint

Checkpoint 将 WAL 中的变更合并到主文件：
1. 累积到一定阈值（默认 1000 页或 16MB WAL）
2. 或手动触发 `CHECKPOINT`
3. 过程：读取 WAL → 应用到 Delta Store → 超过阈值的 Delta 合并到 Column Store
4. Checkpoint 完成后更新文件头的 checkpoint_lsn

---

## 8. 查询引擎

### 8.1 SQL 子集（MVP）

**DDL：**
- `CREATE TABLE`（基本类型、主键）
- `DROP TABLE`

**DML：**
- `INSERT INTO ... VALUES`
- `SELECT ... FROM ... WHERE ...`
- `UPDATE ... SET ... WHERE ...`
- `DELETE FROM ... WHERE ...`

**聚合：**
- `COUNT`, `SUM`, `AVG`, `MIN`, `MAX`
- `GROUP BY`

**事务：**
- `BEGIN`, `COMMIT`, `ROLLBACK`

### 8.2 执行模型

向量化执行引擎：
- 算子：TableScan, Filter, Projection, HashAggregate, Insert, Update, Delete
- 每个算子输入输出 DataChunk（2048 行）
- 流水线执行（Pipeline Breaker：Aggregate, Sort, HashJoin）

### 8.3 优化器

- 规则优化器（RBO）：谓词下推、投影下推、常量折叠
- MVP 阶段不做 CBO（基于成本的优化器）

---

## 9. 模块划分

```
hybriddb/
├── src/
│   ├── main.rs              # CLI 入口
│   ├── lib.rs               # 库入口
│   ├── common/              # 通用工具
│   │   ├── mod.rs
│   │   ├── types.rs         # 数据类型定义
│   │   ├── error.rs         # 错误类型
│   │   └── config.rs        # 配置
│   ├── storage/             # 存储引擎
│   │   ├── mod.rs
│   │   ├── file_format.rs   # 文件格式定义
│   │   ├── buffer_pool.rs   # 缓冲池 (LRU)
│   │   ├── column_store.rs  # 列存主存储 (+ MinMax 跳过索引)
│   │   ├── delta_store.rs   # Delta 行存层
│   │   ├── sparse_index.rs  # 稀疏主索引 (ClickHouse 风格)
│   │   ├── vector_index.rs  # HNSW 向量检索引擎
│   │   ├── compression/     # 轻量级压缩算法
│   │   │   ├── mod.rs
│   │   │   ├── rle.rs       # 游程编码
│   │   │   ├── bitpacking.rs # 位压缩
│   │   │   ├── dictionary.rs # 字典编码
│   │   │   └── for.rs       # Frame of Reference
│   │   └── table.rs         # 表抽象
│   ├── wal/                 # WAL 日志
│   │   ├── mod.rs
│   │   ├── writer.rs
│   │   ├── reader.rs
│   │   └── recovery.rs      # 崩溃恢复
│   ├── txn/                 # 事务管理
│   │   ├── mod.rs
│   │   ├── transaction.rs
│   │   ├── mvcc.rs
│   │   └── savepoint.rs
│   ├── sql/                 # SQL 解析与规划
│   │   ├── mod.rs
│   │   ├── parser.rs
│   │   ├── ast.rs
│   │   ├── planner.rs
│   │   └── optimizer.rs     # RBO (+ 谓词下推 / PREWHERE)
│   └── executor/            # 向量化执行引擎
│       ├── mod.rs
│       ├── executor.rs
│       ├── physical_plan.rs
│       ├── vector.rs        # DataChunk + Vector + SelectionVector + LazyDataChunk
│       ├── operators/
│       │   ├── mod.rs
│       │   ├── table_scan.rs  # 表扫描 (+ MinMax 跳过 + PREWHERE)
│       │   ├── filter.rs      # 过滤 (+ SelectionVector 零拷贝)
│       │   ├── projection.rs  # 投影
│       │   ├── aggregate.rs   # 聚合 (+ Partial + Merge 两阶段)
│       │   └── insert.rs      # 插入
│       └── expression.rs
├── tests/                   # 集成测试
├── benches/                 # 基准测试
├── examples/                # 示例
├── docs/                    # 文档
└── Cargo.toml
```

---

## 10. MVP 范围与里程碑

### 10.1 MVP 目标

**可运行的 demo，能：**
- ✅ 创建/打开单文件数据库
- ✅ 建表（基本类型：INT, BIGINT, DOUBLE, VARCHAR, BOOL）
- ✅ 插入数据（批量插入）
- ✅ 简单查询（SELECT + WHERE + 基本聚合）
- ✅ 事务（BEGIN/COMMIT/ROLLBACK）
- ✅ 基础压缩（RLE + Bit-packing + Dictionary）
- ✅ 崩溃恢复（WAL 重放）
- ✅ MinMax 数据跳过索引（ClickHouse 借鉴）
- ✅ 稀疏主索引（ClickHouse 借鉴）
- ✅ SelectionVector 零拷贝过滤（ClickHouse 借鉴）
- ✅ 懒物化 / PREWHERE 优化框架（ClickHouse 借鉴）
- ✅ 两阶段聚合 Partial+Merge（ClickHouse 借鉴）

### 10.2 MVP 不包含

- ❌ UPDATE / DELETE（仅 INSERT + SELECT）
- ❌ 索引（全表扫描）
- ❌ JOIN
- ❌ 并行查询
- ❌ 二级索引
- ❌ Zstd 通用压缩
- ❌ 复杂 SQL 特性（子查询、窗口函数等）

### 10.3 开发里程碑

| 阶段 | 内容 | 预计工作量 |
|------|------|-----------|
| M1 | 基础框架 + 文件 I/O + 缓冲池 | 1 周 |
| M2 | 列存格式 + 压缩算法 + 元数据 | 2 周 |
| M3 | WAL + 事务 + 崩溃恢复 | 2 周 |
| M4 | Delta 层 + Compaction | 1 周 |
| M5 | SQL Parser + Planner | 2 周 |
| M6 | 向量化执行引擎 | 2 周 |
| M7 | 集成测试 + 基准测试 | 1 周 |
| **合计** | | **~11 周** |

---

## 11. 性能基准测试方案

### 11.1 测试维度

| 测试项 | 说明 |
|--------|------|
| 写入性能 | 批量插入速度（行/秒） |
| 点查性能 | 按主键查询速度 |
| 分析性能 | TPC-H 风格查询（扫描 + 聚合） |
| 压缩率 | 相同数据的磁盘占用对比 |
| 事务开销 | 单条事务写入的延迟 |
| 崩溃恢复 | 异常断电后恢复时间 |

### 11.2 对比对象

- **SQLite 3.x**：行存基准、事务基准
- **DuckDB 1.x**：列存基准、分析性能基准

### 11.3 测试数据集

- **小数据集**：100 万行，10 列（混合类型）
- **中数据集**：1000 万行，10 列
- 数据集设计：包含低基数字符串、递增整数、随机浮点等典型列

---

## 12. WAL + MVCC 事务系统（v0.7 新增）

### 12.1 架构总览

完整的 ACID 事务能力由 **WAL（预写日志）** + **MVCC（多版本并发控制）** 两层协作实现：

```
┌─────────────────────────────────────────────────┐
│              TransactionManager                  │
│  (全局协调：txn_id 分配 / WAL / MVCC / Checkpoint) │
├─────────────────────┬───────────────────────────┤
│      WAL 层          │       MVCC 层             │
│  - 持久化 (D)        │  - 快照隔离 (I)           │
│  - 崩溃恢复          │  - 写-写冲突检测          │
│  - 原子回滚 (A)      │  - 多版本链               │
└─────────────────────┴───────────────────────────┘
```

### 12.2 WAL 模块

#### 12.2.1 记录格式

每条 WAL 记录 19 字节头部 + 可变负载 + CRC32 校验：

| 字段 | 长度 | 说明 |
|------|------|------|
| magic | 2 bytes | `0x5741` ("WA")，用于扫描时定位记录边界 |
| record_type | 1 byte | Begin / Insert / Update / Delete / Commit / Rollback / Checkpoint / Compensation |
| txn_id | 4 bytes | 事务 ID |
| table_id | 4 bytes | 表 ID |
| payload_len | 4 bytes | 负载长度 |
| payload | N bytes | 操作数据 |
| crc32 | 4 bytes | 整记录 CRC32 校验（magic 到 payload 末尾） |

#### 12.2.2 关键特性

- **CRC32 校验**：软件实现，零依赖；篡改数据可被检测
- **缓冲批写**：64KB 写缓冲，减少 syscall；`sync()` 时 fsync 刷盘
- **LSN 管理**：LSN = 文件偏移 + 缓冲偏移，隐式计算不存储
- **容错读取**：magic 不匹配或 CRC 失败时，跳过 1 字节向前扫描，优雅处理部分写入
- **ARIES 风格恢复**：三阶段恢复算法
  - **Analysis Pass**：扫描 WAL，构建事务状态表（active / committed / aborted）
  - **Redo Pass**：重做所有已提交事务的操作
  - **Undo Pass**：回滚所有未提交事务，生成 CLR（Compensation Log Record）

### 12.3 MVCC 模块

#### 12.3.1 版本链结构

每个 key 维护一条版本链，按时间从旧到新排列：

```rust
struct VersionNode<T> {
    value: T,
    begin_ts: Timestamp,  // 版本开始时间戳（提交时更新为 commit_ts）
    end_ts: Option<Timestamp>,  // 版本结束时间戳（None = 最新）
    txn_id: TxnId,        // 创建该版本的事务
    committed: bool,      // 是否已提交（区分未提交版本与最新已提交版本）
}
```

**`committed` 字段的关键作用**：最新已提交版本和未提交版本都有 `end_ts = None`，必须通过 `committed` 标志区分，否则：
- 写冲突检测会误判已提交最新版本为"未提交的其他事务版本"
- 快照读会看到其他事务的未提交写入（脏读）

#### 12.3.2 核心操作

| 操作 | 行为 |
|------|------|
| **write** | 追加新版本（committed=false）；检测写-写冲突 |
| **commit_txn** | 标记 committed=true；设置前一版本 end_ts=commit_ts；更新 begin_ts=commit_ts |
| **rollback_txn** | 移除本事务的所有未提交版本 |
| **get(read_ts)** | 从新到旧找第一个 `committed && begin_ts <= read_ts && (end_ts=None \|\| end_ts > read_ts)` 的版本 |
| **get_for_txn** | 先找自己的未提交写入，再按快照读已提交版本 |
| **gc(oldest_ts)** | 清理 end_ts < oldest_ts 的已提交版本；保留所有未提交版本 |

#### 12.3.3 写-写冲突检测

First-committer-wins 策略：

```
写入时从链尾向前扫描：
  - 遇到未提交版本且属于其他事务 → 冲突，返回 false
  - 遇到已提交版本 → 无冲突，可以写入
  - 遇到自己的未提交版本 → 继续（同一事务可多次写同一 key）
```

#### 12.3.4 快照隔离

`Snapshot` 结构定义事务可见性：

```rust
struct Snapshot {
    snapshot_ts: Timestamp,     // 快照时间戳
    txn_id: TxnId,              // 本事务 ID
    active_txns: HashSet<Timestamp>,  // 活跃事务 start_ts 集合
}
```

可见性规则：
1. 自己的未提交版本 → 可见
2. 其他事务的未提交版本 → 不可见
3. 已提交版本：`begin_ts <= snapshot_ts` 且不在活跃事务中 → 可见

### 12.4 事务管理器

`TransactionManager` 全局协调 WAL + MVCC：

#### 12.4.1 提交流程

```
1. 状态检查（必须 Active）
2. 写 WAL Commit 记录 + fsync  ← 持久性保证
3. 分配 commit_ts
4. MVCC: 提交所有写入版本（标记 committed + 设置前版本 end_ts）
5. 活跃事务表移除
```

#### 12.4.2 回滚流程

```
1. 状态检查（必须 Active）
2. 写 WAL Rollback 记录 + fsync
3. MVCC: 移除所有未提交版本
4. 活跃事务表移除
```

#### 12.4.3 写入路径（Insert / Update / Delete）

```
1. 确保事务 Active
2. 写 WAL 记录（含旧值用于回滚）
3. MVCC 写入（committed=false），检测写-写冲突
4. 记录到 write_set
```

### 12.5 Checkpoint 与 GC

- **Checkpoint**：写 Checkpoint 记录 + fsync + 截断 WAL（保留 Checkpoint 之后部分）
- **GC**：基于最老活跃事务时间戳，清理已过期的历史版本

### 12.6 ACID 保证总结

| 属性 | 实现机制 |
|------|----------|
| **A 原子性** | WAL + Undo：未提交事务回滚时移除所有未提交版本 |
| **C 一致性** | 约束检查 + 事务正确执行（MVCC 确保状态转换正确） |
| **I 隔离性** | MVCC 快照隔离：写不阻塞读，读不阻塞写；写-写冲突 first-committer-wins |
| **D 持久性** | WAL fsync：Commit 记录刷盘后才标记事务提交 |

### 12.7 测试覆盖

12 组测试，41 个断言，全部通过：

1. WAL 基本功能（LSN、记录数、类型、payload）
2. WAL CRC 校验（正常 + 篡改检测）
3. MVCC 多版本读写（旧快照/新快照/版本数）
4. MVCC 事务回滚
5. MVCC 写-写冲突检测
6. 事务管理器 - 提交
7. 事务管理器 - 回滚
8. 快照隔离（Snapshot Isolation）
9. 崩溃恢复 - 已提交事务重做
10. 崩溃恢复 - 未提交事务回滚
11. WAL 持久化
12. 原子性 - 部分写入回滚

---

## 13. 风险与挑战

### 12.1 技术风险

| 风险 | 概率 | 影响 | 缓解措施 |
|------|------|------|----------|
| Delta 合并性能瓶颈 | 中 | 高 | 增量合并、后台线程、分层 Delta |
| 事务正确性（ACID 验证） | 中 | 极高 | 严格测试、故障注入测试、TPC-C 子集 |
| 压缩/解压 CPU 开销 | 低 | 中 | 轻量级压缩优先、自适应选择 |
| 查询优化器效果差 | 中 | 中 | MVP 用 RBO，后续迭代 CBO |
| Rust 编译速度慢 | 高 | 低 | 增量编译、合理模块划分 |

### 12.2 可行性结论

**技术上完全可行**，理由：
1. 核心组件（列存、WAL、MVCC、向量化执行）都是成熟技术
2. SQLite 和 DuckDB 已验证了单文件嵌入式数据库的可行性
3. 混合存储架构在工业界有先例（ClickHouse MergeTree、Snowflake Micro-partition + Delta）
4. Rust 生态已有多个数据库项目可参考

**主要挑战在工程实现**，而非理论可行性。MVP 阶段聚焦核心路径，避免功能膨胀。

---

## 14. 参考资料

1. SQLite File Format Specification - https://www.sqlite.org/fileformat.html
2. DuckDB Storage Format - https://duckdb.org/docs/internals/storage
3. DuckDB Internals Slides - https://blobs.duckdb.org/slides/TaDa-04.pdf
4. ORC File Format - Apache Software Foundation
5. LSM-tree: The Log-Structured Merge-Tree (O'Neil et al., 1996)
6. Vectorized Execution: MonetDB/X100 (Boncz et al., 2005)
7. Morsel-Driven Parallelism (Leis et al., 2014)
8. ClickHouse MergeTree 引擎 - https://clickhouse.com/docs/en/engines/table-engines/mergetree-family/mergetree
9. ClickHouse 数据跳过索引 - https://clickhouse.com/docs/en/engines/table-engines/mergetree-family/mergetree#data-skipping-indexes
10. ClickHouse PREWHERE 优化 - https://clickhouse.com/docs/en/sql-reference/statements/select/prewhere
11. ClickHouse 向量化查询执行 - https://clickhouse.com/docs/en/development/architecture#vectorized-query-execution
