//! SQL Parser 验证脚本
//! 使用 sqlparser-rs 作为解析后端，验证 Phase 1 成果

use hybriddb::sql::parser::parse;
use hybriddb::sql::ast::*;

fn main() {
    let tests = vec![
        ("CREATE TABLE", "CREATE TABLE users (id INT PRIMARY KEY, name VARCHAR NOT NULL, age INT, score DOUBLE)"),
        ("INSERT", "INSERT INTO users VALUES (1, 'alice', 25, 95.5)"),
        ("SELECT simple", "SELECT id, name FROM users WHERE age > 18 ORDER BY name LIMIT 10"),
        ("SELECT *", "SELECT * FROM users"),
        ("SELECT alias", "SELECT id AS user_id, name AS user_name FROM users u"),
        ("COUNT(*)", "SELECT COUNT(*) FROM users"),
        ("GROUP BY + HAVING", "SELECT age, COUNT(*) FROM users GROUP BY age HAVING COUNT(*) > 5"),
        ("BEGIN", "BEGIN"),
        ("COMMIT", "COMMIT"),
        ("ROLLBACK", "ROLLBACK"),
        ("START TRANSACTION", "START TRANSACTION"),
        ("AND/OR", "SELECT * FROM users WHERE age > 18 AND score > 80 OR name = 'test'"),
        ("IS NOT NULL", "SELECT * FROM users WHERE name IS NOT NULL"),
        ("IN list", "SELECT * FROM users WHERE age IN (18, 20, 25)"),
        ("BETWEEN", "SELECT * FROM users WHERE age BETWEEN 18 AND 30"),
        ("LIKE", "SELECT * FROM users WHERE name LIKE 'al%'"),
        ("CASE WHEN", "SELECT CASE WHEN score >= 90 THEN 'A' WHEN score >= 80 THEN 'B' ELSE 'C' END FROM users"),
        ("CAST", "SELECT CAST(age AS DOUBLE) FROM users"),
        ("NOT IN", "SELECT * FROM users WHERE age NOT IN (10, 20)"),
        ("IS NULL", "SELECT * FROM users WHERE name IS NULL"),
    ];

    let mut passed = 0;
    let mut failed = 0;

    for (name, sql) in &tests {
        match parse(sql) {
            Ok(stmt) => {
                println!("✓ {:<20} -> {:?}", name, stmt_type(&stmt));
                passed += 1;
            }
            Err(e) => {
                println!("✗ {:<20} -> {}", name, e);
                failed += 1;
            }
        }
    }

    println!("\n结果: {} 通过, {} 失败", passed, failed);
    if failed == 0 {
        println!("Phase 1 (SQL Parser) - 全部通过!");
    }
}

fn stmt_type(stmt: &Statement) -> &'static str {
    match stmt {
        Statement::CreateTable(_) => "CREATE TABLE",
        Statement::Insert(_) => "INSERT",
        Statement::Select(_) => "SELECT",
        Statement::BeginTransaction => "BEGIN",
        Statement::Commit => "COMMIT",
        Statement::Rollback => "ROLLBACK",
    }
}
