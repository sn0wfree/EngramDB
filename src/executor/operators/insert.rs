//! 插入算子
//!
//! 支持双路径执行：
//! - 事务路径（enable_transaction=true）：保证 ACID，通过 WAL + MVCC
//! - 非事务路径（enable_transaction=false）：高性能直接写入，跳过 WAL/MVCC

use log::{error, warn, info, debug, trace};

use crate::common::error::{Result, EngramDbError};
use crate::storage::Database;
use crate::txn::{ApplyOp, IsolationLevel};
use crate::Value;

/// 执行行式插入（根据配置选择事务/非事务路径）
///
/// `bypass_batch`：绕过 P0-2 攒批合并（INSERT ... RETURNING 需立即读回插入行）。
pub fn execute(
    db: &mut Database,
    table_name: &str,
    rows: Vec<Vec<Value>>,
    bypass_batch: bool,
) -> Result<u64> {
    trace!("insert::execute called: table_name={}, rows_count={}", table_name, rows.len());
    
    // 防御性检查：事务路径需要 txn_manager 已初始化
    if db.config().enable_transaction {
        debug!("Transaction path enabled, checking txn_manager readiness...");
        
        if !db.txn_manager().is_ready() {
            error!("Transaction manager not ready");
            error!("Config: enable_transaction=true but txn_manager is not initialized");
            error!("This is likely a bug in Database initialization");
            
            // 生产环境降级到非事务路径
            warn!("Falling back to non-transaction path due to txn_manager not ready");
            return execute_without_txn(db, table_name, rows);
        }
        debug!("✓ txn_manager is ready");
    }
    
    // 防御性检查：表存在
    debug!("Checking if table '{}' exists...", table_name);
    let _table_id = db.table_names().get(table_name)
        .ok_or_else(|| {
            error!("Table '{}' not found", table_name);
            EngramDbError::TableNotFound(table_name.into())
        })?;
    debug!("✓ Table '{}' exists", table_name);
    
    // 根据配置选择路径
    let path = if db.config().enable_transaction { "txn" } else { "direct" };
    info!("Executing INSERT: table={}, rows={}, path={}", table_name, rows.len(), path);
    
    if db.config().enable_transaction {
        // P0-2 攒批合并（autocommit 逐行 INSERT）：先入批，阈值触发才落盘。
        // v0.20：主键/唯一索引/NOT NULL 表也可入批（入批时即校验，错误
        // 在该语句返回时暴露）；FK/TTL 表仍绕过（table_excludes_batch）。
        if !bypass_batch
            && db.batch_insert_enabled()
            && !db.in_explicit_txn()
            && table_batchable(db, table_name)
            && !table_excludes_batch(db, table_name)
        {
            let n = rows.len();
            // 入批预检：已提交冲突 + 批内自重复（push_checked 内批内判重，
            // 已提交部分在 validate_rows_against_committed）
            db.validate_rows_against_committed(table_name, &rows)?;
            let def = db.get_engine_table(table_name).map(|t| t.def().clone());
            let (pk_col, unique_cols) = match def {
                Some(d) => (
                    d.primary_key_index(),
                    d.indexes.iter()
                        .filter(|i| i.unique)
                        .map(|i| (i.name.clone(), i.key_columns[0]))
                        .collect::<Vec<_>>(),
                ),
                None => (None, Vec::new()),
            };
            let trigger = db.insert_batcher().push_checked(table_name, rows, pk_col, &unique_cols)?;
            if trigger {
                let all = db.insert_batcher().drain(table_name);
                return execute_with_txn(db, table_name, all);
            }
            // 已入批：事务语义等价于"已接受未提交"（WAL 组提交同款异步窗口）
            return Ok(n as u64);
        }
        // P0-2 事务级 Batcher：显式事务内 INSERT 攒入事务私有 buffer，
        // 在非裸 INSERT 语句 / SAVEPOINT / COMMIT 前一次性 flush 为单个
        // 内部批量事务。v0.20：约束表也可攒批（txn_buffer_push 内入批即校验）。
        if !bypass_batch
            && db.in_explicit_txn()
            && db.config().txn_batch_enabled
            && (table_batchable(db, table_name) || !db.config().txn_batch_bypass_constraint_tables)
            && !table_excludes_batch(db, table_name)
        {
            let n = rows.len();
            let trigger = db.txn_buffer_push(table_name, rows)?;
            if trigger {
                db.flush_txn_buffer()?;
            }
            return Ok(n as u64);
        }
        execute_with_txn(db, table_name, rows)
    } else {
        execute_without_txn(db, table_name, rows)
    }
}

