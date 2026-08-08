//! FTS 插入路径分阶段 profiling（v0.21.2 决策用）
//!
//! 三阶段差减计时（复制 sparse.rs add_document_with_tokens 逻辑）：
//!   tokenize-only：tok.tokenize(text)
//!   tf+排序：tokenize + tf 聚合 + ws 检查 + pairs 排序（不含 postings push）
//!   全量：真实 add_document_with_tokens（含 push + doc_lens + n_docs）
//! 差减：push = 全量 - (tf+排序)；tf+排序 = (tf+排序) - tokenize-only
//!
//! 规模扫描：--rows 10k/30k/100k → 每 MB 成本 + 各阶段占比（定位超线性）
//!
//! 用法：bench_fts_insert [corpus.jsonl] [vocab.bin] [rows]

use std::collections::HashMap;
use std::time::Instant;

use engramdb::common::tokenizer::{Token, Tokenizer, UNKNOWN_ID};
use engramdb::search::sparse::TokenInvertedIndex;

fn load_corpus(path: &str) -> Vec<String> {
    let content = std::fs::read_to_string(path).expect("corpus");
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

/// 阶段 1：仅 tokenize
fn phase_tokenize(tok: &Tokenizer, texts: &[String]) -> u128 {
    let t = Instant::now();
    for text in texts {
        let _ = tok.tokenize(text);
    }
    t.elapsed().as_micros()
}

/// 阶段 2：tokenize + tf 聚合 + ws 检查 + 排序（不含 push）
fn phase_tf_sort(tok: &Tokenizer, texts: &[String]) -> u128 {
    let t = Instant::now();
    for (row_id, text) in texts.iter().enumerate() {
        let tokens = tok.tokenize(text);
        let mut ws_ids: Vec<u32> = Vec::new();
        let mut tf: fxhash::FxHashMap<u32, u32> = fxhash::FxHashMap::default();
        for tok0 in &tokens {
            if tok0.id == UNKNOWN_ID {
                continue;
            }
            *tf.entry(tok0.id).or_insert(0) += 1;
            let s = &text[tok0.offset.clone()];
            if !s.is_empty() && s.chars().all(char::is_whitespace) && !ws_ids.contains(&tok0.id) {
                ws_ids.push(tok0.id);
            }
        }
        let mut pairs: Vec<(u32, u32)> = tf.into_iter().collect();
        pairs.sort_by_key(|(id, _)| *id);
        let _ = (row_id, ws_ids, pairs);
    }
    t.elapsed().as_micros()
}

/// 阶段 3：全量真实路径（TokenInvertedIndex::add_document_with_tokens）
fn phase_full(idx: &mut TokenInvertedIndex, tok: &Tokenizer, texts: &[String]) -> u128 {
    let t = Instant::now();
    for (row_id, text) in texts.iter().enumerate() {
        let tokens = tok.tokenize(text);
        idx.add_document_with_tokens(row_id as u32, text, &tokens);
    }
    t.elapsed().as_micros()
}

fn main() {
    let corpus_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/tmp/engram_corpus/full_corpus_merged.jsonl".into());
    let vocab_path = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "data/vocab/engram_vocab_v1.bin".into());
    let rows_n: usize = std::env::args()
        .nth(3)
        .and_then(|s| s.parse().ok())
        .unwrap_or(30_000);
    let corpus = load_corpus(&corpus_path);
    assert!(!corpus.is_empty(), "empty corpus");
    let texts: Vec<String> = corpus.into_iter().take(rows_n).collect();
    let bytes: usize = texts.iter().map(|t| t.len()).sum();
    let vocab = std::fs::read(&vocab_path).expect("vocab");
    let tok = Tokenizer::from_bytes(&vocab).expect("tokenizer");

    println!(
        "profiling：{} 行 / {:.1}MB 文本（行均 {:.0}B）",
        texts.len(),
        bytes as f64 / 1048576.0,
        bytes as f64 / texts.len() as f64
    );

    // 预热（TokenOnceLock 表等）
    {
        let t = Instant::now();
        for text in texts.iter().take(100) {
            let _ = tok.tokenize(text);
        }
        let _ = t.elapsed();
    }

    let t1 = phase_tokenize(&tok, &texts);
    let t2 = phase_tf_sort(&tok, &texts);
    let mut idx = TokenInvertedIndex::with_vocab(tok.version());
    let t3 = phase_full(&mut idx, &tok, &texts);

    let mb = bytes as f64 / 1048576.0;
    let tok_us = t1;
    let tf_sort_us = t2 - t1;
    let push_us = t3 - t2;
    let total = t3;
    println!(
        "tokenize     : {:>10}µs ({:>6.1}% | {:.2}ms/MB)",
        tok_us,
        tok_us as f64 / total as f64 * 100.0,
        tok_us as f64 / 1e3 / mb
    );
    println!(
        "tf+排序       : {:>10}µs ({:>6.1}% | {:.2}ms/MB)",
        tf_sort_us,
        tf_sort_us as f64 / total as f64 * 100.0,
        tf_sort_us as f64 / 1e3 / mb
    );
    println!(
        "postings push: {:>10}µs ({:>6.1}% | {:.2}ms/MB)",
        push_us,
        push_us as f64 / total as f64 * 100.0,
        push_us as f64 / 1e3 / mb
    );
    println!(
        "全量          : {:>10}µs ({:.2}ms/MB | {:.1}µs/行)",
        total,
        total as f64 / 1e3 / mb,
        total as f64 / texts.len() as f64
    );
    let (entries, keys) = idx.size_stats();
    println!("索引状态：{} 键 / {} 条目 / doc_lens {}", keys, entries, idx.n_docs());
}
