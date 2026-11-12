//! WAL 自动 Checkpoint 触发器（A3）
//!
//! 阈值语义（与 `Config` 对应字段一致，OR 关系）：
//! - **字节阈值** `byte_threshold`：自上次 checkpoint（WAL 段滚动）以来，
//!   WAL 累计字节数 ≥ 阈值 → 触发；0 = 禁用。
//! - **时间阈值** `timeout_ms`：距上次 checkpoint 超过该毫秒数、且 WAL
//!   非空（有未 checkpoint 的字节）→ 触发；0 = 禁用。
//!
//! 两个阈值都禁用、或 WAL 为空时永不触发（空 WAL 上做完整 checkpoint
//! 只会白白付出主存储落盘开销）。
//!
//! 触发 ≠ 执行：本模块只做「是否该 checkpoint」的判定；完整 checkpoint
//! （主存储落盘 + WAL 段滚动/回收）由上层在拿到 `should_trigger == true`
//! 后调用既有入口（`Database::checkpoint` / `TransactionManager::checkpoint`）
//! 执行——WAL 单独滚动会丢弃未落盘的 redo 日志，破坏崩溃恢复。

use std::time::{Duration, Instant};

/// WAL 自动 checkpoint 触发器
#[derive(Debug, Clone)]
pub struct CheckpointTrigger {
    /// 字节阈值（0 = 禁用）
    byte_threshold: u64,
    /// 时间阈值（None = 禁用）
    timeout: Option<Duration>,
    /// 上次 checkpoint（段滚动）时刻
    last_checkpoint: Instant,
}

impl CheckpointTrigger {
    /// 创建触发器。`byte_threshold == 0` 禁用字节触发；`timeout_ms == 0` 禁用时间触发。
    pub fn new(byte_threshold: u64, timeout_ms: u64) -> Self {
        Self {
            byte_threshold,
            timeout: Self::timeout_from_ms(timeout_ms),
            last_checkpoint: Instant::now(),
        }
    }

    /// 更新阈值策略（保留上次 checkpoint 时刻）
    pub fn set_policy(&mut self, byte_threshold: u64, timeout_ms: u64) {
        self.byte_threshold = byte_threshold;
        self.timeout = Self::timeout_from_ms(timeout_ms);
    }

    /// 是否应触发 checkpoint
    ///
    /// `bytes_since_checkpoint`：自上次 checkpoint 以来的 WAL 累计字节数
    /// （含未刷盘的缓冲字节）。
    pub fn should_trigger(&self, bytes_since_checkpoint: u64) -> bool {
        // WAL 为空：无可回收内容，不触发（避免空转全量落盘）
        if bytes_since_checkpoint == 0 {
            return false;
        }
        if self.byte_threshold > 0 && bytes_since_checkpoint >= self.byte_threshold {
            return true;
        }
        if let Some(t) = self.timeout {
            if self.last_checkpoint.elapsed() >= t {
                return true;
            }
        }
        false
    }

    /// 标记一次 checkpoint 已完成（段滚动时调用，重置时间窗）
    pub fn mark_checkpointed(&mut self) {
        self.last_checkpoint = Instant::now();
    }

    /// 距上次 checkpoint 的时长
    pub fn last_checkpoint_elapsed(&self) -> Duration {
        self.last_checkpoint.elapsed()
    }

    /// 当前字节阈值（0 = 禁用）
    pub fn byte_threshold(&self) -> u64 {
        self.byte_threshold
    }

    /// 当前时间阈值毫秒数（None = 禁用）
    pub fn timeout_ms(&self) -> Option<u64> {
        self.timeout.map(|d| d.as_millis() as u64)
    }

    fn timeout_from_ms(ms: u64) -> Option<Duration> {
        if ms == 0 {
            None
        } else {
            Some(Duration::from_millis(ms))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_not_due_below_byte_threshold() {
        let t = CheckpointTrigger::new(1024, 0);
        assert!(!t.should_trigger(0), "空 WAL 不触发");
        assert!(!t.should_trigger(1023), "未达字节阈值不触发");
    }

    #[test]
    fn test_due_at_byte_threshold() {
        let t = CheckpointTrigger::new(1024, 0);
        assert!(t.should_trigger(1024), "达到字节阈值应触发");
        assert!(t.should_trigger(4096), "超过字节阈值应触发");
    }

    #[test]
    fn test_byte_threshold_zero_disables() {
        let t = CheckpointTrigger::new(0, 0);
        assert!(!t.should_trigger(u64::MAX), "字节阈值 0 = 禁用");
    }

    #[test]
    fn test_timeout_due_when_bytes_nonzero() {
        let mut t = CheckpointTrigger::new(0, 1);
        // 空 WAL 即便超时也不触发
        assert!(!t.should_trigger(0), "空 WAL 超时不触发");
        std::thread::sleep(Duration::from_millis(5));
        assert!(!t.should_trigger(0), "空 WAL 超时不触发（二次确认）");
        // 有未 checkpoint 字节 + 超时 → 触发
        assert!(t.should_trigger(1), "超时且 WAL 非空应触发");
    }

    #[test]
    fn test_timeout_not_due_before_deadline() {
        let t = CheckpointTrigger::new(0, 60_000);
        assert!(!t.should_trigger(100), "时间窗未到不触发");
    }

    #[test]
    fn test_mark_checkpointed_resets_time_window() {
        let mut t = CheckpointTrigger::new(0, 1);
        std::thread::sleep(Duration::from_millis(5));
        assert!(t.should_trigger(1));
        t.mark_checkpointed();
        // 刚 checkpoint 完：时间窗重置，未到下窗不触发
        assert!(!t.should_trigger(1), "mark 后时间窗应重置");
    }

    #[test]
    fn test_or_semantics() {
        // 字节未达但时间到 → 触发（OR）
        let mut t = CheckpointTrigger::new(1_000_000, 1);
        std::thread::sleep(Duration::from_millis(5));
        assert!(t.should_trigger(10), "时间到即触发（OR 语义）");

        // 时间未到但字节达到 → 触发（OR）
        let mut t2 = CheckpointTrigger::new(100, 60_000);
        assert!(t2.should_trigger(100), "字节达到即触发（OR 语义）");
        t2.mark_checkpointed();
        assert!(t2.should_trigger(100), "mark 不影响字节判定");
    }

    #[test]
    fn test_set_policy_updates_thresholds() {
        let mut t = CheckpointTrigger::new(0, 0);
        assert!(!t.should_trigger(u64::MAX));
        t.set_policy(64, 0);
        assert_eq!(t.byte_threshold(), 64);
        assert!(t.should_trigger(64));
        t.set_policy(0, 0);
        assert!(!t.should_trigger(u64::MAX), "改回 0 = 重新禁用");
    }
}
