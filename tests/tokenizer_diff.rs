//! 统一 Tokenizer 差分测试（v0.21 P0-1d 正确性锁）
//!
//! 自研运行时编码器 vs tokenizers（离线训练产物）：
//! - 无 OOV 样本：逐 token id 全等（同一词表 + 同一预分割）
//! - OOV 样本：tokenizers 无 unk_token 时**丢弃**未登录字符；
//!   自研编码器用 UNKNOWN_ID **标记**（Unicode 字符级兜底，offset 保留原文）。
//!   断言对齐：剔除 UNKNOWN 后 == tokenizers 序列 + 全部文本可逆。
//!
//! golden 由 examples/train_vocab.rs 生成（tokenizers 编码 20 条冒烟语料 + 4 条 OOV 样本）。
//! 词表更新流程：重跑训练 → 替换 data/vocab/ 两文件 → 本测试即新 golden。

use engramdb::common::tokenizer::{Tokenizer, UNKNOWN_ID};

const VOCAB: &[u8] = include_bytes!("../data/vocab/smoke_vocab.bin");
const GOLDEN: &str = include_str!("../data/vocab/smoke_vocab.golden.txt");

fn parse_ids(ids_str: &str) -> Vec<u32> {
    ids_str
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim().parse().unwrap())
        .collect()
}

fn golden_lines() -> Vec<(String, Vec<u32>)> {
    GOLDEN
        .lines()
        .map(|line| {
            let (text_json, ids_str) = line.rsplit_once('|').expect("golden format");
            (
                serde_json::from_str(text_json).expect("golden text json"),
                parse_ids(ids_str),
            )
        })
        .collect()
}

#[test]
fn test_diff_vs_tokenizers() {
    let tok = Tokenizer::from_bytes(VOCAB).expect("load vocab");
    let mut total = 0usize;
    let mut checked = 0usize;
    let mut oov_lines = 0usize;
    for (line_no, (text, expected)) in golden_lines().into_iter().enumerate() {
        let tokens = tok.tokenize(&text);
        let actual: Vec<u32> = tokens.iter().map(|t| t.id).collect();
        if actual.contains(&UNKNOWN_ID) {
            // OOV 行：剔除 UNKNOWN 标记后与 tokenizers（丢弃）一致
            let stripped: Vec<u32> = actual.into_iter().filter(|&id| id != UNKNOWN_ID).collect();
            assert_eq!(
                stripped, expected,
                "OOV 行剔除标记后与 tokenizers 不一致（line {line_no}）：{}",
                &text[..text.len().min(60)]
            );
            oov_lines += 1;
        } else {
            // 无 OOV 行：逐 token 全等
            assert_eq!(
                actual, expected,
                "token 序列与 tokenizers 不一致（line {line_no}）：{}",
                &text[..text.len().min(60)]
            );
        }
        total += expected.len();
        checked += 1;
    }
    assert!(checked >= 10, "golden 样本过少: {checked}");
    assert!(total > 500, "golden token 过少: {total}");
    assert!(oov_lines >= 3, "OOV 样本未覆盖兜底路径: {oov_lines}");
}

#[test]
fn test_diff_oov_marking() {
    // 兜底路径细节：UNKNOWN_ID 标记的字符必须是词表外字符（offset 切片验证）
    let tok = Tokenizer::from_bytes(VOCAB).expect("load vocab");
    let mut unknown_chars = 0usize;
    for (_, (text, _)) in golden_lines().into_iter().enumerate() {
        let tokens = tok.tokenize(&text);
        for t in tokens {
            if t.id == UNKNOWN_ID {
                let c = &text[t.offset.clone()];
                // 单字符检查：标记的必须是不在词表的字符
                assert!(
                    !tok.is_in_vocab(c),
                    "UNKNOWN 标记了词表内字符：{c:?}"
                );
                unknown_chars += 1;
            }
        }
    }
    assert!(unknown_chars > 0, "OOV 样本应产生 UNKNOWN 标记");
}

#[test]
fn test_diff_roundtrip_reconstruct() {
    // 可逆性：含 OOV 的文本必须完整往返（UNKNOWN 标记保留 offset → 原文无损）
    let tok = Tokenizer::from_bytes(VOCAB).expect("load vocab");
    for (text, _) in golden_lines() {
        let tokens = tok.tokenize(&text);
        let recon = tok.reconstruct(&text, &tokens);
        assert_eq!(recon, text, "可逆性失败：{}", &text[..text.len().min(60)]);
    }
}

#[test]
fn test_diff_deterministic() {
    let tok = Tokenizer::from_bytes(VOCAB).expect("load vocab");
    for (text, _) in golden_lines() {
        let a = tok.tokenize(&text);
        let b = tok.tokenize(&text);
        assert_eq!(a, b, "确定性失败");
    }
}

/// 流式前缀序列：增量 tokenize 必须与全量逐 token 一致（真实词表）
#[test]
fn test_diff_incremental_stream() {
    let tok = Tokenizer::from_bytes(VOCAB).expect("load vocab");
    for (text, _) in golden_lines() {
        let mut prev = String::new();
        let mut prev_tokens = Vec::new();
        for i in 1..=text.chars().count() {
            let end = text
                .char_indices()
                .nth(i)
                .map(|(idx, _)| idx)
                .unwrap_or(text.len());
            let next = text[..end].to_string();
            let full = tok.tokenize(&next);
            let inc = tok.tokenize_incremental(&prev, &prev_tokens, &next);
            assert_eq!(inc, full, "增量不一致（step {i}, len {end}）：{}", &text[..end]);
            prev = next;
            prev_tokens = inc;
        }
    }
}
