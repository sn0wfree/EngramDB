//! RRF 混合检索（v0.21 检索层；roadmap B4）
//!
//! sparse（BM25）+ dense（HNSW）两路 top-k → RRF 合并：
//! score(row) = Σ 1 / (k + rank_i(row))，k = 60（经典设定）。

/// RRF 合并（两路 top-k 有序列表；rank 从 1 起）
pub fn rrf(sparse: &[(u32, f32)], dense: &[(u32, f32)], k: usize) -> Vec<(u32, f32)> {
    let mut acc: fxhash::FxHashMap<u32, f32> = fxhash::FxHashMap::default();
    for (i, (row, _)) in sparse.iter().enumerate() {
        *acc.entry(*row).or_insert(0.0) += 1.0 / (k as f32 + i as f32 + 1.0);
    }
    for (i, (row, _)) in dense.iter().enumerate() {
        *acc.entry(*row).or_insert(0.0) += 1.0 / (k as f32 + i as f32 + 1.0);
    }
    let mut out: Vec<(u32, f32)> = acc.into_iter().collect();
    out.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    out
}
