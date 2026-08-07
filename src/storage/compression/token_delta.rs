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
    /// 静态档位（全局频率码长，词表训练产物；块头仅动态词扩展）
    Static,
    /// 块级自适应 Huffman（表头入块）
    Huffman,
}

/// TokenDelta 编解码器
pub struct TokenDeltaCodec<'a> {
    tok: &'a Tokenizer,
    entropy: EntropyMode,
    /// Static 模式：per-id 码长表（全局频率产物，词表训练端提供）
    static_lengths: Option<Vec<u8>>,
}

/// 行内流解码器（三形态统一接口）
enum RowDecoder {
    Varint,
    Huffman {
        table: huffman::HuffmanTable,
    },
}

impl<'a> TokenDeltaCodec<'a> {
    pub fn new(tok: &'a Tokenizer, entropy: EntropyMode, static_lengths: Option<Vec<u8>>) -> Self {
        Self { tok, entropy, static_lengths }
    }

    /// Static 模式码长表：显式传入优先，否则用词表自带（v2 字段）；都无 → 全零（全扩展退化）
    fn effective_static_lengths(&self) -> Vec<u8> {
        match &self.static_lengths {
            Some(sl) => sl.clone(),
            None => {
                let from_vocab = self.tok.static_lengths();
                if from_vocab.is_empty() {
                    vec![0u8; self.tok.vocab_size()]
                } else {
                    from_vocab.to_vec()
                }
            }
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
                    // 全表 = 全局码长 + 块扩展（码长 0 的词表 id 或动态词，24 位定长）
                    let base = self.effective_static_lengths();
                    let mut dyn_ext: Vec<(u32, u8)> = Vec::new();
                    let mut lengths: Vec<(u32, u8)> = base
                        .iter()
                        .enumerate()
                        .filter(|(_, l)| **l > 0)
                        .map(|(id, l)| (id as u32, *l))
                        .collect();
                    let mut seen: fxhash::FxHashSet<u32> =
                        lengths.iter().map(|(s, _)| *s).collect();
                    // 去重后再检查（避免大块 O(n²) 级重复检查）
                    let unique_ids: fxhash::FxHashSet<u32> = new_ids.iter().copied().collect();
                    for id in unique_ids {
                        let needs_ext = (id >= self.tok.vocab_size() as u32)
                            || base[id as usize] == 0;
                        if needs_ext && !seen.contains(&id) {
                            seen.insert(id);
                            dyn_ext.push((id, 24));
                            lengths.push((id, 24));
                        }
                    }
                    let codes = huffman::canonical_codes(&lengths);
                    let mut header = Vec::new();
                    header.extend_from_slice(&(dyn_ext.len() as u32).to_le_bytes());
                    for (id, len) in &dyn_ext {
                        header.extend_from_slice(&id.to_le_bytes());
                        header.push(*len);
                    }
                    (header, Box::new(move |ids: &[u32]| encode_with_codes(ids, &codes)))
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
        let row_decoder = match strategy {
            0 => RowDecoder::Varint,
            1 => {
                // Static：合成全表（全局 + 动态扩展）→ HuffmanDecoder 格式的 header
                let base = self.effective_static_lengths();
                let mut lengths: Vec<(u32, u8)> = base
                    .iter()
                    .enumerate()
                    .filter(|(_, l)| **l > 0)
                    .map(|(id, l)| (id as u32, *l))
                    .collect();
                let mut hpos = 0usize;
                let ext_count = read_u32(&header, &mut hpos)? as usize;
                for _ in 0..ext_count {
                    let id = read_u32(&header, &mut hpos)?;
                    let len = *header
                        .get(hpos)
                        .ok_or_else(|| EngramDbError::Parse("td: ext len".into()))?;
                    hpos += 1;
                    lengths.push((id, len));
                }
                RowDecoder::Huffman { table: huffman::HuffmanTable::from_header(&lengths_to_header(&lengths)) }
            }
            2 => RowDecoder::Huffman { table: huffman::HuffmanTable::from_header(&header) },
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
        // Static 模式：全部词表词 8 位码长（模拟全局频率产物）
        let static_lengths = if entropy == EntropyMode::Static {
            Some(vec![8u8; tok.vocab_size()])
        } else {
            None
        };
        TokenDeltaCodec::new(tok, entropy, static_lengths)
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
