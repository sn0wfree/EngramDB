//! Phase 0 语义验收闸门（纯 Rust，llmRx 零投入）
//!
//! 七项：
//!   G1 AUTOINCREMENT 建表可执行
//!   G2 NOT NULL DEFAULT 真强制
//!   G3 UNIQUE 真拒绝
//!   G4 ON CONFLICT DO UPDATE / DO NOTHING 语义一致
//!   G5 RETURNING id 返回值正确
//!   G6 ALTER TABLE ADD COLUMN 幂等
//!   G7 FK + CHECK 行为记录 + 补偿方案依据
//!
//! 运行：`cargo test --test phase0_semantic_gate -- --nocapture`

use engramdb::{Connection, Value};

fn open() -> Connection {
    Connection::open(":memory:").expect("open memory db")
}

/// G1：SQLite 拼写 AUTOINCREMENT（非 AUTO_INCREMENT）建表 + 自增分配
#[test]
fn g1_autoincrement_sqlite_spelling() {
    let mut c = open();
    // llmRx 写死的是 AUTOINCREMENT
    let r = c.execute(
        "CREATE TABLE t (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL
        )",
    );
    assert!(r.is_ok(), "G1 FAIL: AUTOINCREMENT 建表被拒: {:?}", r.err());

    c.execute("INSERT INTO t (name) VALUES ('a')").unwrap();
    c.execute("INSERT INTO t (name) VALUES ('b')").unwrap();
    let r = c.execute("SELECT id, name FROM t ORDER BY id").unwrap();
    assert_eq!(r.rows.len(), 2);
    assert_eq!(r.rows[0][0], Value::Int64(1));
    assert_eq!(r.rows[1][0], Value::Int64(2));
    println!("G1 PASS: AUTOINCREMENT 拼写可建表，id 自增 1,2");
}

/// G2：NOT NULL DEFAULT
/// - G2a 显式 NULL 被 NOT NULL 拒绝（真强制，非静默 NULL）← 闸门本体
/// - G2b 省略列吃到 DEFAULT ← llmRx 150 处 NOT NULL DEFAULT 的填充语义
#[test]
fn g2_not_null_default_enforced() {
    let mut c = open();
    c.execute(
        "CREATE TABLE t (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            role INTEGER NOT NULL DEFAULT 0,
            status INTEGER NOT NULL DEFAULT 1,
            name TEXT NOT NULL
        )",
    )
    .unwrap();

    // G2a：显式 NULL 必须被拒绝
    let r = c.execute("INSERT INTO t (role, name) VALUES (NULL, 'b')");
    assert!(r.is_err(), "G2a FAIL: 显式 NULL 未被 NOT NULL 拒绝");
    let msg = r.unwrap_err().to_string();
    assert!(
        msg.to_lowercase().contains("not null") || msg.to_lowercase().contains("constraint"),
        "G2a FAIL: 错误信息不像约束冲突: {}",
        msg
    );
    println!("G2a PASS: 显式 NULL 被 NOT NULL 拒绝");

    // G2b：省略列应吃到 DEFAULT（单独路径，避免 G2a 失败插入的残留影响）
    let mut c2 = open();
    c2.execute(
        "CREATE TABLE t (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            role INTEGER NOT NULL DEFAULT 0,
            status INTEGER NOT NULL DEFAULT 1,
            name TEXT NOT NULL
        )",
    )
    .unwrap();
    let r = c2.execute("INSERT INTO t (name) VALUES ('a')");
    match r {
        Ok(_) => {
            let q = c2.execute("SELECT role, status FROM t").unwrap();
            let role = &q.rows[0][0];
            let status = &q.rows[0][1];
            let role_ok = matches!(role, Value::Int64(0) | Value::Int32(0));
            let status_ok = matches!(status, Value::Int64(1) | Value::Int32(1));
            assert!(
                role_ok && status_ok,
                "G2b FAIL: DEFAULT 未填入，got role={:?} status={:?}",
                role,
                status
            );
            println!("G2b PASS: 省略列吃到 DEFAULT (role=0, status=1)");
        }
        Err(e) => {
            panic!(
                "G2b FAIL: INSERT 省略列失败（DEFAULT 未填充）—— llmRx 150 处 \
                 `NOT NULL DEFAULT` 依赖此语义: {}",
                e
            );
        }
    }
}

