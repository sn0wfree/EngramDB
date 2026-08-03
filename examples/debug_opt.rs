use engramdb::Connection;
use engramdb::sql::optimizer;
use engramdb::sql::planner;
use engramdb::sql::parser;

fn main() {
    let mut conn = Connection::open("/tmp/debug_opt.db").unwrap();
    conn.execute("CREATE TABLE t1 (id INT, category INT, value DOUBLE, name VARCHAR);").unwrap();
    conn.execute("INSERT INTO t1 VALUES (1, 10, 100.0, 'a');").unwrap();
    
    // 手动走一遍流程
    let ast = parser::parse("SELECT * FROM t1;").unwrap();
    let plan = planner::plan(ast, &conn.db).unwrap();
    println!("Planner plan: {:#?}", plan);
    
    // 单独测试每个规则
    let r1 = optimizer::constant_folding(plan.clone()).unwrap();
    println!("\nAfter constant_folding: {:#?}", r1);
    
    let r2 = optimizer::predicate_pushdown(r1.clone()).unwrap();
    println!("\nAfter predicate_pushdown: {:#?}", r2);
    
    let r3 = optimizer::projection_pushdown(r2.clone()).unwrap();
    println!("\nAfter projection_pushdown: {:#?}", r3);
    
    conn.close().unwrap();
}
