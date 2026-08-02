// 零依赖核心性能基准测试
// 测试: 列存写入/读取、稀疏索引、向量过滤、两阶段聚合
// 直接用 rustc 编译，无需 cargo 下载依赖

use std::time::{Duration, Instant};
use std::convert::TryInto;

// ========== 基础类型 ==========

#[derive(Debug, Clone, PartialEq)]
enum Value {
    Int(i64),
    Double(f64),
    Null,
}

impl Value {
    fn as_double(&self) -> f64 {
        match self {
            Value::Int(v) => *v as f64,
            Value::Double(v) => *v,
            Value::Null => 0.0,
        }
    }
}

// ========== 列存：ColumnChunk ==========

struct ColumnChunk {
    int_data: Vec<i64>,
    double_data: Vec<f64>,
    is_int: bool,
    row_count: usize,
    min_val: Value,
    max_val: Value,
}

impl ColumnChunk {
    fn new_int(capacity: usize) -> Self {
        ColumnChunk {
            int_data: Vec::with_capacity(capacity),
            double_data: Vec::new(),
            is_int: true,
            row_count: 0,
            min_val: Value::Null,
            max_val: Value::Null,
        }
    }

    fn new_double(capacity: usize) -> Self {
        ColumnChunk {
            int_data: Vec::new(),
            double_data: Vec::with_capacity(capacity),
            is_int: false,
            row_count: 0,
            min_val: Value::Null,
            max_val: Value::Null,
        }
    }

    fn append_int(&mut self, val: i64) {
        self.int_data.push(val);
        self.row_count += 1;
        match &self.min_val {
            Value::Null => {
                self.min_val = Value::Int(val);
                self.max_val = Value::Int(val);
            }
            Value::Int(min) => {
                let max = if let Value::Int(m) = &self.max_val { *m } else { 0 };
                if val < *min { self.min_val = Value::Int(val); }
                if val > max { self.max_val = Value::Int(val); }
            }
            _ => {}
        }
    }

    fn append_double(&mut self, val: f64) {
        self.double_data.push(val);
        self.row_count += 1;
        match &self.min_val {
            Value::Null => {
                self.min_val = Value::Double(val);
                self.max_val = Value::Double(val);
            }
            Value::Double(min) => {
                let max = if let Value::Double(m) = &self.max_val { *m } else { 0.0 };
                if val < *min { self.min_val = Value::Double(val); }
                if val > max { self.max_val = Value::Double(val); }
            }
            _ => {}
        }
    }

    fn get_int(&self, idx: usize) -> i64 {
        self.int_data[idx]
    }

    fn get_double(&self, idx: usize) -> f64 {
        self.double_data[idx]
    }

    fn can_skip_int(&self, low: i64, high: i64) -> bool {
        match (&self.min_val, &self.max_val) {
            (Value::Int(min), Value::Int(max)) => *max < low || *min > high,
            _ => false,
        }
    }

    fn can_skip_double(&self, low: f64, high: f64) -> bool {
        match (&self.min_val, &self.max_val) {
            (Value::Double(min), Value::Double(max)) => *max < low || *min > high,
            _ => false,
        }
    }
}

// ========== 稀疏索引 ==========

const GRANULE_SIZE: usize = 8192;

struct SparseIndex {
    granules: Vec<GranuleEntry>,
}

struct GranuleEntry {
    first_key: i64,
    start_row: usize,
    row_count: usize,
}

impl SparseIndex {
    fn new() -> Self {
        SparseIndex { granules: Vec::new() }
    }

    fn build(&mut self, keys: &[i64]) {
        self.granules.clear();
        let mut i = 0;
        while i < keys.len() {
            let end = std::cmp::min(i + GRANULE_SIZE, keys.len());
            self.granules.push(GranuleEntry {
                first_key: keys[i],
                start_row: i,
                row_count: end - i,
            });
            i = end;
        }
    }

    fn locate_range(&self, low: i64, high: i64) -> Vec<(usize, usize)> {
        let mut result = Vec::new();
        for g in &self.granules {
            let last_key = g.first_key + (g.row_count as i64 - 1);
            if g.first_key <= high && last_key >= low {
                result.push((g.start_row, g.row_count));
            }
        }
        result
    }

    fn granule_count(&self) -> usize {
        self.granules.len()
    }
}

// ========== 向量执行：SelectionVector ==========

