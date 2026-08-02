# HybridDB SQL 完整支持路线图

> 目标：从当前 MVP 级 SQL 能力，演进为支持标准 SQL 的嵌入式分析型数据库引擎
> 定位：SQLite 兼容度优先 + DuckDB 级分析能力 + HybridDB 专属压缩/索引优化

---

## 一、现状评估

### 已有能力（MVP 级）

| 模块 | 代码量 | 状态 | 说明 |
|------|--------|------|------|
| SQL 解析器 | 438 行 | ⚠️ 基础 | 手写递归下降，仅支持 CREATE TABLE / INSERT / SELECT / 事务 |
| AST | 123 行 | ⚠️ 基础 | 覆盖核心 Statement 和 Expression，缺大量语法节点 |
| 查询优化器 | 13 行 | ❌ 空壳 | 仅有模块骨架，无任何优化规则 |
| 查询规划器 | 138 行 | ⚠️ 基础 | 简单的 SQL → PhysicalPlan 映射，无优化 |
| 执行引擎 | ~800 行 | ✅ 可用 | 向量化执行，含 TableScan/Filter/Projection/Aggregate/Insert |
| 表达式计算 | 24 行 | ❌ 极弱 | 仅支持最基础的字面量和列引用 |
| 元数据/Catalog | — | ❌ 缺失 | 无独立 Catalog 层，表信息直接存存储层 |
| 类型系统 | — | ⚠️ 基础 | 有 DataType 定义，缺类型推导和隐式转换 |

### 核心差距

1. **语法覆盖不足**：缺 UPDATE / DELETE / ALTER / DROP / CREATE INDEX / JOIN / 子查询 / CTE / Window 等
2. **表达式能力弱**：缺函数、算术运算、比较运算、逻辑运算的完整实现
3. **无优化器**：SQL 直接生成物理计划，无 RBO / CBO
4. **执行算子不全**：缺 HashJoin / Sort / Limit / Window / Union 等
5. **无 Catalog 抽象**：元数据管理与存储层耦合
6. **无类型推导**：表达式类型在编译期无法确定

---

## 二、技术选型

### 2.1 解析器方案

| 方案 | 优点 | 缺点 | 推荐 |
|------|------|------|------|
| **sqlparser-rs** | 成熟、完整 SQL 支持、社区活跃 | 依赖较重、AST 节点多 | ⭐ 推荐 |
| nom 手写 | 灵活、完全可控 | 工作量大、易出 bug | 备选 |
| 继续手写 | 零依赖、轻量 | 维护成本高、功能完善慢 | 不推荐 |

**决策：采用 sqlparser-rs**
- 业界标准（DataFusion / RisingWave 均使用或基于其改造）
- 支持 ANSI SQL + 多种方言扩展
- 可通过 feature flag 控制编译体积
- 我们的差异化在执行引擎和存储层，不在解析器

### 2.2 优化器架构

| 方案 | 说明 | 推荐 |
|------|------|------|
| **Volcano/Cascades 风格** | 规则驱动 + 成本模型，完整 CBO | 长期目标 |
| **Heuristic (RBO) 优先** | 先上规则优化，再加统计信息和 CBO | ⭐ 推荐 |
| 直接用 DataFusion 优化器 | 功能完整，但耦合重 | 不推荐 |

**决策：RBO 先行，逐步引入 CBO**
- Phase 2-3 实现 RBO（谓词下推、列裁剪、常量折叠等）
- Phase 4 引入统计信息和基础 CBO

### 2.3 执行引擎

当前已是向量化执行（Vectorized Execution），方向正确。
- 继续完善算子集
- 引入向量化表达式计算（与 Vector 对齐）
- 后期考虑编译执行（JIT）作为可选优化

---

## 三、分阶段路线图

### Phase 1：基础 SQL 完善（v0.8.x，预计 3-4 个小版本）

