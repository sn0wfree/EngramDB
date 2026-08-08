//! 行级 token 流缓存（v0.21「checkpoint tokenize 去重共享」）
//!
//! FTS 索引插入时的一次 tokenize 产物入缓存，checkpoint 时 TokenDelta 压缩
//! 逐行消费——一次 tokenize 两用（索引 + 压缩），写入路径 tokenize 成本砍半。
//!
//! 键 = (列号, **单行**内容 hash)：压缩侧解析列字节后逐行精确匹配（行序无关
//! ——compact 按主键排序/删除后内容变化 → miss → 该行回退自 tokenize）。
//! 正确性零风险：tokenize 确定性，行内容 hash 相同则 token 流必然一致。
//!
//! 生命周期：插入累积 → checkpoint 压缩消费（take 即删）→ checkpoint 尾部清残留。
//! 仅在 TokenDelta 启用（Config::token_delta_enabled）且存在 FTS 索引时收集。

use crate::common::tokenizer::{Token, Tokenizer, UNKNOWN_ID};
use std::sync::Mutex;

/// 缓存行：token id 流 + OOV 字符（按出现序）——encode_block 消费所需全部信息
#[derive(Debug, Clone)]
pub struct CachedTokenRow {
    pub ids: Vec<u32>,
    pub unknowns: Vec<String>,
}

/// 单行内容 hash（插入侧与压缩侧调用必须一致）
pub fn row_hash(text: &str) -> u64 {
    fxhash::hash64(text.as_bytes())
}

/// 从 Token 流构建缓存行（tokenize 产物直转；UNKNOWN 需原文取字符）
pub fn cache_row(text: &str, tokens: &[Token]) -> CachedTokenRow {
    let mut ids = Vec::with_capacity(tokens.len());
    let mut unknowns = Vec::new();
    for t in tokens {
        if t.id == UNKNOWN_ID {
            ids.push(UNKNOWN_ID);
            unknowns.push(text[t.offset.clone()].to_string());
        } else {
            ids.push(t.id);
        }
    }
    CachedTokenRow { ids, unknowns }
}

/// 全局 token 流缓存（一次 tokenize 两用：FTS 索引 → TD 压缩）
pub struct TokenStreamCache {
    /// 词表版本（0 = 未初始化；版本不符清空重建）
    vocab_version: u16,
    /// (col_idx, 单行 hash) → 行 token 流（消费 take 即删）
    rows: fxhash::FxHashMap<(u32, u64), CachedTokenRow>,
}

impl TokenStreamCache {
    pub fn new() -> Self {
        Self {
            vocab_version: 0,
            rows: fxhash::FxHashMap::default(),
        }
    }

    /// 插入单行（TD+FTS 同开时由插入路径调用；一次 tokenize 的产物）
    pub fn insert_row(&mut self, col_idx: u32, text: &str, tokens: &[Token], tok: &Tokenizer) {
        if self.vocab_version == 0 {
            self.vocab_version = tok.version();
        } else if self.vocab_version != tok.version() {
            self.rows.clear();
            self.vocab_version = tok.version();
        }
        let row = cache_row(text, tokens);
        self.rows.insert((col_idx, row_hash(text)), row);
    }

    /// 消费单行：内容 hash 精确匹配 → 取走（miss/版本不符 → None）
    pub fn take_row(&mut self, col_idx: u32, text: &str, tok: &Tokenizer) -> Option<CachedTokenRow> {
        if self.vocab_version != 0 && self.vocab_version != tok.version() {
            self.rows.clear();
            self.vocab_version = 0;
            return None;
        }
        self.rows.remove(&(col_idx, row_hash(text)))
    }

    /// 清空（checkpoint 尾部调用：压缩已消费全部可命中项）
    pub fn clear(&mut self) {
        self.rows.clear();
    }

    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

pub static TOKEN_STREAM_CACHE: std::sync::LazyLock<Mutex<TokenStreamCache>> =
    std::sync::LazyLock::new(|| Mutex::new(TokenStreamCache::new()));

/// 当前被压缩列的列号（compress_varchar 消费缓存用；compress 顶层入口不设 →
/// 缓存路径禁用）。用原子避免传参渗透压缩 API。checkpoint 单线程。
pub static CACHE_COL_IDX: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);
