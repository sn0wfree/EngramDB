//! 差分正确性测试：以 SQLite 为 oracle 验证 EngramDB SQL 语义一致性
//!
//! 同一批 SQL 在 EngramDB 与 rusqlite(SQLite) 上执行，逐行比对结果。
//! 数值比较做规范化（整数/浮点等值视为相等），规避 SUM 返回类型的口径差异。
//!
//! 运行：`cargo test --release --test differential_sqlite -- --nocapture`
//!
//! 注：仅覆盖两边语义应当一致的保守 SQL 子集。
//! 已知差异（不计入失败）：LIMIT 非字面量（如 LIMIT 5+1）EngramDB 报解析
//! 错误（v0.22.1 起，见 docs/assessment-v0.22.md）。
//! v0.22.4 起浮点除零已对齐 SQLite（返回 NULL），纳入差分覆盖。

use engramdb::{Connection, Value};

// ---------- 结果规范化 ----------

fn eng_cell(v: &Value) -> String {
    match v {
        Value::Null => "NULL".into(),
        Value::Boolean(b) => if *b { "1" } else { "0" }.into(),
        Value::Int16(i) => i.to_string(),
        Value::Int32(i) => i.to_string(),
        Value::Int64(i) => i.to_string(),
        Value::Float32(f) => format!("{:.6}", f),
        Value::Float64(f) => format!("{:.6}", f),
        Value::Varchar(s) | Value::Enum(s) => s.clone(),
        other => format!("<{:?}>", other),
    }
}

fn lite_cell(v: &rusqlite::types::Value) -> String {
    use rusqlite::types::Value as RV;
    match v {
        RV::Null => "NULL".into(),
        RV::Integer(i) => i.to_string(),
        RV::Real(f) => format!("{:.6}", f),
        RV::Text(s) => s.clone(),
        RV::Blob(_) => "<blob>".into(),
    }
}

/// 数值规范化：两边都解析为 f64，整数等值视为相等（SUM float64 vs SQLite int）
fn norm(cell: &str) -> String {
    if let Ok(f) = cell.parse::<f64>() {
        if f.fract() == 0.0 && f.abs() < 1e15 {
            return format!("{}", f as i64);
        }
        return format!("{:.6}", f);
    }
    cell.to_string()
}

fn eng_rows(conn: &mut Connection, sql: &str) -> Vec<Vec<String>> {
    let r = conn
        .execute(sql)
        .unwrap_or_else(|e| panic!("EngramDB 执行失败: {}\n{}", sql, e));
    r.rows
        .iter()
        .map(|row| row.iter().map(eng_cell).map(|c| norm(&c)).collect())
        .collect()
}

fn lite_rows(conn: &rusqlite::Connection, sql: &str) -> Vec<Vec<String>> {
    let mut stmt = conn.prepare(sql).unwrap_or_else(|e| panic!("SQLite 执行失败: {}\n{}", sql, e));
    let n = stmt.column_count();
    let rows = stmt
        .query_map([], |row| {
            let mut out = Vec::with_capacity(n);
            for i in 0..n {
                let v: rusqlite::types::Value = row.get(i)?;
                out.push(norm(&lite_cell(&v)));
            }
            Ok(out)
        })
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    rows
}

/// 双端执行并断言一致
fn assert_same(name: &str, eng: &mut Connection, lite: &rusqlite::Connection, sql: &str) {
    let e = eng_rows(eng, sql);
    let s = lite_rows(lite, sql);
    assert_eq!(
        e, s,
        "差分不一致 [{}]\n  SQL: {}\n  EngramDB: {:?}\n  SQLite:   {:?}",
        name, sql, e, s
    );
}

// ---------- 测试数据（两端同步构建） ----------

fn setup(eng: &mut Connection, lite: &rusqlite::Connection) {
    eng.execute("CREATE TABLE users (id INT64 PRIMARY KEY, name VARCHAR, age INT, city VARCHAR)")
        .unwrap();
    eng.execute("CREATE TABLE orders (oid INT64 PRIMARY KEY, uid INT, amount INT)")
        .unwrap();
    lite.execute_batch(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, age INTEGER, city TEXT);
         CREATE TABLE orders (oid INTEGER PRIMARY KEY, uid INTEGER, amount INTEGER);",
    )
    .unwrap();

    let cities = ["北京", "上海", "深圳", "杭州"];
    let mut user_sql = String::from("INSERT INTO users VALUES ");
    let mut order_sql = String::from("INSERT INTO orders VALUES ");
    for i in 0..60 {
        let age = 20 + (i * 7) % 40; // 20-59
        let city = cities[i % 4];
        user_sql.push_str(&format!("({}, 'user_{:02}', {}, '{}'),", i, i, age, city));
        if i < 40 {
            let amount = (i * 13) % 500; // 0-499
            order_sql.push_str(&format!("({}, {}, {}),", 1000 + i, i % 30, amount));
        }
    }
    user_sql.pop();
    order_sql.pop();
    eng.execute(&user_sql).unwrap();
    eng.execute(&order_sql).unwrap();
    lite.execute_batch(&user_sql).unwrap();
    lite.execute_batch(&order_sql).unwrap();
}

