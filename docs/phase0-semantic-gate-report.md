# Phase 0 语义验收报告

日期：2026-10-22（首跑）→ 2026-10-22（G2b 修复后 7/7）
测试：`tests/phase0_semantic_gate.rs`
运行：`wsl.exe -d Ubuntu -u ll -- bash scripts/wsl_run_phase0.sh`

## 总览

| # | 闸门 | 结果 | 证据 |
|---|------|------|------|
| G1 | `AUTOINCREMENT` 拼写建表 + 自增 | ✅ PASS | id 1,2 递增 |
| G2a | NOT NULL 真强制（显式 NULL 拒绝） | ✅ PASS | `Constraint violation: NOT NULL` |
| G2b | 省略列吃到 `DEFAULT` | ✅ **PASS**（修复后） | role=0, status=1 |
| G3 | UNIQUE 真拒绝 | ✅ PASS | 二次插入同值报 UNIQUE 冲突 |
| G4 | `ON CONFLICT DO UPDATE / DO NOTHING` | ✅ PASS | 更新/跳过语义正确 |
| G5 | `RETURNING id` 返回自增值 | ✅ PASS | 返回 1、2 |
| G6 | `ALTER TABLE ADD COLUMN` 幂等 | ✅ PASS | 重复 ADD 明确报错 `Column already exists` |
| G7 | FK + CHECK 行为记录 | ✅ RECORDED | 见下 |

**结论：7/7 全过。** 回归：lib 1229 / differential_sqlite 10 亦全绿。

## G7 行为明细

| 约束 | 行为 | 补偿方案 |
|------|------|---------|
| CHECK（列级） | **ENFORCED** — 非法值被拒 | 无需补偿 |
| FOREIGN KEY | **SILENT** — 孤儿 pid=99 入库 | 应用层校验；不得依赖 `ON DELETE CASCADE` |

## G2b 修复（v0.22.5）

根因：`sql::ast::ColumnDef` 无 `default_value`，parser 不提取 `ColumnOption::Default`，
INSERT 省略列时用 `Value::Null` 填充 → NOT NULL 直接拒绝。

改动：

1. `sql::ast::ColumnDef` 增加 `default_value: Option<String>`
2. `parser.rs` CREATE TABLE / ALTER ADD COLUMN 提取 `ColumnOption::Default`
3. `planner.rs` `plan_create_table` 映射到 `types::ColumnDef.default()`
4. `plan_insert` / `eval_insert_rows` 用 `row_with_defaults` 预填省略列
5. `alter_table.rs` 传递 `default_value` 到元数据

字面量转换 `default_literal_to_value`：`0`/`0.1`/`'abc'`/`true`/`NULL`。

## 判定

| 问题 | 判定 |
|------|------|
| Phase 0 语义面 | **全过** |
| 能否当 llmRx 主存储？ | **否**（仍缺 A4 真回滚 / A5 并发 / C1 FK / D5 ATTACH） |
| 能否当分析层/向量层？ | **可以** |

## 修复优先级（若仍要碰主存储）

1. **P0** ~~DEFAULT 填充~~ ✅ 已修
2. **P1** 表级 `FOREIGN KEY` 强制（或文档明确不支持 + llmRx 侧补偿）
3. P2 真事务回滚 / 多连接并发（A4/A5）
4. P3 ATTACH 等价（D5 按天分片）

Phase 0 闭环：**七项语义闸门全绿。**
