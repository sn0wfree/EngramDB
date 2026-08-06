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
        AlterTableOp::DropColumn { column_name } => {
            let table = db.get_table_mut(&stmt.table_name)
                .ok_or_else(|| EngramDbError::TableNotFound(stmt.table_name.clone()))?;
            let col_idx = table.def().column_index(&column_name)
                .ok_or_else(|| EngramDbError::ColumnNotFound(column_name.clone()))?;
            table.def_mut().columns.remove(col_idx);
            Ok(QueryResult {
                columns: vec!["status".to_string()],
                rows: vec![vec![crate::Value::Varchar(format!("Column '{}' dropped from {}", column_name, stmt.table_name))]],
                rows_affected: 0,
            })
        }
        AlterTableOp::RenameColumn { old_name, new_name } => {
            let table = db.get_table_mut(&stmt.table_name)
                .ok_or_else(|| EngramDbError::TableNotFound(stmt.table_name.clone()))?;
            let col = table.def_mut().columns.iter_mut()
                .find(|c| c.name == old_name)
                .ok_or_else(|| EngramDbError::ColumnNotFound(old_name.clone()))?;
            col.name = new_name.clone();
            Ok(QueryResult {
                columns: vec!["status".to_string()],
                rows: vec![vec![crate::Value::Varchar(format!("Column '{}' renamed to '{}'", old_name, new_name))]],
                rows_affected: 0,
            })
        }
        AlterTableOp::RenameTable { new_name } => {
            db.rename_table(&stmt.table_name, &new_name)?;
            Ok(QueryResult {
                columns: vec!["status".to_string()],
                rows: vec![vec![crate::Value::Varchar(format!("Table renamed to '{}'", new_name))]],
                rows_affected: 0,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::ast::ColumnDef;

    fn mk_col(name: &str, dt: crate::common::types::DataType) -> crate::common::types::ColumnDef {
        crate::common::types::ColumnDef {
            name: name.into(), data_type: dt, nullable: true,
            is_primary_key: false, default_value: None, auto_increment: false,
        }
    }

    fn open_db() -> crate::storage::Database {
        let mut db = crate::storage::Database::open(":memory:").unwrap();
        db.create_table(crate::common::types::TableDef {
            id: 0,
            name: "t".into(),
            columns: vec![
                mk_col("a", crate::common::types::DataType::Int32),
                mk_col("b", crate::common::types::DataType::Varchar),
            ],
            row_count: 0, indexes: vec![], cluster_key: None, foreign_keys: vec![],
            engine: crate::common::types::EngineType::Columnar,
            next_auto_increment_id: 0, ttl_seconds: None, ttl_column: None,
        }).unwrap();
        db
    }

    #[test]
    fn test_add_column() {
        let mut db = open_db();
        let stmt = AlterTableStmt {
            table_name: "t".into(),
            operation: AlterTableOp::AddColumn {
                column_def: ColumnDef {
                    name: "c".into(), data_type: crate::common::types::DataType::Int64,
                    nullable: true, primary_key: false, auto_increment: false, unique: false,
                },
                position: None,
            },
        };
        let r = execute(&mut db, stmt).unwrap();
        assert_eq!(r.rows[0][0], crate::Value::Varchar("Column added to t".into()));
        let def = db.get_table("t").unwrap().def();
        assert_eq!(def.columns.len(), 3);
        assert_eq!(def.columns[2].name, "c");
        assert_eq!(def.columns[2].data_type, crate::common::types::DataType::Int64);
    }

    #[test]
    fn test_add_column_not_found() {
        let mut db = open_db();
        let stmt = AlterTableStmt {
            table_name: "missing".into(),
            operation: AlterTableOp::AddColumn {
                column_def: ColumnDef {
                    name: "c".into(), data_type: crate::common::types::DataType::Int32,
                    nullable: true, primary_key: false, auto_increment: false, unique: false,
                },
                position: None,
            },
        };
        let err = execute(&mut db, stmt).unwrap_err();
        assert!(matches!(err, EngramDbError::TableNotFound(_)), "got: {err:?}");
    }

    #[test]
    fn test_drop_column() {
        let mut db = open_db();
        let stmt = AlterTableStmt {
            table_name: "t".into(),
            operation: AlterTableOp::DropColumn { column_name: "b".into() },
        };
        let r = execute(&mut db, stmt).unwrap();
        assert_eq!(r.rows[0][0], crate::Value::Varchar("Column 'b' dropped from t".into()));
        assert_eq!(db.get_table("t").unwrap().def().columns.len(), 1);
        assert_eq!(db.get_table("t").unwrap().def().columns[0].name, "a");
        // 列不存在
        let stmt = AlterTableStmt {
            table_name: "t".into(),
            operation: AlterTableOp::DropColumn { column_name: "zzz".into() },
        };
        assert!(matches!(execute(&mut db, stmt).unwrap_err(), EngramDbError::ColumnNotFound(_)));
        // 表不存在
        let stmt = AlterTableStmt {
            table_name: "missing".into(),
            operation: AlterTableOp::DropColumn { column_name: "a".into() },
        };
        assert!(matches!(execute(&mut db, stmt).unwrap_err(), EngramDbError::TableNotFound(_)));
    }

    #[test]
    fn test_rename_column() {
        let mut db = open_db();
        let stmt = AlterTableStmt {
            table_name: "t".into(),
            operation: AlterTableOp::RenameColumn { old_name: "b".into(), new_name: "bb".into() },
        };
        let r = execute(&mut db, stmt).unwrap();
        assert_eq!(r.rows[0][0], crate::Value::Varchar("Column 'b' renamed to 'bb'".into()));
        assert_eq!(db.get_table("t").unwrap().def().columns[1].name, "bb");
        // 旧列不存在
        let stmt = AlterTableStmt {
            table_name: "t".into(),
            operation: AlterTableOp::RenameColumn { old_name: "zzz".into(), new_name: "x".into() },
        };
        assert!(matches!(execute(&mut db, stmt).unwrap_err(), EngramDbError::ColumnNotFound(_)));
        // 表不存在
        let stmt = AlterTableStmt {
            table_name: "missing".into(),
            operation: AlterTableOp::RenameColumn { old_name: "a".into(), new_name: "x".into() },
        };
        assert!(matches!(execute(&mut db, stmt).unwrap_err(), EngramDbError::TableNotFound(_)));
    }

    #[test]
    fn test_rename_table() {
        let mut db = open_db();
        let stmt = AlterTableStmt {
            table_name: "t".into(),
            operation: AlterTableOp::RenameTable { new_name: "t2".into() },
        };
        let r = execute(&mut db, stmt).unwrap();
        assert_eq!(r.rows[0][0], crate::Value::Varchar("Table renamed to 't2'".into()));
        assert!(db.get_table("t2").is_some());
        assert!(db.get_table("t").is_none());
        assert_eq!(db.get_table("t2").unwrap().def().name, "t2");
        // 表不存在
        let stmt = AlterTableStmt {
            table_name: "missing".into(),
            operation: AlterTableOp::RenameTable { new_name: "x".into() },
        };
        assert!(matches!(execute(&mut db, stmt).unwrap_err(), EngramDbError::TableNotFound(_)));
        // 重命名到已存在的表名 → 冲突
        db.create_table(crate::common::types::TableDef {
            id: 0,
            name: "other".into(),
            columns: vec![mk_col("a", crate::common::types::DataType::Int32)],
            row_count: 0, indexes: vec![], cluster_key: None, foreign_keys: vec![],
            engine: crate::common::types::EngineType::Columnar,
            next_auto_increment_id: 0, ttl_seconds: None, ttl_column: None,
        }).unwrap();
        let stmt = AlterTableStmt {
            table_name: "t2".into(),
            operation: AlterTableOp::RenameTable { new_name: "other".into() },
        };
        assert!(matches!(execute(&mut db, stmt).unwrap_err(), EngramDbError::ConstraintViolation(_)));
        // 冲突后原表仍可用
        assert!(db.get_table("t2").is_some());
    }
}