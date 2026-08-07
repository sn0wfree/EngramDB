//! 规范 Huffman 编解码（v0.21 TokenDelta 熵编码层，P0-2）
//!
//! 经典 Huffman 树构建 → 码字 → **规范（canonical）编码**：
//! 表头只存每个符号的码长，解码端按 (len, symbol) 序重建规范码字——零歧义。
//! 确定性（同频按符号序）；零外部依赖；码流按位打包。
//!
//! 表头格式：
//! ```text
//! [符号数 u32][symbol u32 列表（升序）][码长 u8 × N]...[码流（位打包，尾部补齐 0）]
//! ```
//! 单符号分布：该符号码长 0（无位流）——空输入返回空码流。

use fxhash::FxHashMap;

/// 单符号规范码字
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Code {
    pub len: u8,
    pub bits: u32,
}

/// 由码长表重建规范码字：(symbol, len) 按 (len, symbol) 排序，同长组内符号序递增
pub fn canonical_codes(lengths: &[(u32, u8)]) -> FxHashMap<u32, Code> {
    let mut sorted: Vec<(u8, u32)> =
        lengths.iter().filter(|(_, l)| *l > 0).map(|(s, l)| (*l, *s)).collect();
    sorted.sort_unstable();
    let mut codes: FxHashMap<u32, Code> = FxHashMap::default();
    let mut code: u32 = 0;
    let mut prev_len: u8 = 0;
    for (len, symbol) in sorted {
        // 仅换长度组时左移
        if len != prev_len {
            code <<= len - prev_len;
            prev_len = len;
        }
        codes.insert(symbol, Code { len, bits: code });
        code += 1;
    }
    codes
}

/// Huffman 树节点：叶子带 symbol
#[derive(Clone)]
struct HNode {
    freq: u64,
    symbol: Option<u32>,
    left: Option<usize>,
    right: Option<usize>,
}

/// 由频率表构建码长表（symbol, len）
pub fn build_lengths(freqs: &FxHashMap<u32, u64>) -> Vec<(u32, u8)> {
    use std::collections::BinaryHeap;
    let mut nodes: Vec<HNode> = Vec::new();
    let mut syms: Vec<u32> = freqs.keys().copied().collect();
    syms.sort_unstable();
    // 堆项：(Reverse<freq>, Reverse<node_id>)——node_id 保证同频确定性
    let mut heap: BinaryHeap<(std::cmp::Reverse<u64>, std::cmp::Reverse<usize>)> =
        BinaryHeap::new();
    for s in syms {
        let f = freqs[&s];
        if f == 0 {
            continue;
        }
        let id = nodes.len();
        nodes.push(HNode { freq: f, symbol: Some(s), left: None, right: None });
        heap.push((std::cmp::Reverse(f), std::cmp::Reverse(id)));
    }
    if nodes.is_empty() {
        return Vec::new();
    }
    while heap.len() > 1 {
        let (f1, id1) = heap.pop().unwrap();
        let (f2, id2) = heap.pop().unwrap();
        let id = nodes.len();
        nodes.push(HNode {
            freq: f1.0 + f2.0,
            symbol: None,
            left: Some(id1.0),
            right: Some(id2.0),
        });
        heap.push((std::cmp::Reverse(f1.0 + f2.0), std::cmp::Reverse(id)));
    }
    let root = heap.pop().unwrap().1 .0;
    // 单符号分布：强制码长 1（bits 0），避免 len=0 无码字
    if nodes.len() == 1 {
        return vec![(nodes[root].symbol.unwrap(), 1)];
    }
    let mut lengths: Vec<(u32, u8)> = Vec::new();
    fn walk(nodes: &[HNode], node: usize, depth: u8, lengths: &mut Vec<(u32, u8)>) {
        let n = &nodes[node];
        match (n.left, n.right) {
            (Some(l), Some(r)) => {
                walk(nodes, l, depth + 1, lengths);
                walk(nodes, r, depth + 1, lengths);
            }
            _ => {
                if let Some(sym) = n.symbol {
                    lengths.push((sym, depth));
                }
            }
        }
    }
    walk(&nodes, root, 0, &mut lengths);
    lengths
}

// ============================================================================
// 编码器
// ============================================================================

pub struct HuffmanEncoder {
    codes: FxHashMap<u32, Code>,
    /// symbol 升序 + 对应码长（header 序列化顺序）
    symbols: Vec<u32>,
    lengths: Vec<u8>,
}

impl HuffmanEncoder {
    pub fn new(freqs: &FxHashMap<u32, u64>) -> Self {
        let lengths = build_lengths(freqs);
        let codes = canonical_codes(&lengths);
        let mut symbols: Vec<u32> = lengths.iter().map(|(s, _)| *s).collect();
        symbols.sort_unstable();
        let len_map: FxHashMap<u32, u8> = lengths.iter().copied().collect();
        let lens: Vec<u8> = symbols.iter().map(|s| len_map[s]).collect();
        Self { codes, symbols, lengths: lens }
    }

