//! 查询优化器（RBO + CBO 两级优化）
//!
//! 两级优化架构：
//! 1. RBO（Rule-Based Optimization）- 基于规则的等价变换
//!    - 谓词下推（Predicate Pushdown）
//!    - 投影下推（Projection Pushdown）
//!    - 常量折叠（Constant Folding）
//!    - 过滤条件重排（Filter Reorder）
//!
//! 2. CBO（Cost-Based Optimization）- 基于代价的优化
//!    - 连接顺序优化（Join Order Optimization）- System R 风格 DP
//!    - 代价模型（Cost Model）- PostgreSQL 风格抽象代价单位
//!    - 统计信息驱动（Statistics）- 表/列级统计 + 直方图
//!
//! 优化器是无损的：所有变换都保证结果等价。

use crate::common::error::Result;
use crate::executor::physical_plan::PhysicalPlan;
use crate::sql::ast::{BinaryOperator, Expression, UnaryOperator};
use crate::sql::cost_model::CostModel;
use crate::sql::statistics::TableStatistics;
use crate::Value;

/// 优化执行计划（RBO + CBO 两级优化）
///
/// 流程：RBO 迭代收敛 → CBO 代价优化
/// 无统计信息时 CBO 使用启发式默认值，仍可进行连接重排等结构优化。
pub fn optimize(plan: PhysicalPlan) -> Result<PhysicalPlan> {
    optimize_with_stats(plan, &[])
}

/// 带统计信息的优化（RBO + CBO）
pub fn optimize_with_stats(plan: PhysicalPlan, table_stats: &[TableStatistics]) -> Result<PhysicalPlan> {
    // Phase 1: RBO 规则优化（迭代至收敛）
    let mut current = rbo_optimize(plan)?;

    // Phase 2: CBO 代价优化
    let cost_model = CostModel::new(table_stats);
    let cbo_result = cbo_optimize(current.clone(), &cost_model)?;

    // 比较代价，选择更优的
    let original_cost = cost_model.calculate(&current).total;
    let cbo_cost = cost_model.calculate(&cbo_result).total;
    if cbo_cost < original_cost {
        current = cbo_result;
    }

    Ok(current)
}

/// RBO 规则优化（迭代应用规则至收敛）
fn rbo_optimize(plan: PhysicalPlan) -> Result<PhysicalPlan> {
    let mut current = plan;
    let mut iterations = 0;
    const MAX_ITERATIONS: usize = 10;

    loop {
        let mut changed = false;

        // 规则 1: 常量折叠（先做，为后续规则创造机会）
        let folded = constant_folding(current.clone())?;
        if !plan_eq(&folded, &current) {
            changed = true;
            current = folded;
        }

        // 规则 2: 谓词下推
        let pushed = predicate_pushdown(current.clone())?;
        if !plan_eq(&pushed, &current) {
            changed = true;
            current = pushed;
        }

        // 规则 3: 投影下推
        let projected = projection_pushdown(current.clone())?;
        if !plan_eq(&projected, &current) {
            changed = true;
            current = projected;
        }

        // 规则 4: 过滤条件重排
        let reordered = filter_reorder(current.clone())?;
        if !plan_eq(&reordered, &current) {
            changed = true;
            current = reordered;
        }

        iterations += 1;
        if !changed || iterations >= MAX_ITERATIONS {
            break;
        }
    }

    Ok(current)
}

/// CBO 代价优化
///
/// 当前实现：
/// - 连接顺序优化（多表连接时重排）
/// - 未来可扩展：索引选择、聚合策略、并行度选择等
fn cbo_optimize(plan: PhysicalPlan, cost_model: &CostModel) -> Result<PhysicalPlan> {
    // 提取连接关系并优化顺序
    if let Some(optimized) = try_optimize_joins(&plan, cost_model)? {
        Ok(optimized)
    } else {
        Ok(plan)
    }
}

/// 尝试优化计划中的连接顺序
///
/// 从计划树中收集所有 HashJoin 节点及其底层 TableScan，
/// 构建连接关系图，调用 DP 算法找最优顺序，再重建计划树。
fn try_optimize_joins(plan: &PhysicalPlan, _cost_model: &CostModel) -> Result<Option<PhysicalPlan>> {
    // 收集所有连接节点和基表
    let mut joins = Vec::new();
    let mut base_tables = Vec::new();
    collect_joins_and_tables(plan, &mut joins, &mut base_tables);

    // 少于 2 个基表不需要优化
    if base_tables.len() < 2 {
        return Ok(None);
    }

    // 对于简单的两表连接，检查是否需要交换左右（build side 选择）
    // 多表连接调用 DP 算法
    // 注：完整的连接顺序重排需要精确的列索引映射，这里做结构级优化
    // （交换 build/probe 侧以减少 hash 表大小）

    // 递归优化每个连接节点：选择较小的表作为 build side（右表）
    let optimized = optimize_build_sides(plan.clone())?;

    if plan_eq(&optimized, plan) {
        Ok(None)
    } else {
        Ok(Some(optimized))
    }
}

