//! 检索引擎测试（v0.21「TokenDelta 引擎」= Tokenizer 词表空间 sparse 检索层）
//!
//! 覆盖：TokenInvertedIndex 单元（AND/OR/降级/序列化）、BM25 排序、fuzzy
//! 编辑距离/n-gram、RRF 混合、table 级 FTS 索引 + checkpoint 落盘读回。

use engramdb::common::config::Config;
use engramdb::common::tokenizer::Tokenizer;
use engramdb::common::types::{ColumnDef, DataType, TableDef};
use engramdb::common::vocab_file::VocabFile;
use engramdb::search::{
    fuzzy::{edit_distance, score_edit, score_ngram},
    rrf, search_bm25, Bm25Params, TokenInvertedIndex,
};
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

fn ids(tok: &Tokenizer, text: &str) -> Vec<u32> {
    tok.tokenize(text)
        .iter()
        .filter(|t| t.id != engramdb::common::tokenizer::UNKNOWN_ID)
        .map(|t| t.id)
        .collect()
}

#[test]
fn test_token_inverted_and_or_search() {
    let tok = make_tokenizer();
    let mut idx = TokenInvertedIndex::with_vocab(tok.version());
    idx.add_document(0, "你好世界", Some(&tok));
    idx.add_document(1, "你好 世界 hello", Some(&tok));
    idx.add_document(2, "hello world", Some(&tok));

    let q = ids(&tok, "你好");
    assert_eq!(idx.search_and(&q), vec![0, 1], "AND 应命中含「你好」的行");
    let q2 = ids(&tok, "你好世界");
    assert_eq!(idx.search_and(&q2), vec![0, 1], "AND 交集（你好+世界）：文档 1 两者均含");
    let q3 = ids(&tok, "world");
    assert_eq!(idx.search_or(&q3), vec![1, 2], "doc1 含 o/l，doc2 全含——OR 都召回");
    assert_eq!(idx.doc_len(0), 2, "「你好世界」= 你好+世界 = 2 token");
}

#[test]
fn test_token_inverted_serialization_roundtrip() {
    let tok = make_tokenizer();
    let mut idx = TokenInvertedIndex::with_vocab(tok.version());
    idx.add_document(0, "你好世界", Some(&tok));
    idx.add_document(1, "hello world", Some(&tok));
    idx.add_document(2, "世界 你好", Some(&tok));

    let bytes = idx.to_bytes();
    let restored = TokenInvertedIndex::from_bytes(&bytes).unwrap();
    assert_eq!(restored.vocab_version(), Some(tok.version()));
    assert_eq!(restored.n_docs(), 3);
    assert_eq!(restored.search_and(&ids(&tok, "你好")), vec![0, 2]);
    assert_eq!(restored.doc_len(1), idx.doc_len(1));
}

#[test]
fn test_token_inverted_string_fallback() {
    // 无 Tokenizer → 字符串降级模式（原 InvertedIndex 语义不丢）
    let mut idx = TokenInvertedIndex::new();
    idx.add_document(0, "hello world", None);
    idx.add_document(1, "Hello, World!", None);
    idx.add_document(2, "rust rocks", None);

    assert_eq!(idx.vocab_version(), None);
    let q = "hello world";
    assert_eq!(idx.search(q, None), vec![0, 1], "小写归一 + 标点切分");
    let q2 = "hello";
    assert_eq!(idx.search(q2, None), vec![0, 1]);
    let q3 = "rust hello";
    let none: Option<&Tokenizer> = None;
    assert_eq!(idx.search(q3, none), Vec::<u32>::new(), "AND：无行同时含两者");
}

#[test]
fn test_bm25_ranking() {
    let tok = make_tokenizer();
    let mut idx = TokenInvertedIndex::with_vocab(tok.version());
    // 文档 0 含 3 次「你好」，文档 1 含 1 次
    idx.add_document(0, "你好 你好 你好", Some(&tok));
    idx.add_document(1, "你好 世界 hello", Some(&tok));
    idx.add_document(2, "世界 hello world", Some(&tok));

    let results = search_bm25(&idx, &tok, "你好", 3, &Bm25Params::default());
    assert!(!results.is_empty());
    assert_eq!(results[0].0, 0, "tf=3 的文档应排第一：{:?}", results);
    assert!(results[0].1 > results[1].1, "分数递减：{:?}", results);
}

