//! 正式场景基准（v0.21）：模拟真实 opencode 数据形态的完整 DB 链路测试
//!
//! 数据构造（来自真实 opencode 语料 /tmp/engram_corpus/full_corpus.jsonl）：
//! - 3 个长会话 × 610 条流式快照（消息前缀递增，opencode 会话形态）
//! - 3 万条独立消息（语料抽样，真实混合文本）
//! 表：session_log(session_id INT, seq INT, ts BIGINT, content VARCHAR)
//!
//! 三臂对比（同一数据 × 同一流程）：
//!   A: compress_on_persist=false            → 纯裸存
//!   B: 压缩开 + 无 tokenizer                → Dictionary 臂（v0.12 既有）
//!   C: 压缩开 + v1 词表                     → TokenDelta（三形态 best-of）
//!
//! 指标：磁盘大小 / 插入耗时 / checkpoint 耗时 / 重开读回（逐行校验 + 耗时）
//!
//! 用法：cargo run --release --example formal_scenario -- [corpus.jsonl] [vocab.bin]

use std::time::Instant;

use engramdb::common::config::Config;
use engramdb::common::types::{ColumnDef, DataType, TableDef};
use engramdb::storage::Database;
use engramdb::Value;

const STREAM_SESSIONS: usize = 3;
const STREAM_SNAPSHOTS: usize = 610;
const INDEPENDENT_MSGS: usize = 30_000;

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

/// 场景数据：(session_id, seq, content) 行
fn build_rows(corpus: &[String]) -> Vec<(i64, i64, String)> {
    let mut rows = Vec::with_capacity(STREAM_SESSIONS * STREAM_SNAPSHOTS + INDEPENDENT_MSGS);
    // 1. 流式会话（opencode 形态：消息前缀递增）
    let mut long: Vec<&String> = corpus.iter().collect();
    long.sort_by_key(|s| s.len());
    for sid in 0..STREAM_SESSIONS {
        let text = *long.iter().rev().nth(sid).unwrap();
        let cc = text.chars().count();
        let mut prev = String::new();
        for i in 1..=STREAM_SNAPSHOTS {
            let end = text
                .char_indices()
                .nth(i * cc / STREAM_SNAPSHOTS)
                .map(|(idx, _)| idx)
                .unwrap_or(text.len());
            if prev.len() >= end {
                continue;
            }
            prev = text[..end].to_string();
            rows.push((sid as i64 + 1, i as i64, prev.clone()));
        }
    }
    // 2. 独立消息（语料抽样）
    for (i, t) in corpus.iter().step_by(corpus.len() / INDEPENDENT_MSGS.max(1)).take(INDEPENDENT_MSGS).enumerate() {
        rows.push((STREAM_SESSIONS as i64 + 1, i as i64, t.clone()));
    }
    rows
}