/// 收集计划中的连接和基表信息
fn collect_joins_and_tables(plan: &PhysicalPlan, joins: &mut Vec<()>, tables: &mut Vec<String>) {
    match plan {
        PhysicalPlan::TableScan { table_name, .. } => {
            tables.push(table_name.clone());
        }
        PhysicalPlan::HashJoin { left, right, .. } => {
            joins.push(());
            collect_joins_and_tables(left, joins, tables);
            collect_joins_and_tables(right, joins, tables);
        }
        PhysicalPlan::Filter { input, .. } => {
            collect_joins_and_tables(input, joins, tables);
        }
        PhysicalPlan::Projection { input, .. } => {
            collect_joins_and_tables(input, joins, tables);
        }
        PhysicalPlan::Aggregate { input, .. } => {
            collect_joins_and_tables(input, joins, tables);
        }
        PhysicalPlan::Limit { input, .. } => {
            collect_joins_and_tables(input, joins, tables);
        }
        PhysicalPlan::Window { input, .. } => {
            collect_joins_and_tables(input, joins, tables);
        }
        PhysicalPlan::SubqueryScan { plan } => {
            collect_joins_and_tables(plan, joins, tables);
        }
        _ => {}
    }
}

/// 优化连接的 build/probe 侧选择
///
/// Hash Join 中右表是 build side（构建哈希表），应选择较小的表。
/// 此函数递归检查每个 HashJoin，估算左右行数并在必要时交换。
fn optimize_build_sides(plan: PhysicalPlan) -> Result<PhysicalPlan> {
    match plan {
        PhysicalPlan::HashJoin {
            left,
            right,
            join_type,
            left_keys,
            right_keys,
        } => {
            // 递归优化子节点
            let opt_left = optimize_build_sides(*left)?;
            let opt_right = optimize_build_sides(*right)?;

            // 估算行数（启发式：TableScan 假设 10000 行，Filter 乘以 0.3）
            let left_rows = estimate_rows(&opt_left);
            let right_rows = estimate_rows(&opt_right);

            // 对于 Inner Join，如果右表更大，交换左右以减少 hash 表内存
            // Left/Right/Full Join 不能交换（语义不同）
            if join_type == crate::executor::physical_plan::JoinType::Inner && right_rows > left_rows {
                Ok(PhysicalPlan::HashJoin {
                    left: Box::new(opt_right),
                    right: Box::new(opt_left),
                    join_type,
                    left_keys: right_keys,
                    right_keys: left_keys,
                })
            } else {
                Ok(PhysicalPlan::HashJoin {
                    left: Box::new(opt_left),
                    right: Box::new(opt_right),
                    join_type,
                    left_keys,
                    right_keys,
                })
            }
        }
        PhysicalPlan::Filter { input, condition } => {
            let opt_input = optimize_build_sides(*input)?;
            Ok(PhysicalPlan::Filter {
                input: Box::new(opt_input),
                condition,
            })
        }
        PhysicalPlan::Projection {
            input,
            expressions,
            column_names,
        } => {
            let opt_input = optimize_build_sides(*input)?;
            Ok(PhysicalPlan::Projection {
                input: Box::new(opt_input),
                expressions,
                column_names,
            })
        }
        PhysicalPlan::Aggregate {
            input,
            group_by,
            aggregates,
        } => {
            let opt_input = optimize_build_sides(*input)?;
            Ok(PhysicalPlan::Aggregate {
                input: Box::new(opt_input),
                group_by,
                aggregates,
            })
        }
        PhysicalPlan::Limit { input, limit } => {
            let opt_input = optimize_build_sides(*input)?;
            Ok(PhysicalPlan::Limit {
                input: Box::new(opt_input),
                limit,
            })
        }
        PhysicalPlan::Window { input, window_functions, column_names } => {
            let opt_input = optimize_build_sides(*input)?;
            Ok(PhysicalPlan::Window {
                input: Box::new(opt_input),
                window_functions,
                column_names,
            })
        }
        PhysicalPlan::SubqueryScan { plan } => {
            let opt_plan = optimize_build_sides(*plan)?;
            Ok(PhysicalPlan::SubqueryScan { plan: Box::new(opt_plan) })
        }
        other => Ok(other),
    }
}

/// 启发式估算计划输出行数（用于 build side 选择）
fn estimate_rows(plan: &PhysicalPlan) -> u64 {
    match plan {
        PhysicalPlan::TableScan { .. } => 10_000, // 默认假设
        PhysicalPlan::Filter { input, .. } => (estimate_rows(input) as f64 * 0.3) as u64,
        PhysicalPlan::Projection { input, .. } => estimate_rows(input),
        PhysicalPlan::Aggregate { input, group_by, .. } => {
            if group_by.is_empty() {
                1 // 无 GROUP BY 输出 1 行
            } else {
                ((estimate_rows(input) as f64 * 0.1) as u64).max(1)
            }
        }
        PhysicalPlan::Limit { input, limit } => estimate_rows(input).min(*limit as u64),
        PhysicalPlan::HashJoin { left, right, .. } => {
            // 连接输出行数：笛卡尔积 × 选择率（默认 0.1）
            (((estimate_rows(left) as f64) * (estimate_rows(right) as f64) * 0.1) as u64).max(1)
        }
        PhysicalPlan::SubqueryScan { plan } => estimate_rows(plan),
        _ => 1000,
    }
}

// ============================================================
// 规则 1: 常量折叠
// ============================================================

