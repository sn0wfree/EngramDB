//! HybridDB 基本使用示例
//!
//! 演示建表、插入、查询、条件过滤、LIMIT、事务等核心功能。
//!
//! 运行：cargo run --example basic

use hybriddb::{Connection, Value};

fn main() {
    // 使用 :memory: 内存数据库，避免产生残留文件
    // 如需持久化，改为文件路径如 "example.hdb"
    let mut conn = Connection::open(":memory:").expect("Failed to open database");

    println!("=== HybridDB 基本示例 ===\n");

    // 1. 建表
    println!("1. 创建表 users...");
    conn.execute(
        "CREATE TABLE users (id INT PRIMARY KEY, name VARCHAR, age INT, balance DOUBLE)"
    ).unwrap();
    println!("   表创建成功\n");

    // 2. 批量插入
    println!("2. 插入数据...");
    conn.execute(
        "INSERT INTO users VALUES \
         (1, 'Alice', 30, 1000.5), \
         (2, 'Bob', 25, 500.0), \
         (3, 'Charlie', 35, 2500.75), \
         (4, 'Diana', 28, 800.25), \
         (5, 'Eve', 32, 3000.0)"
    ).unwrap();
    println!("   5 行数据已插入\n");

    // 3. 全表扫描
    println!("3. 查询所有用户:");
    let result = conn.execute("SELECT * FROM users").unwrap();
    for row in &result.rows {
        println!("   id={}, name={}, age={}, balance={}",
            row[0], row[1], row[2], row[3]);
    }
    println!();

    // 4. 条件查询（WHERE）
    println!("4. 查询年龄 > 28 的用户:");
    let result = conn.execute("SELECT name, age FROM users WHERE age > 28").unwrap();
    for row in &result.rows {
        println!("   name={}, age={}", row[0], row[1]);
    }
    println!();

    // 5. 限制行数（LIMIT）
    println!("5. 查询前 3 个用户:");
    let result = conn.execute("SELECT name, balance FROM users LIMIT 3").unwrap();
    for row in &result.rows {
        println!("   name={}, balance={}", row[0], row[1]);
    }
    println!();

    // 6. 聚合查询
    println!("6. 聚合查询:");
    let result = conn.execute("SELECT COUNT(*) FROM users").unwrap();
    println!("   总行数: {}", result.rows[0][0]);
    let result = conn.execute("SELECT SUM(balance) FROM users").unwrap();
    println!("   余额总计: {}", result.rows[0][0]);
    println!();

    // 7. 事务
    println!("7. 事务示例:");
    println!("   开始事务...");
    conn.execute("BEGIN").unwrap();
    println!("   插入新用户 Frank...");
    conn.execute("INSERT INTO users VALUES (6, 'Frank', 40, 5000.0)").unwrap();
    println!("   提交事务...");
    conn.execute("COMMIT").unwrap();
    println!("   事务已提交\n");

    // 8. 验证事务结果
    println!("8. 验证事务结果 (共 6 行):");
    let result = conn.execute("SELECT * FROM users").unwrap();
    println!("   总行数: {}", result.rows.len());
    println!();

    // 9. Prepared Statement 批量写入
    println!("9. Prepared Statement 批量写入:");
    let stmt = conn.prepare("INSERT INTO users VALUES (?, ?, ?, ?)").unwrap();
    let batch = vec![
        vec![Value::Int64(7), Value::Varchar("Grace".into()), Value::Int64(27), Value::Float64(1500.0)],
        vec![Value::Int64(8), Value::Varchar("Henry".into()), Value::Int64(45), Value::Float64(4200.0)],
    ];
    let n = conn.execute_prepared_batch(&stmt, &batch).unwrap();
    println!("   批量插入 {} 行\n", n);

    // 关闭
    conn.close().unwrap();
    println!("=== 完成 ===");
}
