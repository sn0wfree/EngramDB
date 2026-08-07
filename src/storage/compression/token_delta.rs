//! TokenDelta 压缩 codec（v0.21 P0-2）
//!
//! 块级编码：一个 Log 块的多个事件文本 → 压缩字节流。
//! 分层（见 engram-token-stream-compression.md 第 4 章）：
//!   1. tokenize（统一 Tokenizer，Token{id, offset}）
//!   2. ID 映射：词表 id 直用（0..vocab_size）；UNKNOWN 字符 → vocab_size + 块动态索引
//!   3. 前缀 delta（与块内前一行比较，流式追加共享前缀）
//!   4. 熵编码（三形态：Varint / Static（全局频率码长）/ Huffman（块级自适应））
//!
//! 静态热词层说明：三形态熵编码已覆盖频率分布——原「静态热词 1 字节层」功能重叠，去除。
//!
//! 块格式：
//! ```text
//! 块头：[version u16][dyn_count u32][(len u8 + 字符)*n][strategy u8][header_len u32][header]
//! 行：  [shared varint][count varint][stream_len varint][熵编码流（块级表）或 varint 流]
//! ```
//! - Varint：无块级表；行内流 = count 个 LEB128
//! - Static：块头 header = 动态词扩展 [(id u32, len u8)*n]（极少）；全表 = 全局码长 + 扩展
//! - Huffman：块头 header = 符号表 + 码长（huffman.rs 格式）；行内流 = 块级表码流

use crate::common::error::{EngramDbError, Result};
use crate::common::huffman;
use crate::common::tokenizer::{Token, Tokenizer, UNKNOWN_ID};

/// 熵编码形态
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntropyMode {
    /// LEB128 varint（底线）
    Varint,
    /// 静态档位（全局频率码长，词表训练产物；扩展 id 走行内 varint 逃逸流）
    Static,
    /// 块级自适应 Huffman（表头入块）
    Huffman,
}

/// Static 模式逃逸符号：扩展 id（动态词 / 码长 0 的词表 id）在静态表中用该符号
/// 标记，行内流尾部 varint 序列按出现顺序承载真实 id——静态表（含逃逸）一次构建缓存，
/// 避免每块重建全表。u32::MAX-1：词表内 id 与动态 id 都远小于该值，无冲突。
const ESCAPE_ID: u32 = u32::MAX - 1;

/// TokenDelta 编解码器
pub struct TokenDeltaCodec<'a> {
    tok: &'a Tokenizer,
    entropy: EntropyMode,
}

/// 行内流解码器（三形态统一接口）
enum RowDecoder<'a> {
    Varint,
    Huffman {
        table: &'a huffman::HuffmanTable,
    },
    /// Static：缓存表（含逃逸符号）——逃逸 id 在行 stream 尾部 varint 序列
    HuffmanStatic {
        table: &'a huffman::HuffmanTable,
    },
}

impl<'a> TokenDeltaCodec<'a> {
    pub fn new(tok: &'a Tokenizer, entropy: EntropyMode) -> Self {
        Self { tok, entropy }
    }

    /// Static 模式码长表：词表 v2 字段（训练产物）；None = 未生成（全扩展退化）
    fn static_base(&self) -> Option<&[u8]> {
        let from_vocab = self.tok.static_lengths();
        if from_vocab.is_empty() {
            None
        } else {
            Some(from_vocab)
        }
    }

    // ========================================================================
    // 编码
    // ========================================================================