/// v0.20：表是否可入攒批（约束检查在入批时进行，错误即时报）
///
/// 可入批：
/// - 无任何约束的表（任何引擎，沿用 v0.18 行为，无需预检）
/// - Columnar 引擎 + 主键/NOT NULL/auto_increment/唯一索引
///   （这些约束都能在入批时零副作用预检）
/// 绕过：FK / TTL / 非 Columnar 约束表（保守，后续迭代）。
fn table_batchable(db: &Database, table_name: &str) -> bool {
    let Some(table) = db.get_engine_table(table_name) else {
        return false; // 表不存在由上层抛 TableNotFound
    };
    let def = table.def();
    // FK / TTL：无法低成本入批预检，绕过
    if !def.foreign_keys.is_empty() || def.ttl_column.is_some() {
        return false;
    }
    let has_any_constraint = !def.indexes.is_empty()
        || def.columns.iter().any(|c| !c.nullable || c.is_primary_key || c.auto_increment);
    if !has_any_constraint {
        // 无约束表：无需预检，直接入批（v0.18 原行为，任何引擎）
        return true;
    }
    // 有约束表：仅 Columnar 且全部约束都能入批预检（PK / NOT NULL / auto / 唯一索引）
    if def.engine != crate::common::types::EngineType::Columnar {
        return false;
    }
    def.columns.iter().any(|c| !c.nullable || c.is_primary_key || c.auto_increment)
        || def.indexes.iter().any(|i| i.unique)
}

/// v0.20：表必须绕过攒批（入批时无法低成本预检的约束）
///
/// FK：外键引用完整性需要跨表点查（依赖关系复杂，后续迭代）
/// TTL：TTL 填充/检查在落盘路径内联，入批会改变填充时机
fn table_excludes_batch(db: &Database, table_name: &str) -> bool {
    let Some(table) = db.get_engine_table(table_name) else {
        return false; // 表不存在由上层抛 TableNotFound
    };
    let def = table.def();
    !def.foreign_keys.is_empty() || def.ttl_column.is_some()
}

/// P0-2：冲刷全部攒批缓冲（每表一个事务批量落盘）
///
/// 触发点：非裸 INSERT 语句执行前（读己之写）、显式事务开始前、
/// `close` / `sync_wal` / `checkpoint` 前。
pub fn flush_all_batched(db: &mut Database) -> Result<()> {
    if db.insert_batcher().is_empty() {
        return Ok(());
    }
    let pending = db.insert_batcher().drain_all();
    for (table_name, rows) in pending {
        if !rows.is_empty() {
            execute_with_txn(db, &table_name, rows)?;
        }
    }
    Ok(())
}

