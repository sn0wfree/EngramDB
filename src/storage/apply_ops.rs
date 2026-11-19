//! Applies committed transaction operations to engine tables.

use log::{debug, error, info, trace};

use crate::common::error::{EngramDbError, Result};
use crate::storage::Database;
use crate::txn::ApplyOp;
use crate::Value;

/// Applies committed operations in order, batching adjacent inserts where possible.
pub(crate) fn apply_transaction_ops(db: &mut Database, mut ops: Vec<ApplyOp>) -> Result<()> {
    trace!("apply_transaction_ops called with {} operations", ops.len());

    let mut idx = 0;
    while idx < ops.len() {
        if let ApplyOp::InsertBatch {
            table_id,
            base_row_id,
            columns,
        } = &ops[idx]
        {
            let table = db.tables_mut().get_mut(table_id).ok_or_else(|| {
                error!("Table not found: table_id={}", table_id);
                EngramDbError::TableNotFound(format!("id={}", table_id))
            })?;

            let base = table.def().row_count as u32;
            if *base_row_id == base as u64 && !columns.is_empty() {
                let cols = columns.clone();
                let inserted = table.insert_columns_with_check(cols, true)?;
                debug!("✓ InsertBatch applied: table_id={}, rows={}", table_id, inserted);
            } else {
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
                debug!(
                    "InsertBatch applied (non-aligned): table_id={}, rows={}",
                    table_id, num_rows
                );
            }
            idx += 1;
            continue;
        }

        if let ApplyOp::Insert {
            table_id: start_tid, ..
        } = ops[idx]
        {
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
                let mut rows: Vec<Vec<Value>> = Vec::with_capacity(run_len);
                let mut row_ids: Vec<u32> = Vec::with_capacity(run_len);
                for op in &mut ops[idx..run_end] {
                    if let ApplyOp::Insert {
                        table_id: _,
                        row_id,
                        row,
                    } = op
                    {
                        rows.push(std::mem::take(row));
                        row_ids.push(*row_id as u32);
                    }
                }

                let table = db.tables_mut().get_mut(&start_tid).ok_or_else(|| {
                    error!("Table not found: table_id={}", start_tid);
                    EngramDbError::TableNotFound(format!("id={}", start_tid))
                })?;

                let base = table.def().row_count as u32;
                let contiguous = row_ids.iter().enumerate().all(|(i, &rid)| rid == base + i as u32);

                if contiguous {
                    let inserted = table.insert_with_check(rows, true)?;
                    debug!("Batch Insert applied: table_id={}, rows={}", start_tid, inserted);
                } else {
                    for (i, row) in rows.into_iter().enumerate() {
                        table.insert_row_with_check(row_ids[i], &row, true)?;
                    }
                    debug!(
                        "Insert applied (non-contiguous): table_id={}, count={}",
                        start_tid, run_len
                    );
                }

                idx = run_end;
                continue;
            }
        }

        trace!("Applying operation {}/{}", idx + 1, ops.len());
        let op = &mut ops[idx];

        match op {
            ApplyOp::Insert { table_id, row_id, row } => {
                trace!("Insert: table_id={}, row_id={}, row={:?}", table_id, row_id, row);
                let table = db.tables_mut().get_mut(table_id).ok_or_else(|| {
                    error!("Table not found: table_id={}", table_id);
                    error!("This indicates a bug in collect_apply_ops()");
                    EngramDbError::TableNotFound(format!("id={}", table_id))
                })?;
                table.insert_row_with_check(*row_id as u32, row, true)?;
                debug!("Insert applied: table_id={}, row_id={}", table_id, row_id);
            }
            ApplyOp::InsertBatch {
                table_id,
                base_row_id,
                columns,
            } => {
                let table = db.tables_mut().get_mut(table_id).ok_or_else(|| {
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
                debug!(
                    "InsertBatch applied (fallback): table_id={}, rows={}",
                    table_id, num_rows
                );
            }
            ApplyOp::Update {
                table_id,
                row_id,
                new_row,
            } => {
                trace!(
                    "Update: table_id={}, row_id={}, new_row={:?}",
                    table_id,
                    row_id,
                    new_row
                );
                let table = db.tables_mut().get_mut(table_id).ok_or_else(|| {
                    error!("Table not found: table_id={}", table_id);
                    error!("This indicates a bug in collect_apply_ops()");
                    EngramDbError::TableNotFound(format!("id={}", table_id))
                })?;
                table.update_row(*row_id as u32, new_row)?;
                debug!("Update applied: table_id={}, row_id={}", table_id, row_id);
            }
            ApplyOp::Delete { table_id, row_id } => {
                trace!("Delete: table_id={}, row_id={}", table_id, row_id);
                let table = db.tables_mut().get_mut(table_id).ok_or_else(|| {
                    error!("Table not found: table_id={}", table_id);
                    error!("This indicates a bug in collect_apply_ops()");
                    EngramDbError::TableNotFound(format!("id={}", table_id))
                })?;
                table.delete_row(*row_id as u32)?;
                debug!("Delete applied: table_id={}, row_id={}", table_id, row_id);
            }
        }

        idx += 1;
    }

    info!("Applied {} operations to storage", ops.len());
    Ok(())
}
