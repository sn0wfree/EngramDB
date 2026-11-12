# T5 · A-Test W0 · B4 差分收尾 + H2 A4b 门禁硬化

分支：`wave0/b4-a4b-gate`（自 main `cd4f805`）
Agent：A-Test

## 改动文件

- `tests/phase0_semantic_gate.rs`：A4b（`a4_txn_rollback_undoes` 第②段）残留行分支由 `if count==0 / else println!(KNOWN-GAP)` 双分支软断言改为 `assert_eq!(count, 0, ...)` 硬断言，并更新注释记录 H2/F4 假绿历史与 E2 实测 PASS 依据。**A4b 已为硬断言。**
- `tests/differential_sqlite.rs`：**本轮未改动**（见「未完成 / 阻塞」）。

## 命令与结果

```
wsl -e bash -lc "cd /mnt/d/Github/EngramDB && cargo test --test phase0_semantic_gate --test differential_sqlite > target/a-test-w0.log 2>&1"
```

日志：`target/a-test-w0.log`（全量输出重定向，未粘贴全文）

- `differential_sqlite`：**test result: ok. 19 passed; 0 failed; 0 ignored**
- `phase0_semantic_gate`：**test result: ok. 8 passed; 0 failed; 0 ignored**（含 A4b 硬断言）
- 警告 109 条，全部位于 `src/**`（既有基线 + 共享工作区对方 WIP），非本分支改动引入；本分支未新增警告。

## 提交

- `db2e012` `test: A4b 残留行改硬断言，杜绝门禁假绿`（仅含 `tests/phase0_semantic_gate.rs`）

## 未完成（本轮记 GAP，Lead 2026-11-12 裁定）

- **B4 差分补充（窗口变体 / JOIN 变体 / DECIMAL 边界 / 递归 CTE 收敛）未落笔 → 本轮 GAP，转下一步。**
  过程：曾起草 `eng_cell` Decimal 归一化半成品，按收口指令已 `git checkout` 回退，`tests/differential_sqlite.rs` 与基线一致（19 用例全绿不受影响）。
  已完成前置调研（可直接复用）：
  - 窗口：`src/sql/planner.rs:16-25` 支持 ROW_NUMBER/RANK/DENSE_RANK/LAG/LEAD/FIRST_VALUE/LAST_VALUE/NTH_VALUE；`src/executor/operators/window.rs:287-297` 无显式 frame 时默认 `(0, current_idx+1)`（= UNBOUNDED PRECEDING 到 CURRENT ROW，与 SQLite 默认一致）；LAG/LEAD offset 写死 1（`planner.rs:268-269`），`LAG(x, 2)` 变体需先验证。
  - JOIN：INNER/LEFT/RIGHT/FULL + CROSS 均已规划支持（`parser.rs:1217-1239`、`planner.rs:1460`）；差分需绕开表别名限制（沿用全表名限定风格）。
  - 递归 CTE：`planner.rs:894-948` 支持；执行器 `executor.rs:1173-1233` 存在「简化实现：直接执行 recursive 计划、假设已引用 CTE」注释与 1000 次迭代上限——收敛语义差分是重点风险面。
  - DECIMAL：`Value::Decimal(i128, scale)`，insert 有字面量 coerce（`insert.rs:20-47`）；SQLite 侧为动态类型，DECIMAL 边界差分需双端 CAST 对齐口径（`eng_cell` 需加 Decimal 归一化 arm）。
- 需要改 `src/**` → 无权限，不在本任务范围。

## 下一步（B4 GAP，待 Lead 派回）

1. 在共享工作区确认检出回 `wave0/b4-a4b-gate` 后补 B4 用例（按上述调研直接落 `tests/differential_sqlite.rs`，沿用 `assert_same` 风格）。
2. 红灯 case 按 X1 规范：`// issue: ...` 注释或 `#[ignore = "GAP-..."]` + 预期说明，默认 `cargo test` 不得无解释变红。
3. 复跑同一 WSL 命令，追加结果到本文件。
