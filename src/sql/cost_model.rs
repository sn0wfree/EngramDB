//! 代价模型（CBO 核心）
//!
//! 为每个物理计划节点估算执行代价，用于比较不同执行计划的优劣。
//!
//! 代价单位：抽象代价单位（CPU + IO 的加权和），不对应真实时间。
//! 目标是相对比较，而非绝对预测。
//!
//! 参考 PostgreSQL 的代价模型设计，简化为：
//! - 扫描代价：行数 × 每行扫描成本
//! - 过滤代价：行数 × 每行求值成本
//! - 连接代价：左表行数 × 右表行数 × hash 成本（Hash Join）
//! - 聚合代价：行数 × 每行聚合成本
//! - 排序代价：行数 × log(行数) × 每行比较成本

use crate::executor::physical_plan::{PhysicalPlan, JoinType, AggregateFunc};
use crate::sql::ast::{Expression, BinaryOperator};
use crate::sql::statistics::TableStatistics;
use crate::Value;

/// 代价常量（调优参数）
///
/// 这些是经验值，可根据实际硬件调整。
/// 参考 PostgreSQL 的默认配置比例。
pub const COST_SEQUENTIAL_PAGE: f64 = 1.0;       // 顺序扫描每页成本（基准）
pub const COST_RANDOM_PAGE: f64 = 4.0;           // 随机扫描每页成本
pub const COST_CPU_OP: f64 = 0.01;               // 每次 CPU 操作成本
pub const COST_CPU_COMPARE: f64 = 0.005;         // 每次比较成本
pub const COST_HASH: f64 = 0.02;                 // 每次哈希计算成本
pub const COST_HASH_PROBE: f64 = 0.015;          // 每次哈希探测成本
pub const COST_EXPRESSION: f64 = 0.05;           // 每次表达式求值成本
pub const COST_AGGREGATE: f64 = 0.08;            // 每次聚合操作成本
pub const COST_SORT_PER_ROW: f64 = 0.03;         // 排序每行基础成本

/// 估算的计划属性（用于代价计算）
#[derive(Debug, Clone)]
pub struct PlanProperties {
    /// 估计输出行数
    pub row_count: f64,
    /// 估计输出列数
    pub num_columns: usize,
    /// 估计每行大小（字节）
    pub row_size: usize,
}

/// 代价计算结果
#[derive(Debug, Clone, Copy)]
pub struct Cost {
    /// 启动代价（第一行输出前的成本）
    pub startup: f64,
    /// 总代价（全部输出的成本）
    pub total: f64,
}

impl Cost {
    pub fn zero() -> Self {
        Cost { startup: 0.0, total: 0.0 }
    }

    pub fn add(self, other: Cost) -> Self {
        Cost {
            startup: self.startup + other.startup,
            total: self.total + other.total,
        }
    }
}

/// 代价计算器
pub struct CostModel<'a> {
    /// 表统计信息（按表名索引）
    pub table_stats: &'a [TableStatistics],
}

impl<'a> CostModel<'a> {
    pub fn new(table_stats: &'a [TableStatistics]) -> Self {
        CostModel { table_stats }
    }

    /// 计算物理计划的总代价
    pub fn calculate(&self, plan: &PhysicalPlan) -> Cost {
        self.calculate_node(plan).1
    }