fn fresh() -> (Connection, rusqlite::Connection) {
    let mut eng = Connection::open(":memory:").unwrap();
    let lite = rusqlite::Connection::open_in_memory().unwrap();
    setup(&mut eng, &lite);
    (eng, lite)
}

// ---------- 基础查询 ----------

#[test]
fn diff_projection_and_filter() {
    let (mut eng, lite) = fresh();
    assert_same("select star", &mut eng, &lite, "SELECT * FROM users WHERE id = 5");
    assert_same(
        "projection subset",
        &mut eng,
        &lite,
        "SELECT name, age FROM users WHERE id = 10",
    );
    assert_same(
        "projection reorder",
        &mut eng,
        &lite,
        "SELECT age, name FROM users WHERE id = 10",
    );
    assert_same(
        "where gt",
        &mut eng,
        &lite,
        "SELECT id FROM users WHERE age > 50 ORDER BY id",
    );
    assert_same(
        "where and or",
        &mut eng,
        &lite,
        "SELECT id FROM users WHERE age >= 40 AND age <= 45 OR id = 1 ORDER BY id",
    );
    assert_same(
        "where in list",
        &mut eng,
        &lite,
        "SELECT id FROM users WHERE id IN (3, 7, 15, 44) ORDER BY id",
    );
    assert_same(
        "where between",
        &mut eng,
        &lite,
        "SELECT id, age FROM users WHERE age BETWEEN 30 AND 35 ORDER BY id",
    );
    assert_same(
        "where neq",
        &mut eng,
        &lite,
        "SELECT id FROM users WHERE city != '北京' AND id < 8 ORDER BY id",
    );
    // v0.22.3 回归：WHERE over 派生表——optimizer 兜底臂此前会把外层谓词静默丢弃
    assert_same(
        "where over derived table",
        &mut eng,
        &lite,
        "SELECT id, name FROM (SELECT id, name, age FROM users) sub WHERE id < 5 ORDER BY id",
    );
    assert_same(
        "where over derived with limit",
        &mut eng,
        &lite,
        "SELECT id FROM (SELECT id, age FROM users WHERE age > 30) sub WHERE id >= 10 ORDER BY id LIMIT 4",
    );
}

#[test]
fn diff_order_limit() {
    let (mut eng, lite) = fresh();
    assert_same(
        "order desc limit",
        &mut eng,
        &lite,
        "SELECT id, age FROM users ORDER BY age DESC LIMIT 5",
    );
    assert_same(
        "order asc limit",
        &mut eng,
        &lite,
        "SELECT id, age FROM users ORDER BY age ASC LIMIT 5",
    );
    assert_same(
        "order by two keys",
        &mut eng,
        &lite,
        "SELECT id, city, age FROM users ORDER BY city, age DESC LIMIT 8",
    );
    assert_same(
        "limit larger than rows",
        &mut eng,
        &lite,
        "SELECT id FROM users WHERE age > 55 ORDER BY id LIMIT 100",
    );
    // v0.22.1：OFFSET 支持（此前被静默忽略，返回错误行集）
    assert_same(
        "limit offset",
        &mut eng,
        &lite,
        "SELECT id FROM users ORDER BY id LIMIT 5 OFFSET 3",
    );
    assert_same(
        "offset beyond rows",
        &mut eng,
        &lite,
        "SELECT id FROM users ORDER BY id LIMIT 10 OFFSET 58",
    );
    // 注：EngramDB 另支持不带 LIMIT 的裸 OFFSET（SQLite 语法不允许，无法差分，此处用大 LIMIT 等价覆盖）
    assert_same(
        "offset effectively no limit",
        &mut eng,
        &lite,
        "SELECT id FROM users ORDER BY id LIMIT 1000 OFFSET 55",
    );
    assert_same(
        "offset with where",
        &mut eng,
        &lite,
        "SELECT id, age FROM users WHERE age > 30 ORDER BY age, id LIMIT 4 OFFSET 2",
    );
}

#[test]
fn diff_aggregates() {
    let (mut eng, lite) = fresh();
    assert_same("count star", &mut eng, &lite, "SELECT COUNT(*) FROM users");
    assert_same(
        "count where",
        &mut eng,
        &lite,
        "SELECT COUNT(*) FROM users WHERE age > 40",
    );
    assert_same("sum", &mut eng, &lite, "SELECT SUM(amount) FROM orders");
    assert_same("min max", &mut eng, &lite, "SELECT MIN(age), MAX(age) FROM users");
    assert_same("avg", &mut eng, &lite, "SELECT AVG(age) FROM users");
    assert_same(
        "group by",
        &mut eng,
        &lite,
        "SELECT city, COUNT(*) FROM users GROUP BY city ORDER BY city",
    );
    assert_same(
        "group by agg",
        &mut eng,
        &lite,
        "SELECT uid, SUM(amount), COUNT(*) FROM orders GROUP BY uid ORDER BY uid",
    );
    assert_same(
        "group by having",
        &mut eng,
        &lite,
        "SELECT city, COUNT(*) FROM users GROUP BY city HAVING COUNT(*) > 10 ORDER BY city",
    );
    assert_same(
        "empty agg",
        &mut eng,
        &lite,
        "SELECT COUNT(*), SUM(amount) FROM orders WHERE amount > 10000",
    );
}