struct SelectionVector {
    indices: Vec<usize>,
    count: usize,
}

impl SelectionVector {
    fn all(count: usize) -> Self {
        let indices: Vec<usize> = (0..count).collect();
        SelectionVector { indices, count }
    }

    fn empty(capacity: usize) -> Self {
        SelectionVector { indices: Vec::with_capacity(capacity), count: 0 }
    }

    fn push(&mut self, idx: usize) {
        self.indices.push(idx);
        self.count += 1;
    }

    fn len(&self) -> usize {
        self.count
    }
}

// ========== 两阶段聚合 ==========

enum AggFunc {
    Sum,
    Count,
    Avg,
    Min,
    Max,
}

struct PartialAggState {
    sum: f64,
    count: usize,
    min: f64,
    max: f64,
    func: AggFunc,
}

impl PartialAggState {
    fn new(func: AggFunc) -> Self {
        PartialAggState {
            sum: 0.0,
            count: 0,
            min: f64::MAX,
            max: f64::MIN,
            func,
        }
    }

    fn accumulate_double(&mut self, val: f64) {
        self.sum += val;
        self.count += 1;
        if val < self.min { self.min = val; }
        if val > self.max { self.max = val; }
    }

    fn merge(&mut self, other: &PartialAggState) {
        self.sum += other.sum;
        self.count += other.count;
        if other.min < self.min { self.min = other.min; }
        if other.max > self.max { self.max = other.max; }
    }

    fn finalize(&self) -> Value {
        match self.func {
            AggFunc::Sum => Value::Double(self.sum),
            AggFunc::Count => Value::Int(self.count as i64),
            AggFunc::Avg => {
                if self.count == 0 { Value::Null } else { Value::Double(self.sum / self.count as f64) }
            }
            AggFunc::Min => Value::Double(self.min),
            AggFunc::Max => Value::Double(self.max),
        }
    }
}

// ========== 计时工具 ==========

fn bench(name: &str, iters: usize, f: impl Fn()) -> Duration {
    // warmup
    for _ in 0..2 { f(); }
    // measure
    let start = Instant::now();
    for _ in 0..iters { f(); }
    let elapsed = start.elapsed();
    let per_iter = elapsed / iters as u32;
    println!("  {:<40} {:>12.3} ms/iter  ({} iters, total {:.2}s)",
             name, per_iter.as_secs_f64() * 1000.0, iters, elapsed.as_secs_f64());
    per_iter
}

fn fmt_num(n: usize) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        format!("{}", n)
    }
}

// ========== 主测试 ==========

