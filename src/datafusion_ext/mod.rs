//! DataFusion 扩展层
//!
//! 基于 DataFusion 构建 SQL 能力，EngramDB 专注存储层对接与执行优化。
//!
//! 架构:
//! ```text
//! SQL → DataFusion (Parser/Optimizer/Planner) → EngramDB TableProvider → 存储引擎
//! ```

pub mod catalog;
pub mod table_provider;
pub mod types;

pub use catalog::EngramDBCatalog;
pub use table_provider::EngramDBTable;
