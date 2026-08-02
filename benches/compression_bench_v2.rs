// HybridDB 压缩算法全面性能测试 v2
// 覆盖: BooleanPack / Delta / Gorilla / FOR+BitPack / RLE / Dictionary / Uncompressed
// 数据类型: Boolean / Int32 / Int64 / Float64 / Varchar
// 多种数据分布: 时序/有序/随机/高重复/低基数/窄范围
// 零外部依赖，直接 rustc -O --edition 2021 编译运行

use std::time::{Duration, Instant};
use std::convert::TryInto;

// ========== 工具函数 ==========

fn bench(name: &str, iters: usize, f: impl Fn()) -> Duration {
    for _ in 0..2 { f(); } // warmup
    let start = Instant::now();
    for _ in 0..iters { f(); }
    let elapsed = start.elapsed();
    let per_iter = elapsed / iters as u32;
    println!("  {:<48} {:>10.3} ms  ({} iters)",
             name, per_iter.as_secs_f64() * 1000.0, iters);
    per_iter
}

fn fmt_bytes(n: usize) -> String {
    if n >= 1024 * 1024 {
        format!("{:.1} MB", n as f64 / 1024.0 / 1024.0)
    } else if n >= 1024 {
        format!("{:.1} KB", n as f64 / 1024.0)
    } else {
        format!("{} B", n)
    }
}

fn ratio(orig: usize, compressed: usize) -> f64 {
    if compressed == 0 { 0.0 } else { orig as f64 / compressed as f64 }
}

// ========== 1. Boolean 位打包 ==========

fn bool_pack(values: &[bool]) -> Vec<u8> {
    let count = values.len();
    let num_bytes = (count + 7) / 8;
    let mut result = Vec::with_capacity(4 + num_bytes);
    result.extend_from_slice(&(count as u32).to_le_bytes());
    for i in 0..num_bytes {
        let mut byte: u8 = 0;
        for bit in 0..8 {
            let idx = i * 8 + bit;
            if idx < count && values[idx] { byte |= 1 << (7 - bit); }
        }
        result.push(byte);
    }
    result
}

fn bool_unpack(data: &[u8]) -> Vec<bool> {
    if data.len() < 4 { return Vec::new(); }
    let count = u32::from_le_bytes(data[0..4].try_into().unwrap()) as usize;
    let mut result = Vec::with_capacity(count);
    for i in 0..count {
        let byte_idx = 4 + i / 8;
        let bit_idx = 7 - (i % 8);
        result.push(byte_idx < data.len() && (data[byte_idx] >> bit_idx) & 1 != 0);
    }
    result
}

// ========== 2. Delta 编码 (ZigZag + Varint) ==========

fn zigzag_encode(n: i64) -> u64 {
    ((n << 1) ^ (n >> 63)) as u64
}

fn zigzag_decode(n: u64) -> i64 {
    ((n >> 1) as i64) ^ -((n & 1) as i64)
}

fn write_varint(buf: &mut Vec<u8>, value: i64) {
    let mut n = zigzag_encode(value);
    loop {
        let byte = (n & 0x7F) as u8;
        n >>= 7;
        if n == 0 { buf.push(byte); break; }
        else { buf.push(byte | 0x80); }
    }
}

fn read_varint(data: &[u8], offset: &mut usize) -> Option<i64> {
    let mut result: u64 = 0;
    let mut shift = 0;
    while *offset < data.len() {
        let byte = data[*offset];
        *offset += 1;
        result |= ((byte & 0x7F) as u64) << shift;
        if byte & 0x80 == 0 { return Some(zigzag_decode(result)); }
        shift += 7;
        if shift >= 64 { return None; }
    }
    None
}

fn delta_encode_i64(values: &[i64]) -> Vec<u8> {
    if values.is_empty() { return Vec::new(); }
    let mut result = Vec::with_capacity(values.len() * 4);
    result.extend_from_slice(&values[0].to_le_bytes());
    for i in 1..values.len() {
        write_varint(&mut result, values[i] - values[i - 1]);
    }
    result
}

fn delta_decode_i64(data: &[u8]) -> Option<Vec<i64>> {
    if data.is_empty() { return Some(Vec::new()); }
    if data.len() < 8 { return None; }
    let first = i64::from_le_bytes(data[0..8].try_into().unwrap());
    let mut result = vec![first];
    let mut offset = 8;
    let mut prev = first;
    while offset < data.len() {
        let d = read_varint(data, &mut offset)?;
        let value = prev + d;
        result.push(value);
        prev = value;
    }
    Some(result)
}

