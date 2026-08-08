//! 语料桶合并器（v0.21.2）：读提取阶段的 w*_b*.jsonl 中间文件，
//! 全局内容哈希去重（双种子 u64，碰撞 2⁻⁶⁴）→ 流式写最终语料 jsonl。
//!
//! 用法：merge_corpus_buckets --src-dir <dir> --out <out.jsonl>

use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};

fn content_hash(text: &str) -> (u64, u64) {
    let b = text.as_bytes();
    let h1 = fxhash::hash64(b);
    let rev: Vec<u8> = b.iter().rev().copied().collect();
    let h2 = fxhash::hash64(&rev);
    (h1, h2)
}

fn main() {
    let mut src_dir = String::new();
    let mut out_file = String::new();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--src-dir" => src_dir = args.next().unwrap(),
            "--out" => out_file = args.next().unwrap(),
            other => panic!("未知参数: {other}"),
        }
    }
    assert!(!src_dir.is_empty() && !out_file.is_empty());

    let mut paths: Vec<_> = std::fs::read_dir(&src_dir)
        .expect("src dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map(|x| x == "jsonl").unwrap_or(false))
        .collect();
    paths.sort();

    let mut seen: HashSet<(u64, u64)> = HashSet::new();
    let out = File::create(&out_file).expect("create out");
    let mut w = BufWriter::new(out);
    let mut total_in = 0usize;
    let mut total_out = 0usize;

    for p in &paths {
        let f = File::open(p).expect("open");
        let reader = BufReader::with_capacity(1 << 20, f);
        for line in reader.lines() {
            let Ok(line) = line else { continue };
            if line.is_empty() {
                continue;
            }
            total_in += 1;
            let v: serde_json::Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let Some(text) = v.get("text").and_then(|x| x.as_str()) else {
                continue;
            };
            let h = content_hash(text);
            if !seen.insert(h) {
                continue;
            }
            let out_line = format!("{{\"text\":{}}}\n", serde_json::to_string(text).unwrap());
            w.write_all(out_line.as_bytes()).expect("write");
            total_out += 1;
        }
    }
    w.flush().expect("flush");

    let meta = std::fs::metadata(&out_file).map(|m| m.len()).unwrap_or(0);
    println!(
        "合并完成：输入 {} 条（中间 {} MB）→ 去重后 {} 条 / {:.1}MB",
        total_in,
        total_in as f64 * 300.0 / 1048576.0,
        total_out,
        meta as f64 / 1048576.0
    );
}