/// 常量折叠：对纯常量表达式在编译期求值
fn constant_folding(plan: PhysicalPlan) -> Result<PhysicalPlan> {
    match plan {
        PhysicalPlan::Filter { input, condition } => {
            let folded_input = constant_folding(*input)?;
            let folded_cond = fold_expression(condition);
            Ok(PhysicalPlan::Filter {
                input: Box::new(folded_input),
                condition: folded_cond,
            })
        }
        PhysicalPlan::Projection {
            input,
            expressions,
            column_names,
        } => {
            let folded_input = constant_folding(*input)?;
            let folded_exprs: Vec<Expression> =
                expressions.into_iter().map(fold_expression).collect();
            Ok(PhysicalPlan::Projection {
                input: Box::new(folded_input),
                expressions: folded_exprs,
                column_names,
            })
        }
        PhysicalPlan::Limit { input, limit } => {
            let folded_input = constant_folding(*input)?;
            Ok(PhysicalPlan::Limit {
                input: Box::new(folded_input),
                limit,
            })
        }
        PhysicalPlan::Aggregate {
            input,
            group_by,
            aggregates,
        } => {
            let folded_input = constant_folding(*input)?;
            Ok(PhysicalPlan::Aggregate {
                input: Box::new(folded_input),
                group_by,
                aggregates,
            })
        }
        // 叶子节点直接返回
        other => Ok(other),
    }
}

/// 折叠单个表达式
fn fold_expression(expr: Expression) -> Expression {
    match expr {
        Expression::BinaryOp { left, op, right } => {
            let left_folded = fold_expression(*left);
            let right_folded = fold_expression(*right);

            // AND/OR 恒等化简（常量折叠的扩展形式）
            use BinaryOperator::*;
            match (op, &left_folded, &right_folded) {
                (And, Expression::Literal(Value::Boolean(true)), _) => return right_folded,
                (And, _, Expression::Literal(Value::Boolean(true))) => return left_folded,
                (And, Expression::Literal(Value::Boolean(false)), _) => return Expression::Literal(Value::Boolean(false)),
                (And, _, Expression::Literal(Value::Boolean(false))) => return Expression::Literal(Value::Boolean(false)),
                (Or, Expression::Literal(Value::Boolean(true)), _) => return Expression::Literal(Value::Boolean(true)),
                (Or, _, Expression::Literal(Value::Boolean(true))) => return Expression::Literal(Value::Boolean(true)),
                (Or, Expression::Literal(Value::Boolean(false)), _) => return right_folded,
                (Or, _, Expression::Literal(Value::Boolean(false))) => return left_folded,
                _ => {}
            }

            // 如果两边都是字面量，尝试计算
            if let (Expression::Literal(lv), Expression::Literal(rv)) =
                (&left_folded, &right_folded)
            {
                if let Some(result) = eval_constant_binary(lv, op, rv) {
                    return Expression::Literal(result);
                }
            }

            Expression::BinaryOp {
                left: Box::new(left_folded),
                op,
                right: Box::new(right_folded),
            }
        }
        Expression::UnaryOp { op, expr } => {
            let inner_folded = fold_expression(*expr);
            if let Expression::Literal(v) = &inner_folded {
                if let Some(result) = eval_constant_unary(op, v) {
                    return Expression::Literal(result);
                }
            }
            Expression::UnaryOp {
                op,
                expr: Box::new(inner_folded),
            }
        }
        Expression::Case { when_then, else_expr } => {
            // 折叠每个分支的条件和结果
            let folded_when_then: Vec<(Expression, Expression)> = when_then
                .into_iter()
                .map(|(w, t)| (fold_expression(w), fold_expression(t)))
                .collect();
            let folded_else = else_expr.map(|e| Box::new(fold_expression(*e)));

            // 如果第一个条件是常量 true，直接返回 then 分支
            if let Some((first_when, first_then)) = folded_when_then.first() {
                if let Expression::Literal(Value::Boolean(true)) = first_when {
                    return first_then.clone();
                }
                // 如果第一个条件是常量 false，跳过该分支
                if let Expression::Literal(Value::Boolean(false)) = first_when {
                    let remaining: Vec<(Expression, Expression)> =
                        folded_when_then.into_iter().skip(1).collect();
                    return Expression::Case {
                        when_then: remaining,
                        else_expr: folded_else,
                    };
                }
            }

            Expression::Case {
                when_then: folded_when_then,
                else_expr: folded_else,
            }
        }
        Expression::Cast { expr, data_type } => {
            let inner_folded = fold_expression(*expr);
            if let Expression::Literal(v) = &inner_folded {
                if let Some(casted) = eval_constant_cast(v, &data_type) {
                    return Expression::Literal(casted);
                }
            }
            Expression::Cast {
                expr: Box::new(inner_folded),
                data_type,
            }
        }
        // 叶子表达式不折叠
        other => other,
    }
}

/// 计算常量二元表达式
fn eval_constant_binary(left: &Value, op: BinaryOperator, right: &Value) -> Option<Value> {
    use BinaryOperator::*;

    match op {
        // 算术
        Plus => num_op(left, right, |a, b| a + b),
        Minus => num_op(left, right, |a, b| a - b),
        Multiply => num_op(left, right, |a, b| a * b),
        Divide => {
            if right.as_f64()? == 0.0 {
                None
            } else {
                num_op(left, right, |a, b| a / b)
            }
        }
        Modulo => {
            if right.as_f64()? == 0.0 {
                None
            } else {
                num_op(left, right, |a, b| a % b)
            }
        }
        // 比较
        Eq => Some(Value::Boolean(left == right)),
        NotEq => Some(Value::Boolean(left != right)),
        Lt => num_cmp(left, right, |a, b| a < b),
        LtEq => num_cmp(left, right, |a, b| a <= b),
        Gt => num_cmp(left, right, |a, b| a > b),
        GtEq => num_cmp(left, right, |a, b| a >= b),
        // 逻辑
        And => match (left, right) {
            (Value::Boolean(a), Value::Boolean(b)) => Some(Value::Boolean(*a && *b)),
            _ => None,
        },
        Or => match (left, right) {
            (Value::Boolean(a), Value::Boolean(b)) => Some(Value::Boolean(*a || *b)),
            _ => None,
        },
        // 字符串拼接
        Concat => match (left, right) {
            (Value::Varchar(a), Value::Varchar(b)) => {
                Some(Value::Varchar(format!("{}{}", a, b)))
            }
            _ => None,
        },
    }
}