    /// 编码一个块（多个事件文本）
    pub fn encode_block(&self, texts: &[&str]) -> Vec<u8> {
        if texts.is_empty() {
            return Vec::new();
        }
        // 1. tokenize + 动态字典（行间共享前缀 → 增量 tokenize）
        let mut dyn_dict: Vec<String> = Vec::new();
        let mut dyn_index: fxhash::FxHashMap<String, u32> = fxhash::FxHashMap::default();
        let mut rows: Vec<Vec<u32>> = Vec::with_capacity(texts.len());
        let mut prev_text: &str = "";
        let mut prev_tokens: Vec<Token> = Vec::new();
        for text in texts {
            let tokens = self.tok.tokenize_incremental(prev_text, &prev_tokens, text);
            let mut row = Vec::with_capacity(tokens.len());
            for t in &tokens {
                if t.id == UNKNOWN_ID {
                    let ch = &text[t.offset.clone()];
                    let idx = match dyn_index.get(ch) {
                        Some(&i) => i,
                        None => {
                            let i = dyn_dict.len() as u32;
                            dyn_dict.push(ch.to_string());
                            dyn_index.insert(ch.to_string(), i);
                            i
                        }
                    };
                    row.push(self.tok.vocab_size() as u32 + idx);
                } else {
                    row.push(t.id);
                }
            }
            rows.push(row);
            prev_text = text;
            prev_tokens = tokens;
        }

        // 2. 前缀 delta
        let mut deltas: Vec<(u32, Vec<u32>)> = Vec::with_capacity(rows.len());
        let mut prev: &[u32] = &[];
        for row in &rows {
            let shared = common_prefix(prev, row);
            deltas.push((shared as u32, row[shared..].to_vec()));
            prev = row;
        }

        // 3. 块级熵编码表
        let new_ids: Vec<u32> = deltas.iter().flat_map(|(_, n)| n.iter().copied()).collect();
        let (header, row_encode): (Vec<u8>, Box<dyn Fn(&[u32]) -> Vec<u8>>) =
            match self.entropy {
                EntropyMode::Varint => (Vec::new(), Box::new(|ids| {
                    let mut out = Vec::new();
                    for id in ids {
                        encode_varint(&mut out, *id);
                    }
                    out
                })),
                EntropyMode::Static => {
                    // 静态表（含逃逸符号）一次构建缓存；扩展 id（动态词/码长 0）
                    // 走行内 varint 逃逸流，不再逐块重建表
                    let base = self.static_base();
                    let (codes, _table) = self.tok.static_entropy(ESCAPE_ID);
                    let vocab_size = self.tok.vocab_size() as u32;
                    let escape_id = ESCAPE_ID;
                    (
                        Vec::new(), // 块头 header 空（逃逸 id 在行内）
                        Box::new(move |ids: &[u32]| {
                            let mut huf: Vec<u8> = Vec::new();
                            let mut esc: Vec<u8> = Vec::new();
                            let mut buf: u64 = 0;
                            let mut nbits: u32 = 0;
                            for &id in ids {
                                let escape = match base {
                                    Some(b) => id >= vocab_size || b[id as usize] == 0,
                                    None => true, // 无码长表 → 全部逃逸（退化）
                                };
                                let c = if escape {
                                    &codes[&escape_id]
                                } else {
                                    &codes[&id]
                                };
                                if escape {
                                    encode_varint(&mut esc, id);
                                }
                                buf = (buf << c.len as u32) | c.bits as u64;
                                nbits += c.len as u32;
                                while nbits >= 8 {
                                    huf.push((buf >> (nbits - 8)) as u8);
                                    nbits -= 8;
                                    buf &= (1u64 << nbits) - 1;
                                }
                            }
                            if nbits > 0 {
                                huf.push((buf << (8 - nbits)) as u8);
                            }
                            // 码流（字节对齐）+ 逃逸 varint 序列（顺序对应逃逸符号出现）
                            huf.extend_from_slice(&esc);
                            huf
                        }),
                    )
                }
                EntropyMode::Huffman => {
                    let mut freqs: fxhash::FxHashMap<u32, u64> = fxhash::FxHashMap::default();
                    for id in &new_ids {
                        *freqs.entry(*id).or_insert(0) += 1;
                    }
                    let enc = huffman::HuffmanEncoder::new(&freqs);
                    let header = enc.header();
                    (header, Box::new(move |ids: &[u32]| enc.encode(ids)))
                }
            };

        // 4. 组装
        let mut out = Vec::new();
        out.extend_from_slice(&self.tok.version().to_le_bytes()); // 词表版本（解码端校验）
        out.extend_from_slice(&(dyn_dict.len() as u32).to_le_bytes());
        for ch in &dyn_dict {
            out.push(ch.len() as u8);
            out.extend_from_slice(ch.as_bytes());
        }
        out.push(match self.entropy {
            EntropyMode::Varint => 0u8,
            EntropyMode::Static => 1u8,
            EntropyMode::Huffman => 2u8,
        });
        out.extend_from_slice(&(header.len() as u32).to_le_bytes());
        out.extend_from_slice(&header);

        for (shared, new) in &deltas {
            encode_varint(&mut out, *shared);
            encode_varint(&mut out, new.len() as u32);
            let stream = row_encode(new);
            encode_varint(&mut out, stream.len() as u32);
            out.extend_from_slice(&stream);
        }
        out
    }

