//! Token 序列级模糊匹配（v0.21 检索层）
//!
//! 双算法（API 可选）：
//! - `search_edit`：query/候选 token 序列 Levenshtein（行向量 + 长度差剪枝）
//!   + 共享前缀加成 → 相似度 = 1 - d/max(la,lb)
//! - `search_ngram`：token 级 n-gram 重叠（Jaccard）召回打分
//!
//! 两者都需候选行原文（token 序列）——由调用方提供 `get_text` 闭包
//! （table 层从列存解压读行）。

use super::bm25::query_ids;
use super::sparse::TokenInvertedIndex;
use crate::common::tokenizer::{Tokenizer, UNKNOWN_ID};

/// 候选召回上限（防御性：宽召回可能很大；模糊匹配对 top-k=10，256 候选富余）
const CANDIDATE_LIMIT: usize = 256;

/// 模糊匹配输入前缀窗口：只 tokenize 文档前缀（流式会话场景共享前缀即关键区；
/// 避免全文 tokenize 长文档拖慢模糊检索）
const MAX_TEXT_BYTES: usize = 1024;

/// 前缀窗口截断（字节安全：回退到字符边界）
fn prefix_window(text: &str) -> &str {
    if text.len() <= MAX_TEXT_BYTES {
        return text;
    }
    let mut end = MAX_TEXT_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// token 序列级 Levenshtein 距离（banded：只算 |i-j| <= max_dist 对角带，
/// 带外视为不可达——O(m × (2·max_dist)) 替代 O(m×n)）
pub fn edit_distance(a: &[u32], b: &[u32], max_dist: usize) -> usize {
    let m = a.len();
    let n = b.len();
    if m.abs_diff(n) > max_dist {
        return m.abs_diff(n);
    }
    if max_dist == 0 {
        return if a == b { 0 } else { 1 };
    }
    const INF: usize = usize::MAX / 4;
    // 第 0 行：dp[0][j] = j（空 a 到 b 前缀的距离，全有效边界）
    let mut prev: Vec<usize> = (0..=n).collect();
    let mut cur = vec![INF; n + 1];
    for i in 1..=m {
        cur[0] = i;
        let lo = 1usize.max(i.saturating_sub(max_dist));
        let hi = n.min(i + max_dist);
        for j in lo..=hi {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            cur[j] = prev[j].min(cur[j - 1]).min(prev[j - 1]).min(INF - 1) + cost;
        }
        // 清带外（上一行残余值不得泄漏到下一轮读取）
        for j in 0..=n {
            if j > hi + 1 || j + 1 < lo {
                cur[j] = INF;
            }
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[n]
}

/// token 序列 n-gram（有序对列表）
pub fn ngrams(ids: &[u32], n: usize) -> Vec<Vec<u32>> {
    if ids.len() < n {
        return Vec::new();
    }
    ids.windows(n).map(|w| w.to_vec()).collect()
}

/// n-gram 重叠率（Jaccard：交集 / 并集，空集 → 0）
pub fn ngram_overlap(a: &[Vec<u32>], b: &[Vec<u32>]) -> f32 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let set_b: fxhash::FxHashSet<&[u32]> = b.iter().map(|g| g.as_slice()).collect();
    let mut inter = 0usize;
    let mut seen: fxhash::FxHashSet<&[u32]> = fxhash::FxHashSet::default();
    for g in a {
        if !seen.insert(g.as_slice()) {
            continue;
        }
        if set_b.contains(g.as_slice()) {
            inter += 1;
        }
    }
    let union = seen.len() + set_b.len() - inter;
    if union == 0 {
        return 0.0;
    }
    inter as f32 / union as f32
}

/// 候选召回：query token 的 postings OR（上限 CANDIDATE_LIMIT）
pub fn candidate_rows(idx: &TokenInvertedIndex, ids: &[u32]) -> Vec<u32> {
    idx.search_or(ids)
        .into_iter()
        .take(CANDIDATE_LIMIT)
        .collect()
}

/// 编辑距离打分（纯函数）：相似度 ∈ [0,1]，前缀加成 0.1；剪枝返回 None
pub fn score_edit(query_ids: &[u32], doc_ids: &[u32], max_len: usize) -> Option<f32> {
    if doc_ids.is_empty() {
        return None;
    }
    let a: &[u32] = &query_ids[..query_ids.len().min(max_len)];
    let b: &[u32] = &doc_ids[..doc_ids.len().min(max_len)];
    let maxl = a.len().max(b.len());
    let max_dist = maxl / 2 + 1;
    let d = edit_distance(a, b, max_dist);
    if d > max_dist {
        return None;
    }
    let mut score = 1.0 - d as f32 / maxl.max(1) as f32;
    let shared = a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count();
    score += shared as f32 / a.len().max(1) as f32 * 0.1;
    Some(score.min(1.0))
}

/// n-gram 重叠打分（纯函数）：Jaccard
pub fn score_ngram(query_ids: &[u32], doc_ids: &[u32], gram: usize) -> f32 {
    ngram_overlap(&ngrams(query_ids, gram), &ngrams(doc_ids, gram))
}

/// 编辑距离模糊检索 top-k，返回 (row_id, score ∈ [0,1])。
///
/// - 候选：query token 的 postings OR（上限 CANDIDATE_LIMIT）
/// - 打分：相似度 = 1 - d/max(la,lb)，加共享前缀加成（前缀 token 占比 × 0.1）
/// - 剪枝：d > max(la,lb)/2 → 跳过
pub fn search_edit(
    idx: &TokenInvertedIndex,
    tok: &Tokenizer,
    query: &str,
    k: usize,
    get_text: &impl Fn(u32) -> Option<String>,
) -> Vec<(u32, f32)> {
    let ids = query_ids(tok, query);
    if ids.is_empty() {
        return Vec::new();
    }
    let max_score_len = 64usize;
    let mut out: Vec<(u32, f32)> = Vec::new();
    for row in candidate_rows(idx, &ids) {
        let Some(text) = get_text(row) else { continue };
        let doc_ids: Vec<u32> = tok
            .tokenize(prefix_window(&text))
            .iter()
            .filter(|t| t.id != UNKNOWN_ID && !tok.id_to_token(t.id).map_or(false, |s| !s.is_empty() && s.chars().all(char::is_whitespace)))
            .map(|t| t.id)
            .collect();
        if let Some(score) = score_edit(&ids, &doc_ids, max_score_len) {
            out.push((row, score));
        }
    }
    out.sort_by(|x, y| y.1.partial_cmp(&x.1).unwrap_or(std::cmp::Ordering::Equal));
    out.truncate(k);
    out
}

/// n-gram 模糊检索 top-k：Jaccard 重叠率打分
pub fn search_ngram(
    idx: &TokenInvertedIndex,
    tok: &Tokenizer,
    query: &str,
    k: usize,
    gram: usize,
    get_text: &impl Fn(u32) -> Option<String>,
) -> Vec<(u32, f32)> {
    let ids = query_ids(tok, query);
    if ids.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<(u32, f32)> = Vec::new();
    for row in candidate_rows(idx, &ids) {
        let Some(text) = get_text(row) else { continue };
        let doc_ids: Vec<u32> = tok
            .tokenize(prefix_window(&text))
            .iter()
            .filter(|t| t.id != UNKNOWN_ID && !tok.id_to_token(t.id).map_or(false, |s| !s.is_empty() && s.chars().all(char::is_whitespace)))
            .map(|t| t.id)
            .collect();
        let s = score_ngram(&ids, &doc_ids, gram);
        if s > 0.0 {
            out.push((row, s));
        }
    }
    out.sort_by(|x, y| y.1.partial_cmp(&x.1).unwrap_or(std::cmp::Ordering::Equal));
    out.truncate(k);
    out
}
