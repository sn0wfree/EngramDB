//! 向量化执行引擎

pub mod physical_plan;
pub mod vector;
pub mod operators;
pub mod expression;
pub mod executor;

use crate::common::error::Result;
use crate::storage::Database;
use crate::QueryResult;

use physical_plan::PhysicalPlan;

/// 执行物理计划
pub fn execute(plan: PhysicalPlan, db: &mut Database) -> Result<QueryResult> {
    executor::execute(plan, db)
}
