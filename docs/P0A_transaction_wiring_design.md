# P0-A：接通事务到 SQL 写入路径 + 启动恢复

> 任务来源：[audit_v0.12.md](./audit_v0.12.md) P0 任务
> 设计日期：2026-08-03
> 状态：设计中

---

## 一、问题分析

### 1.1 当前问题

**核心问题**：事务子系统与存储层是"平行世界"

- `TransactionManager` 有完整的 `begin/insert/commit/rollback` 接口（[txn/manager.rs#L68-L172](../src/txn/manager.rs#L68-L172)）
- 但 `executor/operators/insert.rs` 完全绕过，直接调用 `table.insert()`
- 结果：**SQL INSERT 既不写 WAL，也不走 MVCC，崩溃后已提交数据丢失**

### 1.2 数据流对比

| 阶段 | 当前路径（绕过事务） | 目标路径（ACID 生效） |
|------|----------------------|----------------------|
| SQL INSERT | `PhysicalPlan::Insert` | `PhysicalPlan::Insert` |
| 执行器 | `executor.rs:65` → `insert::execute()` | `executor.rs` → `txn_manager.begin()` |
| 插入逻辑 | `table.insert(rows)` → 直接写存储 | `txn_manager.insert()` → WAL + MVCC |
| 提交 | 无 | `txn_manager.commit()` → WAL COMMIT + fsync |
| 崩溃恢复 | `open_existing()` **不调用** `recover()` | `open_existing()` 调用 `recover()` 重放 WAL |

---

## 二、设计方案

### 2.1 事务/非事务双路径架构

**核心设计**：通过配置开关灵活切换事务/非事务路径

#### 配置开关

```rust
// common/config.rs
pub struct Config {
    /// 是否启用事务支持（默认 true）
    pub enable_transaction: bool,
    
    /// 事务隔离级别（仅 enable_transaction=true 时生效）
    pub default_isolation_level: IsolationLevel,
}

pub enum IsolationLevel {
    SnapshotIsolation,
    Serializable,
}
```

#### 双路径逻辑

```rust
// executor/operators/insert.rs
pub fn execute(db: &mut Database, table_name: &str, rows: Vec<Vec<Value>>) -> Result<u64> {
    if db.config().enable_transaction {
        execute_with_txn(db, table_name, rows)  // 事务路径：保证 ACID
    } else {
        execute_without_txn(db, table_name, rows)  // 非事务路径：高性能直接写入
    }
}
```

---

### 2.2 MVCC 提交时自动应用：方案 B

**选择理由**：改动范围小、性能最优、测试友好

#### 核心结构

```rust
// txn/types.rs
pub enum ApplyOp {
    Insert { table_id: u32, row_id: u64, row: Vec<Value> },
    Update { table_id: u32, row_id: u64, new_row: Vec<Value> },
    Delete { table_id: u32, row_id: u64 },
}

pub struct CommitResult {
    pub commit_ts: Timestamp,
    pub apply_ops: Vec<ApplyOp>,
}
```

#### 提交流程

```rust
// txn/manager.rs
pub fn commit(&mut self, txn_id: TxnId) -> Result<CommitResult> {
    // 1. 写入 WAL COMMIT 记录并刷盘
    self.wal.write_record(WalRecordType::Commit, txn_id, 0, &[])?;
    self.wal.commit_flush()?;
    
    // 2. 获取 commit_ts
    let commit_ts = self.active_table.commit_txn(txn_id);
    
    // 3. 提交 MVCC 版本
    for (table_id, rowid) in &write_set {
        if let Some(store) = self.mvcc.get_mut(table_id) {
            store.commit_txn(txn_id, commit_ts);
        }
    }
    
    // 4. 收集待应用操作（不直接访问存储层）
    let apply_ops = self.collect_apply_ops(txn_id, commit_ts)?;
    
    Ok(CommitResult { commit_ts, apply_ops })
}

fn collect_apply_ops(&self, txn_id: TxnId, commit_ts: Timestamp) -> Result<Vec<ApplyOp>> {
    let ctx = self.txns.get(&txn_id).unwrap();
    let mut ops = Vec::new();
    
    for (table_id, rowid) in &ctx.write_set {
        if let Some(store) = self.mvcc.get(table_id) {
            if let Some(row) = store.get_for_txn(*rowid, commit_ts, txn_id) {
                ops.push(ApplyOp::Insert {
                    table_id: *table_id,
                    row_id: *rowid,
                    row: row.clone(),
                });
            }
        }
    }
    
    Ok(ops)
}
```

#### 应用到存储层

```rust
// executor/operators/insert.rs
fn execute_with_txn(db: &mut Database, table_name: &str, rows: Vec<Vec<Value>>) -> Result<u64> {
    // ... 开启事务 + 插入 ...
    
    let result = db.txn_manager.commit(txn_id)?;
    apply_to_storage(db, &result.apply_ops)?;
    
    Ok(rows.len() as u64)
}

fn apply_to_storage(db: &mut Database, ops: &[ApplyOp]) -> Result<()> {
    for op in ops {
        match op {
            ApplyOp::Insert { table_id, row_id, row } => {
                let table = db.tables.get_mut(table_id)?;
                table.insert_row(*row_id as u32, row)?;
            }
            ApplyOp::Update { table_id, row_id, new_row } => {
                let table = db.tables.get_mut(table_id)?;
                table.update_row(*row_id as u32, new_row)?;
            }
            ApplyOp::Delete { table_id, row_id } => {
                let table = db.tables.get_mut(table_id)?;
                table.delete_row(*row_id as u32)?;
            }
        }
    }
    Ok(())
}
```

---

### 2.3 启动恢复路径

```rust
// storage/mod.rs
fn open_existing(path: &std::path::Path, config: Config) -> Result<Self> {
    // ... 现有逻辑 ...
    
    // 新增：调用 recover() 重放 WAL
    if config.enable_transaction {
        let recovery_result = wal::recover(&wal_path)?;
        if recovery_result.transactions_committed > 0 {
            log::info!("Recovery: {} transactions redone", recovery_result.transactions_committed);
        }
    }
    
    Ok(db)
}
```

---

## 三、两种模式对比

| 维度 | 事务模式<br/>`enable_transaction=true` | 非事务模式<br/>`enable_transaction=false` |
|------|:--------------------------------------:|:------------------------------------------:|
| **ACID 保证** | ✅ 完整（WAL + MVCC） | ❌ 无（崩溃丢数据） |
| **性能** | 🟡 中（WAL fsync 开销） | 🟢 高（直接写入） |
| **并发事务** | ✅ 支持（MVCC 快照隔离） | ❌ 单线程独占 |
| **崩溃恢复** | ✅ 自动恢复未提交事务 | ❌ 无恢复机制 |
| **适用场景** | 生产环境、在线业务 | 批量导入、离线分析、临时库 |
| **写入吞吐** | ~10K TPS | ~100K TPS |

---

## 四、使用示例

### 4.1 命令行参数

```bash
# 启用事务模式（默认）
hybriddb --db mydb.hdb --enable-transaction true

# 禁用事务模式（批量导入高性能）
hybriddb --db bulk_import.hdb --enable-transaction false

# 指定隔离级别
hybriddb --db mydb.hdb --enable-transaction true --isolation-level snapshot-isolation
```

### 4.2 Rust API

```rust
use hybriddb::{Database, Config, IsolationLevel};

// 生产环境：启用事务
let config = Config {
    enable_transaction: true,
    default_isolation_level: IsolationLevel::SnapshotIsolation,
    ..Default::default()
};
let mut db = Database::open_with_config("production.hdb", config)?;

// 批量导入：禁用事务
let config = Config {
    enable_transaction: false,
    ..Default::default()
};
let mut db = Database::open_with_config("bulk_import.hdb", config)?;
```

---

## 五、改动点清单

| # | 文件 | 改动 | 预估时间 |
|---|------|------|----------|
| 1 | `common/config.rs` | 新增 `enable_transaction` + `default_isolation_level` | 0.5 天 |
| 2 | `common/types.rs` | 新增 `IsolationLevel` 枚举 | 0.5 天 |
| 3 | `txn/mod.rs` | 新增 `ApplyOp` + `CommitResult` | 0.5 天 |
| 4 | `txn/manager.rs` | `commit()` 返回 `CommitResult` | 0.5 天 |
| 5 | `executor/operators/insert.rs` | 双路径逻辑 | 1 天 |
| 6 | `executor/operators/update.rs` | 双路径逻辑 | 0.5 天 |
| 7 | `executor/operators/delete.rs` | 双路径逻辑 | 0.5 天 |
| 8 | `storage/table.rs` | 新增 `insert_row()` + `update_row()` + `delete_row()` | 0.5 天 |
| 9 | `storage/mod.rs` | `open_existing()` 调用 `recover()` | 0.5 天 |
| 10 | `main.rs` | 命令行参数解析 | 0.5 天 |
| 11 | `wal/recovery.rs` | 完善 `recover()` 实际应用 redo | 1 天 |
| 12 | `tests/integration/txn_recovery.rs` | 崩溃恢复集成测试 | 1 天 |
| 13 | `docs/transaction_mode.md` | 使用指南文档 | 0.5 天 |

**总预估**：7 天

---

## 六、验收标准

| 测试 | 验证点 |
|------|--------|
| `test_txn_insert_wal` | SQL INSERT 产生 WAL 记录（`wal.current_lsn() > 0`） |
| `test_txn_commit_fsync` | 提交后 WAL 已刷盘（检查文件大小） |
| `test_crash_recovery` | 插入 1000 行 → kill → 重启 → 数据完整 |
| `test_txn_rollback` | 插入 → 回滚 → 查询无数据 |
| `test_config_disable_txn` | 禁用事务 → 直接写入 → 性能提升 10× |

---

## 七、风险与缓解

| 风险 | 概率 | 影响 | 缓解措施 |
|------|------|------|----------|
| MVCC 与存储层双写 | 高 | 高 | 设计"提交时应用"机制，避免重复写入 |
| 事务路径性能下降 | 中 | 中 | 保留 `enable_transaction` 开关，可降级 |
| 崩溃恢复逻辑复杂 | 中 | 高 | 分步实现：先写 WAL，再完善 recover |
| 并发事务冲突 | 低 | 高 | 利用现有 MVCC 写写冲突检测 |

---

## 八、实施进度

- [x] 设计文档完成
- [ ] Config 新增字段
- [ ] 定义 ApplyOp + CommitResult
- [ ] 改造 insert.rs 双路径逻辑
- [ ] 改造 update.rs / delete.rs
- [ ] storage/table.rs 新增方法
- [ ] 启动恢复调用 recover()
- [ ] 命令行参数解析
- [ ] 测试 + 文档