#[test]
fn test_fuzzy_edit_distance() {
    let a = ids(&make_tokenizer(), "你好世界");
    let b = ids(&make_tokenizer(), "你好世界");
    assert_eq!(edit_distance(&a, &b, 10), 0);
    let c: Vec<u32> = a.iter().take(a.len().saturating_sub(1)).copied().collect();
    let d = edit_distance(&a, &c, 10);
    assert!(d <= 1, "删一个 token 距离应 ≤ 1");
    let score = score_edit(&a, &c, 64).unwrap();
    assert!(score > 0.5, "高度相似序列分数应高：{score}");
}

#[test]
fn test_fuzzy_edit_distance_banded() {
    // banded 与全宽一致（带内）；带外返回长度差（剪枝语义）
    let tok = make_tokenizer();
    let a = ids(&tok, "你好世界 hello");
    let b = ids(&tok, "你好世界 hello world");
    assert_eq!(edit_distance(&a, &a, 8), 0, "相同序列距离 0");
    // b 多「空格+w,o,r,l,d」5 个 token（词表无 world 组合，逐字符）
    assert_eq!(edit_distance(&a, &b, 8), 5, "多 5 个 token 距离 5");
    // 差异超过 max_dist → 剪枝返回长度差
    let c = ids(&tok, "你好世界 hello world world world");
    let d = edit_distance(&a, &c, 2);
    assert_eq!(d, a.len().abs_diff(c.len()), "带外剪枝返回长度差");
    // 带外小距离验证：全宽参考
    let mut brute = 0usize;
    for _ in 0..1 {
        // 与全宽行向量对比（小序列）
        let x = ids(&tok, "你好世界");
        let y = ids(&tok, "你好世界！");
        let mut prev: Vec<usize> = (0..=y.len()).collect();
        let mut cur = vec![0usize; y.len() + 1];
        for i in 1..=x.len() {
            cur[0] = i;
            for j in 1..=y.len() {
                let cost = if x[i - 1] == y[j - 1] { 0 } else { 1 };
                cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
            }
            std::mem::swap(&mut prev, &mut cur);
        }
        brute = prev[y.len()];
    }
    assert_eq!(edit_distance(&ids(&tok, "你好世界"), &ids(&tok, "你好世界！"), 8), brute, "banded 与全宽一致");
}

#[test]
fn test_fuzzy_ngram_overlap() {
    let tok = make_tokenizer();
    let a = ids(&tok, "你好世界 hello");
    let b = ids(&tok, "你好世界 hello world");
    let c = ids(&tok, "世界 hello world");
    let sab = score_ngram(&a, &b, 2);
    let sac = score_ngram(&a, &c, 2);
    assert!(sab > 0.0 && sab > sac, "共享 2-gram 多的文档分数更高：{sab} vs {sac}");
}

#[test]
fn test_rrf_merge() {
    let sparse: Vec<(u32, f32)> = vec![(1, 1.0), (3, 0.8), (5, 0.6)];
    let dense: Vec<(u32, f32)> = vec![(2, 0.9), (3, 0.7), (1, 0.4)];
    let merged = rrf(&sparse, &dense, 60);
    assert_eq!(merged[0].0, 1, "两路都有的行排最前");
    assert!(merged.iter().any(|(r, _)| *r == 2));
    assert!(merged.iter().any(|(r, _)| *r == 5));
    let row1_score = merged.iter().find(|(r, _)| *r == 1).map(|(_, s)| *s).unwrap();
    let row2_score = merged.iter().find(|(r, _)| *r == 2).map(|(_, s)| *s).unwrap();
    assert!(row1_score > row2_score, "双路命中 > 单路命中");
}