    /// 计算单个节点的代价，返回（输出属性, 代价）
    fn calculate_node(&self, plan: &PhysicalPlan) -> (PlanProperties, Cost) {
        match plan {
            PhysicalPlan::TableScan { table_name, column_indices } => {
                self.cost_table_scan(table_name, column_indices)
            }
            // 覆盖索引点查：代价远低于全表扫描（O(log n) + k 行输出）
            PhysicalPlan::IndexOnlyScan { .. } => {
                (PlanProperties { row_count: 10.0, num_columns: 1, row_size: 50 }, Cost { startup: 0.001, total: 0.01 })
            }
            // 非覆盖索引点查（P2）：索引 O(log n) 定位 + 回表 k 行，代价介于 IndexOnlyScan 与全表扫描之间
            PhysicalPlan::IndexScan { .. } => {
                (PlanProperties { row_count: 10.0, num_columns: 1, row_size: 50 }, Cost { startup: 0.002, total: 0.02 })
            }
            // 索引范围扫描（①）：O(log n + k) 有序段扫描 + 回表 k 行，代价与 IndexScan 同级
            PhysicalPlan::IndexRangeScan { .. } => {
                (PlanProperties { row_count: 100.0, num_columns: 1, row_size: 50 }, Cost { startup: 0.002, total: 0.03 })
            }
            PhysicalPlan::Filter { input, condition } => {
                self.cost_filter(input, condition)
            }
            PhysicalPlan::Projection { input, expressions, .. } => {
                self.cost_projection(input, expressions)
            }
            PhysicalPlan::HashJoin { left, right, join_type, left_keys, right_keys } => {
                self.cost_hash_join(left, right, *join_type, left_keys.len(), right_keys.len())
            }
            PhysicalPlan::CrossJoin { left, right } => {
                // CROSS JOIN：笛卡尔积，代价 = left_rows * right_rows
                let (left_props, _) = self.calculate_node(left);
                let (right_props, _) = self.calculate_node(right);
                let total = left_props.row_count.max(1.0) * right_props.row_count.max(1.0);
                (PlanProperties { row_count: total, num_columns: left_props.num_columns + right_props.num_columns, row_size: left_props.row_size + right_props.row_size }, Cost { startup: 0.0, total })
            }
            PhysicalPlan::Aggregate { input, group_by, aggregates } => {
                self.cost_aggregate(input, group_by.len(), aggregates.len())
            }
            PhysicalPlan::Limit { input, limit } => {
                self.cost_limit(input, *limit)
            }
            PhysicalPlan::Sort { input, sort_keys, .. } => {
                self.cost_sort(input, sort_keys.len())
            }
            // 其他节点代价为 0
            PhysicalPlan::Insert { .. } | PhysicalPlan::InsertColumns { .. } | PhysicalPlan::CreateTable { .. } => {
                (PlanProperties { row_count: 1.0, num_columns: 1, row_size: 100 }, Cost::zero())
            }
            PhysicalPlan::BeginTransaction | PhysicalPlan::Commit | PhysicalPlan::Rollback | PhysicalPlan::CountStar { .. } | PhysicalPlan::PrimaryKeyLookup { .. } => {
                (PlanProperties { row_count: 1.0, num_columns: 1, row_size: 100 }, Cost::zero())
            }
            // DDL/管理语句：代价为 0
            PhysicalPlan::CreateIndex { .. }
            | PhysicalPlan::Delete { .. }
            | PhysicalPlan::Update { .. }
            | PhysicalPlan::Analyze { .. }
            | PhysicalPlan::CreateMaterializedView { .. }
            | PhysicalPlan::RefreshMaterializedView { .. }
            | PhysicalPlan::DropMaterializedView { .. }
            | PhysicalPlan::AlterTable(_)
            | PhysicalPlan::Pragma(_)
            | PhysicalPlan::Distinct { .. }
            | PhysicalPlan::Explain { .. }
            | PhysicalPlan::Window { .. }
            | PhysicalPlan::SubqueryScan { .. }
            | PhysicalPlan::SetUnion { .. }
            | PhysicalPlan::InsertSelect { .. }
            | PhysicalPlan::TruncateTable { .. }
            | PhysicalPlan::CreateTableAs { .. }
            | PhysicalPlan::Savepoint { .. }
            | PhysicalPlan::ReleaseSavepoint { .. }
            | PhysicalPlan::RollbackToSavepoint { .. }
            | PhysicalPlan::VectorSearch { .. } => {
                (PlanProperties { row_count: 1.0, num_columns: 1, row_size: 100 }, Cost::zero())
            }
        }
    }

    // --- TableScan ---

