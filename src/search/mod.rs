//! 检索引擎（v0.21「TokenDelta 引擎」= Tokenizer 词表空间的 sparse 检索层）
//!
//! 与 TD 压缩同源同表：同一 Tokenizer / 词表 id 空间 / 同一 token 流；压缩 codec 解耦。
//! - `sparse`：TokenInvertedIndex（token_id → (row, tf) 行级 postings）
//! - `bm25`：BM25 排序检索
//! - `fuzzy`：token 序列级模糊匹配（编辑距离 / n-gram）
//! - `hybrid`：RRF 混合（sparse + HNSW dense）

pub mod bm25;
pub mod fuzzy;
pub mod hybrid;
pub mod sparse;

pub use bm25::{query_ids, search as search_bm25, Bm25Params};
pub use fuzzy::{search_edit, search_ngram};
pub use hybrid::rrf;
pub use sparse::TokenInvertedIndex;
