//! 引擎能力检测表（v0.17.0 M5 / P5）
//!
//! 各引擎的能力矩阵（中央定义）：优化器/规划器据此提前校验 SQL，
//! 不支持的操作在规划阶段即报清晰错误（而非执行期深挖）。
//!
//! | 能力             | Columnar | Memory | Log |
//! |------------------|:--------:|:------:|:---:|
//! | 二级索引         | ✅       | ❌     | ❌ |
//! | 向量索引         | ✅       | ❌     | ❌ |
//! | FTS              | ✅       | ❌     | ❌ |
//! | ALTER TABLE      | ✅       | ❌     | ❌ |
//! | PRAGMA           | ✅       | ❌     | ❌ |
//! | TTL              | ✅       | ❌     | ❌ |
//! | UPDATE           | ✅       | ✅     | ❌ |
//! | DELETE           | ✅       | ✅     | ❌ |
//! | 主键点查短路     | ✅       | ✅     | ❌ |
//! | 扫描代价权重     | 1.0      | 0.1    | 0.5 |
//! | 持久化           | ✅       | ❌     | ✅ |

use crate::common::types::EngineType;

/// 引擎能力位
#[derive(Debug, Clone)]
pub struct EngineCapabilities {
    pub engine: EngineType,
    /// CREATE INDEX / 二级索引
    pub supports_index: bool,
    /// 向量索引（HNSW 等）
    pub supports_vector_index: bool,
    /// 全文检索
    pub supports_fts: bool,
    /// ALTER TABLE
    pub supports_alter: bool,
    /// PRAGMA
    pub supports_pragma: bool,
    /// TTL 自动过期
    pub supports_ttl: bool,
    /// UPDATE 行级更新
    pub supports_update: bool,
    /// DELETE 行级删除
    pub supports_delete: bool,
    /// 主键点查短路（PrimaryKeyLookup）
    pub supports_pk_lookup: bool,
    /// 数据落盘（Memory 例外）
    pub persistent: bool,
    /// 扫描代价权重（JOIN 代价模型，越低越便宜）
    pub scan_cost_weight: f64,
}

impl EngineCapabilities {
    pub fn for_engine(engine: EngineType) -> Self {
        match engine {
            EngineType::Columnar => Self {
                engine,
                supports_index: true,
                supports_vector_index: true,
                supports_fts: true,
                supports_alter: true,
                supports_pragma: true,
                supports_ttl: true,
                supports_update: true,
                supports_delete: true,
                supports_pk_lookup: true,
                persistent: true,
                scan_cost_weight: 1.0,
            },
            EngineType::Memory => Self {
                engine,
                supports_index: false,
                supports_vector_index: false,
                supports_fts: false,
                supports_alter: false,
                supports_pragma: false,
                supports_ttl: false,
                supports_update: true,
                supports_delete: true,
                supports_pk_lookup: true,
                persistent: false,
                scan_cost_weight: 0.1,
            },
            EngineType::Log => Self {
                engine,
                supports_index: false,
                supports_vector_index: false,
                supports_fts: false,
                supports_alter: false,
                supports_pragma: false,
                supports_ttl: false,
                supports_update: false,
                supports_delete: false,
                supports_pk_lookup: false,
                persistent: true,
                scan_cost_weight: 0.5,
            },
        }
    }

    /// 校验能力，失败返回清晰错误（planner 提前拦截用）
    pub fn ensure(&self, capability: &str, enabled: bool, table_name: &str) -> crate::common::error::Result<()> {
        if enabled {
            return Ok(());
        }
        Err(crate::common::error::EngramDbError::NotSupported(format!(
            "{:?} 引擎不支持 {}（表 '{}'）",
            self.engine, capability, table_name
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_columnar_capabilities() {
        let cap = EngineCapabilities::for_engine(EngineType::Columnar);
        assert!(cap.supports_index && cap.supports_vector_index && cap.supports_fts);
        assert!(cap.supports_alter && cap.supports_pragma && cap.supports_ttl);
        assert!(cap.supports_update && cap.supports_delete && cap.supports_pk_lookup);
        assert!(cap.persistent);
        assert_eq!(cap.scan_cost_weight, 1.0);
        assert!(cap.ensure("索引", true, "t").is_ok());
        assert!(cap.ensure("索引", false, "t").is_err());
    }

    #[test]
    fn test_memory_capabilities() {
        let cap = EngineCapabilities::for_engine(EngineType::Memory);
        assert!(!cap.supports_index && !cap.supports_vector_index && !cap.supports_fts);
        assert!(!cap.supports_alter && !cap.supports_pragma && !cap.supports_ttl);
        assert!(cap.supports_update && cap.supports_delete && cap.supports_pk_lookup);
        assert!(!cap.persistent, "Memory 不持久化");
        assert_eq!(cap.scan_cost_weight, 0.1, "Memory 扫描最便宜");
    }

    #[test]
    fn test_log_capabilities() {
        let cap = EngineCapabilities::for_engine(EngineType::Log);
        assert!(!cap.supports_index && !cap.supports_alter && !cap.supports_ttl);
        assert!(!cap.supports_update && !cap.supports_delete && !cap.supports_pk_lookup);
        assert!(cap.persistent, "Log 持久化");
        assert_eq!(cap.scan_cost_weight, 0.5);
    }

    #[test]
    fn test_ensure_error_message() {
        let cap = EngineCapabilities::for_engine(EngineType::Log);
        let err = cap.ensure("UPDATE", false, "events").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("Log"), "错误应包含引擎名: {msg}");
        assert!(msg.contains("UPDATE") && msg.contains("events"), "错误应包含能力与表名: {msg}");
    }

    #[test]
    fn test_ensure_enabled_is_ok() {
        let cap = EngineCapabilities::for_engine(EngineType::Log);
        assert!(cap.ensure("UPDATE", false, "t").is_err());
        assert!(cap.ensure("UPDATE", true, "t").is_ok(), "已启用能力不报错");
    }
}