/// G3：UNIQUE 真拒绝（二次插入同值报错）
#[test]
fn g3_unique_rejects_duplicate() {
    let mut c = open();
    c.execute(
        "CREATE TABLE t (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            code TEXT UNIQUE NOT NULL
        )",
    )
    .unwrap();

    c.execute("INSERT INTO t (code) VALUES ('k1')").unwrap();
    let r = c.execute("INSERT INTO t (code) VALUES ('k1')");
    assert!(r.is_err(), "G3 FAIL: UNIQUE 未拒绝重复 code");
    let msg = r.unwrap_err().to_string();
    assert!(
        msg.to_lowercase().contains("unique") || msg.to_lowercase().contains("constraint"),
        "G3 FAIL: 错误信息不像 UNIQUE 冲突: {}",
        msg
    );

    // 不同 code 可插入
    c.execute("INSERT INTO t (code) VALUES ('k2')").unwrap();
    let q = c.execute("SELECT COUNT(*) FROM t").unwrap();
    assert_eq!(q.rows[0][0], Value::Int64(2));
    println!("G3 PASS: UNIQUE 真拒绝重复值");
}

/// G4：ON CONFLICT DO UPDATE / DO NOTHING
#[test]
fn g4_on_conflict_semantics() {
    let mut c = open();
    c.execute("CREATE TABLE t (id INT PRIMARY KEY, name TEXT, score INT)").unwrap();
    c.execute("INSERT INTO t VALUES (1, 'alice', 100)").unwrap();

    // DO UPDATE
    c.execute(
        "INSERT INTO t VALUES (1, 'alice_upd', 200)
         ON CONFLICT (id) DO UPDATE SET name = excluded.name, score = excluded.score",
    )
    .expect("G4 FAIL: ON CONFLICT DO UPDATE 执行失败");
    let q = c.execute("SELECT name, score FROM t WHERE id = 1").unwrap();
    assert_eq!(q.rows[0][0], Value::Varchar("alice_upd".into()));
    assert_eq!(q.rows[0][1], Value::Int64(200));
    let q = c.execute("SELECT COUNT(*) FROM t").unwrap();
    assert_eq!(q.rows[0][0], Value::Int64(1));

    // DO NOTHING
    c.execute(
        "INSERT INTO t VALUES (1, 'bob', 300) ON CONFLICT (id) DO NOTHING",
    )
    .expect("G4 FAIL: ON CONFLICT DO NOTHING 执行失败");
    let q = c.execute("SELECT name FROM t WHERE id = 1").unwrap();
    assert_eq!(q.rows[0][0], Value::Varchar("alice_upd".into()));
    let q = c.execute("SELECT COUNT(*) FROM t").unwrap();
    assert_eq!(q.rows[0][0], Value::Int64(1));
    println!("G4 PASS: ON CONFLICT DO UPDATE / DO NOTHING 语义正确");
}

/// G5：RETURNING id 返回自增值（llmRx 核销路径的替代契约）
#[test]
fn g5_returning_id() {
    let mut c = open();
    c.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL)",
    )
    .unwrap();

    let r = c
        .execute("INSERT INTO t (name) VALUES ('alice') RETURNING id")
        .expect("G5 FAIL: RETURNING id 执行失败");
    assert_eq!(r.rows.len(), 1, "G5 FAIL: RETURNING 未返回行");
    let id = &r.rows[0][0];
    assert!(
        matches!(id, Value::Int64(1) | Value::Int32(1)),
        "G5 FAIL: RETURNING id 值异常: {:?}",
        id
    );

    let r = c
        .execute("INSERT INTO t (name) VALUES ('bob') RETURNING id, name")
        .unwrap();
    assert_eq!(r.rows.len(), 1);
    assert!(matches!(r.rows[0][0], Value::Int64(2) | Value::Int32(2)));
    assert_eq!(r.rows[0][1], Value::Varchar("bob".into()));
    println!("G5 PASS: RETURNING id 返回正确自增值");
}

