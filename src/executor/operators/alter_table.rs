use crate::common::error::{Result, EngramDbError};
use crate::storage::Database;
use crate::sql::ast::{AlterTableStmt, AlterTableOp, ColumnDef};
use crate::QueryResult;

pub fn execute(db: &mut Database, stmt: AlterTableStmt) -> Result<QueryResult> {
    match stmt.operation {
        AlterTableOp::AddColumn { column_def, .. } => {
            let def = crate::common::types::ColumnDef {
                name: column_def.name.clone(),
                data_type: column_def.data_type,
                nullable: column_def.nullable,
                is_primary_key: column_def.primary_key,
                default_value: None,
                auto_increment: false,
            };
            db.get_table_mut(&stmt.table_name)
                .ok_or_else(|| EngramDbError::TableNotFound(stmt.table_name.clone()))?
                .def_mut().columns.push(def);
            Ok(QueryResult {
                columns: vec!["status".to_string()],
                rows: vec![vec![crate::Value::Varchar(format!("Column added to {}", stmt.table_name))]],
                rows_affected: 0,
            })
        }
        _ => Err(EngramDbError::Parse("Unsupported ALTER TABLE operation".into())),
    }
}