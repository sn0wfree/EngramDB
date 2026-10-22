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

## A4 事务回滚

| 路径 | 结果 |
|------|------|
| A4a 未读直接 ROLLBACK | ✅ 行数归零 |
| A4b 读己之写后 ROLLBACK | ✅ **PASS**（undo-log 修复后） |
| A4c COMMIT 持久化 | ✅ |

**A4b 修复（v0.22.5）**：`Database.txn_flushed_rows` undo-log。
flush 落盘时记录 row_id；ROLLBACK 降序 `delete_row`；COMMIT 清 log 保留行。

## A5 多连接并发（新增）

| 路径 | 结果 |
|------|------|
| A5a 16 线程共享连接写入 | ✅ 3200 行无丢写无串扰 |
| A5b 16 线程并发事务 | ✅ 800 行事务边界干净 |
| A5c 真·多 Connection 同文件 | ⚠️ **GAP**：`Database` 非线程安全，需 `Arc<Mutex<>>` 串行化 |

**A5 契约**：16 并发客户端可用，但必须经外部锁共享单一 `Connection`。
llmRx `SetMaxOpenConns(16)` 接入时，Go driver 侧需持互斥锁（或单连接池）。
真·多连接（16 个独立 `Database` 同文件）属 P2 架构改动。

## 修复优先级（若仍要碰主存储）

1. **P0** ~~DEFAULT 填充~~ ✅
2. **P0** ~~FK 强制~~ ✅
3. **P0** ~~A4b 读后回滚撤销~~ ✅
4. **P1** ~~A5 并发 16~~ ✅（锁串行化契约）/ 真·多连接 = P2
5. P2 ATTACH 等价（D5 按天分片）

Phase 0 闭环：**语义闸门全绿 + A4 全过 + A5 锁契约达标。**
