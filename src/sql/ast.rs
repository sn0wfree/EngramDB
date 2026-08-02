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
    /// ANALYZE：收集表的统计信息（供 CBO 使用）
    Analyze(AnalyzeStmt),
    /// CREATE MATERIALIZED VIEW
    CreateMaterializedView(CreateMaterializedViewStmt),
    /// REFRESH MATERIALIZED VIEW
    RefreshMaterializedView(RefreshMaterializedViewStmt),
    /// DROP MATERIALIZED VIEW
    DropMaterializedView(DropMaterializedViewStmt),
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
}

/// INSERT 语句
#[derive(Debug, Clone)]
pub struct InsertStmt {
    pub table_name: String,
    pub columns: Option<Vec<String>>,
    pub values: Vec<Vec<Expression>>,
}

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
pub struct TableRef {
    pub table_name: String,
    pub alias: Option<String>,
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