/// 事务路径执行：保证 ACID
///
/// 流程：
/// 1. 开启事务
/// 2. 事务内批量插入（写 WAL + MVCC）
/// 3. 提交事务（fsync WAL）
/// 4. 将 apply_ops 应用到存储层
///
/// `pub(crate)`：事务级 Batcher（`Database::flush_txn_buffer`）复用此路径
/// 将事务 buffer 一次性落盘。
pub(crate) fn execute_with_txn(
    db: &mut Database,
    table_name: &str,
    rows: Vec<Vec<Value>>,
) -> Result<u64> {
    debug!("Starting transaction path execution...");

    // 0. 冲突预检（PK / 唯一索引；失败零副作用，事务未开启）
    //    防止 apply 阶段失败导致 MVCC 已提交版本残留（rowid 复用误判 Update）
    if !rows.is_empty() {
        let table_def = db.get_engine_table(table_name)
            .ok_or_else(|| EngramDbError::TableNotFound(table_name.into()))?
            .def()
            .clone();
        let conflict_indices: Vec<usize> = table_def.primary_key_index()
            .map(|pk| vec![pk])
            .unwrap_or_default();
        if !conflict_indices.is_empty() {
            let pk_name = table_def.columns[conflict_indices[0]].name.clone();
            let mut seen = std::collections::HashSet::new();
            for row in &rows {
                if let Some(cell) = conflict_indices.first().and_then(|&i| row.get(i)) {
                    // auto_increment 列在分配前为 Null → 跳过（由 insert 后检查兜底）
                    if cell.is_null() {
                        continue;
                    }
                    if !seen.insert(cell.clone()) {
                        return Err(EngramDbError::ConstraintViolation(format!(
                            "UNIQUE constraint failed: {}={:?}", pk_name, cell
                        )));
                    }
                    let conflict = db.get_engine_table_mut(table_name)
                        .and_then(|t| t.lookup_primary_key(cell));
                    if conflict.is_some() {
                        return Err(EngramDbError::ConstraintViolation(format!(
                            "UNIQUE constraint failed: {}={:?}", pk_name, cell
                        )));
                    }
                }
            }
        }
        // v0.20：唯一索引冲突预检（批内自重复 + 已提交点查；整批一次摊薄）。
        // 与批量 apply（update_indexes_for_rows）的键列语义一致：key_columns[0]。
        let unique_idx: Vec<(String, usize)> = table_def.indexes.iter()
            .filter(|i| i.unique)
            .map(|i| (i.name.clone(), i.key_columns[0]))
            .collect();
        if !unique_idx.is_empty() {
            let mut seen: Vec<std::collections::HashSet<Value>> =
                vec![std::collections::HashSet::new(); unique_idx.len()];
            for row in &rows {
                for (i, (idx_name, key_col)) in unique_idx.iter().enumerate() {
                    if let Some(cell) = row.get(*key_col) {
                        if !seen[i].insert(cell.clone()) {
                            return Err(EngramDbError::ConstraintViolation(format!(
                                "UNIQUE constraint failed: index '{}'", idx_name
                            )));
                        }
                        if db.get_engine_table(table_name)
                            .is_some_and(|t| t.unique_index_contains(idx_name, cell))
                        {
                            return Err(EngramDbError::ConstraintViolation(format!(
                                "UNIQUE constraint failed: index '{}'", idx_name
                            )));
                        }
                    }
                }
            }
        }
    }
    
    // 1. 开启事务
    debug!("Beginning transaction...");
    let isolation = db.config().default_isolation_level;
    let txn_id = db.txn_manager_mut().begin(isolation)?;
    info!("Transaction started: txn_id={}, isolation={:?}", txn_id, isolation);
    
    // 2. 事务内插入行（P-W2a：批量走 batch_insert，单次 WAL + MVCC）
    debug!("Inserting {} rows in transaction {}...", rows.len(), txn_id);
    let table_id = *db.table_names().get(table_name).unwrap();
    let base_row_id = db
        .get_engine_table(table_name)
        .map(|et| et.def().row_count as u32)
        .unwrap_or(0);
    let rows_len = rows.len();

    db.txn_manager_mut().batch_insert(txn_id, table_id, base_row_id as u64, rows)?;
    debug!("✓ All {} rows inserted in transaction {} (batch)", rows_len, txn_id);
    
    // 3. 提交事务（会 fsync WAL）
    debug!("Committing transaction {}...", txn_id);
    let result = db.txn_manager_mut().commit(txn_id)?;
    info!("Transaction {} committed: commit_ts={}, apply_ops_count={}",
          txn_id, result.commit_ts, result.apply_ops.len());
    
    // 4. 应用到存储层；失败必须 abort 事务（清理 MVCC 残留版本，
    //    否则失败事务占用的 rowid 会残留版本链，导致后续语句被误判为 Update）
    debug!("Applying {} operations to storage...", result.apply_ops.len());
    if let Err(e) = apply_to_storage(db, result.apply_ops) {
        let _ = db.txn_manager_mut().rollback(txn_id);
        return Err(e);
    }
    info!("✓ Applied {} operations to storage", rows_len);
    
    info!("Transaction path completed: {} rows inserted", rows_len);
    Ok(rows_len as u64)
}

/// 非事务路径执行：高性能直接写入
///
/// 直接调用 table.insert()，跳过 WAL 和 MVCC
fn execute_without_txn(
    db: &mut Database,
    table_name: &str,
    rows: Vec<Vec<Value>>,
) -> Result<u64> {
    debug!("Starting non-transaction path execution...");
    
    let table = db.get_engine_table_mut(table_name)
        .ok_or_else(|| EngramDbError::TableNotFound(table_name.into()))?;
    
    debug!("Inserting {} rows directly into table '{}'...", rows.len(), table_name);
    table.insert_rows(rows.clone())?;
    
    info!("Non-transaction path completed: {} rows inserted", rows.len());
    Ok(rows.len() as u64)
}

