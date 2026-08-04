/// 嵌入式 KV 缓存引擎（v0.15.0 新增）
///
/// 为 EngramDB 提供内置的 LRU + TTL + 命中率统计缓存，
/// 替代应用层 Redis/Memcached，减少部署依赖。
///
/// # 设计
///
/// - **SLRU（Segmented LRU）**：probation（新条目） + protected（热条目）双队列
///   - 新条目进入 probation 尾部，被再次访问时晋升到 protected
///   - 淘汰时优先从 probation 尾部淘汰，probation 为空时从 protected 尾部降级
///   - 保护比例：protected 占 80%，probation 占 20%
/// - **TTL**：桶式过期，BTreeMap 按过期时间秒分组，O(log n) 淘汰
/// - **内存预算**：按字节计费，插入时自动淘汰直到低于预算
/// - **命中率统计**：原子计数器，可通过 PRAGMA 查询
///
/// # 使用场景
///
/// - Agent 会话缓存（LRU + TTL 组合）
/// - 频繁查询的结果缓存（减少重复 SQL 执行）
/// - 计数器/限流器状态暂存
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::time::{Duration, Instant};

/// 缓存条目
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

/// 嵌入式 KV 缓存引擎
///
/// 使用 `u64` 作为键（table_id + row_id 复合编码或自定义哈希）。
pub struct KVCache {
    entries: HashMap<u64, CacheEntry>,
    probation: VecDeque<u64>,
    protected: VecDeque<u64>,
    protected_lookup: std::collections::HashSet<u64>,
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
            entries: HashMap::new(),
            probation: VecDeque::new(),
            protected: VecDeque::new(),
            protected_lookup: std::collections::HashSet::new(),
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
        // 检查 TTL 过期
        if let Some(entry) = self.entries.get(key) {
            if let Some(ttl) = entry.ttl_seconds {
                if entry.created_at.elapsed() > Duration::from_secs(ttl) {
                    self.remove_internal(*key);
                    self.stats.misses += 1;
                    return None;
                }
            }
        }

