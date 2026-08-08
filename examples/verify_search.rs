//! 端到端检索验证（v0.21 检索层）：真实词表 + 真实语料
//!
//! DB 链路：open(config tokenizer_path) → 建表 → FTS 索引 → 插入 → BM25/模糊/混合
//! 检索 → checkpoint → reopen 索引恢复 → 再检索。
//!
//! 用法：
//!   cargo run --release --example verify_search -- [corpus.jsonl] [vocab.bin]

use std::fs;
use std::time::Instant;

use engramdb::common::config::Config;
use engramdb::common::types::{ColumnDef, DataType, TableDef};
use engramdb::storage::Database;
use engramdb::Value;

fn load_corpus(path: &str) -> Vec<String> {
    let content = fs::read_to_string(path).expect("corpus");
    content
        .lines()
        .filter_map(|l| {
            let l = l.trim();
            if l.is_empty() {
                return None;
            }
            serde_json::from_str::<serde_json::Value>(l)
                .ok()
                .and_then(|v| v.get("text").and_then(|x| x.as_str()).map(|s| s.to_string()))
        })
        .collect()
}

fn main() {
    let corpus_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/tmp/engram_corpus/full_corpus.jsonl".into());
    let vocab_path = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "data/vocab/engram_vocab_v1.bin".into());
    let corpus = load_corpus(&corpus_path);
    let dir = "/tmp/engram_verify_search.hdb";
    let wal = format!("{dir}-wal");
    let _ = std::fs::remove_file(dir);
    let _ = std::fs::remove_file(&wal);

    let mut cfg = Config::default();
    cfg.tokenizer_path = Some(vocab_path.clone());

    // ---- 写阶段 ----
    let mut db = Database::open_with_config(dir, cfg).unwrap();
    let def = TableDef::new(
        1,
        "docs",
        vec![
            ColumnDef::new("id", DataType::Int64),
            ColumnDef::new("content", DataType::Varchar),
        ],
    );
    db.create_table(def).unwrap();
    db.get_table_mut("docs").unwrap().add_fts_index("content").unwrap();

    let sample: Vec<String> = corpus.iter().step_by(corpus.len() / 1000).take(1000).cloned().collect();
    let t = Instant::now();
    let mut batch: Vec<Vec<Value>> = Vec::with_capacity(256);
    for (i, text) in sample.iter().enumerate() {
        batch.push(vec![Value::Int64(i as i64), Value::Varchar(text.clone())]);
        if batch.len() == 256 {
            db.get_table_mut("docs").unwrap().insert(std::mem::take(&mut batch)).unwrap();
        }
    }
    if !batch.is_empty() {
        db.get_table_mut("docs").unwrap().insert(batch).unwrap();
    }
    println!("插入 1000 行（语料抽样）：{:.2}s", t.elapsed().as_secs_f64());

    let queries = ["人工智能", "测试", "数据库", "文件", "hello world"];
    for q in &queries {
        let hits = db.get_table_mut("docs").unwrap().search_fts("content", q);
        let top = db.get_table_mut("docs").unwrap().search_bm25("content", q, 3);
        println!("「{q}」: FTS 命中 {} 行 | BM25 top: {:?}", hits.len(), top.iter().map(|(r, s)| (*r, s.round() as i32)).collect::<Vec<_>>());
    }
    let fuzzy = db.get_table_mut("docs").unwrap().search_fuzzy_edit("content", "人工只能", 3);
    println!("模糊「人工只能」top: {:?}", fuzzy.iter().map(|(r, s)| (*r, (s * 100.0) as i32)).collect::<Vec<_>>());

    db.checkpoint().unwrap();
    drop(db);

    // ---- 读阶段：索引从磁盘恢复 ----
    let mut db = Database::open_with_config(dir, {
        let mut c = Config::default();
        c.tokenizer_path = Some(vocab_path.clone());
        c
    })
    .unwrap();
    let table = db.get_table_mut("docs").unwrap();
    assert!(!table.fts_indexes().is_empty(), "FTS 索引应从索引段恢复");
    for q in &queries[..3] {
        let hits = table.search_fts("content", q);
        println!("reopen「{q}」: FTS 命中 {} 行（应 > 0）", hits.len());
        assert!(hits.len() > 0, "reopen 后检索不应为空");
    }
    println!("OK");
    let _ = std::fs::remove_file(dir);
    let _ = std::fs::remove_file(&wal);
}