/// 运行一臂，返回 (磁盘大小, 插入耗时, checkpoint 耗时, 读回校验结果)
fn run_arm(tag: &str, dir: &str, corpus: &[String], tokenizer: Option<&str>, compress: bool) -> (u64, u128, u128, bool) {
    let _ = std::fs::remove_file(dir);
    let _ = std::fs::remove_file(format!("{dir}-wal"));
    let mut cfg = Config::default();
    cfg.compress_on_persist = compress;
    cfg.tokenizer_path = tokenizer.map(|s| s.to_string());
    let mut db = Database::open_with_config(dir, cfg).unwrap();

    let def = TableDef::new(
        1,
        "session_log",
        vec![
            ColumnDef::new("session_id", DataType::Int64),
            ColumnDef::new("seq", DataType::Int64),
            ColumnDef::new("ts", DataType::Int64),
            ColumnDef::new("content", DataType::Varchar),
        ],
    );
    db.create_table(def).unwrap();

    let rows = build_rows(corpus);
    let mut expect: Vec<(i64, i64, String)> = Vec::with_capacity(rows.len());

    // 插入（批量 2048 行）
    let t0 = Instant::now();
    let mut batch: Vec<Vec<Value>> = Vec::with_capacity(2048);
    for (i, (sid, seq, content)) in rows.iter().enumerate() {
        batch.push(vec![
            Value::Int64(*sid),
            Value::Int64(*seq),
            Value::Int64(1_700_000_000_000 + i as i64),
            Value::Varchar(content.clone()),
        ]);
        expect.push((*sid, *seq, content.clone()));
        if batch.len() >= 2048 {
            db.get_table_mut("session_log").unwrap().insert(std::mem::take(&mut batch)).unwrap();
        }
    }
    if !batch.is_empty() {
        db.get_table_mut("session_log").unwrap().insert(batch).unwrap();
    }
    let insert_us = t0.elapsed().as_micros();

    // checkpoint（压缩落盘）
    let t1 = Instant::now();
    db.checkpoint().unwrap();
    let ckpt_us = t1.elapsed().as_micros();
    drop(db);

    // 重开读回：全量逐行校验
    let t2 = Instant::now();
    let mut cfg2 = Config::default();
    cfg2.tokenizer_path = tokenizer.map(|s| s.to_string());
    let mut db2 = Database::open_with_config(dir, cfg2).unwrap();
    let scan = db2.get_table_mut("session_log").unwrap().scan(&[0, 1, 3]).unwrap();
    let scan_us = t2.elapsed().as_micros();
    let mut ok = scan.len() == expect.len();
    let mut first_bad: Option<(usize, String, String)> = None;
    if ok {
        for (idx, (row, (exp_sid, exp_seq, exp_content))) in scan.iter().zip(expect.iter()).enumerate() {
            let sid = match &row[0] { Value::Int64(v) => *v, _ => i64::MIN };
            let seq = match &row[1] { Value::Int64(v) => *v, _ => i64::MIN };
            let content = match &row[2] { Value::Varchar(s) => s.as_str(), _ => "" };
            if sid != *exp_sid || seq != *exp_seq || content != exp_content {
                ok = false;
                let exp_bytes = exp_content.as_bytes();
                let act_bytes = content.as_bytes();
                let mut diff_at = exp_bytes.len().min(act_bytes.len());
                for (j, (eb, ab)) in exp_bytes.iter().zip(act_bytes.iter()).enumerate() {
                    if eb != ab { diff_at = j; break; }
                }
                first_bad = Some((
                    idx,
                    format!(
                        "sid={exp_sid} seq={exp_seq} | expect len={} got len={} | 首个差异 @ 字节 {diff_at}\n  expect: ...{:?}\n  got:    ...{:?}",
                        exp_bytes.len(),
                        act_bytes.len(),
                        String::from_utf8_lossy(&exp_bytes[diff_at.saturating_sub(20)..diff_at.saturating_add(40).min(exp_bytes.len())]),
                        String::from_utf8_lossy(&act_bytes[diff_at.saturating_sub(20)..diff_at.saturating_add(40).min(act_bytes.len())]),
                    ),
                    String::new(),
                ));
                break;
            }
        }
    }
    drop(db2);

    let main = std::fs::metadata(dir).map(|m| m.len()).unwrap_or(0);
    let wal = std::fs::metadata(format!("{dir}-wal")).map(|m| m.len()).unwrap_or(0);
    match &first_bad {
        Some((idx, e, a)) => println!("  ⚠ 首个不一致 @ 行 {idx}: {e}
                     {a}"),
        None => {}
    }
    println!(
        "{}: 磁盘 {:.2}MB | 插入 {:.2}s | checkpoint {:.2}s | 读回校验 {:.2}s | {} 行 {}",
        tag,
        (main + wal) as f64 / 1048576.0,
        insert_us as f64 / 1e6,
        ckpt_us as f64 / 1e6,
        scan_us as f64 / 1e6,
        scan.len(),
        if ok { "✓ 全部一致" } else { "✗ 数据不一致" }
    );
    (main + wal, insert_us, ckpt_us, ok)
}

fn main() {
    let corpus_path = std::env::args().nth(1).unwrap_or_else(|| "/tmp/engram_corpus/full_corpus.jsonl".into());
    let vocab_path = std::env::args().nth(2).unwrap_or_else(|| "data/vocab/engram_vocab_v1.bin".into());
    let corpus = load_corpus(&corpus_path);
    assert!(!corpus.is_empty(), "语料为空");
    let rows = build_rows(&corpus);
    println!(
        "正式场景：语料 {} 条 | 行数 {}（流式会话 {}×{} + 独立消息 {}）",
        corpus.len(),
        rows.len(),
        STREAM_SESSIONS,
        STREAM_SNAPSHOTS,
        INDEPENDENT_MSGS
    );
    let text_bytes: usize = rows.iter().map(|(_, _, c)| c.len()).sum();
    println!("文本总量 {:.1}MB\n", text_bytes as f64 / 1048576.0);

    let (a_size, a_i, a_c, a_ok) = run_arm("A 裸存          ", "/tmp/formal_a", &corpus, None, false);
    let (b_size, b_i, b_c, b_ok) = run_arm("B Dictionary 臂 ", "/tmp/formal_b", &corpus, None, true);
    let (c_size, c_i, c_c, c_ok) = run_arm("C TokenDelta    ", "/tmp/formal_c", &corpus, Some(&vocab_path), true);

    println!("\n==== 对比 ====");
    println!("磁盘：C/A = {:.2}x，C/B = {:.2}x", a_size as f64 / c_size.max(1) as f64, b_size as f64 / c_size.max(1) as f64);
    println!("压缩率（vs 原文）：A {:.1}x / B {:.1}x / C {:.1}x",
        text_bytes as f64 / a_size.max(1) as f64,
        text_bytes as f64 / b_size.max(1) as f64,
        text_bytes as f64 / c_size.max(1) as f64);
    println!("插入耗时：A {:.2}s / B {:.2}s / C {:.2}s", a_i as f64 / 1e6, b_i as f64 / 1e6, c_i as f64 / 1e6);
    println!("checkpoint：A {:.2}s / B {:.2}s / C {:.2}s", a_c as f64 / 1e6, b_c as f64 / 1e6, c_c as f64 / 1e6);
    assert!(a_ok && b_ok && c_ok, "存在数据不一致");
    println!("数据完整性：三臂全部逐行一致 ✓");
    for d in ["/tmp/formal_a", "/tmp/formal_b", "/tmp/formal_c"] {
        let _ = std::fs::remove_file(d);
        let _ = std::fs::remove_file(format!("{d}-wal"));
    }
}
