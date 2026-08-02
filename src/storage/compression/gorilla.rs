//! Gorilla 浮点压缩（XOR 编码）
//!
//! 源自 Facebook Gorilla 论文《Gorilla: A Fast, Scalable, In-Memory Time Series Database》。
//! 核心思想：时序浮点数据相邻值差异很小，异或后高位有大量连续零。
//!
//! 编码方式：
//! - 第一个值直接存储（64 bits）
//! - 后续值存储与前一个值的 XOR 结果
//! - XOR = 0（值相同）：只存 1 个零位
//! - XOR ≠ 0：存储前导零个数 + 有效位长度 + 有效位
//!
//! ClickHouse 对应：Gorilla codec（CODEC_Gorilla = DoubleDelta + XOR）
//! 对时序/单调/低波动浮点数据压缩率极高（通常 2-4×）。

/// Gorilla 编码 f64 序列
///
/// 格式：[count: 4 bytes][first_value: 8 bytes][xor_encoded_bits...]
/// 位流按字节对齐，末尾不足补零。
pub fn encode_f64(values: &[f64]) -> Vec<u8> {
    if values.is_empty() {
        return 0u32.to_le_bytes().to_vec();
    }

    let mut result = Vec::new();
    // 先写数量
    result.extend_from_slice(&(values.len() as u32).to_le_bytes());

    // 第一个值直接存
    let first = values[0].to_bits();
    result.extend_from_slice(&first.to_le_bytes());

    if values.len() == 1 {
        return result;
    }

    // 位写入器
    let mut writer = BitWriter::new();
    let mut prev = first;

    for &val in &values[1..] {
        let curr = val.to_bits();
        let xor = prev ^ curr;

        if xor == 0 {
            // 值相同：写 1 个 0 位
            writer.write_bit(0);
        } else {
            // 值不同：写 1 个 1 位，然后写前导零 + 有效位
            writer.write_bit(1);

            let leading = xor.leading_zeros();
            let trailing = xor.trailing_zeros();
            let meaningful = 64 - leading - trailing;

            // 前导零用 5 位编码（0-30，31 表示扩展）
            if leading <= 30 {
                writer.write_bits(leading as u64, 5);
            } else {
                writer.write_bits(31, 5); // 31 表示 >= 31
                writer.write_bits(leading as u64, 6); // 再用 6 位存完整值（0-63）
            }

            // 有效位长度用 6 位编码（1-64，存的时候 -1 后的值 0-63）
            writer.write_bits((meaningful - 1) as u64, 6);

            // 写有效位（右对齐）
            let meaningful_bits = xor >> trailing;
            writer.write_bits(meaningful_bits, meaningful as u8);
        }

        prev = curr;
    }

    writer.finalize(&mut result);
    result
}

/// Gorilla 解码 f64 序列
pub fn decode_f64(data: &[u8]) -> Option<Vec<f64>> {
    if data.len() < 4 {
        return None;
    }

    let count = u32::from_le_bytes(data[0..4].try_into().unwrap()) as usize;
    if count == 0 {
        return Some(Vec::new());
    }
    if data.len() < 12 {
        return None;
    }

    let first = u64::from_le_bytes(data[4..12].try_into().unwrap());
    let mut result = vec![f64::from_bits(first)];

    if count == 1 {
        return Some(result);
    }

    let mut reader = BitReader::new(&data[12..]);
    let mut prev = first;

    for _ in 1..count {
        if reader.read_bit()? == 0 {
            // XOR = 0，值不变
            result.push(f64::from_bits(prev));
        } else {
            // 读前导零
            let leading = reader.read_bits(5)?;
            let leading = if leading == 31 {
                reader.read_bits(6)? // 扩展 6 位，完整前导零
            } else {
                    leading
                };

            // 读有效位长度
            let meaningful = reader.read_bits(6)? + 1;

            // 读有效位
            let meaningful_bits = reader.read_bits(meaningful as u8)?;

            // 重建 XOR 值
            let trailing = 64 - leading - meaningful;
            let xor = meaningful_bits << trailing;

            let curr = prev ^ xor;
            result.push(f64::from_bits(curr));
            prev = curr;
        }
    }

    Some(result)
}

// ============================================================================
// 位读写辅助
// ============================================================================

struct BitWriter {
    current_byte: u8,
    bit_pos: u8, // 0-7，0 是最高位
    partial: Vec<u8>,
}

