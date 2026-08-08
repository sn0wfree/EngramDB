//! FTS 升级性能对比基准（v0.21 → 检索层前置验证）
//!
//! 臂 A：现有 InvertedIndex（字符串分词：空白符+标点、小写）——现状基线
//! 臂 B：Tokenizer tokenize → token_id 倒排（模拟 TokenInvertedIndex 引擎）——提案
//!
//! 场景：会话+独立混合（3×610 流式快照 + 30000 独立消息，与 formal_scenario 同构造）
//! 指标：构建吞吐、索引体积、查询耗时+召回（中文/英文/混合 query 集）、单行维护成本
//!
//! 用法：
//!   cargo run --release --example bench_fts_compare -- [corpus.jsonl] [vocab.bin]

use std::fs;
use std::time::Instant;

use engramdb::common::tokenizer::Tokenizer;
use engramdb::storage::index::inverted_index::InvertedIndex;

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
    // 1. 流式会话快照（重复前缀——TokenDelta 增量主场景）
    for sid in 0..STREAM_SESSIONS {
        let text = &corpus[(sid * 37) % corpus.len()];
        let tokens = text.chars().collect::<Vec<_>>();
        if tokens.is_empty() {
            continue;
        }
        let mut prev = String::new();
        for i in 1..=STREAM_SNAPSHOTS {
            let take = (tokens.len() * i / STREAM_SNAPSHOTS).max(1).min(tokens.len());
            let end = tokens[..take].iter().collect::<String>();
            if prev.len() >= end.len() {
                continue;
            }
            prev = end.clone();
            rows.push(end);
        }
    }
    // 2. 独立消息（语料抽样）
    for (i, t) in corpus.iter().step_by(corpus.len() / INDEPENDENT_MSGS.max(1)).take(INDEPENDENT_MSGS).enumerate() {
        let _ = i;
        rows.push(t.clone());
    }
    rows
}

/// 臂 B 的 token_id 倒排（模拟引擎：token_id → (row, tf) postings + 全局频率）
struct TokenIdx {
    postings: fxhash::FxHashMap<u32, Vec<(u32, u32)>>,
    global_freq: fxhash::FxHashMap<u32, u64>,
}

impl TokenIdx {
    fn new() -> Self {
        Self {
            postings: fxhash::FxHashMap::default(),
            global_freq: fxhash::FxHashMap::default(),
        }
    }

    fn add_document(&mut self, tok: &Tokenizer, row_id: u32, text: &str) {
        let mut row_tf: fxhash::FxHashMap<u32, u32> = fxhash::FxHashMap::default();
        for t in tok.tokenize(text) {
            if t.id != engramdb::common::tokenizer::UNKNOWN_ID {
                *row_tf.entry(t.id).or_insert(0) += 1;
            }
        }
        let mut pairs: Vec<(u32, u32)> = row_tf.into_iter().collect();
        pairs.sort_by_key(|(id, _)| *id);
        for (id, tf) in &pairs {
            self.postings.entry(*id).or_default().push((row_id, *tf));
            *self.global_freq.entry(*id).or_insert(0) += *tf as u64;
        }
    }

    /// AND 交集召回（与 InvertedIndex::search 同语义）
    fn search_ids(&self, ids: &[u32]) -> Vec<u32> {
        if ids.is_empty() {
            return Vec::new();
        }
        let mut result: Vec<u32> = self.postings.get(&ids[0]).map(|p| p.iter().map(|(r, _)| *r).collect()).unwrap_or_default();
        for id in &ids[1..] {
            let term: Vec<u32> = self.postings.get(id).map(|p| p.iter().map(|(r, _)| *r).collect()).unwrap_or_default();
            result = result.into_iter().filter(|r| term.binary_search(r).is_ok()).collect();
            if result.is_empty() {
                break;
            }
        }
        result
    }

    fn size_bytes(&self) -> (usize, usize) {
        let entries: usize = self.postings.values().map(|p| p.len()).sum();
        let key_bytes = self.postings.len() * 4;
        (entries, key_bytes + entries * 8)
    }
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
    assert!(!corpus.is_empty(), "empty corpus: {corpus_path}");
    let rows = build_rows(&corpus);
    let text_bytes: usize = rows.iter().map(|r| r.len()).sum();
    println!(
        "FTS 升级对比：语料 {} 条 | 行 {}（会话 {}×{} + 独立 {}）| 文本 {:.1}MB",
        corpus.len(),
        rows.len(),
        STREAM_SESSIONS,
        STREAM_SNAPSHOTS,
        INDEPENDENT_MSGS,
        text_bytes as f64 / 1048576.0
    );

