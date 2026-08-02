//! 基本使用示例
//!
//! 运行：cargo run --example basic

use hybriddb::{Connection, Value};

fn main() {
    // 打开或创建数据库
    let mut conn = Connection::open("example.hdb").expect("Failed to open database");

    println!("=== HybridDB 基本示例 ===\n");

    // 创建表
    println!("1. 创建表 users...");
    conn.execute(
        "CREATE TABLE users (id INT PRIMARY KEY, name VARCHAR, age INT, balance DOUBLE)"
    ).unwrap();
    println!("   ✓ 表创建成功\n");

    // 插入数据
    println!("2. 插入数据...");
    conn.execute(
        "INSERT INTO users VALUES \
         (1, 'Alice', 30, 1000.5), \
         (2, 'Bob', 25, 500.0), \
         (3, 'Charlie', 35, 2500.75), \
         (4, 'Diana', 28, 800.25), \
         (5, 'Eve', 32, 3000.0)"
    ).unwrap();
    println!("   ✓ 5 行数据已插入\n");

    // 查询所有数据
    println!("3. 查询所有用户:");
    let result = conn.execute("SELECT * FROM users").unwrap();
    for row in &result.rows {
        println!("   id={}, name={}, age={}, balance={}",
            row[0], row[1], row[2], row[3]);
    }
    println!();

    // 条件查询
    println!("4. 查询年龄 > 28 的用户:");
    let result = conn.execute("SELECT name, age FROM users WHERE age > 28").unwrap();
    for row in &result.rows {
        println!("   name={}, age={}", row[0], row[1]);
    }
    println!();

    // 限制行数
    println!("5. 查询前 3 个用户:");
    let result = conn.execute("SELECT name, balance FROM users LIMIT 3").unwrap();
    for row in &result.rows {
        println!("   name={}, balance={}", row[0], row[1]);
    }
    println!();

    // 事务
    println!("6. 事务示例:");
    println!("   开始事务...");
    conn.execute("BEGIN").unwrap();
    println!("   插入新用户...");
    conn.execute("INSERT INTO users VALUES (6, 'Frank', 40, 5000.0)").unwrap();
    println!("   提交事务...");
    conn.execute("COMMIT").unwrap();
    println!("   ✓ 事务已提交\n");

    // 验证
    println!("7. 验证事务结果 (共 6 行):");
    let result = conn.execute("SELECT COUNT(*) FROM users").unwrap();
    // 注意：MVP 阶段 COUNT 需通过行数验证
    let result = conn.execute("SELECT * FROM users").unwrap();
    println!("   总行数: {}", result.rows.len());

    // 关闭
    conn.close().unwrap();
    println!("\n=== 完成 ===");
}
