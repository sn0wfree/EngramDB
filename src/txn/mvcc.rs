//! MVCC (Multi-Version Concurrency Control)
//!
//! 完整的多版本并发控制实现，支持快照隔离 (Snapshot Isolation)
//!
//! 核心概念：
//! - 每个写入操作创建新版本，旧版本保留（直到 GC）
//! - 读操作基于 start_ts 看到一致性快照
//! - 写-写冲突检测（first-committer-wins）
//! - 活跃事务表：跟踪所有未提交事务

use std::collections::{HashMap, HashSet, BTreeMap};

/// 时间戳 / 版本号（单调递增）
pub type Timestamp = u64;

/// 事务 ID
pub type TxnId = u32;

// ============================================================================
// 版本链
// ============================================================================

/// 版本节点
#[derive(Debug, Clone)]
pub struct VersionNode<T> {
    pub value: T,
    pub begin_ts: Timestamp,
    pub end_ts: Option<Timestamp>,
    /// 创建该版本的事务 ID
    pub txn_id: TxnId,
    /// 是否已提交
    pub committed: bool,
}

/// MVCC 键值存储
///
/// 每个 key 维护一个版本链，按时间从旧到新排列
pub struct MvccStore<T> {
    /// key -> version chain (按 begin_ts 升序排列)
    versions: HashMap<u64, Vec<VersionNode<T>>>,
}

impl<T: Clone> MvccStore<T> {
    pub fn new() -> Self {
        Self {
            versions: HashMap::new(),
        }
    }

    /// 读取指定时间戳可见的版本
    /// 可见条件：已提交 && begin_ts <= read_ts && (end_ts.is_none() || end_ts > read_ts)
    pub fn get(&self, key: u64, read_ts: Timestamp) -> Option<&T> {
        let chain = self.versions.get(&key)?;

        // 从新到旧找第一个可见的已提交版本
        for node in chain.iter().rev() {
            if node.committed && node.begin_ts <= read_ts && node.end_ts.map_or(true, |e| e > read_ts) {
                return Some(&node.value);
            }
        }
        None
    }

    /// 读取指定事务可见的版本（含自身未提交的写入）
    pub fn get_for_txn(&self, key: u64, read_ts: Timestamp, txn_id: TxnId) -> Option<&T> {
        let chain = self.versions.get(&key)?;

        // 优先找自己未提交的写入
        for node in chain.iter().rev() {
            if !node.committed && node.txn_id == txn_id {
                return Some(&node.value);
            }
        }

        // 否则按快照读已提交版本
        for node in chain.iter().rev() {
            if node.committed && node.begin_ts <= read_ts && node.end_ts.map_or(true, |e| e > read_ts) {
                return Some(&node.value);
            }
        }
        None
    }

    /// 写入新版本（事务内写入，committed = false 表示未提交）
    /// 返回 true 表示写入成功，false 表示写-写冲突
    pub fn write(&mut self, key: u64, value: T, txn_id: TxnId, write_ts: Timestamp) -> bool {
        let chain = self.versions.entry(key).or_insert_with(Vec::new);

        // 写-写冲突检测：检查链头是否有其他事务的未提交版本
        for node in chain.iter().rev() {
            if !node.committed && node.txn_id != txn_id {
                // 有其他未提交事务的版本，冲突
                return false;
            }
            // 找到第一个已提交版本就可以停了（无冲突）
            if node.committed {
                break;
            }
        }

        chain.push(VersionNode {
            value,
            begin_ts: write_ts,
            end_ts: None,
            txn_id,
            committed: false,
        });

        true
    }

    /// 提交事务的所有写入：标记已提交 + 设置前一版本 end_ts
    pub fn commit_txn(&mut self, txn_id: TxnId, commit_ts: Timestamp) {
        for chain in self.versions.values_mut() {
            let mut prev_committed_idx: Option<usize> = None;

            for i in 0..chain.len() {
                if !chain[i].committed && chain[i].txn_id == txn_id {
                    // 关闭前一个已提交版本
                    if let Some(prev_idx) = prev_committed_idx {
                        chain[prev_idx].end_ts = Some(commit_ts);
                    }
                    // 新版本 begin_ts 更新为 commit_ts，标记已提交
                    chain[i].begin_ts = commit_ts;
                    chain[i].committed = true;
                    prev_committed_idx = Some(i);
                } else if chain[i].committed {
                    prev_committed_idx = Some(i);
                }
                // 其他未提交版本跳过
            }
        }
    }

