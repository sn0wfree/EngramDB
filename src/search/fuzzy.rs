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

/// 候选召回上限（防御性：宽召回可能很大）
const CANDIDATE_LIMIT: usize = 512;

/// token 序列级 Levenshtein 距离（行向量，长度差 > max_dist 直接剪枝）
pub fn edit_distance(a: &[u32], b: &[u32], max_dist: usize) -> usize {
    let (la, lb) = (a.len(), b.len());
    let diff = la.abs_diff(lb);
    if diff > max_dist {
        return diff; // 必然超限
    }
    if la == 0 {
        return lb;
    }
    if lb == 0 {
        return la;
    }
    let mut prev: Vec<usize> = (0..=lb).collect();
    let mut cur = vec![0usize; lb + 1];
    for i in 1..=la {
        cur[0] = i;
        let mut row_min = cur[0];
        for j in 1..=lb {
            let cost = if a[i - 1] == b[j - 1] { 0 } else { 1 };
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
            row_min = row_min.min(cur[j]);
        }
        // 行最小已超限 → 可剪枝（后续行最小单调不减）
        if row_min > max_dist {
            return row_min;
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[lb]
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
            .tokenize(&text)
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
            .tokenize(&text)
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
