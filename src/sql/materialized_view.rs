//! 物化视图（Materialized View）
//!
//! 物化视图将查询结果物理存储，加速重复查询。
//! 支持创建、刷新、查询重写三大核心能力。
//!
//! 设计要点：
//! - 物化视图作为特殊表存储（带 MV 元数据标记）
//! - REFRESH 重新执行查询并替换数据
//! - 查询重写：优化器自动将查询改写为命中物化视图
//! - 支持 FULL / CONCURRENTLY 刷新模式

use crate::common::error::Result;
use crate::executor::physical_plan::PhysicalPlan;
use crate::sql::ast::Statement;

/// 物化视图元数据
#[derive(Debug, Clone)]
pub struct MaterializedView {
    /// 视图名称
    pub name: String,
    /// 定义查询的 SQL（原始语句）
    pub definition_sql: String,
    /// 定义查询的物理计划（序列化后存储）
    pub definition_plan: Option<PhysicalPlan>,
    /// 列名列表
    pub columns: Vec<String>,
    /// 最后刷新时间（Unix 时间戳秒）
    pub last_refreshed_at: Option<i64>,
    /// 刷新模式
    pub refresh_mode: RefreshMode,
    /// 是否已填充数据
    pub is_populated: bool,
}

/// 刷新模式
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshMode {
    /// 全量刷新（默认）
    Full,
    /// 并发刷新（不阻塞读）
    Concurrently,
}

impl MaterializedView {
    /// 创建新的物化视图定义
    pub fn new(name: String, definition_sql: String, columns: Vec<String>) -> Self {
        Self {
            name,
            definition_sql,
            definition_plan: None,
            columns,
            last_refreshed_at: None,
            refresh_mode: RefreshMode::Full,
            is_populated: false,
        }
    }

    /// 标记为已刷新
    pub fn mark_refreshed(&mut self) {
        self.is_populated = true;
        self.last_refreshed_at = Some(current_timestamp_secs());
    }
}

/// 物化视图注册表
///
/// 管理数据库中所有物化视图的元数据。
/// 实际生产中应持久化到系统表，这里提供内存中的 Registry 接口。
#[derive(Debug, Default)]
pub struct MaterializedViewRegistry {
    views: std::collections::HashMap<String, MaterializedView>,
}

impl MaterializedViewRegistry {
    /// 创建空注册表
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册物化视图
    pub fn register(&mut self, mv: MaterializedView) {
        self.views.insert(mv.name.clone(), mv);
    }

    /// 删除物化视图
    pub fn drop(&mut self, name: &str) -> Option<MaterializedView> {
        self.views.remove(name)
    }

    /// 获取物化视图
    pub fn get(&self, name: &str) -> Option<&MaterializedView> {
        self.views.get(name)
    }

    /// 列出所有物化视图
    pub fn list(&self) -> Vec<&MaterializedView> {
        self.views.values().collect()
    }

    /// 更新刷新时间
    pub fn mark_refreshed(&mut self, name: &str) -> Result<()> {
        let mv = self.views.get_mut(name)
            .ok_or_else(|| crate::common::error::HybridDbError::Internal(
                format!("materialized view '{}' not found", name)
            ))?;
        mv.mark_refreshed();
        Ok(())
    }
}

/// 查询重写器（Query Rewriter）
///
/// 尝试将用户查询改写为命中物化视图，以加速执行。
///
/// 重写策略：
/// 1. 精确匹配：查询与 MV 定义完全一致 → 直接读 MV 表
/// 2. 聚合上卷：查询是 MV 的更粗粒度聚合 → 在 MV 上再聚合
/// 3. 过滤下推：查询在 MV 基础上有额外过滤 → 读 MV 后过滤
#[derive(Debug)]
pub struct QueryRewriter<'a> {
    registry: &'a MaterializedViewRegistry,
}

impl<'a> QueryRewriter<'a> {
    /// 创建重写器
    pub fn new(registry: &'a MaterializedViewRegistry) -> Self {
        Self { registry }
    }

