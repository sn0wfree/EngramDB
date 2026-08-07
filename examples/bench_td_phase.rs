//! TD 压缩阶段耗时拆分基准（v0.21）
//!
//! 对 encode_block 的四个阶段分别计时：tokenize / 动态字典 / 前缀 delta /
//! 熵编码（三形态分别）。差减验证：sum(阶段) ≈ encode_block 全量。
//!
//! 用法：
//!   cargo run --release --example bench_td_phase -- [corpus.jsonl] [vocab.bin]
//! 默认：/tmp/engram_corpus/full_corpus.jsonl + data/vocab/engram_vocab_v1.bin

use std::fs;
use std::time::Instant;

use engramdb::common::huffman;
use engramdb::common::tokenizer::{Token, Tokenizer, UNKNOWN_ID};
use engramdb::storage::compression::token_delta::{EntropyMode, TokenDeltaCodec};

const STREAM_SNAPSHOTS: usize = 610;
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

struct PhaseTimings {
    tokenize_us: u128,
    dyn_us: u128,
    delta_us: u128,
    ent_varint_us: u128,
    ent_static_us: u128,
    ent_huffman_us: u128,
    ent_varint_size: usize,
    ent_static_size: usize,
    ent_huffman_size: usize,
}

impl PhaseTimings {
    fn zero() -> Self {
        Self {
            tokenize_us: 0,
            dyn_us: 0,
            delta_us: 0,
            ent_varint_us: 0,
            ent_static_us: 0,
            ent_huffman_us: 0,
            ent_varint_size: 0,
            ent_static_size: 0,
            ent_huffman_size: 0,
        }
    }
}

fn phase_bench(tok: &Tokenizer, blocks: &[Vec<String>]) -> PhaseTimings {
    let mut acc = PhaseTimings::zero();
    // Static 熵表预热（词表级 OnceLock 缓存，排除首块建表成本）
    let (codes, _) = tok.static_entropy();
    let vocab_size = tok.vocab_size() as u32;
    let base: Option<&[u8]> = if tok.static_lengths().is_empty() {
        None
    } else {
        Some(tok.static_lengths())
    };

    for block in blocks {
        let strs: Vec<&str> = block.iter().map(|s| s.as_str()).collect();

        // ---- 1. tokenize ----
        let t0 = Instant::now();
        let mut rows_tok: Vec<(Vec<Token>, &str)> = Vec::with_capacity(strs.len());
        let mut prev_text: &str = "";
        let mut prev_tokens: Vec<Token> = Vec::new();
        for text in &strs {
            let tokens = tok.tokenize_incremental(prev_text, &prev_tokens, text);
            rows_tok.push((tokens.clone(), text));
            prev_text = text;
            prev_tokens = tokens;
        }
        acc.tokenize_us += t0.elapsed().as_micros();

        // ---- 2. 动态字典 ----
        let t1 = Instant::now();
        let mut dyn_dict: Vec<String> = Vec::new();
        let mut dyn_index: fxhash::FxHashMap<String, u32> = fxhash::FxHashMap::default();
        let mut rows: Vec<Vec<u32>> = Vec::with_capacity(strs.len());
        for (tokens, text) in &rows_tok {
            let mut row = Vec::with_capacity(tokens.len());
            for t in tokens {
                if t.id == UNKNOWN_ID {
                    let ch = &text[t.offset.clone()];
                    let idx = match dyn_index.get(ch) {
                        Some(&i) => i,
                        None => {
                            let i = dyn_dict.len() as u32;
                            dyn_dict.push(ch.to_string());
                            dyn_index.insert(ch.to_string(), i);
                            i
                        }
                    };
                    row.push(vocab_size + idx);
                } else {
                    row.push(t.id);
                }
            }
            rows.push(row);
        }
        acc.dyn_us += t1.elapsed().as_micros();

        // ---- 3. 前缀 delta ----
        let t2 = Instant::now();
        let mut deltas: Vec<(u32, Vec<u32>)> = Vec::with_capacity(rows.len());
        let mut prev: &[u32] = &[];
        for row in &rows {
            let shared = common_prefix(prev, row);
            deltas.push((shared as u32, row[shared..].to_vec()));
            prev = row;
        }
        acc.delta_us += t2.elapsed().as_micros();

        // ---- 4a. Varint 熵编码 ----
        let t3 = Instant::now();
        let mut ent_varint_size = 0usize;
        for (_, new) in &deltas {
            let mut out = Vec::new();
            for id in new {
                encode_varint(&mut out, *id);
            }
            ent_varint_size += out.len();
        }
        acc.ent_varint_us += t3.elapsed().as_micros();
        acc.ent_varint_size += ent_varint_size;

        // ---- 4b. Static 熵编码 ----
        let t4 = Instant::now();
        let mut ent_static_size = 0usize;
        for (_, new) in &deltas {
            let mut out = static_encode(new, &codes, vocab_size, base);
            ent_static_size += out.len();
        }
        acc.ent_static_us += t4.elapsed().as_micros();
        acc.ent_static_size += ent_static_size;

        // ---- 4c. Huffman 熵编码（含块级建表） ----
        let t5 = Instant::now();
        let new_ids: Vec<u32> = deltas.iter().flat_map(|(_, n)| n.iter().copied()).collect();
        let mut freqs: fxhash::FxHashMap<u32, u64> = fxhash::FxHashMap::default();
        for id in &new_ids {
            *freqs.entry(*id).or_insert(0) += 1;
        }
        let enc = huffman::HuffmanEncoder::new(&freqs);
        let header = enc.header();
        let mut ent_huffman_size = header.len();
        for (_, new) in &deltas {
            let stream = enc.encode(new);
            ent_huffman_size += stream.len();
        }
        acc.ent_huffman_us += t5.elapsed().as_micros();
        acc.ent_huffman_size += ent_huffman_size;
    }
    acc
}

