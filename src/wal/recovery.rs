//! 崩溃恢复
//!
//! 基于 WAL 的 ARIES 风格恢复算法：
//! 1. Analysis Pass：分析 WAL，确定已提交事务、活跃事务、Checkpoint 位置
//! 2. Redo Pass：从 Checkpoint 开始重做所有变更（包括未提交的）
//! 3. Undo Pass：回滚所有未提交的事务（生成 CLR 补偿记录）
//!
//! MVP 阶段：简化实现，聚焦核心流程

use std::collections::{HashMap, HashSet};

use crate::common::error::Result;
use super::{WalRecord, WalRecordType, reader::WalReader};

/// 恢复结果
#[derive(Debug, Default, Clone)]
pub struct RecoveryResult {
    /// 重做的记录数
    pub records_redone: u64,
    /// 回滚的记录数
    pub records_undone: u64,
    /// 已提交的事务数
    pub transactions_committed: u64,
    /// 回滚的事务数
    pub transactions_rolled_back: u64,
    /// Checkpoint LSN
    pub checkpoint_lsn: u64,
    /// 恢复是否成功
    pub success: bool,
}

/// 事务状态（恢复过程中）
#[derive(Debug, Clone, PartialEq, Eq)]
enum TxnRecoveryState {
    Active,
    Committed,
    RolledBack,
}

/// 执行崩溃恢复
///
/// 返回恢复结果统计。实际的数据恢复需要调用方根据 redo_records 和 undo_txns 执行。
pub fn recover(wal_path: &str) -> Result<RecoveryResult> {
    let mut result = RecoveryResult::default();

    // 检查 WAL 文件是否存在
    if !std::path::Path::new(wal_path).exists() {
        result.success = true;
        return Ok(result);
    }

    let mut reader = WalReader::open(wal_path)?;
    let records = reader.read_all()?;

    if records.is_empty() {
        result.success = true;
        return Ok(result);
    }

    // ========== Analysis Pass ==========
    let mut txn_states: HashMap<u32, TxnRecoveryState> = HashMap::new();
    let mut checkpoint_lsn: u64 = 0;
    let mut last_checkpoint_idx: Option<usize> = None;

    for (i, rec) in records.iter().enumerate() {
        match rec.record_type {
            WalRecordType::Begin => {
                txn_states.insert(rec.txn_id, TxnRecoveryState::Active);
            }
            WalRecordType::Commit => {
                txn_states.insert(rec.txn_id, TxnRecoveryState::Committed);
            }
            WalRecordType::Rollback => {
                txn_states.insert(rec.txn_id, TxnRecoveryState::RolledBack);
            }
            WalRecordType::Checkpoint => {
                checkpoint_lsn = rec.lsn;
                last_checkpoint_idx = Some(i);
            }
            _ => {
                // 数据记录（Insert/Update/Delete/Compensation），确保事务在表中
                txn_states.entry(rec.txn_id).or_insert(TxnRecoveryState::Active);
            }
        }
    }

    // 统计
    let committed: HashSet<u32> = txn_states.iter()
        .filter(|(_, s)| **s == TxnRecoveryState::Committed)
        .map(|(id, _)| *id)
        .collect();
    let rolled_back: HashSet<u32> = txn_states.iter()
        .filter(|(_, s)| **s == TxnRecoveryState::RolledBack)
        .map(|(id, _)| *id)
        .collect();
    let active: HashSet<u32> = txn_states.iter()
        .filter(|(_, s)| **s == TxnRecoveryState::Active)
        .map(|(id, _)| *id)
        .collect();

    result.transactions_committed = committed.len() as u64;
    result.transactions_rolled_back = active.len() as u64; // 活跃事务需要回滚
    result.checkpoint_lsn = checkpoint_lsn;

    // ========== Redo Pass ==========
    // 从最后一个 Checkpoint 之后开始重做所有数据记录
    let start_idx = last_checkpoint_idx.map(|i| i + 1).unwrap_or(0);
    for rec in &records[start_idx..] {
        match rec.record_type {
            WalRecordType::Insert | WalRecordType::InsertBatch | WalRecordType::Update | WalRecordType::Delete => {
                result.records_redone += 1;
                // 实际应用变更由调用方完成
                // 这里只统计
            }
            _ => {}
        }
    }

    // ========== Undo Pass ==========
    // 回滚所有未提交且未回滚的事务
    // 逆序扫描，为每个活跃事务的操作生成补偿记录
    for rec in records[start_idx..].iter().rev() {
        if active.contains(&rec.txn_id) {
            match rec.record_type {
                WalRecordType::Insert | WalRecordType::InsertBatch => {
                    // INSERT 的补偿 = DELETE
                    result.records_undone += 1;
                }
                WalRecordType::Update => {
                    // UPDATE 的补偿 = 反向 UPDATE（用旧值）
                    result.records_undone += 1;
                }
                WalRecordType::Delete => {
                    // DELETE 的补偿 = INSERT
                    result.records_undone += 1;
                }
                _ => {}
            }
        }
    }

    // 活跃事务数 = 需要回滚的事务数
    result.transactions_rolled_back = active.len() as u64;
    result.success = true;

    Ok(result)
}