/// 应用事务操作到存储层
///
/// M02 优化：将连续的同表 Insert 段打包走 `table.insert(batch_rows)`，
/// 减少 N 次单条 insert_row 的方法调用与索引维护开销。
/// P-W2c：`ApplyOp::InsertBatch`（由 collect_apply_ops 合并产出）直接走
/// `table.insert_columns` 列式落盘（无行→列转置）。
/// Update/Delete 仍按原顺序逐行应用（保持操作顺序正确性）。
pub fn apply_to_storage(db: &mut Database, mut ops: Vec<ApplyOp>) -> Result<()> {
    trace!("apply_to_storage called with {} operations", ops.len());

    let mut idx = 0;
    while idx < ops.len() {
        // P-W2c：InsertBatch 直接列式落盘（优先分支）
        if let ApplyOp::InsertBatch { table_id, base_row_id, columns } = &ops[idx] {
            let table = db.tables_mut().get_mut(table_id)
                .ok_or_else(|| {
                    error!("Table not found: table_id={}", table_id);
                    EngramDbError::TableNotFound(format!("id={}", table_id))
                })?;

            // 仅当 base_row_id 与当前表行数对齐时走 insert_columns（列式批量）
            // 否则退回逐行 insert_row（保持 rowid 语义）
            let base = table.def().row_count as u32;
            if *base_row_id == base as u64 && !columns.is_empty() {
                let cols = columns.clone();
                // skip_pk_check=true：冲突已由 execute_with_txn 预检
                let inserted = table.insert_columns_with_check(cols, true)?;
                debug!("✓ P-W2c InsertBatch applied: table_id={}, rows={}", table_id, inserted);
            } else {
                // 非对齐：按 base_row_id + i 逐行插入
                let num_rows = columns.first().map(|c| c.len()).unwrap_or(0);
                for i in 0..num_rows {
                    let mut row = Vec::with_capacity(columns.len());
                    for col in columns {
                        if i < col.len() {
                            row.push(col[i].clone());
                        } else {
                            row.push(Value::Null);
                        }
                    }
                    table.insert_row_with_check((*base_row_id + i as u64) as u32, &row, true)?;
                }
                debug!("✓ InsertBatch applied (non-aligned): table_id={}, rows={}", table_id, num_rows);
            }
            idx += 1;
            continue;
        }

        // 检测当前位置开始的「连续同表 Insert」段
        if let ApplyOp::Insert { table_id: start_tid, .. } = ops[idx] {
            // 收集后续与 start_tid 相同的 Insert
            let mut run_end = idx + 1;
            while run_end < ops.len() {
                if let ApplyOp::Insert { table_id: tid, .. } = ops[run_end] {
                    if tid == start_tid {
                        run_end += 1;
                        continue;
                    }
                }
                break;
            }
            let run_len = run_end - idx;

            if run_len > 1 {
                // M02：批量 Insert，尝试走 table.insert(rows) 接口
                // 收集 rows 与预期 row_id 序列（move 语义，避免每行克隆）
                let mut rows: Vec<Vec<Value>> = Vec::with_capacity(run_len);
                let mut row_ids: Vec<u32> = Vec::with_capacity(run_len);
                for op in &mut ops[idx..run_end] {
                    if let ApplyOp::Insert { table_id: _, row_id, row } = op {
                        rows.push(std::mem::take(row));
                        row_ids.push(*row_id as u32);
                    }
                }

                let table = db.tables_mut().get_mut(&start_tid)
                    .ok_or_else(|| {
                        error!("Table not found: table_id={}", start_tid);
                        EngramDbError::TableNotFound(format!("id={}", start_tid))
                    })?;

                // 只有当 row_ids 与当前 table.def.row_count 连续对齐时，才能走 table.insert()
                // （因为 insert() 内部使用 def.row_count 作为 base_row_id）
                let base = table.def().row_count as u32;
                let contiguous = row_ids.iter().enumerate()
                    .all(|(i, &rid)| rid == base + i as u32);

                if contiguous {
                    // 走批量路径（内部一次 row_count += N，一次索引批量构建）
                    // skip_pk_check=true：冲突已由 execute_with_txn 预检
                    let inserted = table.insert_with_check(rows, true)?;
                    debug!("✓ M02 Batch Insert applied: table_id={}, rows={}", start_tid, inserted);
                } else {
                    // 非连续 row_id（罕见场景），退回逐行
                    for (i, row) in rows.into_iter().enumerate() {
                        table.insert_row_with_check(row_ids[i], &row, true)?;
                    }
                    debug!("✓ Insert applied (non-contiguous): table_id={}, count={}", start_tid, run_len);
                }

                idx = run_end;
                continue;
            }
        }

        // 非批量路径：单个操作
        trace!("Applying operation {}/{}", idx + 1, ops.len());
        let op = &mut ops[idx];

        match op {
            ApplyOp::Insert { table_id, row_id, row } => {
                trace!("Insert: table_id={}, row_id={}, row={:?}", table_id, row_id, row);
                let table = db.tables_mut().get_mut(table_id)
                    .ok_or_else(|| {
                        error!("Table not found: table_id={}", table_id);
                        error!("This indicates a bug in collect_apply_ops()");
                        EngramDbError::TableNotFound(format!("id={}", table_id))
                    })?;
                table.insert_row_with_check(*row_id as u32, row, true)?;
                debug!("✓ Insert applied: table_id={}, row_id={}", table_id, row_id);
            }
            ApplyOp::InsertBatch { table_id, base_row_id, columns } => {
                // 防御分支：正常情况下 InsertBatch 在循环顶部已被处理
                // （该分支仅当 ops 顺序异常时可达）
                let table = db.tables_mut().get_mut(table_id)
                    .ok_or_else(|| {
                        error!("Table not found: table_id={}", table_id);
                        EngramDbError::TableNotFound(format!("id={}", table_id))
                    })?;
                let num_rows = columns.first().map(|c| c.len()).unwrap_or(0);
                for i in 0..num_rows {
                    let mut row = Vec::with_capacity(columns.len());
                    for col in columns.iter() {
                        if i < col.len() {
                            row.push(col[i].clone());
                        } else {
                            row.push(Value::Null);
                        }
                    }
                    table.insert_row_with_check((*base_row_id + i as u64) as u32, &row, true)?;
                }
                debug!("✓ InsertBatch applied (fallback): table_id={}, rows={}", table_id, num_rows);
            }
            ApplyOp::Update { table_id, row_id, new_row } => {
                trace!("Update: table_id={}, row_id={}, new_row={:?}", table_id, row_id, new_row);
                let table = db.tables_mut().get_mut(table_id)
                    .ok_or_else(|| {
                        error!("Table not found: table_id={}", table_id);
                        error!("This indicates a bug in collect_apply_ops()");
                        EngramDbError::TableNotFound(format!("id={}", table_id))
                    })?;
                table.update_row(*row_id as u32, new_row)?;
                debug!("✓ Update applied: table_id={}, row_id={}", table_id, row_id);
            }
            ApplyOp::Delete { table_id, row_id } => {
                trace!("Delete: table_id={}, row_id={}", table_id, row_id);
                let table = db.tables_mut().get_mut(table_id)
                    .ok_or_else(|| {
                        error!("Table not found: table_id={}", table_id);
                        error!("This indicates a bug in collect_apply_ops()");
                        EngramDbError::TableNotFound(format!("id={}", table_id))
                    })?;
                table.delete_row(*row_id as u32)?;
                debug!("✓ Delete applied: table_id={}, row_id={}", table_id, row_id);
            }
        }

        idx += 1;
    }

    info!("✓ All {} operations applied to storage", ops.len());
    Ok(())
}