/// G6：ALTER TABLE ADD COLUMN 幂等
///
/// 契约：首次 ADD 成功；重复 ADD 同名列不得静默写坏 catalog，
/// 须报错或等价 no-op（llmRx `addColumnIfMissing` 先查再改，但后端不能静默脏写）。
#[test]
fn g6_alter_add_column_idempotent() {
    let mut c = open();
    c.execute("CREATE TABLE t (id INT PRIMARY KEY, name TEXT)").unwrap();

    c.execute("ALTER TABLE t ADD COLUMN extra INT NOT NULL DEFAULT 0")
        .expect("G6 FAIL: 首次 ADD COLUMN 失败");

    // 读回列存在且可写
    c.execute("INSERT INTO t (id, name, extra) VALUES (1, 'a', 7)").unwrap();
    let q = c.execute("SELECT extra FROM t WHERE id = 1").unwrap();
    assert_eq!(q.rows[0][0], Value::Int64(7));

    // 重复 ADD 同名列
    let r = c.execute("ALTER TABLE t ADD COLUMN extra INT NOT NULL DEFAULT 0");
    match r {
        Ok(_) => {
            // 成功则必须是 no-op：原值仍在、列不重复损坏
            let q = c.execute("SELECT extra FROM t WHERE id = 1").unwrap();
            assert_eq!(
                q.rows[0][0],
                Value::Int64(7),
                "G6 FAIL: 重复 ADD 后原值被写坏"
            );
            println!("G6 PASS: 重复 ADD COLUMN 为 no-op，数据完好");
        }
        Err(e) => {
            // 报错也可接受（幂等由上层 addColumnIfMissing 保证），但须是明确错误
            let msg = e.to_string().to_lowercase();
            assert!(
                msg.contains("exist") || msg.contains("duplicate") || msg.contains("already"),
                "G6 FAIL: 重复 ADD 报错语义不清: {}",
                e
            );
            println!("G6 PASS: 重复 ADD COLUMN 明确报错: {}", e);
        }
    }
}

/// G7：FK + CHECK 行为记录（Phase 0 不修，只钉死现状 + 补偿依据）
#[test]
fn g7_fk_check_behavior_recorded() {
    let mut c = open();

    // ---- CHECK：列级 CHECK 应拒绝非法值 ----
    c.execute("CREATE TABLE chk (id INT PRIMARY KEY, id2 INT CHECK (id2 = 1))")
        .expect("G7: CHECK 建表失败");
    let ok = c.execute("INSERT INTO chk VALUES (1, 1)");
    let bad = c.execute("INSERT INTO chk VALUES (2, 2)");
    let check_enforced = ok.is_ok() && bad.is_err();
    if check_enforced {
        println!("G7-CHECK: ENFORCED（列级 CHECK 拒绝非法值）");
    } else {
        println!(
            "G7-CHECK: SILENT（ok={:?} bad={:?}）→ 补偿：应用层校验",
            ok.map(|_| "ok"),
            bad.map(|_| "ok")
        );
    }

    // ---- FK：建表 + 跨表引用 ----
    let create_fk = c.execute(
        "CREATE TABLE parent (id INT PRIMARY KEY);
         CREATE TABLE child (
            id INT PRIMARY KEY,
            pid INT,
            FOREIGN KEY (pid) REFERENCES parent(id)
         )",
    );
    // 分开执行（EngramDB 可能不支持多语句）
    let _ = create_fk;
    let _ = c.execute("CREATE TABLE parent (id INT PRIMARY KEY)");
    let r = c.execute(
        "CREATE TABLE child (
            id INT PRIMARY KEY,
            pid INT,
            FOREIGN KEY (pid) REFERENCES parent(id)
         )",
    );
    assert!(r.is_ok(), "G7: FK 建表被拒（应至少解析成功）: {:?}", r.err());

    c.execute("INSERT INTO parent VALUES (1)").unwrap();
    // 孤儿行：pid=99 在 parent 中不存在
    let orphan = c.execute("INSERT INTO child VALUES (1, 99)");
    let fk_enforced = orphan.is_err();
    if fk_enforced {
        println!("G7-FK: ENFORCED（孤儿引用被拒）: {}", orphan.unwrap_err());
    } else {
        // 插入成功 = 静默失效 → 记录补偿依据
        let q = c.execute("SELECT pid FROM child WHERE id = 1").unwrap();
        println!(
            "G7-FK: SILENT（孤儿 pid={} 入库）→ 补偿：应用层校验 + 不得依赖 ON DELETE CASCADE",
            match &q.rows[0][0] {
                Value::Int64(i) => *i,
                Value::Int32(i) => *i as i64,
                _ => -999,
            }
        );
    }

    // Phase 0 要求：至少把行为钉死，不求修复
    println!("G7 PASS: FK/CHECK 行为已记录（补偿方案 = 应用层校验，不依赖引擎约束）");
}
