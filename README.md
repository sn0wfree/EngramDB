# HybridDB

> 专用分析型嵌入 AI Agent 数据引擎
> **当前版本：v0.7.6 — 三引擎性能对比（SQLite vs DuckDB vs HybridDB）**

兼具 **SQLite 的事务能力（ACID）** 与 **DuckDB 的列存压缩与分析性能**，单文件嵌入式部署，面向 AI Agent 工作负载优化。

## 版本历史

- **v0.7.6** (2026-08-01)：三引擎性能对比测试——新增 `compare_bench.py`，HybridDB vs SQLite vs DuckDB 同场竞技，覆盖数据导入、索引构建、COUNT/SUM/AVG/点查/范围扫描/GROUP BY 共 8 项指标 + 文件大小对比；10万行×5列数据集，各引擎使用推荐最佳实践；附场景适配建议与选型指南
- **v0.7.5** (2026-08-01)：极限性能基准测试——新增 `limit_bench.rs`，五大极限场景测出性能边界（压缩算法极限/Delta扩展性曲线/Gorilla浮点极限/Boolean千万级吞吐/索引极限）；关键数据：Delta ~1000 M行/秒、BooleanPack ~1.3 B行/秒、RLE 66万倍压缩、布隆查询 ~57M QPS
- **v0.7.4** (2026-08-01)：全面性能基准测试——新增 `compression_bench_v2.rs`（7 种算法 × 5 种数据类型 × 13 种数据分布，含压缩率+编解码速度）和 `index_bench.rs`（跳表/位图/布隆过滤器 3 类索引构建+查询+内存占用对比）；Rust 原生 -O 编译运行，数据翔实可验证
- **v0.7.3** (2026-08-01)：ClickHouse 风格分字段类型压缩算法——Boolean 列位打包（8× 压缩）、整数列 Delta+FOR+Bit-packing+RLE 多策略择优、Float64 列 Gorilla XOR 编码（Facebook Gorilla 论文）、Varchar 列字典编码；新增 3 种压缩类型（Gorilla/ForBitPack/BooleanPack）；100 个压缩模块测试全部通过
- **v0.7.2** (2026-08-01)：多维度索引体系——跳表二级索引（范围查询）、位图索引（低基数列 + AND/OR 位运算）、布隆过滤器（存在性快速判断）；项目定位升级为「专用分析型嵌入 AI Agent 数据引擎」；修复 11 个编译错误，cargo build 零错误通过
- **v0.7.1** (2026-08-01)：单元测试全面扩充（40→236 个），覆盖 WAL、MVCC、事务管理器、压缩算法、向量索引、稀疏索引、数据类型等所有核心模块
- **v0.7**：WAL + MVCC 完整 ACID 事务能力（快照隔离、写写冲突检测、ARIES 风格崩溃恢复）
- **v0.6**：HNSW 向量检索引擎（L2 / 内积 / 余弦相似度）
- **v0.5**：轻量级压缩算法（RLE / Dictionary / Bit-packing / FOR）
- **v0.4**：ClickHouse 风格稀疏主索引 + 数据跳数索引
- **v0.3**：列存主存储 + 行存 Delta 层混合架构
- **v0.2**：SQL 解析器 + 向量化执行引擎 MVP
- **v0.1**：项目初始化 + 单文件格式设计

## SQL 路线图

完整 SQL 支持分 5 个阶段推进：

- **Phase 1 (v0.8.x)**：基础 SQL 完善 — 表达式系统、排序分页、聚合增强、UPDATE/DELETE、索引 DDL
- **Phase 2 (v0.9.x)**：Join + 高级查询 — HashJoin、子查询、Set 操作、CTE
- **Phase 3 (v0.10.x)**：查询优化器 — RBO 规则集、统计信息 + CBO、索引选择优化
- **Phase 4 (v0.11.x)**：高级特性 — 窗口函数、复杂类型 (ARRAY/JSON)、物化视图、向量 SQL
- **Phase 5 (v0.12.x+)**：生产级完善 — 事务隔离级别、权限安全、兼容性增强