/// table 级：FTS 索引 + 行维护 + checkpoint 落盘读回（v0.21 修复「索引不落盘」）
#[test]
fn test_table_fts_persist_roundtrip() {
    use engramdb::common::config::Config;
    let dir = std::env::temp_dir().join(format!(
        "engramdb_fts_{}_{}.hdb",
        std::process::id(),
        std::thread::current().name().unwrap_or("t").replace(['(', ')', ':', ' '], "_")
    ));
    let dir = dir.to_string_lossy().to_string();
    let wal = format!("{dir}-wal");
    let _ = std::fs::remove_file(&dir);
    let _ = std::fs::remove_file(&wal);

    let mut cfg = Config::default();
    cfg.compress_on_persist = true;
    {
        let mut db = Database::open_with_config(&dir, cfg).unwrap();
        let def = TableDef::new(
            1,
            "t",
            vec![ColumnDef::new("id", DataType::Int64), ColumnDef::new("content", DataType::Varchar)],
        );
        db.create_table(def).unwrap();
        // DB open 会按 config 覆盖全局 Tokenizer（默认 None）——这里在 open 后重新注册
        engramdb::storage::compression::set_global_tokenizer(Some(make_tokenizer()));
        let fts_ready = {
            let table = db.get_table_mut("t").unwrap();
            table.add_fts_index("content").unwrap();
            true
        };
        let _ = fts_ready;

        for (i, text) in ["你好世界", "hello world", "世界你好你好"].iter().enumerate() {
            db.get_table_mut("t").unwrap().insert(vec![vec![Value::Int64(i as i64), Value::Varchar(text.to_string())]])
                .unwrap();
        }
        let table = db.get_table_mut("t").unwrap();
        let idx = table.fts_indexes().get("content").unwrap();
        eprintln!("DBG idx ver={:?} keys={} docs={}", idx.vocab_version(), idx.postings().len(), idx.n_docs());
        let hits = table.search_fts("content", "你好");
        assert_eq!(hits, vec![0, 2], "中文词级命中：{:?}", hits);
        let bm25 = table.search_bm25("content", "你好", 3);
        assert_eq!(bm25[0].0, 2, "含 2 次「你好」的行应排第一：{:?}", bm25);
        db.checkpoint().unwrap();
        // 落盘：索引段写入主文件
    }

    // reopen：索引应从磁盘恢复（不再丢失）
    {
        let mut db = Database::open(&dir).unwrap();
        // 生产路径：Config::tokenizer_path 会在 open 时恢复全局 Tokenizer；
        // 测试等价地显式注册
        engramdb::storage::compression::set_global_tokenizer(Some(make_tokenizer()));
        let table = db.get_table_mut("t").unwrap();
        assert!(!table.fts_indexes().is_empty(), "FTS 索引应从索引段恢复");
        let hits = table.search_fts("content", "世界");
        assert_eq!(hits, vec![0, 2], "reopen 后中文命中：{:?}", hits);
        let bm25 = table.search_bm25("content", "hello", 3);
        assert_eq!(bm25[0].0, 1, "reopen 后 BM25：「hello」只命中 doc1：{:?}", bm25);
        let fuzzy = table.search_fuzzy_edit("content", "你好", 3);
        assert!(!fuzzy.is_empty(), "fuzzy 检索不应为空");
    }

    let _ = std::fs::remove_file(&dir);
    let _ = std::fs::remove_file(&wal);
}

/// table 级：hybrid RRF（sparse + HNSW dense）
#[test]
fn test_table_hybrid_search() {
    let dir = std::env::temp_dir().join(format!(
        "engramdb_hybrid_{}_{}.hdb",
        std::process::id(),
        std::thread::current().name().unwrap_or("t").replace(['(', ')', ':', ' '], "_")
    ));
    let dir = dir.to_string_lossy().to_string();
    let wal = format!("{dir}-wal");
    let _ = std::fs::remove_file(&dir);
    let _ = std::fs::remove_file(&wal);

    let mut db = Database::open(&dir).unwrap();
    let def = TableDef::new(
        1,
        "t",
        vec![
            ColumnDef::new("content", DataType::Varchar),
            ColumnDef::new("emb", DataType::Vector { dim: 4 }),
        ],
    );
    db.create_table(def).unwrap();
    // DB open 覆盖全局 Tokenizer 后重新注册
    engramdb::storage::compression::set_global_tokenizer(Some(make_tokenizer()));
    {
        let table = db.get_table_mut("t").unwrap();
        table.add_fts_index("content").unwrap();
    }
    db.create_vector_index(
        "t",
        "idx_emb",
        "emb",
        engramdb::storage::vector_index::DistanceMetric::L2,
        8,
        20,
    )
    .unwrap();
    // 向量列插入（Value::Vector 直接写入）
    for (i, (text, vec)) in [
        ("你好世界", vec![0.1f32, 0.2, 0.3, 0.4]),
        ("hello world", vec![0.9f32, 0.8, 0.7, 0.6]),
        ("世界 你好 hello", vec![0.5f32, 0.4, 0.3, 0.2]),
    ]
    .iter()
    .enumerate()
    {
        let _ = i;
        db.get_table_mut("t").unwrap().insert(vec![vec![
            Value::Varchar(text.to_string()),
            Value::Vector(vec.clone()),
        ]])
        .unwrap();
    }
    let query_vec = vec![0.2f32, 0.3, 0.4, 0.5]; // 近文档 0
    let merged = db
        .get_table_mut("t")
        .unwrap()
        .hybrid_search("content", "idx_emb", "你好", &query_vec, 5)
        .unwrap();
    assert!(!merged.is_empty(), "RRF 混合应有结果");
    assert_eq!(merged[0].0, 0, "文本+向量双路都近的行排第一：{:?}", merged);

    let _ = std::fs::remove_file(&dir);
    let _ = std::fs::remove_file(&wal);
}
