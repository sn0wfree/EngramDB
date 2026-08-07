//! 统一预分割器（v0.21）
//!
//! 类别游标扫描：CJK / 字母 / 数字 / 标点 / 空白 / 符号，**同类连续合并成段**。
//! 段即 BPE 编码边界（跨段不合并——GPT-2 起的主流 BPE 预分割原则）。
//! **同一份代码被离线训练（feed process）与运行时（maxmatch 边界）共用**——
//! 训练/运行结构上一致。
//!
//! 与 GPT-4 pat_str 的区别：不用正则（lookahead 在 `regex` crate 不可用），
//! 用字符类别游标扫描，规则简单确定。种子词贪心挂在 CJK 段上（可选）。

/// 预分割类别
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CharClass {
    /// CJK 统一表意文字（含扩展 A-F）及兼容区
    Cjk,
    /// 字母（含拉丁/希腊/西里尔等非 CJK 字母）
    Letter,
    /// 数字
    Digit,
    /// 空白（空格、tab、换行等）
    Space,
    /// 标点（ASCII + Unicode 标点符号 P* 类）
    Punct,
    /// 其他符号（emoji、数学符号、控制字符等）
    Symbol,
}

/// 判定字符类别（训练/运行共享——一致性关键）
pub fn classify(c: char) -> CharClass {
    if is_cjk(c) {
        CharClass::Cjk
    } else if c.is_whitespace() {
        CharClass::Space
    } else if c.is_ascii_digit() {
        CharClass::Digit
    } else if c.is_alphabetic() {
        CharClass::Letter
    } else if is_punct(c) {
        CharClass::Punct
    } else {
        CharClass::Symbol
    }
}

/// CJK 判断：U+3400-U+4DBF（扩展 A）、U+4E00-U+9FFF（基本）、
/// U+F900-U+FAFF（兼容）、U+20000-U+2FA1F（扩展 B-F）
fn is_cjk(c: char) -> bool {
    matches!(
        c as u32,
        0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xF900..=0xFAFF | 0x20000..=0x2FA1F
    )
}

fn is_punct(c: char) -> bool {
    // ASCII 标点
    if c.is_ascii_punctuation() {
        return true;
    }
    // Unicode 通用标点类（P*）
    matches!(
        c as u32,
        0x2000..=0x206F
            | 0x3000..=0x303F
            | 0xFE10..=0xFE1F
            | 0xFE30..=0xFE4F
            | 0xFF00..=0xFF0F
            | 0xFF1A..=0xFF1F
            | 0xFF3B..=0xFF3F
            | 0xFF5B..=0xFF65
    )
}

/// 预分割段：覆盖原文连续区间（字节偏移），无缝隙无重叠
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Piece {
    /// 字节区间 [start, end)
    pub start: usize,
    pub end: usize,
    pub class: CharClass,
}

/// 类别游标预分割：同类连续合并成段（CJK 连续块整体一段，BPE 段内合并）
pub fn segment(text: &str) -> Vec<Piece> {
    let mut pieces = Vec::new();
    let mut start = 0usize;
    let mut start_class: Option<CharClass> = None;

    let mut chars = text.char_indices();
    let mut current = chars.next();
    while let Some((byte_idx, c)) = current {
        let class = classify(c);
        let next = chars.next();
        let end = match next {
            Some((next_idx, _)) => next_idx,
            None => text.len(),
        };

        match start_class {
            None => {
                start = byte_idx;
                start_class = Some(class);
            }
            Some(cur) => {
                if cur != class {
                    pieces.push(Piece { start, end: byte_idx, class: cur });
                    start = byte_idx;
                    start_class = Some(class);
                }
            }
        }
        current = next;
    }
    if let Some(class) = start_class {
        pieces.push(Piece { start, end: text.len(), class });
    }
    pieces
}

