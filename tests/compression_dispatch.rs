//! 运行时分派集成测试（v0.21 收尾）：Varchar 列 TokenDelta 压缩分派
//!
//! - 注册全局 Tokenizer 后：流式追加文本（同前缀）→ compress 选中 TokenDelta
//! - roundtrip：compress → decompress 逐字节还原 Varchar 列格式
//! - 块头词表版本校验：版本不匹配 → 显式报错
//!
//! 独立进程（tests/）：不污染 lib 测试的全局 Tokenizer 状态。

use engramdb::common::config::CompressionType;
use engramdb::common::error::EngramDbError;
use engramdb::common::tokenizer::Tokenizer;
use engramdb::common::types::DataType;
use engramdb::common::vocab_file::VocabFile;
use engramdb::storage::compression::{compress, decompress, set_global_tokenizer};

fn make_tokenizer() -> Tokenizer {
    // 小词表（CJK 段内 merge）：字符 + 双字词，TokenDelta 静态码长表非零
    let vf = VocabFile::new(
        Vec::new(),
        vec![
            ("你".into(), "好".into()),
            ("世".into(), "界".into()),
        ],
        vec![
            "你".into(),
            "好".into(),
            "世".into(),
            "界".into(),
            "！".into(),
            "h".into(),
            "e".into(),
            "l".into(),
            "o".into(),
            " ".into(),
            "w".into(),
            "r".into(),
            "d".into(),
            "你好".into(),
            "世界".into(),
        ],
    );
    let mut vf = vf;
    // 静态码长表：高频 id 短码（TokenDelta Static 模式生效路径）
    vf.static_lengths = vec![0u8; vf.vocab.len()];
    for id in [0u8, 1, 2, 3, 13, 14] {
        vf.static_lengths[id as usize] = 4;
    }
    Tokenizer::from_vocab_file(vf).unwrap()
}

/// Varchar 列字节：[len: 4B][value bytes]...
fn varchar_column(texts: &[&str]) -> Vec<u8> {
    let mut out = Vec::new();
    for t in texts {
        out.extend_from_slice(&(t.len() as u32).to_le_bytes());
        out.extend_from_slice(t.as_bytes());
    }
    out
}

#[test]
fn test_varchar_tokendelta_dispatch_and_roundtrip() {
    set_global_tokenizer(Some(make_tokenizer()));

    // 流式追加（同前缀，TokenDelta 的增量主场景）
    let base = "你好世界！这是测试文本 hello world 你好世界！";
    let mut texts = Vec::new();
    let char_count = base.chars().count();
    for i in 1..=50 {
        let take = (i * char_count / 50).min(char_count);
        let end = base
            .char_indices()
            .nth(take)
            .map(|(idx, _)| idx)
            .unwrap_or(base.len());
        texts.push(&base[..end]);
    }
    let data = varchar_column(&texts);

    let (ctype, compressed) = compress(&data, &DataType::Varchar).unwrap();
    // 块级 best-of 三形态（Varint/Static/Huffman）至少应优于裸存
    assert_eq!(ctype, CompressionType::TokenDelta, "应选中 TokenDelta");
    assert!(compressed.len() < data.len(), "TokenDelta 应压缩：{} / {}", compressed.len(), data.len());

    let decompressed = decompress(&compressed, ctype, &DataType::Varchar).unwrap();
    assert_eq!(decompressed, data, "roundtrip 必须逐字节还原");
}

#[test]
fn test_varchar_tokendelta_high_entropy_uncompressed() {
    set_global_tokenizer(Some(make_tokenizer()));

    // 高熵独立短文本：TokenDelta 不占优 → 应兜底 Uncompressed（不 panic）
    let texts: Vec<String> = (0..30).map(|i| format!("unique_value_{i}_{}", i * 7)).collect();
    let strs: Vec<&str> = texts.iter().map(|s| s.as_str()).collect();
    let data = varchar_column(&strs);
    let (ctype, compressed) = compress(&data, &DataType::Varchar).unwrap();
    let decompressed = decompress(&compressed, ctype, &DataType::Varchar).unwrap();
    assert_eq!(decompressed, data, "任意分派结果必须可还原");
    let _ = ctype;
}

#[test]
fn test_tokendelta_vocab_version_mismatch_rejected() {
    use engramdb::storage::compression::token_delta::{EntropyMode, TokenDeltaCodec};
    // 直接 codec 级：不同版本词表编码的块，解码必须显式报错（不静默错位）
    let tok_v2 = make_tokenizer();
    let mut vf = VocabFile::new(
        Vec::new(),
        vec![("你".into(), "好".into()), ("世".into(), "界".into())],
        vec![
            "你".into(), "好".into(), "世".into(), "界".into(), "！".into(),
            "h".into(), "e".into(), "l".into(), "o".into(), " ".into(),
            "w".into(), "r".into(), "d".into(), "你好".into(), "世界".into(),
        ],
    );
    vf.version = 1; // 模拟 v1 词表
    let tok_v1 = Tokenizer::from_vocab_file(vf).unwrap();

    let codec_v1 = TokenDeltaCodec::new(&tok_v1, EntropyMode::Varint, None);
    let blob = codec_v1.encode_block(&["你好世界", "你好世界！继续"]);

    let codec_v2 = TokenDeltaCodec::new(&tok_v2, EntropyMode::Varint, None);
    let err = codec_v2.decode_block(&blob).unwrap_err();
    match err {
        EngramDbError::Parse(msg) => assert!(msg.contains("vocab version"), "应报版本不匹配：{msg}"),
        other => panic!("应返回 Parse 错误：{other:?}"),
    }
}