/// 执行列式插入（③：批量 INSERT 列式路径）
///
/// 与 `execute()` 一样按配置选择路径：
/// - 事务路径（enable_transaction=true）：转置为行后走事务插入
///   （WAL + MVCC），保证与 DELETE/UPDATE 的事务语义一致
/// - 非事务路径（enable_transaction=false）：直接列式写入
///   `table.insert_columns`（语义与 `table.insert(rows)` 一致：
///   类型强转 / AUTO_INCREMENT / TTL / NOT NULL / 索引维护）
pub fn execute_columns(
    db: &mut Database,
    table_name: &str,
    columns: Vec<Vec<Value>>,
) -> Result<u64> {
    let num_rows = if columns.is_empty() { 0 } else { columns[0].len() };
    if num_rows == 0 {
        return Ok(0);
    }

    // 防御性检查：事务路径需要 txn_manager 已初始化（与 execute() 一致）
    if db.config().enable_transaction {
        if !db.txn_manager().is_ready() {
            warn!("Falling back to non-transaction path due to txn_manager not ready");
            return insert_columns_direct(db, table_name, columns);
        }
        // 事务路径：转置为行后走事务插入（保证 MVCC 可见性 / WAL 一致性）
        let num_cols = columns.len();
        let mut rows: Vec<Vec<Value>> = Vec::with_capacity(num_rows);
        for r in 0..num_rows {
            let mut row = Vec::with_capacity(num_cols);
            for col in &columns {
                row.push(col[r].clone());
            }
            rows.push(row);
        }
        return execute_with_txn(db, table_name, rows);
    }

    insert_columns_direct(db, table_name, columns)
}

