//! TokenDelta 压缩基准（v0.21 P0-2）
//!
//! 三臂（TokenDelta+Varint / +Static / +Huffman）× 三场景（A 流式追加 / B 覆盖重写 / C 独立文档）
//! + 字节级基线对照（zstd / zstd+CDict）+ 不压缩底线。
//!
//! 用法：
//!   cargo run --release --example bench_tokendelta -- [corpus.jsonl] [vocab.bin]
//! 默认：/tmp/engram_corpus/smoke.jsonl + data/vocab/smoke_vocab.bin

use std::fs;
use std::time::Instant;

use engramdb::common::huffman;
use engramdb::common::tokenizer::{Tokenizer, UNKNOWN_ID};
use engramdb::storage::compression::token_delta::{EntropyMode, TokenDeltaCodec};

const STREAM_SNAPSHOTS: usize = 610; // opencode 实测平均重写次数
const REWRITE_SNAPSHOTS: usize = 200; // B 场景快照数（610 全量过慢，趋势一致）
const DOC_BLOCK_SIZE: usize = 64;

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

/// 场景 A：流式追加快照（快照 i = 原文前 i 个 token 的文本）
fn stream_snapshots(text: &str, tok: &Tokenizer, n: usize) -> Vec<String> {
    let tokens = tok.tokenize(text);
    if tokens.is_empty() {
        return Vec::new();
    }
    let mut snaps = Vec::with_capacity(n);
    for i in 1..=n {
        let idx = (tokens.len() as f64 * i as f64 / n as f64).ceil() as usize;
        let idx = idx.min(tokens.len());
        if idx == 0 {
            continue;
        }
        snaps.push(text[..tokens[idx - 1].offset.end].to_string());
    }
    snaps
}

/// 场景 B：覆盖重写快照（同一文本，每次微改：插入片段）
fn rewrite_snapshots(text: &str, n: usize) -> Vec<String> {
    let mut snaps = Vec::with_capacity(n);
    let char_positions: Vec<usize> = text.char_indices().map(|(i, _)| i).collect();
    for i in 0..n {
        let mut s = text.to_string();
        // 微改：字符边界安全插入（模拟 LLM 编辑重写）
        let marker = format!("[rev{i}]");
        if !char_positions.is_empty() {
            let insert_at = char_positions[(i * 7) % char_positions.len()];
            s.insert_str(insert_at, &marker);
        }
        snaps.push(s);
    }
    snaps
}

fn measure<T>(f: impl FnOnce() -> T) -> (T, u128) {
    let t = Instant::now();
    let out = f();
    (out, t.elapsed().as_micros())
}

struct Row {
    name: String,
    orig: usize,
    comp: usize,
    enc_us: u128,
    dec_us: u128,
}

