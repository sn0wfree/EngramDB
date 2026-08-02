//! DataFusion 扩展层
//!
//! 基于 DataFusion 构建 SQL 能力，HybridDB 专注存储层对接与执行优化。
//!
//! 架构:
//! ```text
//! SQL → DataFusion (Parser/Optimizer/Planner) → HybridDB TableProvider → 存储引擎
//! ```

pub mod catalog;
pub mod table_provider;
pub mod types;

pub use catalog::HybridDBCatalog;
pub use table_provider::HybridDBTable;
