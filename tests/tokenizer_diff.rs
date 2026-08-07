//! 统一 Tokenizer 差分测试（v0.21 P0-1d 正确性锁）
//!
//! 自研运行时编码器 vs tokenizers（离线训练产物）逐 token 一致：
//! 同一词表（data/vocab/smoke_vocab.bin）+ 同一预分割 → ids 全等。
//! golden 由 examples/train_vocab.rs 生成（tokenizers 编码 20 条冒烟语料）。
//!
//! 词表更新流程：重跑训练 → 替换 data/vocab/ 两文件 → 本测试即新 golden。

use engramdb::common::tokenizer::Tokenizer;

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

#[test]
fn test_diff_vs_tokenizers() {
    let tok = Tokenizer::from_bytes(VOCAB).expect("load vocab");
    let mut total = 0usize;
    let mut checked = 0usize;
    for (line_no, line) in GOLDEN.lines().enumerate() {
        let (text_json, ids_str) = line.rsplit_once('|').expect("golden format");
        let text: String = serde_json::from_str(text_json).expect("golden text json");
        let expected = parse_ids(ids_str);
        let actual: Vec<u32> = tok.tokenize(&text).iter().map(|t| t.id).collect();
        assert_eq!(
            actual, expected,
            "token 序列与 tokenizers 不一致（line {line_no}）：{}",
            &text[..text.len().min(60)]
        );
        total += expected.len();
        checked += 1;
    }
    assert!(checked >= 10, "golden 样本过少: {checked}");
    assert!(total > 500, "golden token 过少: {total}");
}

#[test]
fn test_diff_roundtrip_reconstruct() {
    let tok = Tokenizer::from_bytes(VOCAB).expect("load vocab");
    for line in GOLDEN.lines() {
        let (text_json, _) = line.rsplit_once('|').expect("golden format");
        let text: String = serde_json::from_str(text_json).expect("golden text json");
        let tokens = tok.tokenize(&text);
        let recon = tok.reconstruct(&text, &tokens);
        // 冒烟语料全覆盖 → 无丢弃字符 → 完整可逆
        assert_eq!(recon, text, "可逆性失败：{}", &text[..text.len().min(60)]);
    }
}

#[test]
fn test_diff_deterministic() {
    let tok = Tokenizer::from_bytes(VOCAB).expect("load vocab");
    for line in GOLDEN.lines() {
        let (text_json, _) = line.rsplit_once('|').expect("golden format");
        let text: String = serde_json::from_str(text_json).expect("golden text json");
        let a = tok.tokenize(&text);
        let b = tok.tokenize(&text);
        assert_eq!(a, b, "确定性失败");
    }
}
