# T8 进度 · B2 · LEFT/RIGHT JOIN 类型化键

分支：`wave0/b2-join-typed-keys`（自 main `0ddcb4d`）

## 设计要点

- 对齐既有 S2-M3（INNER 单列整数键快路径）与 P1-1（GROUP BY Typed 热路径）模式：
  `Vector::Typed` 的 Int64/Timestamp 键列 → `FxHashMap<i64, …>`（8B key），
  替代 `Vec<Value>` 全量哈希/比较/分配；非 Typed 或类型不支持 → `None` 整体回退 Value 主路径。
- **LEFT**：新增 `hash_join_left_int`（`hash_join.rs`），在 `hash_join_left` 入口按单列键尝试。
  语义差异仅在 probe 侧：NULL 键 / 无匹配 → 左行 + NULL 右列（外连接保留行）；
  build 侧 NULL 键不参与连接（与 `build_hash_table` 一致）。
- **RIGHT**：`execute` 中 RIGHT = 交换参数的 LEFT + 列序换回，自动复用类型化路径。
- **FULL**：第一段经 `hash_join_left` 获得类型化；第二段（右表未匹配行检测）
  新增 `unmatched_right_int`（`FxHashSet<i64>` 成员判定），右表 NULL 键行恒输出、
  左表 NULL 键不入集合，与 Value 主路径逐语义对齐。
- `col_key`（原 `hash_join_inner_int` 内嵌）提升为模块级 `typed_int_key`，INNER/LEFT/FULL 共用。

## 改动

- `src/executor/operators/hash_join.rs`：
  - 新增 `typed_int_key`（模块级）、`hash_join_left_int`、`unmatched_right_int`
  - `hash_join_left` / `hash_join_full` 挂接快路径（单列键，失败即回退）
  - 单测 `test_outer_join_typed_matches_flat`：LEFT/RIGHT/FULL typed↔flat 等价（含 NULL 键，逐类型断言行数 5/4/6）
- `tests/differential_sqlite.rs`：新增 `diff_b2_outer_join_typed_keys`
  （同步插入 uid 无匹配行 + NULL 键行；LEFT/RIGHT/FULL 三查询与 SQLite 对齐）

## 测试摘要（WSL，输出重定向 /tmp/b2_*.log）

- `cargo test --test differential_sqlite` → `test result: ok. 20 passed; 0 failed`（基线 19 + b2 新增 1）
- `cargo test --lib hash_join` → `test result: ok. 10 passed; 0 failed`
- 警告：hash_join.rs 两处 unused `left_cols`/`right_cols` 为 main 既有（`handle_empty_input`），
  本分支未引入新警告。
- 未跑：全量 `cargo test`（时间盒内优先差分 + 相关 lib；bench 未动——A-Bench 并行持有 benches/）。

## 阻塞

- 无。`benches/**` 未改（B3 归 A-Bench）；如需 join bench 挂载由 A-Bench 在统一入口处理。