    /// 回滚事务的所有写入：移除未提交版本
    pub fn rollback_txn(&mut self, txn_id: TxnId) {
        for chain in self.versions.values_mut() {
            chain.retain(|node| node.committed || node.txn_id != txn_id);
        }
    }
    
    /// 检查某个 key 是否在指定事务之前有已提交的版本
    ///
    /// 用于判断操作类型：
    /// - 如果没有旧版本 → Insert
    /// - 如果有旧版本且新版本存在 → Update
    /// - 如果有旧版本但新版本不存在 → Delete
    pub fn has_committed_version_before(&self, key: u64, txn_id: TxnId) -> bool {
        if let Some(chain) = self.versions.get(&key) {
            for node in chain {
                // 检查是否有已提交版本，且不是当前事务创建的
                if node.committed && node.txn_id != txn_id {
                    return true;
                }
            }
        }
        false
    }
    
    /// 获取某个 key 在指定事务的新版本（如果存在）
    ///
    /// 返回新版本的值，用于判断是 Update 还是 Delete
    pub fn get_txn_version(&self, key: u64, txn_id: TxnId) -> Option<&T> {
        if let Some(chain) = self.versions.get(&key) {
            // 找当前事务的版本（已提交或未提交）
            for node in chain.iter().rev() {
                if node.txn_id == txn_id {
                    return Some(&node.value);
                }
            }
        }
        None
    }

    /// 垃圾回收：清理所有 end_ts < oldest_active_ts 的已提交版本
    /// 保留每个 key 最新的已提交版本，以及所有未提交版本
    pub fn gc(&mut self, oldest_active_ts: Timestamp) {
        for chain in self.versions.values_mut() {
            chain.retain(|node| {
                // 未提交版本永远保留
                if !node.committed { return true; }
                // 已提交版本：end_ts 为 None（最新）或 end_ts > oldest_active_ts（仍可见）
                node.end_ts.map_or(true, |e| e > oldest_active_ts)
            });
        }

        // 清理空链
        self.versions.retain(|_, chain| !chain.is_empty());
    }

    /// 获取某个 key 的版本数
    pub fn version_count(&self, key: u64) -> usize {
        self.versions.get(&key).map_or(0, |c| c.len())
    }

    /// 总 key 数
    pub fn key_count(&self) -> usize {
        self.versions.len()
    }

    /// 总版本数
    pub fn total_versions(&self) -> usize {
        self.versions.values().map(|c| c.len()).sum()
    }
}

impl<T: Clone> Default for MvccStore<T> {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// 活跃事务表
// ============================================================================

/// 活跃事务表
///
/// 跟踪所有未提交的事务，用于：
/// - 生成快照（snapshot）
/// - 确定最老活跃事务时间戳（GC 水位线）
/// - 写-写冲突检测
#[derive(Debug, Default)]
pub struct ActiveTxnTable {
    /// txn_id -> start_ts
    active: HashMap<TxnId, Timestamp>,
    /// 下一个事务 ID
    next_txn_id: TxnId,
    /// 下一个时间戳
    next_timestamp: Timestamp,
}

impl ActiveTxnTable {
    pub fn new() -> Self {
        Self {
            active: HashMap::new(),
            next_txn_id: 1,
            next_timestamp: 1,
        }
    }

    /// 分配新的事务 ID 和 start_ts
    pub fn begin_txn(&mut self) -> (TxnId, Timestamp) {
        let txn_id = self.next_txn_id;
        let start_ts = self.next_timestamp;
        self.next_txn_id += 1;
        self.next_timestamp += 1;
        self.active.insert(txn_id, start_ts);
        (txn_id, start_ts)
    }

    /// 提交事务，返回 commit_ts
    pub fn commit_txn(&mut self, txn_id: TxnId) -> Timestamp {
        let commit_ts = self.next_timestamp;
        self.next_timestamp += 1;
        self.active.remove(&txn_id);
        commit_ts
    }

    /// 回滚事务
    pub fn rollback_txn(&mut self, txn_id: TxnId) {
        self.active.remove(&txn_id);
    }

    /// 获取最老的活跃事务时间戳（GC 水位线）
    pub fn oldest_start_ts(&self) -> Option<Timestamp> {
        self.active.values().min().copied()
    }

    /// 活跃事务数
    pub fn active_count(&self) -> usize {
        self.active.len()
    }