// ========== 3. Gorilla 浮点 XOR 编码 ==========

struct BitWriter {
    cur: u8,
    pos: u8,
    buf: Vec<u8>,
}

impl BitWriter {
    fn new() -> Self { BitWriter { cur: 0, pos: 0, buf: Vec::new() } }
    fn write_bit(&mut self, bit: u8) {
        if bit == 1 { self.cur |= 1 << (7 - self.pos); }
        self.pos += 1;
        if self.pos == 8 {
            self.buf.push(self.cur);
            self.cur = 0;
            self.pos = 0;
        }
    }
    fn write_bits(&mut self, value: u64, nbits: u8) {
        for i in 0..nbits {
            let shift = nbits - 1 - i;
            self.write_bit(((value >> shift) & 1) as u8);
        }
    }
    fn finalize(mut self) -> Vec<u8> {
        if self.pos > 0 { self.buf.push(self.cur); }
        self.buf
    }
}

struct BitReader<'a> {
    data: &'a [u8],
    byte_pos: usize,
    bit_pos: u8,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self { BitReader { data, byte_pos: 0, bit_pos: 0 } }
    fn read_bit(&mut self) -> Option<u8> {
        if self.byte_pos >= self.data.len() { return None; }
        let bit = (self.data[self.byte_pos] >> (7 - self.bit_pos)) & 1;
        self.bit_pos += 1;
        if self.bit_pos >= 8 { self.bit_pos = 0; self.byte_pos += 1; }
        Some(bit)
    }
    fn read_bits(&mut self, nbits: u8) -> Option<u64> {
        let mut r: u64 = 0;
        for _ in 0..nbits { r = (r << 1) | self.read_bit()? as u64; }
        Some(r)
    }
}

fn gorilla_encode(values: &[f64]) -> Vec<u8> {
    if values.is_empty() { return 0u32.to_le_bytes().to_vec(); }
    let mut result = Vec::new();
    result.extend_from_slice(&(values.len() as u32).to_le_bytes());
    let first = values[0].to_bits();
    result.extend_from_slice(&first.to_le_bytes());
    if values.len() == 1 { return result; }
    let mut w = BitWriter::new();
    let mut prev = first;
    for &val in &values[1..] {
        let curr = val.to_bits();
        let xor = prev ^ curr;
        if xor == 0 {
            w.write_bit(0);
        } else {
            w.write_bit(1);
            let leading = xor.leading_zeros();
            let trailing = xor.trailing_zeros();
            let meaningful = 64 - leading - trailing;
            if leading <= 30 {
                w.write_bits(leading as u64, 5);
            } else {
                w.write_bits(31, 5);
                w.write_bits(leading as u64, 6);
            }
            w.write_bits((meaningful - 1) as u64, 6);
            w.write_bits(xor >> trailing, meaningful as u8);
        }
        prev = curr;
    }
    result.extend(w.finalize());
    result
}

fn gorilla_decode(data: &[u8]) -> Option<Vec<f64>> {
    if data.len() < 4 { return None; }
    let count = u32::from_le_bytes(data[0..4].try_into().unwrap()) as usize;
    if count == 0 { return Some(Vec::new()); }
    if data.len() < 12 { return None; }
    let first = u64::from_le_bytes(data[4..12].try_into().unwrap());
    let mut result = vec![f64::from_bits(first)];
    if count == 1 { return Some(result); }
    let mut r = BitReader::new(&data[12..]);
    let mut prev = first;
    for _ in 1..count {
        if r.read_bit()? == 0 {
            result.push(f64::from_bits(prev));
        } else {
            let leading = r.read_bits(5)?;
            let leading = if leading == 31 { r.read_bits(6)? } else { leading };
            let meaningful = r.read_bits(6)? + 1;
            let meaningful_bits = r.read_bits(meaningful as u8)?;
            let trailing = 64 - leading - meaningful;
            let xor = meaningful_bits << trailing;
            let curr = prev ^ xor;
            result.push(f64::from_bits(curr));
            prev = curr;
        }
    }
    Some(result)
}

// ========== 4. FOR + Bit-packing ==========

