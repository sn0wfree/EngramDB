# Changelog

本文件记录 EngramDB 的版本变更历史。
版本号遵循 [语义化版本](https://semver.org/lang/zh-CN/) 规范。

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
