# 高优先级功能实现计划

> 创建日期：2026-08-25
> 基于版本：v0.21.x

---

## 一、现状分析（重要发现）

经过代码分析，**很多功能已经实现**，文档未同步：

### 已实现但文档未更新的功能

| 类别 | 文档显示 | 实际状态 | 说明 |
|------|----------|----------|------|
| **标量函数** | 0 项 | **50+ 函数已实现** | 包含字符串/数学/日期/JSON/向量函数 |
| **PRAGMA** | 0 项 | **12+ 命令已实现** | table_info, information_schema, journal_mode 等 |
| **ALTER TABLE** | 缺失 | **AST + 执行已有，缺 parser** | 只需添加 SQL 解析 |
| **日期函数** | 缺失 | **10+ 已实现** | NOW, DATE, TIME, STRFTIME, DATE_ADD 等 |
| **窗口函数** | 缺失 | **已实现** | ROW_NUMBER, RANK, DENSE_RANK, LAG, LEAD |

### 真正需要实现的高优先级功能

| 功能 | 复杂度 | 预计工作量 |
|------|--------|-----------|
| **ALTER TABLE Parser** | 低 | 0.5 天 |
| **VIEW 支持** | 中 | 2-3 天 |
| **CHECK 约束** | 中 | 2-3 天 |
| **标量子查询** | 中 | 3-4 天 |
| **递归 CTE** | 高 | 5-7 天 |

---

## 二、实现计划

### 2.1 ALTER TABLE Parser（0.5 天）

**现状**：AST 定义完整（`AlterTableStmt` + `AlterTableOp`），执行器完整（`alter_table.rs`），仅缺 SQL 解析。

**需要修改的文件**：
- `src/sql/parser.rs` - 添加 ALTER TABLE 解析

**实现步骤**：

```rust
// 在 convert_statement() 中添加 sqlast::Statement::AlterTable 分支
sqlast::Statement::AlterTable { table_name, operations, .. } => {
    // 解析每个操作
    for op in operations {
        match op {
            sqlast::AlterTableOperation::AddColumn { column_def, .. } => { ... }
            sqlast::AlterTableOperation::DropColumn { column_name, .. } => { ... }
            sqlast::AlterTableOperation::RenameColumn { old_column_name, new_column_name, .. } => { ... }
            sqlast::AlterTableOperation::RenameTable { table_name, .. } => { ... }
        }
    }
}
```

**测试用例**：
```sql
ALTER TABLE users ADD COLUMN email VARCHAR;
ALTER TABLE users DROP COLUMN email;
ALTER TABLE users RENAME COLUMN name TO full_name;
ALTER TABLE users RENAME TO customers;
```

---

### 2.2 VIEW 支持（2-3 天）

**需要新增的组件**：

1. **AST 节点**（`src/sql/ast.rs`）：
```rust
// Statement 枚举新增
CreateView {
    name: String,
    columns: Option<Vec<String>>,
    query: Box<Statement>,  // SELECT 语句
    or_replace: bool,
},
DropView {
    name: String,
    if_exists: bool,
},
```

2. **Catalog 存储**：
- View 定义需要持久化到 catalog
- 新增 `ViewDef { name, columns, sql_text }` 结构

3. **Parser**（`src/sql/parser.rs`）：
- sqlparser 已支持 CREATE VIEW，只需在 `convert_statement` 中处理

4. **Planner**（`src/sql/planner.rs`）：
- CREATE VIEW → 保存 view 定义
- 查询 view → 展开为子查询

5. **Executor**：
- 新增 `CreateView` / `DropView` 物理计划节点

**关键设计决策**：
- View 是逻辑定义，不存储数据
- 查询 view 时展开为原始 SELECT
- View 支持 `OR REPLACE` 语法

---

### 2.3 CHECK 约束（2-3 天）

**需要修改的组件**：

1. **ColumnDef 扩展**（`src/common/types.rs`）：
```rust
pub struct ColumnDef {
    // ... 现有字段
    pub check_expr: Option<String>,  // CHECK 表达式文本
}
```

2. **Parser**：
- 解析 `column_name TYPE CHECK (expr)` 语法

3. **INSERT/UPDATE 执行器**：
- 在写入前求值 CHECK 表达式
- 不满足则返回 `ConstraintViolation` 错误

4. **Catalog 持久化**：
- CHECK 表达式随 TableDef 序列化

**实现模式**：
```rust
// 在 insert_row / update_row 前检查
if let Some(check_expr) = &col.check_expr {
    let value = &row[col_idx];
    if !eval_check_constraint(check_expr, value)? {
        return Err(EngramDbError::ConstraintViolation(
            format!("CHECK constraint failed: {}", check_expr)
        ));
    }
}
```

---

### 2.4 标量子查询（3-4 天）

**现状**：部分子查询已支持（IN 子查询），但标量子查询（SELECT 列中的子查询）未实现。

**需要修改的组件**：

1. **AST**（`src/sql/ast.rs`）：
```rust
Expression::Subquery(Box<Statement>),  // 标量子查询
```

2. **Planner**（`src/sql/planner.rs`）：
- 检测 Expression::Subquery
- 递归规划子查询
- 包装为 ScalarSubquery 物理计划节点