impl BitWriter {
    fn new() -> Self {
        BitWriter {
            current_byte: 0,
            bit_pos: 0,
            partial: Vec::new(),
        }
    }

    fn write_bit(&mut self, bit: u8) {
        if bit == 1 {
            self.current_byte |= 1 << (7 - self.bit_pos);
        }
        self.bit_pos += 1;
        if self.bit_pos == 8 {
            self.partial.push(self.current_byte);
            self.current_byte = 0;
            self.bit_pos = 0;
        }
    }

    fn write_bits(&mut self, value: u64, num_bits: u8) {
        for i in 0..num_bits {
            let shift = num_bits - 1 - i;
            let bit = ((value >> shift) & 1) as u8;
            self.write_bit(bit);
        }
    }

    fn finalize(&mut self, buf: &mut Vec<u8>) {
        if !self.partial.is_empty() {
            buf.extend_from_slice(&self.partial);
            self.partial.clear();
        }
        if self.bit_pos > 0 {
            buf.push(self.current_byte);
            self.current_byte = 0;
            self.bit_pos = 0;
        }
    }
}

struct BitReader<'a> {
    data: &'a [u8],
    byte_pos: usize,
    bit_pos: u8, // 0-7，0 是最高位
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        BitReader { data, byte_pos: 0, bit_pos: 0 }
    }

    fn read_bit(&mut self) -> Option<u8> {
        if self.byte_pos >= self.data.len() {
            return None;
        }
        let bit = (self.data[self.byte_pos] >> (7 - self.bit_pos)) & 1;
        self.bit_pos += 1;
        if self.bit_pos >= 8 {
            self.bit_pos = 0;
            self.byte_pos += 1;
        }
        Some(bit)
    }

    fn read_bits(&mut self, num_bits: u8) -> Option<u64> {
        let mut result: u64 = 0;
        for _ in 0..num_bits {
            result = (result << 1) | self.read_bit()? as u64;
        }
        Some(result)
    }
}