/// 非事务路径的列式直写
fn insert_columns_direct(
    db: &mut Database,
    table_name: &str,
    columns: Vec<Vec<Value>>,
) -> Result<u64> {
    let table = db.get_engine_table_mut(table_name)
        .ok_or_else(|| crate::common::error::EngramDbError::TableNotFound(table_name.into()))?;
    table.insert_columns(columns)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::config::Config;
    use crate::Value;

    #[test]
    fn test_insert_direct_txn_path() {
        let mut conn = crate::Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)").unwrap();
        let db = conn.database_mut();
        let n = execute(db, "t", vec![
            vec![Value::Int64(1), Value::Int64(10)],
            vec![Value::Int64(2), Value::Int64(20)],
        ], true).unwrap();
        assert_eq!(n, 2);
        let rows = db.get_table_mut("t").unwrap().scan_to_rows_direct(&[0, 1]).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0][0], Value::Int64(1));
    }

    #[test]
    fn test_insert_without_txn() {
        let mut cfg = Config::default();
        cfg.enable_transaction = false;
        let mut conn = crate::Connection::open_with_config(":memory:", cfg).unwrap();
        conn.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)").unwrap();
        let db = conn.database_mut();
        let n = execute(db, "t", vec![vec![Value::Int64(1), Value::Int64(10)]], false).unwrap();
        assert_eq!(n, 1);
        let rows = db.get_table_mut("t").unwrap().scan_to_rows_direct(&[0, 1]).unwrap();
        assert_eq!(rows.len(), 1);
    }

    #[test]
    fn test_insert_empty_rows() {
        let mut conn = crate::Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)").unwrap();
        let db = conn.database_mut();
        let n = execute(db, "t", vec![], true).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn test_insert_table_not_found() {
        let mut conn = crate::Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)").unwrap();
        let db = conn.database_mut();
        let err = execute(db, "nope", vec![vec![Value::Int64(1)]], true).unwrap_err();
        assert!(matches!(err, EngramDbError::TableNotFound(_)), "got: {err:?}");
    }

    #[test]
    fn test_insert_duplicate_pk_rejected() {
        let mut conn = crate::Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)").unwrap();
        let db = conn.database_mut();
        execute(db, "t", vec![vec![Value::Int64(1), Value::Int64(10)]], true).unwrap();
        let err = execute(db, "t", vec![vec![Value::Int64(1), Value::Int64(99)]], true).unwrap_err();
        assert!(matches!(err, crate::common::error::EngramDbError::ConstraintViolation(_)), "got: {err:?}");
        // 失败无副作用：行数不变
        assert_eq!(db.get_table("t").unwrap().def().row_count, 1);
    }

    #[test]
    fn test_insert_columns_direct() {
        let mut cfg = Config::default();
        cfg.enable_transaction = false;
        let mut conn = crate::Connection::open_with_config(":memory:", cfg).unwrap();
        conn.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)").unwrap();
        let db = conn.database_mut();
        let n = execute_columns(db, "t", vec![
            vec![Value::Int64(1), Value::Int64(2)],
            vec![Value::Int64(10), Value::Int64(20)],
        ]).unwrap();
        assert_eq!(n, 2);
        let rows = db.get_table_mut("t").unwrap().scan_to_rows_direct(&[0, 1]).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1][1], Value::Int64(20));
    }

    #[test]
    fn test_insert_columns_empty() {
        let mut conn = crate::Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)").unwrap();
        let db = conn.database_mut();
        let n = execute_columns(db, "t", vec![]).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn test_flush_all_batched() {
        let mut cfg = Config::default();
        cfg.enable_transaction = true;
        let mut conn = crate::Connection::open_with_config(":memory:", cfg).unwrap();
        conn.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)").unwrap();
        let db = conn.database_mut();
        // 无挂起批次 → 空操作
        flush_all_batched(db).unwrap();
        // 有挂起批次（小批入 batcher）
        let n = execute(db, "t", vec![vec![Value::Int64(1), Value::Int64(10)]], false).unwrap();
        assert_eq!(n, 1);
        flush_all_batched(db).unwrap();
        assert!(db.insert_batcher().is_empty());
        let rows = db.get_table_mut("t").unwrap().scan_to_rows_direct(&[0, 1]).unwrap();
        assert_eq!(rows.len(), 1);
    }

    // ========================================================================
    // v0.20：约束表进入攒批（入批即校验）
    // ========================================================================

    /// 小型攒批阈值：逐行 INSERT 立即入批、少量语句即触发 flush
    fn small_batch_cfg() -> Config {
        let mut cfg = Config::default();
        cfg.insert_batch_rows = 4;
        cfg.insert_batch_bytes = 1024 * 1024;
        cfg.insert_batch_timeout_ms = 0;
        cfg
    }

    #[test]
    fn test_pk_table_batched_autocommit() {
        // 主键表 autocommit 逐行 INSERT：入批 → 阈值触发单事务 flush，
        // 行全部落盘且主键索引正确
        let mut conn = crate::Connection::open_with_config(":memory:", small_batch_cfg()).unwrap();
        conn.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)").unwrap();
        for i in 0..10 {
            conn.execute(&format!("INSERT INTO t VALUES ({i}, {})", i * 10)).unwrap();
        }
        let r = conn.execute("SELECT COUNT(*) FROM t").unwrap();
        assert_eq!(r.rows[0][0], Value::Int64(10));
        let r = conn.execute("SELECT v FROM t WHERE id = 7").unwrap();
        assert_eq!(r.rows[0][0], Value::Int64(70));
    }

    #[test]
    fn test_pk_batch_internal_duplicate_rejected_immediately() {
        // 批内 PK 重复：第二条语句即时报错（入批校验），行数不变
        let mut conn = crate::Connection::open_with_config(":memory:", small_batch_cfg()).unwrap();
        conn.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)").unwrap();
        conn.execute("INSERT INTO t VALUES (1, 10)").unwrap();
        let err = conn.execute("INSERT INTO t VALUES (1, 99)").unwrap_err();
        assert!(matches!(err, crate::common::error::EngramDbError::ConstraintViolation(_)),
            "got: {err:?}");
        let r = conn.execute("SELECT COUNT(*) FROM t").unwrap();
        assert_eq!(r.rows[0][0], Value::Int64(1));
    }

    #[test]
    fn test_pk_batch_conflict_with_committed_rejected_immediately() {
        // 与已提交行 PK 冲突：即时报错（入批校验点查已提交状态）
        let mut conn = crate::Connection::open_with_config(":memory:", small_batch_cfg()).unwrap();
        conn.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)").unwrap();
        conn.execute("INSERT INTO t VALUES (5, 50)").unwrap();
        // flush 使 id=5 已提交
        let r = conn.execute("SELECT COUNT(*) FROM t").unwrap();
        assert_eq!(r.rows[0][0], Value::Int64(1));
        let err = conn.execute("INSERT INTO t VALUES (5, 99)").unwrap_err();
        assert!(matches!(err, crate::common::error::EngramDbError::ConstraintViolation(_)),
            "got: {err:?}");
    }

    #[test]
    fn test_unique_index_batch_duplicate_rejected_immediately() {
        // 唯一索引批内重复：即时报错
        let mut conn = crate::Connection::open_with_config(":memory:", small_batch_cfg()).unwrap();
        conn.execute("CREATE TABLE t (id INT PRIMARY KEY, u INT UNIQUE)").unwrap();
        conn.execute("INSERT INTO t VALUES (1, 100)").unwrap();
        let err = conn.execute("INSERT INTO t VALUES (2, 100)").unwrap_err();
        assert!(matches!(err, crate::common::error::EngramDbError::ConstraintViolation(_)),
            "got: {err:?}");
        let r = conn.execute("SELECT COUNT(*) FROM t").unwrap();
        assert_eq!(r.rows[0][0], Value::Int64(1));
    }

    #[test]
    fn test_batch_apply_unique_conflict_reported() {
        // ①号修复回归：批量 apply（M02/InsertBatch）唯一索引冲突必须报错，
        // 不再静默丢弃（表与索引不一致）
        let mut conn = crate::Connection::open_with_config(":memory:", small_batch_cfg()).unwrap();
        conn.execute("CREATE TABLE t (id INT PRIMARY KEY, u INT UNIQUE)").unwrap();
        // 大批量（>threshold 走列式/批量 apply）含批内唯一重复
        let rows: Vec<Vec<Value>> = (0..2000).map(|i| vec![Value::Int64(i), Value::Int64(i % 100)]).collect();
        let err = crate::executor::operators::insert::execute(conn.database_mut(), "t", rows, true).unwrap_err();
        assert!(matches!(err, crate::common::error::EngramDbError::ConstraintViolation(_)),
            "got: {err:?}");
    }

    #[test]
    fn test_not_null_batch_rejected_immediately() {
        // NOT NULL 违反：即时报错（入批校验）
        let mut conn = crate::Connection::open_with_config(":memory:", small_batch_cfg()).unwrap();
        conn.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT NOT NULL)").unwrap();
        let err = conn.execute("INSERT INTO t VALUES (1, NULL)").unwrap_err();
        assert!(matches!(err, crate::common::error::EngramDbError::ConstraintViolation(_)),
            "got: {err:?}");
        let r = conn.execute("SELECT COUNT(*) FROM t").unwrap();
        assert_eq!(r.rows[0][0], Value::Int64(0));
    }

    #[test]
    fn test_auto_increment_pk_batched_ids_contiguous() {
        // auto_increment 主键表攒批：flush 分配 ID 连续正确
        let mut conn = crate::Connection::open_with_config(":memory:", small_batch_cfg()).unwrap();
        conn.execute("CREATE TABLE t (id INT AUTO_INCREMENT PRIMARY KEY, v INT)").unwrap();
        for i in 0..10 {
            conn.execute("INSERT INTO t (v) VALUES (100)").unwrap();
        }
        let r = conn.execute("SELECT COUNT(*) FROM t").unwrap();
        assert_eq!(r.rows[0][0], Value::Int64(10));
        let r = conn.execute("SELECT id FROM t ORDER BY id").unwrap();
        let ids: Vec<Value> = r.rows.iter().map(|row| row[0].clone()).collect();
        let expect: Vec<Value> = (1..=10).map(|i| Value::Int64(i)).collect();
        assert_eq!(ids, expect, "auto_increment 攒批后 ID 必须 1..10 连续");
    }

    #[test]
    fn test_rollback_discards_batched_pk_inserts() {
        // 显式事务 + 主键表：ROLLBACK 正确丢弃攒批行（旧行为撤不掉）
        let mut conn = crate::Connection::open_with_config(":memory:", small_batch_cfg()).unwrap();
        conn.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)").unwrap();
        conn.execute("BEGIN").unwrap();
        conn.execute("INSERT INTO t VALUES (1, 10)").unwrap();
        conn.execute("INSERT INTO t VALUES (2, 20)").unwrap();
        conn.execute("ROLLBACK").unwrap();
        let r = conn.execute("SELECT COUNT(*) FROM t").unwrap();
        assert_eq!(r.rows[0][0], Value::Int64(0), "ROLLBACK 必须丢弃显式事务内主键表攒批行");
    }

    #[test]
    fn test_txn_api_duplicate_pk_errors_at_insert() {
        // Transaction API：重复 PK 在 insert 即报错（而非 commit 时）
        let mut conn = crate::Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)").unwrap();
        let mut txn = conn.begin().unwrap();
        txn.insert("t", vec![vec![Value::Int64(1), Value::Int64(10)]]).unwrap();
        let err = txn.insert("t", vec![vec![Value::Int64(1), Value::Int64(99)]]).unwrap_err();
        assert!(matches!(err, crate::common::error::EngramDbError::ConstraintViolation(_)),
            "Transaction API 重复 PK 应在 insert 即时报错, got: {err:?}");
        txn.rollback().unwrap();
    }

    #[test]
    fn test_returning_bypasses_batcher() {
        // RETURNING 仍绕过攒批：语句内即时读回插入行
        let mut conn = crate::Connection::open_with_config(":memory:", small_batch_cfg()).unwrap();
        conn.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)").unwrap();
        let r = conn.execute("INSERT INTO t VALUES (1, 10) RETURNING id, v").unwrap();
        assert_eq!(r.rows.len(), 1);
        assert_eq!(r.rows[0][0], Value::Int64(1));
        assert_eq!(r.rows[0][1], Value::Int64(10));
    }

    #[test]
    fn test_fk_table_bypasses_batcher() {
        // FK 表仍绕过攒批（table_excludes_batch）：INSERT 立即落盘（非攒批）
        let mut conn = crate::Connection::open_with_config(":memory:", small_batch_cfg()).unwrap();
        conn.execute("CREATE TABLE p (id INT PRIMARY KEY)").unwrap();
        conn.execute("CREATE TABLE c (id INT PRIMARY KEY, pid INT REFERENCES p(id))").unwrap();
        conn.execute("INSERT INTO p VALUES (1)").unwrap();
        conn.execute("INSERT INTO c VALUES (10, 1)").unwrap();
        // FK 表不攒批：插入后立即可见（无需 flush 即可从原始 API 读到）
        let db = conn.database_mut();
        let rows = db.get_engine_table_mut("c").unwrap().scan_to_rows_direct(&[0, 1], None).unwrap();
        assert_eq!(rows.len(), 1, "FK 表应绕过攒批、立即落盘");
        assert_eq!(rows[0][1], Value::Int64(1));
    }
}