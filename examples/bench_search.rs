//! 检索层性能基准（v0.21 实施后验证，用真实 TokenInvertedIndex/BM25/fuzzy）
//!
//! 1. codec 级：全量混合场景构建 + 序列化体积 + 查询（FTS AND / BM25 /
//!    fuzzy edit / fuzzy ngram / RRF 混合）均耗时
//! 2. DB 链路级：insert 带/不带 FTS 索引吞吐 + checkpoint 对比
//!
//! 用法：
//!   cargo run --release --example bench_search -- [corpus.jsonl] [vocab.bin]

use std::fs;
use std::time::Instant;

use engramdb::common::config::Config;
use engramdb::common::tokenizer::Tokenizer;
use engramdb::common::types::{ColumnDef, DataType, TableDef};
use engramdb::search::{search_bm25, Bm25Params, TokenInvertedIndex};
use engramdb::storage::Database;
use engramdb::Value;

const STREAM_SESSIONS: usize = 3;
const STREAM_SNAPSHOTS: usize = 610;
const INDEPENDENT_MSGS: usize = 30000;

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

fn build_rows(corpus: &[String]) -> Vec<String> {
    let mut rows = Vec::new();
    for sid in 0..STREAM_SESSIONS {
        let text = &corpus[(sid * 37) % corpus.len()];
        let chars = text.chars().collect::<Vec<_>>();
        if chars.is_empty() {
            continue;
        }
        let mut prev = String::new();
        for i in 1..=STREAM_SNAPSHOTS {
            let take = (chars.len() * i / STREAM_SNAPSHOTS).max(1).min(chars.len());
            let end = chars[..take].iter().collect::<String>();
            if prev.len() >= end.len() {
                continue;
            }
            prev = end.clone();
            rows.push(end);
        }
    }
    for t in corpus.iter().step_by(corpus.len() / INDEPENDENT_MSGS.max(1)).take(INDEPENDENT_MSGS) {
        rows.push(t.clone());
    }
    rows
}

fn is_cjk(ch: char) -> bool {
    ('\u{4e00}'..='\u{9fff}').contains(&ch)
}

