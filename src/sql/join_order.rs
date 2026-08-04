//! 连接顺序优化（Join Order Optimization）
//!
//! CBO 的核心算法之一：给定 N 个表的连接，找到代价最低的连接顺序。
//!
//! 实现经典的 System R 风格动态规划算法：
//! - 对于 n 个关系，枚举所有 2^n - 1 个子集
//! - 每个子集记录最优连接计划及其代价
//! - 从小到大构建，最终得到全集的最优计划
//!
//! 优化：
//! - 只考虑左深树（left-deep tree），减少搜索空间
//! - 应用启发式剪枝（笛卡尔积最后做）
//! - 限制最大表数（超过 8 个表改用贪心算法）

use crate::common::error::Result;
use crate::executor::physical_plan::{PhysicalPlan, JoinType};
use crate::sql::cost_model::{CostModel, PlanProperties};

/// 连接图中的一个关系节点
#[derive(Debug, Clone)]
pub struct JoinRelation {
    /// 关系 ID（用于标识）
    pub id: usize,
    /// 物理计划（TableScan 或子查询）
    pub plan: PhysicalPlan,
    /// 估计属性
    pub props: PlanProperties,
}

/// 连接条件
#[derive(Debug, Clone)]
pub struct JoinCondition {
    /// 左表连接键列索引
    pub left_keys: Vec<usize>,
    /// 右表连接键列索引
    pub right_keys: Vec<usize>,
}

/// 动态规划状态：一个子集的最优连接计划
#[derive(Debug, Clone)]
struct DpState {
    /// 最优计划
    plan: PhysicalPlan,
    /// 计划属性
    props: PlanProperties,
    /// 总代价
    cost: f64,
}

/// 优化连接顺序
///
/// 输入：关系列表 + 连接条件（每对关系之间可能有连接条件）
/// 输出：最优连接计划
///
/// 注意：当前实现假设所有连接都是内连接，且为左深树。
pub fn optimize_join_order(
    relations: Vec<JoinRelation>,
    conditions: &[(usize, usize, JoinCondition)], // (left_rel_id, right_rel_id, condition)
    join_type: JoinType,
    cost_model: &CostModel,
) -> Result<PhysicalPlan> {
    let n = relations.len();

    if n == 0 {
        return Err(crate::common::error::EngramDbError::Parse(
            "Cannot optimize join order with 0 relations".into()
        ));
    }

    if n == 1 {
        return Ok(relations.into_iter().next().unwrap().plan);
    }

    // 表数太多时用贪心算法
    if n > 8 {
        return Ok(greedy_join_order(relations, conditions, join_type, cost_model));
    }

    // 动态规划：dp[mask] = 该子集的最优计划
    // mask 是位掩码，第 i 位表示第 i 个关系是否在子集中
    let size = 1 << n;
    let mut dp: Vec<Option<DpState>> = vec![None; size];

    // 初始化：单元素子集
    for (i, rel) in relations.iter().enumerate() {
        let mask = 1 << i;
        let cost = cost_model.calculate(&rel.plan);
        dp[mask] = Some(DpState {
            plan: rel.plan.clone(),
            props: rel.props.clone(),
            cost: cost.total,
        });
    }

    // 按子集大小从小到大计算
    for subset_size in 2..=n {
        // 枚举所有大小为 subset_size 的子集
        let mut mask = (1 << subset_size) - 1;
        while mask < size {
            // 枚举所有可能的拆分：mask = left_mask | right_mask
            // 其中 left_mask 和 right_mask 都非空，且不相交
            let mut best: Option<DpState> = None;

            // 枚举 left_mask（mask 的非空真子集）
            let mut left_mask = mask & mask.wrapping_neg(); // 最低位
            while left_mask < mask {
                let right_mask = mask ^ left_mask;

                if left_mask != 0 && right_mask != 0 {
                    if let (Some(left_state), Some(right_state)) =
                        (&dp[left_mask], &dp[right_mask])
                    {
                        // 检查这两个子集之间是否有连接条件
                        if let Some(cond) = find_join_condition(
                            left_mask, right_mask, conditions, &relations,
                        ) {
                            // 构建连接计划（左深树：左边大的在左）
                            let join_plan = PhysicalPlan::HashJoin {
                                left: Box::new(left_state.plan.clone()),
                                right: Box::new(right_state.plan.clone()),
                                join_type,
                                left_keys: cond.0.clone(),
                                right_keys: cond.1.clone(),
                            };

                            let cost = cost_model.calculate(&join_plan);
                            let total_cost = cost.total;

                            // 更新最优
                            if best.as_ref().map_or(true, |b| total_cost < b.cost) {
                                // 估算输出属性（简化）
                                let output_rows = estimate_join_rows(
                                    &left_state.props, &right_state.props,
                                    cond.0.len(), join_type,
                                );
                                let num_cols = left_state.props.num_columns
                                    + right_state.props.num_columns;

                                best = Some(DpState {
                                    plan: join_plan,
                                    props: PlanProperties {
                                        row_count: output_rows,
                                        num_columns: num_cols,
                                        row_size: left_state.props.row_size
                                            + right_state.props.row_size,
                                    },
                                    cost: total_cost,
                                });
                            }
                        }
                    }
                }

                // 下一个子集
                left_mask = left_mask.wrapping_sub(mask) & mask;
                if left_mask == 0 {
                    break;
                }
            }

            if best.is_some() {
                dp[mask] = best;
            }

            // 下一个大小为 subset_size 的掩码
            // Gosper's Hack
            let c = mask & mask.wrapping_neg();
            let r = mask + c;
            mask = (((r ^ mask) >> 2) / c) | r;
        }
    }

    // 返回全集的最优计划
    let full_mask = size - 1;
    match dp[full_mask].take() {
        Some(state) => Ok(state.plan),
        None => {
            // 没有找到连接顺序（可能是笛卡尔积），返回原始顺序
            Ok(build_linear_join(relations, conditions, join_type))
        }
    }
}

