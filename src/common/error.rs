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

    #[error("Operation not supported by engine: {0}")]
    NotSupported(String),

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_display_messages() {
        assert_eq!(EngramDbError::TableNotFound("t".into()).to_string(), "Table not found: t");
        assert_eq!(EngramDbError::Parse("bad sql".into()).to_string(), "Parse error: bad sql");
        assert_eq!(EngramDbError::InvalidFormat("bad".into()).to_string(), "Invalid file format: bad");
        assert_eq!(EngramDbError::NotSupported("x".into()).to_string(), "Operation not supported by engine: x");
        assert_eq!(EngramDbError::ColumnNotFound("c".into()).to_string(), "Column not found: c");
        assert_eq!(EngramDbError::IndexNotFound("i".into()).to_string(), "Index not found: i");
        assert_eq!(EngramDbError::ConstraintViolation("pk".into()).to_string(), "Constraint violation: pk");
        assert_eq!(EngramDbError::NotImplemented("f".into()).to_string(), "Not implemented: f");
        assert_eq!(EngramDbError::Internal("x".into()).to_string(), "Internal error: x");
        assert_eq!(EngramDbError::Serialization("s".into()).to_string(), "Serialization error: s");
        assert_eq!(EngramDbError::Transaction("t".into()).to_string(), "Transaction error: t");
    }

    #[test]
    fn test_type_mismatch_structured() {
        let e = EngramDbError::TypeMismatch { expected: "INT64".into(), actual: "TEXT".into() };
        assert_eq!(e.to_string(), "Type mismatch: expected INT64, got TEXT");
    }

    #[test]
    fn test_io_error_conversion() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "no file");
        let e: EngramDbError = io_err.into();
        assert!(e.to_string().starts_with("IO error:"));
    }
}
