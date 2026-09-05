//! 向量化执行引擎

pub mod executor;
pub mod expression;
pub mod operators;
pub mod physical_plan;
pub mod vector;

#[cfg(feature = "query-arena")]
pub mod arena;

use crate::common::error::Result;
use crate::storage::Database;
use crate::QueryResult;

use physical_plan::PhysicalPlan;

/// 执行物理计划
///
/// Phase 1 M3：启用 `query-arena` feature 时，整个查询用 bumpalo arena 包裹，
/// 临时分配一次性释放；默认不启用，保持原行为。
pub fn execute(plan: PhysicalPlan, db: &mut Database) -> Result<QueryResult> {
    #[cfg(feature = "query-arena")]
    {
        arena::with_query_arena(|_guard| executor::execute(plan, db))
    }
    #[cfg(not(feature = "query-arena"))]
    {
        executor::execute(plan, db)
    }
}
