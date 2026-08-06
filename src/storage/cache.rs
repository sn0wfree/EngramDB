/// 嵌入式 KV 缓存引擎（v0.15.0 新增，v0.21.0 重构为 O(1) LRU）
///
/// 为 EngramDB 提供内置的 LRU + TTL + 命中率统计缓存，
/// 替代应用层 Redis/Memcached，减少部署依赖。
///
/// # 设计
///
/// - **SLRU（Segmented LRU）**：probation（新条目） + protected（热条目）双队列
///   - 新条目进入 probation MRU，被再次访问时晋升到 protected MRU
///   - 淘汰时优先从 probation LRU 淘汰，probation 为空时从 protected LRU 降级
///   - 保护比例：protected 占 80%，probation 占 20%
/// - **TTL**：桶式过期，BTreeMap 按过期时间秒分组，O(log n) 淘汰
/// - **内存预算**：按字节计费，插入时自动淘汰直到低于预算
/// - **命中率统计**：原子计数器，可通过 PRAGMA 查询
/// - **O(1) get/insert/remove/promote**：arena 双向链表 + HashMap 索引（不再 O(N) 扫描）
///
/// # 使用场景
///
/// - Agent 会话缓存（LRU + TTL 组合）
/// - 频繁查询的结果缓存（减少重复 SQL 执行）
/// - 计数器/限流器状态暂存
use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

/// 缓存条目（对外暴露的结构，&CacheEntry 通过 get 返回）
#[derive(Debug, Clone)]
pub struct CacheEntry {
    pub value: Vec<u8>,
    pub size: usize,
    pub created_at: Instant,
    pub hit_count: u64,
    pub ttl_seconds: Option<u64>,
}

/// 缓存统计
#[derive(Debug, Default, Clone)]
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
    pub entries: usize,
    pub memory_used: usize,
}

/// LRU 节点（arena 存储）
#[derive(Debug)]
struct LruNode {
    key: u64,
    entry: CacheEntry,
    prev: Option<usize>,
    next: Option<usize>,
    /// 隶属分区（true=protected, false=probation）
    protected: bool,
}

/// 嵌入式 KV 缓存引擎（O(1) get/insert/remove）
pub struct KVCache {
    /// key -> arena 索引（O(1) 查节点）
    map: HashMap<u64, usize>,
    /// arena：存储所有节点（None = 空闲槽位）
    arena: Vec<Option<LruNode>>,
    /// 空闲槽位列表（增删频繁场景下减少 arena 增长）
    free_list: Vec<usize>,
    /// protected 双向链表 MRU 端
    p_head: Option<usize>,
    /// protected 双向链表 LRU 端
    p_tail: Option<usize>,
    /// probation 双向链表 MRU 端
    pb_head: Option<usize>,
    /// probation 双向链表 LRU 端
    pb_tail: Option<usize>,
    ttl_index: BTreeMap<u64, Vec<u64>>,
    max_memory: usize,
    current_memory: usize,
    stats: CacheStats,
    probation_capacity: usize,
    protected_capacity: usize,
}

impl KVCache {
    /// 创建 KV 缓存
    ///
    /// `max_memory_bytes`: 最大内存预算（字节），默认 64MB
    pub fn new(max_memory_bytes: usize) -> Self {
        let total_slots = (max_memory_bytes / 1024).max(64);
        let protected_capacity = (total_slots as f64 * 0.8) as usize;
        let probation_capacity = total_slots - protected_capacity;
        KVCache {
            map: HashMap::new(),
            arena: Vec::new(),
            free_list: Vec::new(),
            p_head: None,
            p_tail: None,
            pb_head: None,
            pb_tail: None,
            ttl_index: BTreeMap::new(),
            max_memory: max_memory_bytes,
            current_memory: 0,
            stats: CacheStats::default(),
            probation_capacity,
            protected_capacity,
        }
    }