**目标**：让基础查询真正可用，覆盖 80% 日常 SQL 场景

#### v0.8.0 — 表达式系统完善
- 完整的表达式计算框架（算术/比较/逻辑/位运算）
- 内置函数库（第一期 20+ 个）：
  - 数学：ABS / ROUND / CEIL / FLOOR / MIN / MAX / SUM / AVG / COUNT
  - 字符串：LENGTH / SUBSTR / CONCAT / UPPER / LOWER / TRIM
  - 日期：NOW / DATE_PART / DATE_TRUNC
  - 条件：CASE WHEN / COALESCE / NULLIF
- 类型推导系统（表达式编译期确定返回类型）
- 隐式类型转换规则

#### v0.8.1 — 排序 + 分页 + 完整 WHERE
- ORDER BY 多列排序（ASC/DESC/NULLS FIRST/LAST）
- LIMIT / OFFSET
- WHERE 子句完整支持（AND/OR/NOT/比较/IN/BETWEEN/LIKE/IS NULL）
- 执行器：Sort 算子（外部排序，支持大结果集）
- 执行器：TopN 算子（ORDER BY + LIMIT 优化）

#### v0.8.2 — 聚合增强 + 子查询基础
- GROUP BY 多列 + HAVING
- 聚合函数增强：COUNT(DISTINCT) / GROUP_CONCAT / FIRST_VALUE / LAST_VALUE
- 标量子查询（SELECT 列表 / WHERE 中的子查询）
- IN 子查询（非相关）
- EXISTS 子查询

#### v0.8.3 — DML 完善 + 索引 DDL
- UPDATE 语句（带 WHERE）
- DELETE 语句（带 WHERE）
- CREATE INDEX / DROP INDEX
- DROP TABLE
- TRUNCATE TABLE

**Phase 1 验收标准**：能跑通 TPC-H Q1-Q6 中的简单查询

---

### Phase 2：Join + 高级查询（v0.9.x）

**目标**：支持多表关联查询，达到分析型数据库基础能力

#### v0.9.0 — Hash Join 基础
- 嵌套循环连接（Nested Loop Join）— 小表驱动
- 哈希连接（Hash Join）— 等值连接
- INNER JOIN / LEFT JOIN / RIGHT JOIN / FULL JOIN
- Join 条件推导与类型检查
- 执行器：HashJoin 算子（向量化）

#### v0.9.1 — Join 优化 + Cross Join
- CROSS JOIN
- NATURAL JOIN / USING 语法
- 多表 Join 顺序优化（基础：贪心算法）
- Join 下推优化（谓词下推到 Join 两侧）

#### v0.9.2 — 子查询增强
- 相关子查询（Correlated Subquery）
- 子查询反嵌套（Unnesting）→ 转为 Join
- ANY / ALL / SOME 子查询
- FROM 子查询（派生表）

#### v0.9.3 — Set 操作 + CTE
- UNION / UNION ALL
- INTERSECT / INTERSECT ALL
- EXCEPT / EXCEPT ALL
- 公用表表达式（CTE / WITH 子句）
- 递归 CTE（WITH RECURSIVE）— 可选

**Phase 2 验收标准**：能跑通 TPC-H 全部 22 条查询

---

### Phase 3：查询优化器（v0.10.x）

**目标**：从"能跑"到"跑得快"，引入完整优化器

#### v0.10.0 — RBO 规则集
- 基于规则的优化器（RBO）框架
- 核心优化规则：
  - 谓词下推（Predicate Pushdown）
  - 列裁剪（Column Pruning / Projection Pushdown）
  - 常量折叠（Constant Folding）
  - 过滤器合并（Filter Merge）
  - 投影合并（Projection Merge）
  - 空连接消除（Null-Eliminating Outer Join → Inner）
- 优化规则应用框架（模式匹配 + 重写）