    fn cost_table_scan(&self, table_name: &str, column_indices: &[usize]) -> (PlanProperties, Cost) {
        let stats = self.find_table_stats(table_name);

        let row_count = stats.map(|s| s.row_count as f64).unwrap_or(1000.0);
        let num_cols = column_indices.len().max(1);

        // M5：引擎扫描代价权重（Memory 内存扫描 / Log 块级跳读便宜，
        // 无统计信息时默认 1.0）
        let engine_weight = stats
            .map(|s| crate::storage::capabilities::EngineCapabilities::for_engine(s.engine).scan_cost_weight)
            .unwrap_or(1.0);

        // 估算每行大小（简化：每列 16 字节 + 一些开销）
        let row_size = num_cols * 16;

        // 顺序扫描代价：行数 × 每行成本 × 引擎权重
        let total = (row_count * COST_SEQUENTIAL_PAGE * 0.1 // 每页多行，这里简化
            + row_count * num_cols as f64 * COST_CPU_OP)
            * engine_weight;

        (
            PlanProperties { row_count, num_columns: num_cols, row_size },
            Cost { startup: 0.0, total },
        )
    }

    // --- Filter ---

    fn cost_filter(&self, input: &PhysicalPlan, condition: &Expression) -> (PlanProperties, Cost) {
        let (input_props, input_cost) = self.calculate_node(input);

        // 过滤代价：每行 × 表达式求值成本 × 表达式复杂度
        let expr_complexity = estimate_expression_complexity(condition);
        let filter_cost = input_props.row_count * COST_EXPRESSION * expr_complexity as f64;

        // 估计过滤后的行数（基于选择性）
        let selectivity = estimate_filter_selectivity(condition, &input_props);
        let output_rows = input_props.row_count * selectivity;

        let total = input_cost.total + filter_cost;

        (
            PlanProperties {
                row_count: output_rows,
                num_columns: input_props.num_columns,
                row_size: input_props.row_size,
            },
            Cost {
                startup: input_cost.startup,
                total,
            },
        )
    }

    // --- Projection ---

    fn cost_projection(&self, input: &PhysicalPlan, expressions: &[Expression]) -> (PlanProperties, Cost) {
        let (input_props, input_cost) = self.calculate_node(input);

        // 投影代价：每行 × 表达式数量 × 求值成本
        let total_expr_cost: f64 = expressions.iter()
            .map(|e| estimate_expression_complexity(e) as f64 * COST_EXPRESSION)
            .sum();

        let proj_cost = input_props.row_count * total_expr_cost;
        let total = input_cost.total + proj_cost;

        let row_size = expressions.len() * 16;

        (
            PlanProperties {
                row_count: input_props.row_count,
                num_columns: expressions.len(),
                row_size,
            },
            Cost {
                startup: input_cost.startup,
                total,
            },
        )
    }

    // --- Hash Join ---

    fn cost_hash_join(
        &self,
        left: &PhysicalPlan,
        right: &PhysicalPlan,
        join_type: JoinType,
        left_keys: usize,
        right_keys: usize,
    ) -> (PlanProperties, Cost) {
        let (left_props, left_cost) = self.calculate_node(left);
        let (right_props, right_cost) = self.calculate_node(right);

        // Build 阶段：右表构建哈希表
        let build_cost = right_props.row_count * right_keys as f64 * COST_HASH;

        // Probe 阶段：左表每行探测哈希表
        let probe_cost = left_props.row_count * left_keys as f64 * COST_HASH_PROBE;

        // 估计输出行数
        let output_rows = estimate_join_output_rows(
            &left_props, &right_props, left_keys, join_type
        );

        let total = left_cost.total + right_cost.total + build_cost + probe_cost;

        let num_cols = left_props.num_columns + right_props.num_columns;
        let row_size = left_props.row_size + right_props.row_size;

        (
            PlanProperties { row_count: output_rows, num_columns: num_cols, row_size },
            Cost {
                startup: left_cost.startup + right_cost.startup + build_cost,
                total,
            },
        )
    }

    // --- Aggregate ---

