//! 删除算子
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

/// 执行 DELETE 语句（根据配置选择事务/非事务路径）
///
/// 参数：
/// - `db`: 数据库实例
/// - `table_name`: 表名
/// - `condition`: 可选的 WHERE 条件
///
/// 返回：删除的行数
pub fn execute(
    db: &mut Database,
    table_name: &str,
    condition: Option<Expression>,
) -> Result<usize> {
    trace!("delete::execute called: table_name={}, has_condition={}", table_name, condition.is_some());
    
    // 防御性检查：事务路径需要 txn_manager 已初始化
    if db.config().enable_transaction {
        debug!("Transaction path enabled, checking txn_manager readiness...");
        
        if !db.txn_manager().is_ready() {
            error!("Transaction manager not ready");
            error!("Config: enable_transaction=true but txn_manager is not initialized");
            error!("This is likely a bug in Database initialization");
            
            // 生产环境降级到非事务路径
            warn!("Falling back to non-transaction path due to txn_manager not ready");
            return execute_without_txn(db, table_name, condition);
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
    info!("Executing DELETE: table={}, has_condition={}, path={}", table_name, condition.is_some(), path);
    
    if db.config().enable_transaction {
        execute_with_txn(db, table_name, condition)
    } else {
        execute_without_txn(db, table_name, condition)
    }
}

/// 事务路径执行 DELETE：保证 ACID
///
/// 流程：
/// 1. 扫描表找出符合条件的行（获取 row_id）
/// 2. 开启事务
/// 3. 事务内逐行删除（写 WAL + MVCC）
/// 4. 提交事务（fsync WAL）
/// 5. 将 apply_ops 应用到存储层
fn execute_with_txn(
    db: &mut Database,
    table_name: &str,
    condition: Option<Expression>,
) -> Result<usize> {
    debug!("Starting transaction path DELETE execution...");
    
    // 步骤 1：先收集要删除的行（在开启事务之前，避免借用冲突）
    // 引擎分派（M2）：Columnar = Delta 层行（现有语义），Memory = 全部存活行
    let rows_to_delete = {
        let table = db.get_engine_table_mut(table_name)
            .ok_or_else(|| EngramDbError::TableNotFound(table_name.into()))?;

        let num_cols = table.def().columns.len();
        if num_cols == 0 {
            return Ok(0);
        }

        let col_names: Vec<String> = table.def().columns.iter().map(|c| c.name.clone()).collect();

        let mutable_rows = table.collect_mutable_rows()?;

        let mut rows_to_delete: Vec<(u64, Vec<Value>)> = Vec::new();

        for (row_id, row) in mutable_rows {
            // 评估 WHERE 条件
            if let Some(ref cond) = condition {
                let chunks = rows_to_chunks(&[row.clone()]);
                let filtered = operators::filter::execute(&chunks, cond, &col_names)?;
                if filtered.is_empty() || filtered[0].count == 0 {
                    continue;
                }
            }

            rows_to_delete.push((row_id, row));
        }

        rows_to_delete
    };
    
    if rows_to_delete.is_empty() {
        debug!("No rows to delete in table '{}'", table_name);
        return Ok(0);
    }
    
    debug!("Found {} rows to delete in Delta layer", rows_to_delete.len());
    
    // 步骤 2：开启事务
    debug!("Beginning transaction...");
    let isolation = db.config().default_isolation_level;
    let txn_id = db.txn_manager_mut().begin(isolation)?;
    info!("Transaction started: txn_id={}, isolation={:?}", txn_id, isolation);
    
    // 步骤 3：事务内删除每行
    let table_id = *db.table_names().get(table_name).unwrap();
    
    for (idx, (row_id, old_row)) in rows_to_delete.iter().enumerate() {
        trace!("Deleting row {} (row_id={})", idx, row_id);
        
        db.txn_manager_mut().delete(txn_id, table_id, *row_id, old_row.clone())?;
        
        if idx % 100 == 0 {
            debug!("Deleted {}/{} rows in transaction {}", idx + 1, rows_to_delete.len(), txn_id);
        }
    }
    debug!("✓ All {} rows marked for deletion in transaction {}", rows_to_delete.len(), txn_id);
    
    // 步骤 4：提交事务（会 fsync WAL）
    debug!("Committing transaction {}...", txn_id);
    let result = db.txn_manager_mut().commit(txn_id)?;
    info!("Transaction {} committed: commit_ts={}, apply_ops_count={}",
          txn_id, result.commit_ts, result.apply_ops.len());
    
    // 步骤 5：应用到存储层；失败必须 abort 事务（清理 MVCC 残留）
    debug!("Applying {} operations to storage...", result.apply_ops.len());
    if let Err(e) = operators::insert::apply_to_storage(db, result.apply_ops) {
        let _ = db.txn_manager_mut().rollback(txn_id);
        return Err(e);
    }
    info!("✓ Applied {} operations to storage", rows_to_delete.len());
    
    info!("Transaction path completed: {} rows deleted", rows_to_delete.len());
    Ok(rows_to_delete.len())
}

/// 非事务路径执行 DELETE：高性能直接写入
///
/// 直接调用 table.delete_delta_rows()，跳过 WAL 和 MVCC
fn execute_without_txn(
    db: &mut Database,
    table_name: &str,
    condition: Option<Expression>,
) -> Result<usize> {
    debug!("Starting non-transaction path DELETE execution...");
    
    let engine = db.get_engine_table_mut(table_name)
        .ok_or_else(|| EngramDbError::TableNotFound(table_name.into()))?;

    let num_cols = engine.def().columns.len();
    if num_cols == 0 {
        return Ok(0);
    }
    let col_names: Vec<String> = engine.def().columns.iter().map(|c| c.name.clone()).collect();

    // M3：Log 引擎 —— 追加式引擎不支持 DELETE
    if let crate::storage::engine::EngineTable::Log(_) = engine {
        return Err(EngramDbError::NotSupported(
            "LogEngine 不支持 DELETE（追加式时间序列引擎）".into(),
        ));
    }

    // M2：Memory 引擎 —— 全表删除（内存表无列存/Delta 之分）
    if let crate::storage::engine::EngineTable::Memory(mem) = engine {
        let rows = mem.scan_to_rows_direct(&(0..num_cols).collect::<Vec<usize>>(), None)?;
        let mut count = 0;
        for row in &rows {
            if let Some(ref cond) = condition {
                let chunks = rows_to_chunks(&[row.clone()]);
                let filtered = operators::filter::execute(&chunks, cond, &col_names)?;
                if filtered.is_empty() || filtered[0].count == 0 {
                    continue;
                }
            }
            // 按主键定位 row_id（内存表主键点查）
            let row_id = match mem.pk_row_id(row) {
                Some(rid) => rid,
                None => continue,
            };
            mem.delete_row(row_id)?;
            count += 1;
        }
        info!("Non-transaction path completed: {} rows deleted (Memory)", count);
        return Ok(count);
    }

    // Columnar：现有逻辑（Delta 层）
    let table = engine.as_columnar_mut().expect("checked above");
    // 扫描所有列（用于评估 WHERE 条件）
    let all_col_indices: Vec<usize> = (0..num_cols).collect();
    let all_rows = table.scan(&all_col_indices)?;

    // 计算列存的行数（用于区分列存行和 Delta 行）
    let delta_total = table.delta_store().len();
    let cs_rows = table.def.row_count as usize - delta_total;

    // 如果没有 WHERE 条件，删除所有 Delta 行
    if condition.is_none() {
        let delta_indices: Vec<usize> = (0..delta_total).collect();
        let count = table.delete_delta_rows(&delta_indices)?;
        info!("Non-transaction path completed: {} rows deleted (no condition)", count);
        return Ok(count);
    }

    let cond = condition.unwrap();
    let mut delta_indices_to_delete: Vec<usize> = Vec::new();

    for (row_idx, row) in all_rows.iter().enumerate() {
        // 只处理 Delta 层的行（列存中的行暂不支持删除）
        if row_idx < cs_rows {
            continue;
        }
        let delta_idx = row_idx - cs_rows;

        // 评估 WHERE 条件
        let chunks = rows_to_chunks(&[row.clone()]);
        let filtered = operators::filter::execute(&chunks, &cond, &col_names)?;
        if !filtered.is_empty() && filtered[0].count > 0 {
            delta_indices_to_delete.push(delta_idx);
        }
    }

    let count = table.delete_delta_rows(&delta_indices_to_delete)?;
    info!("Non-transaction path completed: {} rows deleted", count);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::config::Config;
    use crate::sql::ast::{BinaryOperator, Expression};
    use crate::Value;

    fn open_db(txn: bool) -> crate::Connection {
        let mut cfg = Config::default();
        cfg.enable_transaction = txn;
        let mut conn = crate::Connection::open_with_config(":memory:", cfg).unwrap();
        conn.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)").unwrap();
        conn
    }

    fn seed(conn: &mut crate::Connection) {
        conn.execute("INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)").unwrap();
    }

    fn gt_id(n: i32) -> Expression {
        Expression::BinaryOp {
            left: Box::new(Expression::ColumnRef { table: None, column: "id".into() }),
            op: BinaryOperator::Gt,
            right: Box::new(Expression::Literal(Value::Int32(n))),
        }
    }

    fn rows(db: &mut crate::storage::Database) -> Vec<Vec<Value>> {
        db.get_table_mut("t").unwrap().scan_to_rows_direct(&[0, 1]).unwrap()
    }

    #[test]
    fn test_delete_all_direct() {
        let mut conn = open_db(false);
        seed(&mut conn);
        let db = conn.database_mut();
        assert_eq!(execute(db, "t", None).unwrap(), 3);
        assert!(rows(db).is_empty());
    }

    #[test]
    fn test_delete_with_condition_direct() {
        let mut conn = open_db(false);
        seed(&mut conn);
        let db = conn.database_mut();
        assert_eq!(execute(db, "t", Some(gt_id(1))).unwrap(), 2);
        let remaining = rows(db);
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0][0], Value::Int64(1));
        // 再次执行无条件删除
        assert_eq!(execute(db, "t", None).unwrap(), 1);
    }

    #[test]
    fn test_delete_condition_no_match() {
        let mut conn = open_db(false);
        seed(&mut conn);
        let db = conn.database_mut();
        assert_eq!(execute(db, "t", Some(gt_id(100))).unwrap(), 0);
        assert_eq!(rows(db).len(), 3);
    }

    #[test]
    fn test_delete_table_not_found() {
        let mut conn = open_db(false);
        seed(&mut conn);
        let db = conn.database_mut();
        let err = execute(db, "nope", None).unwrap_err();
        assert!(matches!(err, EngramDbError::TableNotFound(_)), "got: {err:?}");
    }

    #[test]
    fn test_delete_log_engine_not_supported() {
        let mut cfg = Config::default();
        cfg.enable_transaction = false;
        let mut conn = crate::Connection::open_with_config(":memory:", cfg).unwrap();
        conn.execute("CREATE TABLE log_t (ts INT64, v INT64) ENGINE = Log").unwrap();
        conn.execute("INSERT INTO log_t VALUES (1, 10)").unwrap();
        let db = conn.database_mut();
        let err = execute(db, "log_t", None).unwrap_err();
        assert!(matches!(err, EngramDbError::NotSupported(_)), "got: {err:?}");
    }

    #[test]
    fn test_delete_memory_engine() {
        let mut cfg = Config::default();
        cfg.enable_transaction = false;
        let mut conn = crate::Connection::open_with_config(":memory:", cfg).unwrap();
        conn.execute("CREATE TABLE mem (id INT PRIMARY KEY, v INT) ENGINE = Memory").unwrap();
        conn.execute("INSERT INTO mem VALUES (1, 10), (2, 20)").unwrap();
        let db = conn.database_mut();
        assert_eq!(execute(db, "mem", Some(gt_id(1))).unwrap(), 1);
        let remaining = db.get_engine_table_mut("mem").unwrap().as_memory_mut().unwrap()
            .scan_to_rows_direct(&[0, 1], None).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0][0], Value::Int64(1));
    }

    #[test]
    fn test_delete_with_txn() {
        let mut conn = open_db(true);
        seed(&mut conn);
        let db = conn.database_mut();
        assert_eq!(execute(db, "t", Some(gt_id(1))).unwrap(), 2);
        assert_eq!(rows(db).len(), 1);
        assert_eq!(execute(db, "t", None).unwrap(), 1);
        assert!(rows(db).is_empty());
    }
}