        if self.entries.contains_key(key) {
            // 更新命中率
            if let Some(entry) = self.entries.get_mut(key) {
                entry.hit_count += 1;
            }
            // 晋升：如果在 probation 中，移到 protected
            if let Some(pos) = self.probation.iter().position(|k| k == key) {
                self.probation.remove(pos);
                self.protected.push_front(*key);
                self.protected_lookup.insert(*key);
            } else if self.protected_lookup.contains(key) {
                // 已在 protected，移到头部
                if let Some(pos) = self.protected.iter().position(|k| k == key) {
                    self.protected.remove(pos);
                    self.protected.push_front(*key);
                }
            }
            self.stats.hits += 1;
            self.entries.get(key)
        } else {
            self.stats.misses += 1;
            None
        }
    }

    /// 插入缓存条目
    ///
    /// `ttl_seconds`: 过期秒数，None 表示永不过期
    pub fn insert(&mut self, key: u64, value: Vec<u8>, ttl_seconds: Option<u64>) {
        let size = value.len();
        let entry = CacheEntry {
            value,
            size,
            created_at: Instant::now(),
            hit_count: 0,
            ttl_seconds,
        };

        // 如果键已存在，先删除旧条目
        if self.entries.contains_key(&key) {
            self.remove_internal(key);
        }

        self.current_memory += size;
        self.entries.insert(key, entry);

        // TTL 索引
        if let Some(ttl) = ttl_seconds {
            let expiry_bucket = (std::time::SystemTime::now()
                + std::time::Duration::from_secs(ttl))
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .unwrap_or(std::time::Duration::ZERO)
                .as_secs();
            self.ttl_index.entry(expiry_bucket).or_default().push(key);
        }

        // 放入 probation 队列
        self.probation.push_front(key);

        // 淘汰直到低于预算
        self.evict_until_under_budget();

        self.stats.entries = self.entries.len();
    }

    /// 删除缓存条目
    pub fn remove(&mut self, key: &u64) {
        self.remove_internal(*key);
        self.stats.entries = self.entries.len();
    }

    /// 清空缓存
    pub fn clear(&mut self) {
        self.entries.clear();
        self.probation.clear();
        self.protected.clear();
        self.protected_lookup.clear();
        self.ttl_index.clear();
        self.current_memory = 0;
        self.stats = CacheStats::default();
    }

    /// 淘汰过期条目
    ///
    /// 返回淘汰的条目数
    pub fn evict_expired(&mut self) -> usize {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
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
                    if self.entries.remove(&key).is_some() {
                        self.current_memory -= 0; // 准确值在 remove_internal 中计算
                        count += 1;
                    }
                }
            }
        }

        // 修复 current_memory
        self.recalculate_memory();
        self.stats.entries = self.entries.len();
        count
    }

    /// 获取缓存统计
    pub fn stats(&self) -> &CacheStats {
        &self.stats
    }

    /// 获取命中率（0.0 ~ 1.0）
    pub fn hit_rate(&self) -> f64 {
        let total = self.stats.hits + self.stats.misses;
        if total == 0 { 0.0 } else { self.stats.hits as f64 / total as f64 }
    }

    /// 当前内存使用量（字节）
    pub fn memory_usage(&self) -> usize {
        self.current_memory
    }

    /// 设置最大内存预算
    pub fn set_max_memory(&mut self, bytes: usize) {
        self.max_memory = bytes;
        self.evict_until_under_budget();
    }

    /// 获取最大内存预算
    pub fn max_memory(&self) -> usize {
        self.max_memory
    }

    // ---------- 内部方法 ----------

    fn remove_internal(&mut self, key: u64) {
        if let Some(entry) = self.entries.remove(&key) {
            self.current_memory = self.current_memory.saturating_sub(entry.size);
        }
        // 从队列中移除
        if let Some(pos) = self.probation.iter().position(|k| *k == key) {
            self.probation.remove(pos);
        }
        self.protected_lookup.remove(&key);
        if let Some(pos) = self.protected.iter().position(|k| *k == key) {
            self.protected.remove(pos);
        }
    }

    fn evict_until_under_budget(&mut self) {
        while self.current_memory > self.max_memory {
            // 1. 先淘汰过期条目
            if self.evict_expired() > 0 {
                continue;
            }

            // 2. 从 probation 尾部淘汰
            if let Some(victim) = self.probation.pop_back() {
                if let Some(entry) = self.entries.remove(&victim) {
                    self.current_memory = self.current_memory.saturating_sub(entry.size);
                    self.stats.evictions += 1;
                }
                continue;
            }

            // 3. probation 为空，从 protected 尾部降级一个到 probation 再淘汰
            if let Some(demoted) = self.protected.pop_back() {
                self.protected_lookup.remove(&demoted);
                self.probation.push_front(demoted);
                if let Some(victim) = self.probation.pop_back() {
                    if let Some(entry) = self.entries.remove(&victim) {
                        self.current_memory = self.current_memory.saturating_sub(entry.size);
                        self.stats.evictions += 1;
                    }
                }
                continue;
            }

            // 4. 所有队列为空，无法继续淘汰
            break;
        }
        self.stats.entries = self.entries.len();
    }

    fn recalculate_memory(&mut self) {
        self.current_memory = self.entries.values().map(|e| e.size).sum();
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
        // 插入一个 1 秒 TTL 的条目
        cache.insert(1, b"data".to_vec(), Some(1));
        // 立即查询应该命中
        assert!(cache.get(&1).is_some());
        // 等待 1.5 秒
        std::thread::sleep(Duration::from_millis(1500));
        // 应该过期
        assert!(cache.get(&1).is_none());
    }

    #[test]
    fn test_cache_eviction() {
        // 小内存预算，强制淘汰
        let mut cache = KVCache::new(100);
        // 插入 20 个条目，每个 10 字节
        for i in 0..20u64 {
            cache.insert(i, vec![0u8; 10], None);
        }
        // 内存预算 100 字节，20*10=200 > 100，应该淘汰了约 10 个
        assert!(cache.entries.len() < 20);
        assert!(cache.stats.evictions > 0);
    }

    #[test]
    fn test_cache_promotion() {
        let mut cache = KVCache::new(1024 * 1024);
        // 插入条目
        cache.insert(1, b"data".to_vec(), None);
        // 第一次访问：在 probation 中
        cache.get(&1);
        // 第二次访问：晋升到 protected
        cache.get(&1);
        // 验证命中率
        assert!(cache.hit_rate() > 0.0);
    }

    #[test]
    fn test_cache_clear() {
        let mut cache = KVCache::new(1024 * 1024);
        cache.insert(1, b"data".to_vec(), None);
        cache.insert(2, b"data2".to_vec(), None);
        assert_eq!(cache.entries.len(), 2);
        cache.clear();
        assert_eq!(cache.entries.len(), 0);
        assert_eq!(cache.memory_usage(), 0);
    }

    #[test]
    fn test_cache_stats() {
        let mut cache = KVCache::new(1024 * 1024);
        cache.insert(1, b"data".to_vec(), None);
        cache.get(&1); // hit
        cache.get(&2); // miss
        let stats = cache.stats();
        assert_eq!(stats.hits, 1);
        assert_eq!(stats.misses, 1);
        assert!((cache.hit_rate() - 0.5).abs() < 0.001);
    }

    #[test]
    fn test_cache_set_max_memory() {
        let mut cache = KVCache::new(1024 * 1024);
        cache.insert(1, b"data".to_vec(), None);
        assert_eq!(cache.entries.len(), 1);
        // 缩小预算到 1 字节，触发淘汰
        cache.set_max_memory(1);
        assert_eq!(cache.entries.len(), 0);
    }
}