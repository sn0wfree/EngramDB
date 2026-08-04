use crate::common::error::Result;
use crate::storage::Database;
use crate::sql::ast::PragmaStmt;
use crate::QueryResult;
use crate::Value;

pub fn execute(db: &mut Database, stmt: PragmaStmt) -> Result<QueryResult> {
    match stmt.name.to_lowercase().as_str() {
        "table_info" => {
            if let Some(table_name) = stmt.arg {
                let table = db.get_table(&table_name)
                    .ok_or_else(|| crate::common::error::EngramDbError::TableNotFound(table_name.clone()))?;
                let mut rows = Vec::with_capacity(table.def.columns.len());
                for (i, col) in table.def.columns.iter().enumerate() {
                    rows.push(vec![
                        Value::Int32(i as i32),
                        Value::Varchar(col.name.clone()),
                        Value::Varchar(col.data_type.name().to_string()),
                        Value::Boolean(!col.nullable),
                        Value::Null,
                        Value::Boolean(col.is_primary_key),
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
                    Value::Int32(0),
                    Value::Varchar("main".into()),
                    Value::Varchar(db.path().to_string_lossy().to_string()),
                ]],
                rows_affected: 0,
            })
        }
        "information_schema_tables" | "tables" => {
            let mut rows = Vec::new();
            let mut names: Vec<String> = db.table_names().keys().cloned().collect();
            names.sort();
            for name in &names {
                if let Some(table) = db.get_table(name) {
                    rows.push(vec![
                        Value::Varchar(name.clone()),
                        Value::Varchar("BASE TABLE".to_string()),
                        Value::Int64(table.def.row_count as i64),
                        Value::Int32(table.def.columns.len() as i32),
                        Value::Varchar(format!("{:?}", table.def.primary_key_index().map(|i| i as i32))),
                        Value::Boolean(table.def.ttl_seconds.is_some()),
                    ]);
                }
            }
            Ok(QueryResult {
                columns: vec!["table_name".into(), "table_type".into(), "row_count".into(), "column_count".into(), "primary_key".into(), "has_ttl".into()],
                rows,
                rows_affected: 0,
            })
        }
        "information_schema_columns" | "columns" => {
            let mut rows = Vec::new();
            let mut names: Vec<String> = db.table_names().keys().cloned().collect();
            names.sort();
            for tbl_name in &names {
                if let Some(table) = db.get_table(tbl_name) {
                    for (i, col) in table.def.columns.iter().enumerate() {
                        rows.push(vec![
                            Value::Varchar(tbl_name.clone()),
                            Value::Varchar(col.name.clone()),
                            Value::Int32(i as i32),
                            Value::Varchar(col.data_type.name().to_string()),
                            Value::Boolean(col.nullable),
                            Value::Boolean(col.is_primary_key),
                            Value::Boolean(col.auto_increment),
                        ]);
                    }
                }
            }
            Ok(QueryResult {
                columns: vec!["table_name".into(), "column_name".into(), "ordinal_position".into(), "data_type".into(), "nullable".into(), "is_primary_key".into(), "auto_increment".into()],
                rows,
                rows_affected: 0,
            })
        }
        "information_schema_indexes" | "indexes" => {
            let mut rows = Vec::new();
            let mut names: Vec<String> = db.table_names().keys().cloned().collect();
            names.sort();
            for tbl_name in &names {
                if let Some(table) = db.get_table(tbl_name) {
                    for idx in &table.def.indexes {
                        let key_cols: Vec<String> = idx.key_columns.iter()
                            .map(|&i| table.def.columns.get(i).map(|c| c.name.clone()).unwrap_or_default())
                            .collect();
                        let incl_cols: Vec<String> = idx.included_columns.iter()
                            .map(|&i| table.def.columns.get(i).map(|c| c.name.clone()).unwrap_or_default())
                            .collect();
                        rows.push(vec![
                            Value::Varchar(tbl_name.clone()),
                            Value::Varchar(idx.name.clone()),
                            Value::Varchar(idx.index_type.clone()),
                            Value::Varchar(key_cols.join(", ")),
                            Value::Varchar(if incl_cols.is_empty() { "".into() } else { incl_cols.join(", ") }),
                            Value::Boolean(idx.unique),
                        ]);
                    }
                }
            }
            Ok(QueryResult {
                columns: vec!["table_name".into(), "index_name".into(), "index_type".into(), "key_columns".into(), "included_columns".into(), "unique".into()],
                rows,
                rows_affected: 0,
            })
        }
        _ => Ok(QueryResult {
            columns: vec!["status".to_string()],
            rows: vec![vec![Value::Varchar(format!("PRAGMA {} ok", stmt.name))]],
            rows_affected: 0,
        }),
    }
}