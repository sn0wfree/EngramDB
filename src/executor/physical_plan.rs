//! 物理执行计划

use crate::common::types::TableDef;
use crate::Value;
use crate::sql::ast::Expression;
use crate::sql::ast::{AlterTableStmt, PragmaStmt, WindowSpec};

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
        returning: Option<Vec<crate::sql::ast::SelectItem>>,
        on_conflict: Option<crate::sql::ast::OnConflictClause>,
    },
    /// 列式插入（向量化写入路径）
    ///
    /// 直接以列式数据插入，跳过行→列转置开销。
    /// 用于 INSERT ... SELECT 等数据已在列存中的场景。
    InsertColumns {
        table_name: String,
        columns: Vec<Vec<Value>>,
    },
    /// INSERT ... SELECT（v0.15.0 新增）
    ///
    /// 执行 source 子计划，将结果行插入目标表。
    /// 支持 INSERT ... SELECT ... FROM ... WHERE ...
    InsertSelect {
        table_name: String,
        columns: Option<Vec<String>>,
        source: Box<PhysicalPlan>,
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
        /// 索引类型（如 hnsw），None 表示普通 B-tree 索引
        using: Option<String>,
        /// 向量索引 WITH 选项
        with_options: Vec<(String, String)>,
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
        sort_keys: Vec<SortKey>,
        limit: Option<usize>,
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
    /// 行数元数据级短路查询（Perf01）
    ///
    /// 单表、无 WHERE、无 GROUP BY、无 HAVING、纯 COUNT(*) 等情况下，
    /// 直接从 `Table.def.row_count` 读元数据，跳过 TableScan→Aggregate 的逐行扫路径。
    /// `output_name` 为输出列名（如 `"count(*)"`），`count` 为预取的行数。
    CountStar {
        output_name: String,
        count: i64,
    },
    /// 主键点查物理节点（Perf03）
    ///
    /// 触发条件：单表查询，WHERE 唯一条件为 `pk_col = Literal`，
    /// 且表定义包含 PRIMARY KEY，且主键索引已启用。
    /// - O(log n) 命中 primary_index 得到 row_id
    /// - 回表取 0/1 行，输出列顺序与表 schema 一致（后续 Projection 做列裁剪）
    PrimaryKeyLookup {
        table_name: String,
        pk_value: crate::Value,
    },
    /// 开始事务
    BeginTransaction,
    /// 提交
    Commit,
    /// 回滚
    Rollback,
    /// 修改表
    AlterTable(AlterTableStmt),
    /// PRAGMA
    Pragma(PragmaStmt),
    /// DISTINCT 去重
    Distinct {
        input: Box<PhysicalPlan>,
    },
    /// EXPLAIN / EXPLAIN ANALYZE
    Explain {
        analyze: bool,
        plan: Box<PhysicalPlan>,
    },
    /// 窗口函数
    Window {
        input: Box<PhysicalPlan>,
        window_functions: Vec<WindowFunctionExpr>,
        column_names: Vec<String>,
    },
    /// 子查询扫描（执行子查询计划，返回结果）
    SubqueryScan {
        plan: Box<PhysicalPlan>,
    },
    /// 集合操作：UNION / UNION ALL（v0.15.0 新增）
    ///
    /// 合并 left 和 right 两个子计划的行：
    /// - UnionAll：直接拼接所有行
    /// - Union：拼接后按行去重（基于多列比较）
    SetUnion {
        op: SetUnionOp,
        left: Box<PhysicalPlan>,
        right: Box<PhysicalPlan>,
    },
    /// TRUNCATE TABLE（v0.15.0 新增）
    TruncateTable {
        table_name: String,
    },
    /// CREATE TABLE AS SELECT（v0.15.0 新增）
    ///
    /// 创建表并填充 SELECT 查询结果。
    CreateTableAs {
        table_def: TableDef,
        source: Box<PhysicalPlan>,
    },
    /// SAVEPOINT name（v0.15.0 Txn05 新增）
    Savepoint { name: String },
    /// RELEASE SAVEPOINT name（v0.15.0 Txn05 新增）
    ReleaseSavepoint { name: String },
    /// ROLLBACK TO SAVEPOINT name（v0.15.0 Txn05 新增）
    RollbackToSavepoint { name: String },
    /// 向量搜索表值函数（v0.15.0 V16 新增）
    VectorSearch {
        table_name: String,
        index_name: String,
        query_vector: Vec<f32>,
        k: usize,
    },
}

/// 集合操作类型（v0.15.0 新增）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetUnionOp {
    /// UNION：合并并去重
    Union,
    /// UNION ALL：合并不去重
    UnionAll,
}

/// 聚合表达式
#[derive(Debug, Clone)]
pub struct AggregateExpr {
    pub func: AggregateFunc,
    pub input: usize, // 输入列索引
    pub distinct: bool, // DISTINCT 去重
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
    Inner,
    Left,
    Right,
    Full,
    Semi,
    Anti,
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

/// 窗口函数表达式
#[derive(Debug, Clone)]
pub struct WindowFunctionExpr {
    pub func: WindowFuncType,
    pub input_column: Option<usize>,
    pub window_spec: WindowSpec,
    pub output_name: String,
}

/// 窗口函数类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowFuncType {
    RowNumber,
    Rank,
    DenseRank,
    Lag(usize),
    Lead(usize),
    FirstValue,
    LastValue,
    NthValue(usize),
    Count,
    Sum,
    Avg,
    Min,
    Max,
}

impl WindowFuncType {
    pub fn name(&self) -> &'static str {
        match self {
            WindowFuncType::RowNumber => "ROW_NUMBER",
            WindowFuncType::Rank => "RANK",
            WindowFuncType::DenseRank => "DENSE_RANK",
            WindowFuncType::Lag(_) => "LAG",
            WindowFuncType::Lead(_) => "LEAD",
            WindowFuncType::FirstValue => "FIRST_VALUE",
            WindowFuncType::LastValue => "LAST_VALUE",
            WindowFuncType::NthValue(_) => "NTH_VALUE",
            WindowFuncType::Count => "COUNT",
            WindowFuncType::Sum => "SUM",
            WindowFuncType::Avg => "AVG",
            WindowFuncType::Min => "MIN",
            WindowFuncType::Max => "MAX",
        }
    }
}