    fn cost_aggregate(
        &self,
        input: &PhysicalPlan,
        num_group_by: usize,
        num_aggregates: usize,
    ) -> (PlanProperties, Cost) {
        let (input_props, input_cost) = self.calculate_node(input);

        // 聚合代价：每行 × （分组键哈希 + 聚合计算）
        let hash_cost = input_props.row_count * num_group_by as f64 * COST_HASH;
        let agg_cost = input_props.row_count * num_aggregates as f64 * COST_AGGREGATE;

        let total = input_cost.total + hash_cost + agg_cost;

        // 估计分组数（简化：假设是输入行数的 1/10，最少 1 组）
        let num_groups = if num_group_by == 0 {
            1.0
        } else {
            (input_props.row_count * 0.1).max(1.0).min(input_props.row_count)
        };

        let num_cols = num_group_by + num_aggregates;

        (
            PlanProperties {
                row_count: num_groups,
                num_columns: num_cols,
                row_size: num_cols * 16,
            },
            Cost {
                startup: input_cost.startup,
                total,
            },
        )
    }

    // --- Sort ---

    fn cost_sort(&self, input: &PhysicalPlan, num_keys: usize) -> (PlanProperties, Cost) {
        let (input_props, input_cost) = self.calculate_node(input);

        // 排序代价：O(n log n) × 每行比较成本 × 排序列数
        // 启动代价：需要先读入所有数据才能开始输出
        let n = input_props.row_count.max(1.0);
        let sort_cost = n * n.log2() * COST_SORT_PER_ROW * num_keys as f64;

        let total = input_cost.total + sort_cost;

        (
            PlanProperties {
                row_count: input_props.row_count,
                num_columns: input_props.num_columns,
                row_size: input_props.row_size,
            },
            Cost {
                startup: total, // 排序需要全部数据加载完才能输出第一行
                total,
            },
        )
    }

    // --- Limit ---

    fn cost_limit(&self, input: &PhysicalPlan, limit: usize) -> (PlanProperties, Cost) {
        let (input_props, input_cost) = self.calculate_node(input);

        let output_rows = (limit as f64).min(input_props.row_count);

        // Limit 本身代价很小，主要是子计划代价
        // 但如果有 Limit，可以提前终止（代价按比例减少）
        let ratio = if input_props.row_count > 0.0 {
            (limit as f64 / input_props.row_count).min(1.0)
        } else {
            1.0
        };

        // 简化：假设 Limit 可以减少子计划代价的比例
        // （实际取决于算子是否支持 early stop）
        let total = input_cost.total * (0.5 + 0.5 * ratio); // 至少 50% 成本（启动开销）

        (
            PlanProperties {
                row_count: output_rows,
                num_columns: input_props.num_columns,
                row_size: input_props.row_size,
            },
            Cost {
                startup: input_cost.startup,
                total,
            },
        )
    }

    /// 查找表统计信息
    fn find_table_stats(&self, table_name: &str) -> Option<&TableStatistics> {
        self.table_stats.iter().find(|s| s.table_name == table_name)
    }
}

// ============================================================================
// 辅助函数
// ============================================================================

/// 估计表达式复杂度（用于代价估算）
///
/// 返回一个相对复杂度分数，用于加权表达式求值代价。
fn estimate_expression_complexity(expr: &Expression) -> usize {
    match expr {
        Expression::Literal(_) => 1,
        Expression::ColumnRef { .. } => 1,
        Expression::BinaryOp { left, right, .. } => {
            1 + estimate_expression_complexity(left) + estimate_expression_complexity(right)
        }
        Expression::UnaryOp { expr, .. } => {
            1 + estimate_expression_complexity(expr)
        }
        Expression::Function { args, .. } => {
            2 + args.iter().map(|a| estimate_expression_complexity(a)).sum::<usize>()
        }
        Expression::Cast { expr, .. } => {
            1 + estimate_expression_complexity(expr)
        }
        Expression::IsNull(e) | Expression::IsNotNull(e) => {
            1 + estimate_expression_complexity(e)
        }
        Expression::InList { expr, list } => {
            2 + estimate_expression_complexity(expr)
                + list.iter().map(|e| estimate_expression_complexity(e)).sum::<usize>()
        }
        Expression::Like { expr, pattern } => {
            3 + estimate_expression_complexity(expr) + estimate_expression_complexity(pattern)
        }
        Expression::Case { when_then, else_expr } => {
            let when_cost: usize = when_then.iter()
                .map(|(w, t)| estimate_expression_complexity(w) + estimate_expression_complexity(t))
                .sum();
            let else_cost = else_expr.as_ref().map(|e| estimate_expression_complexity(e)).unwrap_or(0);
            2 + when_cost + else_cost
        }
        Expression::Placeholder(_) => 1,
        Expression::Subquery(_) => 10,
        Expression::Exists { .. } => 5,
        Expression::InSubquery { .. } => 8,
    }
}