// ============================================================================
// 测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty() {
        let encoded = encode_f64(&[]);
        let decoded = decode_f64(&encoded).unwrap();
        assert!(decoded.is_empty());
    }

    #[test]
    fn test_single_value() {
        let values = vec![3.14];
        let encoded = encode_f64(&values);
        assert_eq!(encoded.len(), 12); // 4 + 8
        let decoded = decode_f64(&encoded).unwrap();
        assert_eq!(decoded, values);
    }

    #[test]
    fn test_all_same() {
        // 全部相同，XOR = 0，每个值只占 1 位
        let values: Vec<f64> = vec![42.0; 100];
        let encoded = encode_f64(&values);
        // 4 (count) + 8 (first) + ceil(99/8) ≈ 25 bytes total
        assert!(encoded.len() < 50, "encoded len = {}", encoded.len());
        let decoded = decode_f64(&encoded).unwrap();
        assert_eq!(decoded, values);
    }

    #[test]
    fn test_slowly_changing() {
        // 缓慢变化的浮点数据（典型时序）
        let mut values = Vec::new();
        let mut val = 100.0f64;
        for _ in 0..100 {
            values.push(val);
            val += 0.001;
        }
        let encoded = encode_f64(&values);
        let original_size = values.len() * 8; // 800 bytes
        assert!(encoded.len() < original_size, "no compression: {} >= {}", encoded.len(), original_size);
        let decoded = decode_f64(&encoded).unwrap();
        assert_eq!(decoded.len(), values.len());
        for (a, b) in decoded.iter().zip(values.iter()) {
            assert!((a - b).abs() < 1e-10);
        }
    }

    #[test]
    fn test_integer_like_floats() {
        // 整数值的浮点
        let values: Vec<f64> = (0..100).map(|i| i as f64).collect();
        let encoded = encode_f64(&values);
        let decoded = decode_f64(&encoded).unwrap();
        assert_eq!(decoded, values);
    }

    #[test]
    fn test_negative_values() {
        let values = vec![-1.5, -1.0, -0.5, 0.0, 0.5, 1.0, 1.5];
        let encoded = encode_f64(&values);
        let decoded = decode_f64(&encoded).unwrap();
        assert_eq!(decoded, values);
    }

    #[test]
    fn test_large_values() {
        let values = vec![1e100, 1e100 + 1.0, 1e100 + 2.0, 1e100 + 3.0];
        let encoded = encode_f64(&values);
        let decoded = decode_f64(&encoded).unwrap();
        assert_eq!(decoded, values);
    }

    #[test]
    fn test_nan_and_inf() {
        // NaN 和 Inf 也能正确编码（虽然实际中少见）
        let values = vec![f64::INFINITY, f64::NEG_INFINITY];
        let encoded = encode_f64(&values);
        let decoded = decode_f64(&encoded).unwrap();
        assert_eq!(decoded[0], f64::INFINITY);
        assert_eq!(decoded[1], f64::NEG_INFINITY);
    }

    #[test]
    fn test_random_like() {
        // 随机波动（压缩率低，但必须能正确编解码）
        let values: Vec<f64> = (0..50).map(|i| (i as f64 * 0.123).sin() * 100.0).collect();
        let encoded = encode_f64(&values);
        let decoded = decode_f64(&encoded).unwrap();
        assert_eq!(decoded.len(), values.len());
        for (a, b) in decoded.iter().zip(values.iter()) {
            assert_eq!(a.to_bits(), b.to_bits());
        }
    }

    #[test]
    fn test_many_values() {
        // 整数等间隔浮点数据，XOR 模式一致，压缩率高
        let values: Vec<f64> = (0..1000).map(|i| i as f64).collect();
        let encoded = encode_f64(&values);
        let original_size = values.len() * 8; // 8000 bytes
        // 整数等间隔浮点，XOR 相同，压缩率应该很高
        assert!(encoded.len() < original_size / 2, "compression too low: {} / {}", encoded.len(), original_size);
        let decoded = decode_f64(&encoded).unwrap();
        assert_eq!(decoded.len(), values.len());
        for (a, b) in decoded.iter().zip(values.iter()) {
            assert_eq!(a.to_bits(), b.to_bits());
        }
    }

    #[test]
    fn test_alternating_values() {
        // 交替值（XOR 变化大，压缩率低）
        let values: Vec<f64> = (0..100).map(|i| if i % 2 == 0 { 0.0 } else { 1.0 }).collect();
        let encoded = encode_f64(&values);
        let decoded = decode_f64(&encoded).unwrap();
        assert_eq!(decoded, values);
    }

    #[test]
    fn test_two_values() {
        // 只有两个值
        let values = vec![1.0, 2.0];
        let encoded = encode_f64(&values);
        let decoded = decode_f64(&encoded).unwrap();
        assert_eq!(decoded, values);
    }

    #[test]
    fn test_bit_writer_reader() {
        let mut writer = BitWriter::new();
        writer.write_bit(1);
        writer.write_bit(0);
        writer.write_bit(1);
        writer.write_bits(0b1010, 4);
        writer.write_bits(42, 8);

        let mut buf = Vec::new();
        writer.finalize(&mut buf);

        let mut reader = BitReader::new(&buf);
        assert_eq!(reader.read_bit().unwrap(), 1);
        assert_eq!(reader.read_bit().unwrap(), 0);
        assert_eq!(reader.read_bit().unwrap(), 1);
        assert_eq!(reader.read_bits(4).unwrap(), 0b1010);
        assert_eq!(reader.read_bits(8).unwrap(), 42);
    }

    #[test]
    fn test_bit_writer_many_bits() {
        // 写入超过 8 位，验证字节边界正确
        let mut writer = BitWriter::new();
        // 写 20 个 1
        for _ in 0..20 {
            writer.write_bit(1);
        }
        let mut buf = Vec::new();
        writer.finalize(&mut buf);
        // 20 bits = 2 full bytes (16 bits) + 4 bits = 3 bytes
        assert_eq!(buf.len(), 3);
        assert_eq!(buf[0], 0xFF);
        assert_eq!(buf[1], 0xFF);
        assert_eq!(buf[2], 0xF0); // 高 4 位是 1

        let mut reader = BitReader::new(&buf);
        for i in 0..20 {
            assert_eq!(reader.read_bit().unwrap(), 1, "bit {} failed", i);
        }
    }

    #[test]
    fn test_bit_writer_aligned_bytes() {
        // 刚好整字节
        let mut writer = BitWriter::new();
        for _ in 0..16 {
            writer.write_bit(1);
        }
        let mut buf = Vec::new();
        writer.finalize(&mut buf);
        assert_eq!(buf.len(), 2);
        assert_eq!(buf[0], 0xFF);
        assert_eq!(buf[1], 0xFF);
    }
}