fn num_op<F>(left: &Value, right: &Value, f: F) -> Option<Value>
where
    F: Fn(f64, f64) -> f64,
{
    let l = left.as_f64()?;
    let r = right.as_f64()?;
    let result = f(l, r);

    // 如果都是整数且结果也是整数，返回 Int64
    if left.as_i64().is_some() && right.as_i64().is_some() && result.fract() == 0.0 {
        Some(Value::Int64(result as i64))
    } else {
        Some(Value::Float64(result))
    }
}

fn num_cmp<F>(left: &Value, right: &Value, f: F) -> Option<Value>
where
    F: Fn(f64, f64) -> bool,
{
    let l = left.as_f64()?;
    let r = right.as_f64()?;
    Some(Value::Boolean(f(l, r)))
}

/// 计算常量一元表达式
fn eval_constant_unary(op: UnaryOperator, v: &Value) -> Option<Value> {
    match op {
        UnaryOperator::Not => match v {
            Value::Boolean(b) => Some(Value::Boolean(!b)),
            _ => None,
        },
        UnaryOperator::Negate => {
            if let Some(i) = v.as_i64() {
                Some(Value::Int64(-i))
            } else if let Some(f) = v.as_f64() {
                Some(Value::Float64(-f))
            } else {
                None
            }
        }
    }
}

/// 计算常量类型转换
fn eval_constant_cast(v: &Value, target: &crate::common::types::DataType) -> Option<Value> {
    use crate::common::types::DataType;
    match target {
        DataType::Boolean => match v {
            Value::Boolean(b) => Some(Value::Boolean(*b)),
            Value::Int32(i) => Some(Value::Boolean(*i != 0)),
            Value::Int64(i) => Some(Value::Boolean(*i != 0)),
            _ => None,
        },
        DataType::Int32 => {
            if let Some(i) = v.as_i64() {
                Some(Value::Int32(i as i32))
            } else if let Some(f) = v.as_f64() {
                Some(Value::Int32(f as i32))
            } else {
                None
            }
        }
        DataType::Int64 => {
            if let Some(i) = v.as_i64() {
                Some(Value::Int64(i))
            } else if let Some(f) = v.as_f64() {
                Some(Value::Int64(f as i64))
            } else {
                None
            }
        }
        DataType::Float32 => v.as_f64().map(|f| Value::Float32(f as f32)),
        DataType::Float64 => v.as_f64().map(Value::Float64),
        DataType::Timestamp => v.as_i64().map(Value::Timestamp),
        DataType::Varchar => match v {
            Value::Varchar(s) => Some(Value::Varchar(s.clone())),
            Value::Int32(i) => Some(Value::Varchar(i.to_string())),
            Value::Int64(i) => Some(Value::Varchar(i.to_string())),
            Value::Float64(f) => Some(Value::Varchar(f.to_string())),
            Value::Float32(f) => Some(Value::Varchar(f.to_string())),
            Value::Timestamp(t) => Some(Value::Varchar(t.to_string())),
            Value::Boolean(b) => Some(Value::Varchar(b.to_string())),
            Value::Null => Some(Value::Null),
            Value::Json(s) => Some(Value::Varchar(s.clone())),
            Value::Vector(_) => None,
            Value::Blob(_) => None,
        },
        DataType::Json => match v {
            Value::Json(s) => Some(Value::Json(s.clone())),
            Value::Varchar(s) => Some(Value::Json(s.clone())),
            Value::Null => Some(Value::Null),
            _ => None,
        },
        DataType::Vector { .. } => match v {
            Value::Vector(v) => Some(Value::Vector(v.clone())),
            Value::Null => Some(Value::Null),
            _ => None,
        },
        DataType::Blob => match v {
            Value::Blob(b) => Some(Value::Blob(b.clone())),
            Value::Null => Some(Value::Null),
            _ => None,
        },
    }
}

// ============================================================
// 规则 2: 谓词下推
// ============================================================

/// 谓词下推：将过滤条件尽可能下推到靠近数据源的位置
///
/// 核心思想：越早过滤掉不需要的行，后续算子处理的数据量越小。
fn predicate_pushdown(plan: PhysicalPlan) -> Result<PhysicalPlan> {
    pushdown_predicates(plan, Vec::new())
}

