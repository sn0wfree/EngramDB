//! 运行时统一编码器（v0.21 统一 Tokenizer，路线 B 运行端）
//!
//! 与离线训练端（examples/train_vocab.rs，tokenizers）**算法同构**：
//! 1. 共享预分割（src/common/pretokenize.rs 类别段 + 种子词）
//! 2. 段内字符级初始 token（未登录字符 byte_fallback `<0xXX>`）
//! 3. **按 merges rank 贪心合并**（最小 rank pair 优先，堆实现——
//!    与 tokenizers `Word::merge_all`、tiktoken `byte_pair_merge` 同款算法）
//!
//! 正确性由差分测试锁定（见 tests/tokenizer_diff.rs）：
//! 同一词表 + 同一预分割 → tokenizers 编码 vs 本编码器逐 token 一致。
//!
//! 零 C 依赖、零外部压缩库；text 可逆（offset 切片），norm 视图独立（FTS 用）。

use std::collections::BinaryHeap;
use std::ops::Range;
use std::sync::Arc;

use fxhash::FxHashMap;

use crate::common::error::Result;
use crate::common::error::EngramDbError;
use crate::common::pretokenize::{self, CharClass};
use crate::common::vocab_file::VocabFile;

/// 单个 token 的编码结果：id（rank）+ 原文字节区间
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub id: u32,
    pub offset: Range<usize>,
}

/// 未登录字符标记（Unicode 字符级动态兜底）：
/// 字符不在词表时输出 `UNKNOWN_ID`，text 从 offset 切片原文获得——
/// 由 TokenDelta 块级动态字典登记（字符文本 → 动态短 ID），保证可逆与检索粒度。
pub const UNKNOWN_ID: u32 = u32::MAX;

/// 统一 Tokenizer（运行时编码器）
pub struct Tokenizer {
    /// token 文本 → id（rank）
    vocab: FxHashMap<String, u32>,
    /// (left_id, right_id) → (rank, merged_id)——merges 按训练顺序
    merges: FxHashMap<(u32, u32), (u32, u32)>,
    /// merges 有序列表（rank 序，静态热词来源）
    merges_ordered: Vec<(String, String)>,
    /// 种子词（jieba 风格，可空）
    seeds: Vec<String>,
    /// id → token 文本（按 id 序，解码用）
    vocab_by_id: Vec<String>,
    /// TokenDelta Static 模式 per-id 码长表（词表 v2 字段，空 = 未生成）
    static_lengths: Vec<u8>,
    /// 词表版本（块头校验：词表不匹配的块不可解压）
    version: u16,
    /// Static 模式熵编码缓存（码长表 → canonical codes + HuffmanTable，含逃逸符号；
    /// 一次构建跨块复用——C 场景 1113 块 × 31756 符号全表重建的根因）
    static_entropy: std::sync::OnceLock<(
        FxHashMap<u32, crate::common::huffman::Code>,
        crate::common::huffman::HuffmanTable,
    )>,
    /// 词表文件字节（自包含，供块头引用/审计）
    _source_len: usize,
}

/// 内部符号（双向链表节点）
#[derive(Clone, Copy)]
struct Symbol {
    id: u32,
    byte_len: usize,
    prev: usize,
    next: usize,
    active: bool,
}

const NONE: usize = usize::MAX;

