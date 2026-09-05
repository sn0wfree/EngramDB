//! EngramDB v0.22.0 新类型示例
//!
//! 演示 v0.22.0 新增的 6 种数据类型：DATE, TIME, JSONB, ARRAY, UUID, ENUM
//! 以及相关的类型转换函数。
//!
//! 运行：cargo run --example v022_new_types

use engramdb::{Connection, Value};

fn main() {
    // 使用 :memory: 内存数据库
    let mut conn = Connection::open(":memory:").expect("Failed to open database");

    println!("=== EngramDB v0.22.0 新类型示例 ===\n");

    // =========================================================================
    // 1. DATE / TIME / TIMESTAMP 类型
    // =========================================================================
    println!("1. DATE / TIME / TIMESTAMP 类型");
    conn.execute(
        "CREATE TABLE events (
            id INT PRIMARY KEY,
            name VARCHAR(100),
            event_date DATE,
            event_time TIME,
            created_at TIMESTAMP
        )",
    )
    .expect("Failed to create table");

    // DATE：自 1970-01-01 起的天数（通过 CAST 转换）
    conn.execute(
        "INSERT INTO events VALUES \
         (1, '会议', CAST('2024-01-15' AS DATE), CAST('14:30:00' AS TIME), 1705271400000), \
         (2, '聚会', CAST('2024-02-20' AS DATE), CAST('18:00:00' AS TIME), 1708384800000)",
    )
    .expect("Failed to insert");
    println!("   插入 2 条事件记录\n");

    // 查询
    let result = conn.execute("SELECT id, name, event_date, event_time FROM events").unwrap();
    for row in &result.rows {
        println!("   id={}, name={}, date={}, time={}", row[0], row[1], row[2], row[3]);
    }
    println!();

    // =========================================================================
    // 2. UUID 和 ENUM 类型
    // =========================================================================
    println!("2. UUID 和 ENUM 类型");
    conn.execute(
        "CREATE TABLE devices (
            id UUID,
            name VARCHAR(100),
            status ENUM('active', 'inactive', 'maintenance')
        )",
    )
    .expect("Failed to create table");

    conn.execute(
        "INSERT INTO devices VALUES \
         (1, CAST('550e8400-e29b-41d4-a716-446655440000' AS UUID), '服务器A', 'active'), \
         (2, CAST('660e8400-e29b-41d4-a716-446655440001' AS UUID), '服务器B', 'maintenance')",
    )
    .expect("Failed to insert");
    println!("   插入 2 台设备\n");

    let result = conn.execute("SELECT id, name, status FROM devices").unwrap();
    for row in &result.rows {
        println!("   id={}, name={}, status={}", row[0], row[1], row[2]);
    }
    println!();

    // =========================================================================
    // 3. JSONB 和 ARRAY 类型
    // =========================================================================
    println!("3. JSONB 和 ARRAY 类型");
    conn.execute(
        "CREATE TABLE products (
            id INT PRIMARY KEY,
            name VARCHAR(100),
            metadata JSONB,
            tags ARRAY(VARCHAR)
        )",
    )
    .expect("Failed to create table");

    conn.execute(
        "INSERT INTO products VALUES \
         (1, '产品A', CAST('{\"price\": 99.9, \"stock\": 100}' AS JSONB), ARRAY('electronics', 'sale')), \
         (2, '产品B', CAST('{\"price\": 199.9, \"stock\": 50}' AS JSONB), ARRAY('electronics'))",
    )
    .expect("Failed to insert");
    println!("   插入 2 个产品\n");

    let result = conn.execute("SELECT id, name, metadata, tags FROM products").unwrap();
    for row in &result.rows {
        println!(
            "   id={}, name={}, metadata={}, tags={}",
            row[0], row[1], row[2], row[3]
        );
    }
    println!();

    // =========================================================================
    // 4. 类型转换函数
    // =========================================================================
    println!("4. 类型转换函数");

    // DATE_TO_STRING：日期转字符串
    let result = conn.execute("SELECT DATE_TO_STRING(event_date) FROM events").unwrap();
    println!("   DATE_TO_STRING: {:?}", result.rows);

    // TIME_TO_STRING：时间转字符串
    let result = conn.execute("SELECT TIME_TO_STRING(event_time) FROM events").unwrap();
    println!("   TIME_TO_STRING: {:?}", result.rows);

    // TIMESTAMP_TO_STRING：时间戳转字符串
    let result = conn.execute("SELECT TIMESTAMP_TO_STRING(created_at) FROM events").unwrap();
    println!("   TIMESTAMP_TO_STRING: {:?}", result.rows);

    // UUID_TO_STRING：UUID 转字符串
    let result = conn.execute("SELECT UUID_TO_STRING(id) FROM devices").unwrap();
    println!("   UUID_TO_STRING: {:?}", result.rows);

    // JSON_TO_JSONB / JSONB_TO_JSON：JSON 与 JSONB 互转
    let result = conn.execute("SELECT JSON_TO_JSONB(metadata) FROM products").unwrap();
    println!("   JSON_TO_JSONB: {:?}", result.rows);
    println!();

    // =========================================================================
    // 5. ALTER TABLE 操作
    // =========================================================================
    println!("5. ALTER TABLE 操作");
    conn.execute("ALTER TABLE events ADD COLUMN location VARCHAR(200)")
        .expect("ADD COLUMN failed");
    conn.execute("ALTER TABLE events RENAME COLUMN location TO venue")
        .expect("RENAME COLUMN failed");
    conn.execute("ALTER TABLE events ADD COLUMN attendee_count INT")
        .expect("ADD COLUMN failed");
    println!("   ALTER TABLE 操作成功（ADD COLUMN, RENAME COLUMN）\n");

    // =========================================================================
    // 6. VIEW 和递归 CTE
    // =========================================================================
    println!("6. VIEW 和递归 CTE");
    conn.execute(
        "CREATE VIEW active_devices AS \
         SELECT id, name FROM devices WHERE status = 'active'",
    )
    .expect("CREATE VIEW failed");
    println!("   创建 VIEW: active_devices");

    let result = conn.execute("SELECT * FROM active_devices").unwrap();
    for row in &result.rows {
        println!("   id={}, name={}", row[0], row[1]);
    }
    println!();

    // 递归 CTE：组织架构
    conn.execute(
        "CREATE TABLE employees (
            id INT PRIMARY KEY,
            name VARCHAR(100),
            manager_id INT
        )",
    )
    .expect("Failed to create employees table");
    conn.execute(
        "INSERT INTO employees VALUES \
         (1, 'CEO', NULL), \
         (2, 'CTO', 1), \
         (3, 'CFO', 1), \
         (4, '工程师A', 2), \
         (5, '工程师B', 2)",
    )
    .expect("Failed to insert employees");

    let result = conn
        .execute(
            "WITH RECURSIVE org_tree AS (
            SELECT id, name, manager_id, 1 AS level
            FROM employees WHERE manager_id IS NULL
            UNION ALL
            SELECT e.id, e.name, e.manager_id, t.level + 1
            FROM employees e JOIN org_tree t ON e.manager_id = t.id
        )
        SELECT name, level FROM org_tree",
        )
        .unwrap();
    println!("   递归 CTE 结果：");
    for row in &result.rows {
        println!("   {} (level {})", row[0], row[1]);
    }
    println!();

    // =========================================================================
    // 7. CHECK 约束
    // =========================================================================
    println!("7. CHECK 约束");
    conn.execute(
        "CREATE TABLE products_with_check (
            id INT PRIMARY KEY,
            name VARCHAR(100),
            price DOUBLE CHECK (price > 0),
            stock INT CHECK (stock >= 0)
        )",
    )
    .expect("Failed to create table");

    // 正常插入
    conn.execute("INSERT INTO products_with_check VALUES (1, '商品A', 99.9, 100)")
        .expect("Insert failed");
    println!("   正常插入成功：price=99.9, stock=100");

    // 违反 CHECK 约束
    let result = conn.execute("INSERT INTO products_with_check VALUES (2, '商品B', -1.0, 50)");
    match result {
        Ok(_) => println!("   ✗ 违反 CHECK 约束但插入成功（不应该发生）"),
        Err(e) => println!("   ✓ 违反 CHECK 约束被拒绝：{}", e),
    }
    println!();

    println!("=== 示例完成 ===");
}