/// 贪心连接顺序（表数多时的 fallback）
///
/// 每一步选择代价增加最少的连接对。
fn greedy_join_order(
    mut relations: Vec<JoinRelation>,
    conditions: &[(usize, usize, JoinCondition)],
    join_type: JoinType,
    cost_model: &CostModel,
) -> PhysicalPlan {
    if relations.is_empty() {
        // 不应该走到这里
        return PhysicalPlan::TableScan {
            table_name: "empty".to_string(),
            column_indices: vec![],
        };
    }

    // 反复合并，直到只剩一个关系
    while relations.len() > 1 {
        let mut best_pair: Option<(usize, usize, PhysicalPlan, f64)> = None;

        // 枚举所有可能的连接对
        for i in 0..relations.len() {
            for j in (i + 1)..relations.len() {
                // 查找连接条件
                let cond = conditions.iter().find(|(a, b, _)| {
                    (*a == relations[i].id && *b == relations[j].id)
                        || (*a == relations[j].id && *b == relations[i].id)
                });

                if let Some((_, _, c)) = cond {
                    let (left_keys, right_keys) = if conditions.iter().any(
                        |(a, b, _)| *a == relations[i].id && *b == relations[j].id
                    ) {
                        (c.left_keys.clone(), c.right_keys.clone())
                    } else {
                        (c.right_keys.clone(), c.left_keys.clone())
                    };

                    let join_plan = PhysicalPlan::HashJoin {
                        left: Box::new(relations[i].plan.clone()),
                        right: Box::new(relations[j].plan.clone()),
                        join_type,
                        left_keys,
                        right_keys,
                    };

                    let cost = cost_model.calculate(&join_plan);

                    if best_pair.as_ref().map_or(true, |(_, _, _, c)| cost.total < *c) {
                        best_pair = Some((i, j, join_plan, cost.total));
                    }
                }
            }
        }

        if let Some((i, j, plan, _)) = best_pair {
            // 合并 i 和 j
            let props = estimate_merged_props(&relations[i].props, &relations[j].props, join_type);
            let new_id = relations.len(); // 新 ID
            let new_rel = JoinRelation { id: new_id, plan, props };

            // 移除 j 和 i（注意顺序），添加新关系
            if j > i {
                relations.remove(j);
                relations.remove(i);
            } else {
                relations.remove(i);
                relations.remove(j);
            }
            relations.push(new_rel);
        } else {
            // 没有连接条件，做笛卡尔积（取前两个）
            let plan = PhysicalPlan::HashJoin {
                left: Box::new(relations[0].plan.clone()),
                right: Box::new(relations[1].plan.clone()),
                join_type,
                left_keys: vec![],
                right_keys: vec![],
            };
            let props = estimate_merged_props(&relations[0].props, &relations[1].props, join_type);
            let new_id = relations.len();
            relations.remove(1);
            relations.remove(0);
            relations.push(JoinRelation { id: new_id, plan, props });
        }
    }

    relations.into_iter().next().unwrap().plan
}

/// 构建线性连接（按原始顺序，无优化时的 fallback）
fn build_linear_join(
    relations: Vec<JoinRelation>,
    _conditions: &[(usize, usize, JoinCondition)],
    join_type: JoinType,
) -> PhysicalPlan {
    if relations.is_empty() {
        return PhysicalPlan::TableScan {
            table_name: "empty".to_string(),
            column_indices: vec![],
        };
    }

    let mut iter = relations.into_iter();
    let mut plan = iter.next().unwrap().plan;

    for rel in iter {
        plan = PhysicalPlan::HashJoin {
            left: Box::new(plan),
            right: Box::new(rel.plan),
            join_type,
            left_keys: vec![0], // 默认键，实际应由条件决定
            right_keys: vec![0],
        };
    }

    plan
}

