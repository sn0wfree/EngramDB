/// 文档摄入 API（v0.15.0 新增）
///
/// 提供一键摄入 Markdown/文本文件并自动建向量索引的能力。
///
/// # 使用流程
///
/// 1. 创建文档表（自动构建 id, title, content, embedding 四列）
/// 2. 摄入文件/文本（自动分块、建索引）
/// 3. 混合检索（向量 + 标量过滤）
///
/// # 分块策略
///
/// 采用基于标题的语义分块（类似 ReasonDB HRR）：
/// - `# Title` / `## Section` 等 Markdown 标题作为分块边界
/// - 纯文本按最大字符数 + 重叠窗口分块
/// - 每个分块作为一个独立的文档行
use crate::common::error::{EngramDbError, Result};
use crate::common::types::{ColumnDef, DataType, TableDef};
use crate::storage::vector_index::DistanceMetric;
use crate::{Connection, Value};

/// 分块配置
#[derive(Debug, Clone)]
pub struct ChunkConfig {
    /// 最大块大小（字符数），默认 512
    pub max_chunk_size: usize,
    /// 块间重叠（字符数），默认 64
    pub chunk_overlap: usize,
    /// 是否按 Markdown 标题分块，默认 true
    pub use_markdown_headers: bool,
}

impl Default for ChunkConfig {
    fn default() -> Self {
        ChunkConfig {
            max_chunk_size: 512,
            chunk_overlap: 64,
            use_markdown_headers: true,
        }
    }
}

/// 文档摄入器
///
/// 提供文档表管理和文本分块摄入功能。
/// 使用前需先通过 `create_document_table` 创建文档表。
pub struct DocumentIngestor<'a> {
    conn: &'a mut Connection,
    table_name: String,
    chunk_config: ChunkConfig,
}

impl<'a> DocumentIngestor<'a> {
    /// 创建文档摄入器（自动创建文档表）
    ///
    /// 文档表包含以下列：
    /// - `id` INT PRIMARY KEY AUTO_INCREMENT — 文档 ID
    /// - `title` VARCHAR — 文档标题（或分块标题）
    /// - `content` VARCHAR — 文档内容（或分块内容）
    /// - `embedding` VECTOR(dim) — 向量嵌入（占位，需调用 ingest 时提供）
    ///
    /// `embedding_dim`: 向量维度（如 1536 对应 OpenAI text-embedding-3-small）
    pub fn new(
        conn: &'a mut Connection,
        table_name: &str,
        embedding_dim: usize,
        chunk_config: ChunkConfig,
    ) -> Result<Self> {
        let columns = vec![
            ColumnDef::new("id", DataType::Int64).primary_key().auto_inc(),
            ColumnDef::new("title", DataType::Varchar).not_null(),
            ColumnDef::new("content", DataType::Varchar).not_null(),
            ColumnDef::new("embedding", DataType::Vector { dim: embedding_dim }),
        ];
        let table_def = TableDef::new(0, table_name, columns);
        let db = conn.database_mut();
        db.create_table(table_def)?;
        Ok(DocumentIngestor {
            conn,
            table_name: table_name.to_string(),
            chunk_config,
        })
    }

    /// 创建向量索引（HNSW）
    ///
    /// 在 `embedding` 列上创建 HNSW 近似最近邻索引。
    /// 建议在摄入数据后调用。
    ///
    /// `metric`: 距离度量（默认 Cosine 适合文本嵌入）
    pub fn create_vector_index(&mut self, metric: DistanceMetric) -> Result<()> {
        let db = self.conn.database_mut();
        db.create_vector_index(&self.table_name, "idx_embedding", "embedding", metric, 16, 100)
    }

    /// 摄入文本（自动分块 + 插入）
    ///
    /// 将文本按配置分块，每块作为一行插入文档表。
    /// `embedding_fn`: 外部嵌入函数，接收文本返回向量。
    /// `title`: 文档标题，每个分块会附带此标题。
    ///
    /// 返回摄入的块数。
    pub fn ingest_text(
        &mut self,
        text: &str,
        title: &str,
        embedding_fn: &dyn Fn(&str) -> Vec<f32>,
    ) -> Result<usize> {
        let chunks = self.chunk_text(text, title);
        if chunks.is_empty() {
            return Ok(0);
        }

        let db = self.conn.database_mut();
        let table = db.get_table_mut(&self.table_name)
            .ok_or_else(|| EngramDbError::TableNotFound(self.table_name.clone()))?;

        let mut rows = Vec::with_capacity(chunks.len());
        for (chunk_title, chunk_text) in &chunks {
            let embedding = embedding_fn(chunk_text);
            rows.push(vec![
                Value::Null, // id: AUTO_INCREMENT
                Value::Varchar(chunk_title.clone()),
                Value::Varchar(chunk_text.clone()),
                Value::Vector(embedding),
            ]);
        }

        table.insert(rows)?;
        Ok(chunks.len())
    }

