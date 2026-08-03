use engramdb::sql::parser;

fn main() {
    let queries = vec![
        "SELECT COUNT(*) FROM t1;",
        "SELECT COUNT(id) FROM t1;",
        "SELECT SUM(value) FROM t1;",
        "SELECT id, COUNT(*) FROM t1 GROUP BY id;",
    ];
    
    for q in queries {
        println!("\n=== {} ===", q);
        match parser::parse(q) {
            Ok(ast) => println!("AST: {:#?}", ast),
            Err(e) => println!("Parse error: {}", e),
        }
    }
}