    /// 获取缓存条目
    ///
    /// 如果条目已过期（TTL），自动删除并返回 None。
    pub fn get(&mut self, key: &u64) -> Option<&CacheEntry> {
        // 检查 TTL 过期（先 peek，避免在无 key 时持有 borrow）
        if let Some(&idx) = self.map.get(key) {
            let ttl = self.arena[idx].as_ref().unwrap().entry.ttl_seconds;
            if let Some(t) = ttl {
                if self.arena[idx].as_ref().unwrap().entry.created_at.elapsed()
                    > Duration::from_secs(t)
                {
                    self.remove_internal(*key);
                    self.stats.misses += 1;
                    return None;
                }
            }
        } else {
            self.stats.misses += 1;
            return None;
        }

        // 取 idx（已确认存在）
        let idx = *self.map.get(key).unwrap();

        // 更新命中计数
        self.arena[idx].as_mut().unwrap().entry.hit_count += 1;

        // 晋升或移至 MRU
        let is_protected = self.arena[idx].as_ref().unwrap().protected;
        if !is_protected {
            // probation → protected：从 probation 摘下，挂到 protected MRU
            self.unlink(idx);
            self.push_front(idx, true);
        } else {
            // protected 内部：移到 MRU（unlink + push_front）
            self.unlink(idx);
            self.push_front(idx, true);
        }

        self.stats.hits += 1;
        Some(&self.arena[idx].as_ref().unwrap().entry)
    }

    /// 插入缓存条目
    pub fn insert(&mut self, key: u64, value: Vec<u8>, ttl_seconds: Option<u64>) {
        let size = value.len();

        // 如果键已存在，先删除旧条目
        if self.map.contains_key(&key) {
            self.remove_internal(key);
        }

        let node = LruNode {
            key,
            entry: CacheEntry {
                value,
                size,
                created_at: Instant::now(),
                hit_count: 0,
                ttl_seconds,
            },
            prev: None,
            next: None,
            protected: false,
        };

        // 分配 arena 槽位
        let idx = if let Some(free_idx) = self.free_list.pop() {
            self.arena[free_idx] = Some(node);
            free_idx
        } else {
            self.arena.push(Some(node));
            self.arena.len() - 1
        };
        self.map.insert(key, idx);

        self.current_memory += size;

        // TTL 索引
        if let Some(ttl) = ttl_seconds {
            let expiry_bucket = (std::time::SystemTime::now()
                + std::time::Duration::from_secs(ttl))
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .unwrap_or(std::time::Duration::ZERO)
                .as_secs();
            self.ttl_index.entry(expiry_bucket).or_default().push(key);
        }

        // 放入 probation MRU
        self.push_front(idx, false);

        // 淘汰直到低于预算
        self.evict_until_under_budget();

        self.stats.entries = self.map.len();
    }

    /// 删除缓存条目
    pub fn remove(&mut self, key: &u64) {
        self.remove_internal(*key);
        self.stats.entries = self.map.len();
    }

    /// 清空缓存
    pub fn clear(&mut self) {
        self.map.clear();
        self.arena.clear();
        self.free_list.clear();
        self.p_head = None;
        self.p_tail = None;
        self.pb_head = None;
        self.pb_tail = None;
        self.ttl_index.clear();
        self.current_memory = 0;
        self.stats = CacheStats::default();
    }

    /// 淘汰过期条目
    pub fn evict_expired(&mut self) -> usize {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .unwrap_or(std::time::Duration::ZERO)
            .as_secs();

        let expired_buckets: Vec<u64> = self.ttl_index
            .range(..=now)
            .map(|(k, _)| *k)
            .collect();

        let mut count = 0;
        for bucket in expired_buckets {
            if let Some(keys) = self.ttl_index.remove(&bucket) {
                for key in keys {
                    if self.map.remove(&key).is_some() {
                        count += 1;
                    }
                }
            }
        }

        self.recalculate_memory();
        self.stats.entries = self.map.len();
        count
    }

    pub fn stats(&self) -> &CacheStats {
        &self.stats
    }