fn main() {
    let corpus_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/tmp/engram_corpus/full_corpus.jsonl".into());
    let vocab_path = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "data/vocab/engram_vocab_v1.bin".into());
    let corpus = load_corpus(&corpus_path);
    let rows = build_rows(&corpus);
    let text_bytes: usize = rows.iter().map(|r| r.len()).sum();
    println!(
        "检索层基准：语料 {} 条 | 行 {} | 文本 {:.1}MB",
        corpus.len(),
        rows.len(),
        text_bytes as f64 / 1048576.0
    );

    let vocab_bytes = std::fs::read(&vocab_path).expect("vocab");
    let tok = Tokenizer::from_bytes(&vocab_bytes).expect("tokenizer");

    // ---- 1. 构建（真实 TokenInvertedIndex）----
    let mut idx = TokenInvertedIndex::with_vocab(tok.version());
    let t0 = Instant::now();
    for (i, r) in rows.iter().enumerate() {
        idx.add_document(i as u32, r, Some(&tok));
    }
    let build_us = t0.elapsed().as_micros();
    let (entries, keys) = idx.size_stats();
    let mem_stream = idx.postings_memory_bytes();
    let ser = idx.to_bytes();
    println!(
        "构建：{:>8}µs ({:>7.0} 行/s, {:.1}µs/行) | 键 {} 条目 {} | postings 流 {:.1}MB | 序列化(TINV2) {:.1}MB",
        build_us,
        rows.len() as f64 / (build_us as f64 / 1e6),
        build_us as f64 / rows.len() as f64,
        keys,
        entries,
        mem_stream as f64 / 1048576.0,
        ser.len() as f64 / 1048576.0
    );

    // ---- 2. query 集（全局频率 top 词）----
    let mut ranked: Vec<(u32, u64)> = Vec::new();
    for (id, cp) in idx.postings() {
        let f: u64 = cp.decode().iter().map(|(_, tf)| *tf as u64).sum();
        ranked.push((*id, f));
    }
    ranked.sort_by_key(|(_, f)| std::cmp::Reverse(*f));
    let mut zh: Vec<String> = Vec::new();
    let mut en: Vec<String> = Vec::new();
    for (id, _) in ranked.iter().take(4000) {
        if let Some(text) = tok.id_to_token(*id) {
            if text.chars().all(is_cjk) && zh.len() < 10 {
                zh.push(text.to_string());
            } else if text.chars().all(|c| c.is_ascii_alphabetic()) && text.len() > 1 && en.len() < 10 {
                en.push(text.to_string());
            }
            if zh.len() == 10 && en.len() == 10 {
                break;
            }
        }
    }
    let mut queries: Vec<String> = zh.clone();
    queries.extend(en.clone());
    for i in 0..5 {
        queries.push(format!("{} {}", zh[i], en[i]));
    }
    println!("query 集：{} 条（中文 {} + 英文 {} + 混合 5）", queries.len(), zh.len(), en.len());

    // ---- 3. 查询耗时（真实检索层 API）----
    let mut t_fts = 0u128;
    let mut t_bm25 = 0u128;
    let mut t_edit = 0u128;
    let mut t_ngram = 0u128;
    let mut t_rrf = 0u128;
    let params = Bm25Params::default();
    let get_text = |row_id: u32| -> Option<String> { rows.get(row_id as usize).cloned() };
    let mut n_edit_hits = 0usize;
    let mut n_ngram_hits = 0usize;

    for q in &queries {
        let t = Instant::now();
        let _ = idx.search(q, Some(&tok));
        t_fts += t.elapsed().as_micros();

        let t = Instant::now();
        let bm = search_bm25(&idx, &tok, q, 10, &params);
        t_bm25 += t.elapsed().as_micros();

        let t = Instant::now();
        let edit = engramdb::search::fuzzy::search_edit(&idx, &tok, q, 10, &get_text);
        t_edit += t.elapsed().as_micros();
        n_edit_hits += edit.len();

        let t = Instant::now();
        let ng = engramdb::search::fuzzy::search_ngram(&idx, &tok, q, 10, 2, &get_text);
        t_ngram += t.elapsed().as_micros();
        n_ngram_hits += ng.len();

        // RRF：BM25 + mock dense（随机行，模拟 HNSW 召回）
        let t = Instant::now();
        let mut dense: Vec<(u32, f32)> = bm.iter().map(|(r, _)| (*r, 1.0)).collect();
        dense.sort_by(|a, b| (a.0 * 31 % 97).cmp(&(b.0 * 31 % 97)));
        let _ = engramdb::search::rrf(&bm, &dense, 60);
        t_rrf += t.elapsed().as_micros();
    }
    let n = queries.len() as f64;
    println!("\n查询均耗时（{} 条 query）:", queries.len());
    println!(
        "  FTS AND      : {:>6.1}µs | BM25 top-10: {:>6.1}µs | fuzzy edit: {:>7.1}µs（命中 {n_edit_hits}）| fuzzy ngram: {:>7.1}µs（命中 {n_ngram_hits}）| RRF: {:>6.1}µs",
        t_fts as f64 / n,
        t_bm25 as f64 / n,
        t_edit as f64 / n,
        t_ngram as f64 / n,
        t_rrf as f64 / n
    );
    let _ = (t_fts, t_bm25, t_rrf);

    // ---- 4. DB 链路级：insert 带/不带 FTS + checkpoint ----
    println!("\nDB 链路级（各插 10000 行独立消息）：");
    for (tag, with_fts) in [("无 FTS 索引", false), ("有 FTS 索引", true)] {
        let dir = format!("/tmp/bench_search_{}.hdb", if with_fts { "fts" } else { "nof" });
        let wal = format!("{dir}-wal");
        let _ = std::fs::remove_file(&dir);
        let _ = std::fs::remove_file(&wal);
        let mut cfg = Config::default();
        cfg.tokenizer_path = Some(vocab_path.clone());
        let mut db = Database::open_with_config(&dir, cfg).unwrap();
        let def = TableDef::new(
            1,
            "t",
            vec![ColumnDef::new("id", DataType::Int64), ColumnDef::new("content", DataType::Varchar)],
        );
        db.create_table(def).unwrap();
        if with_fts {
            db.get_table_mut("t").unwrap().add_fts_index("content").unwrap();
        }
        let data: Vec<String> = corpus.iter().step_by(corpus.len() / 10000).take(10000).cloned().collect();
        let t = Instant::now();
        let mut batch: Vec<Vec<Value>> = Vec::with_capacity(2048);
        for (i, text) in data.iter().enumerate() {
            batch.push(vec![Value::Int64(i as i64), Value::Varchar(text.clone())]);
            if batch.len() == 2048 {
                db.get_table_mut("t").unwrap().insert(std::mem::take(&mut batch)).unwrap();
            }
        }
        if !batch.is_empty() {
            db.get_table_mut("t").unwrap().insert(batch).unwrap();
        }
        let ins_us = t.elapsed().as_micros();
        let t = Instant::now();
        db.checkpoint().unwrap();
        let ckpt_us = t.elapsed().as_micros();
        let t = Instant::now();
        let hits = db.get_table_mut("t").unwrap().search_fts("content", "测试");
        let search_us = t.elapsed().as_micros();
        println!(
            "{tag}: 插入 {:.1}µs/行 | checkpoint {:.1}ms | 「测试」FTS 命中 {}（{search_us}µs）",
            ins_us as f64 / 10000.0,
            ckpt_us as f64 / 1000.0,
            hits.len()
        );
        drop(db);
        let _ = std::fs::remove_file(&dir);
        let _ = std::fs::remove_file(&wal);
    }
}