#[test]
fn diff_joins() {
    let (mut eng, lite) = fresh();
    assert_same(
        "inner join",
        &mut eng,
        &lite,
        "SELECT u.name, o.amount FROM users u JOIN orders o ON u.id = o.uid WHERE o.uid = 5 ORDER BY o.amount",
    );
    assert_same(
        "inner join agg",
        &mut eng,
        &lite,
        "SELECT u.name, SUM(o.amount) FROM users u JOIN orders o ON u.id = o.uid GROUP BY u.name ORDER BY u.name",
    );
    assert_same(
        "left join null fill",
        &mut eng,
        &lite,
        "SELECT u.id, o.amount FROM users u LEFT JOIN orders o ON u.id = o.uid WHERE u.id < 6 ORDER BY u.id, o.amount",
    );
}

#[test]
fn diff_subqueries() {
    let (mut eng, lite) = fresh();
    assert_same(
        "scalar subquery",
        &mut eng,
        &lite,
        "SELECT name, (SELECT MAX(amount) FROM orders) FROM users WHERE id = 3",
    );
    assert_same(
        "in subquery",
        &mut eng,
        &lite,
        "SELECT id, name FROM users WHERE id IN (SELECT uid FROM orders WHERE amount > 450) ORDER BY id",
    );
    assert_same("exists subquery", &mut eng, &lite,
        "SELECT id FROM users u WHERE EXISTS (SELECT 1 FROM orders o WHERE o.uid = u.id AND o.amount > 480) ORDER BY id");
}

#[test]
fn diff_null_semantics() {
    let (mut eng, lite) = fresh();
    // 插入含 NULL 的行
    eng.execute("INSERT INTO users VALUES (100, 'nullage', NULL, '北京')").unwrap();
    lite.execute_batch("INSERT INTO users VALUES (100, 'nullage', NULL, '北京')")
        .unwrap();

    assert_same("is null", &mut eng, &lite, "SELECT id FROM users WHERE age IS NULL");
    assert_same(
        "is not null count",
        &mut eng,
        &lite,
        "SELECT COUNT(*) FROM users WHERE age IS NOT NULL",
    );
    assert_same(
        "null comparison empty",
        &mut eng,
        &lite,
        "SELECT id FROM users WHERE age = NULL",
    );
    assert_same(
        "null in agg skipped",
        &mut eng,
        &lite,
        "SELECT COUNT(age), SUM(age) FROM users WHERE id >= 95",
    );
    assert_same(
        "null in order",
        &mut eng,
        &lite,
        "SELECT id, age FROM users WHERE id >= 99 ORDER BY age, id",
    );
}

#[test]
fn diff_dml_roundtrip() {
    let (mut eng, lite) = fresh();

    eng.execute("UPDATE users SET age = 99 WHERE city = '深圳'").unwrap();
    lite.execute_batch("UPDATE users SET age = 99 WHERE city = '深圳'").unwrap();
    assert_same(
        "after update",
        &mut eng,
        &lite,
        "SELECT id, age FROM users WHERE age = 99 ORDER BY id",
    );

    eng.execute("DELETE FROM orders WHERE amount < 100").unwrap();
    lite.execute_batch("DELETE FROM orders WHERE amount < 100").unwrap();
    assert_same("after delete count", &mut eng, &lite, "SELECT COUNT(*) FROM orders");
    assert_same(
        "after delete rows",
        &mut eng,
        &lite,
        "SELECT oid, amount FROM orders WHERE amount >= 100 ORDER BY oid",
    );
}

#[test]
fn diff_expression_arith() {
    let (mut eng, lite) = fresh();
    assert_same(
        "mul add",
        &mut eng,
        &lite,
        "SELECT id, age * 2 + 1 FROM users WHERE id < 5 ORDER BY id",
    );
    assert_same("agg expr", &mut eng, &lite, "SELECT SUM(amount * 2) FROM orders");
    assert_same(
        "predicate expr",
        &mut eng,
        &lite,
        "SELECT id FROM orders WHERE amount * 2 >= 900 ORDER BY oid",
    );
    // v0.22.4：浮点除零 → NULL（对齐 SQLite；oid=1000 的 amount=0）
    assert_same(
        "float div by zero",
        &mut eng,
        &lite,
        "SELECT amount * 1.0 / amount FROM orders WHERE oid = 1000",
    );
}