详细规划见 [docs/sql_roadmap.md](docs/sql_roadmap.md)

## 特性

- 📦 **单文件**：整个数据库是一个 `.hdb` 文件，备份/迁移零成本
- ⚡ **列存主存储**：列式存储 + ClickHouse 风格分类型压缩（Delta/Gorilla/FOR/Bit-pack/RLE/Dict），分析查询性能优异
- 🔄 **混合架构**：行存 Delta 层吸收随机写入，定期合并到列存
- 🔒 **完整事务**：WAL + MVCC，支持快照隔离和崩溃恢复
- 🔍 **多维度索引**：稀疏主索引 + 跳表二级索引 + 位图索引 + 布隆过滤器 + HNSW 向量索引
- 🚀 **向量化执行**：基于 DataChunk 的向量化查询引擎
- 🦀 **Rust 实现**：内存安全、零成本抽象、现代工具链

## 架构概览

```
SQL Interface → Query Planner → Vectorized Executor
       ↓
Transaction Manager (MVCC + WAL)
       ↓
Storage Engine: [Delta Store (行存)] → [Column Store (列存)]
       ↓
Buffer Pool → Single File Format (.hdb)
```

## 快速开始

### 构建

```bash
cd hybriddb
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
```

## 支持的 SQL 语法（MVP）

### DDL
```sql
CREATE TABLE table_name (
    column_name TYPE [PRIMARY KEY] [NOT NULL],
    ...
);
```

支持类型：`INT`, `BIGINT`, `DOUBLE`, `VARCHAR`, `BOOLEAN`

### DML
```sql
INSERT INTO table_name VALUES (...), (...);
SELECT * FROM table_name [WHERE condition] [LIMIT n];
SELECT col1, col2 FROM table_name [WHERE condition] [LIMIT n];
```

### 事务
```sql
BEGIN;
COMMIT;
ROLLBACK;
```

## 项目结构

```
hybriddb/
├── src/
│   ├── main.rs          # CLI 入口
│   ├── lib.rs           # 库入口
│   ├── common/          # 通用工具（类型、错误、配置）
│   ├── storage/         # 存储引擎
│   │   ├── file_format.rs    # 文件格式定义
│   │   ├── buffer_pool.rs    # 缓冲池
│   │   ├── column_store.rs   # 列存主存储
│   │   ├── delta_store.rs    # Delta 行存层
│   │   ├── compression/      # 压缩算法（Delta/Gorilla/FOR/Bit-pack/RLE/Dict）
│   │   └── table.rs          # 表抽象
│   ├── wal/             # WAL 日志
│   ├── txn/             # 事务管理（MVCC）
│   ├── sql/             # SQL 解析与规划
│   └── executor/        # 向量化执行引擎
├── tests/               # 集成测试
├── benches/             # 基准测试
├── examples/            # 示例
└── docs/                # 文档
```

## 技术方案

详细技术方案见 [docs/01-technical-design.md](docs/01-technical-design.md)

## 开发路线图

- [x] **M1**: 基础框架 + 文件 I/O + 缓冲池
- [x] **M2**: 列存格式 + 轻量级压缩 (RLE/Dict/Bit-pack/FOR)
- [x] **M3**: WAL + MVCC 事务 + 崩溃恢复（完整 ACID）
- [x] **M4**: Delta 层 + Compaction
- [x] **M5**: ClickHouse 风格性能优化（稀疏索引/跳过索引/预聚合）
- [x] **M6**: 向量检索引擎 (HNSW 索引)
- [x] **M7**: WAL + MVCC 完整 ACID 事务 (v0.7)
- [ ] **后续**: 集成测试 + 基准测试、UPDATE/DELETE、索引、JOIN、并行查询...

## 许可证

MIT