    /// 尝试重写查询计划
    ///
    /// 返回 Some(rewritten_plan) 如果找到可命中的物化视图，
    /// 返回 None 表示无需重写（使用原始计划）。
    pub fn try_rewrite(&self, _plan: &PhysicalPlan) -> Option<PhysicalPlan> {
        // 完整的查询重写需要：
        // 1. 从计划中提取查询模式（扫描的表、过滤条件、聚合、投影）
        // 2. 与每个物化视图的定义进行模式匹配
        // 3. 验证等价性（列映射、聚合粒度等）
        // 4. 生成重写后的计划（扫描 MV 表 + 必要的后处理）
        //
        // 这里提供框架，具体匹配逻辑在后续版本完善。
        // 简单策略：如果查询的表是某个 MV 的名字且 MV 已填充，
        // 则直接使用 MV 表（等价于普通表扫描，MV 表物理存在）。

        None
    }

    /// 检查语句是否是物化视图相关操作
    pub fn is_mv_statement(stmt: &Statement) -> bool {
        matches!(stmt,
            Statement::CreateMaterializedView { .. }
            | Statement::RefreshMaterializedView { .. }
            | Statement::DropMaterializedView { .. }
        )
    }
}

/// 获取当前 Unix 时间戳（秒）
fn current_timestamp_secs() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ============================================================
// 测试
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mv_registry_basic() {
        let mut registry = MaterializedViewRegistry::new();
        assert_eq!(registry.list().len(), 0);

        let mv = MaterializedView::new(
            "mv_sales_summary".to_string(),
            "SELECT date, SUM(amount) FROM sales GROUP BY date".to_string(),
            vec!["date".to_string(), "sum_amount".to_string()],
        );
        registry.register(mv);

        assert_eq!(registry.list().len(), 1);
        assert!(registry.get("mv_sales_summary").is_some());
        assert!(!registry.get("nonexistent").is_some());

        let mv = registry.get("mv_sales_summary").unwrap();
        assert_eq!(mv.columns.len(), 2);
        assert!(!mv.is_populated);
        assert!(mv.last_refreshed_at.is_none());
    }

    #[test]
    fn test_mv_mark_refreshed() {
        let mut registry = MaterializedViewRegistry::new();
        let mv = MaterializedView::new(
            "mv_test".to_string(),
            "SELECT * FROM t".to_string(),
            vec!["id".to_string()],
        );
        registry.register(mv);
        registry.mark_refreshed("mv_test").unwrap();

        let mv = registry.get("mv_test").unwrap();
        assert!(mv.is_populated);
        assert!(mv.last_refreshed_at.is_some());
        assert!(mv.last_refreshed_at.unwrap() > 0);
    }

    #[test]
    fn test_mv_drop() {
        let mut registry = MaterializedViewRegistry::new();
        let mv = MaterializedView::new(
            "mv_drop_test".to_string(),
            "SELECT 1".to_string(),
            vec!["one".to_string()],
        );
        registry.register(mv);
        assert_eq!(registry.list().len(), 1);

        let dropped = registry.drop("mv_drop_test");
        assert!(dropped.is_some());
        assert_eq!(registry.list().len(), 0);

        let dropped_again = registry.drop("mv_drop_test");
        assert!(dropped_again.is_none());
    }

    #[test]
    fn test_refresh_mode_default() {
        let mv = MaterializedView::new(
            "mv_mode".to_string(),
            "SELECT 1".to_string(),
            vec!["c".to_string()],
        );
        assert_eq!(mv.refresh_mode, RefreshMode::Full);
    }

    #[test]
    fn test_rewriter_no_match() {
        let registry = MaterializedViewRegistry::new();
        let rewriter = QueryRewriter::new(&registry);
        let plan = PhysicalPlan::TableScan {
            table_name: "t".to_string(),
            column_indices: vec![0],
        };
        assert!(rewriter.try_rewrite(&plan).is_none());
    }
}