#### v0.10.1 — 统计信息 + 基础 CBO
- 表/列统计信息收集（ANALYZE TABLE）
- 统计信息类型：行数、NDV（唯一值数）、空值率、Min/Max、直方图
- 成本模型（Cost Model）：IO 成本 + CPU 成本
- 基础 CBO：Join 顺序选择（动态规划）
- 基数估算（Cardinality Estimation）

#### v0.10.2 — 索引选择优化
- 索引可用性分析
- 索引选择优化器（Access Path Selection）
- 位图索引优化（Bitmap Index + AND/OR 下推）
- 布隆过滤器下推（Bloom Filter Pushdown）

#### v0.10.3 — 向量化优化 + 表达式编译
- 表达式向量化执行优化
- 过滤条件向量化（SIMD 友好）
- 运行时代码生成（可选，JIT 框架预留）
- 自适应查询执行（Adaptive Query Execution）

**Phase 3 验收标准**：TPC-H 性能较 v0.9 提升 2-5 倍

---

### Phase 4：高级特性（v0.11.x）

**目标**：覆盖高级分析功能，达到 DuckDB 级分析能力

#### v0.11.0 — 窗口函数
- Window 函数框架（分区 + 排序 + 帧）
- 排名函数：ROW_NUMBER / RANK / DENSE_RANK / NTILE
- 分析函数：LAG / LEAD / FIRST_VALUE / LAST_VALUE / NTH_VALUE
- 窗口聚合：SUM / AVG / COUNT / MIN / MAX OVER
- 帧定义：ROWS / RANGE / GROUPS 模式

#### v0.11.1 — 复杂数据类型
- ARRAY 类型 + 数组函数
- JSON 类型 + JSON 函数（JSON_EXTRACT / JSON_VALUE）
- MAP / STRUCT 类型
- 嵌套类型的向量化存储

#### v0.11.2 — 视图 + 物化视图
- CREATE VIEW / DROP VIEW（普通视图，查询时展开）
- CREATE MATERIALIZED VIEW（物化视图，存储结果）
- 物化视图刷新机制（全量 / 增量）
- 查询重写（自动使用物化视图）

#### v0.11.3 — 全文检索 + 向量 SQL
- 全文检索函数（MATCH / AGAINST 风格）
- 向量相似度查询 SQL 语法（<-> 距离操作符）
- 向量索引 HNSW 的 SQL 接口
- Approximate Nearest Neighbor 查询

**Phase 4 验收标准**：支持 TPC-DS 核心查询集

---

### Phase 5：生产级完善（v0.12.x+）

**目标**：生产可用，生态完善

#### v0.12.0 — 事务 + 并发控制完善
- 完整的事务隔离级别支持（READ COMMITTED / SNAPSHOT / SERIALIZABLE）
- 死锁检测
- 锁等待与超时
- Savepoint 支持

#### v0.12.1 — 权限 + 安全
- 用户管理（CREATE USER / DROP USER）
- 权限管理（GRANT / REVOKE）
- 角色（ROLE）
- 行级安全（Row Level Security）

#### v0.12.2 — 兼容性增强
- SQLite 兼容模式（语法 + 函数）
- PostgreSQL 兼容模式（常用语法）
- 数据导入导出（COPY FROM / COPY TO）
- CSV / JSON / Parquet 外部表

---

## 四、架构总览

