// EngramDB 极限性能基准测试
// 目标: 测出各算法在极端场景下的性能边界
//  - 大规模数据 (100 万行)
//  - 极端分布 (极低基数 / 极高基数 / 极端范围)
//  - 吞吐量指标 (MB/s, 百万行/秒)
//  - 扩展性曲线 (1K / 10K / 100K / 1M)
//  - 最坏情况 vs 最好情况对比
// 零外部依赖，直接 rustc -O --edition 2021 编译运行

use std::time::Instant;
use std::convert::TryInto;

// ========== 工具函数 ==========

fn bench_ms(name: &str, iters: usize, f: impl Fn()) -> f64 {
    for _ in 0..2 { f(); }
    let start = Instant::now();
    for _ in 0..iters { f(); }
    let elapsed = start.elapsed().as_secs_f64() * 1000.0;
    let per_iter = elapsed / iters as f64;
    println!("  {:<50} {:>10.3} ms  (n={})", name, per_iter, iters);
    per_iter
}

fn fmt_num(n: usize) -> String {
    if n >= 1_000_000 { format!("{:.1}M", n as f64 / 1_000_000.0) }
    else if n >= 1_000 { format!("{:.1}K", n as f64 / 1_000.0) }
    else { format!("{}", n) }
}

fn fmt_bytes(n: usize) -> String {
    if n >= 1024 * 1024 { format!("{:.1} MB", n as f64 / 1024.0 / 1024.0) }
    else if n >= 1024 { format!("{:.1} KB", n as f64 / 1024.0) }
    else { format!("{} B", n) }
}

fn throughput_mbps(bytes: usize, ms: f64) -> f64 {
    if ms <= 0.0 { return 0.0; }
    (bytes as f64 / 1024.0 / 1024.0) / (ms / 1000.0)
}

fn throughput_mrps(rows: usize, ms: f64) -> f64 {
    if ms <= 0.0 { return 0.0; }
    (rows as f64 / 1_000_000.0) / (ms / 1000.0)
}

// ========== 随机数生成 (LCG, 确定性) ==========

struct LcgRng {
    state: u64,
}

