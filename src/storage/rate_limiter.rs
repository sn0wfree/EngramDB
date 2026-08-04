/// 滑动窗口限流器（v0.15.0 新增）
///
/// 使用滑动窗口计数器算法，每个 key 在指定时间窗口内最多允许 `limit` 次操作。
///
/// # 设计
///
/// - 每个 key 维护一个 `VecDeque<Instant>` 时间戳队列
/// - `check(key, limit, window)` 时：移除过期时间戳，检查队列长度
/// - `increment(key)` 时：追加当前时间戳
/// - 内存可控：`VecDeque` 最大长度 = `limit`（每个 key 最多存 limit 个时间戳）
/// - 自动清理：`check()` 和 `increment()` 时触发过期清理
///
/// # 使用场景
///
/// - LLM API 限流：`check("user:alice:rpm", 60, 60)` 每分钟最多 60 次
/// - Token 限流：`check("org:acme:tpm", 100000, 60)` 每分钟最多 100K token
/// - 熔断：`check("service:llm:errors", 10, 60)` 每分钟最多 10 次错误
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

/// 限流器统计
#[derive(Debug, Default, Clone)]
pub struct RateLimiterStats {
    pub total_checks: u64,
    pub total_allowed: u64,
    pub total_denied: u64,
    pub active_keys: usize,
}

/// 滑动窗口限流器
pub struct RateLimiter {
    windows: HashMap<String, VecDeque<Instant>>,
    stats: RateLimiterStats,
}

impl RateLimiter {
    /// 创建限流器
    pub fn new() -> Self {
        RateLimiter {
            windows: HashMap::new(),
            stats: RateLimiterStats::default(),
        }
    }

    /// 检查 key 是否允许通过
    ///
    /// - `key`: 限流标识（如 "user:alice:rpm"）
    /// - `limit`: 窗口内允许的最大次数
    /// - `window_secs`: 窗口大小（秒）
    ///
    /// 返回 `true` 表示允许通过，`false` 表示超出限制
    pub fn check(&mut self, key: &str, limit: usize, window_secs: u64) -> bool {
        self.stats.total_checks += 1;
        let now = Instant::now();
        let window = Duration::from_secs(window_secs);

        let queue = self.windows.entry(key.to_string()).or_insert_with(VecDeque::new);

        // 移除窗口外的时间戳
        while let Some(&t) = queue.front() {
            if now.duration_since(t) > window {
                queue.pop_front();
            } else {
                break;
            }
        }

        // 检查是否超出限制
        if queue.len() >= limit {
            self.stats.total_denied += 1;
            false
        } else {
            queue.push_back(now);
            self.stats.total_allowed += 1;
            true
        }
    }

    /// 原子递增计数
    ///
    /// 与 `check()` 不同，`increment()` 总是记录计数，不检查限制。
    /// 用于仅计数不限制的场景（如统计接口调用量）。
    pub fn increment(&mut self, key: &str, window_secs: u64) {
        let now = Instant::now();
        let window = Duration::from_secs(window_secs);

        let queue = self.windows.entry(key.to_string()).or_insert_with(VecDeque::new);

        // 移除过期时间戳
        while let Some(&t) = queue.front() {
            if now.duration_since(t) > window {
                queue.pop_front();
            } else {
                break;
            }
        }

        queue.push_back(now);
    }

    /// 获取指定 key 当前窗口内的计数
    pub fn count(&mut self, key: &str, window_secs: u64) -> usize {
        let now = Instant::now();
        let window = Duration::from_secs(window_secs);

        if let Some(queue) = self.windows.get_mut(key) {
            // 清理过期时间戳
            while let Some(&t) = queue.front() {
                if now.duration_since(t) > window {
                    queue.pop_front();
                } else {
                    break;
                }
            }
            queue.len()
        } else {
            0
        }
    }

    /// 重置指定 key 的计数
    pub fn reset(&mut self, key: &str) {
        self.windows.remove(key);
    }

    /// 清空所有计数
    pub fn clear(&mut self) {
        self.windows.clear();
        self.stats = RateLimiterStats::default();
    }

    /// 获取统计信息
    pub fn stats(&self) -> &RateLimiterStats {
        &self.stats
    }

    /// 当前活跃的 key 数量
    pub fn active_keys(&self) -> usize {
        self.windows.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;

    #[test]
    fn test_rate_limiter_basic() {
        let mut rl = RateLimiter::new();
        // 每分钟最多 3 次
        assert!(rl.check("api:test", 3, 60));
        assert!(rl.check("api:test", 3, 60));
        assert!(rl.check("api:test", 3, 60));
        // 第 4 次应该被拒绝
        assert!(!rl.check("api:test", 3, 60));
    }

    #[test]
    fn test_rate_limiter_different_keys() {
        let mut rl = RateLimiter::new();
        // 不同的 key 独立计数
        assert!(rl.check("key:a", 1, 60));
        assert!(!rl.check("key:a", 1, 60));
        assert!(rl.check("key:b", 1, 60));
        assert!(!rl.check("key:b", 1, 60));
    }

    #[test]
    fn test_rate_limiter_window_expiry() {
        let mut rl = RateLimiter::new();
        // 1 秒窗口，最多 2 次
        assert!(rl.check("api", 2, 1));
        assert!(rl.check("api", 2, 1));
        assert!(!rl.check("api", 2, 1));
        // 等待窗口过期
        sleep(Duration::from_millis(1100));
        // 窗口已过期，应该允许
        assert!(rl.check("api", 2, 1));
    }

    #[test]
    fn test_rate_limiter_increment() {
        let mut rl = RateLimiter::new();
        rl.increment("counter", 60);
        rl.increment("counter", 60);
        assert_eq!(rl.count("counter", 60), 2);
    }

    #[test]
    fn test_rate_limiter_reset() {
        let mut rl = RateLimiter::new();
        rl.increment("key", 60);
        rl.increment("key", 60);
        assert_eq!(rl.count("key", 60), 2);
        rl.reset("key");
        assert_eq!(rl.count("key", 60), 0);
    }

    #[test]
    fn test_rate_limiter_clear() {
        let mut rl = RateLimiter::new();
        rl.increment("a", 60);
        rl.increment("b", 60);
        assert_eq!(rl.active_keys(), 2);
        rl.clear();
        assert_eq!(rl.active_keys(), 0);
    }

    #[test]
    fn test_rate_limiter_count() {
        let mut rl = RateLimiter::new();
        // 空 key 返回 0
        assert_eq!(rl.count("nonexistent", 60), 0);
        // 递增后返回正确计数
        rl.increment("stats", 60);
        assert_eq!(rl.count("stats", 60), 1);
    }

    #[test]
    fn test_rate_limiter_large_limit() {
        let mut rl = RateLimiter::new();
        let limit = 1000;
        // 大批量允许
        for _ in 0..limit {
            assert!(rl.check("bulk", limit, 60));
        }
        // 超出限制
        assert!(!rl.check("bulk", limit, 60));
    }

    #[test]
    fn test_rate_limiter_stats() {
        let mut rl = RateLimiter::new();
        rl.check("a", 1, 60);  // allowed
        rl.check("a", 1, 60);  // denied
        rl.check("b", 1, 60);  // allowed
        let stats = rl.stats();
        assert_eq!(stats.total_checks, 3);
        assert_eq!(stats.total_allowed, 2);
        assert_eq!(stats.total_denied, 1);
    }
}