//! 多库合并语料提取器（v0.21.2 大数据压测准备）
//!
//! 从 opencode.db 及其备份（多时点快照）只读提取文本类 part，按内容 hash
//! 分桶写中间文件（桶内去重）——后续合并阶段跨库去重（同 hash 必同桶，
//! 桶间零重复 → 桶级并行合并即全局去重）。
//!
//! 提取内容（每字符串字段独立一条 {"text":...}，利于跨库去重）：
//! - text / reasoning / compaction part：$.text
//! - tool part：state.input.{command, description, oldString, newString, content}
//!   + state.output（单条截断 64KB）
//!
//! 分片：--shard i --shards n → rowid 范围均分（多 worker 进程并行）。
//!
//! 用法：
//!   extract_corpus_merged --db <db> --worker-id N --shard i --shards n --out-dir <dir>
//!   （worker-id 用于中间文件名前缀，防并发写冲突）

use std::collections::HashSet;
use std::fs::File;
use std::io::{BufWriter, Write};

use engramdb::common::error::Result;

const BUCKETS: usize = 64;
const MAX_OUTPUT_LEN: usize = 64 * 1024;

/// 双种子内容哈希（桶键 + 去重键；碰撞 2⁻⁶⁴ 量级）
fn content_hash(text: &str) -> (u64, u64) {
    let b = text.as_bytes();
    let h1 = fxhash::hash64(b);
    let rev: Vec<u8> = b.iter().rev().copied().collect();
    let h2 = fxhash::hash64(&rev);
    (h1, h2)
}

/// UTF-8 安全截断到 max_len 字节（char 边界回退）
fn truncate_utf8(s: &str, max_len: usize) -> &str {
    if s.len() <= max_len {
        return s;
    }
    let mut end = max_len;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn main() {
    let mut db_path = String::new();
    let mut worker_id = 0usize;
    let mut shard = 0usize;
    let mut shards = 1usize;
    let mut out_dir = String::new();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--db" => db_path = args.next().unwrap(),
            "--worker-id" => worker_id = args.next().unwrap().parse().unwrap(),
            "--shard" => shard = args.next().unwrap().parse().unwrap(),
            "--shards" => shards = args.next().unwrap().parse().unwrap(),
            "--out-dir" => out_dir = args.next().unwrap(),
            other => panic!("未知参数: {other}"),
        }
    }
    assert!(!db_path.is_empty() && !out_dir.is_empty());
    let _ = worker_id;

    let conn = rusqlite::Connection::open_with_flags(
        &db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("open db");

    // rowid 范围均分
    let (lo, hi): (i64, i64) = conn
        .query_row("SELECT MIN(rowid), MAX(rowid) FROM part", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .expect("rowid range");
    let span = (hi - lo) / shards as i64 + 1;
    let start = lo + shard as i64 * span;
    let end = (start + span - 1).min(hi);

    // 桶文件（worker 前缀防并发冲突）
    let mut writers: Vec<Option<BufWriter<File>>> = (0..BUCKETS).map(|_| None).collect();
    let mut seen: Vec<HashSet<(u64, u64)>> = (0..BUCKETS).map(|_| HashSet::new()).collect();

    let mut out = |text: String, writers: &mut Vec<Option<BufWriter<File>>>| {
        if text.is_empty() {
            return;
        }
        let (h1, h2) = content_hash(&text);
        let b = (h1 as usize) % BUCKETS;
        if !seen[b].insert((h1, h2)) {
            return;
        }
        let w = writers[b].get_or_insert_with(|| {
            let path = format!("{out_dir}/w{worker_id}_b{b:02}.jsonl");
            BufWriter::new(File::create(path).expect("create bucket"))
        });
        let line = format!("{{\"text\":{}}}\n", serde_json::to_string(&text).unwrap());
        w.write_all(line.as_bytes()).expect("write");
    };

    // 1. text / reasoning / compaction
    {
        let mut stmt = conn
            .prepare(
                "SELECT json_extract(data,'$.text') FROM part WHERE rowid BETWEEN ?1 AND ?2 \
                 AND json_extract(data,'$.type') IN ('text','reasoning','compaction')",
            )
            .expect("stmt text");
        let mut rows = stmt.query([start, end]).expect("query text");
        while let Some(row) = rows.next().expect("next") {
            if let Ok(Some(t)) = row.get::<_, Option<String>>(0) {
                out(t, &mut writers);
            }
        }
    }

    // 2. tool part：input 各字符串字段 + output
    {
        let mut stmt = conn
            .prepare(
                "SELECT json_extract(data,'$.state') FROM part WHERE rowid BETWEEN ?1 AND ?2 \
                 AND json_extract(data,'$.type')='tool'",
            )
            .expect("stmt tool");
        let mut rows = stmt.query([start, end]).expect("query tool");
        while let Some(row) = rows.next().expect("next") {
            let state: Option<String> = row.get(0).unwrap_or(None);
            let Some(state) = state else { continue };
            let v: serde_json::Value = match serde_json::from_str(&state) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let mut fields: Vec<&str> = Vec::new();
            if let Some(inp) = v.get("input") {
                for k in ["command", "description", "oldString", "newString", "content"] {
                    if let Some(s) = inp.get(k).and_then(|x| x.as_str()) {
                        fields.push(s);
                    }
                }
            }
            if let Some(s) = v.get("output").and_then(|x| x.as_str()) {
                let s = truncate_utf8(s, MAX_OUTPUT_LEN);
                fields.push(s);
            }
            for f in fields {
                out(f.to_string(), &mut writers);
            }
        }
    }

    // 收尾：flush + 统计
    let mut total = 0usize;
    for (i, w) in writers.iter_mut().enumerate() {
        if let Some(w) = w {
            w.flush().expect("flush");
        }
        total += seen[i].len();
    }
    println!("shard {shard}/{shards} @ {db_path}: 去重后 {total} 条");
}