/// 估计过滤条件的选择性（0.0 ~ 1.0）
///
/// 基于启发式规则：
/// - 等值比较：1/NDV（未知 NDV 时默认 0.1）
/// - 范围比较：默认 0.33（1/3 规则）
/// - AND：各选择性相乘
/// - OR：1 - (1-s1)(1-s2)
/// - NOT：1 - s
fn estimate_filter_selectivity(condition: &Expression, _input_props: &PlanProperties) -> f64 {
    match condition {
        Expression::BinaryOp { left, op, right } => {
            use BinaryOperator::*;
            match op {
                Eq => {
                    // 等值比较：默认 0.1（未知 NDV 时的保守估计）
                    let is_const = matches!(left.as_ref(), Expression::Literal(_))
                        || matches!(right.as_ref(), Expression::Literal(_));
                    if is_const { 0.1 } else { 0.5 }
                }
                NotEq => {
                    1.0 - estimate_filter_selectivity(
                        &Expression::BinaryOp { left: left.clone(), op: Eq, right: right.clone() },
                        _input_props
                    )
                }
                Lt | LtEq | Gt | GtEq => {
                    // 范围比较：默认 0.33
                    0.33
                }
                And => {
                    let sel_left = estimate_filter_selectivity(left, _input_props);
                    let sel_right = estimate_filter_selectivity(right, _input_props);
                    sel_left * sel_right
                }
                Or => {
                    let sel_left = estimate_filter_selectivity(left, _input_props);
                    let sel_right = estimate_filter_selectivity(right, _input_props);
                    1.0 - (1.0 - sel_left) * (1.0 - sel_right)
                }
                _ => 0.5, // 未知类型，默认 50%
            }
        }
        Expression::UnaryOp { op, expr } => {
            use crate::sql::ast::UnaryOperator::*;
            match op {
                Not => 1.0 - estimate_filter_selectivity(expr, _input_props),
                Negate => estimate_filter_selectivity(expr, _input_props),
            }
        }
        Expression::IsNull(_) => 0.1,  // 默认 10% NULL
        Expression::IsNotNull(_) => 0.9, // 90% 非空
        Expression::InList { list, .. } => {
            // IN (list)：list 长度 × 等值选择性
            (list.len() as f64 * 0.1).min(0.9)
        }
        Expression::Like { .. } => 0.1, // LIKE 默认 10%
        Expression::Literal(Value::Boolean(true)) => 1.0,
        Expression::Literal(Value::Boolean(false)) => 0.0,
        _ => 0.5, // 未知，默认 50%
    }
}

/// 估计连接输出行数
fn estimate_join_output_rows(
    left: &PlanProperties,
    right: &PlanProperties,
    num_keys: usize,
    join_type: JoinType,
) -> f64 {
    // 简化估计：左表行数 × 右表匹配率
    // 假设每个左表行平均匹配 1 / NDV 个右表行
    // 未知 NDV 时，默认匹配率 1/10
    let match_rate = 0.1;

    let inner_rows = left.row_count * right.row_count * match_rate / (num_keys as f64).max(1.0);

    match join_type {
        JoinType::Inner => inner_rows,
        JoinType::Left => inner_rows.max(left.row_count),
        JoinType::Right => inner_rows.max(right.row_count),
        JoinType::Full => inner_rows.max(left.row_count).max(right.row_count),
        JoinType::Semi => left.row_count,
        JoinType::Anti => left.row_count,
    }
}

