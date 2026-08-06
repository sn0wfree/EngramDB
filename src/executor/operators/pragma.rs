use crate::common::config::WalFlushMode;
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
        "database_list" => {
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
                if let Some(table) = db.get_engine_table(name) {
                    let def = table.def();
                    rows.push(vec![
                        Value::Varchar(name.clone()),
                        Value::Varchar("BASE TABLE".to_string()),
                        Value::Int64(def.row_count as i64),
                        Value::Int32(def.columns.len() as i32),
                        Value::Varchar(format!("{:?}", def.primary_key_index().map(|i| i as i32))),
                        Value::Boolean(def.ttl_seconds.is_some()),
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
                if let Some(table) = db.get_engine_table(tbl_name) {
                    for (i, col) in table.def().columns.iter().enumerate() {
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

        // P03: PRAGMA index_info / index_list — 索引详细信息
        "index_info" => {
            if let Some(table_name) = stmt.arg {
                let table = db.get_table(&table_name)
                    .ok_or_else(|| crate::common::error::EngramDbError::TableNotFound(table_name.clone()))?;
                let mut rows = Vec::new();
                for idx in &table.def.indexes {
                    for (seq, &col_idx) in idx.key_columns.iter().enumerate() {
                        let col_name = table.def.columns.get(col_idx).map(|c| c.name.clone()).unwrap_or_default();
                        rows.push(vec![
                            Value::Int32(seq as i32),
                            Value::Varchar(col_name),
                            Value::Varchar(idx.name.clone()),
                        ]);
                    }
                }
                Ok(QueryResult {
                    columns: vec!["seqno".into(), "name".into(), "index_name".into()],
                    rows,
                    rows_affected: 0,
                })
            } else {
                Err(crate::common::error::EngramDbError::Parse("PRAGMA index_info requires table name".into()))
            }
        }
        "index_list" => {
            if let Some(table_name) = stmt.arg {
                let table = db.get_table(&table_name)
                    .ok_or_else(|| crate::common::error::EngramDbError::TableNotFound(table_name.clone()))?;
                let mut rows = Vec::new();
                for (seq, idx) in table.def.indexes.iter().enumerate() {
                    let key_cols: Vec<String> = idx.key_columns.iter()
                        .map(|&i| table.def.columns.get(i).map(|c| c.name.clone()).unwrap_or_default())
                        .collect();
                    rows.push(vec![
                        Value::Int32(seq as i32),
                        Value::Varchar(idx.name.clone()),
                        Value::Varchar(key_cols.join(", ")),
                        Value::Boolean(idx.unique),
                        Value::Varchar(idx.index_type.clone()),
                    ]);
                }
                Ok(QueryResult {
                    columns: vec!["seq".into(), "name".into(), "key_columns".into(), "unique".into(), "type".into()],
                    rows,
                    rows_affected: 0,
                })
            } else {
                Err(crate::common::error::EngramDbError::Parse("PRAGMA index_list requires table name".into()))
            }
        }

        // P04: PRAGMA journal_mode — WAL / 普通模式切换
        "journal_mode" => {
            let mode = if let Some(arg) = stmt.arg {
                let new_mode = match arg.to_lowercase().as_str() {
                    "wal" => WalFlushMode::Sync,
                    "delete" | "truncate" | "persist" => WalFlushMode::Periodic,
                    "memory" | "off" => WalFlushMode::BufferFull,
                    _ => { return Err(crate::common::error::EngramDbError::Parse(format!("unknown journal_mode: {}", arg))); }
                };
                db.set_wal_flush_mode(new_mode);
                arg.to_uppercase()
            } else {
                match db.config().wal_flush_mode {
                    WalFlushMode::Sync => "wal",
                    WalFlushMode::Periodic => "delete",
                    WalFlushMode::BufferFull => "off",
                }.to_string()
            };
            Ok(QueryResult {
                columns: vec!["journal_mode".into()],
                rows: vec![vec![Value::Varchar(mode)]],
                rows_affected: 0,
            })
        }

        // P05: PRAGMA synchronous — 同步级别设置
        "synchronous" => {
            let level = if let Some(arg) = stmt.arg {
                match arg.to_lowercase().as_str() {
                    "0" | "off" => {
                        db.set_wal_flush_mode(WalFlushMode::BufferFull);
                        "0".to_string()
                    }
                    "1" | "normal" => {
                        db.set_wal_flush_mode(WalFlushMode::Periodic);
                        "1".to_string()
                    }
                    "2" | "full" => {
                        db.set_wal_flush_mode(WalFlushMode::Sync);
                        "2".to_string()
                    }
                    _ => { return Err(crate::common::error::EngramDbError::Parse(format!("unknown synchronous level: {}", arg))); }
                }
            } else {
                match db.config().wal_flush_mode {
                    WalFlushMode::Sync => "2",
                    WalFlushMode::Periodic => "1",
                    WalFlushMode::BufferFull => "0",
                }.to_string()
            };
            Ok(QueryResult {
                columns: vec!["synchronous".into()],
                rows: vec![vec![Value::Varchar(level)]],
                rows_affected: 0,
            })
        }

        // P06: PRAGMA cache_size — 缓存大小设置
        "cache_size" => {
            let size = if let Some(arg) = stmt.arg {
                let kb: i64 = arg.parse().map_err(|_| {
                    crate::common::error::EngramDbError::Parse(format!("invalid cache_size: {}", arg))
                })?;
                let bytes = if kb >= 0 { kb as usize * 1024 } else { ((-kb) as usize) * 1024 };
                db.cache().set_max_memory(bytes);
                kb
            } else {
                (db.cache().max_memory() / 1024) as i64
            };
            Ok(QueryResult {
                columns: vec!["cache_size".into()],
                rows: vec![vec![Value::Int64(size)]],
                rows_affected: 0,
            })
        }

        // P07: PRAGMA page_size / page_count — 页大小和页数
        "page_size" => {
            let size = db.config().block_size as i64;
            Ok(QueryResult {
                columns: vec!["page_size".into()],
                rows: vec![vec![Value::Int64(size)]],
                rows_affected: 0,
            })
        }
        "page_count" => {
            let file_size = std::fs::metadata(db.path()).map(|m| m.len()).unwrap_or(0);
            let page_size = db.config().block_size as u64;
            let count = if page_size > 0 { file_size / page_size } else { 0 };
            Ok(QueryResult {
                columns: vec!["page_count".into()],
                rows: vec![vec![Value::Int64(count as i64)]],
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::config::WalFlushMode;
    use crate::sql::ast::PragmaStmt;

    fn open_db() -> crate::storage::Database {
        crate::storage::Database::open(":memory:").unwrap()
    }

    fn exec(db: &mut crate::storage::Database, name: &str, arg: Option<&str>) -> QueryResult {
        execute(
            db,
            PragmaStmt { name: name.into(), arg: arg.map(|s| s.into()) },
        ).unwrap()
    }

    fn exec_err(db: &mut crate::storage::Database, name: &str, arg: Option<&str>) -> crate::common::error::EngramDbError {
        execute(
            db,
            PragmaStmt { name: name.into(), arg: arg.map(|s| s.into()) },
        ).unwrap_err()
    }

    #[test]
    fn test_table_info_basic() {
        let mut db = open_db();
        db.create_table(crate::common::types::TableDef {
            id: 0,
            name: "users".into(),
            columns: vec![
                crate::common::types::ColumnDef {
                    name: "id".into(), data_type: crate::common::types::DataType::Int32,
                    nullable: false, is_primary_key: true, default_value: None, auto_increment: false,
                },
                crate::common::types::ColumnDef {
                    name: "name".into(), data_type: crate::common::types::DataType::Varchar,
                    nullable: true, is_primary_key: false, default_value: None, auto_increment: false,
                },
            ],
            row_count: 0, indexes: vec![], cluster_key: None, foreign_keys: vec![],
            engine: crate::common::types::EngineType::Columnar,
            next_auto_increment_id: 0, ttl_seconds: None, ttl_column: None,
        }).unwrap();

        let r = exec(&mut db, "table_info", Some("users"));
        assert_eq!(r.rows.len(), 2);
        assert_eq!(r.columns, vec!["cid", "name", "type", "notnull", "dflt_value", "pk"]);
        // 第一列：id，非空，主键
        assert_eq!(r.rows[0][0], Value::Int32(0));
        assert_eq!(r.rows[0][1], Value::Varchar("id".into()));
        assert_eq!(r.rows[0][3], Value::Boolean(true));
        assert_eq!(r.rows[0][4], Value::Null);
        assert_eq!(r.rows[0][5], Value::Boolean(true));
        // 第二列：name，可空，非主键
        assert_eq!(r.rows[1][1], Value::Varchar("name".into()));
        assert_eq!(r.rows[1][3], Value::Boolean(false));
        assert_eq!(r.rows[1][5], Value::Boolean(false));
    }

    #[test]
    fn test_table_info_errors() {
        let mut db = open_db();
        // 缺表名
        let err = exec_err(&mut db, "table_info", None);
        assert!(matches!(err, crate::common::error::EngramDbError::Parse(_)), "got: {err:?}");
        // 表不存在
        let err = exec_err(&mut db, "table_info", Some("nope"));
        assert!(matches!(err, crate::common::error::EngramDbError::TableNotFound(_)), "got: {err:?}");
    }

    #[test]
    fn test_database_list() {
        let mut db = open_db();
        let r = exec(&mut db, "database_list", None);
        assert_eq!(r.rows.len(), 1);
        assert_eq!(r.rows[0][0], Value::Int32(0));
        assert_eq!(r.rows[0][1], Value::Varchar("main".into()));
    }

    #[test]
    fn test_tables_and_columns_info() {
        let mut db = open_db();
        let mk_col = |name: &str, dt: crate::common::types::DataType| crate::common::types::ColumnDef {
            name: name.into(), data_type: dt, nullable: true,
            is_primary_key: false, default_value: None, auto_increment: false,
        };
        let mk_def = |name: &str, cols: Vec<crate::common::types::ColumnDef>, engine: crate::common::types::EngineType| {
            crate::common::types::TableDef {
                id: 0, name: name.into(), columns: cols, row_count: 0, indexes: vec![],
                cluster_key: None, foreign_keys: vec![], engine,
                next_auto_increment_id: 0, ttl_seconds: None, ttl_column: None,
            }
        };
        db.create_table(mk_def("b_tbl", vec![mk_col("c1", crate::common::types::DataType::Int32)], crate::common::types::EngineType::Columnar)).unwrap();
        db.create_table(mk_def("a_tbl", vec![mk_col("x", crate::common::types::DataType::Varchar)], crate::common::types::EngineType::Memory)).unwrap();

        // information_schema_tables：按名称排序
        let r = exec(&mut db, "information_schema_tables", None);
        assert_eq!(r.rows.len(), 2);
        assert_eq!(r.rows[0][0], Value::Varchar("a_tbl".into()));
        assert_eq!(r.rows[1][0], Value::Varchar("b_tbl".into()));
        assert_eq!(r.rows[0][1], Value::Varchar("BASE TABLE".into()));

        // 别名 "tables" 等价
        let r2 = exec(&mut db, "tables", None);
        assert_eq!(r2.rows.len(), 2);

        // information_schema_columns：2 表共 2 列
        let r = exec(&mut db, "information_schema_columns", None);
        assert_eq!(r.rows.len(), 2);
        assert_eq!(r.rows[0][0], Value::Varchar("a_tbl".into()));
        assert_eq!(r.rows[0][2], Value::Int32(0));
        // 别名 "columns"
        let r2 = exec(&mut db, "columns", None);
        assert_eq!(r2.rows.len(), 2);
    }

    #[test]
    fn test_indexes_info() {
        let mut db = open_db();
        db.create_table(crate::common::types::TableDef {
            id: 0,
            name: "t".into(),
            columns: vec![
                crate::common::types::ColumnDef {
                    name: "id".into(), data_type: crate::common::types::DataType::Int32,
                    nullable: false, is_primary_key: true, default_value: None, auto_increment: false,
                },
                crate::common::types::ColumnDef {
                    name: "v".into(), data_type: crate::common::types::DataType::Int32,
                    nullable: true, is_primary_key: false, default_value: None, auto_increment: false,
                },
            ],
            row_count: 0, indexes: vec![], cluster_key: None, foreign_keys: vec![],
            engine: crate::common::types::EngineType::Columnar,
            next_auto_increment_id: 0, ttl_seconds: None, ttl_column: None,
        }).unwrap();
        db.create_index("t", "idx_v", &[1usize], &[], false).unwrap();

        // information_schema_indexes
        let r = exec(&mut db, "information_schema_indexes", None);
        assert_eq!(r.rows.len(), 1);
        assert_eq!(r.rows[0][0], Value::Varchar("t".into()));
        assert_eq!(r.rows[0][1], Value::Varchar("idx_v".into()));
        assert_eq!(r.rows[0][3], Value::Varchar("v".into()));

        // index_list
        let r = exec(&mut db, "index_list", Some("t"));
        assert_eq!(r.rows.len(), 1);
        assert_eq!(r.rows[0][1], Value::Varchar("idx_v".into()));
        assert_eq!(r.rows[0][3], Value::Boolean(false));

        // index_info：键列明细
        let r = exec(&mut db, "index_info", Some("t"));
        assert_eq!(r.rows.len(), 1);
        assert_eq!(r.rows[0][0], Value::Int32(0));
        assert_eq!(r.rows[0][1], Value::Varchar("v".into()));
        assert_eq!(r.rows[0][2], Value::Varchar("idx_v".into()));

        // 缺表名 / 表不存在
        assert!(matches!(exec_err(&mut db, "index_info", None), crate::common::error::EngramDbError::Parse(_)));
        assert!(matches!(exec_err(&mut db, "index_list", Some("nope")), crate::common::error::EngramDbError::TableNotFound(_)));
    }

    #[test]
    fn test_journal_mode() {
        let mut db = open_db();
        // 默认（Sync → wal）
        let r = exec(&mut db, "journal_mode", None);
        assert_eq!(r.rows[0][0], Value::Varchar("wal".into()));
        // 设置
        let r = exec(&mut db, "journal_mode", Some("delete"));
        assert_eq!(r.rows[0][0], Value::Varchar("DELETE".into()));
        assert_eq!(db.config().wal_flush_mode, WalFlushMode::Periodic);
        let r = exec(&mut db, "journal_mode", Some("off"));
        assert_eq!(db.config().wal_flush_mode, WalFlushMode::BufferFull);
        let r = exec(&mut db, "journal_mode", Some("wal"));
        assert_eq!(r.rows[0][0], Value::Varchar("WAL".into()));
        assert_eq!(db.config().wal_flush_mode, WalFlushMode::Sync);
        // 未知模式
        let err = exec_err(&mut db, "journal_mode", Some("bogus"));
        assert!(matches!(err, crate::common::error::EngramDbError::Parse(_)), "got: {err:?}");
    }

    #[test]
    fn test_synchronous() {
        let mut db = open_db();
        // 默认（Sync → "2"）
        let r = exec(&mut db, "synchronous", None);
        assert_eq!(r.rows[0][0], Value::Varchar("2".into()));
        exec(&mut db, "synchronous", Some("off"));
        assert_eq!(db.config().wal_flush_mode, WalFlushMode::BufferFull);
        exec(&mut db, "synchronous", Some("1"));
        assert_eq!(db.config().wal_flush_mode, WalFlushMode::Periodic);
        let r = exec(&mut db, "synchronous", Some("full"));
        assert_eq!(r.rows[0][0], Value::Varchar("2".into()));
        assert_eq!(db.config().wal_flush_mode, WalFlushMode::Sync);
        // 未知级别
        assert!(matches!(exec_err(&mut db, "synchronous", Some("9")), crate::common::error::EngramDbError::Parse(_)));
    }

    #[test]
    fn test_cache_size() {
        let mut db = open_db();
        // 读取默认
        let r = exec(&mut db, "cache_size", None);
        assert_eq!(r.rows[0][0], Value::Int64((db.cache().max_memory() / 1024) as i64));
        // 设置正值
        exec(&mut db, "cache_size", Some("4096"));
        assert_eq!(db.cache().max_memory(), 4096 * 1024);
        // 设置负值（绝对值）
        exec(&mut db, "cache_size", Some("-2048"));
        assert_eq!(db.cache().max_memory(), 2048 * 1024);
        // 非法值
        assert!(matches!(exec_err(&mut db, "cache_size", Some("abc")), crate::common::error::EngramDbError::Parse(_)));
    }

    #[test]
    fn test_page_size_and_count() {
        let mut db = open_db();
        let r = exec(&mut db, "page_size", None);
        assert_eq!(r.rows[0][0], Value::Int64(db.config().block_size as i64));
        // page_count：文件大小 > 0
        let r = exec(&mut db, "page_count", None);
        assert_eq!(r.rows[0][0], Value::Int64((std::fs::metadata(db.path()).unwrap().len() / db.config().block_size as u64) as i64));
    }

    #[test]
    fn test_unknown_pragma_ok() {
        let mut db = open_db();
        let r = exec(&mut db, "some_unknown_pragma", Some("x"));
        assert_eq!(r.rows[0][0], Value::Varchar("PRAGMA some_unknown_pragma ok".into()));
    }
}