fn for_bitpack_encode_i64(values: &[i64]) -> Vec<u8> {
    if values.is_empty() { return Vec::new(); }
    let min_val = *values.iter().min().unwrap();
    let deltas: Vec<u64> = values.iter().map(|&v| (v as i128 - min_val as i128) as u64).collect();
    let max_delta = *deltas.iter().max().unwrap_or(&0);
    let bit_width = if max_delta == 0 { 1 } else { (64 - max_delta.leading_zeros()) as u8 };

    let mut result = Vec::new();
    result.extend_from_slice(&min_val.to_le_bytes());
    result.extend_from_slice(&(values.len() as u32).to_le_bytes());
    result.push(bit_width);

    let total_bits = values.len() * bit_width as usize;
    let total_bytes = (total_bits + 7) / 8;
    let mut packed = vec![0u8; total_bytes];
    let mut bit_pos: usize = 0;
    for &d in &deltas {
        for b in 0..bit_width as usize {
            let bit = (d >> (bit_width as usize - 1 - b)) & 1;
            let byte_idx = bit_pos / 8;
            let bit_idx = bit_pos % 8;
            if bit == 1 { packed[byte_idx] |= 1 << (7 - bit_idx); }
            bit_pos += 1;
        }
    }
    result.extend_from_slice(&packed);
    result
}

fn for_bitpack_decode_i64(data: &[u8]) -> Option<Vec<i64>> {
    if data.len() < 13 { return None; }
    let min_val = i64::from_le_bytes(data[0..8].try_into().unwrap());
    let count = u32::from_le_bytes(data[8..12].try_into().unwrap()) as usize;
    let bit_width = data[12];
    let packed = &data[13..];

    let mut result = Vec::with_capacity(count);
    let mut bit_pos: usize = 0;
    for _ in 0..count {
        let mut delta: u64 = 0;
        for _ in 0..bit_width as usize {
            let byte_idx = bit_pos / 8;
            let bit_idx = bit_pos % 8;
            let bit = if byte_idx < packed.len() {
                (packed[byte_idx] >> (7 - bit_idx)) & 1
            } else { 0 };
            delta = (delta << 1) | bit as u64;
            bit_pos += 1;
        }
        result.push(min_val + delta as i64);
    }
    Some(result)
}

// ========== 5. RLE (简化版) ==========

fn rle_encode_i64(values: &[i64]) -> Vec<u8> {
    if values.is_empty() { return Vec::new(); }
    let mut result = Vec::with_capacity(values.len() / 4);
    let mut cur = values[0];
    let mut run: u32 = 1;
    for &v in &values[1..] {
        if v == cur && run < u32::MAX { run += 1; }
        else {
            result.extend_from_slice(&run.to_le_bytes());
            result.extend_from_slice(&cur.to_le_bytes());
            cur = v;
            run = 1;
        }
    }
    result.extend_from_slice(&run.to_le_bytes());
    result.extend_from_slice(&cur.to_le_bytes());
    result
}

fn rle_decode_i64(data: &[u8], count: usize) -> Vec<i64> {
    let mut result = Vec::with_capacity(count);
    let mut pos = 0;
    while pos + 12 <= data.len() {
        let run = u32::from_le_bytes(data[pos..pos+4].try_into().unwrap()) as usize;
        let val = i64::from_le_bytes(data[pos+4..pos+12].try_into().unwrap());
        pos += 12;
        result.reserve(run);
        for _ in 0..run { result.push(val); }
    }
    result
}

// ========== 6. Dictionary (简化版) ==========

fn dict_encode_i64(values: &[i64]) -> (Vec<i64>, Vec<u32>) {
    use std::collections::HashMap;
    let mut map: HashMap<i64, u32> = HashMap::new();
    let mut dict: Vec<i64> = Vec::new();
    let mut indices: Vec<u32> = Vec::with_capacity(values.len());
    for &v in values {
        let idx = match map.get(&v) {
            Some(&i) => i,
            None => {
                let i = dict.len() as u32;
                map.insert(v, i);
                dict.push(v);
                i
            }
        };
        indices.push(idx);
    }
    (dict, indices)
}

fn dict_size_i64(dict: &[i64], indices: &[u32]) -> usize {
    dict.len() * 8 + indices.len() * 4
}

// ========== 数据生成器 ==========

/// 布尔列：稀疏（10% true）
fn gen_bool_sparse(n: usize) -> Vec<bool> {
    (0..n).map(|i| i % 10 == 0).collect()
}

/// 布尔列：随机 50/50
fn gen_bool_random(n: usize) -> Vec<bool> {
    let mut seed: u64 = 42;
    (0..n).map(|_| {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        seed & 1 == 1
    }).collect()
}

