//! SQL 抽象语法树 (AST)

pub use crate::common::types::DataType;
use crate::Value;

/// SQL 语句
#[derive(Debug, Clone)]
pub enum Statement {
    CreateTable(CreateTableStmt),
    CreateIndex(CreateIndexStmt),
    Insert(InsertStmt),
    Select(SelectStmt),
    Delete(DeleteStmt),
    Update(UpdateStmt),
    BeginTransaction,
    Commit,
    Rollback,
    Analyze(AnalyzeStmt),
    CreateMaterializedView(CreateMaterializedViewStmt),
    RefreshMaterializedView(RefreshMaterializedViewStmt),
    DropMaterializedView(DropMaterializedViewStmt),
    AlterTable(AlterTableStmt),
    Pragma(PragmaStmt),
    Explain(ExplainStmt),
    /// TRUNCATE TABLE（v0.15.0 新增）
    TruncateTable {
        table_name: String,
    },
}

#[derive(Debug, Clone)]
pub struct ExplainStmt {
    pub analyze: bool,
    pub statement: Box<Statement>,
}

#[derive(Debug, Clone)]
pub struct AlterTableStmt {
    pub table_name: String,
    pub operation: AlterTableOp,
}

#[derive(Debug, Clone)]
pub enum AlterTableOp {
    AddColumn { column_def: ColumnDef, position: Option<String> },
    DropColumn { column_name: String },
    RenameColumn { old_name: String, new_name: String },
    RenameTable { new_name: String },
}

#[derive(Debug, Clone)]
pub struct PragmaStmt {
    pub name: String,
    pub arg: Option<String>,
}

/// CREATE INDEX 语句（v0.12.0 新增，覆盖索引）
#[derive(Debug, Clone)]
pub struct CreateIndexStmt {
    pub index_name: String,
    pub table_name: String,
    /// 索引键列（按顺序）
    pub key_columns: Vec<String>,
    /// 覆盖列（INCLUDE 子句，冗余存储在索引中，查询时免回表）
    pub included_columns: Vec<String>,
    /// 是否唯一索引
    pub unique: bool,
}

/// CREATE TABLE 语句
#[derive(Debug, Clone)]
pub struct CreateTableStmt {
    pub table_name: String,
    pub columns: Vec<ColumnDef>,
}

/// 列定义
#[derive(Debug, Clone)]
pub struct ColumnDef {
    pub name: String,
    pub data_type: DataType,
    pub nullable: bool,
    pub primary_key: bool,
    /// AUTO_INCREMENT 自增主键（v0.14.0 新增）
    pub auto_increment: bool,
    /// 列级 UNIQUE 约束（v0.14.0 新增）
    pub unique: bool,
}

/// INSERT 语句
#[derive(Debug, Clone)]
pub struct InsertStmt {
    pub table_name: String,
    pub columns: Option<Vec<String>>,
    /// INSERT ... VALUES 的字面值
    pub values: Vec<Vec<Expression>>,
    /// INSERT ... SELECT 的 SELECT 子查询（v0.15.0 新增）
    pub select: Option<Box<SelectStmt>>,
    pub returning: Option<Vec<SelectItem>>,
    pub on_conflict: Option<OnConflictClause>,
}

#[derive(Debug, Clone)]
pub struct OnConflictClause {
    pub conflict_columns: Vec<String>,
    pub action: OnConflictAction,
}

#[derive(Debug, Clone)]
pub enum OnConflictAction {
    DoNothing,
    DoUpdate { assignments: Vec<(String, Expression)> }}

/// DELETE 语句（v0.12.0 新增）
#[derive(Debug, Clone)]
pub struct DeleteStmt {
    pub table_name: String,
    pub where_clause: Option<Expression>,
}

/// UPDATE 语句（v0.12.0 新增）
#[derive(Debug, Clone)]
pub struct UpdateStmt {
    pub table_name: String,
    /// SET 子句：列名 → 新值表达式
    pub assignments: Vec<(String, Expression)>,
    pub where_clause: Option<Expression>,
}

/// SELECT 语句
#[derive(Debug, Clone)]
pub struct SelectStmt {
    pub select_list: Vec<SelectItem>,
    pub from: Option<TableRef>,
    pub where_clause: Option<Expression>,
    pub group_by: Vec<Expression>,
    pub having: Option<Expression>,
    pub order_by: Vec<OrderByItem>,
    pub limit: Option<usize>,
    pub distinct: bool,
    /// CTE (WITH 子句)
    pub ctes: Vec<Cte>,
    /// 集合操作（v0.15.0 新增）：UNION / UNION ALL
    ///
    /// `set_op = Some((Union, right))` 表示 `self UNION right`
    pub set_op: Option<(SetOpType, Box<SelectStmt>)>,
}