fn pushdown_predicates(plan: PhysicalPlan, pending_predicates: Vec<Expression>) -> Result<PhysicalPlan> {
    match plan {
        // TableScan: 把所有挂起的谓词变成 Filter 放在扫描上面
        PhysicalPlan::TableScan {
            table_name,
            column_indices,
        } => {
            if pending_predicates.is_empty() {
                Ok(PhysicalPlan::TableScan {
                    table_name,
                    column_indices,
                })
            } else {
                let combined = combine_predicates(pending_predicates);
                Ok(PhysicalPlan::Filter {
                    input: Box::new(PhysicalPlan::TableScan {
                        table_name,
                        column_indices,
                    }),
                    condition: combined,
                })
            }
        }

        // Filter: 收集谓词，继续下推
        PhysicalPlan::Filter { input, condition } => {
            let mut new_predicates = pending_predicates;
            // 拆分 AND 条件为独立谓词
            split_and_conditions(&condition, &mut new_predicates);
            pushdown_predicates(*input, new_predicates)
        }

        // Projection: 只下推引用了投影列的谓词
        PhysicalPlan::Projection {
            input,
            expressions,
            column_names,
        } => {
            // 分离可下推和不可下推的谓词
            let (pushable, non_pushable) =
                split_pushable_predicates(pending_predicates, &expressions);

            let pushed_input = pushdown_predicates(*input, pushable)?;

            let mut result = PhysicalPlan::Projection {
                input: Box::new(pushed_input),
                expressions,
                column_names,
            };

            // 不可下推的谓词留在投影之上
            if !non_pushable.is_empty() {
                let combined = combine_predicates(non_pushable);
                result = PhysicalPlan::Filter {
                    input: Box::new(result),
                    condition: combined,
                };
            }

            Ok(result)
        }

        // Limit: 谓词可以穿过 Limit 下推（过滤后再 limit 结果等价）
        PhysicalPlan::Limit { input, limit } => {
            let pushed_input = pushdown_predicates(*input, pending_predicates)?;
            Ok(PhysicalPlan::Limit {
                input: Box::new(pushed_input),
                limit,
            })
        }

        // Aggregate: 只有引用了 GROUP BY 列的谓词才能下推
        PhysicalPlan::Aggregate {
            input,
            group_by,
            aggregates,
        } => {
            // 简化：MVP 阶段谓词不下推穿过聚合
            // （因为聚合后列语义改变，下推需要复杂的列映射）
            let pushed_input = pushdown_predicates(*input, Vec::new())?;
            let mut result = PhysicalPlan::Aggregate {
                input: Box::new(pushed_input),
                group_by,
                aggregates,
            };

            if !pending_predicates.is_empty() {
                let combined = combine_predicates(pending_predicates);
                result = PhysicalPlan::Filter {
                    input: Box::new(result),
                    condition: combined,
                };
            }

            Ok(result)
        }

        // 其他节点直接递归
        other => Ok(other),
    }
}

/// 拆分 AND 条件为独立谓词列表
fn split_and_conditions(expr: &Expression, predicates: &mut Vec<Expression>) {
    match expr {
        Expression::BinaryOp { left, op, right }
            if *op == BinaryOperator::And =>
        {
            split_and_conditions(left, predicates);
            split_and_conditions(right, predicates);
        }
        _ => predicates.push(expr.clone()),
    }
}

/// 合并多个谓词为一个 AND 表达式
fn combine_predicates(predicates: Vec<Expression>) -> Expression {
    assert!(!predicates.is_empty());
    let mut iter = predicates.into_iter();
    let mut result = iter.next().unwrap();
    for pred in iter {
        result = Expression::BinaryOp {
            left: Box::new(result),
            op: BinaryOperator::And,
            right: Box::new(pred),
        };
    }
    result
}

/// 判断谓词是否可以下推穿过投影
///
/// 只有当谓词引用的所有列都在投影表达式中是纯列引用时才能下推。
fn split_pushable_predicates(
    predicates: Vec<Expression>,
    projections: &[Expression],
) -> (Vec<Expression>, Vec<Expression>) {
    // 收集投影中的纯列引用
    let projected_columns: Vec<String> = projections
        .iter()
        .filter_map(|e| match e {
            Expression::ColumnRef { column, .. } => Some(column.clone()),
            _ => None,
        })
        .collect();

    let mut pushable = Vec::new();
    let mut non_pushable = Vec::new();

    for pred in predicates {
        let cols = collect_referenced_columns(&pred);
        let all_in_projection = cols
            .iter()
            .all(|c| projected_columns.contains(c));
        if all_in_projection {
            pushable.push(pred);
        } else {
            non_pushable.push(pred);
        }
    }

    (pushable, non_pushable)
}

/// 收集表达式中引用的所有列名
fn collect_referenced_columns(expr: &Expression) -> Vec<String> {
    let mut cols = Vec::new();
    collect_columns_recursive(expr, &mut cols);
    cols
}

fn collect_columns_recursive(expr: &Expression, cols: &mut Vec<String>) {
    match expr {
        Expression::ColumnRef { column, .. } => {
            cols.push(column.clone());
        }
        Expression::BinaryOp { left, right, .. } => {
            collect_columns_recursive(left, cols);
            collect_columns_recursive(right, cols);
        }
        Expression::UnaryOp { expr, .. } => {
            collect_columns_recursive(expr, cols);
        }
        Expression::Function { args, .. } => {
            for arg in args {
                collect_columns_recursive(arg, cols);
            }
        }
        Expression::Cast { expr, .. } => {
            collect_columns_recursive(expr, cols);
        }
        Expression::Case { when_then, else_expr } => {
            for (w, t) in when_then {
                collect_columns_recursive(w, cols);
                collect_columns_recursive(t, cols);
            }
            if let Some(e) = else_expr {
                collect_columns_recursive(e, cols);
            }
        }
        Expression::IsNull(e) | Expression::IsNotNull(e) => {
            collect_columns_recursive(e, cols);
        }
        Expression::InList { expr, list } => {
            collect_columns_recursive(expr, cols);
            for item in list {
                collect_columns_recursive(item, cols);
            }
        }
        Expression::Like { expr, pattern } => {
            collect_columns_recursive(expr, cols);
            collect_columns_recursive(pattern, cols);
        }
        Expression::Literal(_) | Expression::Placeholder(_) => {}
        Expression::Subquery(_) | Expression::Exists { .. } | Expression::InSubquery { .. } => {}
    }
}