3. **物理计划**（`src/executor/physical_plan.rs`）：
```rust
PhysicalPlan::ScalarSubquery {
    input: Box<PhysicalPlan>,  // 外层查询
    subquery: Box<PhysicalPlan>,  // 子查询
    column_name: String,
}
```

4. **Executor**：
- 对每行执行子查询
- 返回单值作为列值

**支持的语法**：
```sql
SELECT name, (SELECT COUNT(*) FROM orders WHERE user_id = users.id) AS order_count
FROM users;

SELECT * FROM users WHERE age > (SELECT AVG(age) FROM users);
```

---

### 2.5 递归 CTE（5-7 天）

**现状**：普通 CTE 部分支持，递归 CTE 未实现。

**需要修改的组件**：

1. **AST**（`src/sql/ast.rs`）：
```rust
Statement::WithRecursive {
    cte_name: String,
    columns: Vec<String>,
    anchor: Box<Statement>,      // 非递归部分
    recursive: Box<Statement>,   // 递归部分
    main_query: Box<Statement>,  // 主查询
}
```

2. **Parser**：
- 解析 `WITH RECURSIVE cte_name AS (anchor UNION ALL recursive) SELECT ...`

3. **Planner**：
- 识别递归引用
- 生成迭代执行计划

4. **Executor**：
- 迭代执行直到结果集为空
- 设置最大迭代次数防止无限递归

**支持的语法**：
```sql
WITH RECURSIVE hierarchy AS (
    SELECT id, name, parent_id, 0 AS depth
    FROM categories WHERE parent_id IS NULL
    UNION ALL
    SELECT c.id, c.name, c.parent_id, h.depth + 1
    FROM categories c JOIN hierarchy h ON c.parent_id = h.id
)
SELECT * FROM hierarchy;
```

---

## 三、实施顺序

| 优先级 | 功能 | 工作量 | 依赖 | 建议版本 |
|--------|------|--------|------|----------|
| P0 | ALTER TABLE Parser | 0.5 天 | 无 | v0.22.0 |
| P1 | VIEW 支持 | 2-3 天 | 无 | v0.22.0 |
| P1 | CHECK 约束 | 2-3 天 | 无 | v0.22.0 |
| P2 | 标量子查询 | 3-4 天 | 无 | v0.23.0 |
| P3 | 递归 CTE | 5-7 天 | 标量子查询 | v0.24.0 |

**总预计工作量**：13-18 天

---

## 四、验收标准

### ALTER TABLE Parser
- [x] `ALTER TABLE t ADD COLUMN c TYPE` 可执行
- [x] `ALTER TABLE t DROP COLUMN c` 可执行
- [x] `ALTER TABLE t RENAME COLUMN old TO new` 可执行
- [x] `ALTER TABLE t RENAME TO new_name` 可执行
- [x] 集成测试覆盖所有操作

### VIEW 支持
- [x] `CREATE VIEW v AS SELECT ...` 可执行
- [x] `DROP VIEW v` 可执行
- [x] `SELECT * FROM v` 可查询（视图定义存储）
- [x] View 定义持久化到 catalog
- [ ] 嵌套 View 支持（v0.23+）

### CHECK 约束
- [x] `CREATE TABLE t (c INT CHECK (c > 0))` 可创建
- [x] `INSERT INTO t VALUES (-1)` 报错
- [x] `INSERT INTO t VALUES (1)` 成功
- [x] CHECK 表达式持久化
- [x] 复合表达式支持（AND/OR/NOT）
- [x] BETWEEN 表达式支持
- [x] IN 列表支持

### 标量子查询
- [x] SELECT 列中的标量子查询可执行
- [x] WHERE 中的标量子查询可执行
- [x] EXISTS 子查询支持
- [x] IN 子查询支持
- [x] NULL 三值逻辑支持
- [ ] 相关子查询支持（v0.23+）

### 递归 CTE
- [x] `WITH RECURSIVE ... UNION ALL ...` 可执行
- [x] 最大迭代次数限制（默认 1000）
- [x] 无限循环检测
- [x] 树形遍历示例可运行
- [x] 列数匹配验证
- [x] FROM 子句验证

## 五、剩余工作（下一版本）

### 中优先级功能
- [ ] 嵌套 View 支持（VIEW 引用 VIEW）
- [ ] 相关子查询（子查询引用外层查询的列）
- [ ] ALTER COLUMN（修改列类型/可空性）
- [ ] CHECK 约束支持跨列表达式（如 `price > cost`）

### 低优先级功能
- [ ] DATE/TIME 类型
- [ ] ARRAY 类型
- [ ] JSONB 类型
- [ ] UUID 类型
- [ ] ENUM/SET 类型

### 文档完善
- [ ] 更新 README.md 的特性列表
- [ ] 更新 CHANGELOG.md
- [ ] 添加 v0.22.0 发布说明
- [ ] 添加使用示例

### 性能优化
- [ ] 视图查询优化（视图合并）
- [ ] 递归 CTE 性能优化（增量计算）
- [ ] CHECK 约束性能优化（延迟验证）