/// 集合操作类型（v0.15.0 新增）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetOpType {
    /// UNION：合并两个结果集并去重
    Union,
    /// UNION ALL：合并两个结果集不去重
    UnionAll,
}

/// ANALYZE 语句：收集表的统计信息
#[derive(Debug, Clone)]
pub struct AnalyzeStmt {
    pub table_name: String,
    /// 指定要分析的列；为空表示所有列
    pub columns: Vec<String>,
}

/// CREATE MATERIALIZED VIEW 语句
#[derive(Debug, Clone)]
pub struct CreateMaterializedViewStmt {
    pub view_name: String,
    /// 视图定义查询
    pub query: Box<SelectStmt>,
    /// 是否立即填充数据
    pub with_data: bool,
}

/// REFRESH MATERIALIZED VIEW 语句
#[derive(Debug, Clone)]
pub struct RefreshMaterializedViewStmt {
    pub view_name: String,
    /// 是否并发刷新（不阻塞读）
    pub concurrently: bool,
}

/// DROP MATERIALIZED VIEW 语句
#[derive(Debug, Clone)]
pub struct DropMaterializedViewStmt {
    pub view_name: String,
    pub if_exists: bool,
}

/// SELECT 项
#[derive(Debug, Clone)]
pub enum SelectItem {
    Wildcard,
    Expression(Expression, Option<String>), // expr, alias
}

/// 表引用
#[derive(Debug, Clone)]
pub enum TableRef {
    /// 物理表
    Table { table_name: String, alias: Option<String> },
    /// 派生表（子查询）
    Derived { query: Box<SelectStmt>, alias: String },
}

/// ORDER BY 项
#[derive(Debug, Clone)]
pub struct OrderByItem {
    pub expr: Expression,
    pub ascending: bool,
}

/// 表达式
#[derive(Debug, Clone)]
pub enum Expression {
    Literal(Value),
    /// 参数占位符（? 或 $1），用于 prepared statement
    Placeholder(usize),
    ColumnRef {
        table: Option<String>,
        column: String,
    },
    BinaryOp {
        left: Box<Expression>,
        op: BinaryOperator,
        right: Box<Expression>,
    },
    UnaryOp {
        op: UnaryOperator,
        expr: Box<Expression>,
    },
    /// 函数调用
    Function {
        name: String,
        args: Vec<Expression>,
        distinct: bool,
        count_star: bool,
        /// OVER 子句（窗口函数）
        over: Option<WindowSpec>,
    },
    /// CAST(expr AS type)
    Cast {
        expr: Box<Expression>,
        data_type: DataType,
    },
    /// IS NULL
    IsNull(Box<Expression>),
    /// IS NOT NULL
    IsNotNull(Box<Expression>),
    /// expr IN (list)
    InList {
        expr: Box<Expression>,
        list: Vec<Expression>,
    },
    /// expr LIKE pattern
    Like {
        expr: Box<Expression>,
        pattern: Box<Expression>,
    },
    /// CASE WHEN ... THEN ... [ELSE ...] END
    Case {
        when_then: Vec<(Expression, Expression)>,
        else_expr: Option<Box<Expression>>,
    },
    /// 标量子查询 (SELECT ...)
    Subquery(Box<SelectStmt>),
    /// EXISTS (SELECT ...)
    Exists {
        subquery: Box<SelectStmt>,
        negated: bool,
    },
    /// expr IN (SELECT ...)
    InSubquery {
        expr: Box<Expression>,
        subquery: Box<SelectStmt>,
        negated: bool,
    },
}

/// 二元运算符
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOperator {
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    Plus,
    Minus,
    Multiply,
    Divide,
    Modulo,
    And,
    Or,
    Concat,
}

/// 一元运算符
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOperator {
    Not,
    Negate,
}

/// CTE（WITH 子句）
#[derive(Debug, Clone)]
pub struct Cte {
    pub alias: String,
    pub query: Box<SelectStmt>,
    pub columns: Vec<String>,
}

/// 窗口规范（OVER 子句）
#[derive(Debug, Clone)]
pub struct WindowSpec {
    pub partition_by: Vec<Expression>,
    pub order_by: Vec<OrderByItem>,
    pub window_frame: Option<WindowFrame>,
}

/// 窗口帧
#[derive(Debug, Clone)]
pub struct WindowFrame {
    pub units: WindowFrameUnits,
    pub start: WindowFrameBound,
    pub end: Option<WindowFrameBound>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowFrameUnits {
    Rows,
    Range,
    Groups,
}

#[derive(Debug, Clone)]
pub enum WindowFrameBound {
    UnboundedPreceding,
    NPreceding(usize),
    CurrentRow,
    NFollowing(usize),
    UnboundedFollowing,
}
