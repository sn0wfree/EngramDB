// 轻量级压缩算法基准测试
// 测试: RLE / Dictionary / Bit-packing (FOR) 的压缩率和编解码速度
// 零外部依赖，直接 rustc -O --edition 2021 编译

use std::time::{Duration, Instant};
use std::convert::TryInto;

// ========== 工具函数 ==========

fn bench(name: &str, iters: usize, f: impl Fn()) -> Duration {
    for _ in 0..2 { f(); } // warmup
    let start = Instant::now();
    for _ in 0..iters { f(); }
    let elapsed = start.elapsed();
    let per_iter = elapsed / iters as u32;
    println!("  {:<45} {:>10.3} ms  ({} iters)",
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

// ========== RLE ==========

fn rle_encode_i64(values: &[i64]) -> Vec<u8> {
    if values.is_empty() { return Vec::new(); }
    let mut result = Vec::with_capacity(values.len() / 4);
    let mut current_val = values[0];
    let mut run_len: u32 = 1;
    for &val in &values[1..] {
        if val == current_val && run_len < u32::MAX {
            run_len += 1;
        } else {
            result.extend_from_slice(&run_len.to_le_bytes());
            result.extend_from_slice(&current_val.to_le_bytes());
            current_val = val;
            run_len = 1;
        }
    }
    result.extend_from_slice(&run_len.to_le_bytes());
    result.extend_from_slice(&current_val.to_le_bytes());
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

// ========== Dictionary ==========

struct DictEncoded {
    dict: Vec<i64>,
    indices: Vec<u8>,
    width: u8,
}

fn dict_encode_i64(values: &[i64]) -> DictEncoded {
    use std::collections::HashMap;
    let mut map: HashMap<i64, u32> = HashMap::new();
    let mut dict: Vec<i64> = Vec::new();
    let mut indices: Vec<u32> = Vec::with_capacity(values.len());
    for &val in values {
        let idx = match map.get(&val) {
            Some(&i) => i,
            None => {
                let i = dict.len() as u32;
                map.insert(val, i);
                dict.push(val);
                i
            }
        };
        indices.push(idx);
    }
    let width: u8 = if dict.len() <= 256 { 1 } else if dict.len() <= 65536 { 2 } else { 4 };
    let mut packed = Vec::with_capacity(values.len() * width as usize);
    match width {
        1 => for &idx in &indices { packed.push(idx as u8); },
        2 => for &idx in &indices { packed.extend_from_slice(&(idx as u16).to_le_bytes()); },
        4 => for &idx in &indices { packed.extend_from_slice(&idx.to_le_bytes()); },
        _ => unreachable!(),
    }
    DictEncoded { dict, indices: packed, width }
}

fn dict_decode_i64(enc: &DictEncoded, count: usize) -> Vec<i64> {
    let mut result = Vec::with_capacity(count);
    match enc.width {
        1 => for i in 0..count { result.push(enc.dict[enc.indices[i] as usize]); },
        2 => for i in 0..count {
            let idx = u16::from_le_bytes(enc.indices[i*2..i*2+2].try_into().unwrap()) as usize;
            result.push(enc.dict[idx]);
        },
        4 => for i in 0..count {
            let idx = u32::from_le_bytes(enc.indices[i*4..i*4+4].try_into().unwrap()) as usize;
            result.push(enc.dict[idx]);
        },
        _ => unreachable!(),
    }
    result
}

fn dict_total_bytes(enc: &DictEncoded) -> usize {
    enc.dict.len() * 8 + enc.indices.len()
}

// ========== Bit-packing / FOR ==========

struct BitPacked {
    frame: i64,
    bit_width: u8,
    count: usize,
    data: Vec<u8>,
}

fn bitpack_encode_i64(values: &[i64]) -> BitPacked {
    if values.is_empty() { return BitPacked { frame: 0, bit_width: 0, count: 0, data: Vec::new() }; }
    let mut min_val = values[0];
    let mut max_val = values[0];
    for &v in &values[1..] {
        if v < min_val { min_val = v; }
        if v > max_val { max_val = v; }
    }
    let frame = min_val;
    let max_delta = (max_val - min_val) as u64;
    let bit_width = if max_delta == 0 { 1 } else { (64 - max_delta.leading_zeros()) as u8 };
    let total_bits = values.len() * bit_width as usize;
    let total_bytes = (total_bits + 7) / 8;
    let mut data = vec![0u8; total_bytes];
    let mut bit_pos: usize = 0;
    for &v in values {
        let delta = (v - frame) as u64;
        for b in 0..bit_width as usize {
            let bit = (delta >> b) & 1;
            let byte_idx = (bit_pos + b) / 8;
            let bit_idx = (bit_pos + b) % 8;
            if bit == 1 { data[byte_idx] |= 1 << bit_idx; }
        }
        bit_pos += bit_width as usize;
    }
    BitPacked { frame, bit_width, count: values.len(), data }
}

fn bitpack_decode_i64(packed: &BitPacked) -> Vec<i64> {
    let mut result = Vec::with_capacity(packed.count);
    let mask = if packed.bit_width == 64 { u64::MAX } else { (1u64 << packed.bit_width) - 1 };
    let mut bit_pos: usize = 0;
    for _ in 0..packed.count {
        let mut delta: u64 = 0;
        for b in 0..packed.bit_width as usize {
            let byte_idx = (bit_pos + b) / 8;
            let bit_idx = (bit_pos + b) % 8;
            let bit = (packed.data[byte_idx] >> bit_idx) & 1;
            if bit == 1 { delta |= 1 << b; }
        }
        delta &= mask;
        result.push(packed.frame + delta as i64);
        bit_pos += packed.bit_width as usize;
    }
    result
}

// ========== 生成测试数据 ==========

/// 生成高重复数据（适合 RLE）：10 个值，每个重复 10000 次
fn gen_high_repeat(n: usize) -> Vec<i64> {
    let mut v = Vec::with_capacity(n);
    let num_distinct = 10;
    let per_val = n / num_distinct;
    for i in 0..num_distinct {
        for _ in 0..per_val { v.push(i as i64); }
    }
    while v.len() < n { v.push(0); }
    v
}

/// 生成低基数数据（适合 Dictionary）：200 个不同值随机分布
fn gen_low_cardinality(n: usize) -> Vec<i64> {
    let mut v = Vec::with_capacity(n);
    let num_distinct = 200;
    for i in 0..n {
        v.push((i % num_distinct) as i64 * 100);
    }
    v
}

/// 生成窄范围递增数据（适合 Bit-packing）：范围 0-9999
fn gen_narrow_range(n: usize) -> Vec<i64> {
    let mut v = Vec::with_capacity(n);
    for i in 0..n {
        v.push(10000 + (i as i64 % 10000));
    }
    v
}

/// 生成随机 64 位整数（难压缩，基线）
fn gen_random_i64(n: usize) -> Vec<i64> {
    let mut v = Vec::with_capacity(n);
    let mut seed: u64 = 42;
    for _ in 0..n {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        v.push(seed as i64);
    }
    v
}

// ========== 主测试 ==========

fn main() {
    const N: usize = 100_000; // 10 万行

    println!("╔══════════════════════════════════════════════════════════╗");
    println!("║  轻量级压缩算法基准测试 (Rust 原生)                       ║");
    println!("╚══════════════════════════════════════════════════════════╝");
    println!();
    println!("数据集: {} 行 INT 列", N);
    println!("原始大小: {}", fmt_bytes(N * 8));
    println!();

    // ===== 场景1: 高重复数据 =====
    println!("━━━ 场景1: 高重复数据 (10 个不同值, 适合 RLE) ━━━");
    let data = gen_high_repeat(N);
    let orig_bytes = N * 8;

    // RLE
    let rle_enc = rle_encode_i64(&data);
    let rle_ratio = orig_bytes as f64 / rle_enc.len() as f64;
    println!("  压缩率:");
    println!("    RLE:          {:.1}x  ({})", rle_ratio, fmt_bytes(rle_enc.len()));

    let dict_enc = dict_encode_i64(&data);
    let dict_size = dict_total_bytes(&dict_enc);
    let dict_ratio = orig_bytes as f64 / dict_size as f64;
    println!("    Dictionary:   {:.1}x  ({})", dict_ratio, fmt_bytes(dict_size));

    let bp_enc = bitpack_encode_i64(&data);
    let bp_size = bp_enc.data.len() + 17;
    let bp_ratio = orig_bytes as f64 / bp_size as f64;
    println!("    Bit-packing:  {:.1}x  ({})  (bit_width={})", bp_ratio, fmt_bytes(bp_size), bp_enc.bit_width);

    println!("  编码速度:");
    bench("  RLE encode", 50, || { let e = rle_encode_i64(&data); std::hint::black_box(e.len()); });
    bench("  Dict encode", 50, || { let e = dict_encode_i64(&data); std::hint::black_box(dict_total_bytes(&e)); });
    bench("  BitPack encode", 50, || { let e = bitpack_encode_i64(&data); std::hint::black_box(e.data.len()); });

    println!("  解码速度:");
    let rle_data = rle_encode_i64(&data);
    let dict_data = dict_encode_i64(&data);
    let bp_data = bitpack_encode_i64(&data);
    bench("  RLE decode", 50, || { let d = rle_decode_i64(&rle_data, N); std::hint::black_box(d.len()); });
    bench("  Dict decode", 50, || { let d = dict_decode_i64(&dict_data, N); std::hint::black_box(d.len()); });
    bench("  BitPack decode", 50, || { let d = bitpack_decode_i64(&bp_data); std::hint::black_box(d.len()); });

    // 验证正确性
    let rle_dec = rle_decode_i64(&rle_data, N);
    let dict_dec = dict_decode_i64(&dict_data, N);
    let bp_dec = bitpack_decode_i64(&bp_data);
    assert_eq!(rle_dec, data, "RLE 解码不一致");
    assert_eq!(dict_dec, data, "Dict 解码不一致");
    assert_eq!(bp_dec, data, "BitPack 解码不一致");
    println!("  ✓ 三种算法解码验证通过");
    println!();

    // ===== 场景2: 低基数数据 =====
    println!("━━━ 场景2: 低基数数据 (200 个不同值, 适合 Dictionary) ━━━");
    let data = gen_low_cardinality(N);

    let rle_enc = rle_encode_i64(&data);
    let rle_ratio = orig_bytes as f64 / rle_enc.len() as f64;
    println!("  压缩率:");
    println!("    RLE:          {:.2}x  ({})", rle_ratio, fmt_bytes(rle_enc.len()));

    let dict_enc = dict_encode_i64(&data);
    let dict_size = dict_total_bytes(&dict_enc);
    let dict_ratio = orig_bytes as f64 / dict_size as f64;
    println!("    Dictionary:   {:.1}x  ({})", dict_ratio, fmt_bytes(dict_size));

    let bp_enc = bitpack_encode_i64(&data);
    let bp_size = bp_enc.data.len() + 17;
    let bp_ratio = orig_bytes as f64 / bp_size as f64;
    println!("    Bit-packing:  {:.1}x  ({})  (bit_width={})", bp_ratio, fmt_bytes(bp_size), bp_enc.bit_width);

    println!("  编码速度:");
    bench("  RLE encode", 30, || { let e = rle_encode_i64(&data); std::hint::black_box(e.len()); });
    bench("  Dict encode", 30, || { let e = dict_encode_i64(&data); std::hint::black_box(dict_total_bytes(&e)); });
    bench("  BitPack encode", 30, || { let e = bitpack_encode_i64(&data); std::hint::black_box(e.data.len()); });

    println!("  解码速度:");
    let rle_data = rle_encode_i64(&data);
    let dict_data = dict_encode_i64(&data);
    let bp_data = bitpack_encode_i64(&data);
    bench("  RLE decode", 30, || { let d = rle_decode_i64(&rle_data, N); std::hint::black_box(d.len()); });
    bench("  Dict decode", 30, || { let d = dict_decode_i64(&dict_data, N); std::hint::black_box(d.len()); });
    bench("  BitPack decode", 30, || { let d = bitpack_decode_i64(&bp_data); std::hint::black_box(d.len()); });

    let dict_dec = dict_decode_i64(&dict_data, N);
    let bp_dec = bitpack_decode_i64(&bp_data);
    assert_eq!(dict_dec, data, "Dict 解码不一致");
    assert_eq!(bp_dec, data, "BitPack 解码不一致");
    println!("  ✓ 解码验证通过");
    println!();

    // ===== 场景3: 窄范围递增 =====
    println!("━━━ 场景3: 窄范围递增 (范围 10000-19999, 适合 Bit-packing) ━━━");
    let data = gen_narrow_range(N);

    let rle_enc = rle_encode_i64(&data);
    let rle_ratio = orig_bytes as f64 / rle_enc.len() as f64;
    println!("  压缩率:");
    println!("    RLE:          {:.2}x  ({})", rle_ratio, fmt_bytes(rle_enc.len()));

    let dict_enc = dict_encode_i64(&data);
    let dict_size = dict_total_bytes(&dict_enc);
    let dict_ratio = orig_bytes as f64 / dict_size as f64;
    println!("    Dictionary:   {:.2}x  ({})", dict_ratio, fmt_bytes(dict_size));

    let bp_enc = bitpack_encode_i64(&data);
    let bp_size = bp_enc.data.len() + 17;
    let bp_ratio = orig_bytes as f64 / bp_size as f64;
    println!("    Bit-packing:  {:.1}x  ({})  (bit_width={})", bp_ratio, fmt_bytes(bp_size), bp_enc.bit_width);

    println!("  编码速度:");
    bench("  RLE encode", 30, || { let e = rle_encode_i64(&data); std::hint::black_box(e.len()); });
    bench("  Dict encode", 30, || { let e = dict_encode_i64(&data); std::hint::black_box(dict_total_bytes(&e)); });
    bench("  BitPack encode", 30, || { let e = bitpack_encode_i64(&data); std::hint::black_box(e.data.len()); });

    println!("  解码速度:");
    let rle_data = rle_encode_i64(&data);
    let dict_data = dict_encode_i64(&data);
    let bp_data = bitpack_encode_i64(&data);
    bench("  RLE decode", 30, || { let d = rle_decode_i64(&rle_data, N); std::hint::black_box(d.len()); });
    bench("  Dict decode", 30, || { let d = dict_decode_i64(&dict_data, N); std::hint::black_box(d.len()); });
    bench("  BitPack decode", 30, || { let d = bitpack_decode_i64(&bp_data); std::hint::black_box(d.len()); });

    let rle_dec = rle_decode_i64(&rle_data, N);
    let dict_dec = dict_decode_i64(&dict_data, N);
    let bp_dec = bitpack_decode_i64(&bp_data);
    assert_eq!(rle_dec, data, "RLE 解码不一致");
    assert_eq!(dict_dec, data, "Dict 解码不一致");
    assert_eq!(bp_dec, data, "BitPack 解码不一致");
    println!("  ✓ 三种算法解码验证通过");
    println!();

    // ===== 场景4: 随机数据（难压缩） =====
    println!("━━━ 场景4: 随机 64 位整数 (难压缩, 基线) ━━━");
    let data = gen_random_i64(N);

    let rle_enc = rle_encode_i64(&data);
    let rle_ratio = orig_bytes as f64 / rle_enc.len() as f64;
    println!("  压缩率:");
    println!("    RLE:          {:.3}x  ({})  (负压缩)", rle_ratio, fmt_bytes(rle_enc.len()));

    let dict_enc = dict_encode_i64(&data);
    let dict_size = dict_total_bytes(&dict_enc);
    let dict_ratio = orig_bytes as f64 / dict_size as f64;
    println!("    Dictionary:   {:.3}x  ({})  (负压缩)", dict_ratio, fmt_bytes(dict_size));

    let bp_enc = bitpack_encode_i64(&data);
    let bp_size = bp_enc.data.len() + 17;
    let bp_ratio = orig_bytes as f64 / bp_size as f64;
    println!("    Bit-packing:  {:.3}x  ({})  (bit_width={})", bp_ratio, fmt_bytes(bp_size), bp_enc.bit_width);
    println!("  ⚠ 随机数据三种轻量级压缩均无效，应回退为不压缩或用 zstd");
    println!();

    // ===== 总结 =====
    println!("╔══════════════════════════════════════════════════════════╗");
    println!("║  压缩率总结 (越高越好)                                    ║");
    println!("╠══════════════════════════════════════════════════════════╣");

    let high_rle = { let e = rle_encode_i64(&gen_high_repeat(N)); orig_bytes as f64 / e.len() as f64 };
    let high_dict = { let e = dict_encode_i64(&gen_high_repeat(N)); orig_bytes as f64 / dict_total_bytes(&e) as f64 };
    let high_bp = { let e = bitpack_encode_i64(&gen_high_repeat(N)); orig_bytes as f64 / (e.data.len() + 17) as f64 };

    let low_dict = { let e = dict_encode_i64(&gen_low_cardinality(N)); orig_bytes as f64 / dict_total_bytes(&e) as f64 };
    let low_bp = { let e = bitpack_encode_i64(&gen_low_cardinality(N)); orig_bytes as f64 / (e.data.len() + 17) as f64 };

    let narrow_bp = { let e = bitpack_encode_i64(&gen_narrow_range(N)); orig_bytes as f64 / (e.data.len() + 17) as f64 };

    println!("║  高重复(10值)  RLE {:>6.1}x  Dict {:>5.1}x  BP {:>5.1}x  ║", high_rle, high_dict, high_bp);
    println!("║  低基数(200值)  RLE  ~1.0x  Dict {:>5.1}x  BP {:>5.1}x  ║", low_dict, low_bp);
    println!("║  窄范围(10K)    RLE  ~1.0x  Dict  ~1.0x  BP {:>5.1}x  ║", narrow_bp);
    println!("║  随机64位       全部负压缩，应回退不压缩或 zstd          ║");
    println!("╠══════════════════════════════════════════════════════════╣");
    println!("║  推荐策略：按列自动选择最佳压缩算法                       ║");
    println!("║  1. 先试 RLE → 2. 再试 Dict → 3. 再试 BitPack           ║");
    println!("║  4. 都 < 1.2x 则不压缩（或用 zstd 重量级压缩）           ║");
    println!("╚══════════════════════════════════════════════════════════╝");
}