/// 训练端 word 划分：segment() 的段 + 可选种子词贪心切分（CJK 段内）。
/// 返回值与段一一对应（含偏移），供离线训练 feed 与差分测试共用。
///
/// 特殊处理：**空白段按单字符切分**——tokenizers 的 merges.txt 用空格分隔行，
/// 含空白字符的 merge（如 "\n\n"、"  "）会破坏格式；空白压缩收益小，
/// 拆单字符保证 merges 导出安全（token 永不含空白）。训练/运行共享本函数 → 两端自动一致。
pub fn segment_words(text: &str, seeds: &[String]) -> Vec<(String, Piece)> {
    let pieces = segment(text);
    let mut out = Vec::new();
    for piece in pieces {
        let slice = &text[piece.start..piece.end];
        if piece.class == CharClass::Cjk && !seeds.is_empty() {
            // CJK 段内种子词贪心最长匹配：词内可合并，未命中单字
            for (word, rel_start, rel_end) in seed_segment(slice, seeds) {
                out.push((
                    word,
                    Piece {
                        start: piece.start + rel_start,
                        end: piece.start + rel_end,
                        class: CharClass::Cjk,
                    },
                ));
            }
        } else if piece.class == CharClass::Space {
            // 空白段：逐字符独立 word（不参与 merges 合并）
            let mut pos = 0usize;
            for c in slice.chars() {
                let len = c.len_utf8();
                out.push((
                    c.to_string(),
                    Piece {
                        start: piece.start + pos,
                        end: piece.start + pos + len,
                        class: CharClass::Space,
                    },
                ));
                pos += len;
            }
        } else {
            out.push((slice.to_string(), piece));
        }
    }
    out
}

/// CJK 段内种子词贪心最长匹配（jieba 风格）
/// 返回 (词文本, 相对字节起点, 相对字节终点)
pub fn seed_segment(text: &str, seeds: &[String]) -> Vec<(String, usize, usize)> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut pos = 0usize; // 字节位置
    while pos < text.len() {
        let rest = &text[pos..];
        let mut best: Option<&str> = None;
        for seed in seeds {
            if rest.starts_with(seed) {
                if best.map_or(true, |b: &str| seed.len() > b.len()) {
                    best = Some(seed);
                }
            }
        }
        match best {
            Some(word) => {
                out.push((word.to_string(), pos, pos + word.len()));
                pos += word.len();
            }
            None => {
                // 单字符（UTF-8 边界安全）
                let ch_len = rest.chars().next().map(|c| c.len_utf8()).unwrap_or(1);
                let word = &rest[..ch_len];
                out.push((word.to_string(), pos, pos + ch_len));
                pos += ch_len;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_segment_english_sentence() {
        let text = "Hello, world!";
        let pieces = segment(text);
        assert!(pieces.len() >= 4);
        assert_eq!(&text[pieces[0].start..pieces[0].end], "Hello");
        assert_eq!(&text[pieces[1].start..pieces[1].end], ",");
        assert_eq!(&text[pieces[2].start..pieces[2].end], " ");
        assert_eq!(&text[pieces[3].start..pieces[3].end], "world");
    }

    #[test]
    fn test_segment_cjk_block_merged() {
        let text = "你好world";
        let pieces = segment(text);
        // CJK 连续块整体一段
        assert_eq!(pieces.len(), 2);
        assert_eq!(&text[pieces[0].start..pieces[0].end], "你好");
        assert_eq!(&text[pieces[1].start..pieces[1].end], "world");
    }

    #[test]
    fn test_segment_coverage() {
        let text = "中文 abc 123，！\n🎉🙂";
        let pieces = segment(text);
        let mut offset = 0;
        for p in &pieces {
            assert_eq!(p.start, offset);
            assert!(p.end > p.start);
            offset = p.end;
        }
        assert_eq!(offset, text.len());
    }

    #[test]
    fn test_segment_digits_punct() {
        let text = "v0.21_alpha-2";
        let pieces = segment(text);
        let joined: String = pieces
            .iter()
            .map(|p| &text[p.start..p.end])
            .collect::<Vec<_>>()
            .join("|");
        assert_eq!(joined, "v|0|.|21|_|alpha|-|2");
    }

    #[test]
    fn test_seed_segment() {
        let seeds = vec!["银行".to_string(), "上海".to_string()];
        let words: Vec<String> = segment_words("上海银行间", &seeds)
            .into_iter()
            .map(|(w, _)| w)
            .collect();
        // 贪心：上海(2) 银行(2) 间(1)
        assert_eq!(words, vec!["上海", "银行", "间"]);
    }

    #[test]
    fn test_seed_segment_no_seed() {
        let (words, pieces) = segment_words("你好abc", &[])
            .into_iter()
            .map(|(w, p)| (w, p))
            .collect::<Vec<_>>()
            .into_iter()
            .unzip::<String, Piece, Vec<String>, Vec<Piece>>();
        assert_eq!(words, vec!["你好", "abc"]);
        assert_eq!(pieces.len(), 2);
    }

    #[test]
    fn test_empty_and_single() {
        assert!(segment("").is_empty());
        let pieces = segment("a");
        assert_eq!(pieces.len(), 1);
        assert_eq!(pieces[0].class, CharClass::Letter);
    }
}
