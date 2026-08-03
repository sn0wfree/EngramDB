use crate::common::error::Result;
use crate::storage::Database;
use crate::sql::ast::PragmaStmt;
use crate::QueryResult;

pub fn execute(db: &mut Database, stmt: PragmaStmt) -> Result<QueryResult> {
    match stmt.name.to_lowercase().as_str() {
        "table_info" => {
            if let Some(table_name) = stmt.arg {
                let table = db.get_table(&table_name)
                    .ok_or_else(|| crate::common::error::EngramDbError::TableNotFound(table_name.clone()))?;
                let mut rows = Vec::with_capacity(table.def.columns.len());
                for (i, col) in table.def.columns.iter().enumerate() {
                    rows.push(vec![
                        crate::Value::Int32(i as i32),
                        crate::Value::Varchar(col.name.clone()),
                        crate::Value::Varchar(format!("{:?}", col.data_type)),
                        crate::Value::Boolean(!col.nullable),
                        crate::Value::Null,
                        crate::Value::Boolean(col.is_primary_key),
                    ]);
                }
                Ok(QueryResult {
                    columns: vec!["cid".into(), "name".into(), "type".into(), "notnull".into(), "dflt_value".into(), "pk".into()],
                    rows,
                    rows_affected: 0,
                })
            } else {
                Err(crate::common::error::EngramDbError::Parse("PRAGMA table_info requires table name".into()))
            }
        }
        "database_list" | "database_list" => {
            Ok(QueryResult {
                columns: vec!["seq".into(), "name".into(), "file".into()],
                rows: vec![vec![
                    crate::Value::Int32(0),
                    crate::Value::Varchar("main".into()),
                    crate::Value::Varchar(db.path().to_string_lossy().to_string()),
                ]],
                rows_affected: 0,
            })
        }
        _ => Ok(QueryResult {
            columns: vec!["status".to_string()],
            rows: vec![vec![crate::Value::Varchar(format!("PRAGMA {} ok", stmt.name))]],
            rows_affected: 0,
        }),
    }
}