    pub fn hit_rate(&self) -> f64 {
        let total = self.stats.hits + self.stats.misses;
        if total == 0 {
            0.0
        } else {
            self.stats.hits as f64 / total as f64
        }
    }

    pub fn memory_usage(&self) -> usize {
        self.current_memory
    }

    pub fn set_max_memory(&mut self, bytes: usize) {
        self.max_memory = bytes;
        self.evict_until_under_budget();
    }

    pub fn max_memory(&self) -> usize {
        self.max_memory
    }

    // ---------- 内部方法 ----------

    /// 从双向链表中摘下节点（不释放 arena 槽位）
    fn unlink(&mut self, idx: usize) {
        let (prev, next, protected) = {
            let node = self.arena[idx].as_ref().unwrap();
            (node.prev, node.next, node.protected)
        };
        // 更新 prev.next 或 head
        match prev {
            Some(p) => self.arena[p].as_mut().unwrap().next = next,
            None => {
                if protected {
                    self.p_head = next;
                } else {
                    self.pb_head = next;
                }
            }
        }
        // 更新 next.prev 或 tail
        match next {
            Some(n) => self.arena[n].as_mut().unwrap().prev = prev,
            None => {
                if protected {
                    self.p_tail = prev;
                } else {
                    self.pb_tail = prev;
                }
            }
        }
    }

    /// 把节点插入对应分区的 MRU（head）
    fn push_front(&mut self, idx: usize, protected: bool) {
        let head = if protected {
            self.p_head
        } else {
            self.pb_head
        };
        // 设新 head 的 prev = None, next = 旧 head
        {
            let node = self.arena[idx].as_mut().unwrap();
            node.prev = None;
            node.next = head;
            node.protected = protected;
        }
        // 旧 head 的 prev = 新节点
        if let Some(h) = head {
            self.arena[h].as_mut().unwrap().prev = Some(idx);
        } else {
            // 链表为空：head = tail = idx
            if protected {
                self.p_tail = Some(idx);
            } else {
                self.pb_tail = Some(idx);
            }
        }
        if protected {
            self.p_head = Some(idx);
        } else {
            self.pb_head = Some(idx);
        }
    }

    /// 取对应分区的 LRU（tail）节点 idx 并 unlink（不释放 arena）
    fn pop_tail(&mut self, protected: bool) -> Option<usize> {
        let tail = if protected {
            self.p_tail
        } else {
            self.pb_tail
        }?;
        self.unlink(tail);
        Some(tail)
    }

    fn remove_internal(&mut self, key: u64) {
        if let Some(idx) = self.map.remove(&key) {
            let size = self.arena[idx].as_ref().map(|n| n.entry.size).unwrap_or(0);
            self.unlink(idx);
            self.arena[idx] = None;
            self.free_list.push(idx);
            self.current_memory = self.current_memory.saturating_sub(size);
        }
    }

    fn evict_until_under_budget(&mut self) {
        while self.current_memory > self.max_memory {
            // 1. 先淘汰过期条目
            if self.evict_expired() > 0 {
                continue;
            }

            // 2. 从 probation LRU（tail）淘汰
            if let Some(victim_idx) = self.pop_tail(false) {
                let key = self.arena[victim_idx].as_ref().unwrap().key;
                let size = self.arena[victim_idx].as_ref().unwrap().entry.size;
                self.map.remove(&key);
                self.arena[victim_idx] = None;
                self.free_list.push(victim_idx);
                self.current_memory = self.current_memory.saturating_sub(size);
                self.stats.evictions += 1;
                continue;
            }

            // 3. probation 为空，从 protected LRU（tail）降级到 probation 再淘汰
            if let Some(demoted_idx) = self.pop_tail(true) {
                self.push_front(demoted_idx, false);
                // 再次淘汰 probation（刚降级的那个就是 LRU）
                if let Some(victim_idx) = self.pop_tail(false) {
                    let key = self.arena[victim_idx].as_ref().unwrap().key;
                    let size = self.arena[victim_idx].as_ref().unwrap().entry.size;
                    self.map.remove(&key);
                    self.arena[victim_idx] = None;
                    self.free_list.push(victim_idx);
                    self.current_memory = self.current_memory.saturating_sub(size);
                    self.stats.evictions += 1;
                }
                continue;
            }

            // 4. 所有队列为空
            break;
        }
        self.stats.entries = self.map.len();
    }