    /// 检查事务是否活跃
    pub fn is_active(&self, txn_id: TxnId) -> bool {
        self.active.contains_key(&txn_id)
    }

    /// 获取当前时间戳（用于只读事务的快照）
    pub fn current_ts(&self) -> Timestamp {
        self.next_timestamp - 1
    }

    /// 获取所有活跃事务的 start_ts 集合（用于快照可见性判断）
    pub fn active_snapshot(&self) -> HashSet<Timestamp> {
        self.active.values().copied().collect()
    }
}

// ============================================================================
// 快照
// ============================================================================

/// 事务快照
///
/// 定义了一个事务能看到哪些数据：
/// - 所有 begin_ts <= snapshot_ts 的已提交数据
/// - 所有自己写入的未提交数据
/// - 看不到其他活跃事务的写入
#[derive(Debug, Clone)]
pub struct Snapshot {
    /// 快照时间戳
    pub snapshot_ts: Timestamp,
    /// 本事务 ID
    pub txn_id: TxnId,
    /// 活跃事务的 start_ts 集合（这些事务的写入不可见）
    pub active_txns: HashSet<Timestamp>,
}

impl Snapshot {
    /// 判断某个版本是否可见
    pub fn is_visible(&self, node: &VersionNode<impl Clone>) -> bool {
        // 自己写的未提交版本总是可见
        if !node.committed && node.txn_id == self.txn_id {
            return true;
        }

        // 未提交的其他事务版本不可见
        if !node.committed {
            return false;
        }

        // 已提交版本：begin_ts <= snapshot_ts 且 end_ts > snapshot_ts（或无界）
        // 且不在活跃事务中
        if node.begin_ts > self.snapshot_ts {
            return false;
        }
        if let Some(end_ts) = node.end_ts {
            if end_ts <= self.snapshot_ts {
                return false;
            }
        }

        if self.active_txns.contains(&node.begin_ts) {
            return false;
        }

        true
    }
}

// ============================================================================
// 测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mvcc_basic() {
        let mut store: MvccStore<i32> = MvccStore::new();
        let mut txn_table = ActiveTxnTable::new();

        // Txn 1: 写入
        let (txn1, ts1) = txn_table.begin_txn();
        assert!(store.write(1, 100, txn1, ts1));
        let commit_ts1 = txn_table.commit_txn(txn1);
        store.commit_txn(txn1, commit_ts1);

        // 读验证
        assert_eq!(store.get(1, commit_ts1), Some(&100));
        assert_eq!(store.get(1, ts1), None); // 事务开始时还没提交

        // Txn 2: 更新
        let (txn2, ts2) = txn_table.begin_txn();
        assert!(store.write(1, 200, txn2, ts2));
        let commit_ts2 = txn_table.commit_txn(txn2);
        store.commit_txn(txn2, commit_ts2);

