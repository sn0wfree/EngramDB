//! HybridDB CLI 入口

use std::io::{self, BufRead, Write};

use hybriddb::Connection;

fn main() {
    env_logger::init();

    let args: Vec<String> = std::env::args().collect();

    if args.len() < 2 {
        println!("Usage: hybriddb <database_file>");
        println!("  or: hybriddb <database_file> \"<SQL statement>\"");
        return;
    }

    let db_path = &args[1];

    if args.len() >= 3 {
        // 单条命令模式
        let sql = &args[2..].join(" ");
        if let Err(e) = execute_single(db_path, sql) {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    } else {
        // 交互模式
        if let Err(e) = run_interactive(db_path) {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    }
}

fn execute_single(path: &str, sql: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mut conn = Connection::open(path)?;
    let result = conn.execute(sql)?;
    print_result(&result);
    conn.close()?;
    Ok(())
}

fn run_interactive(path: &str) -> Result<(), Box<dyn std::error::Error>> {
    println!("HybridDB v0.1.0 — 输入 SQL 语句，输入 .exit 退出");
    println!("数据库: {}", path);
    println!();

    let mut conn = Connection::open(path)?;
    let stdin = io::stdin();
    let mut buffer = String::new();

    loop {
        print!("hybriddb> ");
        io::stdout().flush()?;

        buffer.clear();
        if stdin.lock().read_line(&mut buffer)? == 0 {
            break;
        }

        let input = buffer.trim();
        if input.is_empty() {
            continue;
        }

        if input == ".exit" || input == ".quit" {
            break;
        }

        match conn.execute(input) {
            Ok(result) => print_result(&result),
            Err(e) => println!("错误: {}", e),
        }
    }

    conn.close()?;
    println!("再见！");
    Ok(())
}

fn print_result(result: &hybriddb::QueryResult) {
    if result.columns.is_empty() {
        println!("OK ({} 行受影响)", result.rows_affected);
        return;
    }

    // 打印表头
    let col_widths: Vec<usize> = result.columns
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let mut max_width = name.len();
            for row in &result.rows {
                if i < row.len() {
                    let val_str = format!("{}", row[i]);
                    max_width = max_width.max(val_str.len());
                }
            }
            max_width.max(8)
        })
        .collect();

    // 分隔线
    let separator: String = col_widths
        .iter()
        .map(|w| format!("+{}", "-".repeat(w + 2)))
        .collect::<Vec<_>>()
        .join("") + "+";

    println!("{}", separator);

    // 表头
    print!("|");
    for (i, col) in result.columns.iter().enumerate() {
        print!(" {:>width$} |", col, width = col_widths[i]);
    }
    println!();
    println!("{}", separator);

    // 数据行
    for row in &result.rows {
        print!("|");
        for (i, val) in row.iter().enumerate() {
            let val_str = format!("{}", val);
            print!(" {:>width$} |", val_str, width = col_widths[i]);
        }
        println!();
    }

    println!("{}", separator);
    println!("{} 行", result.rows.len());
}
