// 分层索引专项测试（v0.19）：delta 稠密 + 列存稀疏
//
// 覆盖：legacy vs 分层对拍、compact 前后一致性、跨层 PK 冲突、
// 乱序负载降级、稀疏段持久化往返、数值归一化跨层。
use crate::common::config::Config;

/// 强制全量 compact（把 delta 全部并入列存，触发稀疏索引维护）
fn force_compact(db: &mut crate::storage::Database) -> u64 {
    db.compact_all().unwrap()
}

/// 分层模式（默认配置）下执行 SQL 序列，返回每步点查结果
fn run_layered(sqls: &[&str], point_queries: &[i64]) -> Vec<Option<i64>> {
    let cfg = Config::default();
    assert!(!cfg.primary_index_legacy);
    let mut conn = Connection::open_with_config(":memory:", cfg).unwrap();
    for sql in sqls {
        conn.execute(sql).unwrap();
    }
    point_queries
        .iter()
        .map(|&pk| {
            let r = conn
                .execute(&format!("SELECT v FROM t WHERE id = {pk}"))
                .unwrap();
            if r.rows.is_empty() {
                None
            } else {
                match &r.rows[0][0] {
                    Value::Int64(v) => Some(*v),
                    _ => None,
                }
            }
        })
        .collect()
}

/// legacy 模式（全表 BTreeMap）下执行同样 SQL 序列，作为正确性基准
fn run_legacy(sqls: &[&str], point_queries: &[i64]) -> Vec<Option<i64>> {
    let mut cfg = Config::default();
    cfg.primary_index_legacy = true;
    let mut conn = Connection::open_with_config(":memory:", cfg).unwrap();
    for sql in sqls {
        conn.execute(sql).unwrap();
    }
    point_queries
        .iter()
        .map(|&pk| {
            let r = conn
                .execute(&format!("SELECT v FROM t WHERE id = {pk}"))
                .unwrap();
            if r.rows.is_empty() {
                None
            } else {
                match &r.rows[0][0] {
                    Value::Int64(v) => Some(*v),
                    _ => None,
                }
            }
        })
        .collect()
}

#[test]
fn test_layered_vs_legacy_point_consistency() {
    // 基础对拍：插入（单行+批量）→ 点查 → 删除 → 点查 → 更新主键 → 点查
    let sqls = [
        "CREATE TABLE t (id INT PRIMARY KEY, v INT)",
        "INSERT INTO t VALUES (1, 10), (2, 20), (3, 30), (4, 40)",
        "INSERT INTO t VALUES (5, 50)",
        "DELETE FROM t WHERE id = 2",
        "UPDATE t SET id = 100 WHERE id = 3",
    ];
    let queries = [1i64, 2, 3, 4, 5, 100, 999];
    let layered = run_layered(&sqls, &queries);
    let legacy = run_legacy(&sqls, &queries);
    assert_eq!(
        layered, legacy,
        "分层与 legacy 点查结果必须一致: layered={layered:?} legacy={legacy:?}"
    );
    assert_eq!(layered[0], Some(10));
    assert_eq!(layered[1], None, "id=2 已删除");
    assert_eq!(layered[2], None, "id=3 已迁移");
    assert_eq!(layered[5], Some(30), "主键迁移后新主键命中");
}

#[test]
fn test_layered_compact_consistency() {
    // compact 前后一致性：数据从 delta → 列存（稀疏索引维护），点查不丢
    let mut conn = Connection::open(":memory:").unwrap();
    conn.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)").unwrap();
    for i in 0..500 {
        conn.execute(&format!("INSERT INTO t VALUES ({i}, {})", i * 2)).unwrap();
    }
    let check = |conn: &mut Connection| {
        for i in [0usize, 1, 127, 255, 499] {
            let r = conn
                .execute(&format!("SELECT v FROM t WHERE id = {i}"))
                .unwrap();
            assert_eq!(
                r.rows[0][0],
                Value::Int64((i * 2) as i64),
                "compact 前后点查必须命中 id={i}"
            );
        }
    };
    check(&mut conn);
    force_compact(conn.database_mut());
    check(&mut conn);
    // compact 后再插入一批，再 compact（第二批段追加）
    for i in 500..700 {
        conn.execute(&format!("INSERT INTO t VALUES ({i}, {})", i * 2)).unwrap();
    }
    force_compact(conn.database_mut());
    for i in [499usize, 500, 650, 699] {
        let r = conn.execute(&format!("SELECT v FROM t WHERE id = {i}")).unwrap();
        assert_eq!(r.rows[0][0], Value::Int64((i * 2) as i64));
    }
    // 全部落入列存后，稀疏 granule 应已建立
    let table = conn.database_mut().get_table("t").unwrap();
    assert!(
        table.column_store().sparse_granule_count() > 0,
        "compact 后稀疏索引应有 granule"
    );
    // 有序负载（主键单调递增）→ 全局有序模式（二分可用）
    assert!(table.column_store().sparse_sorted(), "有序负载应保持有序模式");
}

