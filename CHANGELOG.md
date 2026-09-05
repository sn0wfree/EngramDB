# Changelog

本文件记录 EngramDB 的版本变更历史。
版本号遵循 [语义化版本](https://semver.org/lang/zh-CN/) 规范。

## [0.22.1] - 2026-08-30

### 崩溃安全加固（P0 修复）

#### 文件头原子性
- **双副本 + CRC32**：文件头页现在包含两份带 CRC 校验的头部副本（页内偏移 0 与 256），任一份撕裂写入时另一份可校验恢复；两份均损坏时以 `InvalidFormat` 显式报错，不再静默错值
- **fsync 顺序**：`save_data` 数据段先 `sync_data` 落盘，头部再指向它；`save_data` / `save_catalog` / `save_indexes` 三处头部更新统一走 `persist_header`（原子页写 + fsync）
- **向后兼容**：旧格式文件（无 CRC）仍可正常打开

#### OFFSET 支持
- **`LIMIT n OFFSET m`**：SELECT 支持 OFFSET 子句（此前被静默忽略，返回错误行集）
- **纯 OFFSET**：`OFFSET m` 不带 LIMIT 时跳过前 m 行
- **ORDER BY + OFFSET**：Top-N 排序正确覆盖 offset + limit
- **严格解析**：`LIMIT 5+1` 等非字面量不再被静默当作"无限制"，改为报 Parse error

#### 打开路径防损坏
- **截断文件不再 panic**：列存反序列化（`data_from_bytes`）对 min/max 段补齐边界检查，文件尾部截断返回 `InvalidFormat` 而非 slice 越界 panic
- **防超大分配**：损坏的 row group / column count 触发防御性校验，防止打开损坏文件时 OOM

#### 查询计划缓存
- **失效清单补全**：`CREATE VIEW` / `DROP VIEW` 现在会清空计划缓存（此前 DROP VIEW 后可能命中旧计划返回错误结果）

### 修改文件
- `src/storage/file_format.rs`：头部双副本 + CRC + `parse_core` 重构 + 撕裂恢复测试
- `src/storage/mod.rs`：`persist_header` 助手 + fsync 顺序 + 加载路径读完整页
- `src/storage/column_store.rs`：`data_from_bytes` 边界检查
- `src/sql/ast.rs` / `src/sql/parser.rs`：`SelectStmt.offset` + OFFSET/LIMIT 严格解析
- `src/sql/planner.rs` / `src/executor/` / `src/sql/optimizer.rs` / `src/sql/cost_model.rs`：`PhysicalPlan::Limit.offset` 全链路
- `src/lib.rs`：计划缓存失效清单补 CreateView/DropView
- `tests/differential_sqlite.rs`：OFFSET 差分测试用例

## [0.22.0] - 2026-08-25

### SQL 功能完善（高优先级）

#### VIEW 支持
- **CREATE VIEW**：创建普通视图，支持 OR REPLACE 语法
- **DROP VIEW**：删除视图，支持 IF EXISTS 语法
- **视图持久化**：视图定义随数据库持久化
- **视图查询**：支持从视图查询数据

#### CHECK 约束
- **列级 CHECK**：CREATE TABLE 时定义 CHECK 约束
- **ALTER TABLE CHECK**：ALTER TABLE ADD COLUMN 时定义 CHECK 约束
- **约束验证**：INSERT 时自动验证 CHECK 约束
- **支持表达式**：`>`, `<`, `>=`, `<=`, `=`, `!=`, `<>`, `IS NULL`, `IS NOT NULL`, `NOT NULL`

#### 标量子查询
- **标量子查询**：SELECT 列中的子查询（返回单值）
- **EXISTS 子查询**：WHERE 中的 EXISTS 条件
- **IN 子查询**：WHERE 中的 IN (SELECT ...) 条件
- **运行时求值**：子查询在执行时动态求值

#### 递归 CTE
- **WITH RECURSIVE**：支持递归公用表表达式
- **UNION ALL**：锚点 + 递归部分的 UNION ALL 结构
- **迭代执行**：自动迭代直到结果集不再变化
- **最大迭代次数**：防止无限递归（默认 1000 次）

#### ALTER TABLE 增强
- **Parser 支持**：ALTER TABLE 语句现在可以通过 SQL 执行
- **ADD COLUMN**：添加列
- **DROP COLUMN**：删除列
- **RENAME COLUMN**：重命名列
- **RENAME TABLE**：重命名表

### 修改文件
- `src/sql/ast.rs`：新增 CreateViewStmt, DropViewStmt, CHECK 约束字段, 递归 CTE 字段
- `src/sql/parser.rs`：新增 VIEW/CHECK/ALTER TABLE 解析，递归 CTE 解析
- `src/sql/planner.rs`：新增 VIEW/递归 CTE 规划，子查询处理
- `src/executor/physical_plan.rs`：新增 CreateView, DropView, RecursiveCte 物理计划节点
- `src/executor/executor.rs`：新增 VIEW/递归 CTE 执行逻辑
- `src/executor/expression.rs`：新增标量子查询/EXISTS/IN 子查询求值
- `src/executor/operators/insert.rs`：新增 CHECK 约束验证
- `src/common/types.rs`：新增 CHECK 约束字段
- `src/storage/mod.rs`：新增 ViewDef 结构和视图管理方法

## [0.21.x] - 2026-08-xx

### TokenDelta 检索引擎
- **统一 Tokenizer**：NFKC 归一化，词表文件自定义格式，运行时编码器
- **TokenDelta 压缩**：Token 级前缀 delta + 熵编码（Varint/Static/Huffman 三形态）
- **TokenInvertedIndex**：Token 级倒排索引（row, tf 行级 postings）
- **BM25 排序检索**：基于 TokenInvertedIndex 的 BM25 打分
- **模糊匹配**：编辑距离 + n-gram 两种模式
- **RRF 混合检索**：sparse (BM25) + dense (HNSW) 混合排序
- **zstd 压缩**：Varchar 列块级字典压缩（level 3）
- **Rayon 并行**：checkpoint tokenize 行级并行
- **FTS 索引持久化**：倒排索引随表落盘（v0.21.2 zstd 压缩格式）
- **token 流缓存**：行级 token 流缓存，TD 压缩与 FTS 共享

### 性能优化
- **v0.21.2**：TINV2 紧凑格式（delta varint + tf u8 流）
- **v0.21.2**：zstd 先行调度（达标块直选 zstd，省 TD 编码）
- **v0.21.2**：插入并行化（postings 分片 push）

## [0.20.0] - 2026-08-xx

### 约束表攒批
- **约束表入批预检**：主键/唯一索引/NOT NULL 表也可攒批，入批时即校验
- **批内自重复检测**：O(1) seen-set 判重（主键 + 唯一索引）
- **已提交状态点查**：入批前校验已提交数据的主键/唯一索引冲突
- **rowid 基准对齐**：compact 后 rowid 与表行数脱节修复

### Bug 修复
- **唯一索引批量路径**：修复批量 INSERT 静默吞掉唯一索引冲突的 bug
- **事务 buffer 批内约束**：修复事务内连续 INSERT 的约束检查

## [0.19.0] - 2026-08-xx

### 分层索引
- **Delta 稠密索引**：Delta 层主键 BTreeMap 索引
- **列存稀疏索引**：列存层 ClickHouse 风格稀疏主索引
- **分层查询**：Delta 稠密 + 列存稀疏联合查询
- **主键索引重建**：v0.19 之前文件无稀疏索引段时自动重建

## [0.18.0] - 2026-08-xx

### 查询计划缓存
- **计划缓存**：相同 SQL 跳过 parse/plan/optimize（P0-1）
- **Prepared 直通路径**：裸 INSERT 免计划结构，直接求值绑定行
- **DDL 清缓存**：CREATE/ALTER/DROP 后自动清空缓存

### INSERT 攒批合并
- **Ingest Buffer / Batcher**：autocommit 逐行 INSERT 合批落盘（P0-2）
- **事务级 Batcher**：显式事务内 INSERT 攒批，COMMIT/读时 flush
- **攒批阈值**：行数/字节/时间三维度触发

### LogEngine 优化
- **冻结块释放**：块满后释放写入缓冲，内存减半（P1-4）
- **可配置块行数**：`log_block_rows` 参数（P1-5）
- **列式直写**：跳过「列→行→列」双重转置

### 其他
- **jemalloc 全局分配器**：小对象分配优化
- **事务内 ROLLBACK 修复**：ROLLBACK 可撤销事务内操作

## [0.17.0] - 2026-08-xx

### 多引擎架构
- **StorageEngine trait**：统一引擎接口契约
- **EngineTable 枚举**：运行时持有与分派（Columnar/Memory/Log）
- **ENGINE = xxx 子句**：CREATE TABLE 指定存储引擎
- **MemoryEngine**：全内存表，Agent session 缓存，不持久化
- **LogEngine**：追加式时间序列引擎，块级 MinMax 跳读
- **Auto 引擎**：根据 heat 自动分层到 Columnar/Memory/Log

### 分层迁移
- **HeatTracker**：每表访问热度追踪
- **tier_migration**：基于 heat 决策的自动迁移
- **migration**：引擎间数据搬迁

### 索引增强
- **主键 Mark Index**：主键索引持久化（M1-7）
- **Bloom Filter**：列级等值跳读（M1-8）
- **WAL engine_type**：WAL 记录头增加引擎类型字段（20 字节）

### 兼容性
- **旧格式兼容**：WAL 双解析兼容 19 字节旧格式
- **向后兼容**：旧文件无 engine 字段时默认 Columnar

## [0.16.0] - 2026-08-xx

### 类型转换
- **Varchar → Vector/VectorInt8**：类型强转支持

## [0.15.0] - 2026-08-xx

### 新增数据类型
- **VectorInt8**：INT8 量化向量，存储减 75%，精度损失 1-5%
- **TTL**：表级时间戳自动过期

### SQL 增强
- **TRUNCATE TABLE**：清空表数据
- **SAVEPOINT / RELEASE / ROLLBACK TO SAVEPOINT**：嵌套事务
- **PRAGMA**：table_info 等元数据查询
- **INSERT OR IGNORE / INSERT OR REPLACE**：冲突处理
- **INSERT ... SELECT**：从查询结果插入
- **CTAS**：CREATE TABLE AS SELECT
- **UNION / UNION ALL / INTERSECT / EXCEPT**：集合操作
- **HAVING**：聚合后过滤
- **表值函数**：vector_search(...) 表值函数
- **CREATE VECTOR INDEX ... WITH (...)**：向量索引参数化创建

### JSON 函数
- **JSON_OBJECT**：构造 JSON 对象
- **JSON_ARRAY**：构造 JSON 数组
- **JSON_SET**：设置/创建路径的值
- **JSON_INSERT**：仅当路径不存在时设置
- **JSON_REPLACE**：仅当路径存在时替换
- **JSON_REMOVE**：删除指定路径的字段

### 字符函数
- **TRIM / LTRIM / RTRIM**：字符串修剪
- **CEIL / FLOOR**：取整函数

### 向量增强
- **搜索 trace**：向量搜索返回访问路径、入口点、候选节点数（V13）
- **向量索引参数化**：CREATE VECTOR INDEX ... WITH (m, ef_construction)

### 事务增强
- **只读事务**：跳过 WAL 写入，避免不必要 fsync（Txn09）

### 存储增强
- **KV 缓存引擎**：嵌式 KV 缓存（v0.21.0 重构为 O(1) LRU）
- **限流器**：滑动窗口限流
- **文档摄入 API**：ingestion 模块

## [0.14.0] - 2026-08-04

### 新增功能
- **FLOAT32 类型**：DataType::Float32 + Value::Float32(f32)，完整读写路径（列存/SkipList/WAL/压缩）
- **TIMESTAMP 类型**：DataType::Timestamp + Value::Timestamp(i64)，支持 Timestamp/Datetime 关键字
- **AUTO_INCREMENT 自增**：列级 AUTO_INCREMENT/AUTOINCREMENT，INSERT 时自动分配递增 ID
- **列级 UNIQUE 约束**：`col_name TYPE UNIQUE` 语法，自动创建唯一索引
- **INSERT...RETURNING**：INSERT 后返回插入行的值，支持单列/多列/通配符
- **UPSERT**：INSERT...ON CONFLICT DO UPDATE/NOTHING，冲突时更新或跳过

## [0.13.0] - 2026-08-04

### 性能优化
- **A-1 事务写入**：通过 WAL 组提交 + 批量 INSERT 优化，事务写入 2.63x vs SQLite（目标 ≤10x）
- **A-2 索引点查**：BTreeMap 主键索引 + PrimaryKeyLookup 短路计划节点
- **A-3 COUNT(*)**：行数元数据缓存，O(1) 返回结果
- **Top-N 排序**：BinaryHeap 堆排序优化，ORDER BY + LIMIT 避免全排序
- **主键索引持久化**：重启后自动重建 BTreeMap 主键索引

### Bug 修复
- **COUNT(DISTINCT)**：修复 DISTINCT 静默被丢弃的 bug，使用 HashSet 去重
- **Unique 索引冲突检测**：CREATE UNIQUE INDEX 时检测重复键并报错
- **NOT NULL 约束**：INSERT/UPDATE 时检查非空列

### 新增功能
- **ALTER TABLE**：支持 ADD COLUMN 操作
- **PRAGMA**：支持 table_info 等查询
- **Prepared Statement**：计划缓存支持
- **SELECT DISTINCT**：去重查询
- **BLOB 类型**：二进制数据存储
- **外键框架**：ForeignKeyDef 定义 + 级联操作类型
- **JSON 操作符**：-> 语法（转换为 JSON_EXTRACT 函数）
- **复合索引**：多列键索引（Varchar 拼接编码）
- **索引类型**：IndexDef 新增 index_type 字段

### 函数新增
- IFNULL(expr, default) — 别名指向 COALESCE
- REPLACE(str, from, to) — 字符串替换
- MOD(a, b) — 整数取模
- TINYINT 类型别名

### 体验改进
- information_schema 基础（通过 PRAGMA）
- 查询计划缓存提升重复查询性能

## [0.12.0] - 2026-08-02

### 新增
- **JSON 类型**：`Value::Json` 支持 JSON 文本存储与路径查询
  - `JSON_EXTRACT(json, path)` 函数：按 JSONPath 提取值
  - `JSON_CONTAINS(json, value, path)` 函数：判断数组是否包含元素
- **Vector 类型**：`Value::Vector(Vec<f32>)` 支持浮点向量
  - `VECTOR_DISTANCE(v1, v2, metric)` 函数：L2 / 内积 / 余弦距离
  - 与 HNSW 索引配合实现近似最近邻搜索
- **索引持久化**：二级索引与向量索引可序列化到数据文件
  - `Database::save_indexes()` / `load_indexes()`
  - 文件头新增 `index_root` / `index_size` 字段
- **覆盖索引**：`create_index` 支持 `included_cols` 覆盖列
- **serde_json 依赖**：用于 JSON 解析

### 适用场景
- AI Agent 元数据存储（工具参数、调用结果、状态）
- 语义记忆（embedding 向量 + HNSW 检索）
- RAG 检索（向量相似度 + 关系过滤）

## [0.11.x] - 2026-08-01

### v0.11.4 - WAL 组提交
- **WAL 组提交**：Sync 模式下多条事务共享一次 fsync
  - `set_wal_group_commit_size(size)` API
  - 推荐范围 8~32，吞吐提升 5-20x
  - 崩溃时最多丢 `size` 条未 fsync 事务
- **sync_wal_compact 联动**：Periodic 刷盘后自动触发 Delta 合并

### v0.11.2 - 零拷贝列式导入
- **import_columns**：跳过 SQL 层直接列式写入
  - 大批量（≥1000 行且 ≥ row_group_size/4）直接写列存
  - 小批量走列式 Delta（P4 优化）
- **Prepared Statement 批量执行**：`execute_prepared_batch`
- **Delta 聚簇**：`set_cluster_key` 按 session_id 聚簇 AI Agent 交互数据

### v0.11.0 - Compact 策略
- **四种合并策略**：
  - `Manual`：完全手动 compact()
  - `Full(threshold)`：全量合并
  - `Incremental(threshold, batch_size)`：增量分批
  - `default_adaptive(row_group_size)`：自适应分桶（默认）
- **DataFusion 集成**：`datafusion_ext` TableProvider
- **Arrow 互操作**：`arrow_integration` 格式转换
- **物化视图框架**：`materialized_view` 预聚合

## [0.10.x] - 2026-08-01

### 查询优化器
- **RBO 规则优化**：谓词下推、列裁剪、常量折叠、Join 重排
- **CBO 成本优化**：基于统计信息选择最优计划
- **成本模型**：`cost_model.rs` CPU/IO/内存三维估算
- **统计信息**：`statistics.rs` 列基数、直方图、NULL 比例
- **Join 顺序优化**：`join_order.rs` 贪心 + 动态规划

## [0.9.x] - 2026-08-01

### Join 与高级查询
- **HashJoin 算子**：`executor/operators/hash_join.rs`
- **子查询支持**（部分）
- **Set 操作**（UNION/INTERSECT/EXCEPT，部分）

## [0.8.x] - 2026-08-01

### 基础 SQL 完善
- **UDF 框架**：`sql/udf.rs` 运行时动态注册函数
  - 标量 UDF 向量化批量执行
  - 类型安全的参数与返回值声明
- **表达式系统增强**
- **排序分页**：ORDER BY + LIMIT/OFFSET
- **聚合增强**：COUNT/SUM/AVG/MIN/MAX + GROUP BY

## [0.7.6] - 2026-08-01

### 三引擎性能对比
- 新增 `compare_bench.py`：EngramDB vs SQLite vs DuckDB
- 覆盖数据导入、索引构建、COUNT/SUM/AVG/点查/范围扫描/GROUP BY 共 8 项
- 10 万行 × 5 列数据集，附文件大小对比与选型指南

## [0.7.5] - 2026-08-01

### 极限性能基准
- 新增 `limit_bench.rs`，五大极限场景：
  - 压缩算法极限 / Delta 扩展性 / Gorilla 浮点 / Boolean 千万级 / 索引极限
- 关键数据：Delta ~1000 M rows/s、BooleanPack ~1.3 B rows/s、RLE 66 万倍压缩、布隆 ~57M QPS

## [0.7.4] - 2026-08-01

### 全面性能基准
- `compression_bench_v2.rs`：7 算法 × 5 类型 × 13 分布
- `index_bench.rs`：跳表/位图/布隆 3 类索引对比
- `vector_bench.rs` / `write_bench.rs` / `compact_strategy_bench.rs`

## [0.7.3] - 2026-08-01

### 分字段类型压缩
- **Boolean 列位打包**：8x 压缩
- **整数列 Delta+FOR+Bit-pack+RLE**：多策略择优
- **Float64 列 Gorilla XOR**：Facebook Gorilla 论文算法
- 新增 CompressionType：Gorilla / ForBitPack / BooleanPack

## [0.7.2] - 2026-08-01

### 多维度索引体系
- **跳表二级索引**：O(log n) 范围查询
- **位图索引**：低基数列 + AND/OR 位运算
- **布隆过滤器**：存在性快速判断
- 项目定位升级为「专用分析型嵌入 AI Agent 数据引擎」

## [0.7.1] - 2026-08-01

### 测试扩充
- 单元测试从 40 个扩充到 236 个
- 覆盖 WAL / MVCC / 事务 / 压缩 / 向量索引 / 稀疏索引 / 数据类型

## [0.7.0] - 2026-08-01

### 完整 ACID 事务
- **WAL 预写日志**：Sync / Async / Periodic 三种刷盘模式
- **MVCC 多版本并发**：快照隔离、版本链
- **写写冲突检测**
- **ARIES 崩溃恢复**：Analysis / Redo / Undo 三阶段

## [0.6.0] - 2026-08-01

### HNSW 向量检索
- **HNSW 索引**：多层图结构，O(log n) 近似最近邻
- **三种距离度量**：L2 / 内积 / 余弦相似度
- 参数可调：M / ef_construction / ef_search

## [0.5.0] - 2026-08-01

### 轻量级压缩
- **RLE**：Run-Length Encoding，连续重复值
- **Dictionary**：字典编码，低基数字符串
- **Bit-packing**：整数按位宽打包
- **FOR**：Frame of Reference，偏移后 Bit-pack

## [0.4.0] - 2026-08-01

### 稀疏主索引
- ClickHouse 风格稀疏主索引（每个 Row Group 一个条目）
- 范围查询裁剪（range pruning）
- 点查定位

## [0.3.0] - 2026-08-01

### 混合存储架构
- **列存主存储**：Row Group 分组，按列存储
- **行存 Delta 层**：吸收随机写入
- **Compaction**：Delta 达阈值后合并到列存

## [0.2.0] - 2026-07-31

### SQL + 执行引擎 MVP
- **SQL 解析器**：基于 sqlparser-rs
- **查询规划器**：AST → 物理计划
- **向量化执行**：DataChunk（1024 行/chunk）
- **算子**：TableScan / Filter / Projection / Insert / Aggregate / Sort

## [0.1.0] - 2026-07-31

### 项目初始化
- Cargo.toml 项目元信息与依赖声明
- 单文件格式设计（文件头、页布局、魔数）
- 缓冲池（LRU 页缓存）
- 通用模块：types / error / config / memory_pool
- Connection / Value / QueryResult 公共 API
- CLI 入口（交互模式 + 单条命令模式）