    // ========================================================================
    // 解码
    // ========================================================================

    pub fn decode_block(&self, bytes: &[u8]) -> Result<Vec<String>> {
        if bytes.is_empty() {
            return Ok(Vec::new());
        }
        let mut pos = 0usize;
        let version = read_u16(bytes, &mut pos)?;
        // 词表版本校验：块用旧词表编码（id 映射不同）→ 拒绝解压（需旧词表）
        if version != self.tok.version() {
            return Err(EngramDbError::Parse(format!(
                "td: vocab version mismatch (block={version}, current={})",
                self.tok.version()
            )));
        }
        let dyn_count = read_u32(bytes, &mut pos)? as usize;
        let mut dyn_dict: Vec<String> = Vec::with_capacity(dyn_count);
        for _ in 0..dyn_count {
            let len = *bytes
                .get(pos)
                .ok_or_else(|| EngramDbError::Parse("td: dict len".into()))?
                as usize;
            pos += 1;
            let s = std::str::from_utf8(
                bytes
                    .get(pos..pos + len)
                    .ok_or_else(|| EngramDbError::Parse("td: dict text".into()))?,
            )
            .map_err(|e| EngramDbError::Parse(format!("td: dict utf8 {e}")))?;
            dyn_dict.push(s.to_string());
            pos += len;
        }
        let strategy = *bytes
            .get(pos)
            .ok_or_else(|| EngramDbError::Parse("td: strat".into()))?;
        pos += 1;
        let header_len = read_u32(bytes, &mut pos)? as usize;
        let header = bytes
            .get(pos..pos + header_len)
            .ok_or_else(|| EngramDbError::Parse("td: header".into()))?
            .to_vec();
        pos += header_len;

        // 构建行解码器（表只建一次，行级只做轻量流状态）
        let huf_table = if strategy == 2 {
            Some(huffman::HuffmanTable::from_header(&header))
        } else {
            None
        };
        let row_decoder = match strategy {
            0 => RowDecoder::Varint,
            1 => {
                // Static：缓存表（全局码长 + 逃逸符号，词表级一次构建）；
                // 逃逸流在行 stream 尾部，解码时按逃逸符号出现顺序取
                let (_codes, table) = self.tok.static_entropy(ESCAPE_ID);
                RowDecoder::HuffmanStatic { table }
            }
            2 => RowDecoder::Huffman { table: huf_table.as_ref().unwrap() },
            _ => return Err(EngramDbError::Parse("td: unknown strategy".into())),
        };

        // 行
        let mut out = Vec::new();
        let mut prev: Vec<u32> = Vec::new();
        while pos < bytes.len() {
            let shared = decode_varint(bytes, &mut pos)? as usize;
            let count = decode_varint(bytes, &mut pos)? as usize;
            let stream_len = decode_varint(bytes, &mut pos)? as usize;
            let stream = bytes
                .get(pos..pos + stream_len)
                .ok_or_else(|| EngramDbError::Parse("td: stream".into()))?;
            pos += stream_len;

            let new_ids: Vec<u32> = match &row_decoder {
                RowDecoder::Varint => {
                    let mut ids = Vec::with_capacity(count);
                    let mut p = 0usize;
                    for _ in 0..count {
                        ids.push(decode_varint(stream, &mut p)?);
                    }
                    ids
                }
                RowDecoder::Huffman { table } => {
                    let mut dec = huffman::HuffmanDecoder::from_table(table, stream.to_vec());
                    dec.decode(count)
                }
                RowDecoder::HuffmanStatic { table } => {
                    let mut dec = huffman::HuffmanDecoder::from_table(table, stream.to_vec());
                    let syms = dec.decode(count);
                    // 逃逸流：Huffman 码流字节边界（含对齐 padding）之后
                    let consumed = dec.consumed_bytes();
                    let esc_stream = stream.get(consumed..).unwrap_or(&[]);
                    let mut ep = 0usize;
                    let mut ids = Vec::with_capacity(syms.len());
                    for s in syms {
                        if s == ESCAPE_ID {
                            ids.push(decode_varint(esc_stream, &mut ep).map_err(|e| {
                                EngramDbError::Parse(format!("td: escape stream: {e}"))
                            })?);
                        } else {
                            ids.push(s);
                        }
                    }
                    ids
                }
            };

            prev.truncate(shared);
            prev.extend_from_slice(&new_ids);
            let mut text = String::new();
            for &id in &prev {
                if (id as usize) < self.tok.vocab_size() {
                    if let Some(t) = self.tok.id_to_token(id) {
                        text.push_str(t);
                    }
                } else {
                    let idx = (id as usize) - self.tok.vocab_size();
                    if let Some(ch) = dyn_dict.get(idx) {
                        text.push_str(ch);
                    }
                }
            }
            out.push(text);
        }
        Ok(out)
    }
}