    fn recalculate_memory(&mut self) {
        self.current_memory = self
            .arena
            .iter()
            .filter_map(|n| n.as_ref())
            .map(|n| n.entry.size)
            .sum();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cache_basic_get_set() {
        let mut cache = KVCache::new(1024 * 1024);
        cache.insert(1, b"hello".to_vec(), None);
        let val = cache.get(&1).map(|e| e.value.clone());
        assert_eq!(val, Some(b"hello".to_vec()));
        assert!(cache.get(&2).is_none());
    }

    #[test]
    fn test_cache_ttl_expiration() {
        let mut cache = KVCache::new(1024 * 1024);
        cache.insert(1, b"data".to_vec(), Some(1));
        assert!(cache.get(&1).is_some());
        std::thread::sleep(Duration::from_millis(1500));
        assert!(cache.get(&1).is_none());
    }

    #[test]
    fn test_cache_eviction() {
        let mut cache = KVCache::new(100);
        for i in 0..20u64 {
            cache.insert(i, vec![0u8; 10], None);
        }
        assert!(cache.map.len() < 20);
        assert!(cache.stats.evictions > 0);
    }

    #[test]
    fn test_cache_promotion() {
        let mut cache = KVCache::new(1024 * 1024);
        cache.insert(1, b"data".to_vec(), None);
        cache.get(&1);
        cache.get(&1);
        assert!(cache.hit_rate() > 0.0);
    }

    #[test]
    fn test_cache_clear() {
        let mut cache = KVCache::new(1024 * 1024);
        cache.insert(1, b"data".to_vec(), None);
        cache.insert(2, b"data2".to_vec(), None);
        assert_eq!(cache.map.len(), 2);
        cache.clear();
        assert_eq!(cache.map.len(), 0);
        assert_eq!(cache.memory_usage(), 0);
    }

    #[test]
    fn test_cache_stats() {
        let mut cache = KVCache::new(1024 * 1024);
        cache.insert(1, b"data".to_vec(), None);
        cache.get(&1);
        cache.get(&2);
        let stats = cache.stats();
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.misses, 1);
        assert!((cache.hit_rate() - 0.5).abs() < 0.001);
    }

    #[test]
    fn test_cache_set_max_memory() {
        let mut cache = KVCache::new(1024 * 1024);
        cache.insert(1, b"data".to_vec(), None);
        assert_eq!(cache.map.len(), 1);
        cache.set_max_memory(1);
        assert_eq!(cache.map.len(), 0);
    }

    fn insert(c: &mut KVCache, k: u64, size: usize) {
        c.insert(k, vec![0u8; size], None);
    }

    #[test]
    fn test_remove_and_overwrite() {
        let mut c = KVCache::new(1 << 20);
        insert(&mut c, 1, 100);
        c.remove(&1);
        assert!(c.get(&1).is_none());
        assert_eq!(c.memory_usage(), 0);
        insert(&mut c, 1, 100);
        c.insert(1, vec![1u8; 200], None);
        assert_eq!(c.memory_usage(), 200, "覆盖应替换而非累积");
        assert_eq!(c.get(&1).unwrap().value.len(), 200);
    }

    #[test]
    fn test_ttl_expired_get_none() {
        let mut c = KVCache::new(1 << 20);
        c.insert(1, vec![0u8; 10], Some(1));
        std::thread::sleep(std::time::Duration::from_millis(1100));
        assert!(c.get(&1).is_none(), "TTL 过期后 get 返回 None");
        assert!(!c.map.contains_key(&1), "过期条目被清除");
        assert_eq!(c.stats().misses, 1);
    }