fn common_prefix(a: &[u32], b: &[u32]) -> usize {
    let mut n = 0;
    while n < a.len() && n < b.len() && a[n] == b[n] {
        n += 1;
    }
    n
}

fn encode_varint(out: &mut Vec<u8>, mut v: u32) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            break;
        }
        out.push(b | 0x80);
    }
}

fn static_encode(ids: &[u32], codes: &fxhash::FxHashMap<u32, huffman::Code>, vocab_size: u32, base: Option<&[u8]>) -> Vec<u8> {
    let is_esc = |id: u32| match base {
        Some(b) => id >= vocab_size || b[id as usize] == 0,
        None => true,
    };
    let mut push_huf = |ids: &[u32], out: &mut Vec<u8>| {
        let mut buf: u64 = 0;
        let mut nbits: u32 = 0;
        for &id in ids {
            let c = &codes[&id];
            buf = (buf << c.len as u32) | c.bits as u64;
            nbits += c.len as u32;
            while nbits >= 8 {
                out.push((buf >> (nbits - 8)) as u8);
                nbits -= 8;
                buf &= (1u64 << nbits) - 1;
            }
        }
        if nbits > 0 {
            out.push((buf << (8 - nbits)) as u8);
        }
    };
    if !ids.iter().any(|&id| is_esc(id)) {
        let mut out = Vec::with_capacity(ids.len() + 1);
        out.push(0u8);
        push_huf(ids, &mut out);
        return out;
    }
    let mut flags: Vec<u8> = Vec::with_capacity((ids.len() + 7) / 8);
    let mut fbuf: u64 = 0;
    let mut fnbits: u32 = 0;
    let mut huf_ids: Vec<u32> = Vec::new();
    let mut esc: Vec<u8> = Vec::new();
    for &id in ids {
        let e = is_esc(id);
        fbuf = (fbuf << 1) | e as u64;
        fnbits += 1;
        if fnbits == 8 {
            flags.push(fbuf as u8);
            fbuf = 0;
            fnbits = 0;
        }
        if e {
            encode_varint(&mut esc, id);
        } else {
            huf_ids.push(id);
        }
    }
    if fnbits > 0 {
        flags.push((fbuf << (8 - fnbits)) as u8);
    }
    let mut out = Vec::new();
    out.push(1u8);
    out.extend_from_slice(&flags);
    push_huf(&huf_ids, &mut out);
    out.extend_from_slice(&esc);
    out
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
    let vocab_bytes = std::fs::read(&vocab_path).expect("vocab");
    let tok = Tokenizer::from_bytes(&vocab_bytes).expect("tokenizer");
    println!("corpus: {} texts | vocab: {}", corpus.len(), tok.vocab_size());

    let long_texts: Vec<&String> = {
        let mut v: Vec<&String> = corpus.iter().collect();
        v.sort_by_key(|s| s.len());
        v.into_iter().rev().take(3).collect()
    };
    let stream_blocks: Vec<Vec<String>> = long_texts
        .iter()
        .map(|t| stream_snapshots(t, &tok, STREAM_SNAPSHOTS))
        .collect();
    let doc_blocks: Vec<Vec<String>> = corpus.chunks(DOC_BLOCK_SIZE).map(|c| c.to_vec()).collect();

    for (name, blocks) in [
        ("A 流式追加", &stream_blocks),
        ("C 独立文档", &doc_blocks),
    ] {
        println!("\n=== 场景 {name}（{} 块，{} 事件）===", blocks.len(), blocks.iter().map(|b| b.len()).sum::<usize>());
        let p = phase_bench(&tok, blocks);

        let total_ent = p.ent_varint_us.min(p.ent_static_us).min(p.ent_huffman_us);
        let total = p.tokenize_us + p.dyn_us + p.delta_us + total_ent;
        println!(
            "{:<20} {:>10} {:>7.1}%",
            "1. tokenize", p.tokenize_us, p.tokenize_us as f64 / total as f64 * 100.0
        );
        println!(
            "{:<20} {:>10} {:>7.1}%",
            "2. 动态字典", p.dyn_us, p.dyn_us as f64 / total as f64 * 100.0
        );
        println!(
            "{:<20} {:>10} {:>7.1}%",
            "3. 前缀 delta", p.delta_us, p.delta_us as f64 / total as f64 * 100.0
        );
        println!("熵编码（三形态分别，与 1-3 不可直接相加）：");
        for (n, us, size) in [
            ("  Varint", p.ent_varint_us, p.ent_varint_size),
            ("  Static", p.ent_static_us, p.ent_static_size),
            ("  Huffman", p.ent_huffman_us, p.ent_huffman_size),
        ] {
            println!("{:<20} {:>10} ({:>7.1}% of total+该形态) | 熵流 {size} B", n, us, us as f64 / (total + us - total_ent) as f64 * 100.0);
        }
        println!(
            "{:<20} {:>10}（= 1+2+3 + 最快熵形态，基准对齐 encode_block）",
            "合计(基准)", total
        );

        // 对照：真实 encode_block（三形态各测一次编码全量耗时）
        for (en, mode) in [
            ("Varint", EntropyMode::Varint),
            ("Static", EntropyMode::Static),
            ("Huffman", EntropyMode::Huffman),
        ] {
            let codec = TokenDeltaCodec::new(&tok, mode);
            let t = Instant::now();
            let mut size = 0usize;
            for b in blocks {
                let strs: Vec<&str> = b.iter().map(|s| s.as_str()).collect();
                size += codec.encode_block(&strs).len();
            }
            println!(
                "encode_block({en}): {:>8}µs | 全量体积 {size} B",
                t.elapsed().as_micros()
            );
        }
    }
}
