//! Token 级倒排索引（v0.21 检索层核心）
//!
//! 「TokenDelta 引擎」的 sparse 部分：token_id → (row_id, tf) 行级 postings。
//! 与 TD 压缩同源（同一 Tokenizer / 词表 id 空间 / 同一 token 流）；压缩 codec 解耦。
//!
//! 两种模式：
//! - **Tokenizer 模式**（主）：键 = 词表 id（u32），中文词级可搜；tf 计数供 BM25
//! - **字符串降级模式**：无全局 Tokenizer 时按空白符+标点分词（原 InvertedIndex 逻辑），
//!   不丢功能
//!
//! 块格式（to_bytes，手动字节流，对齐 storage/index 持久化风格）：
//! ```text
//! [magic u32][vocab_version i16(-1=字符串模式)][n_docs u32]
//! [postings_len u32][(token_id u32, count u32, [(row u32, tf u32)]*count)*]
//! [doc_lens_len u32][(row u32, len u32)*]
//! [str_len u32][(term_len u32, term, count u32, [row u32]*count)*]   // 降级模式
//! ```

use crate::common::tokenizer::{Tokenizer, UNKNOWN_ID};

pub const TOKEN_INV_MAGIC: u32 = 0x54494e56; // "TINV"

/// Token 级倒排索引（行级 postings）
#[derive(Debug, Clone)]
pub struct TokenInvertedIndex {
    /// token_id → (row_id, tf)，row_id 严格递增
    postings: fxhash::FxHashMap<u32, Vec<(u32, u32)>>,
    /// row_id → token 总数（BM25 doc_len；含重复）
    doc_lens: Vec<u32>,
    n_docs: u32,
    /// 词表版本（None = 字符串降级模式；查询/加载时版本不符须重建）
    vocab_version: Option<u16>,
    /// 降级模式 postings（term → rows）
    string_postings: fxhash::FxHashMap<String, Vec<u32>>,
}

impl TokenInvertedIndex {
    pub fn new() -> Self {
        Self {
            postings: fxhash::FxHashMap::default(),
            doc_lens: Vec::new(),
            n_docs: 0,
            vocab_version: None,
            string_postings: fxhash::FxHashMap::default(),
        }
    }

    pub fn with_vocab(version: u16) -> Self {
        let mut s = Self::new();
        s.vocab_version = Some(version);
        s
    }

    pub fn clear(&mut self) {
        self.postings.clear();
        self.doc_lens.clear();
        self.n_docs = 0;
        self.string_postings.clear();
    }

    pub fn n_docs(&self) -> u32 {
        self.n_docs
    }

    /// 当前模式版本（None = 字符串降级）
    pub fn vocab_version(&self) -> Option<u16> {
        self.vocab_version
    }

    /// 词表版本不符 → 索引须重建（旧 id 空间不可用）
    pub fn requires_rebuild(&self, tok: &Tokenizer) -> bool {
        self.vocab_version.map_or(true, |v| v != tok.version())
    }

    /// 添加文档。tok = None → 字符串降级模式。
    pub fn add_document(&mut self, row_id: u32, text: &str, tok: Option<&Tokenizer>) {
        if row_id as usize >= self.doc_lens.len() {
            self.doc_lens.resize(row_id as usize + 1, 0);
        }
        match tok {
            Some(tok) => {
                let mut tf: fxhash::FxHashMap<u32, u32> = fxhash::FxHashMap::default();
                for t in tok.tokenize(text) {
                    if t.id != UNKNOWN_ID {
                        *tf.entry(t.id).or_insert(0) += 1;
                    }
                }
                let mut pairs: Vec<(u32, u32)> = tf.into_iter().collect();
                pairs.sort_by_key(|(id, _)| *id);
                let mut n = 0u32;
                for (id, count) in &pairs {
                    self.postings.entry(*id).or_default().push((row_id, *count));
                    // 空白 token（空格/制表/换行）不计入文档长度（BM25 惯例：
                    // 文档长度应按有效词计，避免标点/空格惩罚长文本）
                    if !is_ws_token(tok, *id) {
                        n += *count;
                    }
                }
                self.doc_lens[row_id as usize] = n;
                self.vocab_version = Some(tok.version());
            }
            None => {
                for term in tokenize_string(text) {
                    let entry = self.string_postings.entry(term).or_default();
                    if entry.last() != Some(&row_id) {
                        entry.push(row_id);
                    }
                }
            }
        }
        if row_id >= self.n_docs {
            self.n_docs = row_id + 1;
        }
    }

