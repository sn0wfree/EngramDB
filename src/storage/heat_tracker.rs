//! Phase 2 P0-B：Heat Tracker（热度跟踪）
//!
//! 每表维护访问热度统计：
//! - `access_count`：累计访问次数（读 + 写）
//! - `last_access`：上次访问时间戳（毫秒）
//! - `last_decay`：上次衰减时间
//!
//! 热度计算（`heat_score()`）：
//!   score = access_count * exp(-λ * 距 last_decay 的时间)
//!   λ = ln(2) / half_life_ms（默认 60_000ms，半衰期 1 分钟）
//!
//! 调度决策（外部策略）：
//! - score > 1000 且 row_count < 10_000 → Memory 引擎（高频小表）
//! - score < 1 且 row_count > 100_000 → Log 引擎（冷数据）
//! - 其他 → Columnar（温数据）
//!
//! ## 性能
//! - `record_access()`：O(1)
//! - `heat_score()`：O(1)
//! - `tick_decay()`：每表 O(1)，总 O(N_tables) per call
//!
//! ## 线程安全
//! 当前为单线程（`&mut self`）。后续多线程可通过 `Mutex<HeatTracker>` 包装。

use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// 单表热度记录
#[derive(Debug, Clone, Copy)]
struct HeatEntry {
    access_count: u64,
    last_access_ms: u64,
    last_decay_ms: u64,
}

impl Default for HeatEntry {
    fn default() -> Self {
        let now_ms = current_time_ms();
        Self {
            access_count: 0,
            last_access_ms: now_ms,
            last_decay_ms: now_ms,
        }
    }
}

/// Heat Tracker：跟踪每表访问热度
///
/// 用法：
/// ```ignore
/// let mut tracker = HeatTracker::new();
/// tracker.record_access(table_id);
/// let score = tracker.heat_score(table_id);
/// ```
#[derive(Debug, Default)]
pub struct HeatTracker {
    entries: HashMap<u32, HeatEntry>,
    /// 半衰期（默认 60 秒）
    half_life: Duration,
}

impl HeatTracker {
    /// 默认构造（半衰期 60 秒）
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
            half_life: Duration::from_secs(60),
        }
    }

    /// 自定义半衰期
    pub fn with_half_life(half_life: Duration) -> Self {
        Self {
            entries: HashMap::new(),
            half_life,
        }
    }

    /// 记录一次访问（递增计数）
    pub fn record_access(&mut self, table_id: u32) {
        let now_ms = current_time_ms();
        let entry = self.entries.entry(table_id).or_default();
        entry.access_count = entry.access_count.saturating_add(1);
        entry.last_access_ms = now_ms;
    }

    /// 计算热度分数（带衰减）
    ///
    /// 公式：`score = access_count * 2^(-Δ / half_life)`，其中 Δ = now - last_decay
    ///
    /// 注意：每次调用会更新 last_decay（"惰性衰减"，避免每访问都重算）
    pub fn heat_score(&mut self, table_id: u32) -> f64 {
        let now_ms = current_time_ms();
        let entry = self.entries.entry(table_id).or_default();

        let elapsed_ms = now_ms.saturating_sub(entry.last_decay_ms) as f64;
        let half_life_ms = self.half_life.as_millis() as f64;
        let decay_factor = 2f64.powf(-elapsed_ms / half_life_ms);

        let score = entry.access_count as f64 * decay_factor;
        entry.last_decay_ms = now_ms;
        score
    }

    /// 获取访问次数（不触发衰减）
    pub fn access_count(&self, table_id: u32) -> u64 {
        self.entries.get(&table_id).map(|e| e.access_count).unwrap_or(0)
    }

    /// 重置指定表的热度（迁移后调用，避免热度延续）
    pub fn reset(&mut self, table_id: u32) {
        self.entries.remove(&table_id);
    }

    /// 清空所有热度记录
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// 获取已跟踪的表数
    pub fn tracked_table_count(&self) -> usize {
        self.entries.len()
    }
}

/// 当前时间戳（毫秒，Unix epoch）
fn current_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 调度建议：根据 heat_score 与表大小决定目标引擎
///
/// 返回 Phase 2 P0-B 当前策略（保守）：
/// - 高频小表（score > 100, row_count < 10000）→ Memory
/// - 冷大表（score < 0.5, row_count > 100000）→ Log
/// - 其他 → Columnar（默认）
pub fn suggest_target_engine(score: f64, row_count: u64) -> crate::common::types::EngineType {
    use crate::common::types::EngineType;
    if score > 100.0 && row_count < 10_000 {
        EngineType::Memory
    } else if score < 0.5 && row_count > 100_000 {
        EngineType::Log
    } else {
        EngineType::Columnar
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_record_access_increments_count() {
        let mut tracker = HeatTracker::new();
        tracker.record_access(1);
        tracker.record_access(1);
        tracker.record_access(1);
        assert_eq!(tracker.access_count(1), 3);
    }

    #[test]
    fn test_heat_score_after_many_accesses() {
        let mut tracker = HeatTracker::new();
        for _ in 0..100 {
            tracker.record_access(1);
        }
        // 立即计算：score = 100 * 1.0（无衰减）
        let s = tracker.heat_score(1);
        assert!(s >= 99.0 && s <= 101.0, "score should be ~100, got {}", s);
    }

    #[test]
    fn test_heat_score_decays() {
        let mut tracker = HeatTracker::with_half_life(Duration::from_millis(50));
        tracker.record_access(1);
        tracker.record_access(1);
        // 立即：score ≈ 2
        let s1 = tracker.heat_score(1);
        assert!(s1 >= 1.5);

        // 等待一个半衰期
        std::thread::sleep(Duration::from_millis(60));
        let s2 = tracker.heat_score(1);
        // 经过 ~60ms（半衰期 50ms），衰减到 ~50%
        assert!(s2 < s1, "score should decay: {} -> {}", s1, s2);
        assert!(s2 > 0.0, "score should still be positive: {}", s2);
    }

    #[test]
    fn test_reset() {
        let mut tracker = HeatTracker::new();
        tracker.record_access(1);
        tracker.record_access(1);
        tracker.reset(1);
        assert_eq!(tracker.access_count(1), 0);
    }

    #[test]
    fn test_suggest_high_freq_small_table() {
        // 高频 + 小表 → Memory
        let e = suggest_target_engine(150.0, 5_000);
        assert_eq!(e, crate::common::types::EngineType::Memory);
    }

    #[test]
    fn test_suggest_cold_large_table() {
        // 冷 + 大表 → Log
        let e = suggest_target_engine(0.1, 200_000);
        assert_eq!(e, crate::common::types::EngineType::Log);
    }

    #[test]
    fn test_suggest_warm_table() {
        // 中等热度 + 中等大小 → Columnar（默认）
        let e = suggest_target_engine(10.0, 50_000);
        assert_eq!(e, crate::common::types::EngineType::Columnar);
    }

    #[test]
    fn test_tracked_count() {
        let mut tracker = HeatTracker::new();
        assert_eq!(tracker.tracked_table_count(), 0);
        tracker.record_access(1);
        tracker.record_access(2);
        tracker.record_access(3);
        assert_eq!(tracker.tracked_table_count(), 3);
    }
}