impl Tokenizer {
    /// 从词表文件字节加载（include_bytes! 或外部文件）
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let vf = VocabFile::from_bytes(bytes)
            .map_err(|e| EngramDbError::Parse(format!("vocab deserialize: {e}")))?;
        Self::from_vocab_file(vf)
    }

    pub fn from_vocab_file(vf: VocabFile) -> Result<Self> {
        if &vf.magic != &crate::common::vocab_file::VOCAB_MAGIC {
            return Err(EngramDbError::Parse("vocab magic mismatch".into()));
        }
        let mut vocab: FxHashMap<String, u32> = FxHashMap::default();
        let mut vocab_by_id: Vec<String> = Vec::with_capacity(vf.vocab.len());
        for (id, t) in vf.vocab.iter().enumerate() {
            vocab.insert(t.clone(), id as u32);
            vocab_by_id.push(t.clone());
        }
        // merges pair → (rank, merged_id)：merged token 文本 = a + b
        let mut merges: FxHashMap<(u32, u32), (u32, u32)> = FxHashMap::default();
        for (rank, (a, b)) in vf.merges.iter().enumerate() {
            let Some(&aid) = vocab.get(a.as_str()) else { continue };
            let Some(&bid) = vocab.get(b.as_str()) else { continue };
            let merged_text = format!("{a}{b}");
            let Some(&mid) = vocab.get(merged_text.as_str()) else { continue };
            merges.insert((aid, bid), (rank as u32, mid));
        }
        Ok(Self {
            vocab,
            merges,
            merges_ordered: vf.merges,
            seeds: vf.seeds,
            vocab_by_id,
            static_lengths: vf.static_lengths,
            version: vf.version,
            static_entropy: std::sync::OnceLock::new(),
            _source_len: 0,
        })
    }

    /// 词表大小
    pub fn vocab_size(&self) -> usize {
        self.vocab.len()
    }

    /// 词表版本（块头写入/校验用）
    pub fn version(&self) -> u16 {
        self.version
    }

    /// TokenDelta Static 模式 per-id 码长表（空 = 未生成，Static 退化）
    pub fn static_lengths(&self) -> &[u8] {
        &self.static_lengths
    }

    /// Static 模式熵编码（一次构建缓存）：canonical codes + 解码表，含逃逸符号。
    /// `escape_id` 为行内逃逸标记（TokenDelta 扩展 id 走 varint 逃逸流，不进表）。
    pub fn static_entropy(
        &self,
        escape_id: u32,
    ) -> &(
        FxHashMap<u32, crate::common::huffman::Code>,
        crate::common::huffman::HuffmanTable,
    ) {
        self.static_entropy.get_or_init(|| {
            let base = &self.static_lengths;
            let mut lengths: Vec<(u32, u8)> = base
                .iter()
                .enumerate()
                .filter(|(_, l)| **l > 0)
                .map(|(id, l)| (id as u32, *l))
                .collect();
            lengths.push((escape_id, 24));
            let codes = crate::common::huffman::canonical_codes(&lengths);
            let table = crate::common::huffman::HuffmanTable::from_lengths(&lengths);
            (codes, table)
        })
    }

    /// 文本是否在词表中（未登录检测，供测试/审计使用）
    pub fn is_in_vocab(&self, text: &str) -> bool {
        self.vocab.contains_key(text)
    }

    /// id → token 文本（解码还原用；id 需为词表内 id）
    pub fn id_to_token(&self, id: u32) -> Option<&str> {
        self.vocab_by_id.get(id as usize).map(|s| s.as_str())
    }

    /// token 文本 → id
    pub fn token_to_id(&self, token: &str) -> Option<u32> {
        self.vocab.get(token).copied()
    }

    pub fn seeds(&self) -> &[String] {
        &self.seeds
    }

    /// 静态热词：merges 前 TOP_N 个产物 id（TokenDelta 静态层）
    pub fn hot_words(&self, top_n: usize) -> Vec<u32> {
        let mut out = Vec::new();
        for (a, b) in self.merges_ordered.iter().take(top_n) {
            let Some(&aid) = self.vocab.get(a.as_str()) else { continue };
            let Some(&bid) = self.vocab.get(b.as_str()) else { continue };
            if let Some(&(_, mid)) = self.merges.get(&(aid, bid)) {
                if !out.contains(&mid) {
                    out.push(mid);
                }
            }
        }
        out
    }

    /// 编码整段文本 → token 流（id + offset）
    pub fn tokenize(&self, text: &str) -> Vec<Token> {
        let mut tokens = Vec::new();
        for (word, piece) in pretokenize::segment_words(text, &self.seeds) {
            self.encode_word(&word, piece.start, &mut tokens);
        }
        tokens
    }

    /// 增量编码：`new_text` 以 `prev_text` 为前缀（流式追加）时复用 `prev_tokens`，
    /// 仅重放跨界段（公共前缀边界所在词块）与新增尾部。
    /// 跨界合并正确性：BPE merge 词级独立，重放段内结果与全量 tokenize 一致。
    /// 非前缀关系 / 文本变短 → 等价全量（变短时直接截断）。
    pub fn tokenize_incremental(
        &self,
        prev_text: &str,
        prev_tokens: &[Token],
        new_text: &str,
    ) -> Vec<Token> {
        let pbytes = prev_text.as_bytes();
        let nbytes = new_text.as_bytes();
        let mut p = 0usize;
        while p < pbytes.len() && p < nbytes.len() && pbytes[p] == nbytes[p] {
            p += 1;
        }
        // 回退到字符边界（多字节字符内部不同 → 取该字符起点）
        while p > 0 && !prev_text.is_char_boundary(p) {
            p -= 1;
        }
        let k = prev_tokens.iter().take_while(|t| t.offset.end <= p).count();
        if p == new_text.len() {
            // new 是 prev 的前缀（文本变短）→ 截断（跨界 token 被丢弃）
            return prev_tokens[..k].to_vec();
        }
        if p == 0 || p < 8 {
            return self.tokenize(new_text); // 无共享前缀 / 前缀过短 → 全量
        }
        // 公共前缀落在某段（类别段，含段内种子词切分）中间或 prev 最后段尾 →
        // 该段需整体重放：截断段的 word 划分 ≠ 完整段的划分，段完整重放才与
        // 全量 tokenize 逐 token 一致。p 恰在非末段段边界 → 从 p 起重放下段。
        let mut seg_start = p;
        let pieces = pretokenize::segment(prev_text);
        for (i, piece) in pieces.iter().enumerate() {
            if piece.end >= p {
                if piece.end == p && i + 1 < pieces.len() {
                    seg_start = p;
                } else {
                    seg_start = piece.start;
                }
                break;
            }
        }
        // 复用率 < 50% 时增量无收益（重放近全量 + 段扫描开销）→ 全量
        if seg_start * 2 < new_text.len() {
            return self.tokenize(new_text);
        }
        // prev 保留到重放段起点之前；重放段内 merge 可能吞噬边界 token，
        // 故整段重放并全部保留（与 prev 保留部分无缝拼接）
        let k2 = prev_tokens.iter().take_while(|t| t.offset.end <= seg_start).count();
        let mut tokens = prev_tokens[..k2].to_vec();
        for t in self.tokenize(&new_text[seg_start..]) {
            tokens.push(Token { id: t.id, offset: (t.offset.start + seg_start)..(t.offset.end + seg_start) });
        }
        tokens
    }

    /// 编码单个段（word）：字符级初始 token → merges rank 贪心合并
    /// 未登录字符 → `UNKNOWN_ID` 标记（Unicode 字符级兜底，块动态字典登记）
    fn encode_word(&self, word: &str, base: usize, out: &mut Vec<Token>) {
        // --- 初始符号：字符级（未登录 → UNKNOWN_ID 标记） ---
        let mut symbols: Vec<Symbol> = Vec::new();
        for c in word.chars() {
            let len = c.len_utf8();
            let id = match self.vocab.get(&c.to_string()) {
                Some(&id) => id,
                None => UNKNOWN_ID,
            };
            let idx = symbols.len();
            symbols.push(Symbol {
                id,
                byte_len: len,
                prev: if idx == 0 { NONE } else { idx - 1 },
                next: NONE,
                active: true,
            });
            if idx > 0 {
                symbols[idx - 1].next = idx;
            }
        }
        if symbols.is_empty() {
            return;
        }

        // --- 初始堆：所有相邻 pair（在 merges 中）按 rank 入堆 ---
        // 条目携带 (rank, new_id)：pop 时校验 pair 未变（对齐 tokenizers merge_all）
        let mut heap: BinaryHeap<std::cmp::Reverse<(u32, u32, usize)>> = BinaryHeap::new();
        for i in 0..symbols.len() {
            if let Some((rank, new_id)) = self.pair_rank(&symbols, i) {
                heap.push(std::cmp::Reverse((rank, new_id, i)));
            }
        }

        // --- 贪心合并：最小 rank pair 优先 ---
        while let Some(std::cmp::Reverse((rank, exp_new, pos))) = heap.pop() {
            if !symbols[pos].active || symbols[pos].next == NONE {
                continue;
            }
            let right = symbols[pos].next;
            if !symbols[right].active {
                continue;
            }
            let pair = (symbols[pos].id, symbols[right].id);
            let Some((cur_rank, new_id)) = self.merges.get(&pair).copied() else {
                continue; // 过期条目
            };
            // 校验条目：当前 pair 的 rank/new_id 必须与入堆时一致，
            // 否则是「pair 已变化」的过期条目（如 (a,s) 顺延成 (a,st)）——跳过
            if cur_rank != rank || new_id != exp_new {
                continue;
            }
            // 合并：pos ← new_id，吞噬 right
            symbols[pos].id = new_id;
            symbols[pos].byte_len += symbols[right].byte_len;
            symbols[pos].next = symbols[right].next;
            symbols[right].active = false;
            if symbols[pos].next != NONE {
                let n = symbols[pos].next;
                symbols[n].prev = pos;
            }
            // 新邻接 pair 入堆
            let p = symbols[pos].prev;
            if p != NONE && symbols[p].active {
                if let Some((rank, new_id)) = self.pair_rank(&symbols, p) {
                    heap.push(std::cmp::Reverse((rank, new_id, p)));
                }
            }
            if let Some((rank, new_id)) = self.pair_rank(&symbols, pos) {
                heap.push(std::cmp::Reverse((rank, new_id, pos)));
            }
        }

        // --- 输出：按序收集，字节偏移推进 ---
        let mut offset = base;
        let mut i = 0usize;
        while i < symbols.len() {
            if symbols[i].active {
                let byte_len = symbols[i].byte_len;
                out.push(Token { id: symbols[i].id, offset: offset..offset + byte_len });
                offset += byte_len;
            }
            i += 1;
        }
    }

    fn pair_rank(&self, symbols: &[Symbol], pos: usize) -> Option<(u32, u32)> {
        if !symbols[pos].active || symbols[pos].next == NONE {
            return None;
        }
        let right = symbols[pos].next;
        if !symbols[right].active {
            return None;
        }
        self.merges.get(&(symbols[pos].id, symbols[right].id)).copied()
    }

    /// 还原原文（可逆性验证）：按 offset 从 text 切片拼接 token 文本
    pub fn reconstruct<'a>(&self, text: &'a str, tokens: &[Token]) -> String {
        let mut out = String::with_capacity(text.len());
        for t in tokens {
            if t.offset.end <= text.len() {
                out.push_str(&text[t.offset.clone()]);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn smoke_tokenizer() -> Tokenizer {
        // 词表必须与预分割规则自洽：merge 仅限段内 pair
        // （「！」是 Punct 独立段，与 CJK 段不合并；段内单字符全覆盖保证可逆）
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
        Tokenizer::from_vocab_file(vf).unwrap()
    }

    #[test]
    fn test_roundtrip_reconstruct() {
        let tok = smoke_tokenizer();
        let text = "你好世界！hello world";
        let tokens = tok.tokenize(text);
        let recon = tok.reconstruct(text, &tokens);
        assert_eq!(recon, text);
    }

    #[test]
    fn test_deterministic() {
        let tok = smoke_tokenizer();
        let text = "你好世界！这是测试 text 123";
        let a = tok.tokenize(text);
        let b = tok.tokenize(text);
        assert_eq!(a, b);
    }

    #[test]
    fn test_incremental_equals_full() {
        let tok = smoke_tokenizer();
        // 流式前缀序列：每步增量 tokenize 必须与全量一致
        let base = "你好世界！这是测试 text 123 hello";
        let mut prev = String::new();
        let mut prev_tokens: Vec<Token> = Vec::new();
        for i in 1..=base.chars().count() {
            let end = base
                .char_indices()
                .nth(i)
                .map(|(idx, _)| idx)
                .unwrap_or(base.len());
            let next = base[..end].to_string();
            let full = tok.tokenize(&next);
            let inc = tok.tokenize_incremental(&prev, &prev_tokens, &next);
            assert_eq!(inc, full, "step {i}");
            prev = next;
            prev_tokens = inc;
        }
    }

    #[test]
    fn test_incremental_prefix_shrink() {
        let tok = smoke_tokenizer();
        let full = "你好世界！这是测试 text 123 hello";
        let tokens = tok.tokenize(full);
        let mid = "你好世界！这是测试";
        let inc = tok.tokenize_incremental(full, &tokens, mid);
        assert_eq!(inc, tok.tokenize(mid));
    }

    #[test]
    fn test_incremental_no_shared_prefix() {
        let tok = smoke_tokenizer();
        let tokens = tok.tokenize("你好世界");
        let other = "hello world 123";
        let inc = tok.tokenize_incremental("你好世界", &tokens, other);
        assert_eq!(inc, tok.tokenize(other));
    }

    #[test]
    fn test_merge_behavior() {
        let tok = smoke_tokenizer();
        let text = "你好世界！";
        let tokens = tok.tokenize(text);
        // 段内合并：你好(rank0)、世界(rank1)；「！」Punct 独立段不跨段合并
        let joined: String = tokens
            .iter()
            .map(|t| &text[t.offset.clone()])
            .collect::<Vec<_>>()
            .join("|");
        assert_eq!(joined, "你好|世界|！");
    }

    #[test]
    fn test_unknown_char_marked_and_reversible() {
        // Unicode 字符级兜底：未登录字符 → UNKNOWN_ID 标记（非丢弃），
        // offset 保留原文 → 可逆性完整
        let tok = smoke_tokenizer();
        let text = "你好😀世界";
        let tokens = tok.tokenize(text);
        let unknown: Vec<bool> = tokens.iter().map(|t| t.id == UNKNOWN_ID).collect();
        assert!(unknown.iter().any(|&b| b), "未登录字符应标记");
        assert_eq!(tokens.iter().filter(|t| t.id == UNKNOWN_ID).count(), 1);
        let recon = tok.reconstruct(text, &tokens);
        assert_eq!(recon, text, "未登录字符标记后仍可逆");
        // 其余 token 正常
        let joined: String = tokens
            .iter()
            .filter(|t| t.id != UNKNOWN_ID)
            .map(|t| &text[t.offset.clone()])
            .collect::<Vec<_>>()
            .join("|");
        assert_eq!(joined, "你好|世界");
    }

    #[test]
    fn test_hot_words() {
        let tok = smoke_tokenizer();
        let hot = tok.hot_words(2);
        // merges 前 2 个产物：你好、世界
        assert_eq!(hot.len(), 2);
        assert_eq!(tok.vocab_size(), 15);
    }
}