    /// 删除文档（需原文重算 token）
    pub fn remove_document(&mut self, row_id: u32, text: &str, tok: Option<&Tokenizer>) {
        match tok {
            Some(tok) => {
                let mut ids: Vec<u32> = Vec::new();
                for t in tok.tokenize(text) {
                    if t.id != UNKNOWN_ID && !ids.contains(&t.id) {
                        ids.push(t.id);
                    }
                }
                for id in ids {
                    if let Some(p) = self.postings.get_mut(&id) {
                        p.retain(|(r, _)| *r != row_id);
                        if p.is_empty() {
                            self.postings.remove(&id);
                        }
                    }
                }
            }
            None => {
                for term in tokenize_string(text) {
                    if let Some(entry) = self.string_postings.get_mut(&term) {
                        entry.retain(|&r| r != row_id);
                        if entry.is_empty() {
                            self.string_postings.remove(&term);
                        }
                    }
                }
            }
        }
        if row_id as usize == self.n_docs as usize - 1 {
            // 末行删除可收缩；中间行保持（row_id 空间不重排）
        }
    }

    /// 单 token 召回（词表 id）
    pub fn search_term(&self, id: u32) -> Vec<u32> {
        self.postings
            .get(&id)
            .map(|p| p.iter().map(|(r, _)| *r).collect())
            .unwrap_or_default()
    }

    /// AND 交集（最小 postings 驱动，两侧均有序 → 二分/游标合并）
    pub fn search_and(&self, query_ids: &[u32]) -> Vec<u32> {
        if query_ids.is_empty() {
            return Vec::new();
        }
        // 最小集合驱动：先把各 term 的 postings 取出，从最小的开始交集
        let mut lists: Vec<Vec<u32>> = Vec::with_capacity(query_ids.len());
        for id in query_ids {
            let l = self.search_term(*id);
            if l.is_empty() {
                return Vec::new(); // 缺词 → AND 为空
            }
            lists.push(l);
        }
        lists.sort_by_key(Vec::len);
        let mut result = lists[0].clone();
        for list in &lists[1..] {
            result = intersect(&result, list);
            if result.is_empty() {
                break;
            }
        }
        result
    }

    /// OR 并集（结果有序去重）
    pub fn search_or(&self, query_ids: &[u32]) -> Vec<u32> {
        let mut result: Vec<u32> = Vec::new();
        for id in query_ids {
            result.extend(self.search_term(*id));
        }
        result.sort_unstable();
        result.dedup();
        result
    }

    /// 统一查询入口：有 tokenizer → id 路径；否则字符串降级
    pub fn search(&self, query: &str, tok: Option<&Tokenizer>) -> Vec<u32> {
        match tok {
            Some(tok) if self.vocab_version.is_some() => {
                let ids: Vec<u32> = tok
                    .tokenize(query)
                    .iter()
                    .filter(|t| t.id != UNKNOWN_ID && !is_ws_token(tok, t.id))
                    .map(|t| t.id)
                    .collect();
                self.search_and(&ids)
            }
            _ => {
                let terms = tokenize_string(query);
                if terms.is_empty() {
                    return Vec::new();
                }
                let mut result = self.string_postings.get(&terms[0]).cloned().unwrap_or_default();
                for term in &terms[1..] {
                    let l = self.string_postings.get(term).cloned().unwrap_or_default();
                    result = intersect(&result, &l);
                    if result.is_empty() {
                        break;
                    }
                }
                result
            }
        }
    }

    /// 词表 id 模式 postings（只读）
    pub fn postings(&self) -> &fxhash::FxHashMap<u32, Vec<(u32, u32)>> {
        &self.postings
    }

    /// row_id → token 数（BM25 doc_len）
    pub fn doc_len(&self, row_id: u32) -> u32 {
        self.doc_lens.get(row_id as usize).copied().unwrap_or(0)
    }

    /// 平均 doc_len（BM25 分母；n_docs=0 → 1 防除零）
    pub fn avg_doc_len(&self) -> f32 {
        if self.n_docs == 0 {
            return 1.0;
        }
        let sum: u64 = self.doc_lens.iter().map(|l| *l as u64).sum();
        (sum as f32 / self.n_docs as f32).max(1.0)
    }

    pub fn size_stats(&self) -> (usize, usize) {
        let entries: usize = self.postings.values().map(|p| p.len()).sum();
        let str_entries: usize = self.string_postings.values().map(|p| p.len()).sum();
        (entries + str_entries, self.postings.len() + self.string_postings.len())
    }

