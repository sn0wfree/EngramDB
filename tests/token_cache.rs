//! TokenStreamCache 测试（v0.21.1 checkpoint tokenize 去重共享）
//!
//! 覆盖：缓存 insert/take 精确匹配（内容哈希）、错配安全 miss、缓存直供与
//! 自 tokenize 编码输出一致、DB 链路 TD+FTS 同开 roundtrip（走缓存路径）。

use engramdb::common::config::{Config, TokenDeltaEntropy};
use engramdb::common::tokenizer::{Token, Tokenizer, UNKNOWN_ID};
use engramdb::common::types::{ColumnDef, DataType, TableDef};
use engramdb::common::vocab_file::VocabFile;
use engramdb::storage::compression::token_delta::TokenDeltaCodec;
use engramdb::storage::compression::token_stream_cache::{cache_row, CachedTokenRow, TokenStreamCache, TOKEN_STREAM_CACHE};
use engramdb::storage::compression::set_global_tokenizer;
use engramdb::storage::Database;
use engramdb::Value;

fn make_tokenizer() -> Tokenizer {
    let vf = VocabFile::new(
        Vec::new(),
        vec![("你".into(), "好".into()), ("世".into(), "界".into())],
        vec![
            "你".into(), "好".into(), "世".into(), "界".into(), "！".into(),
            "h".into(), "e".into(), "l".into(), "o".into(), " ".into(),
            "w".into(), "r".into(), "d".into(), "你好".into(), "世界".into(),
        ],
    );
    Tokenizer::from_vocab_file(vf).unwrap()
}

#[test]
fn test_cache_insert_take_exact_match() {
    let tok = make_tokenizer();
    let mut cache = TokenStreamCache::new();
    let texts = ["你好世界", "hello world", "世界你好"];
    let rows: Vec<Vec<Token>> = texts.iter().map(|t| tok.tokenize(t)).collect();
    for (i, (t, r)) in texts.iter().zip(rows.iter()).enumerate() {
        cache.insert_row(0, t, r, &tok);
    }
    assert_eq!(cache.len(), 3);

    // 精确匹配（同列同内容）→ 命中
    let got = cache.take_row(0, "你好世界", &tok).expect("精确匹配应命中");
    assert_eq!(got.ids.len(), rows[0].len(), "token id 流一致");
    assert_eq!(cache.len(), 2, "消费即删");

    // 已消费 → miss
    assert!(cache.take_row(0, "你好世界", &tok).is_none());
}

#[test]
fn test_cache_miss_on_content_change() {
    let tok = make_tokenizer();
    let mut cache = TokenStreamCache::new();
    let rows: Vec<Vec<Token>> = ["你好世界", "hello world"].iter().map(|t| tok.tokenize(t)).collect();
    cache.insert_row(0, "你好世界", &rows[0], &tok);
    cache.insert_row(0, "hello world", &rows[1], &tok);

    // 同列但内容不同（模拟 compact 排序/删除后变化）→ 安全 miss
    assert!(cache.take_row(0, "hello world!", &tok).is_none(), "内容变化必须 miss");
    // 不同列号 → miss
    assert!(cache.take_row(1, "你好世界", &tok).is_none(), "列号不同必须 miss");
}

#[test]
fn test_encode_from_cache_identical_to_self_tokenize() {
    let tok = make_tokenizer();
    let texts: Vec<&str> = vec!["你好世界", "你好世界！继续", "hello world 测试", "世界 你好"];
    let codec = TokenDeltaCodec::new(&tok, engramdb::common::config::TokenDeltaEntropy::Static);
    let blob_self = codec.encode_block(&texts);

    // 缓存直供路径必须产出逐字节相同 blob
    let rows: Vec<Vec<Token>> = texts.iter().map(|t| tok.tokenize(t)).collect();
    let cached: Vec<CachedTokenRow> = rows
        .iter()
        .zip(texts.iter())
        .map(|(tokens, text)| cache_row(text, tokens))
        .collect();
    let blob_cached = codec.encode_block_from_cache(&texts, &cached);
    assert_eq!(blob_cached, blob_self, "缓存直供与自 tokenize 输出必须一致");

    // 解码一致
    let dec = codec.decode_block(&blob_cached).unwrap();
    assert_eq!(dec, texts);
}