```
┌─────────────────────────────────────────────────────────────┐
│                      SQL Interface                           │
├─────────────────────────────────────────────────────────────┤
│  Parser (sqlparser-rs)  →  AST  →  Validator (类型检查)     │
├─────────────────────────────────────────────────────────────┤
│  Planner (逻辑计划生成)                                      │
├─────────────────────────────────────────────────────────────┤
│  Optimizer                                                  │
│  ├─ RBO: 谓词下推 / 列裁剪 / 常量折叠 / ...                  │
│  └─ CBO: 统计信息 / 成本模型 / Join 顺序 / 索引选择          │
├─────────────────────────────────────────────────────────────┤
│  Executor (向量化执行)                                       │
│  ├─ Scan: TableScan / IndexScan / BitmapScan                │
│  ├─ Filter / Projection / Sort / TopN / Limit               │
│  ├─ Join: NestedLoop / Hash / Merge (后期)                  │
│  ├─ Aggregate: HashAgg / SortAgg                            │
│  ├─ Window / Union / Subquery                               │
│  └─ Modify: Insert / Update / Delete                        │
├─────────────────────────────────────────────────────────────┤
│  Catalog (元数据管理)                                        │
│  ├─ Table / Column / Index / View 元数据                    │
│  └─ Statistics (统计信息)                                    │
├─────────────────────────────────────────────────────────────┤
│  Transaction Manager (MVCC + WAL)                           │
├─────────────────────────────────────────────────────────────┤
│  Storage Engine (列存 + 分类型压缩 + 多维度索引)              │
└─────────────────────────────────────────────────────────────┘
```

---

## 五、关键设计决策

### 5.1 解析器：sqlparser-rs 集成策略

```
sqlparser-rs AST → 转换层 → HybridDB 内部 AST → Planner
                    ↑
              方言扩展点（HybridDB 特有语法）
```

- 不直接使用 sqlparser-rs 的 AST 贯穿全链路
- 增加一层转换，内部 AST 精简且可控
- 方言扩展通过 sqlparser-rs 的 Dialect trait 实现

### 5.2 优化器：规则框架设计

```rust
trait Rule {
    fn pattern(&self) -> &Pattern;  // 匹配模式
    fn apply(&self, plan: &mut LogicalPlan) -> bool;  // 重写
    fn name(&self) -> &str;
}

struct Optimizer {
    rules: Vec<Box<dyn Rule>>,
    max_passes: usize,  // 迭代直到收敛或达到上限
}
```

### 5.3 执行器：Volcano 模型 + 向量化

```
每个算子实现:
  fn next_batch(&mut self) -> Result<Option<DataChunk>>

DataChunk: 一批行（默认 1024 行），列式存储
  每列是一个 Vector（支持 NULL 位图）
```

### 5.4 Catalog：持久化 + 内存缓存

- Catalog 表存储在 .hdb 文件的系统表空间
- 启动时加载到内存
- DDL 操作同时更新内存和持久化
- 表统计信息也存在 Catalog 中

---

## 六、优先级与里程碑

| 阶段 | 版本 | 核心价值 | 预计工作量 |
|------|------|----------|------------|
| Phase 1 | v0.8.x | 基础查询可用，覆盖日常 80% 场景 | 高 |
| Phase 2 | v0.9.x | 多表分析能力，可跑 TPC-H | 很高 |
| Phase 3 | v0.10.x | 性能飞跃，优化器驱动 | 很高 |
| Phase 4 | v0.11.x | 高级分析，差异化竞争力 | 中 |
| Phase 5 | v0.12.x+ | 生产级完善 | 中 |

**建议先做 Phase 1（v0.8.x）**，验证 SQL 层与存储层的完整打通，再逐步推进。

---

## 七、与现有模块的对接点

| 现有模块 | 对接方式 | 改动量 |
|----------|----------|--------|
| 存储引擎 (column_store) | TableScan/IndexScan 算子调用存储层 API | 小 |
| Delta Store | Insert/Update/Delete 写入 Delta 层 | 中 |
| 事务管理器 (txn) | 每条 SQL 绑定事务上下文 | 中 |
| WAL | DML 操作写 WAL | 小 |
| 压缩模块 | 扫描时自动解码，已在存储层封装 | 无 |
| 索引模块 | IndexScan / BitmapScan 算子 | 中 |
| 向量索引 | 向量相似度查询算子 | 中 |

---

*文档版本：v1.0 · 2026-08-01 · 基于 HybridDB v0.7.6 现状规划*
