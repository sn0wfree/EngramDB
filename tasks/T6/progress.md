# T6 / A-Persist · W0 进度（A3 + H1）

- 分支：`wave0/persist-a3-longrun`
- Commit：`ff8eca5` feat: A3 可配置 checkpoint 阈值（字节+时间）自动触发，H1 WAL 长跑上界断言测试
- 状态：**代码已提交；集成测试 wal_growth 未跑通（观测逻辑 bug，非接线问题），见下「测试摘要」与「剩余工作」**

## 设计要点（阈值语义）

- `Config.wal_checkpoint_threshold`（已有字段，本次赋予语义）：自上次 WAL checkpoint（段滚动）以来 WAL 累计字节（≈ 当前段 LSN，含未刷盘缓冲）≥ 阈值 → 触发；0 = 禁用。默认 16MB。
- `Config.wal_checkpoint_timeout_ms`（新增）：距上次 checkpoint 超过该毫秒数、且 WAL 非空 → 触发；0 = 禁用。默认 0（禁用，可选阈值）。
- 两阈值 **OR** 关系；WAL 为空永不触发（避免空转全量落盘）。
- 判定逻辑在 `src/wal/checkpoint.rs::CheckpointTrigger`（纯逻辑，可单测）；`WalWriter` 持有并在 `roll_segment()` 时重置时间窗；`TransactionManager::new` 从 Config 接线，暴露 `auto_checkpoint_due()`。
- **触发 ≠ 执行**：完整 checkpoint = 主存储落盘（`Database::checkpoint`，src/storage，本周不可改）→ WAL 段滚动回收。只滚 WAL 会丢未落盘 redo。执行点：`Transaction::commit`（src/txn，本方可改）在 `apply_to_storage` 之后、非内存库、`auto_checkpoint_due()` 时调用 `Database::checkpoint()`。
- **GAP（需 Lead/他 Agent 接线）**：SQL autocommit 路径（executor 的 insert/update/delete/executor.rs，`db.on_commit()` 调用点）不在本 Agent 所有权内，未接自动触发——SQL 长跑仍需上层轮询 `auto_checkpoint_due()` 或由 executor 加一行 hook。

## 改动文件（仅本 Agent 所有权内）

| 文件 | 内容 |
|---|---|
| `src/common/config.rs` | 新增 `wal_checkpoint_timeout_ms`；`wal_checkpoint_threshold` 语义注释；默认值 + 测试 |
| `src/wal/checkpoint.rs`（新） | `CheckpointTrigger`（字节/时间阈值、OR、空 WAL 不触发、mark 重置）+ 8 个单测 |
| `src/wal/mod.rs` | 导出 `CheckpointTrigger` |
| `src/wal/writer.rs` | WalWriter 持有 trigger；`set_checkpoint_policy` / `checkpoint_due`；`roll_segment` 重置；+3 单测 |
| `src/txn/manager.rs` | `new` 接线 Config；`auto_checkpoint_due()` / `set_wal_checkpoint_policy()` |
| `src/txn/transaction.rs` | `commit()` 尾部自动触发完整 checkpoint（apply 之后，保证先落盘再滚 WAL） |
| `tests/wal_growth.rs`（新） | H1 长跑上界断言 + A3 阈值语义集成测试（未跑通，见下） |

未动：`src/storage/**`、`src/executor/**`、`src/sql/**`、`src/lib.rs`、`benches/**`、`.github/**`、`tests/phase0_semantic_gate.rs`（A-Test 的，未 commit）。

## 测试摘要（WSL，输出重定向 `target/wsl_logs/*.log`）

| 套件 | 结果 |
|---|---|
| `cargo test --lib wal::`（含 checkpoint/writer/recovery 单测） | **ok. 78 passed; 0 failed** |
| `cargo test --lib txn::` | **ok. 83 passed; 0 failed** |
| `cargo test --lib common::config` | **ok. 4 passed; 0 failed** |
| `cargo test --test wal_growth` | **FAILED. 0 passed; 2 failed** |

wal_growth 失败分析（**观测逻辑 bug，非触发接线失效**）：
- `wal_growth_auto_checkpoint_bounded`：核心上界断言 2000 次迭代**全部通过**（WAL 始终 ≤ 224KB 预算，末态 27KB，远小于累计写入 ~数百 KB）→ 自动 checkpoint 确实在滚动回收。失败仅在「首次触发」检测：要求观测到 total ≥ 128KB，但触发在同一 commit 内把大小重置，观测点永远看不到 ≥ 阈值的瞬间。
- `auto_checkpoint_due_semantics`：循环顶部查 `due`，但 due 一变 true 就被同一 commit 的自动 checkpoint 消费掉，顶部永远看不到 true。应改为：直接构造 `TransactionManager`（无自动消费）测 due 翻转 + 用「WAL 大小断崖下跌」作 Connection 路径触发证据。
- 模块级「达到触发/不达不触发」已有单测覆盖且全绿（`src/wal/checkpoint.rs` 8 个、`src/wal/writer.rs` 3 个）。

## 剩余工作（下轮，均为测试修正，非功能）

1. `tests/wal_growth.rs`：首次触发检测改为「total 出现下降即 checkpoint 发生」；floor 断言常量 `ITERATIONS*150`；test2 拆为 (A) Connection 不达不触发（已过）(B) 直接 `TransactionManager` 测 due 翻转+checkpoint 复位 (C) Connection 小阈值下数据完整。
2. 我引入的警告：`src/wal/checkpoint.rs` 两处 unused `mut`（121/149 行）。其余警告（transaction.rs unused Database import、writer.rs WalCompression/Seek 等）经 `git show HEAD~1` 核对为 **main 上已存在**，非本次引入。
3. SQL autocommit 路径自动触发 hook（executor，需 Lead 授权）。

## 阻塞

- 无硬阻塞。共享工作区曾把 HEAD 带到 `wave0/b4-a4b-gate`（A-Test 已 commit db2e012），已切回本分支，commit 只含本 Agent 7 文件。
- `cargo fmt`（stable）会因 rustfmt.toml nightly-only 选项与全库格式不兼容，**勿在本仓库跑 stable fmt**（本次已确认无内容损伤，仅行尾噪音，checkout 后已消失）。