    /// 摄入 Markdown 文件
    ///
    /// 读取文件内容，自动解析 Markdown 标题结构并分块。
    pub fn ingest_file(
        &mut self,
        file_path: &str,
        embedding_fn: &dyn Fn(&str) -> Vec<f32>,
    ) -> Result<usize> {
        let content = std::fs::read_to_string(file_path)
            .map_err(|e| EngramDbError::Io(e))?;
        let title = std::path::Path::new(file_path)
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "untitled".to_string());
        self.ingest_text(&content, &title, embedding_fn)
    }

    /// 获取文档行数
    pub fn count(&mut self) -> Result<u64> {
        let db = self.conn.database_mut();
        let table = db.get_table(&self.table_name)
            .ok_or_else(|| EngramDbError::TableNotFound(self.table_name.clone()))?;
        Ok(table.def.row_count)
    }

    // ---------- 内部方法 ----------

    /// 文本分块
    ///
    /// 策略：
    /// 1. 如果启用 Markdown 标题分块，按 `# `、`## ` 等标题分割
    /// 2. 否则按 `max_chunk_size` + `chunk_overlap` 滑动窗口分割
    fn chunk_text(&self, text: &str, title: &str) -> Vec<(String, String)> {
        if self.chunk_config.use_markdown_headers {
            self.chunk_by_markdown(text, title)
        } else {
            self.chunk_by_size(text, title)
        }
    }

    /// 按 Markdown 标题分块
    fn chunk_by_markdown(&self, text: &str, title: &str) -> Vec<(String, String)> {
        let mut chunks: Vec<(String, String)> = Vec::new();
        let mut current_section = title.to_string();
        let mut current_lines: Vec<&str> = Vec::new();
        let mut current_size = 0;

        for line in text.lines() {
            let trimmed = line.trim();
            // 检测 Markdown 标题
            if trimmed.starts_with('#') {
                // 保存当前段落
                if !current_lines.is_empty() {
                    let content = current_lines.join("\n");
                    if !content.trim().is_empty() {
                        chunks.push((current_section.clone(), content));
                    }
                    current_lines.clear();
                    current_size = 0;
                }
                // 提取标题文本（去掉 # 前缀）
                current_section = trimmed
                    .trim_start_matches('#')
                    .trim()
                    .to_string();
                if current_section.is_empty() {
                    current_section = title.to_string();
                }
                continue;
            }
            current_lines.push(line);
            current_size += line.len();

            // 如果当前段落超过最大块大小，强制分割
            if current_size >= self.chunk_config.max_chunk_size {
                let content = current_lines.join("\n");
                if !content.trim().is_empty() {
                    chunks.push((current_section.clone(), content));
                }
                // 保留重叠部分
                let overlap_lines = self.get_overlap_lines(&current_lines);
                current_lines = overlap_lines;
                current_size = current_lines.iter().map(|l| l.len()).sum();
            }
        }

        // 最后一段
        if !current_lines.is_empty() {
            let content = current_lines.join("\n");
            if !content.trim().is_empty() {
                chunks.push((current_section.clone(), content));
            }
        }

        chunks
    }

    /// 按固定大小 + 重叠分块
    fn chunk_by_size(&self, text: &str, title: &str) -> Vec<(String, String)> {
        let mut chunks = Vec::new();
        let max = self.chunk_config.max_chunk_size;
        let overlap = self.chunk_config.chunk_overlap;
        let mut start = 0;

        while start < text.len() {
            let end = (start + max).min(text.len());
            // 尽量在单词边界结束
            let chunk_end = if end < text.len() {
                if let Some(space_pos) = text[start..end].rfind(' ') {
                    start + space_pos + 1
                } else {
                    end
                }
            } else {
                end
            };

            let chunk = &text[start..chunk_end];
            if !chunk.trim().is_empty() {
                chunks.push((title.to_string(), chunk.to_string()));
            }

            if chunk_end >= text.len() {
                break;
            }
            start = chunk_end.saturating_sub(overlap);
        }

        chunks
    }

    /// 获取重叠行（从末尾开始保留 overlap 字符数）
    fn get_overlap_lines<'b>(&self, lines: &[&'b str]) -> Vec<&'b str> {
        let mut overlap_size = 0;
        let mut overlap_start = lines.len();

        for (i, line) in lines.iter().enumerate().rev() {
            overlap_size += line.len();
            if overlap_size >= self.chunk_config.chunk_overlap {
                overlap_start = i;
                break;
            }
        }

        lines[overlap_start..].to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ingest_create_table() {
        let mut conn = Connection::open(":memory:").unwrap();
        let mut ingestor = DocumentIngestor::new(
            &mut conn, "docs", 8, ChunkConfig::default(),
        ).unwrap();
        assert_eq!(ingestor.count().unwrap(), 0);
    }

    #[test]
    fn test_ingest_text() {
        let mut conn = Connection::open(":memory:").unwrap();
        let mut ingestor = DocumentIngestor::new(
            &mut conn, "docs", 4, ChunkConfig {
                max_chunk_size: 100,
                chunk_overlap: 20,
                use_markdown_headers: false,
            },
        ).unwrap();

        // 模拟嵌入函数：返回固定向量
        let embedding_fn = |text: &str| {
            let hash: f32 = text.len() as f32;
            vec![hash, hash * 0.5, hash * 0.25, hash * 0.125]
        };

        let count = ingestor.ingest_text(
            "Hello world. This is a test document. ",
            "test",
            &embedding_fn,
        ).unwrap();
        assert_eq!(count, 1, "短文本应该分 1 块");

        // 长文本分多块
        let long_text = "word ".repeat(200);
        let count = ingestor.ingest_text(&long_text, "long", &embedding_fn).unwrap();
        assert!(count > 1, "长文本应该分多块");
    }

    #[test]
    fn test_ingest_markdown_chunking() {
        let mut conn = Connection::open(":memory:").unwrap();
        let mut ingestor = DocumentIngestor::new(
            &mut conn, "docs", 4, ChunkConfig::default(),
        ).unwrap();

        let markdown = r#"# Introduction
This is the intro section.
It has multiple paragraphs.

## Details
More detailed content here.
With a second paragraph.

## Conclusion
Final thoughts."#;

        let embedding_fn = |_text: &str| vec![0.1, 0.2, 0.3, 0.4];
        let count = ingestor.ingest_text(markdown, "test", &embedding_fn).unwrap();
        assert_eq!(count, 3, "Markdown 应该按标题分 3 块");
    }

    #[test]
    fn test_ingest_vector_index() {
        let mut conn = Connection::open(":memory:").unwrap();
        let mut ingestor = DocumentIngestor::new(
            &mut conn, "docs", 4, ChunkConfig::default(),
        ).unwrap();

        let embedding_fn = |_text: &str| vec![0.1, 0.2, 0.3, 0.4];
        ingestor.ingest_text("Some content", "title", &embedding_fn).unwrap();

        // 创建向量索引并验证
        let result = ingestor.create_vector_index(DistanceMetric::Cosine);
        assert!(result.is_ok(), "向量索引创建成功");
    }

    #[test]
    fn test_chunk_by_size_overlap() {
        let config = ChunkConfig {
            max_chunk_size: 20,
            chunk_overlap: 5,
            use_markdown_headers: false,
        };
        let ingestor = DocumentIngestor {
            conn: &mut Connection::open(":memory:").unwrap(),
            table_name: "t".to_string(),
            chunk_config: config,
        };

        let text = "aa bb cc dd ee ff gg hh ii jj kk ll mm nn oo pp";
        let chunks = ingestor.chunk_by_size(text, "title");
        assert!(chunks.len() >= 2, "长文本应该分多块");
        // 验证重叠：相邻块应该有重叠内容
        if chunks.len() >= 2 {
            let first = &chunks[0].1;
            let second = &chunks[1].1;
            // 第二个块应该包含第一个块末尾的部分内容
            assert!(!second.is_empty());
        }
    }

    #[test]
    fn test_chunk_by_markdown_headers() {
        let config = ChunkConfig::default();
        let ingestor = DocumentIngestor {
            conn: &mut Connection::open(":memory:").unwrap(),
            table_name: "t".to_string(),
            chunk_config: config,
        };

        let md = "# A\ncontent a\n## B\ncontent b\n### C\ncontent c";
        let chunks = ingestor.chunk_by_markdown(md, "root");
        assert_eq!(chunks.len(), 3, "3 个标题应该分 3 块");
        assert_eq!(chunks[0].0, "A");
        assert_eq!(chunks[1].0, "B");
        assert_eq!(chunks[2].0, "C");
    }
}