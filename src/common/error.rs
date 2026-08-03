//! 错误类型定义

use thiserror::Error;

pub type Result<T> = std::result::Result<T, EngramDbError>;

#[derive(Error, Debug)]
pub enum EngramDbError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Serialization error: {0}")]
    Serialization(String),

    #[error("Parse error: {0}")]
    Parse(String),

    #[error("Invalid file format: {0}")]
    InvalidFormat(String),

    #[error("Table not found: {0}")]
    TableNotFound(String),

    #[error("Column not found: {0}")]
    ColumnNotFound(String),

    #[error("Index not found: {0}")]
    IndexNotFound(String),

    #[error("Type mismatch: expected {expected}, got {actual}")]
    TypeMismatch { expected: String, actual: String },

    #[error("Transaction error: {0}")]
    Transaction(String),

    #[error("Constraint violation: {0}")]
    ConstraintViolation(String),

    #[error("Not implemented: {0}")]
    NotImplemented(String),

    #[error("Internal error: {0}")]
    Internal(String),
}
