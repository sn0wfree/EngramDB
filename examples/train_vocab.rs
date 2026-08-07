//! 离线词表训练工具（v0.21 统一 Tokenizer，路线 B 训练端）
//!
//! 依赖 tokenizers（dev-dependencies，examples 可引用；运行时零 C 依赖）：
//!   语料(JSONL) → 共享预分割（src/common/pretokenize.rs，类别段 + 可选种子词）
//!   → BpeTrainer::feed（word 级 BPE 训练，字节 fallback）→ 导出 VocabFile（bincode）
//!
//! 用法：
//!   cargo run --example train_vocab -- --input /tmp/corpus.jsonl --output /tmp/vocab.bin \
//!       --vocab-size 4096 [--seeds jieba_words.txt]

use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

use engramdb::common::pretokenize::segment_words;
use tokenizers::models::bpe::{BpeTrainer, BPE};
use tokenizers::tokenizer::{Model, Trainer};

fn load_jsonl_texts(path: &PathBuf) -> Vec<String> {
    let f = File::open(path).expect("open corpus");
    let reader = BufReader::new(f);
    let mut texts = Vec::new();
    for line in reader.lines() {
        let line = line.expect("read line");
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
            if let Some(t) = v.get("text").and_then(|x| x.as_str()) {
                texts.push(t.to_string());
            }
        }
    }
    texts
}

fn load_seeds(path: &Option<PathBuf>) -> Vec<String> {
    let Some(path) = path else { return Vec::new() };
    let f = File::open(path).expect("open seeds");
    let reader = BufReader::new(f);
    let mut seeds = Vec::new();
    for line in reader.lines() {
        let line = line.expect("read seeds");
        let line = line.trim();
        if !line.is_empty() {
            seeds.push(line.to_string());
        }
    }
    seeds
}

fn main() {
    let mut input = PathBuf::from("/tmp/engram_corpus/smoke.jsonl");
    let mut output = PathBuf::from("/tmp/engram_corpus/smoke_vocab.bin");
    let mut vocab_size: usize = 4096;
    let mut seeds_path: Option<PathBuf> = None;
    let mut min_frequency: u64 = 1;

    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--input" => input = PathBuf::from(args.next().expect("--input path")),
            "--output" => output = PathBuf::from(args.next().expect("--output path")),
            "--vocab-size" => {
                vocab_size = args.next().expect("--vocab-size n").parse().expect("usize")
            }
            "--seeds" => seeds_path = Some(PathBuf::from(args.next().expect("--seeds path"))),
            "--min-frequency" => {
                min_frequency = args.next().expect("--min-frequency n").parse().expect("u64")
            }
            other => panic!("unknown arg: {other}"),
        }
    }

    let texts = load_jsonl_texts(&input);
    assert!(!texts.is_empty(), "empty corpus: {input:?}");
    println!("loaded {} texts from {:?}", texts.len(), input);

    let seeds = load_seeds(&seeds_path);
    println!(
        "seeds: {} ({}模式)",
        seeds.len(),
        if seeds.is_empty() { "纯类别" } else { "种子词" }
    );

    // BPE 训练（直接 feed pretokenized words——与运行时共享同一预分割逻辑）
    let mut model = BPE::builder()
        .byte_fallback(true)
        .build()
        .expect("build BPE");
    let mut trainer = BpeTrainer::new(min_frequency, vocab_size);
    trainer.show_progress = false;

    trainer
        .feed(texts.iter(), |t| {
            // 训练端 word 划分 = 共享 segment_words（类别段 + 可选种子词）
            Ok(segment_words(t, &seeds).into_iter().map(|(w, _)| w).collect())
        })
        .expect("feed corpus");

    println!("unique words: {}", trainer.get_word_count());
    let _special = trainer.train(&mut model).expect("train BPE");

    // 导出 merges（rank 序）：Model::save 写 vocab.json + merges.txt，再解析
    let save_dir = std::env::temp_dir().join(format!("engram_vocab_save_{}", std::process::id()));
    std::fs::create_dir_all(&save_dir).expect("create save dir");
    model.save(&save_dir, None).expect("save model");
    let merges_txt = std::fs::read_to_string(save_dir.join("merges.txt")).expect("read merges.txt");
    let _ = std::fs::remove_dir_all(&save_dir);
    let merges: Vec<(String, String)> = merges_txt
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            let mut it = line.split(' ');
            let a = it.next()?.to_string();
            let b = it.next()?.to_string();
            Some((a, b))
        })
        .collect();

    // 导出 VocabFile：token 按 id 升序（rank 序）；merges 按训练顺序
    let vocab_ids = model.get_vocab();
    let mut token_ranks: Vec<(String, u32)> =
        vocab_ids.into_iter().map(|(t, id)| (t, id)).collect();
    token_ranks.sort_by_key(|(_, id)| *id);
    let vocab: Vec<String> = token_ranks.iter().map(|(t, _)| t.clone()).collect();

    let vf = engramdb::common::vocab_file::VocabFile::new(seeds, merges, vocab);
    let bytes = vf.to_bytes().expect("serialize vocab");
    let mut f = File::create(&output).expect("create output");
    f.write_all(&bytes).expect("write output");
    println!(
        "vocab: {} tokens, {} bytes -> {:?}",
        vf.vocab.len(),
        bytes.len(),
        output
    );

    // 冒烟自检：tokenizers 编码（差分测试 golden 参考）
    // 格式：JSON 转义文本 + "|" + ids（文本含 "|" 安全）
    let mut golden = String::new();
    for t in texts.iter().take(20) {
        // 模拟运行时路径：共享预分割 → model.tokenize 每段
        let words = segment_words(t, &[]);
        let mut ids: Vec<u32> = Vec::new();
        for (w, _) in &words {
            let tokens = model.tokenize(w).expect("tokenize word");
            ids.extend(tokens.iter().map(|tok| tok.id));
        }
        let text_json = serde_json::to_string(t).expect("json escape");
        golden.push_str(&format!("{}|{:?}\n", text_json, ids));
    }
    let golden_path = output.with_extension("golden.txt");
    std::fs::write(&golden_path, golden).expect("write golden");
    println!("golden (20 texts) -> {:?}", golden_path);
}