#[test]
fn test_cache_row_unknowns() {
    // OOV 字符（不在词表的字符）→ unknowns 按出现序收集，可还原
    let tok = make_tokenizer();
    let text = "你好Δ世界"; // Δ 不在词表
    let tokens = tok.tokenize(text);
    let has_unknown = tokens.iter().any(|t| t.id == UNKNOWN_ID);
    assert!(has_unknown, "Δ 应产生 UNKNOWN token");
    let row = cache_row(text, &tokens);
    assert!(!row.unknowns.is_empty());
    assert_eq!(row.unknowns[0], "Δ", "OOV 字符文本按序缓存");

    let codec = TokenDeltaCodec::new(&tok, TokenDeltaEntropy::Static);
    let texts = vec![text, "世界Δ你好"];
    let blob_self = codec.encode_block(&texts);
    let rows: Vec<Vec<Token>> = texts.iter().map(|t| tok.tokenize(t)).collect();
    let cached: Vec<CachedTokenRow> = rows
        .iter()
        .zip(texts.iter())
        .map(|(tokens, text)| cache_row(text, tokens))
        .collect();
    let blob_cached = codec.encode_block_from_cache(&texts, &cached);
    assert_eq!(blob_cached, blob_self, "OOV 行缓存直供一致");
    let dec = codec.decode_block(&blob_cached).unwrap();
    assert_eq!(dec, texts, "OOV 行 roundtrip");
}

/// DB 链路：TD 压缩 + FTS 索引同开（自增主键 → 插入序 = 主键序 → 缓存可命中），
/// checkpoint 走缓存直供路径，数据必须逐行一致
#[test]
fn test_db_td_fts_shared_tokenize_roundtrip() {
    let dir = std::env::temp_dir().join(format!(
        "engramdb_td_fts_{}_{}.hdb",
        std::process::id(),
        std::thread::current().name().unwrap_or("t").replace(['(', ')', ':', ' '], "_")
    ));
    let dir = dir.to_string_lossy().to_string();
    let wal = format!("{dir}-wal");
    let _ = std::fs::remove_file(&dir);
    let _ = std::fs::remove_file(&wal);

    let tok = make_tokenizer();
    let mut cfg = Config::default();
    cfg.compress_on_persist = true;
    cfg.token_delta_enabled = true;
    let mut db = Database::open_with_config(&dir, cfg).unwrap();
    set_global_tokenizer(Some(make_tokenizer()));
    engramdb::storage::compression::set_token_delta_entropy(TokenDeltaEntropy::Static);

    let def = TableDef::new(
        1,
        "t",
        vec![
            ColumnDef::new("id", DataType::Int64),
            ColumnDef::new("content", DataType::Varchar),
        ],
    );
    db.create_table(def).unwrap();
    db.get_table_mut("t").unwrap().add_fts_index("content").unwrap();

    let texts = ["你好世界", "hello world", "世界你好 你好", "测试文本内容", "Δ特殊字符"];
    for text in texts {
        db.get_table_mut("t")
            .unwrap()
            .insert(vec![vec![Value::Int64(0), Value::Varchar(text.to_string())]])
            .unwrap();
    }
    // 插入期间缓存应收集（TD 启用 + FTS 存在）
    {
        let cache = TOKEN_STREAM_CACHE.lock().unwrap_or_else(|p| p.into_inner());
        assert!(cache.len() > 0, "插入路径应收集 token 流缓存");
    }
    db.checkpoint().unwrap();
    // checkpoint 尾部清空缓存
    {
        let cache = TOKEN_STREAM_CACHE.lock().unwrap_or_else(|p| p.into_inner());
        assert!(cache.is_empty(), "checkpoint 后缓存应清空");
    }

    // 读回逐行一致（走缓存直供压缩的块必须精确还原）
    let table = db.get_table_mut("t").unwrap();
    for (i, text) in texts.iter().enumerate() {
        let row = table.get_row_by_id(i as u32).unwrap().unwrap();
        assert_eq!(row[1], Value::Varchar(text.to_string()), "行 {i} 必须还原");
    }
    // FTS 索引仍工作（索引路径独立于压缩）
    let hits = table.search_fts("content", "你好");
    assert!(!hits.is_empty(), "FTS 检索应命中：{:?}", hits);

    // reopen 后数据仍一致（压缩态落盘读回）
    drop(db);
    let mut db = Database::open_with_config(&dir, {
        let mut c = Config::default();
        c.compress_on_persist = true;
        c.token_delta_enabled = true;
        c
    })
    .unwrap();
    set_global_tokenizer(Some(make_tokenizer()));
    let table = db.get_table_mut("t").unwrap();
    for (i, text) in texts.iter().enumerate() {
        let row = table.get_row_by_id(i as u32).unwrap().unwrap();
        assert_eq!(row[1], Value::Varchar(text.to_string()), "reopen 行 {i} 必须还原");
    }
    let _ = &tok;

    let _ = std::fs::remove_file(&dir);
    let _ = std::fs::remove_file(&wal);
}