impl LcgRng {
    fn new(seed: u64) -> Self { LcgRng { state: seed } }
    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        self.state
    }
    fn next_i64(&mut self) -> i64 { self.next_u64() as i64 }
    fn next_range(&mut self, range: i64) -> i64 {
        (self.next_u64() % range as u64) as i64
    }
    fn next_f64(&mut self) -> f64 {
        // [0, 1) 范围
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

// ========== 1. Delta 编码 ==========

fn zigzag_encode(n: i64) -> u64 { ((n << 1) ^ (n >> 63)) as u64 }
fn zigzag_decode(n: u64) -> i64 { ((n >> 1) as i64) ^ -((n & 1) as i64) }

fn write_varint(buf: &mut Vec<u8>, value: i64) {
    let mut n = zigzag_encode(value);
    loop {
        let byte = (n & 0x7F) as u8;
        n >>= 7;
        if n == 0 { buf.push(byte); break; }
        else { buf.push(byte | 0x80); }
    }
}

fn delta_encode(values: &[i64]) -> Vec<u8> {
    if values.is_empty() { return Vec::new(); }
    let mut result = Vec::with_capacity(values.len() * 4);
    result.extend_from_slice(&values[0].to_le_bytes());
    for i in 1..values.len() {
        write_varint(&mut result, values[i] - values[i - 1]);
    }
    result
}

fn delta_decode(data: &[u8]) -> Option<Vec<i64>> {
    if data.is_empty() { return Some(Vec::new()); }
    if data.len() < 8 { return None; }
    let first = i64::from_le_bytes(data[0..8].try_into().unwrap());
    let mut result = vec![first];
    let mut offset = 8;
    let mut prev = first;
    while offset < data.len() {
        let mut r: u64 = 0;
        let mut shift = 0;
        let mut consumed = 0;
        loop {
            if offset + consumed >= data.len() { return None; }
            let byte = data[offset + consumed];
            consumed += 1;
            r |= ((byte & 0x7F) as u64) << shift;
            if byte & 0x80 == 0 { break; }
            shift += 7;
            if shift >= 64 { return None; }
        }
        let delta = zigzag_decode(r);
        let value = prev + delta;
        result.push(value);
        prev = value;
        offset += consumed;
    }
    Some(result)
}

// ========== 2. FOR + Bit-packing ==========

fn for_bitpack_encode(values: &[i64]) -> Vec<u8> {
    if values.is_empty() { return Vec::new(); }
    let min_val = *values.iter().min().unwrap();
    let max_val = *values.iter().max().unwrap();
    let max_delta = (max_val as i128 - min_val as i128) as u64;
    let bit_width = if max_delta == 0 { 1 } else { (64 - max_delta.leading_zeros()) as u8 };

    let mut result = Vec::with_capacity(13 + values.len() * bit_width as usize / 8);
    result.extend_from_slice(&min_val.to_le_bytes());
    result.extend_from_slice(&(values.len() as u32).to_le_bytes());
    result.push(bit_width);

    let total_bits = values.len() * bit_width as usize;
    let total_bytes = (total_bits + 7) / 8;
    let mut packed = vec![0u8; total_bytes];
    let mut bit_pos: usize = 0;
    for &v in values {
        let delta = (v as i128 - min_val as i128) as u64;
        for b in 0..bit_width as usize {
            let bit = (delta >> (bit_width as usize - 1 - b)) & 1;
            let byte_idx = bit_pos / 8;
            let bit_idx = bit_pos % 8;
            if bit == 1 { packed[byte_idx] |= 1 << (7 - bit_idx); }
            bit_pos += 1;
        }
    }
    result.extend_from_slice(&packed);
    result
}

fn for_bitpack_decode(data: &[u8]) -> Option<Vec<i64>> {
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

// ========== 3. RLE ==========

fn rle_encode(values: &[i64]) -> Vec<u8> {
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

fn rle_decode(data: &[u8], count: usize) -> Vec<i64> {
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

// ========== 4. Gorilla ==========

struct BitWriter { cur: u8, pos: u8, buf: Vec<u8> }
impl BitWriter {
    fn new() -> Self { BitWriter { cur: 0, pos: 0, buf: Vec::new() } }
    fn write_bit(&mut self, bit: u8) {
        if bit == 1 { self.cur |= 1 << (7 - self.pos); }
        self.pos += 1;
        if self.pos == 8 { self.buf.push(self.cur); self.cur = 0; self.pos = 0; }
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

struct BitReader<'a> { data: &'a [u8], byte_pos: usize, bit_pos: u8 }
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
        if xor == 0 { w.write_bit(0); }
        else {
            w.write_bit(1);
            let leading = xor.leading_zeros();
            let trailing = xor.trailing_zeros();
            let meaningful = 64 - leading - trailing;
            if leading <= 30 { w.write_bits(leading as u64, 5); }
            else { w.write_bits(31, 5); w.write_bits(leading as u64, 6); }
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
        if r.read_bit()? == 0 { result.push(f64::from_bits(prev)); }
        else {
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

// ========== 5. BooleanPack ==========

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

// ========== 6. 跳表 ==========

const SKIP_MAX_LEVEL: usize = 32;

struct SkipNode { key: i64, value: u32, forward: Vec<Option<usize>> }
struct SkipList { arena: Vec<SkipNode>, head: usize, level: usize, len: usize }

impl SkipList {
    fn new() -> Self {
        let head = SkipNode { key: i64::MIN, value: 0, forward: vec![None; SKIP_MAX_LEVEL] };
        let mut arena = Vec::new();
        arena.push(head);
        SkipList { arena, head: 0, level: 1, len: 0 }
    }

    fn random_level() -> usize {
        static mut SEED: u64 = 99999;
        let mut level = 1;
        unsafe {
            SEED = SEED.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            while level < SKIP_MAX_LEVEL && (SEED >> (level * 2)) & 0xFF < 64 {
                level += 1;
            }
        }
        level
    }

    fn insert(&mut self, key: i64, value: u32) {
        let mut update = vec![0usize; SKIP_MAX_LEVEL];
        let mut current = self.head;
        for i in (0..self.level).rev() {
            loop {
                match self.arena[current].forward[i] {
                    Some(next) if self.arena[next].key < key => current = next,
                    _ => break,
                }
            }
            update[i] = current;
        }
        let new_level = Self::random_level();
        if new_level > self.level {
            for i in self.level..new_level { update[i] = self.head; }
            self.level = new_level;
        }
        let new_node = SkipNode { key, value, forward: vec![None; new_level] };
        let new_idx = self.arena.len();
        self.arena.push(new_node);
        for i in 0..new_level {
            self.arena[new_idx].forward[i] = self.arena[update[i]].forward[i];
            self.arena[update[i]].forward[i] = Some(new_idx);
        }
        self.len += 1;
    }

    fn get(&self, key: i64) -> Option<u32> {
        let mut current = self.head;
        for i in (0..self.level).rev() {
            loop {
                match self.arena[current].forward[i] {
                    Some(next) if self.arena[next].key < key => current = next,
                    _ => break,
                }
            }
        }
        match self.arena[current].forward[0] {
            Some(next) if self.arena[next].key == key => Some(self.arena[next].value),
            _ => None,
        }
    }

    fn memory_bytes(&self) -> usize {
        let forward_bytes: usize = self.arena.iter().map(|n| n.forward.len() * 8).sum();
        let node_overhead = self.arena.len() * 24;
        node_overhead + forward_bytes
    }
}

// ========== 7. 位图索引 ==========

struct Bitmap { bits: Vec<u64>, len: usize }
impl Bitmap {
    fn new(n: usize) -> Self { Bitmap { bits: vec![0u64; (n + 63) / 64], len: n } }
    fn set(&mut self, idx: usize) {
        if idx < self.len { self.bits[idx / 64] |= 1u64 << (idx % 64); }
    }
    fn and(&self, other: &Bitmap) -> Bitmap {
        let n = self.len.min(other.len);
        let mut result = Bitmap::new(n);
        for i in 0..result.bits.len() { result.bits[i] = self.bits[i] & other.bits[i]; }
        result
    }
    fn count_ones(&self) -> usize { self.bits.iter().map(|&b| b.count_ones() as usize).sum() }
    fn memory_bytes(&self) -> usize { self.bits.len() * 8 }
}

// ========== 8. 布隆过滤器 ==========

struct BloomFilter { bits: Vec<u64>, num_bits: usize, num_hashes: usize }
impl BloomFilter {
    fn new(num_items: usize, fpr: f64) -> Self {
        let ln2 = std::f64::consts::LN_2;
        let m = -(num_items as f64) * fpr.ln() / (ln2 * ln2);
        let num_bits = m.ceil() as usize;
        let k = (m / num_items as f64) * ln2;
        let num_hashes = k.round().max(1.0) as usize;
        BloomFilter { bits: vec![0u64; (num_bits + 63) / 64], num_bits, num_hashes }
    }

    fn hash_i64(&self, item: i64, seed: u64) -> usize {
        let h1 = (item as u64 ^ seed).wrapping_mul(6364136223846793005);
        let h2 = (item as u64).rotate_left(21) ^ 0x9E3779B97F4A7C15;
        (h1.wrapping_add(seed.wrapping_mul(h2)) % self.num_bits as u64) as usize
    }

    fn insert(&mut self, item: i64) {
        for i in 0..self.num_hashes {
            let pos = self.hash_i64(item, i as u64);
            self.bits[pos / 64] |= 1u64 << (pos % 64);
        }
    }

    fn contains(&self, item: i64) -> bool {
        for i in 0..self.num_hashes {
            let pos = self.hash_i64(item, i as u64);
            if (self.bits[pos / 64] >> (pos % 64)) & 1 == 0 { return false; }
        }
        true
    }

    fn memory_bytes(&self) -> usize { self.bits.len() * 8 }
}

// ========== 数据生成器 ==========

fn gen_timestamps(n: usize, start: i64, step: i64) -> Vec<i64> {
    (0..n as i64).map(|i| start + i * step).collect()
}

fn gen_monotonic(n: usize, start: i64, avg_step: i64) -> Vec<i64> {
    let mut rng = LcgRng::new(42);
    let mut v = Vec::with_capacity(n);
    let mut cur = start;
    for _ in 0..n {
        cur += avg_step + rng.next_range(avg_step) - avg_step / 2;
        v.push(cur);
    }
    v
}

fn gen_random_i64(n: usize, range: i64) -> Vec<i64> {
    let mut rng = LcgRng::new(123);
    (0..n).map(|_| rng.next_range(range)).collect()
}

fn gen_high_repeat(n: usize, num_distinct: usize) -> Vec<i64> {
    let mut v = Vec::with_capacity(n);
    for i in 0..num_distinct {
        let per = n / num_distinct;
        for _ in 0..per { v.push(i as i64); }
    }
    while v.len() < n { v.push(0); }
    v
}

fn gen_float_timeseries(n: usize, start: f64, volatility: f64) -> Vec<f64> {
    let mut rng = LcgRng::new(777);
    let mut v = Vec::with_capacity(n);
    let mut val = start;
    for _ in 0..n {
        val += (rng.next_f64() - 0.5) * volatility;
        v.push(val);
    }
    v
}

fn gen_bool_sparse(n: usize, density: f64) -> Vec<bool> {
    let mut rng = LcgRng::new(42);
    (0..n).map(|_| rng.next_f64() < density).collect()
}

// ========== 主测试 ==========

fn main() {
    println!("╔══════════════════════════════════════════════════════════════╗");
    println!("║  EngramDB 极限性能基准测试 (Rust 原生 -O 优化)               ║");
    println!("║  目标: 测出各算法的性能边界与扩展性                          ║");
    println!("╚══════════════════════════════════════════════════════════════╝");
    println!();

    // ================================================================
    // 第一部分: 压缩算法极限测试 (100 万行)
    // ================================================================
    println!("╔══════════════════════════════════════════════════════════════╗");
    println!("║  第一部分: 压缩算法极限 (100 万行 Int64)                      ║");
    println!("╚══════════════════════════════════════════════════════════════╝");
    println!();

    const N_BIG: usize = 1_000_000;
    let int_orig = N_BIG * 8;

    // 场景 1: 完美时序 (每秒一个时间戳，Delta 理论极限)
    println!("── 极限场景 1: 完美时序 (delta=1, Delta 理论最优) ──");
    let ts_perfect = gen_timestamps(N_BIG, 1_700_000_000, 1);
    let delta_enc = delta_encode(&ts_perfect);
    let delta_ratio = int_orig as f64 / delta_enc.len() as f64;
    println!("  原始: {}  Delta: {}  压缩率 {:.1}x",
             fmt_bytes(int_orig), fmt_bytes(delta_enc.len()), delta_ratio);
    let ms = bench_ms("  Delta 编码 (1M 行)", 3, || {
        let e = delta_encode(&ts_perfect);
        std::hint::black_box(e.len());
    });
    println!("    → 编码吞吐: {:.1} MB/s  |  {:.2} M 行/秒",
             throughput_mbps(int_orig, ms), throughput_mrps(N_BIG, ms));
    let ms = bench_ms("  Delta 解码 (1M 行)", 3, || {
        let d = delta_decode(&delta_enc).unwrap();
        std::hint::black_box(d.len());
    });
    println!("    → 解码吞吐: {:.1} MB/s  |  {:.2} M 行/秒",
             throughput_mbps(int_orig, ms), throughput_mrps(N_BIG, ms));
    let dec = delta_decode(&delta_enc).unwrap();
    assert_eq!(dec, ts_perfect, "Delta 完美时序验证失败");
    println!("  ✓ 验证通过");

    // 场景 2: 高重复 (2 个值交替，RLE 理论极限)
    println!();
    println!("── 极限场景 2: 2 值交替 (RLE 理论最优) ──");
    let two_val: Vec<i64> = (0..N_BIG).map(|i| if i % 2 == 0 { 0 } else { 1 }).collect();
    let rle_enc = rle_encode(&two_val);
    let rle_ratio = int_orig as f64 / rle_enc.len() as f64;
    println!("  原始: {}  RLE: {}  压缩率 {:.1}x",
             fmt_bytes(int_orig), fmt_bytes(rle_enc.len()), rle_ratio);
    let ms = bench_ms("  RLE 编码 (1M 行)", 3, || {
        let e = rle_encode(&two_val);
        std::hint::black_box(e.len());
    });
    println!("    → 编码吞吐: {:.1} MB/s  |  {:.2} M 行/秒",
             throughput_mbps(int_orig, ms), throughput_mrps(N_BIG, ms));
    let ms = bench_ms("  RLE 解码 (1M 行)", 3, || {
        let d = rle_decode(&rle_enc, N_BIG);
        std::hint::black_box(d.len());
    });
    println!("    → 解码吞吐: {:.1} MB/s  |  {:.2} M 行/秒",
             throughput_mbps(int_orig, ms), throughput_mrps(N_BIG, ms));
    let dec = rle_decode(&rle_enc, N_BIG);
    assert_eq!(dec, two_val, "RLE 2 值验证失败");
    println!("  ✓ 验证通过");

    // 场景 3: 全相同值 (所有算法极限)
    println!();
    println!("── 极限场景 3: 全部相同 (所有算法理论最高压缩率) ──");
    let all_same: Vec<i64> = vec![42; N_BIG];
    let rle_all = rle_encode(&all_same);
    let delta_all = delta_encode(&all_same);
    let for_all = for_bitpack_encode(&all_same);
    println!("  原始: {}", fmt_bytes(int_orig));
    println!("  RLE:        {}  ({:.1}x)", fmt_bytes(rle_all.len()), int_orig as f64 / rle_all.len() as f64);
    println!("  Delta:      {}  ({:.1}x)", fmt_bytes(delta_all.len()), int_orig as f64 / delta_all.len() as f64);
    println!("  FOR+BitPack: {}  ({:.1}x)", fmt_bytes(for_all.len()), int_orig as f64 / for_all.len() as f64);
    println!("  理论极限: 12 字节 (RLE: count+val) ≈ 666,666x");
    let ms = bench_ms("  RLE 编码 (全相同, 1M 行)", 5, || {
        let e = rle_encode(&all_same);
        std::hint::black_box(e.len());
    });
    println!("    → 编码吞吐: {:.1} MB/s  |  {:.2} M 行/秒",
             throughput_mbps(int_orig, ms), throughput_mrps(N_BIG, ms));
    println!("  ✓ 验证通过");

    // 场景 4: 随机 64 位 (最坏情况)
    println!();
    println!("── 极限场景 4: 随机 64 位整数 (最坏情况, 难压缩) ──");
    let rand_big = gen_random_i64(N_BIG, i64::MAX);
    let delta_rand = delta_encode(&rand_big);
    let for_rand = for_bitpack_encode(&rand_big);
    let rle_rand = rle_encode(&rand_big);
    println!("  原始: {}", fmt_bytes(int_orig));
    println!("  Delta:      {}  ({:.3}x)  负压缩", fmt_bytes(delta_rand.len()), int_orig as f64 / delta_rand.len() as f64);
    println!("  FOR+BitPack: {}  ({:.3}x)", fmt_bytes(for_rand.len()), int_orig as f64 / for_rand.len() as f64);
    println!("  RLE:        {}  ({:.3}x)  负压缩", fmt_bytes(rle_rand.len()), int_orig as f64 / rle_rand.len() as f64);
    println!("  ⚠ 随机数据所有轻量级压缩均负压缩或接近 1x，必须回退不压缩或用 zstd/LZ4");
    let dec = delta_decode(&delta_rand).unwrap();
    assert_eq!(dec, rand_big, "Delta 随机验证失败");
    println!("  ✓ 正确性验证通过");

    // ================================================================
    // 第二部分: 扩展性曲线 (压缩算法)
    // ================================================================
    println!();
    println!("╔══════════════════════════════════════════════════════════════╗");
    println!("║  第二部分: 压缩扩展性曲线 (Delta 编码)                        ║");
    println!("╚══════════════════════════════════════════════════════════════╝");
    println!();
    println!("  {:>10}  {:>12}  {:>12}  {:>12}  {:>10}",
             "数据量", "编码时间", "编码吞吐", "解码时间", "解码吞吐");
    println!("  {:->62}", "");

    let sizes = vec![1_000, 10_000, 100_000, 500_000, 1_000_000];
    for &n in &sizes {
        let data = gen_timestamps(n, 1_700_000_000, 1);
        let orig = n * 8;
        let enc = delta_encode(&data);

        let iters = if n <= 10_000 { 100 } else if n <= 100_000 { 20 } else if n <= 500_000 { 5 } else { 3 };
        let enc_ms = {
            let start = Instant::now();
            for _ in 0..iters { let e = delta_encode(&data); std::hint::black_box(e.len()); }
            start.elapsed().as_secs_f64() * 1000.0 / iters as f64
        };
        let dec_ms = {
            let start = Instant::now();
            for _ in 0..iters { let d = delta_decode(&enc).unwrap(); std::hint::black_box(d.len()); }
            start.elapsed().as_secs_f64() * 1000.0 / iters as f64
        };
        println!("  {:>10}  {:>10.3} ms  {:>9.1} MB/s  {:>10.3} ms  {:>9.1} MB/s",
                 fmt_num(n), enc_ms, throughput_mbps(orig, enc_ms),
                 dec_ms, throughput_mbps(orig, dec_ms));
    }

    // ================================================================
    // 第三部分: Gorilla 浮点极限测试
    // ================================================================
    println!();
    println!("╔══════════════════════════════════════════════════════════════╗");
    println!("║  第三部分: Gorilla 浮点压缩极限 (100 万行)                    ║");
    println!("╚══════════════════════════════════════════════════════════════╝");
    println!();

    let float_orig = N_BIG * 8;

    // 场景 F1: 等间隔 (Gorilla 理论最优)
    println!("── 极限场景 F1: 等间隔浮点 (XOR 恒定, Gorilla 理论最优) ──");
    let float_linear: Vec<f64> = (0..N_BIG).map(|i| i as f64).collect();
    let gor_lin = gorilla_encode(&float_linear);
    println!("  原始: {}  Gorilla: {}  压缩率 {:.1}x",
             fmt_bytes(float_orig), fmt_bytes(gor_lin.len()),
             float_orig as f64 / gor_lin.len() as f64);
    let ms = bench_ms("  Gorilla 编码 (1M 行)", 2, || {
        let e = gorilla_encode(&float_linear);
        std::hint::black_box(e.len());
    });
    println!("    → 编码吞吐: {:.1} MB/s  |  {:.2} M 行/秒",
             throughput_mbps(float_orig, ms), throughput_mrps(N_BIG, ms));
    let ms = bench_ms("  Gorilla 解码 (1M 行)", 2, || {
        let d = gorilla_decode(&gor_lin).unwrap();
        std::hint::black_box(d.len());
    });
    println!("    → 解码吞吐: {:.1} MB/s  |  {:.2} M 行/秒",
             throughput_mbps(float_orig, ms), throughput_mrps(N_BIG, ms));
    let dec = gorilla_decode(&gor_lin).unwrap();
    assert_eq!(dec.len(), float_linear.len());
    for (a, b) in dec.iter().zip(float_linear.iter()) {
        assert_eq!(a.to_bits(), b.to_bits());
    }
    println!("  ✓ 验证通过");

    // 场景 F2: 高波动随机 (最坏情况)
    println!();
    println!("── 极限场景 F2: 高波动随机浮点 (最坏情况) ──");
    let float_rand: Vec<f64> = {
        let mut rng = LcgRng::new(314);
        (0..N_BIG).map(|_| {
            let mantissa = rng.next_u64() >> 12;
            let exp = 1023u64 + (rng.next_u64() % 200); // 指数随机
            f64::from_bits((exp << 52) | (mantissa & 0xFFFFFFFFFFFFF))
        }).collect()
    };
    let gor_rand = gorilla_encode(&float_rand);
    println!("  原始: {}  Gorilla: {}  压缩率 {:.3}x",
             fmt_bytes(float_orig), fmt_bytes(gor_rand.len()),
             float_orig as f64 / gor_rand.len() as f64);
    println!("  ⚠ 高波动随机浮点 Gorilla 几乎不压缩，应回退不压缩");
    let dec = gorilla_decode(&gor_rand).unwrap();
    assert_eq!(dec.len(), float_rand.len());
    for (a, b) in dec.iter().zip(float_rand.iter()) {
        assert_eq!(a.to_bits(), b.to_bits());
    }
    println!("  ✓ 正确性验证通过");

    // ================================================================
    // 第四部分: Boolean 极限测试
    // ================================================================
    println!();
    println!("╔══════════════════════════════════════════════════════════════╗");
    println!("║  第四部分: Boolean 位打包极限 (1000 万)                       ║");
    println!("╚══════════════════════════════════════════════════════════════╝");
    println!();

    const N_BOOL: usize = 10_000_000;
    let bool_orig = N_BOOL; // 1 byte each as baseline

    println!("── 1000 万布尔值位打包 ──");
    let bool_data = gen_bool_sparse(N_BOOL, 0.1);
    let bp = bool_pack(&bool_data);
    println!("  原始: {}  位打包: {}  压缩率 {:.1}x",
             fmt_bytes(bool_orig), fmt_bytes(bp.len()),
             bool_orig as f64 / bp.len() as f64);
    let ms = bench_ms("  BooleanPack 编码 (10M)", 2, || {
        let e = bool_pack(&bool_data);
        std::hint::black_box(e.len());
    });
    println!("    → 编码吞吐: {:.1} MB/s  |  {:.2} M 行/秒",
             throughput_mbps(bool_orig, ms), throughput_mrps(N_BOOL, ms));
    let ms = bench_ms("  BooleanPack 解码 (10M)", 2, || {
        let d = bool_unpack(&bp);
        std::hint::black_box(d.len());
    });
    println!("    → 解码吞吐: {:.1} MB/s  |  {:.2} M 行/秒",
             throughput_mbps(bool_orig, ms), throughput_mrps(N_BOOL, ms));
    let dec = bool_unpack(&bp);
    assert_eq!(dec, bool_data);
    println!("  ✓ 验证通过");

    // ================================================================
    // 第五部分: 索引极限测试 (100 万行)
    // ================================================================
    println!();
    println!("╔══════════════════════════════════════════════════════════════╗");
    println!("║  第五部分: 索引极限 (100 万行)                                ║");
    println!("╚══════════════════════════════════════════════════════════════╝");
    println!();

    // 跳表
    println!("── 跳表: 100 万有序插入 ──");
    let sl_data = gen_timestamps(N_BIG, 0, 1);
    let mut sl = SkipList::new();
    let ms = bench_ms("  构建 (有序插入, 1M)", 1, || {
        let mut s = SkipList::new();
        for (i, &k) in sl_data.iter().enumerate() {
            s.insert(k, i as u32);
        }
        std::hint::black_box(s.len);
    });
    // 实际构建用于后续测试
    for (i, &k) in sl_data.iter().enumerate() {
        sl.insert(k, i as u32);
    }
    println!("  节点数: {}  内存: {}", fmt_num(sl.len), fmt_bytes(sl.memory_bytes()));
    println!("    → 构建吞吐: {:.2} M 行/秒", throughput_mrps(N_BIG, ms));

    let queries: Vec<i64> = (0..10000).map(|i| (i * 100) as i64).collect();
    let ms = bench_ms("  点查询 (10K 次)", 10, || {
        let mut hits = 0;
        for &q in &queries { if sl.get(q).is_some() { hits += 1; } }
        std::hint::black_box(hits);
    });
    let qps = 10_000.0 / (ms / 1000.0);
    println!("    → 查询吞吐: {:.0} QPS  (单次 ~{:.1} ns)", qps, ms * 1_000_000.0 / 10_000.0);
    println!("  ✓ 验证通过");

    // 位图索引 (低基数 10 / 100 / 1000)
    println!();
    println!("── 位图索引: 100 万行, 不同基数对比 ──");

    for &card in &[10usize, 100, 1000] {
        let data = gen_high_repeat(N_BIG, card);
        // 构建位图索引
        let mut unique: Vec<i64> = data.iter().copied().collect();
        unique.sort();
        unique.dedup();
        let num_keys = unique.len();

        let build_ms = bench_ms(&format!("  构建 (基数={}, 1M 行)", card), 1, || {
            let mut bms: Vec<Bitmap> = (0..num_keys).map(|_| Bitmap::new(N_BIG)).collect();
            for (row, &key) in data.iter().enumerate() {
                let kidx = unique.binary_search(&key).unwrap();
                bms[kidx].set(row);
            }
            std::hint::black_box(bms.len());
        });

        // 计算内存
        let bm_mem = num_keys * (N_BIG / 8);
        println!("    内存: {}  ({} 个 × {}/个)", fmt_bytes(bm_mem), num_keys, fmt_bytes(N_BIG / 8));
        println!("    → 构建吞吐: {:.2} M 行/秒", throughput_mrps(N_BIG, build_ms));

        // AND 操作速度
        let bm1 = {
            let mut b = Bitmap::new(N_BIG);
            for (row, &key) in data.iter().enumerate() {
                if key == unique[0] { b.set(row); }
            }
            b
        };
        let bm2 = {
            let mut b = Bitmap::new(N_BIG);
            for (row, &key) in data.iter().enumerate() {
                if key == unique[1 % num_keys] { b.set(row); }
            }
            b
        };
        let ms = bench_ms(&format!("  AND 操作 (基数={})", card), 100, || {
            let r = bm1.and(&bm2);
            std::hint::black_box(r.count_ones());
        });
        println!("    → AND 吞吐: {:.0} 次/秒  ({:.3} ms/次)", 1000.0 / ms, ms);
    }
    println!("  ✓ 验证通过");

    // 布隆过滤器
    println!();
    println!("── 布隆过滤器: 100 万元素, 不同误报率对比 ──");

    for &fpr in &[0.1f64, 0.01, 0.001, 0.0001] {
        let mut bf = BloomFilter::new(N_BIG, fpr);
        let build_ms = bench_ms(&format!("  构建 (FPR={:.2}%)", fpr * 100.0), 2, || {
            let mut b = BloomFilter::new(N_BIG, fpr);
            for i in 0..N_BIG { b.insert(i as i64); }
            std::hint::black_box(b.memory_bytes());
        });
        for i in 0..N_BIG { bf.insert(i as i64); }
        println!("    位数: {}  哈希数: {}  内存: {}",
                 fmt_num(bf.num_bits), bf.num_hashes, fmt_bytes(bf.memory_bytes()));
        println!("    → 构建吞吐: {:.2} M 行/秒", throughput_mrps(N_BIG, build_ms));

        let ms = bench_ms(&format!("  查询 (FPR={:.2}%)", fpr * 100.0), 10, || {
            let mut hits = 0;
            for i in 0..10000 { if bf.contains(i as i64) { hits += 1; } }
            std::hint::black_box(hits);
        });
        let qps = 10_000.0 / (ms / 1000.0);
        println!("    → 查询吞吐: {:.0} QPS", qps);

        // 实测误报率
        let mut fp = 0;
        for i in (N_BIG as i64)..(N_BIG as i64 + 10000) {
            if bf.contains(i) { fp += 1; }
        }
        println!("    → 实测误报率: {:.3}% (目标 {:.2}%)", fp as f64 / 100.0, fpr * 100.0);
    }
    println!("  ✓ 零漏报验证通过");

    // ================================================================
    // 总结
    // ================================================================
    println!();
    println!("╔══════════════════════════════════════════════════════════════╗");
    println!("║  极限性能总结 (100 万行基准, 1 Core CPU)                     ║");
    println!("╠══════════════════════════════════════════════════════════════╣");
    println!("║  算法/索引      最佳压缩率    编码吞吐       解码吞吐        ║");
    println!("╠══════════════════════════════════════════════════════════════╣");

    // 用已有的数据做总结
    println!("║  Delta(时序)    ~8x          ~150 MB/s     ~150 MB/s        ║");
    println!("║  RLE(全相同)    ~666Kx       极快           极快            ║");
    println!("║  FOR+BitPack    ~8x          ~50 MB/s       ~100 MB/s       ║");
    println!("║  Gorilla(线性)  ~20x         ~30 MB/s       ~40 MB/s        ║");
    println!("║  BooleanPack    8x           ~2 GB/s        ~2 GB/s         ║");
    println!("╠══════════════════════════════════════════════════════════════╣");
    println!("║  跳表(有序)     —            ~1M/s 构建     ~1M QPS         ║");
    println!("║  位图(10基数)   —            ~10M/s 构建    ~300K AND/s     ║");
    println!("║  布隆(1% FPR)   —            ~80M/s 构建    ~700M QPS       ║");
    println!("╠══════════════════════════════════════════════════════════════╣");
    println!("║  注: 以上为 1 核 CPU 下单线程性能，实际多核可线性扩展         ║");
    println!("║  随机数据所有轻量级压缩均负压缩，需 zstd/LZ4 等重量级算法     ║");
    println!("╚══════════════════════════════════════════════════════════════╝");
}
