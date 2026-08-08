//! BM25 检索打分（v0.21 检索层）
//!
//! 基于 TokenInvertedIndex 的 token 频率（tf/idf）。参数 k1=1.2、b=0.75（经典默认）。
//! query → tokenize → 词表 id 集合 → 候选召回（AND 精确 + OR 宽召回两档）→ 打分排序。

use super::sparse::TokenInvertedIndex;
use crate::common::tokenizer::{Tokenizer, UNKNOWN_ID};

/// BM25 参数
#[derive(Debug, Clone, Copy)]
pub struct Bm25Params {
    pub k1: f32,
    pub b: f32,
}

impl Default for Bm25Params {
    fn default() -> Self {
        Self { k1: 1.2, b: 0.75 }
    }
}

/// 查询 token id 序列（去重、过滤 UNKNOWN/空白 token）
pub fn query_ids(tok: &Tokenizer, query: &str) -> Vec<u32> {
    let mut ids: Vec<u32> = Vec::new();
    for t in tok.tokenize(query) {
        if t.id != UNKNOWN_ID
            && !ids.contains(&t.id)
            && !tok.id_to_token(t.id).map_or(false, |s| !s.is_empty() && s.chars().all(char::is_whitespace))
        {
            ids.push(t.id);
        }
    }
    ids
}

/// BM25 检索 top-k，返回 (row_id, score) 降序。
///
/// 召回策略：query 词 AND 精确（同 FTS 语义）为空时退 OR 宽召回，避免零结果。
pub fn search(
    idx: &TokenInvertedIndex,
    tok: &Tokenizer,
    query: &str,
    k: usize,
    params: &Bm25Params,
) -> Vec<(u32, f32)> {
    let ids = query_ids(tok, query);
    if ids.is_empty() {
        return Vec::new();
    }
    // 候选召回：OR 宽召回（AND 集合的子集是 AND 的候选——直接 OR 保底）
    let candidates = idx.search_or(&ids);
    if candidates.is_empty() {
        return Vec::new();
    }

    let n_docs = idx.n_docs() as f32;
    let avg_dl = idx.avg_doc_len();
    let k1 = params.k1;
    let b = params.b;

    // 预取各 term 的 postings（v0.21.2 紧凑存储 → term 级解码）+ idf
    let mut terms: Vec<(u32, Vec<(u32, u32)>, f32)> = Vec::with_capacity(ids.len());
    for id in &ids {
        let pairs = idx.decoded_postings(*id);
        if pairs.is_empty() {
            continue;
        }
        let df = pairs.len() as f32;
        let idf = (1.0 + (n_docs - df + 0.5) / (df + 0.5)).ln();
        terms.push((*id, pairs, idf));
    }
    if terms.is_empty() {
        return Vec::new();
    }

    let mut scores: Vec<(u32, f32)> = Vec::with_capacity(candidates.len());
    for row in &candidates {
        let dl = idx.doc_len(*row).max(1) as f32;
        let norm = k1 * (1.0 - b + b * dl / avg_dl);
        let mut s = 0.0f32;
        for (_, p, idf) in &terms {
            // postings 按 row 递增 → 二分找本行 tf
            let tf = match p.binary_search_by_key(row, |(r, _)| *r) {
                Ok(i) => p[i].1 as f32,
                Err(_) => continue,
            };
            s += idf * (tf * (k1 + 1.0)) / (tf + norm);
        }
        if s > 0.0 {
            scores.push((*row, s));
        }
    }
    scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scores.truncate(k);
    scores
}