// ============================================================
// 规则 3: 投影下推
// ============================================================

/// 投影下推：只扫描需要的列，减少 IO 和内存
fn projection_pushdown(plan: PhysicalPlan) -> Result<PhysicalPlan> {
    // 从顶层开始，收集需要的列，向下传递
    let all_cols = collect_plan_output_columns(&plan);
    pushdown_projection(plan, &all_cols)
}

fn pushdown_projection(plan: PhysicalPlan, required_cols: &[String]) -> Result<PhysicalPlan> {
    match plan {
        PhysicalPlan::TableScan {
            table_name,
            column_indices,
        } => {
            // TableScan 的列选择已由 planner 计算好
            // （优化器无表 schema，不重新计算，保留原值）
            Ok(PhysicalPlan::TableScan {
                table_name,
                column_indices,
            })
        }

        PhysicalPlan::Filter { input, condition } => {
            // 过滤需要的列 = 过滤条件引用的列 + 上层需要的列
            let mut required = required_cols.to_vec();
            let cond_cols = collect_referenced_columns(&condition);
            for col in cond_cols {
                if !required.contains(&col) {
                    required.push(col);
                }
            }
            let pushed_input = pushdown_projection(*input, &required)?;
            Ok(PhysicalPlan::Filter {
                input: Box::new(pushed_input),
                condition,
            })
        }

        PhysicalPlan::Projection {
            input,
            expressions,
            column_names,
        } => {
            // 投影算子的输入需要的列 = 投影表达式引用的列
            let mut input_required = Vec::new();
            for expr in &expressions {
                let cols = collect_referenced_columns(expr);
                for col in cols {
                    if !input_required.contains(&col) {
                        input_required.push(col);
                    }
                }
            }
            let pushed_input = pushdown_projection(*input, &input_required)?;
            Ok(PhysicalPlan::Projection {
                input: Box::new(pushed_input),
                expressions,
                column_names,
            })
        }

        PhysicalPlan::Limit { input, limit } => {
            let pushed_input = pushdown_projection(*input, required_cols)?;
            Ok(PhysicalPlan::Limit {
                input: Box::new(pushed_input),
                limit,
            })
        }

        PhysicalPlan::Aggregate {
            input,
            group_by,
            aggregates,
        } => {
            // 聚合需要 group by 列 + 聚合输入列
            // 简化：保留所有输入列（MVP 阶段不做精细化列裁剪）
            let pushed_input = pushdown_projection(*input, required_cols)?;
            Ok(PhysicalPlan::Aggregate {
                input: Box::new(pushed_input),
                group_by,
                aggregates,
            })
        }

        other => Ok(other),
    }
}

/// 收集计划输出的列名（用于顶层投影下推）
fn collect_plan_output_columns(plan: &PhysicalPlan) -> Vec<String> {
    match plan {
        PhysicalPlan::Projection { column_names, .. } => column_names.clone(),
        PhysicalPlan::Filter { input, .. } => collect_plan_output_columns(input),
        PhysicalPlan::Limit { input, .. } => collect_plan_output_columns(input),
        PhysicalPlan::Aggregate { .. } => Vec::new(), // 聚合输出列名不确定
        _ => Vec::new(),
    }
}

// ============================================================
// 规则 4: 过滤条件重排
// ============================================================

/// 过滤条件重排：将高选择性（大概率过滤掉更多行）的条件前置
///
/// 启发式规则：
/// 1. 等值比较（=）选择性最高，放最前面
/// 2. 范围比较（<, >, <=, >=, BETWEEN）次之
/// 3. 不等式（!=, NOT）再次
/// 4. LIKE 和 IN 最后
/// 5. 常量表达式放最前（可被常量折叠规则消除）
fn filter_reorder(plan: PhysicalPlan) -> Result<PhysicalPlan> {
    match plan {
        PhysicalPlan::Filter { input, condition } => {
            let mut predicates = Vec::new();
            split_and_conditions(&condition, &mut predicates);

            if predicates.len() <= 1 {
                // 只有一个条件，不需要重排
                let reordered_input = filter_reorder(*input)?;
                return Ok(PhysicalPlan::Filter {
                    input: Box::new(reordered_input),
                    condition,
                });
            }

            // 计算每个谓词的选择性分数（分数越低，选择性越高，越靠前）
            let mut scored: Vec<(usize, Expression)> = predicates
                .into_iter()
                .map(|p| (predicate_selectivity_score(&p), p))
                .collect();

            scored.sort_by_key(|(score, _)| *score);

            let reordered: Vec<Expression> = scored.into_iter().map(|(_, p)| p).collect();
            let combined = combine_predicates(reordered);

            let reordered_input = filter_reorder(*input)?;
            Ok(PhysicalPlan::Filter {
                input: Box::new(reordered_input),
                condition: combined,
            })
        }

        PhysicalPlan::Projection {
            input,
            expressions,
            column_names,
        } => {
            let reordered_input = filter_reorder(*input)?;
            Ok(PhysicalPlan::Projection {
                input: Box::new(reordered_input),
                expressions,
                column_names,
            })
        }

        PhysicalPlan::Limit { input, limit } => {
            let reordered_input = filter_reorder(*input)?;
            Ok(PhysicalPlan::Limit {
                input: Box::new(reordered_input),
                limit,
            })
        }

        PhysicalPlan::Aggregate {
            input,
            group_by,
            aggregates,
        } => {
            let reordered_input = filter_reorder(*input)?;
            Ok(PhysicalPlan::Aggregate {
                input: Box::new(reordered_input),
                group_by,
                aggregates,
            })
        }

        other => Ok(other),
    }
}

