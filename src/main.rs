//! EngramDB CLI 入口
//!
//! 用法：
//!   engramdb <database_file>              # 交互模式（默认开启事务）
//!   engramdb --no-transaction <db>       # 交互模式（关闭事务，高性能模式）
//!   engramdb <database_file> "SQL"        # 单条命令模式
//!   engramdb --no-txn <db> "SQL"          # 单条命令（关闭事务）
//!   engramdb --help                       # 查看帮助

use std::io::{self, BufRead, Write};

use engramdb::{Connection, Config};

fn main() {
    env_logger::init();

    let args: Vec<String> = std::env::args().collect();

    let mut enable_transaction: Option<bool> = None;
    let mut db_path: Option<String> = None;
    let mut sql_stmt: Option<String> = None;
    let mut i = 1; // skip program name

    while i < args.len() {
        let arg = &args[i];
        match arg.as_str() {
            "--help" | "-h" => {
                print_usage();
                return;
            }
            "--no-transaction" | "--no-txn" => {
                enable_transaction = Some(false);
            }
            "--enable-transaction" | "--txn" => {
                enable_transaction = Some(true);
            }
            s if s.starts_with("--") => {
                eprintln!("未知参数: {}", s);
                eprintln!("使用 --help 查看可用参数");
                std::process::exit(1);
            }
            _ => {
                if db_path.is_none() {
                    db_path = Some(arg.clone());
                } else if sql_stmt.is_none() {
                    // 剩余参数拼成 SQL 语句
                    let remaining: Vec<String> = args[i..].to_vec();
                    sql_stmt = Some(remaining.join(" "));
                    break;
                }
            }
        }
        i += 1;
    }

    let db_path = match db_path {
        Some(p) => p,
        None => {
            print_usage();
            std::process::exit(1);
        }
    };

    // 构建配置：默认启用事务，除非显式指定 --no-transaction
    let mut config = Config::default();
    if let Some(enabled) = enable_transaction {
        config.enable_transaction = enabled;
    }
    let txn_status = if config.enable_transaction { "ON" } else { "OFF" };

    if let Some(sql) = sql_stmt {
        // 单条命令模式
        if let Err(e) = execute_single(&db_path, &sql, &config) {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    } else {
        // 交互模式
        if let Err(e) = run_interactive(&db_path, &config, txn_status) {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    }
}

fn print_usage() {
    println!("EngramDB v0.12.0");
    println!("=================");
    println!();
    println!("用法:");
    println!("  engramdb <database_file>              交互模式（默认开启事务）");
    println!("  engramdb --no-transaction <db>         交互模式（关闭事务）");
    println!("  engramdb <database_file> \"<SQL>\"       单条命令执行");
    println!("  engramdb --no-txn <db> \"<SQL>\"         单条命令（关闭事务）");
    println!();
    println!("参数:");
    println!("  --no-transaction, --no-txn    关闭事务支持（高性能模式）");
    println!("  --enable-transaction, --txn   强制开启事务（默认）");
    println!("  --help, -h                    查看帮助");
}

fn execute_single(path: &str, sql: &str, config: &Config) -> Result<(), Box<dyn std::error::Error>> {
    let mut conn = Connection::open_with_config(path, config.clone())?;
    let result = conn.execute(sql)?;
    print_result(&result);
    conn.close()?;
    Ok(())
}

fn run_interactive(path: &str, config: &Config, txn_status: &str) -> Result<(), Box<dyn std::error::Error>> {
    println!("EngramDB v0.12.0 — 输入 SQL 语句，输入 .exit 退出");
    println!("数据库: {}", path);
    println!("事务: {}", txn_status);
    println!();

    let mut conn = Connection::open_with_config(path, config.clone())?;

    let stdin = io::stdin();
    let mut buffer = String::new();

    loop {
        print!("engramdb> ");
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

fn print_result(result: &engramdb::QueryResult) {
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