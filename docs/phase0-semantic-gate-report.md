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
| FOREIGN KEY | **ENFORCED**（v0.22.5）— 孤儿引用被拒 | 无需补偿 |

## A4 事务回滚（新增闸门）

| 路径 | 结果 |
|------|------|
| A4a 未读直接 ROLLBACK | ✅ 行数归零 |
| A4b 读己之写后 ROLLBACK | ⚠️ **KNOWN-GAP**：flush-on-read 走独立内部事务，ROLLBACK 撤不掉已 flush 行 |
| A4c COMMIT 持久化 | ✅ |

**A4b 根因**：`flush_txn_buffer` 调 `execute_with_txn` 开独立事务落盘，
外层 `ROLLBACK` 只能撤未 flush 的 buffer。llmRx 要求「Rollback 真的撤销」→ 未满。

**修复方向**：flush 时不走独立事务，行进外层事务的 staging/undo-log；
或 rollback 时按 write_set 删除已 flush 的 rowid。属 P1 架构改动。

## 修复优先级（若仍要碰主存储）

1. **P0** ~~DEFAULT 填充~~ ✅ 已修
2. **P0** ~~FK 强制~~ ✅ 已修
3. **P1** A4b 读后回滚撤销（undo-log / staging）
4. P2 多连接并发（A5）
5. P3 ATTACH 等价（D5 按天分片）

Phase 0 闭环：**七项语义闸门全绿 + A4 部分达成（A4b 已知缺口）。**
