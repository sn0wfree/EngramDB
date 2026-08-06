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
use log::trace;

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

        // 规则 5: 恒等投影消除（IdentityProjection Elimination）
        // 当 Projection 节点的所有表达式都是 ColumnRef、且与 TableScan column_indices 一一对应时，
        // 该 Projection 是恒等变换，直接消除。
        let id_elim = identity_projection_elimination(current.clone());
        if !plan_eq(&id_elim, &current) {
            changed = true;
            current = id_elim;
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
        PhysicalPlan::InsertSelect { source, .. } => {
            collect_joins_and_tables(source, joins, tables);
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
        PhysicalPlan::InsertSelect { table_name, columns, source } => {
            let opt_source = optimize_build_sides(*source)?;
            Ok(PhysicalPlan::InsertSelect {
                table_name,
                columns,
                source: Box::new(opt_source),
            })
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
        PhysicalPlan::InsertSelect { source, .. } => estimate_rows(source),
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
            Value::VectorInt8(_) => None,
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
            Value::VectorInt8(v) => Some(Value::VectorInt8(v.clone())),
            Value::Null => Some(Value::Null),
            _ => None,
        },
        DataType::Blob => match v {
            Value::Blob(b) => Some(Value::Blob(b.clone())),
            Value::Null => Some(Value::Null),
            _ => None,
        },
        DataType::VectorInt8 { .. } => match v {
            Value::VectorInt8(v) => Some(Value::VectorInt8(v.clone())),
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

        // HashJoin: 谓词不能下推穿过 JOIN（② 修复）
        //
        // WHERE 谓词语义在 JOIN 之后过滤；若下推到 JOIN 一侧（尤其
        // LEFT/RIGHT/FULL 的非保留侧），会把本应 NULL 补齐的行提前过滤，
        // 改变结果集。因此 JOIN 之上的谓词原样保留为 JOIN 之上的 Filter，
        // 仅递归处理两侧内部的谓词。
        PhysicalPlan::HashJoin {
            left,
            right,
            join_type,
            left_keys,
            right_keys,
        } => {
            let opt_left = pushdown_predicates(*left, Vec::new())?;
            let opt_right = pushdown_predicates(*right, Vec::new())?;
            let mut result = PhysicalPlan::HashJoin {
                left: Box::new(opt_left),
                right: Box::new(opt_right),
                join_type,
                left_keys,
                right_keys,
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

        // CrossJoin: 与 HashJoin 相同，谓词保留在 JOIN 之上
        // （下推会改变笛卡尔积 → 过滤的语义顺序；且 WHERE 引用任一
        // 侧列，直接透传会丢失过滤）
        PhysicalPlan::CrossJoin { left, right } => {
            let opt_left = pushdown_predicates(*left, Vec::new())?;
            let opt_right = pushdown_predicates(*right, Vec::new())?;
            let mut result = PhysicalPlan::CrossJoin {
                left: Box::new(opt_left),
                right: Box::new(opt_right),
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

/// 恒等投影消除（IdentityProjection Elimination）
///
/// 检测 `Projection { all ColumnRef, 输出顺序与输入列一致 }` 的恒等变换，
/// 直接消除该节点以避免 `chunk.columns[idx].clone()` × N 次重复深克隆。
///
/// 适用场景：`SELECT *` 与 `SELECT col1, col2, ...`（显式列出且与扫描列顺序一致）。
///
/// 注意：planner 的 `collect_referenced_columns` 已保证 `scan_column_indices` 与 SELECT 列顺序一致，
/// 所以 `expressions[i].column` 必对应 `scan_column_indices[i]`。IdentityProjection 检查仅需验证：
/// - 所有表达式都是 ColumnRef
/// - expressions 数量 == column_indices 数量
fn identity_projection_elimination(plan: PhysicalPlan) -> PhysicalPlan {
    match plan {
        PhysicalPlan::Projection { input, expressions, column_names } => {
            // 递归处理子节点
            let new_input = identity_projection_elimination(*input);

            // 仅处理输入是 TableScan 的情况
            if let PhysicalPlan::TableScan { ref table_name, ref column_indices } = new_input {
                // 检查：所有表达式都是纯 ColumnRef
                if !expressions.iter().all(|e| matches!(e, Expression::ColumnRef { .. })) {
                    return PhysicalPlan::Projection {
                        input: Box::new(new_input),
                        expressions,
                        column_names,
                    };
                }

                // 检查：expressions 数量 == scan 的列数
                if expressions.len() != column_indices.len() {
                    return PhysicalPlan::Projection {
                        input: Box::new(new_input),
                        expressions,
                        column_names,
                    };
                }

                // 检查：expressions[i].column == column_names[i]（projection 输出列名一致性）
                // 由于 planner 保证 column_indices 顺序与 SELECT 列表一致，
                // 所以 scan_column_names[i] == column_names[i] == expressions[i].column
                let matches = expressions.iter().enumerate().all(|(i, expr)| {
                    if let Expression::ColumnRef { column, .. } = expr {
                        column_names.get(i).map(|n| n == column).unwrap_or(false)
                    } else {
                        false
                    }
                });

                if !matches {
                    return PhysicalPlan::Projection {
                        input: Box::new(new_input),
                        expressions,
                        column_names,
                    };
                }

                // 所有条件满足：恒等投影，直接消除
                trace!("IdentityProjection eliminated for table '{}'", table_name);
                return new_input;
            }

            // 非 TableScan 输入：保留 Projection 节点
            PhysicalPlan::Projection {
                input: Box::new(new_input),
                expressions,
                column_names,
            }
        }
        PhysicalPlan::Filter { input, condition } => PhysicalPlan::Filter {
            input: Box::new(identity_projection_elimination(*input)),
            condition,
        },
        PhysicalPlan::Limit { input, limit } => PhysicalPlan::Limit {
            input: Box::new(identity_projection_elimination(*input)),
            limit,
        },
        PhysicalPlan::Sort { input, sort_keys, limit } => PhysicalPlan::Sort {
            input: Box::new(identity_projection_elimination(*input)),
            sort_keys,
            limit,
        },
        PhysicalPlan::Aggregate { input, group_by, aggregates } => PhysicalPlan::Aggregate {
            input: Box::new(identity_projection_elimination(*input)),
            group_by,
            aggregates,
        },
        PhysicalPlan::Distinct { input } => PhysicalPlan::Distinct {
            input: Box::new(identity_projection_elimination(*input)),
        },
        other => other,
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

    // ============ 常量折叠深路径 ============

    #[test]
    fn test_fold_division_and_modulo() {
        // 10 / 3 = 3.33…（非整数结果 → Float64，无整数截断）
        let e = BinaryOp {
            left: Box::new(Literal(Value::Int64(10))),
            op: Divide,
            right: Box::new(Literal(Value::Int64(3))),
        };
        assert!(matches!(fold_expression(e),
            Literal(Value::Float64(v)) if (v - 10.0 / 3.0).abs() < 1e-9));
        // 10 / 2 = 5（整数结果 → Int64）
        let e = BinaryOp {
            left: Box::new(Literal(Value::Int64(10))),
            op: Divide,
            right: Box::new(Literal(Value::Int64(2))),
        };
        assert!(matches!(fold_expression(e), Literal(Value::Int64(5))));
        // 1 / 2 = 0.5（结果非整 → Float64）
        let e = BinaryOp {
            left: Box::new(Literal(Value::Int64(1))),
            op: Divide,
            right: Box::new(Literal(Value::Int64(2))),
        };
        assert!(matches!(fold_expression(e),
            Literal(Value::Float64(v)) if (v - 0.5).abs() < 1e-9));
        // 10 % 3 = 1
        let e = BinaryOp {
            left: Box::new(Literal(Value::Int64(10))),
            op: Modulo,
            right: Box::new(Literal(Value::Int64(3))),
        };
        assert!(matches!(fold_expression(e), Literal(Value::Int64(1))));
        // 除零：不折叠，保留表达式
        let e = BinaryOp {
            left: Box::new(Literal(Value::Int64(1))),
            op: Divide,
            right: Box::new(Literal(Value::Int64(0))),
        };
        assert!(matches!(fold_expression(e), BinaryOp { .. }));
        // 模零：保留表达式
        let e = BinaryOp {
            left: Box::new(Literal(Value::Int64(1))),
            op: Modulo,
            right: Box::new(Literal(Value::Int64(0))),
        };
        assert!(matches!(fold_expression(e), BinaryOp { .. }));
        // Float64 运算 → Float64
        let e = BinaryOp {
            left: Box::new(Literal(Value::Float64(1.5))),
            op: Plus,
            right: Box::new(Literal(Value::Float64(2.5))),
        };
        assert!(matches!(fold_expression(e),
            Literal(Value::Float64(v)) if (v - 4.0).abs() < 1e-9));
        // Float32 不支持数值折叠（as_f64 不含 Float32）→ 保留
        let e = BinaryOp {
            left: Box::new(Literal(Value::Float32(1.5))),
            op: Plus,
            right: Box::new(Literal(Value::Float32(2.5))),
        };
        assert!(matches!(fold_expression(e), BinaryOp { .. }));
        // 负号参与：-5 + 3 = -2
        let e = BinaryOp {
            left: Box::new(UnaryOp {
                op: UnaryOperator::Negate,
                expr: Box::new(Literal(Value::Int64(5))),
            }),
            op: Plus,
            right: Box::new(Literal(Value::Int64(3))),
        };
        assert!(matches!(fold_expression(e), Literal(Value::Int64(-2))));
    }

    #[test]
    fn test_fold_cross_type_comparison() {
        // 1 < 2.5 → true（数值跨类型比较走 as_f64）
        let e = BinaryOp {
            left: Box::new(Literal(Value::Int64(1))),
            op: Lt,
            right: Box::new(Literal(Value::Float64(2.5))),
        };
        assert!(matches!(fold_expression(e), Literal(Value::Boolean(true))));
        // Int64(1) == Float64(1.0)：Value 按变体比较 → false
        let e = BinaryOp {
            left: Box::new(Literal(Value::Int64(1))),
            op: Eq,
            right: Box::new(Literal(Value::Float64(1.0))),
        };
        assert!(matches!(fold_expression(e), Literal(Value::Boolean(false))));
        // 同变体布尔相等
        let e = BinaryOp {
            left: Box::new(Literal(Value::Boolean(false))),
            op: Eq,
            right: Box::new(Literal(Value::Boolean(false))),
        };
        assert!(matches!(fold_expression(e), Literal(Value::Boolean(true))));
        // 布尔 < 比较：无数值语义 → 保留表达式
        let e = BinaryOp {
            left: Box::new(Literal(Value::Boolean(false))),
            op: Lt,
            right: Box::new(Literal(Value::Boolean(true))),
        };
        assert!(matches!(fold_expression(e), BinaryOp { .. }));
        // Null 与任何值 Eq → false（变体不同），Null == Null → true
        let e = BinaryOp {
            left: Box::new(Literal(Value::Null)),
            op: Eq,
            right: Box::new(Literal(Value::Null)),
        };
        assert!(matches!(fold_expression(e), Literal(Value::Boolean(true))));
    }

    #[test]
    fn test_fold_logic_identity() {
        // x AND true → x
        let e = BinaryOp {
            left: Box::new(ColumnRef { table: None, column: "a".into() }),
            op: And,
            right: Box::new(Literal(Value::Boolean(true))),
        };
        assert!(matches!(fold_expression(e), ColumnRef { .. }));
        // false AND x → false
        let e = BinaryOp {
            left: Box::new(Literal(Value::Boolean(false))),
            op: And,
            right: Box::new(ColumnRef { table: None, column: "a".into() }),
        };
        assert!(matches!(fold_expression(e), Literal(Value::Boolean(false))));
        // true OR x → true
        let e = BinaryOp {
            left: Box::new(Literal(Value::Boolean(true))),
            op: Or,
            right: Box::new(ColumnRef { table: None, column: "a".into() }),
        };
        assert!(matches!(fold_expression(e), Literal(Value::Boolean(true))));
        // x OR false → x
        let e = BinaryOp {
            left: Box::new(ColumnRef { table: None, column: "a".into() }),
            op: Or,
            right: Box::new(Literal(Value::Boolean(false))),
        };
        assert!(matches!(fold_expression(e), ColumnRef { .. }));
        // 非布尔 AND（Int64 1 AND x）→ 不恒等化简，保留
        let e = BinaryOp {
            left: Box::new(Literal(Value::Int64(1))),
            op: And,
            right: Box::new(ColumnRef { table: None, column: "a".into() }),
        };
        assert!(matches!(fold_expression(e), BinaryOp { .. }));
        // 双方字面量：true AND false → false
        let e = BinaryOp {
            left: Box::new(Literal(Value::Boolean(true))),
            op: And,
            right: Box::new(Literal(Value::Boolean(false))),
        };
        assert!(matches!(fold_expression(e), Literal(Value::Boolean(false))));
        // 双重恒等化简：x AND true AND false → false
        let e = BinaryOp {
            left: Box::new(BinaryOp {
                left: Box::new(ColumnRef { table: None, column: "a".into() }),
                op: And,
                right: Box::new(Literal(Value::Boolean(true))),
            }),
            op: And,
            right: Box::new(Literal(Value::Boolean(false))),
        };
        assert!(matches!(fold_expression(e), Literal(Value::Boolean(false))));
    }

    #[test]
    fn test_fold_string_concat() {
        let e = BinaryOp {
            left: Box::new(Literal(Value::Varchar("a".into()))),
            op: Concat,
            right: Box::new(Literal(Value::Varchar("b".into()))),
        };
        assert!(matches!(fold_expression(e), Literal(Value::Varchar(s)) if s == "ab"));
        // 非字符串拼接 → 保留
        let e = BinaryOp {
            left: Box::new(Literal(Value::Int64(1))),
            op: Concat,
            right: Box::new(Literal(Value::Varchar("b".into()))),
        };
        assert!(matches!(fold_expression(e), BinaryOp { .. }));
    }

    #[test]
    fn test_fold_unary_limits() {
        // NOT 非布尔 → 保留
        let e = UnaryOp {
            op: UnaryOperator::Not,
            expr: Box::new(Literal(Value::Int64(5))),
        };
        assert!(matches!(fold_expression(e), UnaryOp { .. }));
        // Negate Float64
        let e = UnaryOp {
            op: UnaryOperator::Negate,
            expr: Box::new(Literal(Value::Float64(2.5))),
        };
        assert!(matches!(fold_expression(e),
            Literal(Value::Float64(v)) if (v + 2.5).abs() < 1e-9));
        // Negate 非数值 → 保留
        let e = UnaryOp {
            op: UnaryOperator::Negate,
            expr: Box::new(Literal(Value::Boolean(true))),
        };
        assert!(matches!(fold_expression(e), UnaryOp { .. }));
        // 双重 NOT：NOT (NOT true) → true
        let inner = UnaryOp {
            op: UnaryOperator::Not,
            expr: Box::new(Literal(Value::Boolean(true))),
        };
        let e = UnaryOp { op: UnaryOperator::Not, expr: Box::new(inner) };
        assert!(matches!(fold_expression(e), Literal(Value::Boolean(true))));
        // Negate 整数最小值边界：-(-5) = 5
        let inner = UnaryOp {
            op: UnaryOperator::Negate,
            expr: Box::new(Literal(Value::Int64(5))),
        };
        let e = UnaryOp { op: UnaryOperator::Negate, expr: Box::new(inner) };
        assert!(matches!(fold_expression(e), Literal(Value::Int64(5))));
    }

    #[test]
    fn test_fold_cast_variants() {
        use crate::common::types::DataType;
        let cast = |v: Value, t: DataType| {
            fold_expression(Cast {
                expr: Box::new(Literal(v)),
                data_type: t,
            })
        };
        // Int64 → Boolean（非零为 true）
        assert!(matches!(cast(Value::Int64(1), DataType::Boolean),
            Literal(Value::Boolean(true))));
        assert!(matches!(cast(Value::Int64(0), DataType::Boolean),
            Literal(Value::Boolean(false))));
        // Boolean → Int64：as_i64 不支持 → 保留 Cast
        assert!(matches!(cast(Value::Boolean(true), DataType::Int64), Cast { .. }));
        // Float64 → Int64 截断
        assert!(matches!(cast(Value::Float64(3.7), DataType::Int64),
            Literal(Value::Int64(3))));
        // Int64 → Varchar
        assert!(matches!(cast(Value::Int64(42), DataType::Varchar),
            Literal(Value::Varchar(s)) if s == "42"));
        // Varchar → Json
        assert!(matches!(cast(Value::Varchar("{}".into()), DataType::Json),
            Literal(Value::Json(s)) if s == "{}"));
        // Int64 → Float32
        assert!(matches!(cast(Value::Int64(5), DataType::Float32),
            Literal(Value::Float32(f)) if f == 5.0));
        // Timestamp → Varchar
        assert!(matches!(cast(Value::Timestamp(123), DataType::Varchar),
            Literal(Value::Varchar(_))));
        // Varchar → Int64 无规则 → 保留
        assert!(matches!(cast(Value::Varchar("5".into()), DataType::Int64), Cast { .. }));
        // Vector → Varchar 无规则 → 保留
        assert!(matches!(cast(Value::Vector(vec![1.0]), DataType::Varchar), Cast { .. }));
        // Null → Varchar → Null
        assert!(matches!(cast(Value::Null, DataType::Varchar), Literal(Value::Null)));
        // Null → Blob → Null
        assert!(matches!(cast(Value::Null, DataType::Blob), Literal(Value::Null)));
    }

    #[test]
    fn test_fold_case_branches() {
        // CASE WHEN false THEN 1 WHEN true THEN 2 ELSE 3 END → CASE WHEN true THEN 2 ELSE 3
        let e = Case {
            when_then: vec![
                (Literal(Value::Boolean(false)), Literal(Value::Int64(1))),
                (Literal(Value::Boolean(true)), Literal(Value::Int64(2))),
            ],
            else_expr: Some(Box::new(Literal(Value::Int64(3)))),
        };
        match fold_expression(e) {
            Case { when_then, else_expr } => {
                assert_eq!(when_then.len(), 1);
                assert!(matches!(&when_then[0].0, Literal(Value::Boolean(true))));
                assert!(matches!(&when_then[0].1, Literal(Value::Int64(2))));
                assert!(else_expr.is_some());
            }
            other => panic!("expected Case with skipped branch, got {other:?}"),
        }
        // 非常量 WHEN 保留结构
        let e = Case {
            when_then: vec![(
                ColumnRef { table: None, column: "a".into() },
                Literal(Value::Int64(1)),
            )],
            else_expr: Some(Box::new(Literal(Value::Int64(2)))),
        };
        assert!(matches!(fold_expression(e), Case { .. }));
        // 多分支全部折叠：当条件非常量但结果常量 → 分支内部折叠
        let e = Case {
            when_then: vec![(
                BinaryOp {
                    left: Box::new(Literal(Value::Int64(1))),
                    op: Plus,
                    right: Box::new(Literal(Value::Int64(1))),
                },
                BinaryOp {
                    left: Box::new(Literal(Value::Int64(2))),
                    op: Multiply,
                    right: Box::new(Literal(Value::Int64(3))),
                },
            )],
            else_expr: None,
        };
        match fold_expression(e) {
            Case { when_then, .. } => {
                assert!(matches!(&when_then[0].0, Literal(Value::Int64(2))));
                assert!(matches!(&when_then[0].1, Literal(Value::Int64(6))));
            }
            other => panic!("expected folded Case, got {other:?}"),
        }
    }

    // ============ 谓词下推深路径 ============

    fn scan(t: &str, cols: &[usize]) -> PhysicalPlan {
        PhysicalPlan::TableScan {
            table_name: t.to_string(),
            column_indices: cols.to_vec(),
        }
    }

    fn col_ref(c: &str) -> Expression {
        ColumnRef { table: None, column: c.to_string() }
    }

    fn lit_i(v: i64) -> Expression {
        Literal(Value::Int64(v))
    }

    fn cmp_expr(op: BinaryOperator, l: Expression, r: Expression) -> Expression {
        BinaryOp { left: Box::new(l), op, right: Box::new(r) }
    }

    #[test]
    fn test_pushdown_through_projection() {
        // Filter(a>5 AND x>10) over Projection(x = a+1, b)
        // → a>5 可下推（a 是纯列引用）；x>10 引用计算列 → 保留在投影之上
        let cond = BinaryOp {
            left: Box::new(cmp_expr(Gt, col_ref("a"), lit_i(5))),
            op: And,
            right: Box::new(cmp_expr(Gt, col_ref("x"), lit_i(10))),
        };
        let plan = PhysicalPlan::Filter {
            input: Box::new(PhysicalPlan::Projection {
                input: Box::new(scan("t", &[0, 1])),
                // 输出 a（纯列引用，可下推谓词）+ x（计算列，谓词不可下推）
                expressions: vec![col_ref("a"), cmp_expr(Plus, col_ref("a"), lit_i(1))],
                column_names: vec!["a".into(), "x".into()],
            }),
            condition: cond,
        };
        let result = predicate_pushdown(plan).unwrap();
        match &result {
            // 顶层 Filter 保留不可下推谓词 x>10
            PhysicalPlan::Filter { condition, input } => {
                assert!(matches!(input.as_ref(), PhysicalPlan::Projection { .. }));
                let top_preds: Vec<Expression> = {
                    let mut v = Vec::new();
                    split_and_conditions(condition, &mut v);
                    v
                };
                assert_eq!(top_preds.len(), 1, "expected x>10 kept above projection");
                // 投影之下已插入 a>5 的 Filter
                match input.as_ref() {
                    PhysicalPlan::Projection { input, .. } => {
                        assert!(matches!(input.as_ref(), PhysicalPlan::Filter { .. }));
                    }
                    other => panic!("expected Projection, got {other:?}"),
                }
            }
            other => panic!("expected Filter on top, got {other:?}"),
        }
    }

    #[test]
    fn test_pushdown_projection_non_pushable_kept() {
        // Filter(b>1) over Projection(a)（b 不在投影输出中）→ 不可下推，保留
        let plan = PhysicalPlan::Filter {
            input: Box::new(PhysicalPlan::Projection {
                input: Box::new(scan("t", &[0])),
                expressions: vec![col_ref("a")],
                column_names: vec!["a".into()],
            }),
            condition: cmp_expr(Gt, col_ref("b"), lit_i(1)),
        };
        let result = predicate_pushdown(plan).unwrap();
        match &result {
            PhysicalPlan::Filter { input, .. } => {
                assert!(matches!(input.as_ref(), PhysicalPlan::Projection { .. }));
            }
            other => panic!("expected Filter kept, got {other:?}"),
        }
        // 投影之下不应再有 Filter（无谓词可下推）
        if let PhysicalPlan::Filter { input, .. } = &result {
            if let PhysicalPlan::Projection { input, .. } = input.as_ref() {
                assert!(!matches!(input.as_ref(), PhysicalPlan::Filter { .. }),
                    "b>1 must not be pushed below projection");
            }
        }
    }

    #[test]
    fn test_pushdown_multiple_filters_merge() {
        // Filter(x)(Filter(y)(Scan)) → 单 Filter(x AND y) 于扫描上
        let plan = PhysicalPlan::Filter {
            input: Box::new(PhysicalPlan::Filter {
                input: Box::new(scan("t", &[0, 1])),
                condition: cmp_expr(Gt, col_ref("a"), lit_i(5)),
            }),
            condition: cmp_expr(Lt, col_ref("b"), lit_i(10)),
        };
        let result = predicate_pushdown(plan).unwrap();
        match &result {
            PhysicalPlan::Filter { input, condition } => {
                assert!(matches!(input.as_ref(), PhysicalPlan::TableScan { .. }));
                let mut preds = Vec::new();
                split_and_conditions(condition, &mut preds);
                assert_eq!(preds.len(), 2);
            }
            other => panic!("expected merged Filter, got {other:?}"),
        }
    }

    #[test]
    fn test_pushdown_kept_above_aggregate() {
        // Filter(SUM(a) > 10) over Aggregate → Filter 保留在聚合上
        let plan = PhysicalPlan::Filter {
            input: Box::new(PhysicalPlan::Aggregate {
                input: Box::new(scan("t", &[0])),
                group_by: vec![],
                aggregates: vec![],
            }),
            condition: cmp_expr(Gt, col_ref("sum_a"), lit_i(10)),
        };
        let result = predicate_pushdown(plan).unwrap();
        match &result {
            PhysicalPlan::Filter { input, .. } => {
                assert!(matches!(input.as_ref(), PhysicalPlan::Aggregate { .. }));
            }
            other => panic!("expected Filter above Aggregate, got {other:?}"),
        }
    }

    #[test]
    fn test_pushdown_kept_above_hashjoin() {
        // Filter over HashJoin：谓词不得穿过 JOIN（外连接语义）
        let join = PhysicalPlan::HashJoin {
            left: Box::new(scan("t", &[0])),
            right: Box::new(scan("u", &[0])),
            join_type: crate::executor::physical_plan::JoinType::Inner,
            left_keys: vec![0],
            right_keys: vec![0],
        };
        let plan = PhysicalPlan::Filter {
            input: Box::new(join),
            condition: cmp_expr(Gt, col_ref("t.a"), lit_i(5)),
        };
        let result = predicate_pushdown(plan).unwrap();
        match &result {
            PhysicalPlan::Filter { input, condition } => {
                assert!(matches!(input.as_ref(), PhysicalPlan::HashJoin { .. }));
                assert!(format!("{condition:?}").contains("t.a"));
            }
            other => panic!("expected Filter above HashJoin, got {other:?}"),
        }
    }

    #[test]
    fn test_pushdown_kept_above_crossjoin() {
        let join = PhysicalPlan::CrossJoin {
            left: Box::new(scan("t", &[0])),
            right: Box::new(scan("u", &[0])),
        };
        let plan = PhysicalPlan::Filter {
            input: Box::new(join),
            condition: cmp_expr(Gt, col_ref("a"), lit_i(5)),
        };
        let result = predicate_pushdown(plan).unwrap();
        match &result {
            PhysicalPlan::Filter { input, .. } => {
                assert!(matches!(input.as_ref(), PhysicalPlan::CrossJoin { .. }));
            }
            other => panic!("expected Filter above CrossJoin, got {other:?}"),
        }
    }

    #[test]
    fn test_pushdown_through_limit_deep_chain() {
        // Filter(Limit(Filter(Projection(Filter(Scan))))) → 全部谓词汇聚到扫描
        let plan = PhysicalPlan::Filter {
            input: Box::new(PhysicalPlan::Limit {
                input: Box::new(PhysicalPlan::Filter {
                    input: Box::new(PhysicalPlan::Projection {
                        input: Box::new(PhysicalPlan::Filter {
                            input: Box::new(scan("t", &[0, 1])),
                            condition: cmp_expr(Gt, col_ref("a"), lit_i(1)),
                        }),
                        expressions: vec![col_ref("a"), col_ref("b")],
                        column_names: vec!["a".into(), "b".into()],
                    }),
                    condition: cmp_expr(Lt, col_ref("b"), lit_i(100)),
                }),
                limit: 10,
            }),
            condition: cmp_expr(GtEq, col_ref("a"), lit_i(50)),
        };
        let result = predicate_pushdown(plan).unwrap();
        // 顶层为 Limit，所有谓词（a>=50、b<100、a>1）都下沉到扫描附近
        assert!(matches!(&result, PhysicalPlan::Limit { .. }));
        let tree = format!("{result:?}");
        assert!(tree.contains("TableScan"), "scan preserved: {tree}");
        assert!(tree.contains("GtEq"), "a>=50 preserved: {tree}");
        assert!(tree.contains("Lt"), "b<100 preserved: {tree}");
        assert!(tree.contains("Gt"), "a>1 preserved: {tree}");
        // Limit 之下仍有 Filter（谓词未丢失）
        if let PhysicalPlan::Limit { input, .. } = &result {
            assert!(format!("{input:?}").contains("Filter"),
                "predicates pushed below limit: {input:?}");
        }
    }

    // ============ 投影下推 ============

    #[test]
    fn test_projection_pushdown_structure_preserved() {
        // 深链结构保留 + 递归无错
        let plan = PhysicalPlan::Limit {
            input: Box::new(PhysicalPlan::Filter {
                input: Box::new(PhysicalPlan::Projection {
                    input: Box::new(scan("t", &[0, 1])),
                    expressions: vec![cmp_expr(Plus, col_ref("a"), lit_i(1)), col_ref("b")],
                    column_names: vec!["x".into(), "b".into()],
                }),
                condition: cmp_expr(Gt, col_ref("x"), lit_i(0)),
            }),
            limit: 5,
        };
        let result = projection_pushdown(plan).unwrap();
        match &result {
            PhysicalPlan::Limit { input, limit: 5 } => match input.as_ref() {
                PhysicalPlan::Filter { input, .. } => {
                    assert!(matches!(input.as_ref(), PhysicalPlan::Projection { .. }));
                }
                other => panic!("expected Filter, got {other:?}"),
            },
            other => panic!("expected Limit, got {other:?}"),
        }
    }

    #[test]
    fn test_projection_pushdown_filter_adds_condition_cols() {
        // Filter 条件列被加入 required 集合（传递到输入）
        let plan = PhysicalPlan::Filter {
            input: Box::new(scan("t", &[0, 1])),
            condition: cmp_expr(Gt, col_ref("hidden"), lit_i(1)),
        };
        let result = projection_pushdown(plan).unwrap();
        match &result {
            PhysicalPlan::Filter { condition, input } => {
                assert!(matches!(input.as_ref(), PhysicalPlan::TableScan { .. }));
                assert!(format!("{condition:?}").contains("hidden"));
            }
            other => panic!("expected Filter, got {other:?}"),
        }
    }

    // ============ 恒等投影消除 ============

    #[test]
    fn test_identity_projection_elimination_removes() {
        // Projection(全部 ColumnRef, 与扫描列一致) → 消除
        let plan = PhysicalPlan::Projection {
            input: Box::new(scan("t", &[0, 1])),
            expressions: vec![col_ref("a"), col_ref("b")],
            column_names: vec!["a".into(), "b".into()],
        };
        let result = identity_projection_elimination(plan);
        assert!(matches!(result, PhysicalPlan::TableScan { .. }));
    }

    #[test]
    fn test_identity_projection_elimination_keeps() {
        // 表达式含计算 → 保留
        let plan = PhysicalPlan::Projection {
            input: Box::new(scan("t", &[0, 1])),
            expressions: vec![cmp_expr(Plus, col_ref("a"), lit_i(1)), col_ref("b")],
            column_names: vec!["a".into(), "b".into()],
        };
        assert!(matches!(identity_projection_elimination(plan),
            PhysicalPlan::Projection { .. }));
        // 数量不匹配 → 保留
        let plan = PhysicalPlan::Projection {
            input: Box::new(scan("t", &[0, 1])),
            expressions: vec![col_ref("a")],
            column_names: vec!["a".into()],
        };
        assert!(matches!(identity_projection_elimination(plan),
            PhysicalPlan::Projection { .. }));
        // 列名不匹配（a 输出为 x）→ 保留
        let plan = PhysicalPlan::Projection {
            input: Box::new(scan("t", &[0, 1])),
            expressions: vec![col_ref("a"), col_ref("b")],
            column_names: vec!["x".into(), "b".into()],
        };
        assert!(matches!(identity_projection_elimination(plan),
            PhysicalPlan::Projection { .. }));
        // 非 TableScan 输入 → 保留
        let plan = PhysicalPlan::Projection {
            input: Box::new(PhysicalPlan::Filter {
                input: Box::new(scan("t", &[0, 1])),
                condition: cmp_expr(Gt, col_ref("a"), lit_i(1)),
            }),
            expressions: vec![col_ref("a"), col_ref("b")],
            column_names: vec!["a".into(), "b".into()],
        };
        assert!(matches!(identity_projection_elimination(plan),
            PhysicalPlan::Projection { .. }));
        // 嵌套：恒等投影下还有恒等投影 → 递归消除
        let plan = PhysicalPlan::Projection {
            input: Box::new(PhysicalPlan::Projection {
                input: Box::new(scan("t", &[0])),
                expressions: vec![col_ref("a")],
                column_names: vec!["a".into()],
            }),
            expressions: vec![col_ref("a")],
            column_names: vec!["a".into()],
        };
        assert!(matches!(identity_projection_elimination(plan),
            PhysicalPlan::TableScan { .. }));
    }

    // ============ build side 交换（CBO） ============

    #[test]
    fn test_build_side_swap_inner_join() {
        // 左 Filter（约 3000 行）右 Scan（10000 行）→ 交换：小表为 build side
        let plan = PhysicalPlan::HashJoin {
            left: Box::new(PhysicalPlan::Filter {
                input: Box::new(scan("small", &[0])),
                condition: cmp_expr(Gt, col_ref("a"), lit_i(5)),
            }),
            right: Box::new(scan("big", &[0])),
            join_type: crate::executor::physical_plan::JoinType::Inner,
            left_keys: vec![0],
            right_keys: vec![1],
        };
        let result = optimize_build_sides(plan).unwrap();
        match &result {
            PhysicalPlan::HashJoin { left, right, join_type, left_keys, right_keys } => {
                // 交换后：left = 原 big（TableScan），right = 原 small（Filter）
                assert!(matches!(left.as_ref(), PhysicalPlan::TableScan { .. }));
                assert!(matches!(right.as_ref(), PhysicalPlan::Filter { .. }));
                assert_eq!(*join_type, crate::executor::physical_plan::JoinType::Inner);
                // 键映射随侧交换
                assert_eq!(*left_keys, vec![1]);
                assert_eq!(*right_keys, vec![0]);
            }
            other => panic!("expected swapped HashJoin, got {other:?}"),
        }
    }

    #[test]
    fn test_build_side_no_swap() {
        // 左大右小 → 不交换
        let plan = PhysicalPlan::HashJoin {
            left: Box::new(scan("big", &[0])),
            right: Box::new(PhysicalPlan::Filter {
                input: Box::new(scan("small", &[0])),
                condition: cmp_expr(Gt, col_ref("a"), lit_i(5)),
            }),
            join_type: crate::executor::physical_plan::JoinType::Inner,
            left_keys: vec![0],
            right_keys: vec![1],
        };
        let result = optimize_build_sides(plan).unwrap();
        match &result {
            PhysicalPlan::HashJoin { left, right, left_keys, right_keys, .. } => {
                assert!(matches!(left.as_ref(), PhysicalPlan::TableScan { .. }));
                assert!(matches!(right.as_ref(), PhysicalPlan::Filter { .. }));
                assert_eq!(*left_keys, vec![0]);
                assert_eq!(*right_keys, vec![1]);
            }
            other => panic!("expected unswapped HashJoin, got {other:?}"),
        }
        // Left Join 即使右大也不交换（语义保留）
        let plan = PhysicalPlan::HashJoin {
            left: Box::new(PhysicalPlan::Filter {
                input: Box::new(scan("small", &[0])),
                condition: cmp_expr(Gt, col_ref("a"), lit_i(5)),
            }),
            right: Box::new(scan("big", &[0])),
            join_type: crate::executor::physical_plan::JoinType::Left,
            left_keys: vec![0],
            right_keys: vec![1],
        };
        let result = optimize_build_sides(plan).unwrap();
        match &result {
            PhysicalPlan::HashJoin { left, right, join_type, .. } => {
                assert!(matches!(left.as_ref(), PhysicalPlan::Filter { .. }));
                assert!(matches!(right.as_ref(), PhysicalPlan::TableScan { .. }));
                assert_eq!(*join_type, crate::executor::physical_plan::JoinType::Left);
            }
            other => panic!("expected Left join unswapped, got {other:?}"),
        }
    }

    #[test]
    fn test_build_side_swap_nested() {
        // 三表：外层 join 右大交换，内层 join 左大右小不交换
        let inner = PhysicalPlan::HashJoin {
            left: Box::new(scan("a", &[0])),
            right: Box::new(PhysicalPlan::Filter {
                input: Box::new(scan("b", &[0])),
                condition: cmp_expr(Gt, col_ref("x"), lit_i(1)),
            }),
            join_type: crate::executor::physical_plan::JoinType::Inner,
            left_keys: vec![0],
            right_keys: vec![0],
        };
        let outer = PhysicalPlan::HashJoin {
            left: Box::new(inner),
            right: Box::new(scan("c", &[0])),
            join_type: crate::executor::physical_plan::JoinType::Inner,
            left_keys: vec![0],
            right_keys: vec![0],
        };
        let result = optimize_build_sides(outer).unwrap();
        match &result {
            PhysicalPlan::HashJoin { left, right, left_keys, right_keys, .. } => {
                // 外层：左（inner join 估算 3000*10000*0.1=3M）右 10000 → 不交换？！
                // inner 估算：10000*3000*0.1=3,000,000 > 10000 → right 小 → 不交换
                assert!(matches!(right.as_ref(), PhysicalPlan::TableScan { .. }));
                assert_eq!(*left_keys, vec![0]);
                assert_eq!(*right_keys, vec![0]);
                // 内层：a(10000) vs b-filter(3000) → 不交换
                match left.as_ref() {
                    PhysicalPlan::HashJoin { left, right, .. } => {
                        assert!(matches!(left.as_ref(), PhysicalPlan::TableScan { .. }));
                        assert!(matches!(right.as_ref(), PhysicalPlan::Filter { .. }));
                    }
                    other => panic!("expected inner HashJoin, got {other:?}"),
                }
            }
            other => panic!("expected nested HashJoin, got {other:?}"),
        }
    }

    #[test]
    fn test_estimate_rows_variants() {
        assert_eq!(estimate_rows(&scan("t", &[0])), 10_000);
        let filter = PhysicalPlan::Filter {
            input: Box::new(scan("t", &[0])),
            condition: cmp_expr(Gt, col_ref("a"), lit_i(1)),
        };
        assert_eq!(estimate_rows(&filter), 3_000);
        // 聚合无 group_by → 1
        let agg = PhysicalPlan::Aggregate {
            input: Box::new(scan("t", &[0])),
            group_by: vec![],
            aggregates: vec![],
        };
        assert_eq!(estimate_rows(&agg), 1);
        // 有 group_by → max(1000, 1)
        let agg2 = PhysicalPlan::Aggregate {
            input: Box::new(scan("t", &[0])),
            group_by: vec![0],
            aggregates: vec![],
        };
        assert_eq!(estimate_rows(&agg2), 1_000);
        // Limit 取 min
        let lim = PhysicalPlan::Limit {
            input: Box::new(scan("t", &[0])),
            limit: 7,
        };
        assert_eq!(estimate_rows(&lim), 7);
        // HashJoin 笛卡尔积 × 0.1
        let join = PhysicalPlan::HashJoin {
            left: Box::new(scan("t", &[0])),
            right: Box::new(scan("u", &[0])),
            join_type: crate::executor::physical_plan::JoinType::Inner,
            left_keys: vec![0],
            right_keys: vec![0],
        };
        assert_eq!(estimate_rows(&join), 10_000_000);
        // 未知节点 → 1000
        let p = PhysicalPlan::CountStar { output_name: "c".into(), count: 3 };
        assert_eq!(estimate_rows(&p), 1000);
        // 恒等投影行数不变
        let proj = PhysicalPlan::Projection {
            input: Box::new(scan("t", &[0])),
            expressions: vec![col_ref("a")],
            column_names: vec!["a".into()],
        };
        assert_eq!(estimate_rows(&proj), 10_000);
    }

    // ============ 过滤条件重排深路径 ============

    #[test]
    fn test_predicate_selectivity_order() {
        // 等值 < IS NULL < 范围 < IN < 不等 < LIKE；常量最前；NOT +10
        let pred = |e: Expression| predicate_selectivity_score(&e);
        assert_eq!(pred(Literal(Value::Boolean(true))), 0);
        assert_eq!(pred(cmp_expr(Eq, col_ref("a"), lit_i(1))), 1);
        assert_eq!(pred(IsNull(Box::new(col_ref("a")))), 2);
        assert_eq!(pred(cmp_expr(Gt, col_ref("a"), lit_i(1))), 3);
        assert_eq!(pred(cmp_expr(LtEq, col_ref("a"), lit_i(1))), 3);
        assert_eq!(pred(InList {
            expr: Box::new(col_ref("a")),
            list: vec![lit_i(1), lit_i(2)],
        }), 4);
        assert_eq!(pred(cmp_expr(NotEq, col_ref("a"), lit_i(1))), 5);
        assert_eq!(pred(Like {
            expr: Box::new(col_ref("a")),
            pattern: Box::new(lit_i(1)),
        }), 6);
        assert_eq!(pred(col_ref("a")), 7);
        // NOT 包裹 = 内部 + 10
        let not = UnaryOp {
            op: UnaryOperator::Not,
            expr: Box::new(cmp_expr(Eq, col_ref("a"), lit_i(1))),
        };
        assert_eq!(pred(not), 11);
    }

    #[test]
    fn test_filter_reorder_full_order() {
        // 常量 + LIKE + Eq → 常量最前，Eq 在 LIKE 前
        let cond = BinaryOp {
            left: Box::new(BinaryOp {
                left: Box::new(Literal(Value::Boolean(true))),
                op: And,
                right: Box::new(Like {
                    expr: Box::new(col_ref("c")),
                    pattern: Box::new(Literal(Value::Varchar("%x%".into()))),
                }),
            }),
            op: And,
            right: Box::new(cmp_expr(Eq, col_ref("b"), lit_i(10))),
        };
        let plan = PhysicalPlan::Filter {
            input: Box::new(scan("t", &[0, 1, 2])),
            condition: cond,
        };
        let result = filter_reorder(plan).unwrap();
        match &result {
            PhysicalPlan::Filter { condition, .. } => {
                let mut preds = Vec::new();
                split_and_conditions(condition, &mut preds);
                assert_eq!(preds.len(), 3);
                assert!(matches!(&preds[0], Literal(_)), "constant first");
                assert!(matches!(&preds[1], BinaryOp { op: Eq, .. }), "Eq second");
                assert!(matches!(&preds[2], Like { .. }), "Like last");
            }
            other => panic!("expected Filter, got {other:?}"),
        }
    }

    #[test]
    fn test_filter_reorder_single_predicate_unchanged() {
        let plan = PhysicalPlan::Filter {
            input: Box::new(scan("t", &[0])),
            condition: cmp_expr(Gt, col_ref("a"), lit_i(1)),
        };
        let before = format!("{plan:?}");
        let result = filter_reorder(plan).unwrap();
        assert_eq!(format!("{result:?}"), before);
    }

    // ============ 辅助函数 ============

    #[test]
    fn test_split_and_conditions_nested() {
        let cond = BinaryOp {
            left: Box::new(BinaryOp {
                left: Box::new(col_ref("a")),
                op: And,
                right: Box::new(BinaryOp {
                    left: Box::new(col_ref("b")),
                    op: And,
                    right: Box::new(col_ref("c")),
                }),
            }),
            op: And,
            right: Box::new(col_ref("d")),
        };
        let mut preds = Vec::new();
        split_and_conditions(&cond, &mut preds);
        assert_eq!(preds.len(), 4);
        // OR 不被拆分
        let or_cond = BinaryOp {
            left: Box::new(col_ref("a")),
            op: Or,
            right: Box::new(col_ref("b")),
        };
        let mut preds = Vec::new();
        split_and_conditions(&or_cond, &mut preds);
        assert_eq!(preds.len(), 1);
    }

    #[test]
    fn test_split_pushable_predicates() {
        let preds = vec![
            cmp_expr(Gt, col_ref("a"), lit_i(1)),
            cmp_expr(Gt, cmp_expr(Plus, col_ref("a"), col_ref("b")), lit_i(2)),
            cmp_expr(Eq, col_ref("c"), lit_i(3)),
        ];
        // 投影只有 a（纯列引用）
        let projections = vec![col_ref("a")];
        let (pushable, non_pushable) = split_pushable_predicates(preds, &projections);
        assert_eq!(pushable.len(), 1); // a>1
        assert_eq!(non_pushable.len(), 2); // a+b>2、c=3
        // 空投影：全部不可下推
        let (pushable, non_pushable) = split_pushable_predicates(
            vec![cmp_expr(Gt, col_ref("a"), lit_i(1))], &[]);
        assert!(pushable.is_empty());
        assert_eq!(non_pushable.len(), 1);
    }

    #[test]
    fn test_collect_joins_and_tables() {
        let plan = PhysicalPlan::Filter {
            input: Box::new(PhysicalPlan::HashJoin {
                left: Box::new(scan("t", &[0])),
                right: Box::new(PhysicalPlan::Projection {
                    input: Box::new(scan("u", &[0])),
                    expressions: vec![col_ref("a")],
                    column_names: vec!["a".into()],
                }),
                join_type: crate::executor::physical_plan::JoinType::Inner,
                left_keys: vec![0],
                right_keys: vec![0],
            }),
            condition: cmp_expr(Gt, col_ref("a"), lit_i(1)),
        };
        let mut joins = Vec::new();
        let mut tables = Vec::new();
        collect_joins_and_tables(&plan, &mut joins, &mut tables);
        assert_eq!(joins.len(), 1);
        assert_eq!(tables, vec!["t".to_string(), "u".to_string()]);
    }

    #[test]
    fn test_plan_eq_compare() {
        let p1 = scan("t", &[0]);
        let p2 = scan("t", &[0]);
        let p3 = scan("t", &[1]);
        assert!(plan_eq(&p1, &p2));
        assert!(!plan_eq(&p1, &p3));
    }

    #[test]
    fn test_combine_predicates() {
        let combined = combine_predicates(vec![col_ref("a"), col_ref("b"), col_ref("c")]);
        let mut preds = Vec::new();
        split_and_conditions(&combined, &mut preds);
        assert_eq!(preds.len(), 3);
    }

    // ============ 全链路 ============

    #[test]
    fn test_optimize_build_side_swap_end_to_end() {
        // optimize() 全链路触发 build side 交换
        let plan = PhysicalPlan::HashJoin {
            left: Box::new(PhysicalPlan::Filter {
                input: Box::new(scan("small", &[0])),
                condition: cmp_expr(Gt, col_ref("a"), lit_i(5)),
            }),
            right: Box::new(scan("big", &[0])),
            join_type: crate::executor::physical_plan::JoinType::Inner,
            left_keys: vec![0],
            right_keys: vec![1],
        };
        let result = optimize(plan).unwrap();
        match &result {
            PhysicalPlan::HashJoin { left, right, left_keys, right_keys, .. } => {
                assert!(matches!(left.as_ref(), PhysicalPlan::TableScan { .. }),
                    "build side swap through optimize()");
                assert!(matches!(right.as_ref(), PhysicalPlan::Filter { .. }));
                assert_eq!(*left_keys, vec![1]);
                assert_eq!(*right_keys, vec![0]);
            }
            other => panic!("expected HashJoin, got {other:?}"),
        }
    }

    #[test]
    fn test_optimize_full_pipeline() {
        // 复杂计划：Filter(Join(Filter(Scan), Scan)) + Limit + Projection
        // 全链路优化：谓词下推 + 恒等投影消除 + build side 交换 + 常量折叠
        let plan = PhysicalPlan::Projection {
            input: Box::new(PhysicalPlan::Limit {
                input: Box::new(PhysicalPlan::Filter {
                    input: Box::new(PhysicalPlan::HashJoin {
                        left: Box::new(PhysicalPlan::Filter {
                            input: Box::new(scan("small", &[0, 1])),
                            condition: BinaryOp {
                                left: Box::new(cmp_expr(Gt, col_ref("a"), lit_i(1))),
                                op: And,
                                right: Box::new(cmp_expr(Eq, col_ref("b"), lit_i(2))),
                            },
                        }),
                        right: Box::new(scan("big", &[0, 1])),
                        join_type: crate::executor::physical_plan::JoinType::Inner,
                        left_keys: vec![0],
                        right_keys: vec![0],
                    }),
                    condition: BinaryOp {
                        left: Box::new(cmp_expr(Lt, col_ref("small.a"), lit_i(100))),
                        op: And,
                        right: Box::new(cmp_expr(Plus, lit_i(1), lit_i(1)), )
                    },
                }),
                limit: 50,
            }),
            expressions: vec![col_ref("a")],
            column_names: vec!["a".into()],
        };
        let result = optimize(plan).unwrap();
        // 常量 1+1 → 2 折叠进条件
        let tree = format!("{result:?}");
        assert!(!tree.contains("Plus"), "constant folding in pipeline: {tree}");
        assert!(tree.contains("HashJoin"));
    }
}