    // ------------------------------------------------------------------
    // 序列化（修复现状「FTS 索引不落盘」缺陷；词表版本不符 → 惰性重建）
    // ------------------------------------------------------------------

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&TOKEN_INV_MAGIC.to_le_bytes());
        let ver: i16 = self.vocab_version.map(|v| v as i16).unwrap_or(-1);
        buf.extend_from_slice(&ver.to_le_bytes());
        buf.extend_from_slice(&self.n_docs.to_le_bytes());
        buf.extend_from_slice(&(self.postings.len() as u32).to_le_bytes());
        for (id, pairs) in &self.postings {
            buf.extend_from_slice(&id.to_le_bytes());
            buf.extend_from_slice(&(pairs.len() as u32).to_le_bytes());
            for (r, tf) in pairs {
                buf.extend_from_slice(&r.to_le_bytes());
                buf.extend_from_slice(&tf.to_le_bytes());
            }
        }
        buf.extend_from_slice(&(self.doc_lens.len() as u32).to_le_bytes());
        for (r, l) in self.doc_lens.iter().enumerate() {
            buf.extend_from_slice(&(r as u32).to_le_bytes());
            buf.extend_from_slice(&l.to_le_bytes());
        }
        buf.extend_from_slice(&(self.string_postings.len() as u32).to_le_bytes());
        for (term, rows) in &self.string_postings {
            buf.extend_from_slice(&(term.len() as u32).to_le_bytes());
            buf.extend_from_slice(term.as_bytes());
            buf.extend_from_slice(&(rows.len() as u32).to_le_bytes());
            for r in rows {
                buf.extend_from_slice(&r.to_le_bytes());
            }
        }
        buf
    }

    pub fn from_bytes(data: &[u8]) -> Result<Self, String> {
        let mut pos = 0usize;
        let rd = |pos: &mut usize, n: usize, what: &str| -> Result<&[u8], String> {
            if *pos + n > data.len() {
                return Err(format!("token inverted: truncated {what}"));
            }
            let s = &data[*pos..*pos + n];
            *pos += n;
            Ok(s)
        };
        let u32_at = |p: &[u8]| u32::from_le_bytes(p.try_into().unwrap());
        let i16_at = |p: &[u8]| i16::from_le_bytes(p.try_into().unwrap());

        let magic = u32_at(rd(&mut pos, 4, "magic")?);
        if magic != TOKEN_INV_MAGIC {
            return Err("token inverted: bad magic".into());
        }
        let ver = i16_at(rd(&mut pos, 2, "version")?);
        let n_docs = u32_at(rd(&mut pos, 4, "n_docs")?);
        let mut idx = TokenInvertedIndex::new();
        idx.n_docs = n_docs;
        idx.vocab_version = if ver < 0 { None } else { Some(ver as u16) };

        let n_postings = u32_at(rd(&mut pos, 4, "postings count")?) as usize;
        for _ in 0..n_postings {
            let id = u32_at(rd(&mut pos, 4, "token id")?);
            let count = u32_at(rd(&mut pos, 4, "posting count")?) as usize;
            let mut pairs = Vec::with_capacity(count);
            for _ in 0..count {
                let r = u32_at(rd(&mut pos, 4, "row")?);
                let tf = u32_at(rd(&mut pos, 4, "tf")?);
                pairs.push((r, tf));
            }
            idx.postings.insert(id, pairs);
        }
        let n_lens = u32_at(rd(&mut pos, 4, "doc lens count")?) as usize;
        let mut dl: Vec<(u32, u32)> = Vec::with_capacity(n_lens);
        for _ in 0..n_lens {
            let r = u32_at(rd(&mut pos, 4, "doc row")?);
            let l = u32_at(rd(&mut pos, 4, "doc len")?);
            dl.push((r, l));
        }
        dl.sort_by_key(|(r, _)| *r);
        if let Some(&(last, _)) = dl.last() {
            idx.doc_lens = vec![0u32; last as usize + 1];
            for (r, l) in dl {
                idx.doc_lens[r as usize] = l;
            }
        }
        let n_str = u32_at(rd(&mut pos, 4, "string postings count")?) as usize;
        for _ in 0..n_str {
            let tlen = u32_at(rd(&mut pos, 4, "term len")?) as usize;
            let term = String::from_utf8_lossy(rd(&mut pos, tlen, "term")?).into_owned();
            let count = u32_at(rd(&mut pos, 4, "string count")?) as usize;
            let mut rows = Vec::with_capacity(count);
            for _ in 0..count {
                rows.push(u32_at(rd(&mut pos, 4, "string row")?));
            }
            idx.string_postings.insert(term, rows);
        }
        Ok(idx)
    }
}

/// 空白 token 判定（词表内全空白字符的 id——BM25 文档长度/查询均排除）
fn is_ws_token(tok: &Tokenizer, id: u32) -> bool {
    tok.id_to_token(id).map_or(false, |t| !t.is_empty() && t.chars().all(char::is_whitespace))
}

/// 字符串降级分词（原 InvertedIndex::tokenize 语义）
fn tokenize_string(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_alphanumeric() || ch == '_' || ch == '-' {
            current.push(ch);
        } else {
            if !current.is_empty() {
                tokens.push(current.to_lowercase());
                current.clear();
            }
        }
    }
    if !current.is_empty() {
        tokens.push(current.to_lowercase());
    }
    tokens
}

/// 两个有序列表交集（游标法）
fn intersect(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut out = Vec::new();
    let mut i = 0usize;
    let mut j = 0usize;
    while i < a.len() && j < b.len() {
        match a[i].cmp(&b[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                out.push(a[i]);
                i += 1;
                j += 1;
            }
        }
    }
    out
}

impl Default for TokenInvertedIndex {
    fn default() -> Self {
        Self::new()
    }
}