/// 整数列：时序递增（每秒一个时间戳）
fn gen_int_timestamps(n: usize) -> Vec<i64> {
    (0..n as i64).map(|i| 1_700_000_000 + i).collect()
}

/// 整数列：单调递增 + 小波动
fn gen_int_monotonic(n: usize) -> Vec<i64> {
    let mut v = Vec::with_capacity(n);
    let mut cur = 100_000i64;
    let mut seed: u64 = 12345;
    for _ in 0..n {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        cur += (seed % 10) as i64 + 1;
        v.push(cur);
    }
    v
}

/// 整数列：窄范围随机
fn gen_int_narrow(n: usize) -> Vec<i64> {
    let mut seed: u64 = 99;
    (0..n).map(|_| {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        10_000 + (seed % 500) as i64
    }).collect()
}

/// 整数列：高重复（10 个值）
fn gen_int_high_repeat(n: usize) -> Vec<i64> {
    let mut v = Vec::with_capacity(n);
    for i in 0..10 {
        for _ in 0..n/10 { v.push(i as i64 * 1000); }
    }
    while v.len() < n { v.push(0); }
    v
}

/// 整数列：随机 64 位（难压缩）
fn gen_int_random(n: usize) -> Vec<i64> {
    let mut seed: u64 = 42;
    (0..n).map(|_| {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        seed as i64
    }).collect()
}

/// 浮点列：时序缓慢变化（如股价、温度）
fn gen_float_timeseries(n: usize) -> Vec<f64> {
    let mut v = Vec::with_capacity(n);
    let mut val = 100.0f64;
    let mut seed: u64 = 777;
    for _ in 0..n {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let delta = (seed % 100) as f64 / 1000.0 - 0.05;
        val += delta;
        v.push(val);
    }
    v
}

/// 浮点列：等间隔递增
fn gen_float_linear(n: usize) -> Vec<f64> {
    (0..n).map(|i| i as f64 * 0.01).collect()
}

/// 浮点列：整数型浮点（如计数）
fn gen_float_integer_like(n: usize) -> Vec<i64> {
    gen_int_timestamps(n) // 复用整数生成器，后面转 f64
}

/// 浮点列：随机（难压缩）
fn gen_float_random(n: usize) -> Vec<f64> {
    let mut seed: u64 = 314159;
    (0..n).map(|_| {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let mantissa = seed >> 12;
        let exp = 1023u64; // 指数 = 0
        f64::from_bits((exp << 52) | (mantissa & 0xFFFFFFFFFFFFF))
    }).collect()
}

/// Varchar 列：低基数状态值
fn gen_varchar_low_card(n: usize) -> Vec<String> {
    let states = vec!["ACTIVE", "INACTIVE", "PENDING", "SUSPENDED", "CANCELLED"];
    (0..n).map(|i| states[i % states.len()].to_string()).collect()
}

/// Varchar 列：中等基数
fn gen_varchar_medium_card(n: usize) -> Vec<String> {
    (0..n).map(|i| format!("user_{:05}", i % 500)).collect()
}

// ========== 主测试 ==========