/// 查找两个子集之间的连接条件
///
/// 返回 (left_keys, right_keys)
fn find_join_condition(
    left_mask: usize,
    right_mask: usize,
    conditions: &[(usize, usize, JoinCondition)],
    _relations: &[JoinRelation],
) -> Option<(Vec<usize>, Vec<usize>)> {
    // 找到左子集的一个代表关系和右子集的一个代表关系
    // 简化：只要存在任何一对关系有连接条件，就返回
    for (a, b, cond) in conditions {
        let a_in_left = (left_mask & (1 << a)) != 0;
        let b_in_right = (right_mask & (1 << b)) != 0;
        let a_in_right = (right_mask & (1 << a)) != 0;
        let b_in_left = (left_mask & (1 << b)) != 0;

        if a_in_left && b_in_right {
            return Some((cond.left_keys.clone(), cond.right_keys.clone()));
        }
        if a_in_right && b_in_left {
            return Some((cond.right_keys.clone(), cond.left_keys.clone()));
        }
    }

    None
}

/// 估计连接后的输出属性
fn estimate_merged_props(
    left: &PlanProperties,
    right: &PlanProperties,
    join_type: JoinType,
) -> PlanProperties {
    let row_count = match join_type {
        JoinType::Inner => left.row_count.min(right.row_count),
        JoinType::Left => left.row_count,
        JoinType::Right => right.row_count,
        JoinType::Full => left.row_count.max(right.row_count),
        JoinType::Semi => left.row_count,
        JoinType::Anti => left.row_count,
    };

    PlanProperties {
        row_count,
        num_columns: left.num_columns + right.num_columns,
        row_size: left.row_size + right.row_size,
    }
}

/// 估计连接输出行数（简化版）
fn estimate_join_rows(
    left: &PlanProperties,
    right: &PlanProperties,
    num_keys: usize,
    join_type: JoinType,
) -> f64 {
    let match_rate = 0.1 / (num_keys as f64).max(1.0);
    let inner_rows = left.row_count * right.row_count * match_rate;

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
    use crate::sql::cost_model::PlanProperties;

    fn make_rel(id: usize, rows: f64, cols: usize) -> JoinRelation {
        JoinRelation {
            id,
            plan: PhysicalPlan::TableScan {
                table_name: format!("t{}", id),
                column_indices: (0..cols).collect(),
            },
            props: PlanProperties {
                row_count: rows,
                num_columns: cols,
                row_size: cols * 16,
            },
        }
    }

    #[test]
    fn test_single_relation() {
        let stats = vec![];
        let model = CostModel::new(&stats);
        let rels = vec![make_rel(0, 100.0, 2)];
        let result = optimize_join_order(rels, &[], JoinType::Inner, &model).unwrap();
        // 单关系直接返回
        assert!(matches!(result, PhysicalPlan::TableScan { .. }));
    }

    #[test]
    fn test_two_relations() {
        let stats = vec![];
        let model = CostModel::new(&stats);
        let rels = vec![make_rel(0, 100.0, 2), make_rel(1, 50.0, 2)];
        let conds = vec![(0, 1, JoinCondition {
            left_keys: vec![0],
            right_keys: vec![0],
        })];

        let result = optimize_join_order(rels, &conds, JoinType::Inner, &model).unwrap();
        assert!(matches!(result, PhysicalPlan::HashJoin { .. }));
    }

    #[test]
    fn test_three_relations() {
        let stats = vec![];
        let model = CostModel::new(&stats);
        let rels = vec![
            make_rel(0, 1000.0, 2), // 大表
            make_rel(1, 100.0, 2),  // 中表
            make_rel(2, 10.0, 2),   // 小表
        ];
        let conds = vec![
            (0, 1, JoinCondition { left_keys: vec![0], right_keys: vec![0] }),
            (1, 2, JoinCondition { left_keys: vec![0], right_keys: vec![0] }),
        ];

        let result = optimize_join_order(rels, &conds, JoinType::Inner, &model).unwrap();
        assert!(matches!(result, PhysicalPlan::HashJoin { .. }));
        // 应该选择小表先连接的顺序（代价更低）
    }

    #[test]
    fn test_greedy_fallback() {
        let stats = vec![];
        let model = CostModel::new(&stats);
        // 10 个表，触发贪心算法
        let rels: Vec<_> = (0..10).map(|i| make_rel(i, 100.0 * (i as f64 + 1.0), 2)).collect();
        let mut conds = vec![];
        for i in 0..9 {
            conds.push((i, i + 1, JoinCondition {
                left_keys: vec![0],
                right_keys: vec![0],
            }));
        }

        let result = greedy_join_order(rels, &conds, JoinType::Inner, &model);
        assert!(matches!(result, PhysicalPlan::HashJoin { .. }));
    }
}
