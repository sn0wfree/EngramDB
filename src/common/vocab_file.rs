//! 词表文件格式（v0.21 统一 Tokenizer，自定义自包含格式）
//!
//! 由离线训练 bin（examples/train_vocab.rs，依赖 tokenizers）产出；
//! 运行时编码器（src/common/tokenizer.rs）加载。
//! bincode 序列化，include_bytes! 或外部文件加载。
//!
//! v2：新增 `static_lengths`（TokenDelta Static 模式的 per-id 码长表，
//! 训练端从语料频率生成）——词表自包含，运行时无需第二文件。
//! v1 → v2 兼容：`from_bytes` 按 version 分支解析，v1 缺字段自动补空。
//!
//! 格式演进：加字段需提升 version，旧版块仍按自身 version 加载——词表版本化保证
//! 旧块永远可解压（见 engram-token-stream-compression.md 3.4）。

use serde::{Deserialize, Serialize};

pub const VOCAB_MAGIC: [u8; 4] = *b"ENGV";
pub const VOCAB_VERSION: u16 = 2;
pub const VOCAB_VERSION_V1: u16 = 1;

/// 词表文件（自包含：预分割规则版本 + 种子词 + merges + 全部 token + 归一化标志 + 码长表）
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
    /// v2：TokenDelta Static 模式 per-id 码长表（0 = 无码长；空 = 未生成 → Static 退化）
    pub static_lengths: Vec<u8>,
}

/// v1 结构（无 static_lengths 字段，bincode 布局兼容）
#[derive(Serialize, Deserialize)]
struct VocabFileV1 {
    magic: [u8; 4],
    version: u16,
    normalize: bool,
    seeds: Vec<String>,
    merges: Vec<(String, String)>,
    vocab: Vec<String>,
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
            static_lengths: Vec::new(),
        }
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>, bincode::Error> {
        bincode::serialize(self)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, bincode::Error> {
        let version = bytes.get(4..6).map(|b| u16::from_le_bytes([b[0], b[1]]));
        match version {
            Some(VOCAB_VERSION_V1) => {
                let v1: VocabFileV1 = bincode::deserialize(bytes)?;
                Ok(Self {
                    magic: v1.magic,
                    version: v1.version,
                    normalize: v1.normalize,
                    seeds: v1.seeds,
                    merges: v1.merges,
                    vocab: v1.vocab,
                    static_lengths: Vec::new(),
                })
            }
            _ => bincode::deserialize(bytes),
        }
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
    fn test_vocab_v1_compat() {
        // v1 布局：无 static_lengths 字段——手工构造 v1 字节流
        #[derive(Serialize)]
        struct V1 {
            magic: [u8; 4],
            version: u16,
            normalize: bool,
            seeds: Vec<String>,
            merges: Vec<(String, String)>,
            vocab: Vec<String>,
        }
        let v1 = V1 {
            magic: VOCAB_MAGIC,
            version: 1,
            normalize: true,
            seeds: vec![],
            merges: vec![],
            vocab: vec!["a".into(), "b".into()],
        };
        let bytes = bincode::serialize(&v1).unwrap();
        let loaded = VocabFile::from_bytes(&bytes).unwrap();
        assert_eq!(loaded.version, 1);
        assert_eq!(loaded.vocab, vec!["a".to_string(), "b".to_string()]);
        assert!(loaded.static_lengths.is_empty());
    }

    #[test]
    fn test_vocab_magic_check() {
        let vf = VocabFile::new(Vec::new(), Vec::new(), Vec::new());
        let bytes = vf.to_bytes().unwrap();
        assert_eq!(&bytes[..4], &VOCAB_MAGIC);
    }
}
