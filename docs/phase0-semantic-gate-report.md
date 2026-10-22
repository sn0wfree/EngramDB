# Phase 0 语义验收报告

日期：2026-10-22
测试：`tests/phase0_semantic_gate.rs`
运行：`wsl.exe -d Ubuntu -u ll -- bash scripts/wsl_run_phase0.sh`

## 总览

| # | 闸门 | 结果 | 证据 |
|---|------|------|------|
| G1 | `AUTOINCREMENT` 拼写建表 + 自增 | ✅ PASS | id 1,2 递增 |
| G2a | NOT NULL 真强制（显式 NULL 拒绝） | ✅ PASS | `Constraint violation: NOT NULL` |
| G2b | 省略列吃到 `DEFAULT` | ❌ **FAIL** | INSERT 省略列失败（DEFAULT 未解析/未填充） |
| G3 | UNIQUE 真拒绝 | ✅ PASS | 二次插入同值报 UNIQUE 冲突 |
| G4 | `ON CONFLICT DO UPDATE / DO NOTHING` | ✅ PASS | 更新/跳过语义正确 |
| G5 | `RETURNING id` 返回自增值 | ✅ PASS | 返回 1、2 |
| G6 | `ALTER TABLE ADD COLUMN` 幂等 | ✅ PASS | 重复 ADD 明确报错 `Column already exists` |
| G7 | FK + CHECK 行为记录 | ✅ RECORDED | 见下 |

**结论：6/7 过。唯一硬缺口 = G2b DEFAULT 填充。**

## G7 行为明细

| 约束 | 行为 | 补偿方案 |
|------|------|---------|
| CHECK（列级） | **ENFORCED** — 非法值被拒 | 无需补偿 |
| FOREIGN KEY | **SILENT** — 孤儿 pid=99 入库 | 应用层校验；不得依赖 `ON DELETE CASCADE` |

## 根因（G2b）

`src/sql/parser.rs` 的 `convert_statement` / CREATE TABLE 分支只提取：

```
nullable / primary_key / auto_increment / unique / check_expr
```

**没有提取 `ColumnOption::Default`**，内部 `ColumnDef` 也无 `default_value` 字段。
表级 `FOREIGN KEY ... REFERENCES` 约束（`constraints` 字段）同样被忽略。

llmRx 主库有 **150 处 `NOT NULL DEFAULT`**，缺 DEFAULT 填充则：
- 省略列的 INSERT 全部失败，或
- 必须在应用层显式写全 150 处默认值（与 SQLite 行为不兼容）

## 判定

| 问题 | 判定 |
|------|------|
| 能否当 llmRx 主存储？ | **否**（DEFAULT 缺口 + 既有 A4/A5/C1/C2/D5 硬阻塞） |
| 能否当分析层/向量层？ | **可以**，Phase 0 的 SQL 语义面（G1/G3–G6）已达标 |
| G2b 是否阻塞分析层？ | 否——分析层走 `import_columns` 列式直写，不依赖 DEFAULT |

## 修复优先级（若仍要碰主存储）

1. **P0** 解析 `ColumnOption::Default` → `ColumnDef.default_value`，INSERT 省略列时填充
2. **P1** 解析表级 `FOREIGN KEY` 约束并强制（或文档明确不支持 + llmRx 侧补偿）
3. P2 真事务回滚 / 多连接并发（A4/A5）
4. P3 ATTACH 等价（D5 按天分片）

Phase 0 到此闭环：**语义面缺口已钉死，补偿方案已定。**