    /// 表头字节（解码端重建）
    pub fn header(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&(self.symbols.len() as u32).to_le_bytes());
        for s in &self.symbols {
            out.extend_from_slice(&s.to_le_bytes());
        }
        for len in &self.lengths {
            out.push(*len);
        }
        out
    }

    /// 编码符号序列 → 码流（不含表头）
    pub fn encode(&self, symbols: &[u32]) -> Vec<u8> {
        let mut buf: u64 = 0;
        let mut nbits: u32 = 0;
        let mut out: Vec<u8> = Vec::new();
        for s in symbols {
            let c = self.codes[s];
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
}

// ============================================================================
// 解码器
// ============================================================================

pub struct HuffmanDecoder {
    /// first_code[len]：该码长的最小码字
    first_code: [u32; 33],
    /// count[len]：该码长符号数
    count: [u32; 33],
    /// 每码长的符号列表（升序）
    symbols_by_len: Vec<Vec<u32>>,
    /// 码流
    data: Vec<u8>,
    pos: usize,
    buf: u64,
    nbits: u32,
}

impl HuffmanDecoder {
    pub fn new(header: &[u8], stream: Vec<u8>) -> Self {
        let n = u32::from_le_bytes(header[0..4].try_into().unwrap()) as usize;
        let mut off = 4;
        let mut symbols = Vec::with_capacity(n);
        for _ in 0..n {
            symbols.push(u32::from_le_bytes(header[off..off + 4].try_into().unwrap()));
            off += 4;
        }
        let mut pairs: Vec<(u8, u32)> = Vec::with_capacity(n);
        for s in symbols {
            let len = header[off];
            off += 1;
            if len > 0 {
                pairs.push((len, s));
            }
        }
        pairs.sort_unstable();
        let mut first_code = [0u32; 33];
        let mut count = [0u32; 33];
        let mut symbols_by_len: Vec<Vec<u32>> = vec![Vec::new(); 33];
        for (len, s) in &pairs {
            count[*len as usize] += 1;
            symbols_by_len[*len as usize].push(*s);
        }
        // first_code[len] = (first_code[len-1] + count[len-1]) << 1
        let mut code: u32 = 0;
        for len in 1..33 {
            code = (code + count[len - 1]) << 1;
            first_code[len] = code;
        }
        Self {
            first_code,
            count,
            symbols_by_len,
            data: stream,
            pos: 0,
            buf: 0,
            nbits: 0,
        }
    }

    fn read_bit(&mut self) -> bool {
        if self.nbits == 0 {
            if self.pos >= self.data.len() {
                return false;
            }
            self.buf = self.data[self.pos] as u64;
            self.pos += 1;
            self.nbits = 8;
        }
        let bit = (self.buf >> 7) & 1 == 1;
        self.buf = (self.buf << 1) & 0xFF;
        self.nbits -= 1;
        bit
    }

    /// 解码全部符号（count 个）
    pub fn decode(&mut self, count: usize) -> Vec<u32> {
        let mut out = Vec::with_capacity(count);
        let mut code: u32 = 0;
        let mut len: usize = 0;
        for _ in 0..count {
            loop {
                code = (code << 1) | (self.read_bit() as u32);
                len += 1;
                let fc = self.first_code[len];
                let cnt = self.count[len];
                if code >= fc && code < fc + cnt {
                    let idx = (code - fc) as usize;
                    out.push(self.symbols_by_len[len][idx]);
                    code = 0;
                    len = 0;
                    break;
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_canonical_codes() {
        // a:1 b:2 c:3（b 频最高）→ 长度 a=2 b=1 c=2
        // canonical：b="0"，a="10"，c="11"
        let lengths = vec![(1u32, 2u8), (2, 1), (3, 2)];
        let codes = canonical_codes(&lengths);
        assert_eq!(codes[&2], Code { len: 1, bits: 0 }); // b: 0
        assert_eq!(codes[&1], Code { len: 2, bits: 2 }); // a: 10
        assert_eq!(codes[&3], Code { len: 2, bits: 3 }); // c: 11
    }

    #[test]
    fn test_roundtrip() {
        let mut freqs = FxHashMap::default();
        for s in [1u32, 1, 1, 2, 2, 3, 4, 4, 4, 4, 4, 5] {
            *freqs.entry(s).or_insert(0) += 1;
        }
        let enc = HuffmanEncoder::new(&freqs);
        let header = enc.header();
        let symbols = vec![1u32, 2, 3, 4, 5, 1, 2, 4];
        let stream = enc.encode(&symbols);
        let mut dec = HuffmanDecoder::new(&header, stream);
        let out = dec.decode(symbols.len());
        assert_eq!(out, symbols);
    }

    #[test]
    fn test_single_symbol() {
        let mut freqs = FxHashMap::default();
        freqs.insert(7u32, 100);
        let enc = HuffmanEncoder::new(&freqs);
        let header = enc.header();
        let symbols = vec![7u32, 7, 7];
        let stream = enc.encode(&symbols);
        let mut dec = HuffmanDecoder::new(&header, stream);
        let out = dec.decode(symbols.len());
        assert_eq!(out, symbols);
    }

    #[test]
    fn test_skewed() {
        let mut freqs = FxHashMap::default();
        for s in [0u32, 1, 2, 3, 4, 5, 6] {
            *freqs.entry(s).or_insert(0) += 1;
        }
        *freqs.entry(0).or_insert(0) += 1000;
        let enc = HuffmanEncoder::new(&freqs);
        let header = enc.header();
        let mut symbols = Vec::new();
        for i in 0..200 {
            symbols.push(i % 7);
        }
        let stream = enc.encode(&symbols);
        let mut dec = HuffmanDecoder::new(&header, stream);
        let out = dec.decode(symbols.len());
        assert_eq!(out, symbols);
    }
}