    // ---- 臂 A：现有字符串倒排 ----
    let mut a = InvertedIndex::new("content");
    let t0 = Instant::now();
    for (i, r) in rows.iter().enumerate() {
        a.add_document(i as u32, r);
    }
    let a_build_us = t0.elapsed().as_micros();
    let a_entries: usize = a.postings().values().map(|p| p.len()).sum();
    let a_key_bytes: usize = a.postings().keys().map(|k| k.len()).sum();
    println!(
        "A 字符串倒排 : 构建 {:>8}µs ({:>8.0} 行/s) | 键 {} | 条目 {} | 体积 {:.1}KB",
        a_build_us,
        rows.len() as f64 / (a_build_us as f64 / 1e6),
        a.postings().len(),
        a_entries,
        (a_key_bytes + a_entries * 4) as f64 / 1024.0
    );

    // ---- 臂 B：token_id 倒排 ----
    let vocab_bytes = std::fs::read(&vocab_path).expect("vocab");
    let tok = Tokenizer::from_bytes(&vocab_bytes).expect("tokenizer");
    let mut b = TokenIdx::new();
    let t1 = Instant::now();
    for (i, r) in rows.iter().enumerate() {
        b.add_document(&tok, i as u32, r);
    }
    let b_build_us = t1.elapsed().as_micros();
    let (b_entries, b_bytes) = b.size_bytes();
    println!(
        "B token 倒排  : 构建 {:>8}µs ({:>8.0} 行/s) | 键 {} | 条目 {} | 体积 {:.1}KB",
        b_build_us,
        rows.len() as f64 / (b_build_us as f64 / 1e6),
        b.postings.len(),
        b_entries,
        b_bytes as f64 / 1024.0
    );
    println!(
        "构建对比：B/A = {:.2}x（{}µs/行 vs {}µs/行）",
        b_build_us as f64 / a_build_us.max(1) as f64,
        b_build_us as f64 / rows.len() as f64,
        a_build_us as f64 / rows.len() as f64
    );

    // ---- query 集：B 构建时统计的全局频率取 top 词 ----
    let mut ranked: Vec<(u32, u64)> = b.global_freq.iter().map(|(id, f)| (*id, *f)).collect();
    ranked.sort_by_key(|(_, f)| std::cmp::Reverse(*f));
    let mut zh: Vec<String> = Vec::new();
    let mut en: Vec<String> = Vec::new();
    for (id, _) in ranked.iter().take(4000) {
        if let Some(text) = tok.id_to_token(*id) {
            if text.chars().all(is_cjk) && zh.len() < 10 {
                zh.push(text.to_string());
            } else if text.chars().all(|c| c.is_ascii_alphabetic()) && en.len() < 10 {
                en.push(text.to_string());
            }
            if zh.len() == 10 && en.len() == 10 {
                break;
            }
        }
    }
    let mut queries: Vec<String> = Vec::new();
    for q in &zh {
        queries.push(q.clone());
    }
    for q in &en {
        queries.push(q.clone());
    }
    for i in 0..5 {
        queries.push(format!("{} {}", zh[i], en[i]));
    }

    // ---- 查询对比 ----
    println!("\n查询对比（{} 条：中文 {} + 英文 {} + 混合 {}）：", queries.len(), zh.len(), en.len(), 5);
    let mut a_total = 0u128;
    let mut b_total = 0u128;
    let mut a_hits = 0usize;
    let mut b_hits = 0usize;
    for q in &queries {
        let t = Instant::now();
        let a_r = a.search(q);
        let a_us = t.elapsed().as_micros();
        let t = Instant::now();
        let ids: Vec<u32> = tok
            .tokenize(q)
            .iter()
            .filter(|t| t.id != engramdb::common::tokenizer::UNKNOWN_ID)
            .map(|t| t.id)
            .collect();
        let b_r = b.search_ids(&ids);
        let b_us = t.elapsed().as_micros();
        a_total += a_us;
        b_total += b_us;
        a_hits += a_r.len();
        b_hits += b_r.len();
        println!(
            "  {:>24.24} | A {a_us:>5}µs 召回 {:<5} | B {b_us:>5}µs 召回 {:<5}",
            q, a_r.len(), b_r.len()
        );
    }
    println!(
        "均查：A {:.1}µs / B {:.1}µs（B/A = {:.2}x）| 总召回：A {} / B {}（B/A = {:.2}x）",
        a_total as f64 / queries.len() as f64,
        b_total as f64 / queries.len() as f64,
        b_total as f64 / a_total.max(1) as f64,
        a_hits,
        b_hits,
        b_hits as f64 / a_hits.max(1) as f64
    );
}
