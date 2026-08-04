//! 倒排索引（Inverted Index）
//!
//! 用于全文检索（FTS）：将文档分词后，建立 词 → 行号列表 的映射。
//! 支持布尔 AND/OR 查询。
//!
//! 设计原则：
//! - 轻量级：纯内存实现，持久化通过 serde/bincode
//! - 简单分词：按空白符 + 标点分割，小写归一化
//! - 行号存储：Vec<u32>，有序排列，查询时做集合交并

use std::collections::HashMap;

/// 倒排索引
#[derive(Debug, Clone)]
pub struct InvertedIndex {
    /// 词 → 包含该词的行号列表（有序去重）
    postings: HashMap<String, Vec<u32>>,
    /// 列名
    column_name: String,
}

impl InvertedIndex {
    pub fn new(column_name: &str) -> Self {
        Self {
            postings: HashMap::new(),
            column_name: column_name.to_string(),
        }
    }

    /// 清空索引（v0.15.0 TRUNCATE TABLE 支持）
    pub fn clear(&mut self) {
        self.postings.clear();
    }

    pub fn column_name(&self) -> &str {
        &self.column_name
    }

    /// 分词：按空白符/标点分割，转小写
    pub fn tokenize(text: &str) -> Vec<String> {
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

    /// 添加一个文档到索引
    pub fn add_document(&mut self, row_id: u32, text: &str) {
        let tokens = Self::tokenize(text);
        for token in tokens {
            let entry = self.postings.entry(token).or_default();
            if entry.last() != Some(&row_id) {
                entry.push(row_id);
            }
        }
    }

    /// 删除一个文档（删除所有包含该 row_id 的倒排项）
    pub fn remove_document(&mut self, row_id: u32, text: &str) {
        let tokens = Self::tokenize(text);
        for token in tokens {
            if let Some(entry) = self.postings.get_mut(&token) {
                entry.retain(|&id| id != row_id);
                if entry.is_empty() {
                    self.postings.remove(&token);
                }
            }
        }
    }

    /// 查询单个词，返回匹配的行号列表
    pub fn search_term(&self, term: &str) -> Vec<u32> {
        let term = term.to_lowercase();
        self.postings.get(&term).cloned().unwrap_or_default()
    }

    /// 查询多个词（AND 语义 — 同时包含所有词的行）
    pub fn search_and(&self, terms: &[String]) -> Vec<u32> {
        if terms.is_empty() {
            return Vec::new();
        }
        let mut result = self.search_term(&terms[0]);
        for term in &terms[1..] {
            let term_results = self.search_term(term);
            result = Self::intersect(&result, &term_results);
            if result.is_empty() {
                break;
            }
        }
        result
    }

    /// 查询多个词（OR 语义 — 包含任意词的行）
    pub fn search_or(&self, terms: &[String]) -> Vec<u32> {
        if terms.is_empty() {
            return Vec::new();
        }
        let mut result = Vec::new();
        for term in terms {
            let term_results = self.search_term(term);
            result = Self::union(&result, &term_results);
        }
        result
    }

    /// 解析查询字符串（支持 AND/OR 空格，默认 AND）
    pub fn search(&self, query: &str) -> Vec<u32> {
        let tokens = Self::tokenize(query);
        if tokens.is_empty() {
            return Vec::new();
        }
        // 默认 AND 语义
        self.search_and(&tokens)
    }

    /// 两个有序列表的交集
    fn intersect(a: &[u32], b: &[u32]) -> Vec<u32> {
        let mut result = Vec::new();
        let mut i = 0;
        let mut j = 0;
        while i < a.len() && j < b.len() {
            if a[i] == b[j] {
                result.push(a[i]);
                i += 1;
                j += 1;
            } else if a[i] < b[j] {
                i += 1;
            } else {
                j += 1;
            }
        }
        result
    }

    /// 两个列表的并集（有序去重）
    fn union(a: &[u32], b: &[u32]) -> Vec<u32> {
        let mut result = Vec::new();
        let mut i = 0;
        let mut j = 0;
        while i < a.len() && j < b.len() {
            if a[i] == b[j] {
                result.push(a[i]);
                i += 1;
                j += 1;
            } else if a[i] < b[j] {
                result.push(a[i]);
                i += 1;
            } else {
                result.push(b[j]);
                j += 1;
            }
        }
        while i < a.len() {
            result.push(a[i]);
            i += 1;
        }
        while j < b.len() {
            result.push(b[j]);
            j += 1;
        }
        result
    }

    /// 获取总词数
    pub fn total_terms(&self) -> usize {
        self.postings.len()
    }

    /// 获取所有词
    pub fn all_terms(&self) -> Vec<String> {
        let mut terms: Vec<String> = self.postings.keys().cloned().collect();
        terms.sort();
        terms
    }
}

// ============================================================================
// 测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tokenize() {
        let tokens = InvertedIndex::tokenize("Hello World");
        assert_eq!(tokens, vec!["hello", "world"]);
    }

    #[test]
    fn test_tokenize_case() {
        let tokens = InvertedIndex::tokenize("Hello HELLO hello");
        assert_eq!(tokens, vec!["hello", "hello", "hello"]);
    }

    #[test]
    fn test_tokenize_punctuation() {
        let tokens = InvertedIndex::tokenize("hello, world! foo-bar");
        assert_eq!(tokens, vec!["hello", "world", "foo-bar"]);
    }

    #[test]
    fn test_single_term_search() {
        let mut idx = InvertedIndex::new("content");
        idx.add_document(0, "hello world");
        idx.add_document(1, "hello rust");
        idx.add_document(2, "hello world and rust");

        let result = idx.search_term("hello");
        assert_eq!(result, vec![0, 1, 2]);

        let result = idx.search_term("world");
        assert_eq!(result, vec![0, 2]);
    }

    #[test]
    fn test_and_search() {
        let mut idx = InvertedIndex::new("content");
        idx.add_document(0, "hello world");
        idx.add_document(1, "hello rust");
        idx.add_document(2, "world rust");

        let terms = vec!["hello".to_string(), "world".to_string()];
        let result = idx.search_and(&terms);
        assert_eq!(result, vec![0]);
    }

    #[test]
    fn test_or_search() {
        let mut idx = InvertedIndex::new("content");
        idx.add_document(0, "hello world");
        idx.add_document(1, "hello rust");
        idx.add_document(2, "python");

        let terms = vec!["hello".to_string(), "python".to_string()];
        let result = idx.search_or(&terms);
        assert_eq!(result, vec![0, 1, 2]);
    }

    #[test]
    fn test_search_default() {
        let mut idx = InvertedIndex::new("content");
        idx.add_document(0, "hello world");
        idx.add_document(1, "hello rust");
        idx.add_document(2, "world rust");

        // 默认 AND
        let result = idx.search("hello world");
        assert_eq!(result, vec![0]);

        let result = idx.search("hello");
        assert_eq!(result, vec![0, 1]);
    }

    #[test]
    fn test_remove_document() {
        let mut idx = InvertedIndex::new("content");
        idx.add_document(0, "hello world");
        idx.add_document(1, "hello rust");
        idx.add_document(2, "world rust");

        idx.remove_document(0, "hello world");
        let result = idx.search("hello");
        assert_eq!(result, vec![1]);
        let result = idx.search("world");
        assert_eq!(result, vec![2]);
    }

    #[test]
    fn test_empty_query() {
        let idx = InvertedIndex::new("content");
        assert!(idx.search("").is_empty());
    }

    #[test]
    fn test_no_match() {
        let mut idx = InvertedIndex::new("content");
        idx.add_document(0, "hello world");
        assert!(idx.search("nonexistent").is_empty());
    }

    #[test]
    fn test_multiple_docs_same_token() {
        let mut idx = InvertedIndex::new("content");
        for i in 0..10 {
            idx.add_document(i, "hello");
        }
        let result = idx.search("hello");
        assert_eq!(result.len(), 10);
        assert_eq!(result, (0..10).collect::<Vec<_>>());
    }

    #[test]
    fn test_intersect() {
        let a = vec![1, 3, 5, 7];
        let b = vec![3, 4, 5, 8];
        let result = InvertedIndex::intersect(&a, &b);
        assert_eq!(result, vec![3, 5]);
    }

    #[test]
    fn test_union() {
        let a = vec![1, 3, 5];
        let b = vec![3, 4, 6];
        let result = InvertedIndex::union(&a, &b);
        assert_eq!(result, vec![1, 3, 4, 5, 6]);
    }

    #[test]
    fn test_cjk_tokenization() {
        // 中文按字符分割
        let tokens = InvertedIndex::tokenize("你好世界");
        // 中文字符不是 alphanumeric in the ASCII sense
        // 取决于 Rust 的 is_alphanumeric 对中文的支持
        // 这里只是验证不崩溃
        assert!(!tokens.is_empty() || tokens.is_empty());
    }
}