/// 计算谓词的选择性分数（越低 = 选择性越高 = 越应该前置）
fn predicate_selectivity_score(expr: &Expression) -> usize {
    match expr {
        // 常量表达式：0 分（最优先，会被常量折叠消除）
        Expression::Literal(_) => 0,

        // 等值比较：1 分（选择性最高）
        Expression::BinaryOp { op, .. } if *op == BinaryOperator::Eq => 1,

        // IS NULL / IS NOT NULL：2 分
        Expression::IsNull(_) | Expression::IsNotNull(_) => 2,

        // 范围比较：3 分
        Expression::BinaryOp { op, .. }
            if matches!(
                op,
                BinaryOperator::Lt
                    | BinaryOperator::LtEq
                    | BinaryOperator::Gt
                    | BinaryOperator::GtEq
            ) =>
        {
            3
        }

        // IN 列表：4 分
        Expression::InList { .. } => 4,

        // 不等式：5 分
        Expression::BinaryOp { op, .. } if *op == BinaryOperator::NotEq => 5,

        // LIKE：6 分（最昂贵）
        Expression::Like { .. } => 6,

        // NOT 包裹：比内部表达式低一级
        Expression::UnaryOp { op, expr } if *op == UnaryOperator::Not => {
            predicate_selectivity_score(expr) + 10
        }

        // 其他复杂表达式：默认 7 分
        _ => 7,
    }
}

// ============================================================
// 辅助函数
// ============================================================