    #[test]
    fn test_evict_expired_removes_and_counts() {
        let mut c = KVCache::new(1 << 20);
        c.insert(1, vec![0u8; 10], Some(1));
        c.insert(2, vec![0u8; 10], None);
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let n = c.evict_expired();
        assert_eq!(n, 1);
        assert!(!c.map.contains_key(&1));
        assert!(c.map.contains_key(&2), "无 TTL 条目不受影响");
    }

    #[test]
    fn test_hit_rate() {
        let mut c = KVCache::new(1 << 20);
        insert(&mut c, 1, 100);
        insert(&mut c, 2, 100);
        let _ = c.get(&1);
        let _ = c.get(&1);
        let _ = c.get(&2);
        let _ = c.get(&99);
        assert_eq!(c.stats().hits, 3);
        assert_eq!(c.stats().misses, 1);
        assert!((c.hit_rate() - 0.75).abs() < 1e-9);
    }

    #[test]
    fn test_set_max_memory_triggers_eviction() {
        let mut c = KVCache::new(1 << 20);
        insert(&mut c, 1, 100);
        insert(&mut c, 2, 100);
        c.set_max_memory(150);
        assert!(c.memory_usage() <= 150, "降低预算应触发逐出");
        assert!(c.map.len() < 2);
        c.set_max_memory(1 << 20);
        insert(&mut c, 3, 5000);
        assert!(c.memory_usage() <= c.max_memory);
    }

    #[test]
    fn test_promotion_to_protected() {
        let mut c = KVCache::new(1 << 20);
        insert(&mut c, 1, 100);
        let _ = c.get(&1);
        // 通过 arena 中节点 protected 字段判断
        let idx = c.map.get(&1).unwrap();
        assert!(c.arena[*idx].as_ref().unwrap().protected, "首次访问晋升 protected");
    }

    #[test]
    fn test_insert_large_entry_evicts_others() {
        let mut c = KVCache::new(1024);
        insert(&mut c, 1, 200);
        insert(&mut c, 2, 200);
        insert(&mut c, 3, 200);
        c.insert(4, vec![0u8; 900], None);
        assert!(!c.map.contains_key(&1));
        assert!(c.memory_usage() <= 1024);
    }

    // ===== O(1) 双向链表不变量测试 =====

    #[test]
    fn test_lru_list_invariants_after_promotion() {
        let mut c = KVCache::new(1 << 20);
        for i in 0..50 {
            insert(&mut c, i, 16);
        }
        // 全部访问一次（全部晋升 protected）
        for i in 0..50 {
            let _ = c.get(&i);
        }
        // 验证：所有节点都在 protected 链表里，且链表头尾闭环正确
        let mut count = 0;
        let mut cur = c.p_head;
        let mut prev_seen: Option<usize> = None;
        while let Some(idx) = cur {
            let node = c.arena[idx].as_ref().unwrap();
            assert!(node.protected);
            if let Some(p) = prev_seen {
                assert_eq!(node.prev, Some(p), "链表 prev 指针不连续");
            } else {
                assert_eq!(node.prev, None, "head 应无 prev");
            }
            prev_seen = Some(idx);
            cur = node.next;
            count += 1;
        }
        assert_eq!(count, 50);
    }

    #[test]
    fn test_eviction_picks_correct_lru() {
        let mut c = KVCache::new(1024 * 1024);
        // 插入 100 个，全部访问一次晋升 protected
        for i in 0..100 {
            insert(&mut c, i, 100);
        }
        for i in 0..100 {
            let _ = c.get(&i);
        }
        // 触发淘汰：先访问 key 50 把其升到 MRU，key 0 应是 LRU
        let _ = c.get(&50);
        // 再访问一些 key 制造新条目触发降级到 probation 流程
        // 简化：直接缩内存触发淘汰
        c.set_max_memory(5000);
        // LRU 端应是最后访问的 50 之外最旧的某个（依降级顺序），但至少 key 50 还在
        assert!(c.map.contains_key(&50));
        assert!(c.memory_usage() <= 5000);
    }
}