/// 码长表 → HuffmanDecoder header 格式（符号升序 + 码长）
fn lengths_to_header(lengths: &[(u32, u8)]) -> Vec<u8> {
    let mut symbols: Vec<u32> = lengths.iter().map(|(s, _)| *s).collect();
    symbols.sort_unstable();
    symbols.dedup();
    let len_map: fxhash::FxHashMap<u32, u8> = lengths.iter().copied().collect();
    let mut out = Vec::new();
    out.extend_from_slice(&(symbols.len() as u32).to_le_bytes());
    for s in &symbols {
        out.extend_from_slice(&s.to_le_bytes());
    }
    for s in &symbols {
        out.push(len_map[s]);
    }
    out
}

/// 按码字表编码（静态模式行内流）
fn encode_with_codes(ids: &[u32], codes: &fxhash::FxHashMap<u32, huffman::Code>) -> Vec<u8> {
    let mut buf: u64 = 0;
    let mut nbits: u32 = 0;
    let mut out: Vec<u8> = Vec::new();
    for id in ids {
        let c = codes[id];
        buf = (buf << c.len as u32) | c.bits as u64;
        nbits += c.len as u32;
        while nbits >= 8 {
            out.push((buf >> (nbits - 8)) as u8);
            nbits -= 8;
            buf &= (1u64 << nbits) - 1;
        }
    }
    if nbits > 0 {
        out.push((buf << (8 - nbits)) as u8);
    }
    out
}

fn common_prefix(a: &[u32], b: &[u32]) -> usize {
    let mut n = 0;
    while n < a.len() && n < b.len() && a[n] == b[n] {
        n += 1;
    }
    n
}

fn encode_varint(out: &mut Vec<u8>, mut v: u32) {
    loop {
        let mut byte = (v & 0x7F) as u8;
        v >>= 7;
        if v != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if v == 0 {
            break;
        }
    }
}

fn decode_varint(bytes: &[u8], pos: &mut usize) -> Result<u32> {
    let mut v: u32 = 0;
    let mut shift = 0u32;
    loop {
        let b = *bytes
            .get(*pos)
            .ok_or_else(|| EngramDbError::Parse("td: varint".into()))?;
        *pos += 1;
        v |= ((b & 0x7F) as u32) << shift;
        if b & 0x80 == 0 {
            break;
        }
        shift += 7;
        if shift >= 32 {
            return Err(EngramDbError::Parse("td: varint overflow".into()));
        }
    }
    Ok(v)
}

fn read_u16(bytes: &[u8], pos: &mut usize) -> Result<u16> {
    let b = bytes
        .get(*pos..*pos + 2)
        .ok_or_else(|| EngramDbError::Parse("td: u16".into()))?;
    *pos += 2;
    Ok(u16::from_le_bytes(b.try_into().unwrap()))
}

