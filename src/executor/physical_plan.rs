//! 物理执行计划

use crate::common::types::TableDef;
use crate::Value;
use crate::sql::ast::Expression;

/// 物理计划节点
#[derive(Debug, Clone)]
pub enum PhysicalPlan {
    /// 全表扫描
    TableScan {
        table_name: String,
        column_indices: Vec<usize>,
    },
    /// 覆盖索引点查（v0.12.0 新增，IndexOnlyScan）
    ///
    /// 当 WHERE 条件为索引键列等值比较，且所有输出列都在索引覆盖范围内时，
    /// 直接从跳表索引返回结果，跳过全表扫描。
    IndexOnlyScan {
        table_name: String,
        index_name: String,
        /// 索引键列的等值查找值
        key_value: Value,
        /// 输出列的索引（对应表定义中的列索引）
        /// 这些列必须全部在索引的 key_columns + included_columns 中
        output_column_indices: Vec<usize>,
        /// 每个输出列在索引条目中的位置映射
        /// 键列排在前面（key_columns 顺序），然后是 included_columns 顺序
        /// output_col_map[i] = j 表示第 i 个输出列对应索引条目的第 j 个值
        output_col_map: Vec<usize>,
    },
    /// 过滤
    Filter {
        input: Box<PhysicalPlan>,
        condition: Expression,
    },
    /// 投影
    Projection {
        input: Box<PhysicalPlan>,
        expressions: Vec<Expression>,
        column_names: Vec<String>,
    },
    /// 聚合
    Aggregate {
        input: Box<PhysicalPlan>,
        group_by: Vec<usize>,
        aggregates: Vec<AggregateExpr>,
    },
    /// 插入
    Insert {
        table_name: String,
        rows: Vec<Vec<Value>>,
    },
    /// 列式插入（向量化写入路径）
    ///
    /// 直接以列式数据插入，跳过行→列转置开销。
    /// 用于 INSERT ... SELECT 等数据已在列存中的场景。
    InsertColumns {
        table_name: String,
        columns: Vec<Vec<Value>>,
    },
    /// 创建表
    CreateTable {
        table_def: TableDef,
    },
    /// 创建索引（v0.12.0 新增，覆盖索引）
    CreateIndex {
        table_name: String,
        index_name: String,
        key_columns: Vec<usize>,
        included_columns: Vec<usize>,
        unique: bool,
    },
    /// 删除（v0.12.0 新增，DELETE）
    Delete {
        table_name: String,
        /// WHERE 条件表达式（None 表示删除所有行）
        condition: Option<Expression>,
    },
    /// 更新（v0.12.0 新增，UPDATE）
    Update {
        table_name: String,
        /// SET 子句：(列索引, 新值表达式)
        assignments: Vec<(usize, Expression)>,
        /// WHERE 条件表达式（None 表示更新所有行）
        condition: Option<Expression>,
    },
    /// 排序（v0.12.0 新增，ORDER BY）
    Sort {
        input: Box<PhysicalPlan>,
        /// 排序键：列索引 + 方向
        sort_keys: Vec<SortKey>,
    },
    /// 哈希连接
    HashJoin {
        left: Box<PhysicalPlan>,
        right: Box<PhysicalPlan>,
        join_type: JoinType,
        left_keys: Vec<usize>,  // 左表连接键列索引
        right_keys: Vec<usize>, // 右表连接键列索引
    },
    /// 限制行数
    Limit {
        input: Box<PhysicalPlan>,
        limit: usize,
    },
    /// 分析表（收集统计信息）
    Analyze {
        table_name: String,
        column_indices: Vec<usize>,
    },
    /// 创建物化视图
    CreateMaterializedView {
        view_name: String,
        query: Box<PhysicalPlan>,
        column_names: Vec<String>,
        with_data: bool,
    },
    /// 刷新物化视图
    RefreshMaterializedView {
        view_name: String,
        concurrently: bool,
    },
    /// 删除物化视图
    DropMaterializedView {
        view_name: String,
        if_exists: bool,
    },
    /// 开始事务
    BeginTransaction,
    /// 提交
    Commit,
    /// 回滚
    Rollback,
}

/// 聚合表达式
#[derive(Debug, Clone)]
pub struct AggregateExpr {
    pub func: AggregateFunc,
    pub input: usize, // 输入列索引
}

/// 聚合函数
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AggregateFunc {
    Count,
    Sum,
    Avg,
    Min,
    Max,
}

/// 连接类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinType {
    /// 内连接：只返回匹配的行
    Inner,
    /// 左连接：返回左表所有行 + 右表匹配行（不匹配为 NULL）
    Left,
    /// 右连接：返回右表所有行 + 左表匹配行（不匹配为 NULL）
    Right,
    /// 全外连接：返回左右表所有行（不匹配为 NULL）
    Full,
}

/// 排序方向
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortDirection {
    Asc,
    Desc,
}

/// 排序键
#[derive(Debug, Clone)]
pub struct SortKey {
    pub column_index: usize,
    pub direction: SortDirection,
}
