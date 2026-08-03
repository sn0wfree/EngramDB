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
pub fn execute(
    db: &mut Database,
    table_name: &str,
    rows: Vec<Vec<Value>>,
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
        execute_with_txn(db, table_name, rows)
    } else {
        execute_without_txn(db, table_name, rows)
    }
}

/// 事务路径执行：保证 ACID
///
/// 流程：
/// 1. 开启事务
/// 2. 事务内逐行插入（写 WAL + MVCC）
/// 3. 提交事务（fsync WAL）
/// 4. 将 apply_ops 应用到存储层
fn execute_with_txn(
    db: &mut Database,
    table_name: &str,
    rows: Vec<Vec<Value>>,
) -> Result<u64> {
    debug!("Starting transaction path execution...");
    
    // 1. 开启事务
    debug!("Beginning transaction...");
    let isolation = db.config().default_isolation_level;
    let txn_id = db.txn_manager_mut().begin(isolation)?;
    info!("Transaction started: txn_id={}, isolation={:?}", txn_id, isolation);
    
    // 2. 事务内插入每行
    debug!("Inserting {} rows in transaction {}...", rows.len(), txn_id);
    let table_id = *db.table_names().get(table_name).unwrap();
    let base_row_id = db.get_table(table_name).unwrap().def.row_count as u32;
    
    for (idx, row) in rows.iter().enumerate() {
        let row_id = base_row_id + idx as u32;
        trace!("Inserting row {} (row_id={}): {:?}", idx, row_id, row);
        
        db.txn_manager_mut().insert(txn_id, table_id, row_id as u64, row.clone())?;
        
        if idx % 100 == 0 {
            debug!("Inserted {}/{} rows in transaction {}", idx + 1, rows.len(), txn_id);
        }
    }
    debug!("✓ All {} rows inserted in transaction {}", rows.len(), txn_id);
    
    // 3. 提交事务（会 fsync WAL）
    debug!("Committing transaction {}...", txn_id);
    let result = db.txn_manager_mut().commit(txn_id)?;
    info!("Transaction {} committed: commit_ts={}, apply_ops_count={}",
          txn_id, result.commit_ts, result.apply_ops.len());
    
    // 4. 应用到存储层
    debug!("Applying {} operations to storage...", result.apply_ops.len());
    apply_to_storage(db, &result.apply_ops)?;
    info!("✓ Applied {} operations to storage", result.apply_ops.len());
    
    info!("Transaction path completed: {} rows inserted", rows.len());
    Ok(rows.len() as u64)
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
    
    let table = db.get_table_mut(table_name)
        .ok_or_else(|| EngramDbError::TableNotFound(table_name.into()))?;
    
    debug!("Inserting {} rows directly into table '{}'...", rows.len(), table_name);
    table.insert(rows.clone())?;
    
    info!("Non-transaction path completed: {} rows inserted", rows.len());
    Ok(rows.len() as u64)
}

/// 应用事务操作到存储层
///
/// M02 优化：将连续的同表 Insert 段打包走 `table.insert(batch_rows)`，
/// 减少 N 次单条 insert_row 的方法调用与索引维护开销。
/// Update/Delete 仍按原顺序逐行应用（保持操作顺序正确性）。
pub fn apply_to_storage(db: &mut Database, ops: &[ApplyOp]) -> Result<()> {
    trace!("apply_to_storage called with {} operations", ops.len());

    let mut idx = 0;
    while idx < ops.len() {
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
                // 收集 rows 与预期 row_id 序列
                let mut rows: Vec<Vec<Value>> = Vec::with_capacity(run_len);
                let mut row_ids: Vec<u32> = Vec::with_capacity(run_len);
                for op in &ops[idx..run_end] {
                    if let ApplyOp::Insert { table_id: _, row_id, row } = op {
                        rows.push(row.clone());
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
                let base = table.def.row_count as u32;
                let contiguous = row_ids.iter().enumerate()
                    .all(|(i, &rid)| rid == base + i as u32);

                if contiguous {
                    // 走批量路径（内部一次 row_count += N，一次索引批量构建）
                    let inserted = table.insert(rows)?;
                    debug!("✓ M02 Batch Insert applied: table_id={}, rows={}", start_tid, inserted);
                } else {
                    // 非连续 row_id（罕见场景），退回逐行
                    for (i, row) in rows.into_iter().enumerate() {
                        table.insert_row(row_ids[i], &row)?;
                    }
                    debug!("✓ Insert applied (non-contiguous): table_id={}, count={}", start_tid, run_len);
                }

                idx = run_end;
                continue;
            }
        }

        // 非批量路径：单个操作
        let op = &ops[idx];
        trace!("Applying operation {}/{}: {:?}", idx + 1, ops.len(), op);

        match op {
            ApplyOp::Insert { table_id, row_id, row } => {
                trace!("Insert: table_id={}, row_id={}, row={:?}", table_id, row_id, row);
                let table = db.tables_mut().get_mut(table_id)
                    .ok_or_else(|| {
                        error!("Table not found: table_id={}", table_id);
                        error!("This indicates a bug in collect_apply_ops()");
                        EngramDbError::TableNotFound(format!("id={}", table_id))
                    })?;
                table.insert_row(*row_id as u32, row)?;
                debug!("✓ Insert applied: table_id={}, row_id={}", table_id, row_id);
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

/// 执行列式插入（向量化写入路径）
///
/// 直接以列式数据写入，跳过 SQL 解析和行→列转置。
/// 性能比行式插入高 30-50%（取决于列数和数据类型）。
pub fn execute_columns(
    db: &mut Database,
    table_name: &str,
    columns: Vec<Vec<Value>>,
) -> Result<u64> {
    let table = db.get_table_mut(table_name)
        .ok_or_else(|| crate::common::error::EngramDbError::TableNotFound(table_name.into()))?;

    let num_rows = if columns.is_empty() { 0 } else { columns[0].len() };
    if num_rows == 0 {
        return Ok(0);
    }

    // 大批量：直接写入列存（跳过 Delta 层）
    let direct_threshold = (table.column_store().row_group_size() / 4) as usize;
    if num_rows >= direct_threshold && num_rows >= 1000 {
        table.column_store_mut().append_columns(&columns)?;
        table.def_mut().row_count += num_rows as u64;
    } else {
        // 小批量：走列式 Delta 层
        table.delta_store_mut().insert_columns(columns)?;
    }

    Ok(num_rows as u64)
}