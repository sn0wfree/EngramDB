//! 运行时分派端到端验证（v0.21 收尾）：
//! 真实 v2 词表（含 static_lengths）→ 注册全局 Tokenizer → compress/decompress
//! 验证：流式追加 / 独立文档 两场景的压缩率 + roundtrip。
//!
//! 用法：cargo run --release --example verify_dispatch -- [corpus.jsonl]

use engramdb::common::tokenizer::Tokenizer;
use engramdb::common::types::DataType;
use engramdb::storage::compression::{compress, decompress, set_global_tokenizer};

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

fn varchar_column(texts: &[String]) -> Vec<u8> {
    let mut out = Vec::new();
    for t in texts {
        out.extend_from_slice(&(t.len() as u32).to_le_bytes());
        out.extend_from_slice(t.as_bytes());
    }
    out
}

fn main() {
    let corpus_path = std::env::args().nth(1).unwrap_or_else(|| "/tmp/engram_corpus/full_corpus.jsonl".into());
    let corpus = load_corpus(&corpus_path);
    assert!(!corpus.is_empty());
    let vocab = std::fs::read("data/vocab/engram_vocab_v1.bin").expect("vocab");
    let tok = Tokenizer::from_bytes(&vocab).expect("tokenizer");
    println!("vocab v{} ({} tokens, {} static code-lengths)",
        tok.version(), tok.vocab_size(), tok.static_lengths().iter().filter(|l| **l > 0).count());
    set_global_tokenizer(Some(tok));

    // 场景 A'：流式追加（最长文本的 610 前缀快照）
    let mut long: Vec<&String> = corpus.iter().collect();
    long.sort_by_key(|s| s.len());
    let text = *long.last().unwrap();
    let char_count = text.chars().count();
    let mut snaps = Vec::new();
    for i in 1..=610 {
        let take = (i * char_count / 610).min(char_count);
        let end = text.char_indices().nth(take).map(|(idx, _)| idx).unwrap_or(text.len());
        snaps.push(text[..end].to_string());
    }
    let data = varchar_column(&snaps);
    let (ctype, comp) = compress(&data, &DataType::Varchar).unwrap();
    println!("场景A' 流式追加: {} 事件 {}B -> {:?} {}B ({:.1}x)",
        snaps.len(), data.len(), ctype, comp.len(), data.len() as f64 / comp.len() as f64);
    let dec = decompress(&comp, ctype, &DataType::Varchar).unwrap();
    assert_eq!(dec, data, "roundtrip 失败");
    println!("  roundtrip OK");

    // 场景 C'：独立文档块（64 条）
    let docs: Vec<String> = corpus.iter().take(64).cloned().collect();
    let data2 = varchar_column(&docs);
    let (ctype2, comp2) = compress(&data2, &DataType::Varchar).unwrap();
    println!("场景C' 独立文档: {} 事件 {}B -> {:?} {}B ({:.1}x)",
        docs.len(), data2.len(), ctype2, comp2.len(), data2.len() as f64 / comp2.len() as f64);
    let dec2 = decompress(&comp2, ctype2, &DataType::Varchar).unwrap();
    assert_eq!(dec2, data2, "roundtrip 失败");
    println!("  roundtrip OK");
}