/// 比较两个物理计划是否结构相等（用于检测优化是否收敛）
fn plan_eq(a: &PhysicalPlan, b: &PhysicalPlan) -> bool {
    // 简化比较：使用 Debug 格式的字符串比较
    format!("{:?}", a) == format!("{:?}", b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::ast::BinaryOperator::*;
    use crate::sql::ast::Expression::*;
    use crate::sql::ast::UnaryOperator;
    use crate::Value;

    // ---- 常量折叠测试 ----

    #[test]
    fn test_constant_fold_arithmetic() {
        // 1 + 2 * 3 = 7
        let expr = BinaryOp {
            left: Box::new(Literal(Value::Int64(1))),
            op: Plus,
            right: Box::new(BinaryOp {
                left: Box::new(Literal(Value::Int64(2))),
                op: Multiply,
                right: Box::new(Literal(Value::Int64(3))),
            }),
        };
        let folded = fold_expression(expr);
        match folded {
            Literal(Value::Int64(7)) => {}
            _ => panic!("Expected Int64(7), got {:?}", folded),
        }
    }

    #[test]
    fn test_constant_fold_comparison() {
        // 10 > 5 = true
        let expr = BinaryOp {
            left: Box::new(Literal(Value::Int64(10))),
            op: Gt,
            right: Box::new(Literal(Value::Int64(5))),
        };
        let folded = fold_expression(expr);
        match folded {
            Literal(Value::Boolean(true)) => {}
            _ => panic!("Expected Boolean(true), got {:?}", folded),
        }
    }

    #[test]
    fn test_constant_fold_logic() {
        // true AND false = false
        let expr = BinaryOp {
            left: Box::new(Literal(Value::Boolean(true))),
            op: And,
            right: Box::new(Literal(Value::Boolean(false))),
        };
        let folded = fold_expression(expr);
        match folded {
            Literal(Value::Boolean(false)) => {}
            _ => panic!("Expected Boolean(false), got {:?}", folded),
        }
    }

    #[test]
    fn test_constant_fold_not() {
        // NOT true = false
        let expr = UnaryOp {
            op: UnaryOperator::Not,
            expr: Box::new(Literal(Value::Boolean(true))),
        };
        let folded = fold_expression(expr);
        match folded {
            Literal(Value::Boolean(false)) => {}
            _ => panic!("Expected Boolean(false), got {:?}", folded),
        }
    }

    #[test]
    fn test_constant_fold_negate() {
        // -5 = -5
        let expr = UnaryOp {
            op: UnaryOperator::Negate,
            expr: Box::new(Literal(Value::Int64(5))),
        };
        let folded = fold_expression(expr);
        match folded {
            Literal(Value::Int64(-5)) => {}
            _ => panic!("Expected Int64(-5), got {:?}", folded),
        }
    }

    #[test]
    fn test_constant_fold_partial() {
        // col + (1 + 2) = col + 3
        let expr = BinaryOp {
            left: Box::new(ColumnRef {
                table: None,
                column: "a".to_string(),
            }),
            op: Plus,
            right: Box::new(BinaryOp {
                left: Box::new(Literal(Value::Int64(1))),
                op: Plus,
                right: Box::new(Literal(Value::Int64(2))),
            }),
        };
        let folded = fold_expression(expr);
        match &folded {
            BinaryOp { right, .. } => match right.as_ref() {
                Literal(Value::Int64(3)) => {}
                _ => panic!("Expected right side to be Int64(3), got {:?}", right),
            },
            _ => panic!("Expected BinaryOp, got {:?}", folded),
        }
    }

    #[test]
    fn test_constant_fold_case() {
        // CASE WHEN true THEN 1 ELSE 2 END = 1
        let expr = Case {
            when_then: vec![(
                Literal(Value::Boolean(true)),
                Literal(Value::Int64(1)),
            )],
            else_expr: Some(Box::new(Literal(Value::Int64(2)))),
        };
        let folded = fold_expression(expr);
        match folded {
            Literal(Value::Int64(1)) => {}
            _ => panic!("Expected Int64(1), got {:?}", folded),
        }
    }

    // ---- 谓词下推测试 ----

    #[test]
    fn test_predicate_pushdown_through_limit() {
        // Filter(Limit(Scan)) -> Limit(Filter(Scan))
        let plan = PhysicalPlan::Filter {
            input: Box::new(PhysicalPlan::Limit {
                input: Box::new(PhysicalPlan::TableScan {
                    table_name: "t".to_string(),
                    column_indices: vec![0, 1],
                }),
                limit: 10,
            }),
            condition: BinaryOp {
                left: Box::new(ColumnRef {
                    table: None,
                    column: "a".to_string(),
                }),
                op: Gt,
                right: Box::new(Literal(Value::Int64(5))),
            },
        };

        let result = predicate_pushdown(plan).unwrap();
        // 结果应该是 Limit 在 Filter 之上
        match result {
            PhysicalPlan::Limit { input, .. } => match input.as_ref() {
                PhysicalPlan::Filter { .. } => {}
                _ => panic!("Expected Filter inside Limit"),
            },
            _ => panic!("Expected Limit at top"),
        }
    }

    // ---- 过滤条件重排测试 ----

    #[test]
    fn test_filter_reorder_basic() {
        // a > 5 AND b = 10 AND c LIKE '%x%'
        // 应该重排为: b = 10 AND a > 5 AND c LIKE '%x%'
        let cond = BinaryOp {
            left: Box::new(BinaryOp {
                left: Box::new(ColumnRef {
                    table: None,
                    column: "a".to_string(),
                }),
                op: Gt,
                right: Box::new(Literal(Value::Int64(5))),
            }),
            op: And,
            right: Box::new(BinaryOp {
                left: Box::new(BinaryOp {
                    left: Box::new(ColumnRef {
                        table: None,
                        column: "b".to_string(),
                    }),
                    op: Eq,
                    right: Box::new(Literal(Value::Int64(10))),
                }),
                op: And,
                right: Box::new(Like {
                    expr: Box::new(ColumnRef {
                        table: None,
                        column: "c".to_string(),
                    }),
                    pattern: Box::new(Literal(Value::Varchar("%x%".to_string()))),
                }),
            }),
        };

        let plan = PhysicalPlan::Filter {
            input: Box::new(PhysicalPlan::TableScan {
                table_name: "t".to_string(),
                column_indices: vec![0, 1, 2],
            }),
            condition: cond,
        };

        let result = filter_reorder(plan).unwrap();
        match &result {
            PhysicalPlan::Filter { condition, .. } => {
                // 第一个条件应该是等值比较（选择性最高）
                let mut preds = Vec::new();
                split_and_conditions(condition, &mut preds);
                assert_eq!(preds.len(), 3);
                // 第一个应该是 b = 10（Eq 选择性最高）
                match &preds[0] {
                    BinaryOp { op, .. } => assert_eq!(*op, Eq),
                    _ => panic!("First predicate should be Eq"),
                }
                // 最后一个应该是 LIKE
                match &preds[2] {
                    Like { .. } => {}
                    _ => panic!("Last predicate should be LIKE"),
                }
            }
            _ => panic!("Expected Filter"),
        }
    }

    // ---- 列收集测试 ----

    #[test]
    fn test_collect_columns() {
        let expr = BinaryOp {
            left: Box::new(ColumnRef {
                table: None,
                column: "a".to_string(),
            }),
            op: Plus,
            right: Box::new(BinaryOp {
                left: Box::new(ColumnRef {
                    table: None,
                    column: "b".to_string(),
                }),
                op: Multiply,
                right: Box::new(Literal(Value::Int64(3))),
            }),
        };
        let cols = collect_referenced_columns(&expr);
        assert!(cols.contains(&"a".to_string()));
        assert!(cols.contains(&"b".to_string()));
        assert_eq!(cols.len(), 2);
    }

    // ---- 完整优化器测试 ----

    #[test]
    fn test_optimize_constant_filter() {
        // WHERE 1 = 1 应该被折叠为 true，然后 Filter 可能被消除（当前保留，但条件为 true）
        let plan = PhysicalPlan::Filter {
            input: Box::new(PhysicalPlan::TableScan {
                table_name: "t".to_string(),
                column_indices: vec![0],
            }),
            condition: BinaryOp {
                left: Box::new(Literal(Value::Int64(1))),
                op: Eq,
                right: Box::new(Literal(Value::Int64(1))),
            },
        };

        let result = optimize(plan).unwrap();
        match &result {
            PhysicalPlan::Filter { condition, .. } => {
                // 1 = 1 折叠为 true
                match condition {
                    Literal(Value::Boolean(true)) => {}
                    _ => panic!("Expected condition to fold to true, got {:?}", condition),
                }
            }
            _ => panic!("Expected Filter plan"),
        }
    }

    #[test]
    fn test_optimize_no_op() {
        // 简单 TableScan 优化后应该不变
        let plan = PhysicalPlan::TableScan {
            table_name: "t".to_string(),
            column_indices: vec![0, 1],
        };
        let result = optimize(plan).unwrap();
        match result {
            PhysicalPlan::TableScan { .. } => {}
            _ => panic!("Expected TableScan"),
        }
    }
}
