//! tokenize 内部子环节占比 profiling（SIMD 加速调研，v0.21.2）
//!
//! 复制 tokenizer 热路径分段计时：
//!   A. 预分割（segment_word_pieces：字符分类 + 段划分）
//!   B. 字符初始化（BMP 直查表 → Symbol）
//!   C. BPE 贪心合并（堆）
//!   D. 输出（Token Vec）
//!
//! 用法：bench_tokenize_simd [corpus.jsonl] [vocab.bin] [rows]

use std::time::Instant;

use engramdb::common::pretokenize::segment_word_pieces;
use engramdb::common::tokenizer::{Token, Tokenizer, UNKNOWN_ID};

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

/// 复现 tokenize 并分段计时（逻辑与 tokenizer.rs 对齐）
fn profile_tokenize(tok: &Tokenizer, texts: &[String]) -> (u128, u128, u128, u128) {
    let mut seg = 0u128;
    let mut init = 0u128;
    let mut merge = 0u128;
    let mut out = 0u128;
    for text in texts {
        let t0 = Instant::now();
        let pieces = segment_word_pieces(text, &tok.seeds());
        seg += t0.elapsed().as_nanos();
        for piece in &pieces {
            let word = &text[piece.start..piece.end];
            let t1 = Instant::now();
            let mut symbols: Vec<(u32, u32)> = Vec::with_capacity(word.len());
            for c in word.chars() {
                let cp = c as u32;
                let id = if cp < 0x10000 {
                    tok_bmp_id(tok, cp)
                } else {
                    UNKNOWN_ID
                };
                symbols.push((id, c.len_utf8() as u32));
            }
            init += t1.elapsed().as_nanos();
            let t2 = Instant::now();
            let mut heap: std::collections::BinaryHeap<(std::cmp::Reverse<u32>, std::cmp::Reverse<u32>, std::cmp::Reverse<u32>)> =
                std::collections::BinaryHeap::new();
            for i in 0..symbols.len() as u32 {
                heap.push((std::cmp::Reverse(u32::MAX), std::cmp::Reverse(0), std::cmp::Reverse(i)));
            }
            while let Some(x) = heap.pop() {
                let _ = x;
            }
            merge += t2.elapsed().as_nanos();
            let t3 = Instant::now();
            let mut toks: Vec<Token> = Vec::with_capacity(symbols.len());
            for (id, len) in &symbols {
                toks.push(Token { id: *id, offset: 0..(*len as usize) });
            }
            out += t3.elapsed().as_nanos();
        }
    }
    (seg, init, merge, out)
}

/// BMP 直查表模拟（真实表在 Tokenizer 内部——用公开 API 近似：
/// token_to_id 是 String 键哈希，查表实测更快；此处仅估算分配/循环成本）
fn tok_bmp_id(tok: &Tokenizer, _cp: u32) -> u32 {
    // 用 token_to_id 近似（最坏情形：哈希路径）
    tok.token_to_id(&char::from_u32(0x4E00).unwrap().to_string())
        .unwrap_or(UNKNOWN_ID)
}

fn main() {
    let corpus_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/tmp/engram_corpus/full_corpus_merged.jsonl".into());
    let vocab_path = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "data/vocab/engram_vocab_v1.bin".into());
    let rows_n: usize = std::env::args().nth(3).and_then(|s| s.parse().ok()).unwrap_or(10_000);
    let corpus = load_corpus(&corpus_path);
    let texts: Vec<String> = corpus.into_iter().take(rows_n).collect();
    let bytes: usize = texts.iter().map(|t| t.len()).sum();
    let vocab = std::fs::read(&vocab_path).expect("vocab");
    let tok = Tokenizer::from_bytes(&vocab).expect("tokenizer");

    let (seg, init, merge, out) = profile_tokenize(&tok, &texts);
    let total = seg + init + merge + out;
    let mb = bytes as f64 / 1048576.0;
    println!("{} 行 / {:.1}MB（行均 {:.0}B）", texts.len(), mb, bytes as f64 / texts.len() as f64);
    for (name, us) in [
        ("A. 预分割(分类+段)", seg),
        ("B. 字符初始化(查表)", init),
        ("C. 堆初始化(模拟)", merge),
        ("D. 输出 Token Vec", out),
    ] {
        println!(
            "{:<22} {:>10}µs ({:>5.1}% | {:.2}ms/MB)",
            name,
            us / 1000,
            us as f64 / total as f64 * 100.0,
            us as f64 / 1e6 / mb
        );
    }
    println!("合计（不含真实 merges 合并与重放段）: {:.1}ms/MB", total as f64 / 1e6 / mb);
    let t = Instant::now();
    let mut n = 0usize;
    for text in &texts {
        n += tok.tokenize(text).len();
    }
    println!("真实 tokenize 全量: {:.1}ms/MB（{} tokens）", t.elapsed().as_micros() as f64 / 1e3 / mb, n);
}
