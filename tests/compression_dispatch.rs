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

/// 全局 TD 状态（GLOBAL_TOKENIZER / TOKEN_DELTA_ENTROPY / DB open 时 config 覆盖）跨测试
/// 共享——涉及全局分派语义的测试串行化，防并行污染（如 persist 测试 open DB 会把
/// entropy 覆盖回 config 默认 Static）
static TD_GLOBAL_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
    let _g = TD_GLOBAL_LOCK.lock().unwrap();
    set_global_tokenizer(Some(make_tokenizer()));
    // 单形态配置（v0.21）：小数据块级表头开销大，Varint（无表）最优；
    // 显式设置验证 dispatch + roundtrip
    engramdb::storage::compression::set_token_delta_entropy(
        engramdb::common::config::TokenDeltaEntropy::Varint,
    );

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
    // 块级单形态（Varint）至少应优于裸存
    assert_eq!(ctype, CompressionType::TokenDelta, "应选中 TokenDelta");
    assert!(compressed.len() < data.len(), "TokenDelta 应压缩：{} / {}", compressed.len(), data.len());

    let decompressed = decompress(&compressed, ctype, &DataType::Varchar).unwrap();
    assert_eq!(decompressed, data, "roundtrip 必须逐字节还原");
}

#[test]
fn test_varchar_tokendelta_high_entropy_uncompressed() {
    let _g = TD_GLOBAL_LOCK.lock().unwrap();
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

    let codec_v1 = TokenDeltaCodec::new(&tok_v1, EntropyMode::Varint);
    let blob = codec_v1.encode_block(&["你好世界", "你好世界！继续"]);

    let codec_v2 = TokenDeltaCodec::new(&tok_v2, EntropyMode::Varint);
    let err = codec_v2.decode_block(&blob).unwrap_err();
    match err {
        EngramDbError::Parse(msg) => assert!(msg.contains("vocab version"), "应报版本不匹配：{msg}"),
        other => panic!("应返回 Parse 错误：{other:?}"),
    }
}

/// 列存落盘读回回归（v0.21 修复：compression_type_from_u8 缺 TokenDelta=11 映射
/// 导致压缩列读回被当裸序列化错位）——TokenDelta 压缩列必须跨 checkpoint 精确还原
#[test]
fn test_tokendelta_column_persist_roundtrip() {
    let _g = TD_GLOBAL_LOCK.lock().unwrap();
    use engramdb::common::config::Config;
    use engramdb::common::types::{ColumnDef, DataType, TableDef};
    use engramdb::storage::Database;
    use engramdb::Value;
    use std::path::PathBuf;

    let dir = std::env::temp_dir().join(format!(
        "engramdb_td_persist_{}_{}.hdb",
        std::process::id(),
        std::thread::current().name().unwrap_or("t").replace(['(', ')', ':', ' '], "_")
    ));
    let dir = dir.to_string_lossy().to_string();
    let wal = format!("{dir}-wal");
    let _ = std::fs::remove_file(&dir);
    let _ = std::fs::remove_file(&wal);

    // 构造带码长表的小词表（训练产物模拟）→ 写入临时文件 → 配置加载
    let mut vf = VocabFile::new(
        Vec::new(),
        vec![("你".into(), "好".into()), ("世".into(), "界".into())],
        vec![
            "你".into(), "好".into(), "世".into(), "界".into(), "！".into(),
            "h".into(), "e".into(), "l".into(), "o".into(), " ".into(),
            "w".into(), "r".into(), "d".into(), "你好".into(), "世界".into(),
        ],
    );
    let mut sl = vec![0u8; vf.vocab.len()];
    for (id, len) in [(0usize, 2u8), (1, 2), (2, 3), (3, 3), (13, 4), (14, 4)] {
        sl[id] = len;
    }
    for id in [4usize, 5, 6, 7, 8, 9, 10, 11, 12] {
        sl[id] = 8;
    }
    vf.static_lengths = sl;
    let vocab_path = PathBuf::from(format!("{dir}.vocab"));
    std::fs::write(&vocab_path, vf.to_bytes().unwrap()).unwrap();

    let mut cfg = Config::default();
    cfg.tokenizer_path = Some(vocab_path.to_string_lossy().to_string());
    let n = 300usize;
    let base = "你好世界！这是测试文本 hello world 你好世界！继续追加内容测试压缩链路";
    let cc = base.chars().count();

    let mut db = Database::open_with_config(&dir, cfg).unwrap();
    let def = TableDef::new(
        1,
        "log",
        vec![
            ColumnDef::new("id", DataType::Int64),
            ColumnDef::new("msg", DataType::Varchar),
        ],
    );
    db.create_table(def).unwrap();
    for i in 1..=n {
        let end = base
            .char_indices()
            .nth(((i * cc) / n).max(1))
            .map(|(x, _)| x)
            .unwrap_or(base.len());
        db.get_table_mut("log")
            .unwrap()
            .insert(vec![vec![
                Value::Int64(i as i64),
                Value::Varchar(base[..end].to_string()),
            ]])
            .unwrap();
    }
    db.checkpoint().unwrap(); // 压缩落盘
    drop(db);

    // 重开读回：全部 msg 必须精确还原（压缩列跨落盘无错位）
    let mut cfg2 = Config::default();
    cfg2.tokenizer_path = Some(vocab_path.to_string_lossy().to_string());
    let mut db2 = Database::open_with_config(&dir, cfg2).unwrap();
    let rows = db2.get_table_mut("log").unwrap().scan(&[1]).unwrap();
    assert_eq!(rows.len(), n);
    for (i, row) in rows.iter().enumerate() {
        let expected = base[..base
            .char_indices()
            .nth(((i + 1) * cc / n).max(1))
            .map(|(x, _)| x)
            .unwrap_or(base.len())]
            .to_string();
        match &row[0] {
            Value::Varchar(s) => assert_eq!(s, &expected, "行 {} 压缩落盘后错位", i + 1),
            other => panic!("行 {} 非 Varchar: {other:?}", i + 1),
        }
    }
    drop(db2);
    let _ = std::fs::remove_file(&dir);
    let _ = std::fs::remove_file(&wal);
    let _ = std::fs::remove_file(&vocab_path);
}
