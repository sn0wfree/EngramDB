//! 真实 DB 链路验证：SQL 流式追加 → checkpoint → 读回验证；
//! 对比 压缩开/关 的落盘体积（TokenDelta 运行时分派实际收益）。
use engramdb::common::config::Config;
use engramdb::Connection;

fn main() {
    let dir = "/tmp/engram_db_verify";
    let dir_a = "/tmp/engram_db_verify_plain";
    let dir_b = "/tmp/engram_db_verify_td";
    let base = "项目进度：已完成 v0.21 TokenDelta 压缩引擎联调，压测数据 12345 行，性能提升 3.2 倍";
    let char_count = base.chars().count();
    let mut rows = Vec::new();
    for i in 1..=800 {
        let end = base
            .char_indices()
            .nth(i * char_count / 800)
            .map(|(idx, _)| idx)
            .unwrap_or(base.len());
        rows.push(format!("({i}, '{}')", base[..end].to_string().replace('\'', "''")));
    }

    let dir_size = |d: &str| -> u64 {
        std::fs::metadata(d).map(|m| m.len()).unwrap_or(0)
            + std::fs::metadata(format!("{d}-wal")).map(|m| m.len()).unwrap_or(0)
    };

    // 段 1：无 Tokenizer（TokenDelta 禁用，压缩关）→ 落盘基准
    let _ = std::fs::remove_dir_all(dir_a);
    let mut c0 = Connection::open(dir_a).unwrap();
    c0.execute("CREATE TABLE log (id INT PRIMARY KEY, msg VARCHAR)").unwrap();
    for r in &rows {
        c0.execute(&format!("INSERT INTO log VALUES {r}")).unwrap();
    }
    
    let size_plain = dir_size(dir_a);
    drop(c0); // checkpoint
    // 段 1 重开读回（验证无 TokenDelta 路径数据完整性）
    let mut d0 = Connection::open(dir_a).unwrap();
    let r0 = d0.execute("SELECT COUNT(*) FROM log").unwrap();
    let n0 = match &r0.rows[0][0] { engramdb::Value::Int64(v) => *v, _ => -1 };
    println!("plain 重开 COUNT = {n0}");
    drop(d0);

    // 段 2：注册 v1 词表 → TokenDelta 分派启用 → 落盘对比
    let _ = std::fs::remove_dir_all(dir_b);
    let mut cfg = Config::default();
    cfg.tokenizer_path = Some("data/vocab/engram_vocab_v1.bin".into());
    let mut c1 = Connection::open_with_config(dir_b, cfg).unwrap();
    c1.execute("CREATE TABLE log (id INT PRIMARY KEY, msg VARCHAR)").unwrap();
    for r in &rows {
        c1.execute(&format!("INSERT INTO log VALUES {r}")).unwrap();
    }
    
    let size_td = dir_size(dir_b);

    // 读回验证（压缩开启的库）
    drop(c1); // checkpoint
    let mut cfg3 = Config::default();
    cfg3.tokenizer_path = Some("data/vocab/engram_vocab_v1.bin".into());
    let mut c1 = Connection::open_with_config(dir_b, cfg3).unwrap();
    let result = c1.execute("SELECT COUNT(*) FROM log").unwrap();
    let n = match &result.rows[0][0] { engramdb::Value::Int64(v) => *v, _ => -1 };
    let result = c1.execute("SELECT msg FROM log WHERE id = 400").unwrap();
    let msg400 = match &result.rows[0][0] { engramdb::Value::Varchar(v) => v.clone(), _ => String::new() };
    let result = c1.execute("SELECT msg FROM log WHERE id = 1").unwrap();
    let msg1 = match &result.rows[0][0] { engramdb::Value::Varchar(v) => v.clone(), _ => String::new() };
    let exp400 = base[..base
        .char_indices()
        .nth(400 * char_count / 800)
        .map(|(idx, _)| idx)
        .unwrap_or(base.len())]
        .to_string();
    let exp1 = base[..base
        .char_indices()
        .nth(1 * char_count / 800)
        .map(|(idx, _)| idx)
        .unwrap_or(base.len())]
        .to_string();
    assert_eq!(n, 800);
    assert_eq!(msg400, exp400, "id=400 内容不一致");
    assert_eq!(msg1, exp1, "id=1 内容不一致");

    println!("==== 真实 DB 链路验证（800 流式行，drop 自动 checkpoint）====");
    println!("无 TokenDelta 落盘: {size_plain} bytes");
    println!("TokenDelta 落盘:   {size_td} bytes");
    println!("磁盘节省:          {:.2}x", size_plain as f64 / size_td.max(1) as f64);
    println!("readback: COUNT={n}, id=1 OK, id=400 OK");
    let _ = std::fs::remove_dir_all(dir_a);
    let _ = std::fs::remove_dir_all(dir_b);
    println!("OK");
}