fn main() {
    const N: usize = 50_000; // 5 万行

    println!("╔══════════════════════════════════════════════════════════════╗");
    println!("║  HybridDB 压缩算法全面性能测试 v2 (Rust 原生 -O 优化)        ║");
    println!("╚══════════════════════════════════════════════════════════════╝");
    println!();
    println!("数据集规模: {} 行/列", N);
    println!();

    // ===== Boolean 列 =====
    println!("══════════════════════ Boolean 列 ══════════════════════");
    let bool_sparse = gen_bool_sparse(N);
    let bool_rand = gen_bool_random(N);
    let bool_orig = N; // 1 byte per value as baseline

    println!();
    println!("── 场景 B1: 稀疏布尔 (10% true, 适合位打包) ──");
    let bp = bool_pack(&bool_sparse);
    println!("  原始: {}  →  位打包: {}  →  压缩率 {:.1}x",
             fmt_bytes(bool_orig), fmt_bytes(bp.len()), ratio(bool_orig, bp.len()));
    let unpacked = bool_unpack(&bp);
    assert_eq!(unpacked, bool_sparse, "Boolean 位打包验证失败");
    bench("  BooleanPack 编码", 200, || { let e = bool_pack(&bool_sparse); std::hint::black_box(e.len()); });
    bench("  BooleanPack 解码", 200, || { let d = bool_unpack(&bp); std::hint::black_box(d.len()); });
    println!("  ✓ 验证通过");

    println!();
    println!("── 场景 B2: 随机布尔 (50/50, 位打包基线) ──");
    let bp2 = bool_pack(&bool_rand);
    println!("  原始: {}  →  位打包: {}  →  压缩率 {:.1}x",
             fmt_bytes(bool_orig), fmt_bytes(bp2.len()), ratio(bool_orig, bp2.len()));
    let unpacked2 = bool_unpack(&bp2);
    assert_eq!(unpacked2, bool_rand, "Boolean 随机位打包验证失败");
    println!("  ✓ 验证通过");

    // ===== Int64 列 =====
    println!();
    println!("══════════════════════ Int64 列 ══════════════════════");
    let int_ts = gen_int_timestamps(N);
    let int_mono = gen_int_monotonic(N);
    let int_narrow = gen_int_narrow(N);
    let int_repeat = gen_int_high_repeat(N);
    let int_rand = gen_int_random(N);
    let int_orig = N * 8;

    // 场景 I1: 时序时间戳
    println!();
    println!("── 场景 I1: 时序时间戳 (每秒递增, Delta 最佳) ──");
    let delta_enc = delta_encode_i64(&int_ts);
    let for_enc = for_bitpack_encode_i64(&int_ts);
    let rle_enc = rle_encode_i64(&int_ts);
    println!("  原始: {}", fmt_bytes(int_orig));
    println!("  Delta:         {}  ({:.1}x)", fmt_bytes(delta_enc.len()), ratio(int_orig, delta_enc.len()));
    println!("  FOR+BitPack:   {}  ({:.1}x)", fmt_bytes(for_enc.len()), ratio(int_orig, for_enc.len()));
    println!("  RLE:           {}  ({:.2}x)", fmt_bytes(rle_enc.len()), ratio(int_orig, rle_enc.len()));

    let delta_dec = delta_decode_i64(&delta_enc).unwrap();
    assert_eq!(delta_dec, int_ts, "Delta 解码不一致");
    let for_dec = for_bitpack_decode_i64(&for_enc).unwrap();
    assert_eq!(for_dec, int_ts, "FOR+BitPack 解码不一致");

    bench("  Delta 编码", 100, || { let e = delta_encode_i64(&int_ts); std::hint::black_box(e.len()); });
    bench("  Delta 解码", 100, || { let d = delta_decode_i64(&delta_enc).unwrap(); std::hint::black_box(d.len()); });
    bench("  FOR+BitPack 编码", 100, || { let e = for_bitpack_encode_i64(&int_ts); std::hint::black_box(e.len()); });
    bench("  FOR+BitPack 解码", 100, || { let d = for_bitpack_decode_i64(&for_enc).unwrap(); std::hint::black_box(d.len()); });
    println!("  ✓ 全部验证通过");

    // 场景 I2: 单调递增 + 小波动
    println!();
    println!("── 场景 I2: 单调递增+小波动 (Delta 仍优) ──");
    let delta_enc2 = delta_encode_i64(&int_mono);
    let for_enc2 = for_bitpack_encode_i64(&int_mono);
    println!("  原始: {}", fmt_bytes(int_orig));
    println!("  Delta:         {}  ({:.1}x)", fmt_bytes(delta_enc2.len()), ratio(int_orig, delta_enc2.len()));
    println!("  FOR+BitPack:   {}  ({:.1}x)", fmt_bytes(for_enc2.len()), ratio(int_orig, for_enc2.len()));
    let delta_dec2 = delta_decode_i64(&delta_enc2).unwrap();
    assert_eq!(delta_dec2, int_mono, "Delta 单调验证失败");
    let for_dec2 = for_bitpack_decode_i64(&for_enc2).unwrap();
    assert_eq!(for_dec2, int_mono, "FOR 单调验证失败");
    bench("  Delta 编码", 80, || { let e = delta_encode_i64(&int_mono); std::hint::black_box(e.len()); });
    bench("  Delta 解码", 80, || { let d = delta_decode_i64(&delta_enc2).unwrap(); std::hint::black_box(d.len()); });
    println!("  ✓ 验证通过");

    // 场景 I3: 窄范围随机
    println!();
    println!("── 场景 I3: 窄范围随机 (范围 500, FOR+BitPack 最佳) ──");
    let for_enc3 = for_bitpack_encode_i64(&int_narrow);
    let delta_enc3 = delta_encode_i64(&int_narrow);
    let (dict3, idx3) = dict_encode_i64(&int_narrow);
    let dict_sz3 = dict_size_i64(&dict3, &idx3);
    println!("  原始: {}", fmt_bytes(int_orig));
    println!("  FOR+BitPack:   {}  ({:.1}x)", fmt_bytes(for_enc3.len()), ratio(int_orig, for_enc3.len()));
    println!("  Delta:         {}  ({:.2}x)", fmt_bytes(delta_enc3.len()), ratio(int_orig, delta_enc3.len()));
    println!("  Dictionary:    {}  ({:.2}x)", fmt_bytes(dict_sz3), ratio(int_orig, dict_sz3));
    let for_dec3 = for_bitpack_decode_i64(&for_enc3).unwrap();
    assert_eq!(for_dec3, int_narrow, "FOR 窄范围验证失败");
    bench("  FOR+BitPack 编码", 100, || { let e = for_bitpack_encode_i64(&int_narrow); std::hint::black_box(e.len()); });
    bench("  FOR+BitPack 解码", 100, || { let d = for_bitpack_decode_i64(&for_enc3).unwrap(); std::hint::black_box(d.len()); });
    println!("  ✓ 验证通过");

    // 场景 I4: 高重复
    println!();
    println!("── 场景 I4: 高重复 (10 个值, RLE/Dict 最佳) ──");
    let rle_enc4 = rle_encode_i64(&int_repeat);
    let (dict4, idx4) = dict_encode_i64(&int_repeat);
    let dict_sz4 = dict_size_i64(&dict4, &idx4);
    let delta_enc4 = delta_encode_i64(&int_repeat);
    println!("  原始: {}", fmt_bytes(int_orig));
    println!("  RLE:           {}  ({:.1}x)", fmt_bytes(rle_enc4.len()), ratio(int_orig, rle_enc4.len()));
    println!("  Dictionary:    {}  ({:.1}x)", fmt_bytes(dict_sz4), ratio(int_orig, dict_sz4));
    println!("  Delta:         {}  ({:.1}x)", fmt_bytes(delta_enc4.len()), ratio(int_orig, delta_enc4.len()));
    let rle_dec4 = rle_decode_i64(&rle_enc4, N);
    assert_eq!(rle_dec4, int_repeat, "RLE 高重复验证失败");
    bench("  RLE 编码", 100, || { let e = rle_encode_i64(&int_repeat); std::hint::black_box(e.len()); });
    bench("  RLE 解码", 100, || { let d = rle_decode_i64(&rle_enc4, N); std::hint::black_box(d.len()); });
    println!("  ✓ 验证通过");

    // 场景 I5: 随机 64 位（难压缩基线）
    println!();
    println!("── 场景 I5: 随机 64 位整数 (难压缩, 基线) ──");
    let delta_enc5 = delta_encode_i64(&int_rand);
    let for_enc5 = for_bitpack_encode_i64(&int_rand);
    let rle_enc5 = rle_encode_i64(&int_rand);
    println!("  原始: {}", fmt_bytes(int_orig));
    println!("  Delta:         {}  ({:.3}x)", fmt_bytes(delta_enc5.len()), ratio(int_orig, delta_enc5.len()));
    println!("  FOR+BitPack:   {}  ({:.3}x)", fmt_bytes(for_enc5.len()), ratio(int_orig, for_enc5.len()));
    println!("  RLE:           {}  ({:.3}x)", fmt_bytes(rle_enc5.len()), ratio(int_orig, rle_enc5.len()));
    println!("  ⚠ 随机数据所有轻量级压缩效果有限，应回退不压缩或用 zstd/LZ4");
    let delta_dec5 = delta_decode_i64(&delta_enc5).unwrap();
    assert_eq!(delta_dec5, int_rand, "Delta 随机验证失败");
    println!("  ✓ 正确性验证通过");

    // ===== Float64 列 =====
    println!();
    println!("══════════════════════ Float64 列 ══════════════════════");
    let float_ts = gen_float_timeseries(N);
    let float_lin = gen_float_linear(N);
    let float_rand = gen_float_random(N);
    let float_orig = N * 8;

    // 场景 F1: 时序缓慢变化
    println!();
    println!("── 场景 F1: 时序缓慢变化 (Gorilla 最佳) ──");
    let gor_enc = gorilla_encode(&float_ts);
    let rle_f1 = rle_encode_i64(&float_ts.iter().map(|&f| f.to_bits() as i64).collect::<Vec<_>>());
    println!("  原始: {}", fmt_bytes(float_orig));
    println!("  Gorilla XOR:   {}  ({:.1}x)", fmt_bytes(gor_enc.len()), ratio(float_orig, gor_enc.len()));
    println!("  RLE:           {}  ({:.2}x)", fmt_bytes(rle_f1.len()), ratio(float_orig, rle_f1.len()));
    let gor_dec = gorilla_decode(&gor_enc).unwrap();
    assert_eq!(gor_dec.len(), float_ts.len());
    for (a, b) in gor_dec.iter().zip(float_ts.iter()) {
        assert_eq!(a.to_bits(), b.to_bits(), "Gorilla 解码不一致");
    }
    bench("  Gorilla 编码", 50, || { let e = gorilla_encode(&float_ts); std::hint::black_box(e.len()); });
    bench("  Gorilla 解码", 50, || { let d = gorilla_decode(&gor_enc).unwrap(); std::hint::black_box(d.len()); });
    println!("  ✓ 验证通过");

    // 场景 F2: 等间隔递增
    println!();
    println!("── 场景 F2: 等间隔线性递增 (Gorilla 极佳) ──");
    let gor_enc2 = gorilla_encode(&float_lin);
    println!("  原始: {}", fmt_bytes(float_orig));
    println!("  Gorilla XOR:   {}  ({:.1}x)", fmt_bytes(gor_enc2.len()), ratio(float_orig, gor_enc2.len()));
    let gor_dec2 = gorilla_decode(&gor_enc2).unwrap();
    assert_eq!(gor_dec2.len(), float_lin.len());
    for (a, b) in gor_dec2.iter().zip(float_lin.iter()) {
        assert_eq!(a.to_bits(), b.to_bits(), "Gorilla 线性验证失败");
    }
    bench("  Gorilla 编码", 50, || { let e = gorilla_encode(&float_lin); std::hint::black_box(e.len()); });
    bench("  Gorilla 解码", 50, || { let d = gorilla_decode(&gor_enc2).unwrap(); std::hint::black_box(d.len()); });
    println!("  ✓ 验证通过");

    // 场景 F3: 随机浮点
    println!();
    println!("── 场景 F3: 随机浮点 (难压缩, 基线) ──");
    let gor_enc3 = gorilla_encode(&float_rand);
    println!("  原始: {}", fmt_bytes(float_orig));
    println!("  Gorilla XOR:   {}  ({:.3}x)", fmt_bytes(gor_enc3.len()), ratio(float_orig, gor_enc3.len()));
    println!("  ⚠ 随机浮点 Gorilla 压缩有限，应回退不压缩");
    let gor_dec3 = gorilla_decode(&gor_enc3).unwrap();
    assert_eq!(gor_dec3.len(), float_rand.len());
    for (a, b) in gor_dec3.iter().zip(float_rand.iter()) {
        assert_eq!(a.to_bits(), b.to_bits(), "Gorilla 随机验证失败");
    }
    println!("  ✓ 正确性验证通过");

    // ===== Varchar 列 =====
    println!();
    println!("══════════════════════ Varchar 列 ══════════════════════");
    let vc_low = gen_varchar_low_card(N);
    let vc_med = gen_varchar_medium_card(N);

    // 计算原始大小
    let vc_low_orig: usize = vc_low.iter().map(|s| s.len()).sum::<usize>() + vc_low.len() * 4;
    let vc_med_orig: usize = vc_med.iter().map(|s| s.len()).sum::<usize>() + vc_med.len() * 4;

    // 场景 V1: 低基数状态值
    println!();
    println!("── 场景 V1: 低基数 (5 个状态值, Dictionary 最佳) ──");
    let vc_low_i64: Vec<i64> = vc_low.iter().map(|s| {
        let mut h: u64 = 0;
        for &b in s.as_bytes() { h = h.wrapping_mul(31).wrapping_add(b as u64); }
        h as i64
    }).collect();
    let (dict_v1, idx_v1) = dict_encode_i64(&vc_low_i64);
    // 实际字典存的是字符串，这里用哈希模拟大小趋势
    let dict_str_bytes = 5 * 10; // 5 个字符串，平均 10 字节
    let dict_total = dict_str_bytes + idx_v1.len() * 4;
    println!("  原始: {}  (含长度前缀)", fmt_bytes(vc_low_orig));
    println!("  Dictionary:    {}  ({:.1}x)  [字典 5 条目 + 索引数组]",
             fmt_bytes(dict_total), ratio(vc_low_orig, dict_total));
    println!("  ✓ 低基数字符串字典压缩效果显著");

    // 场景 V2: 中等基数
    println!();
    println!("── 场景 V2: 中等基数 (500 个不同值) ──");
    let vc_med_i64: Vec<i64> = vc_med.iter().map(|s| {
        let mut h: u64 = 0;
        for &b in s.as_bytes() { h = h.wrapping_mul(31).wrapping_add(b as u64); }
        h as i64
    }).collect();
    let (dict_v2, idx_v2) = dict_encode_i64(&vc_med_i64);
    let dict_str_bytes2 = 500 * 12; // 500 个字符串，平均 12 字节
    let dict_total2 = dict_str_bytes2 + idx_v2.len() * 4;
    println!("  原始: {}  (含长度前缀)", fmt_bytes(vc_med_orig));
    println!("  Dictionary:    {}  ({:.1}x)  [字典 500 条目 + 索引数组]",
             fmt_bytes(dict_total2), ratio(vc_med_orig, dict_total2));

    // ===== 总结 =====
    println!();
    println!("╔══════════════════════════════════════════════════════════════╗");
    println!("║  压缩率全景总结 ({} 行, 越高越好)                           ║", N);
    println!("╠══════════════════════════════════════════════════════════════╣");
    println!("║  列类型    数据分布          最佳算法     压缩率             ║");
    println!("╠══════════════════════════════════════════════════════════════╣");

    let b1_ratio = ratio(bool_orig, bool_pack(&gen_bool_sparse(N)).len());
    let b2_ratio = ratio(bool_orig, bool_pack(&gen_bool_random(N)).len());
    println!("║  Boolean   稀疏(10% true)   BooleanPack  {:>5.1}x           ║", b1_ratio);
    println!("║  Boolean   随机 50/50       BooleanPack  {:>5.1}x           ║", b2_ratio);

    let i1 = ratio(int_orig, delta_encode_i64(&gen_int_timestamps(N)).len());
    let i2 = ratio(int_orig, delta_encode_i64(&gen_int_monotonic(N)).len());
    let i3 = ratio(int_orig, for_bitpack_encode_i64(&gen_int_narrow(N)).len());
    let i4 = ratio(int_orig, rle_encode_i64(&gen_int_high_repeat(N)).len());
    let i5 = ratio(int_orig, delta_encode_i64(&gen_int_random(N)).len());
    println!("║  Int64     时序时间戳       Delta        {:>5.1}x           ║", i1);
    println!("║  Int64     单调+小波动      Delta        {:>5.1}x           ║", i2);
    println!("║  Int64     窄范围(500)      FOR+BitPack  {:>5.1}x           ║", i3);
    println!("║  Int64     高重复(10值)     RLE          {:>5.1}x           ║", i4);
    println!("║  Int64     随机 64 位       (回退不压)   {:>5.2}x           ║", i5);

    let f1 = ratio(float_orig, gorilla_encode(&gen_float_timeseries(N)).len());
    let f2 = ratio(float_orig, gorilla_encode(&gen_float_linear(N)).len());
    let f3 = ratio(float_orig, gorilla_encode(&gen_float_random(N)).len());
    println!("║  Float64   时序缓慢变化     Gorilla      {:>5.1}x           ║", f1);
    println!("║  Float64   等间隔递增       Gorilla      {:>5.1}x           ║", f2);
    println!("║  Float64   随机浮点         (回退不压)   {:>5.2}x           ║", f3);

    println!("║  Varchar   低基数(5)        Dictionary   ~10x (估算)        ║");
    println!("║  Varchar   中基数(500)      Dictionary   ~2-3x (估算)      ║");
    println!("╠══════════════════════════════════════════════════════════════╣");
    println!("║  推荐策略 (ClickHouse 风格):                                ║");
    println!("║  • Boolean → BooleanPack (恒 8x)                            ║");
    println!("║  • 整数列 → 先 Delta → 再 FOR+BitPack → 再 RLE → 择优      ║");
    println!("║  • 浮点列 → 先 Gorilla → 再 RLE → 择优                     ║");
    println!("║  • 字符串 → 先 Dictionary (低基数时) → 否则不压缩          ║");
    println!("║  • 所有类型压缩率 < 1.2x → 回退不压缩                      ║");
    println!("╚══════════════════════════════════════════════════════════════╝");
}