        // 快照读：旧快照看到旧值
        assert_eq!(store.get(1, commit_ts1), Some(&100));
        // 新快照看到新值
        assert_eq!(store.get(1, commit_ts2), Some(&200));
    }

    #[test]
    fn test_mvcc_rollback() {
        let mut store: MvccStore<i32> = MvccStore::new();
        let mut txn_table = ActiveTxnTable::new();

        // Txn 1: 写入并提交
        let (txn1, ts1) = txn_table.begin_txn();
        store.write(1, 100, txn1, ts1);
        let commit_ts1 = txn_table.commit_txn(txn1);
        store.commit_txn(txn1, commit_ts1);

        // Txn 2: 写入但回滚
        let (txn2, ts2) = txn_table.begin_txn();
        store.write(1, 200, txn2, ts2);
        txn_table.rollback_txn(txn2);
        store.rollback_txn(txn2);

        // 仍然只能看到 txn1 的值
        assert_eq!(store.get(1, ts2 + 10), Some(&100));
        assert_eq!(store.version_count(1), 1); // 回滚的版本被移除
    }

    #[test]
    fn test_mvcc_write_write_conflict() {
        let mut store: MvccStore<i32> = MvccStore::new();
        let mut txn_table = ActiveTxnTable::new();

        let (txn1, ts1) = txn_table.begin_txn();
        let (txn2, ts2) = txn_table.begin_txn();

        // Txn 1 先写
        assert!(store.write(1, 100, txn1, ts1));

        // Txn 2 写同一个 key — 应该冲突
        assert!(!store.write(1, 200, txn2, ts2));
    }

    #[test]
    fn test_mvcc_gc() {
        let mut store: MvccStore<i32> = MvccStore::new();
        let mut txn_table = ActiveTxnTable::new();

        // 写入 3 个版本
        for i in 0..3 {
            let (txn_id, ts) = txn_table.begin_txn();
            store.write(1, 100 + i as i32, txn_id, ts);
            let commit_ts = txn_table.commit_txn(txn_id);
            store.commit_txn(txn_id, commit_ts);
        }

        assert_eq!(store.version_count(1), 3);

        // GC：保留最新版本
        let oldest = txn_table.oldest_start_ts().unwrap_or(txn_table.current_ts());
        store.gc(oldest.saturating_sub(1));
        // 所有版本都已提交且 end_ts 可能为 None（最新版本），所以都保留
        assert!(store.version_count(1) >= 1);
    }

    #[test]
    fn test_active_txn_table() {
        let mut table = ActiveTxnTable::new();

        assert_eq!(table.active_count(), 0);

        let (id1, ts1) = table.begin_txn();
        let (id2, ts2) = table.begin_txn();

        assert_eq!(table.active_count(), 2);
        assert!(table.is_active(id1));
        assert!(table.is_active(id2));
        assert_eq!(table.oldest_start_ts(), Some(ts1.min(ts2)));

        table.commit_txn(id1);
        assert_eq!(table.active_count(), 1);
        assert!(!table.is_active(id1));
    }

    #[test]
    fn test_snapshot_visibility() {
        let mut store: MvccStore<i32> = MvccStore::new();
        let mut txn_table = ActiveTxnTable::new();

        // Txn 1: 提交
        let (txn1, ts1) = txn_table.begin_txn();
        store.write(1, 100, txn1, ts1);
        let c1 = txn_table.commit_txn(txn1);
        store.commit_txn(txn1, c1);

        // Txn 2: 活跃（未提交）
        let (txn2, ts2) = txn_table.begin_txn();
        store.write(1, 200, txn2, ts2);

        // Txn 3 的快照
        let (txn3, ts3) = txn_table.begin_txn();
        let snapshot = Snapshot {
            snapshot_ts: ts3,
            txn_id: txn3,
            active_txns: txn_table.active_snapshot(),
        };

        // 检查 txn1 的版本可见性
        let chain = store.versions.get(&1).unwrap();
        // 第一个版本（txn1 提交的）应该可见
        let v1 = &chain[0];
        assert!(snapshot.is_visible(v1));

        // 第二个版本（txn2 未提交的）应该不可见
        if chain.len() > 1 {
            let v2 = &chain[1];
            assert!(!snapshot.is_visible(v2));
        }
    }

    // ===== 扩充测试 =====

    // --- MVCC 基础 ---

    #[test]
    fn test_mvcc_empty_store() {
        let store: MvccStore<i32> = MvccStore::new();
        assert_eq!(store.get(1, 100), None);
        assert_eq!(store.version_count(1), 0);
        assert_eq!(store.key_count(), 0);
        assert_eq!(store.total_versions(), 0);
    }

    #[test]
    fn test_mvcc_multiple_keys() {
        let mut store: MvccStore<i32> = MvccStore::new();
        let mut txn_table = ActiveTxnTable::new();

        let (txn1, ts1) = txn_table.begin_txn();
        assert!(store.write(1, 100, txn1, ts1));
        assert!(store.write(2, 200, txn1, ts1));
        assert!(store.write(3, 300, txn1, ts1));
        let c1 = txn_table.commit_txn(txn1);
        store.commit_txn(txn1, c1);

        assert_eq!(store.key_count(), 3);
        assert_eq!(store.total_versions(), 3);
        assert_eq!(store.get(1, c1), Some(&100));
        assert_eq!(store.get(2, c1), Some(&200));
        assert_eq!(store.get(3, c1), Some(&300));
    }

    #[test]
    fn test_mvcc_read_before_commit_returns_none() {
        let mut store: MvccStore<i32> = MvccStore::new();
        let mut txn_table = ActiveTxnTable::new();

        let (txn1, ts1) = txn_table.begin_txn();
        store.write(1, 100, txn1, ts1);

        // 提交前，其他快照看不到
        assert_eq!(store.get(1, ts1), None);
        assert_eq!(store.get(1, ts1 + 100), None);

        let c1 = txn_table.commit_txn(txn1);
        store.commit_txn(txn1, c1);

        // 提交后可见
        assert_eq!(store.get(1, c1), Some(&100));
    }

    #[test]
    fn test_mvcc_three_versions_snapshot_read() {
        let mut store: MvccStore<i32> = MvccStore::new();
        let mut txn_table = ActiveTxnTable::new();

        let (t1, s1) = txn_table.begin_txn();
        store.write(1, 10, t1, s1);
        let c1 = txn_table.commit_txn(t1);
        store.commit_txn(t1, c1);

        let (t2, s2) = txn_table.begin_txn();
        store.write(1, 20, t2, s2);
        let c2 = txn_table.commit_txn(t2);
        store.commit_txn(t2, c2);

        let (t3, s3) = txn_table.begin_txn();
        store.write(1, 30, t3, s3);
        let c3 = txn_table.commit_txn(t3);
        store.commit_txn(t3, c3);

        assert_eq!(store.version_count(1), 3);
        assert_eq!(store.get(1, c1), Some(&10));
        assert_eq!(store.get(1, c2), Some(&20));
        assert_eq!(store.get(1, c3), Some(&30));
    }

    // --- get_for_txn ---

    #[test]
    fn test_get_for_txn_sees_own_uncommitted() {
        let mut store: MvccStore<i32> = MvccStore::new();
        let mut txn_table = ActiveTxnTable::new();

        let (txn1, ts1) = txn_table.begin_txn();
        store.write(1, 100, txn1, ts1);

        // 自己能看到自己未提交的写入
        assert_eq!(store.get_for_txn(1, ts1, txn1), Some(&100));
        // 普通快照看不到
        assert_eq!(store.get(1, ts1), None);
    }

    #[test]
    fn test_get_for_txn_does_not_see_others_uncommitted() {
        let mut store: MvccStore<i32> = MvccStore::new();
        let mut txn_table = ActiveTxnTable::new();

        let (txn1, ts1) = txn_table.begin_txn();
        store.write(1, 100, txn1, ts1);

        let (txn2, ts2) = txn_table.begin_txn();
        // txn2 看不到 txn1 的未提交写入
        assert_eq!(store.get_for_txn(1, ts2, txn2), None);
    }

    #[test]
    fn test_get_for_txn_sees_committed() {
        let mut store: MvccStore<i32> = MvccStore::new();
        let mut txn_table = ActiveTxnTable::new();

        let (txn1, ts1) = txn_table.begin_txn();
        store.write(1, 100, txn1, ts1);
        let c1 = txn_table.commit_txn(txn1);
        store.commit_txn(txn1, c1);

        let (txn2, ts2) = txn_table.begin_txn();
        // txn2 能看到 txn1 已提交的
        assert_eq!(store.get_for_txn(1, ts2, txn2), Some(&100));
    }

    // --- 写-写冲突 ---

    #[test]
    fn test_write_write_conflict_same_key() {
        let mut store: MvccStore<i32> = MvccStore::new();
        let mut txn_table = ActiveTxnTable::new();

        let (txn1, ts1) = txn_table.begin_txn();
        let (txn2, ts2) = txn_table.begin_txn();

        assert!(store.write(1, 100, txn1, ts1));
        assert!(!store.write(1, 200, txn2, ts2));
    }

    #[test]
    fn test_no_conflict_different_keys() {
        let mut store: MvccStore<i32> = MvccStore::new();
        let mut txn_table = ActiveTxnTable::new();

        let (txn1, ts1) = txn_table.begin_txn();
        let (txn2, ts2) = txn_table.begin_txn();

        // 不同 key 不冲突
        assert!(store.write(1, 100, txn1, ts1));
        assert!(store.write(2, 200, txn2, ts2));
    }

    #[test]
    fn test_same_txn_can_write_same_key_multiple_times() {
        let mut store: MvccStore<i32> = MvccStore::new();
        let mut txn_table = ActiveTxnTable::new();

        let (txn1, ts1) = txn_table.begin_txn();
        assert!(store.write(1, 100, txn1, ts1));
        assert!(store.write(1, 200, txn1, ts1)); // 同一事务多次写同一 key 不冲突
        assert_eq!(store.version_count(1), 2);
    }

    #[test]
    fn test_commit_resolves_conflict_for_next_txn() {
        let mut store: MvccStore<i32> = MvccStore::new();
        let mut txn_table = ActiveTxnTable::new();

        let (txn1, ts1) = txn_table.begin_txn();
        store.write(1, 100, txn1, ts1);
        let c1 = txn_table.commit_txn(txn1);
        store.commit_txn(txn1, c1);

        // txn1 提交后，新事务可以写（不会冲突，因为已提交版本不算"未提交其他事务"）
        let (txn2, ts2) = txn_table.begin_txn();
        assert!(store.write(1, 200, txn2, ts2));
    }

    // --- 回滚 ---

    #[test]
    fn test_rollback_removes_only_uncommitted() {
        let mut store: MvccStore<i32> = MvccStore::new();
        let mut txn_table = ActiveTxnTable::new();

        let (txn1, ts1) = txn_table.begin_txn();
        store.write(1, 100, txn1, ts1);
        let c1 = txn_table.commit_txn(txn1);
        store.commit_txn(txn1, c1);

        let (txn2, ts2) = txn_table.begin_txn();
        store.write(1, 200, txn2, ts2);
        store.rollback_txn(txn2);

        // 只移除了 txn2 的未提交版本，txn1 的版本保留
        assert_eq!(store.version_count(1), 1);
        assert_eq!(store.get(1, c1 + 10), Some(&100));
    }

    #[test]
    fn test_rollback_nonexistent_txn_is_safe() {
        let mut store: MvccStore<i32> = MvccStore::new();
        // 回滚不存在的事务不会 panic
        store.rollback_txn(999);
        assert_eq!(store.key_count(), 0);
    }

    // --- GC ---

    #[test]
    fn test_gc_removes_old_versions() {
        let mut store: MvccStore<i32> = MvccStore::new();
        let mut txn_table = ActiveTxnTable::new();

        // 写入 5 个版本
        for i in 0..5 {
            let (t, s) = txn_table.begin_txn();
            store.write(1, i, t, s);
            let c = txn_table.commit_txn(t);
            store.commit_txn(t, c);
        }

        assert_eq!(store.version_count(1), 5);

        // GC：用一个足够新的时间戳（大于所有 end_ts），应该只保留最新版本（end_ts = None）
        store.gc(100);
        assert_eq!(store.version_count(1), 1);
    }

    #[test]
    fn test_gc_preserves_uncommitted() {
        let mut store: MvccStore<i32> = MvccStore::new();
        let mut txn_table = ActiveTxnTable::new();

        let (t1, s1) = txn_table.begin_txn();
        store.write(1, 10, t1, s1);
        let c1 = txn_table.commit_txn(t1);
        store.commit_txn(t1, c1);

        let (t2, s2) = txn_table.begin_txn();
        store.write(1, 20, t2, s2); // 未提交

        // GC 不应该移除未提交版本
        store.gc(0);
        assert_eq!(store.version_count(1), 2); // 1 已提交 + 1 未提交
    }

    #[test]
    fn test_gc_empty_store() {
        let mut store: MvccStore<i32> = MvccStore::new();
        store.gc(100); // 空 store GC 不 panic
        assert_eq!(store.key_count(), 0);
    }

    // --- ActiveTxnTable ---

    #[test]
    fn test_active_txn_table_monotonic_ids() {
        let mut table = ActiveTxnTable::new();
        let (id1, _) = table.begin_txn();
        let (id2, _) = table.begin_txn();
        let (id3, _) = table.begin_txn();
        assert!(id2 > id1);
        assert!(id3 > id2);
    }

    #[test]
    fn test_active_txn_table_monotonic_timestamps() {
        let mut table = ActiveTxnTable::new();
        let (_, ts1) = table.begin_txn();
        let (_, ts2) = table.begin_txn();
        let c1 = table.commit_txn(1);
        let (_, ts3) = table.begin_txn();
        assert!(ts2 > ts1);
        assert!(c1 > ts2);
        assert!(ts3 > c1);
    }

    #[test]
    fn test_active_txn_table_rollback() {
        let mut table = ActiveTxnTable::new();
        let (id1, _) = table.begin_txn();
        assert_eq!(table.active_count(), 1);
        table.rollback_txn(id1);
        assert_eq!(table.active_count(), 0);
        assert!(!table.is_active(id1));
    }

    #[test]
    fn test_active_txn_table_oldest_start_ts() {
        let mut table = ActiveTxnTable::new();
        assert_eq!(table.oldest_start_ts(), None);

        let (_, ts1) = table.begin_txn();
        let (_, ts2) = table.begin_txn();
        let (_, ts3) = table.begin_txn();

        assert_eq!(table.oldest_start_ts(), Some(ts1));
        assert!(ts2 > ts1);
        assert!(ts3 > ts2);

        // 提交最老的，oldest 应该变成第二个
        table.commit_txn(1);
        assert_eq!(table.oldest_start_ts(), Some(ts2));
    }

    #[test]
    fn test_active_txn_table_current_ts() {
        let mut table = ActiveTxnTable::new();
        // 初始状态 next_timestamp = 1, current = 0
        assert_eq!(table.current_ts(), 0);

        table.begin_txn();
        // begin 后 next_timestamp = 2, current = 1
        assert_eq!(table.current_ts(), 1);

        table.commit_txn(1);
        // commit 后 next_timestamp = 3, current = 2
        assert_eq!(table.current_ts(), 2);
    }

    // --- Snapshot ---

    #[test]
    fn test_snapshot_sees_own_writes() {
        let store: MvccStore<i32> = MvccStore::new();
        let snapshot = Snapshot {
            snapshot_ts: 10,
            txn_id: 5,
            active_txns: HashSet::new(),
        };

        // 自己的未提交版本可见
        let node = VersionNode {
            value: 42,
            begin_ts: 5,
            end_ts: None,
            txn_id: 5,
            committed: false,
        };
        assert!(snapshot.is_visible(&node));
    }

    #[test]
    fn test_snapshot_hides_other_uncommitted() {
        let mut active = HashSet::new();
        active.insert(3);
        let snapshot = Snapshot {
            snapshot_ts: 10,
            txn_id: 5,
            active_txns: active,
        };

        // 其他事务的未提交版本不可见
        let node = VersionNode {
            value: 42,
            begin_ts: 3,
            end_ts: None,
            txn_id: 3,
            committed: false,
        };
        assert!(!snapshot.is_visible(&node));
    }

    #[test]
    fn test_snapshot_sees_committed_before_snapshot() {
        let snapshot = Snapshot {
            snapshot_ts: 10,
            txn_id: 5,
            active_txns: HashSet::new(),
        };

        let node = VersionNode {
            value: 42,
            begin_ts: 5,
            end_ts: None,
            txn_id: 1,
            committed: true,
        };
        assert!(snapshot.is_visible(&node));
    }

    #[test]
    fn test_snapshot_hides_committed_after_snapshot() {
        let snapshot = Snapshot {
            snapshot_ts: 10,
            txn_id: 5,
            active_txns: HashSet::new(),
        };

        let node = VersionNode {
            value: 42,
            begin_ts: 15, // 在快照之后提交
            end_ts: None,
            txn_id: 1,
            committed: true,
        };
        assert!(!snapshot.is_visible(&node));
    }

    #[test]
    fn test_snapshot_hides_active_txn_committed_later() {
        let mut active = HashSet::new();
        active.insert(3); // txn3 在快照生成时还活跃
        let snapshot = Snapshot {
            snapshot_ts: 10,
            txn_id: 5,
            active_txns: active,
        };

        // txn3 在快照时刻是活跃的，即使它后来提交了也不可见
        let node = VersionNode {
            value: 42,
            begin_ts: 3,
            end_ts: None,
            txn_id: 3,
            committed: true,
        };
        assert!(!snapshot.is_visible(&node));
    }

    #[test]
    fn test_snapshot_version_with_end_ts() {
        let snapshot = Snapshot {
            snapshot_ts: 10,
            txn_id: 5,
            active_txns: HashSet::new(),
        };

        // end_ts > snapshot_ts → 版本在快照时仍有效
        let node = VersionNode {
            value: 42,
            begin_ts: 2,
            end_ts: Some(20),
            txn_id: 1,
            committed: true,
        };
        assert!(snapshot.is_visible(&node));

        // end_ts <= snapshot_ts → 版本已过期
        let node2 = VersionNode {
            value: 42,
            begin_ts: 2,
            end_ts: Some(5),
            txn_id: 1,
            committed: true,
        };
        assert!(!snapshot.is_visible(&node2));
    }

    // --- Default ---

    #[test]
    fn test_mvcc_default() {
        let store: MvccStore<i32> = MvccStore::default();
        assert_eq!(store.key_count(), 0);
    }

    #[test]
    fn test_active_table_default() {
        let table = ActiveTxnTable::default();
        assert_eq!(table.active_count(), 0);
    }
}