fn main() {
    println!("╔══════════════════════════════════════════════════════════╗");
    println!("║  HybridDB 核心性能基准测试 (Rust 原生)                   ║");
    println!("╚══════════════════════════════════════════════════════════╝");
    println!();

    const N: usize = 1_000_000; // 100 万行
    println!("数据集: {} 行, 单列 INT + 单列 DOUBLE", fmt_num(N));
    println!();

    // ===== 1. 列存写入 =====
    println!("━━━ 1. 列存写入 ━━━");
    let mut col_int = ColumnChunk::new_int(N);
    let mut col_double = ColumnChunk::new_double(N);
    let mut keys: Vec<i64> = Vec::with_capacity(N);

    let write_time = bench("写入 1M 行 (INT + DOUBLE)", 5, || {
        let mut ci = ColumnChunk::new_int(N);
        let mut cd = ColumnChunk::new_double(N);
        let mut k = Vec::with_capacity(N);
        for i in 0..N {
            let iv = i as i64;
            let dv = (i as f64) * 1.5;
            ci.append_int(iv);
            cd.append_double(dv);
            k.push(iv);
        }
        // 防止优化掉
        std::hint::black_box(ci.row_count);
        std::hint::black_box(cd.row_count);
        std::hint::black_box(k.len());
    });

    // 填充真实数据给后续测试用
    for i in 0..N {
        let iv = i as i64;
        let dv = (i as f64) * 1.5;
        col_int.append_int(iv);
        col_double.append_double(dv);
        keys.push(iv);
    }

    let rows_per_sec = N as f64 / write_time.as_secs_f64();
    println!("     吞吐: {:.1} 行/秒", rows_per_sec);
    println!();

    // ===== 2. 稀疏索引构建 =====
    println!("━━━ 2. 稀疏索引构建 ━━━");
    let mut index = SparseIndex::new();
    let index_build_time = bench("构建稀疏索引 (1M 行, 8192/granule)", 10, || {
        let mut idx = SparseIndex::new();
        idx.build(&keys);
        std::hint::black_box(idx.granule_count());
    });
    index.build(&keys);
    println!("     granule 数量: {}", index.granule_count());
    println!("     内存估算: ~{} bytes", index.granule_count() * 16);
    println!();

    // ===== 3. 稀疏索引范围查询 =====
    println!("━━━ 3. 稀疏索引范围查询 ━━━");

    // 高选择性 (0.1% 数据)
    let low = N as i64 / 2;
    let high = low + (N as i64) / 1000;
    bench("索引定位 (高选择性 ~0.1%)", 1000, || {
        let ranges = index.locate_range(low, high);
        std::hint::black_box(ranges.len());
    });

    // 中选择性 (10% 数据)
    let low = N as i64 / 4;
    let high = low + (N as i64) / 10;
    bench("索引定位 (中选择性 ~10%)", 1000, || {
        let ranges = index.locate_range(low, high);
        std::hint::black_box(ranges.len());
    });

    // 低选择性 (90% 数据)
    let low = (N as i64) / 20;
    let high = (N as i64) * 19 / 20;
    bench("索引定位 (低选择性 ~90%)", 1000, || {
        let ranges = index.locate_range(low, high);
        std::hint::black_box(ranges.len());
    });
    println!();

    // ===== 4. 向量过滤 (SelectionVector) =====
    println!("━━━ 4. 向量过滤 (SelectionVector) ━━━");

    // 全表扫描过滤 (高选择性: 1% 通过)
    let threshold = (N / 100) as i64;
    let filter_time_high = bench("向量过滤 INT (高选择性 ~1%)", 10, || {
        let mut sel = SelectionVector::empty(N);
        for i in 0..N {
            if col_int.get_int(i) < threshold {
                sel.push(i);
            }
        }
        std::hint::black_box(sel.len());
    });
    println!("     吞吐: {:.1} 行/秒", N as f64 / filter_time_high.as_secs_f64());

    // 中选择性: 50% 通过
    let threshold = (N / 2) as i64;
    let filter_time_mid = bench("向量过滤 INT (中选择性 ~50%)", 10, || {
        let mut sel = SelectionVector::empty(N);
        for i in 0..N {
            if col_int.get_int(i) < threshold {
                sel.push(i);
            }
        }
        std::hint::black_box(sel.len());
    });
    println!("     吞吐: {:.1} 行/秒", N as f64 / filter_time_mid.as_secs_f64());

    // 低选择性: 99% 通过
    let threshold = (N * 99 / 100) as i64;
    let filter_time_low = bench("向量过滤 INT (低选择性 ~99%)", 10, || {
        let mut sel = SelectionVector::empty(N);
        for i in 0..N {
            if col_int.get_int(i) < threshold {
                sel.push(i);
            }
        }
        std::hint::black_box(sel.len());
    });
    println!("     吞吐: {:.1} 行/秒", N as f64 / filter_time_low.as_secs_f64());
    println!();

    // ===== 5. PREWHERE 跳读（min-max 跳过 + 向量过滤） =====
    println!("━━━ 5. PREWHERE 跳读 (min-max + 向量过滤) ━━━");

    // 模拟 RowGroup 粒度的 min-max 跳过
    const ROWS_PER_GROUP: usize = 100_000;
    let num_groups = N / ROWS_PER_GROUP;

    // 高选择性: 只有 1 个 group 命中
    let low_val = 5000;
    let high_val = 15000;

    let prewhere_time = bench("PREWHERE 跳读 (高选择性, 1/10 groups)", 50, || {
        let mut total_rows = 0;
        for g in 0..num_groups {
            let start = g * ROWS_PER_GROUP;
            let g_min = col_int.get_int(start);
            let g_max = col_int.get_int(start + ROWS_PER_GROUP - 1);
            if g_max < low_val || g_min > high_val {
                continue; // 跳过整个 group
            }
            // 只扫描命中的 group
            let mut count = 0;
            for i in start..start + ROWS_PER_GROUP {
                if col_int.get_int(i) >= low_val && col_int.get_int(i) <= high_val {
                    count += 1;
                }
            }
            total_rows += count;
        }
        std::hint::black_box(total_rows);
    });

    // 对比: 全表扫描过滤
    let full_scan_time = bench("全表扫描过滤 (无跳读)", 50, || {
        let mut count = 0;
        for i in 0..N {
            let v = col_int.get_int(i);
            if v >= low_val && v <= high_val {
                count += 1;
            }
        }
        std::hint::black_box(count);
    });

    let speedup = full_scan_time.as_secs_f64() / prewhere_time.as_secs_f64();
    println!("     加速比: {:.2}x", speedup);
    println!();

    // ===== 6. 两阶段聚合 =====
    println!("━━━ 6. 两阶段聚合 (Partial + Merge) ━━━");

    // 单阶段聚合 (基线)
    let single_agg_time = bench("单阶段 SUM (全表)", 20, || {
        let mut sum = 0.0;
        for i in 0..N {
            sum += col_double.get_double(i);
        }
        std::hint::black_box(sum);
    });

    // 两阶段聚合 (模拟多核: 分块 partial + merge)
    let num_chunks = 4;
    let chunk_size = N / num_chunks;
    let two_phase_time = bench("两阶段 SUM (4 partial + merge)", 20, || {
        let mut partials: Vec<PartialAggState> = Vec::with_capacity(num_chunks);
        for c in 0..num_chunks {
            let mut state = PartialAggState::new(AggFunc::Sum);
            let start = c * chunk_size;
            let end = start + chunk_size;
            for i in start..end {
                state.accumulate_double(col_double.get_double(i));
            }
            partials.push(state);
        }
        // merge
        let mut final_state = PartialAggState::new(AggFunc::Sum);
        for p in &partials {
            final_state.merge(p);
        }
        std::hint::black_box(final_state.finalize());
    });

    println!("     单阶段吞吐: {:.1} 行/秒", N as f64 / single_agg_time.as_secs_f64());
    println!("     两阶段吞吐: {:.1} 行/秒", N as f64 / two_phase_time.as_secs_f64());
    println!();

    // ===== 7. 序列化/反序列化 (模拟 I/O) =====
    println!("━━━ 7. 列存序列化/反序列化 ━━━");

    // 序列化 INT 列
    let ser_time = bench("序列化 INT 列 (1M 行)", 10, || {
        let mut buf = Vec::with_capacity(N * 8);
        for &v in &col_int.int_data {
            buf.extend_from_slice(&v.to_le_bytes());
        }
        std::hint::black_box(buf.len());
    });
    println!("     吞吐: {:.1} MB/s", (N * 8) as f64 / ser_time.as_secs_f64() / 1024.0 / 1024.0);

    // 反序列化 INT 列
    let raw_bytes: Vec<u8> = col_int.int_data.iter().flat_map(|&v| v.to_le_bytes().to_vec()).collect();
    let deser_time = bench("反序列化 INT 列 (1M 行)", 10, || {
        let mut result = Vec::with_capacity(N);
        let mut i = 0;
        while i < raw_bytes.len() {
            let bytes: [u8; 8] = raw_bytes[i..i+8].try_into().unwrap();
            result.push(i64::from_le_bytes(bytes));
            i += 8;
        }
        std::hint::black_box(result.len());
    });
    println!("     吞吐: {:.1} MB/s", (N * 8) as f64 / deser_time.as_secs_f64() / 1024.0 / 1024.0);
    println!();

    // ===== 总结 =====
    println!("╔══════════════════════════════════════════════════════════╗");
    println!("║  性能总结 (Rust 原生, 单核)                              ║");
    println!("╠══════════════════════════════════════════════════════════╣");
    println!("║  列存写入:  {:>10.1} 行/秒                         ║", rows_per_sec);
    println!("║  向量过滤:  {:>10.1} 行/秒 (高选择性)              ║", N as f64 / filter_time_high.as_secs_f64());
    println!("║  向量过滤:  {:>10.1} 行/秒 (低选择性)              ║", N as f64 / filter_time_low.as_secs_f64());
    println!("║  PREWHERE 加速:  {:>6.2}x (高选择性)                    ║", speedup);
    println!("║  聚合吞吐:  {:>10.1} 行/秒                         ║", N as f64 / single_agg_time.as_secs_f64());
    println!("║  序列化:    {:>10.1} MB/s                            ║", (N * 8) as f64 / ser_time.as_secs_f64() / 1024.0 / 1024.0);
    println!("╚══════════════════════════════════════════════════════════╝");
}