#[test]
fn test_layered_unsorted_load_degrades_gracefully() {
    // 乱序负载：第二批主键更小 → 全局有序性破坏 → 降级线性扫，正确性不受影响
    let mut conn = Connection::open(":memory:").unwrap();
    conn.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)").unwrap();
    for i in 100..200 {
        conn.execute(&format!("INSERT INTO t VALUES ({i}, {})", i)).unwrap();
    }
    force_compact(conn.database_mut());
    // 第二批：主键整体小于第一批
    for i in 0..100 {
        conn.execute(&format!("INSERT INTO t VALUES ({i}, {})", i)).unwrap();
    }
    force_compact(conn.database_mut());

    let table = conn.database_mut().get_table("t").unwrap();
    assert!(
        !table.column_store().sparse_sorted(),
        "乱序追加后应降级为无序模式"
    );
    for i in [0usize, 50, 99, 100, 150, 199] {
        let r = conn.execute(&format!("SELECT v FROM t WHERE id = {i}")).unwrap();
        assert_eq!(
            r.rows[0][0],
            Value::Int64(i as i64),
            "乱序负载点查必须正确 id={i}"
        );
    }
}

#[test]
fn test_layered_pk_conflict_across_layers() {
    // 跨层 PK 冲突：列存已有 1..500（compact 后），新插入撞 100 → 必须报错
    let mut conn = Connection::open(":memory:").unwrap();
    conn.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)").unwrap();
    for i in 0..500 {
        conn.execute(&format!("INSERT INTO t VALUES ({i}, {})", i)).unwrap();
    }
    force_compact(conn.database_mut());
    // 撞列存中的主键
    let err = conn
        .execute("INSERT INTO t VALUES (100, 999)")
        .unwrap_err();
    assert!(
        matches!(err, crate::common::error::EngramDbError::ConstraintViolation(_)),
        "跨层 PK 冲突必须报错: {err:?}"
    );
    // 撞 delta 中的主键（未 compact）
    conn.execute("INSERT INTO t VALUES (600, 1)").unwrap();
    let err = conn
        .execute("INSERT INTO t VALUES (600, 2)")
        .unwrap_err();
    assert!(
        matches!(err, crate::common::error::EngramDbError::ConstraintViolation(_)),
        "delta 层 PK 冲突必须报错: {err:?}"
    );
    // 同批自重复
    let err = conn
        .execute("INSERT INTO t VALUES (700, 1), (700, 2)")
        .unwrap_err();
    assert!(matches!(err, crate::common::error::EngramDbError::ConstraintViolation(_)));
}

#[test]
fn test_layered_sparse_persistence_roundtrip() {
    // 稀疏段持久化往返：写文件 → 重开 → 点查命中（不触发全量重建）
    let path = format!("/tmp/engramdb_layered_{}.hdb", std::process::id());
    std::fs::remove_file(&path).ok();
    std::fs::remove_file(format!("{path}-wal")).ok();
    {
        let mut conn = Connection::open(&path).unwrap();
        conn.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)").unwrap();
        for i in 0..1000 {
            conn.execute(&format!("INSERT INTO t VALUES ({i}, {})", i)).unwrap();
        }
        conn.close().unwrap();
    }
    {
        let mut conn = Connection::open(&path).unwrap();
        let r = conn.execute("SELECT v FROM t WHERE id = 777").unwrap();
        assert_eq!(r.rows[0][0], Value::Int64(777));
        // 稀疏索引应从持久化段恢复（granule 数 > 0，无需重建）
        let table = conn.database_mut().get_table("t").unwrap();
        assert!(
            table.column_store().sparse_granule_count() > 0,
            "重启后稀疏索引应从持久化段恢复"
        );
        conn.close().unwrap();
    }
    std::fs::remove_file(&path).ok();
    std::fs::remove_file(format!("{path}-wal")).ok();
}

#[test]
fn test_layered_normalized_pk_across_layers() {
    // 数值归一化跨层：Int32 主键 + Int64 字面量（列存层与 delta 层都验证）
    let mut conn = Connection::open(":memory:").unwrap();
    conn.execute("CREATE TABLE t (id INT PRIMARY KEY, v INT)").unwrap();
    conn.execute("INSERT INTO t VALUES (1, 10)").unwrap();
    // delta 层归一化
    let r = conn.execute("SELECT v FROM t WHERE id = 1").unwrap();
    assert_eq!(r.rows[0][0], Value::Int64(10));
    force_compact(conn.database_mut());
    // 列存层归一化
    let r = conn.execute("SELECT v FROM t WHERE id = 1").unwrap();
    assert_eq!(r.rows[0][0], Value::Int64(10));
}

#[test]
fn test_layered_no_pk_table_unaffected() {
    // 无主键表：分层索引零介入，行为不变
    let mut conn = Connection::open(":memory:").unwrap();
    conn.execute("CREATE TABLE t (id INT, v INT)").unwrap();
    conn.execute("INSERT INTO t VALUES (1, 10), (2, 20)").unwrap();
    force_compact(conn.database_mut());
    let r = conn.execute("SELECT COUNT(*) FROM t").unwrap();
    assert_eq!(r.rows[0][0], Value::Int64(2));
    let table = conn.database_mut().get_table("t").unwrap();
    assert_eq!(table.column_store().sparse_granule_count(), 0, "无主键表无稀疏索引");
}