fn read_u32(bytes: &[u8], pos: &mut usize) -> Result<u32> {
    let b = bytes
        .get(*pos..*pos + 4)
        .ok_or_else(|| EngramDbError::Parse("td: u32".into()))?;
    *pos += 4;
    Ok(u32::from_le_bytes(b.try_into().unwrap()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::vocab_file::VocabFile;

    fn smoke_tok() -> Tokenizer {
        Tokenizer::from_vocab_file(VocabFile::new(
            Vec::new(),
            vec![("你".into(), "好".into()), ("世".into(), "界".into())],
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
        ))
        .unwrap()
    }

    fn codec<'a>(tok: &'a Tokenizer, entropy: EntropyMode) -> TokenDeltaCodec<'a> {
        TokenDeltaCodec::new(tok, entropy)
    }

    /// Static 模式：带码长表的词表（高频词短码 + 低频词长码 + 0 码长未登录）
    fn smoke_tok_static() -> Tokenizer {
        let mut vf = VocabFile::new(
            Vec::new(),
            vec![("你".into(), "好".into()), ("世".into(), "界".into())],
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
        // Kraft 平衡的码长表：你/好 2 位、世/界 3 位、你好/世界 4 位、
        // 标点/字母 8 位（0 位 = 无码长 → 走逃逸）
        let mut sl = vec![0u8; vf.vocab.len()];
        for (id, len) in [(0usize, 2u8), (1, 2), (2, 3), (3, 3), (13, 4), (14, 4)] {
            sl[id] = len;
        }
        for id in [4usize, 5, 6, 7, 8, 9, 10, 11, 12] {
            sl[id] = 8;
        }
        vf.static_lengths = sl;
        Tokenizer::from_vocab_file(vf).unwrap()
    }

    #[test]
    fn test_roundtrip_varint() {
        let tok = smoke_tok();
        let codec = codec(&tok, EntropyMode::Varint);
        let texts = vec!["你好世界！hello world", "你好世界！hello", "世界！世界！"];
        let bytes = codec.encode_block(&texts);
        let decoded = codec.decode_block(&bytes).unwrap();
        assert_eq!(decoded, texts);
    }

    #[test]
    fn test_roundtrip_static() {
        let tok = smoke_tok();
        let codec = codec(&tok, EntropyMode::Static);
        let texts = vec!["你好世界！hello world", "你好世界！hello", "世界！世界！"];
        let bytes = codec.encode_block(&texts);
        let decoded = codec.decode_block(&bytes).unwrap();
        assert_eq!(decoded, texts);
    }

    #[test]
    fn test_roundtrip_static_with_lengths() {
        // 带码长表：短码 + 8 位 + 0 码长（逃逸）混合路径
        let tok = smoke_tok_static();
        let codec = codec(&tok, EntropyMode::Static);
        // 含词表外字符（UNKNOWN → 动态词 → 逃逸流）+ 码长 0 词表词（低频）
        let texts = vec![
            "你好世界！hello world",
            "你好世界！𠀀𠀁 hello",
            "世界！世界！𠀀",
        ];
        let bytes = codec.encode_block(&texts);
        let decoded = codec.decode_block(&bytes).unwrap();
        assert_eq!(decoded, texts);
    }

    #[test]
    fn test_roundtrip_huffman() {
        let tok = smoke_tok();
        let codec = codec(&tok, EntropyMode::Huffman);
        let texts = vec!["你好世界！hello world", "你好世界！hello", "世界！世界！"];
        let bytes = codec.encode_block(&texts);
        let decoded = codec.decode_block(&bytes).unwrap();
        assert_eq!(decoded, texts);
    }

    #[test]
    fn test_roundtrip_streaming_growth() {
        // 流式追加：快照逐 token 增长（opencode 形态）
        let tok = smoke_tok();
        let codec = codec(&tok, EntropyMode::Huffman);
        let base = "你好世界！hello world world world";
        let tokens = tok.tokenize(base);
        let mut texts = Vec::new();
        let mut prev_end = 0usize;
        for t in &tokens {
            if t.offset.end > prev_end {
                prev_end = t.offset.end;
                texts.push(base[..prev_end].to_string());
            }
        }
        assert!(texts.len() >= 3);
        let bytes = codec.encode_block(&texts.iter().map(|s| s.as_str()).collect::<Vec<_>>());
        let decoded = codec.decode_block(&bytes).unwrap();
        assert_eq!(decoded, texts);
    }

    #[test]
    fn test_roundtrip_with_unknown() {
        // 含词表外字符（emoji → 动态字典）
        let tok = smoke_tok();
        let codec = codec(&tok, EntropyMode::Huffman);
        let texts = vec!["你好😀世界", "世界😀你好"];
        let bytes = codec.encode_block(&texts);
        let decoded = codec.decode_block(&bytes).unwrap();
        assert_eq!(decoded, texts);
    }
}