// ============================================================================
// 测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::physical_plan::*;
    use crate::sql::ast::{Expression, BinaryOperator};

    fn make_scan(rows: f64, cols: usize) -> PhysicalPlan {
        // 用一个假的 TableScan，代价模型会用默认值
        PhysicalPlan::TableScan {
            table_name: "test".to_string(),
            column_indices: (0..cols).collect(),
        }
    }

    #[test]
    fn test_scan_cost() {
        let stats = vec![];
        let model = CostModel::new(&stats);
        let plan = make_scan(1000.0, 5);
        let cost = model.calculate(&plan);
        assert!(cost.total > 0.0);
    }

    #[test]
    fn test_filter_reduces_rows() {
        let stats = vec![];
        let model = CostModel::new(&stats);

        let scan = make_scan(1000.0, 3);
        let filter = PhysicalPlan::Filter {
            input: Box::new(scan),
            condition: Expression::BinaryOp {
                left: Box::new(Expression::ColumnRef { table: None, column: "id".into() }),
                op: BinaryOperator::Eq,
                right: Box::new(Expression::Literal(Value::Int64(42))),
            },
        };

        let (props, _) = model.calculate_node(&filter);
        // 等值过滤选择性 0.1 → 100 行
        assert!((props.row_count - 100.0).abs() < 1.0);
    }

    #[test]
    fn test_and_selectivity() {
        let stats = vec![];
        let model = CostModel::new(&stats);

        let scan = make_scan(1000.0, 3);
        let filter = PhysicalPlan::Filter {
            input: Box::new(scan),
            condition: Expression::BinaryOp {
                left: Box::new(Expression::BinaryOp {
                    left: Box::new(Expression::ColumnRef { table: None, column: "a".into() }),
                    op: BinaryOperator::Eq,
                    right: Box::new(Expression::Literal(Value::Int64(1))),
                }),
                op: BinaryOperator::And,
                right: Box::new(Expression::BinaryOp {
                    left: Box::new(Expression::ColumnRef { table: None, column: "b".into() }),
                    op: BinaryOperator::Eq,
                    right: Box::new(Expression::Literal(Value::Int64(2))),
                }),
            },
        };

        let (props, _) = model.calculate_node(&filter);
        // 两个 AND 等值条件：0.1 * 0.1 = 0.01 → 10 行
        assert!((props.row_count - 10.0).abs() < 1.0);
    }

    #[test]
    fn test_join_cost() {
        let stats = vec![];
        let model = CostModel::new(&stats);

        let left = make_scan(1000.0, 2);
        let right = make_scan(500.0, 2);

        let join = PhysicalPlan::HashJoin {
            left: Box::new(left),
            right: Box::new(right),
            join_type: JoinType::Inner,
            left_keys: vec![0],
            right_keys: vec![0],
        };

        let cost = model.calculate(&join);
        assert!(cost.total > 0.0);
    }

    #[test]
    fn test_limit_reduces_cost() {
        let stats = vec![];
        let model = CostModel::new(&stats);

        let scan = make_scan(10000.0, 5);
        let limit = PhysicalPlan::Limit {
            input: Box::new(scan.clone()),
            limit: 10,
        };

        let full_cost = model.calculate(&scan);
        let limit_cost = model.calculate(&limit);

        // LIMIT 应该降低总代价
        assert!(limit_cost.total < full_cost.total);
    }

    #[test]
    fn test_expression_complexity() {
        let simple = Expression::Literal(Value::Int64(1));
        assert_eq!(estimate_expression_complexity(&simple), 1);

        let binary = Expression::BinaryOp {
            left: Box::new(Expression::Literal(Value::Int64(1))),
            op: BinaryOperator::Plus,
            right: Box::new(Expression::Literal(Value::Int64(2))),
        };
        assert_eq!(estimate_expression_complexity(&binary), 3); // 1 + 1 + 1
    }
}