fn main() {
    let corpus_path = std::env::args().nth(1).unwrap_or_else(|| "/tmp/engram_corpus/smoke.jsonl".into());
    let vocab_path = std::env::args().nth(2).unwrap_or_else(|| "data/vocab/smoke_vocab.bin".into());
    let corpus = load_corpus(&corpus_path);
    assert!(!corpus.is_empty(), "empty corpus: {corpus_path}");
    let vocab_bytes = std::fs::read(&vocab_path).expect("vocab");
    // Static 模式码长表：从语料频率算 Huffman 码长，塞进词表（v2 字段）
    let mut vf = engramdb::common::vocab_file::VocabFile::from_bytes(&vocab_bytes).expect("vocab vf");
    let tok0 = Tokenizer::from_bytes(&vocab_bytes).expect("tokenizer");
    let mut freqs: fxhash::FxHashMap<u32, u64> = fxhash::FxHashMap::default();
    for text in &corpus {
        for t in tok0.tokenize(text) {
            if t.id != UNKNOWN_ID {
                *freqs.entry(t.id).or_insert(0) += 1;
            }
        }
    }
    vf.static_lengths = {
        let mut sl = vec![0u8; tok0.vocab_size()];
        for (id, len) in huffman::build_lengths(&freqs) {
            if (id as usize) < tok0.vocab_size() {
                sl[id as usize] = len;
            }
        }
        sl
    };
    let tok = Tokenizer::from_vocab_file(vf).expect("tokenizer");
    println!("corpus: {} texts | vocab: {}", corpus.len(), tok.vocab_size());
    println!("static code-lengths: {} non-zero (of {})", tok.static_lengths().iter().filter(|l| **l > 0).count(), tok.vocab_size());

    // 静态码长（全局频率 → Huffman 码长表）已写入词表（上方 vf.static_lengths）
    let mut total_tokens = 0usize;
    for text in &corpus {
        for t in tok.tokenize(text) {
            if t.id != UNKNOWN_ID {
                total_tokens += 1;
            }
        }
    }

    // ---- 场景数据 ----
    let long_texts: Vec<&String> = {
        let mut v: Vec<&String> = corpus.iter().collect();
        v.sort_by_key(|s| s.len());
        v.into_iter().rev().take(3).collect()
    };
    // A 流式
    let stream_blocks: Vec<Vec<String>> = long_texts
        .iter()
        .map(|t| stream_snapshots(t, &tok, STREAM_SNAPSHOTS))
        .collect();
    // B 覆盖
    let rewrite_blocks: Vec<Vec<String>> = long_texts
        .iter()
        .map(|t| rewrite_snapshots(t, REWRITE_SNAPSHOTS))
        .collect();
    // C 独立文档（分块）
    let doc_blocks: Vec<Vec<String>> = corpus.chunks(DOC_BLOCK_SIZE).map(|c| c.to_vec()).collect();

    // ---- 编码/解码闭包（三种熵编码）----
    let arms: Vec<(&str, EntropyMode)> = vec![
        ("TokenDelta+Varint", EntropyMode::Varint),
        ("TokenDelta+Static", EntropyMode::Static),
        ("TokenDelta+Huffman", EntropyMode::Huffman),
    ];
    // zstd 闭包（无字典 / CDict）
    let dict_samples: Vec<&str> = corpus.iter().take(100).map(|s| s.as_str()).collect();
    let cdict = zstd::dict::from_samples(&dict_samples, 16 * 1024).expect("zstd dict");

    for (scenario, blocks) in [("A 流式追加", &stream_blocks), ("B 覆盖重写", &rewrite_blocks), ("C 独立文档", &doc_blocks)] {
        println!("\n=== 场景 {scenario}（{} 块，{} 事件）===", blocks.len(), blocks.iter().map(|b| b.len()).sum::<usize>());

        let mut results = Vec::new();
        for (name, mode) in &arms {
            let codec = TokenDeltaCodec::new(&tok, *mode);
            let encode = |blocks: &[Vec<String>]| -> Vec<Vec<u8>> {
                blocks.iter().map(|b| {
                    let strs: Vec<&str> = b.iter().map(|s| s.as_str()).collect();
                    codec.encode_block(&strs)
                }).collect()
            };
            let decode = |comps: &[Vec<u8>]| -> usize {
                let mut total = 0usize;
                for c in comps {
                    if let Ok(texts) = codec.decode_block(c) {
                        total += texts.iter().map(|t| t.len()).sum::<usize>();
                    }
                }
                total
            };
            let (comps, enc_us) = measure(|| encode(blocks));
            let (dec_ok, dec_us) = measure(|| decode(&comps));
            let orig: usize = blocks.iter().map(|b| b.iter().map(|s| s.len()).sum::<usize>()).sum();
            results.push(Row {
                name: name.to_string(),
                orig,
                comp: comps.iter().map(|c| c.len()).sum(),
                enc_us,
                dec_us: if dec_ok > 0 { dec_us } else { 0 },
            });
        }
        // zstd 无字典：块内事件拼接压缩
        {
            let encode = |blocks: &[Vec<String>]| -> Vec<Vec<u8>> {
                blocks.iter().map(|b| {
                    let joined = b.join("\n");
                    zstd::bulk::compress(joined.as_bytes(), 3).unwrap_or_default()
                }).collect()
            };
            let decode = |comps: &[Vec<u8>]| -> usize {
                let mut total = 0usize;
                for c in comps {
                    if let Ok(dec) = zstd::bulk::decompress(c, 16 * 1024 * 1024) {
                        total += dec.len();
                    }
                }
                total
            };
            let (comps, enc_us) = measure(|| encode(blocks));
            let (dec_ok, dec_us) = measure(|| decode(&comps));
            let orig: usize = blocks.iter().map(|b| b.iter().map(|s| s.len()).sum::<usize>()).sum();
            results.push(Row {
                name: "zstd-3".into(),
                orig,
                comp: comps.iter().map(|c| c.len()).sum(),
                enc_us,
                dec_us: if dec_ok > 0 { dec_us } else { 0 },
            });
        }
        // zstd+CDict
        {
            let encode = |blocks: &[Vec<String>]| -> Vec<Vec<u8>> {
                let mut comp = zstd::bulk::Compressor::with_dictionary(3, &cdict).expect("zstd comp");
                blocks.iter().map(|b| {
                    let joined = b.join("\n");
                    comp.compress(joined.as_bytes()).unwrap_or_default()
                }).collect()
            };
            let decode = |comps: &[Vec<u8>]| -> usize {
                let mut decomp =
                    zstd::bulk::Decompressor::with_dictionary(&cdict).expect("zstd decomp");
                let mut total = 0usize;
                for c in comps {
                    if let Ok(dec) = decomp.decompress(c, 16 * 1024 * 1024) {
                        total += dec.len();
                    }
                }
                total
            };
            let (comps, enc_us) = measure(|| encode(blocks));
            let (dec_ok, dec_us) = measure(|| decode(&comps));
            let orig: usize = blocks.iter().map(|b| b.iter().map(|s| s.len()).sum::<usize>()).sum();
            results.push(Row {
                name: "zstd-3+CDict".into(),
                orig,
                comp: comps.iter().map(|c| c.len()).sum(),
                enc_us,
                dec_us: if dec_ok > 0 { dec_us } else { 0 },
            });
        }
        // 不压缩底线
        {
            let orig: usize = blocks.iter().map(|b| b.iter().map(|s| s.len()).sum::<usize>()).sum();
            results.push(Row { name: "不压缩".into(), orig, comp: orig, enc_us: 0, dec_us: 0 });
        }

        println!("{:<22} {:>10} {:>10} {:>8} {:>8} {:>8}", "臂", "原始B", "压缩B", "比率", "编码µs", "解码µs");
        for r in &results {
            let ratio = if r.comp > 0 { r.orig as f64 / r.comp as f64 } else { 0.0 };
            println!(
                "{:<22} {:>10} {:>10} {:>8.3} {:>8} {:>8}",
                r.name, r.orig, r.comp, ratio, r.enc_us, r.dec_us
            );
        }
        let base = &results[results.len() - 1].comp;
        for r in &results[..results.len() - 1] {
            println!("  {:<22} 体积比不压缩: {:.3} | vs 最佳: {:.3}", r.name, r.comp as f64 / *base as f64, r.comp as f64 / results.iter().map(|x| x.comp).min().unwrap() as f64);
        }
    }
    println!("\n总 token 数（静态频率统计样本）: {total_tokens}");
}
