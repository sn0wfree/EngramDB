//! 词表文件格式（v0.21 统一 Tokenizer，自定义自包含格式）
//!
//! 由离线训练 bin（tools/train_vocab.rs，依赖 tokenizers）产出；
//! 运行时编码器（src/common/tokenizer.rs）加载。
//! bincode 序列化，include_bytes! 或外部文件加载。
//!
//! 格式演进：加字段需提升 version，旧版块仍按自身 version 加载——词表版本化保证
//! 旧块永远可解压（见 engram-token-stream-compression.md 3.4）。

use serde::{Deserialize, Serialize};

pub const VOCAB_MAGIC: [u8; 4] = *b"ENGV";
pub const VOCAB_VERSION: u16 = 1;

/// 词表文件（自包含：预分割规则版本 + 种子词 + merges + 全部 token + 归一化标志）
///
/// rank 语义：merges 按训练合并顺序存储（rank 序），**静态热词 = merges 前 TOP_N 个产物**；
/// vocab 按 token id 升序存储（BPE 训练 id 按合并顺序分配）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VocabFile {
    pub magic: [u8; 4],
    pub version: u16,
    /// 归一化标志（运行时 norm 视图是否 NFKC + lowercase）
    pub normalize: bool,
    /// 种子词（jieba 风格贪心切分词典，可空）
    pub seeds: Vec<String>,
    /// BPE merges，按训练合并顺序（rank 升序）——静态热词 = 前 TOP_N 个产物
    pub merges: Vec<(String, String)>,
    /// 全部 token，按 id 升序（rank 序）
    pub vocab: Vec<String>,
}

impl VocabFile {
    pub fn new(seeds: Vec<String>, merges: Vec<(String, String)>, vocab: Vec<String>) -> Self {
        Self {
            magic: VOCAB_MAGIC,
            version: VOCAB_VERSION,
            normalize: true,
            seeds,
            merges,
            vocab,
        }
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>, bincode::Error> {
        bincode::serialize(self)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, bincode::Error> {
        bincode::deserialize(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vocab_roundtrip() {
        let vf = VocabFile::new(
            vec!["银行".into()],
            vec![("银".into(), "行".into())],
            vec!["银".into(), "行".into(), "银行".into()],
        );
        let bytes = vf.to_bytes().unwrap();
        let loaded = VocabFile::from_bytes(&bytes).unwrap();
        assert_eq!(loaded.magic, VOCAB_MAGIC);
        assert_eq!(loaded.version, VOCAB_VERSION);
        assert_eq!(loaded.seeds, vec!["银行".to_string()]);
        assert_eq!(loaded.merges, vec![("银".to_string(), "行".to_string())]);
        assert_eq!(loaded.vocab, vec!["银".to_string(), "行".to_string(), "银行".to_string()]);
    }

    #[test]
    fn test_vocab_magic_check() {
        let vf = VocabFile::new(Vec::new(), Vec::new(), Vec::new());
        let bytes = vf.to_bytes().unwrap();
        assert_eq!(&bytes[..4], &VOCAB_MAGIC);
    }
}