/// 获取需要重做的记录列表（供调用方实际应用）
pub fn get_redo_records(wal_path: &str) -> Result<Vec<WalRecord>> {
    if !std::path::Path::new(wal_path).exists() {
        return Ok(Vec::new());
    }

    let mut reader = WalReader::open(wal_path)?;
    let records = reader.read_all()?;

    // 找出最后一个 Checkpoint 的位置
    let mut last_ckpt_idx: Option<usize> = None;
    for (i, rec) in records.iter().enumerate() {
        if rec.record_type == WalRecordType::Checkpoint {
            last_ckpt_idx = Some(i);
        }
    }

    // 找出所有已提交的事务
    let mut committed = HashSet::new();
    for rec in &records {
        if rec.record_type == WalRecordType::Commit {
            committed.insert(rec.txn_id);
        }
    }

    let start_idx = last_ckpt_idx.map(|i| i + 1).unwrap_or(0);
    let redo: Vec<WalRecord> = records[start_idx..].iter()
        .filter(|r| {
            matches!(r.record_type, WalRecordType::Insert | WalRecordType::InsertBatch | WalRecordType::Update | WalRecordType::Delete)
                && committed.contains(&r.txn_id)
        })
        .cloned()
        .collect();

    Ok(redo)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wal::{WalWriter, WalRecordType, make_insert_payload, make_insert_batch_payload};
    use crate::Value;

    fn tmp(name: &str) -> String {
        let mut p = std::env::temp_dir();
        let tid = format!("{:?}", std::thread::current().id())
            .replace('(', "_").replace(')', "")
            .replace([':', ' '], "_");
        p.push(format!("engramdb_wal_{}_{}_{}.hdb-wal", name, std::process::id(), tid));
        p.to_string_lossy().to_string()
    }

    #[test]
    fn test_recover_empty_wal() {
        let result = recover("/tmp/nonexistent_wal.hdb-wal").unwrap();
        assert_eq!(result.records_redone, 0);
        assert_eq!(result.transactions_committed, 0);
        assert!(result.success);
    }

    #[test]
    fn test_recover_committed_txn() {
        let tmp = tmp("recover_committed");
        let _ = std::fs::remove_file(&tmp);

        {
            let mut writer = WalWriter::open(&tmp).unwrap();
            writer.write_record(WalRecordType::Begin, 1, 0, &[]).unwrap();
            let payload = make_insert_payload(1, &[Value::Int64(42)]);
            writer.write_record(WalRecordType::Insert, 1, 1, &payload).unwrap();
            writer.write_record(WalRecordType::Commit, 1, 0, &[]).unwrap();
            writer.sync().unwrap();
        }

        let result = recover(&tmp).unwrap();
        assert_eq!(result.transactions_committed, 1);
        assert_eq!(result.transactions_rolled_back, 0);
        assert_eq!(result.records_redone, 1);
        assert_eq!(result.records_undone, 0);
        assert!(result.success);

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn test_recover_aborted_txn() {
        let tmp = tmp("recover_aborted");
        let _ = std::fs::remove_file(&tmp);

        {
            let mut writer = WalWriter::open(&tmp).unwrap();
            writer.write_record(WalRecordType::Begin, 1, 0, &[]).unwrap();
            let payload = make_insert_payload(1, &[Value::Int64(42)]);
            writer.write_record(WalRecordType::Insert, 1, 1, &payload).unwrap();
            // 没有 Commit — 模拟崩溃
            writer.flush().unwrap();
        }

        let result = recover(&tmp).unwrap();
        assert_eq!(result.transactions_committed, 0);
        assert_eq!(result.transactions_rolled_back, 1);
        assert_eq!(result.records_redone, 1); // Redo 阶段重做了
        assert_eq!(result.records_undone, 1);  // Undo 阶段回滚了
        assert!(result.success);

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn test_recover_insert_batch_committed() {
        // P-W2a：InsertBatch 记录应被 Redo 识别并计入
        let tmp = tmp("recover_insert_batch");
        let _ = std::fs::remove_file(&tmp);

        {
            let mut writer = WalWriter::open(&tmp).unwrap();
            writer.write_record(WalRecordType::Begin, 1, 0, &[]).unwrap();
            let payload = make_insert_batch_payload(0, &[
                vec![Value::Int64(1), Value::Varchar("a".into())],
                vec![Value::Int64(2), Value::Varchar("b".into())],
                vec![Value::Int64(3), Value::Varchar("c".into())],
            ]);
            writer.write_record(WalRecordType::InsertBatch, 1, 1, &payload).unwrap();
            writer.write_record(WalRecordType::Commit, 1, 0, &[]).unwrap();
            writer.sync().unwrap();
        }

        let result = recover(&tmp).unwrap();
        assert_eq!(result.transactions_committed, 1);
        assert_eq!(result.transactions_rolled_back, 0);
        assert_eq!(result.records_redone, 1); // InsertBatch 记录计入 1 条
        assert_eq!(result.records_undone, 0);
        assert!(result.success);

        // get_redo_records 也应包含 InsertBatch
        let redo = get_redo_records(&tmp).unwrap();
        assert_eq!(redo.len(), 1);
        assert_eq!(redo[0].record_type, WalRecordType::InsertBatch);

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn test_recover_mixed_txns() {
        let tmp = tmp("recover_mixed");
        let _ = std::fs::remove_file(&tmp);

        {
            let mut writer = WalWriter::open(&tmp).unwrap();
            // Txn 1: 已提交
            writer.write_record(WalRecordType::Begin, 1, 0, &[]).unwrap();
            writer.write_record(WalRecordType::Insert, 1, 1, &[1]).unwrap();
            writer.write_record(WalRecordType::Commit, 1, 0, &[]).unwrap();
            // Txn 2: 未提交（崩溃）
            writer.write_record(WalRecordType::Begin, 2, 0, &[]).unwrap();
            writer.write_record(WalRecordType::Insert, 2, 1, &[2]).unwrap();
            writer.write_record(WalRecordType::Insert, 2, 1, &[3]).unwrap();
            // 没有 Commit
            writer.flush().unwrap();
        }

        let result = recover(&tmp).unwrap();
        assert_eq!(result.transactions_committed, 1);
        assert_eq!(result.transactions_rolled_back, 1);
        assert_eq!(result.records_redone, 3); // 1 (txn1) + 2 (txn2)
        assert_eq!(result.records_undone, 2);  // txn2 的 2 条
        assert!(result.success);

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn test_recover_multiple_committed_txns() {
        let tmp = tmp("recover_multi_committed");
        let _ = std::fs::remove_file(&tmp);

        {
            let mut writer = WalWriter::open(&tmp).unwrap();
            for i in 1..=5 {
                writer.write_record(WalRecordType::Begin, i, 0, &[]).unwrap();
                writer.write_record(WalRecordType::Insert, i, 1, &[i as u8]).unwrap();
                writer.write_record(WalRecordType::Commit, i, 0, &[]).unwrap();
            }
            writer.sync().unwrap();
        }

        let result = recover(&tmp).unwrap();
        assert_eq!(result.transactions_committed, 5);
        assert_eq!(result.transactions_rolled_back, 0);
        assert_eq!(result.records_redone, 5);
        assert_eq!(result.records_undone, 0);
        assert!(result.success);

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn test_recover_multiple_aborted_txns() {
        let tmp = tmp("recover_multi_aborted");
        let _ = std::fs::remove_file(&tmp);

        {
            let mut writer = WalWriter::open(&tmp).unwrap();
            for i in 1..=3 {
                writer.write_record(WalRecordType::Begin, i, 0, &[]).unwrap();
                writer.write_record(WalRecordType::Insert, i, 1, &[i as u8]).unwrap();
                // 没有 Commit
            }
            writer.flush().unwrap();
        }

        let result = recover(&tmp).unwrap();
        assert_eq!(result.transactions_committed, 0);
        assert_eq!(result.transactions_rolled_back, 3);
        assert_eq!(result.records_redone, 3);
        assert_eq!(result.records_undone, 3);
        assert!(result.success);

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn test_recover_rollback_record() {
        let tmp = tmp("recover_rollback");
        let _ = std::fs::remove_file(&tmp);

        {
            let mut writer = WalWriter::open(&tmp).unwrap();
            writer.write_record(WalRecordType::Begin, 1, 0, &[]).unwrap();
            writer.write_record(WalRecordType::Insert, 1, 1, &[1, 2, 3]).unwrap();
            writer.write_record(WalRecordType::Rollback, 1, 0, &[]).unwrap();
            writer.sync().unwrap();
        }

        let result = recover(&tmp).unwrap();
        // 有 Rollback 记录的事务不算 rolled_back（它已经显式回滚了）
        assert_eq!(result.transactions_committed, 0);
        assert!(result.success);

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn test_get_redo_records_committed() {
        let tmp = tmp("recover_get_redo_records");
        let _ = std::fs::remove_file(&tmp);

        {
            let mut writer = WalWriter::open(&tmp).unwrap();
            writer.write_record(WalRecordType::Begin, 1, 0, &[]).unwrap();
            writer.write_record(WalRecordType::Insert, 1, 1, &[10]).unwrap();
            writer.write_record(WalRecordType::Insert, 1, 1, &[20]).unwrap();
            writer.write_record(WalRecordType::Commit, 1, 0, &[]).unwrap();
            writer.sync().unwrap();
        }

        let redo = get_redo_records(&tmp).unwrap();
        // 只有 Insert 记录是 redo 数据操作
        assert_eq!(redo.len(), 2);
        assert_eq!(redo[0].payload, vec![10]);
        assert_eq!(redo[1].payload, vec![20]);

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn test_recovery_result_default() {
        let r = RecoveryResult::default();
        assert_eq!(r.records_redone, 0);
        assert_eq!(r.records_undone, 0);
        assert_eq!(r.transactions_committed, 0);
        assert_eq!(r.transactions_rolled_back, 0);
        assert_eq!(r.checkpoint_lsn, 0);
        assert!(!r.success);
    }
}
