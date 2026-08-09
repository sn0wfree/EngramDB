//! token 流 dump（TEXT/TOKS 行对，v0.21.2）
//! 用法：dump_tokens [corpus.jsonl] [vocab.bin] [rows] [out.txt]

use std::io::Write;

use engramdb::common::tokenizer::Tokenizer;

fn main() {
    let corpus_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/tmp/engram_corpus/full_corpus_merged.jsonl".into());
    let vocab_path = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "data/vocab/engram_vocab_v1.bin".into());
    let rows_n: usize = std::env::args().nth(3).and_then(|s| s.parse().ok()).unwrap_or(5000);
    let out_path = std::env::args().nth(4).unwrap_or_else(|| "/tmp/opencode_dump.txt".into());
    let content = std::fs::read_to_string(&corpus_path).expect("corpus");
    let vocab = std::fs::read(&vocab_path).expect("vocab");
    let tok = Tokenizer::from_bytes(&vocab).expect("tokenizer");
    let mut f = std::fs::File::create(&out_path).expect("out");
    let mut n = 0usize;
    for line in content.lines() {
        if n >= rows_n {
            break;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        let Some(text) = v.get("text").and_then(|x| x.as_str()) else { continue };
        let tokens = tok.tokenize(text);
        let parts: Vec<String> = tokens
            .iter()
            .map(|t| {
                if (t.id as usize) < tok.vocab_size() {
                    tok.id_to_token(t.id).unwrap_or("?").to_string()
                } else {
                    format!("UNK({})", t.id)
                }
            })
            .collect();
        writeln!(f, "TEXT: {}", text.replace('\n', "\\n")).expect("write");
        writeln!(f, "TOKS: {}", parts.join("|")).expect("write");
        n += 1;
    }
    println!("dumped {n} rows");
}
