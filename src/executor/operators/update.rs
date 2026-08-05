//! 更新算子
//!
//! 支持双路径执行：
//! - 事务路径（enable_transaction=true）：保证 ACID，通过 WAL + MVCC
//! - 非事务路径（enable_transaction=false）：高性能直接写入，跳过 WAL/MVCC

use log::{error, warn, info, debug, trace};

use crate::common::error::{Result, EngramDbError};
use crate::sql::ast::Expression;
use crate::executor::operators;
use crate::executor::vector::DataChunk;
use crate::storage::Database;
use crate::Value;

/// 执行 UPDATE 语句（根据配置选择事务/非事务路径）
///
/// 参数：
/// - `db`: 数据库实例
/// - `table_name`: 表名
/// - `assignments`: SET 子句中的列赋值列表 Vec<(col_idx, Expression)>
/// - `condition`: 可选的 WHERE 条件
///
/// 返回：更新的行数
pub fn execute(
    db: &mut Database,
    table_name: &str,
    assignments: &[(usize, Expression)],
    condition: Option<Expression>,
) -> Result<usize> {
    trace!("update::execute called: table_name={}, assignments_count={}, has_condition={}", 
           table_name, assignments.len(), condition.is_some());
    
    // 防御性检查：事务路径需要 txn_manager 已初始化
    if db.config().enable_transaction {
        debug!("Transaction path enabled, checking txn_manager readiness...");
        
        if !db.txn_manager().is_ready() {
            error!("Transaction manager not ready");
            error!("Config: enable_transaction=true but txn_manager is not initialized");
            error!("This is likely a bug in Database initialization");
            
            // 生产环境降级到非事务路径
            warn!("Falling back to non-transaction path due to txn_manager not ready");
            return execute_without_txn(db, table_name, assignments, condition);
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
    info!("Executing UPDATE: table={}, assignments={}, has_condition={}, path={}", 
          table_name, assignments.len(), condition.is_some(), path);
    
    if db.config().enable_transaction {
        execute_with_txn(db, table_name, assignments, condition)
    } else {
        execute_without_txn(db, table_name, assignments, condition)
    }
}

/// 事务路径执行 UPDATE：保证 ACID
///
/// 流程：
/// 1. 扫描表找出符合条件的行（获取 row_id）
/// 2. 计算每行更新后的新值
/// 3. 开启事务
/// 4. 事务内逐行更新（写 WAL + MVCC）
/// 5. 提交事务（fsync WAL）
/// 6. 将 apply_ops 应用到存储层
fn execute_with_txn(
    db: &mut Database,
    table_name: &str,
    assignments: &[(usize, Expression)],
    condition: Option<Expression>,
) -> Result<usize> {
    debug!("Starting transaction path UPDATE execution...");
    
    // 步骤 1：先收集要更新的行并计算新值（在开启事务之前，避免借用冲突）
    // 引擎分派（M2）：Columnar = Delta 层行（现有语义），Memory = 全部存活行
    let updates = {
        let table = db.get_engine_table_mut(table_name)
            .ok_or_else(|| EngramDbError::TableNotFound(table_name.into()))?;

        let num_cols = table.def().columns.len();
        if num_cols == 0 {
            return Ok(0);
        }

        let col_names: Vec<String> = table.def().columns.iter().map(|c| c.name.clone()).collect();

        let mutable_rows = table.collect_mutable_rows()?;

        // 找出匹配的行，并计算新值
        let mut updates: Vec<(u64, Vec<Value>, Vec<Value>)> = Vec::new();
        // (row_id, old_row, new_row)

        for (row_id, row) in mutable_rows {
            // 评估 WHERE 条件
            if let Some(ref cond) = condition {
                let chunks = rows_to_chunks(&[row.clone()]);
                let filtered = operators::filter::execute(&chunks, cond, &col_names)?;
                if filtered.is_empty() || filtered[0].count == 0 {
                    continue;
                }
            }
            
            // 计算每个 SET 列的新值
            let mut new_row = row.clone();
            let mut updated = false;
            
            for &(col_idx, ref expr) in assignments {
                // 简单表达式求值：通过 projection 模块
                let chunks = rows_to_chunks(&[row.clone()]);
                let result = operators::projection::execute(
                    &chunks,
                    &[expr.clone()],
                    &col_names,
                    &[],  // output column_names（DataChunk 不使用）
                )?;
                if !result.is_empty() && result[0].count > 0 {
                    let val = result[0].columns[0].get(0).clone();
                    if col_idx < new_row.len() {
                        new_row[col_idx] = val;
                        updated = true;
                    }
                }
            }
            
            if updated {
                updates.push((row_id, row, new_row));
            }
        }
        
        updates
    };
    
    if updates.is_empty() {
        debug!("No rows to update in table '{}'", table_name);
        return Ok(0);
    }
    
    debug!("Found {} rows to update in Delta layer", updates.len());
    
    // 步骤 2：开启事务
    debug!("Beginning transaction...");
    let isolation = db.config().default_isolation_level;
    let txn_id = db.txn_manager_mut().begin(isolation)?;
    info!("Transaction started: txn_id={}, isolation={:?}", txn_id, isolation);
    
    // 步骤 3：事务内更新每行
    let table_id = *db.table_names().get(table_name).unwrap();
    
    for (idx, (row_id, old_row, new_row)) in updates.iter().enumerate() {
        trace!("Updating row {} (row_id={})", idx, row_id);
        
        db.txn_manager_mut().update(txn_id, table_id, *row_id, old_row.clone(), new_row.clone())?;
        
        if idx % 100 == 0 {
            debug!("Updated {}/{} rows in transaction {}", idx + 1, updates.len(), txn_id);
        }
    }
    debug!("✓ All {} rows updated in transaction {}", updates.len(), txn_id);
    
    // 步骤 4：提交事务（会 fsync WAL）
    debug!("Committing transaction {}...", txn_id);
    let result = db.txn_manager_mut().commit(txn_id)?;
    info!("Transaction {} committed: commit_ts={}, apply_ops_count={}",
          txn_id, result.commit_ts, result.apply_ops.len());
    
    // 步骤 5：应用到存储层
    debug!("Applying {} operations to storage...", result.apply_ops.len());
    let applied_count = result.apply_ops.len();
    operators::insert::apply_to_storage(db, result.apply_ops)?;
    info!("✓ Applied {} operations to storage", applied_count);
    
    info!("Transaction path completed: {} rows updated", updates.len());
    Ok(updates.len())
}

/// 非事务路径执行 UPDATE：高性能直接写入
///
/// 直接调用 table.update_delta_rows()，跳过 WAL 和 MVCC
fn execute_without_txn(
    db: &mut Database,
    table_name: &str,
    assignments: &[(usize, Expression)],
    condition: Option<Expression>,
) -> Result<usize> {
    debug!("Starting non-transaction path UPDATE execution...");
    
    let engine = db.get_engine_table_mut(table_name)
        .ok_or_else(|| EngramDbError::TableNotFound(table_name.into()))?;

    let num_cols = engine.def().columns.len();
    if num_cols == 0 {
        return Ok(0);
    }
    let col_names: Vec<String> = engine.def().columns.iter().map(|c| c.name.clone()).collect();

    // M3：Log 引擎 —— 追加式引擎不支持 UPDATE
    if let crate::storage::engine::EngineTable::Log(_) = engine {
        return Err(EngramDbError::NotSupported(
            "LogEngine 不支持 UPDATE（追加式时间序列引擎）".into(),
        ));
    }

    // M2：Memory 引擎 —— 全表更新（内存表无列存/Delta 之分）
    if let crate::storage::engine::EngineTable::Memory(mem) = engine {
        let all_rows = mem.scan_to_rows_direct(&(0..num_cols).collect::<Vec<usize>>(), None)?;
        let mut count = 0;
        for row in &all_rows {
            if let Some(ref cond) = condition {
                let chunks = rows_to_chunks(&[row.clone()]);
                let filtered = operators::filter::execute(&chunks, cond, &col_names)?;
                if filtered.is_empty() || filtered[0].count == 0 {
                    continue;
                }
            }
            let mut new_vals: Vec<(usize, Value)> = Vec::new();
            for &(col_idx, ref expr) in assignments {
                let chunks = rows_to_chunks(&[row.clone()]);
                let result = operators::projection::execute(
                    &chunks,
                    &[expr.clone()],
                    &col_names,
                    &[],
                )?;
                if !result.is_empty() && result[0].count > 0 {
                    new_vals.push((col_idx, result[0].columns[0].get(0).clone()));
                }
            }
            if !new_vals.is_empty() {
                let mut new_row = row.clone();
                for (ci, v) in &new_vals {
                    if *ci < new_row.len() {
                        new_row[*ci] = v.clone();
                    }
                }
                if let Some(rid) = mem.pk_row_id(&new_row) {
                    mem.update_row(rid, &new_row)?;
                    count += 1;
                }
            }
        }
        info!("Non-transaction path completed: {} rows updated (Memory)", count);
        return Ok(count);
    }

    // Columnar：现有逻辑（Delta 层）
    let table = engine.as_columnar_mut().expect("checked above");
    // 扫描所有列
    let all_col_indices: Vec<usize> = (0..num_cols).collect();
    let all_rows = table.scan(&all_col_indices)?;

    let delta_total = table.delta_store().len();
    let cs_rows = table.def.row_count as usize - delta_total;
    
    // 找出匹配的 Delta 行，并计算新值
    let mut updates: Vec<(usize, Vec<(usize, Value)>)> = Vec::new();
    // (delta_idx, Vec<(col_idx, new_value)>)
    
    for (row_idx, row) in all_rows.iter().enumerate() {
        // 只处理 Delta 层的行
        if row_idx < cs_rows {
            continue;
        }
        let delta_idx = row_idx - cs_rows;
        
        // 评估 WHERE 条件
        if let Some(ref cond) = condition {
            let chunks = rows_to_chunks(&[row.clone()]);
            let filtered = operators::filter::execute(&chunks, cond, &col_names)?;
            if filtered.is_empty() || filtered[0].count == 0 {
                continue;
            }
        }
        
        // 计算每个 SET 列的新值
        let mut new_vals: Vec<(usize, Value)> = Vec::new();
        
        for &(col_idx, ref expr) in assignments {
            // 简单表达式求值
            let chunks = rows_to_chunks(&[row.clone()]);
            let result = operators::projection::execute(
                &chunks,
                &[expr.clone()],
                &col_names,
                &[],
            )?;
            if !result.is_empty() && result[0].count > 0 {
                let val = result[0].columns[0].get(0).clone();
                new_vals.push((col_idx, val));
            }
        }
        
        if !new_vals.is_empty() {
            updates.push((delta_idx, new_vals));
        }
    }
    
    let count = table.update_delta_rows(&updates)?;
    info!("Non-transaction path completed: {} rows updated", count);
    Ok(count)
}

/// 将行数据转换为 DataChunk
fn rows_to_chunks(rows: &[Vec<Value>]) -> Vec<DataChunk> {
    let batch_size = crate::executor::vector::VECTOR_SIZE;
    let mut chunks = Vec::new();
    for batch in rows.chunks(batch_size) {
        chunks.push(DataChunk::from_rows(batch));
    }
    chunks
}