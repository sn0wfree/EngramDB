//! EngramDB 原生性能基准测试
//!
//! 纯 Rust 实现，对比列存（EngramDB 架构）vs 行存（传统架构）的核心操作性能。
//! 全部为原生编译，无解释器、无外部依赖。

use rand::Rng;
use rustc_hash::FxHashMap;
use std::time::Instant;

const NUM_ROWS: usize = 100_000;
const JOIN_TABLE_SIZE: usize = 10_000;

// ============================================================
// 行存结构（模拟 SQLite / 传统行存数据库）
// ============================================================

#[derive(Clone)]
struct EmployeeRow {
    id: i64,
    name: String,
    age: i32,
    salary: f64,
    department: String,
    city: String,
}

// ============================================================
// 列存结构（EngramDB 架构）
// ============================================================

struct ColumnarEmployees {
    ids: Vec<i64>,
    names: Vec<String>,
    ages: Vec<i32>,
    salaries: Vec<f64>,
    departments: Vec<String>,
    cities: Vec<String>,
}

impl ColumnarEmployees {
    fn len(&self) -> usize { self.ids.len() }
}

// ============================================================
// 数据生成
// ============================================================

fn generate_rows(n: usize) -> Vec<EmployeeRow> {
    let depts: Vec<String> = (0..10).map(|i| format!("dept_{}", i)).collect();
    let cities: Vec<String> = (0..50).map(|i| format!("city_{}", i)).collect();
    (0..n)
        .map(|i| EmployeeRow {
            id: i as i64,
            name: format!("user_{}", i),
            age: 20 + (i % 45) as i32,
            salary: 30000.0 + (i * 7 % 170000) as f64,
            department: depts[i % 10].clone(),
            city: cities[i % 50].clone(),
        })
        .collect()
}

fn rows_to_columnar(rows: &[EmployeeRow]) -> ColumnarEmployees {
    let n = rows.len();
    let mut ids = Vec::with_capacity(n);
    let mut names = Vec::with_capacity(n);
    let mut ages = Vec::with_capacity(n);
    let mut salaries = Vec::with_capacity(n);
    let mut departments = Vec::with_capacity(n);
    let mut cities = Vec::with_capacity(n);
    for e in rows {
        ids.push(e.id);
        names.push(e.name.clone());
        ages.push(e.age);
        salaries.push(e.salary);
        departments.push(e.department.clone());
        cities.push(e.city.clone());
    }
    ColumnarEmployees { ids, names, ages, salaries, departments, cities }
}

fn generate_dept_rows(n: usize) -> Vec<(i64, String, f64, String)> {
    let mut rng = rand::thread_rng();
    (0..n)
        .map(|i| (i as i64, format!("dept_{}", i), 1_000_000.0 + rng.gen_range(0.0..9_000_000.0), format!("manager_{}", i)))
        .collect()
}

// ============================================================
// 计时工具
// ============================================================

fn bench<F, R>(name: &str, f: F) -> (f64, R)
where
    F: FnOnce() -> R,
{
    let start = Instant::now();
    let result = f();
    let elapsed = start.elapsed().as_secs_f64() * 1000.0;
    println!("  {:<42} {:>10.3} ms", name, elapsed);
    (elapsed, result)
}

// ============================================================
// 1. 全表扫描 + 聚合
// ============================================================

// 行存版
fn row_sum_salary(rows: &[EmployeeRow]) -> f64 {
    rows.iter().map(|e| e.salary).sum()
}

fn row_avg_age(rows: &[EmployeeRow]) -> f64 {
    let sum: i64 = rows.iter().map(|e| e.age as i64).sum();
    sum as f64 / rows.len() as f64
}

// 列存版
fn col_sum_salary(col: &ColumnarEmployees) -> f64 {
    col.salaries.iter().sum()
}

fn col_avg_age(col: &ColumnarEmployees) -> f64 {
    let sum: i64 = col.ages.iter().map(|&x| x as i64).sum();
    sum as f64 / col.ages.len() as f64
}

// ============================================================
// 2. 过滤查询
// ============================================================

fn row_filter_count_ge(rows: &[EmployeeRow], threshold: i64) -> usize {
    rows.iter().filter(|e| e.id >= threshold).count()
}

fn row_filter_count_lt(rows: &[EmployeeRow], threshold: i64) -> usize {
    rows.iter().filter(|e| e.id < threshold).count()
}

fn col_filter_count_ge(col: &ColumnarEmployees, threshold: i64) -> usize {
    col.ids.iter().filter(|&&x| x >= threshold).count()
}

fn col_filter_count_lt(col: &ColumnarEmployees, threshold: i64) -> usize {
    col.ids.iter().filter(|&&x| x < threshold).count()
}

// ============================================================
// 3. GROUP BY 哈希聚合
// ============================================================

fn row_group_by_sum(rows: &[EmployeeRow]) -> FxHashMap<String, f64> {
    let mut result = FxHashMap::default();
    result.reserve(rows.len() / 10);
    for e in rows {
        *result.entry(e.department.clone()).or_insert(0.0) += e.salary;
    }
    result
}

fn row_group_by_avg_city(rows: &[EmployeeRow]) -> FxHashMap<String, (f64, usize)> {
    let mut result = FxHashMap::default();
    result.reserve(rows.len() / 10);
    for e in rows {
        let entry = result.entry(e.city.clone()).or_insert((0.0, 0));
        entry.0 += e.salary;
        entry.1 += 1;
    }
    result
}

fn col_group_by_sum(group_col: &[String], val_col: &[f64]) -> FxHashMap<String, f64> {
    let mut result = FxHashMap::default();
    result.reserve(group_col.len() / 10);
    for i in 0..group_col.len() {
        *result.entry(group_col[i].clone()).or_insert(0.0) += val_col[i];
    }
    result
}

fn col_group_by_avg(group_col: &[String], val_col: &[f64]) -> FxHashMap<String, (f64, usize)> {
    let mut result = FxHashMap::default();
    result.reserve(group_col.len() / 10);
    for i in 0..group_col.len() {
        let entry = result.entry(group_col[i].clone()).or_insert((0.0, 0));
        entry.0 += val_col[i];
        entry.1 += 1;
    }
    result
}

// ============================================================
// 4. Hash Join
// ============================================================

// 行存版 join
fn row_hash_join(
    employees: &[EmployeeRow],
    depts: &[(i64, String, f64, String)],
) -> usize {
    let mut build_map: FxHashMap<&str, Vec<usize>> = FxHashMap::default();
    for (i, (_, name, _, _)) in depts.iter().enumerate() {
        build_map.entry(name.as_str()).or_default().push(i);
    }
    let mut count = 0;
    for e in employees {
        if let Some(matches) = build_map.get(e.department.as_str()) {
            count += matches.len();
        }
    }
    count
}

// 列存版 join
fn col_hash_join(
    left_depts: &[String],
    right_dept_names: &[String],
) -> usize {
    let mut build_map: FxHashMap<&str, Vec<usize>> = FxHashMap::default();
    for i in 0..right_dept_names.len() {
        build_map.entry(&right_dept_names[i]).or_default().push(i);
    }
    let mut count = 0;
    for i in 0..left_depts.len() {
        if let Some(matches) = build_map.get(left_depts[i].as_str()) {
            count += matches.len();
        }
    }
    count
}

// ============================================================
// 5. 排序 + Top-N
// ============================================================

fn row_sort_topn(rows: &[EmployeeRow], n: usize) -> Vec<(i64, f64)> {
    let mut pairs: Vec<(i64, f64)> = rows.iter().map(|e| (e.id, e.salary)).collect();
    if n < pairs.len() {
        let (top_part, _, _) = pairs.select_nth_unstable_by(n, |a, b| {
            b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal)
        });
        let mut top: Vec<(i64, f64)> = top_part.to_vec();
        top.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        top.truncate(n);
        top
    } else {
        pairs.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        pairs.truncate(n);
        pairs
    }
}

fn col_sort_topn(ids: &[i64], salaries: &[f64], n: usize) -> Vec<(i64, f64)> {
    let mut pairs: Vec<(i64, f64)> = ids.iter().zip(salaries.iter()).map(|(&id, &sal)| (id, sal)).collect();
    if n < pairs.len() {
        let (top_part, _, _) = pairs.select_nth_unstable_by(n, |a, b| {
            b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal)
        });
        let mut top: Vec<(i64, f64)> = top_part.to_vec();
        top.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        top.truncate(n);
        top
    } else {
        pairs.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        pairs.truncate(n);
        pairs
    }
}

// ============================================================
// 6. 点查
// ============================================================

fn row_point_lookup(rows: &[EmployeeRow], id: i64) -> Option<usize> {
    rows.iter().position(|e| e.id == id)
}

fn col_point_lookup(ids: &[i64], id: i64) -> Option<usize> {
    ids.iter().position(|&x| x == id)
}

// ============================================================
// 主函数
// ============================================================

fn main() {
    println!();
    println!("{}", "=".repeat(72));
    println!("  EngramDB 原生性能基准测试 (Rust 1.97 | {} 行)", NUM_ROWS);
    println!("{}", "=".repeat(72));
    println!("  列存 (EngramDB 架构): 列式存储 + 向量化访问 + 缓存友好");
    println!("  行存 (传统架构):     行式存储 + 逐字段访问 + 指针跳跃");
    println!("  全部原生 Rust 编译 (release + LTO)，无解释器开销");
    println!("{}", "=".repeat(72));

    // 生成数据
    println!("\n生成测试数据...");
    let rows = generate_rows(NUM_ROWS);
    let dept_rows = generate_dept_rows(JOIN_TABLE_SIZE);
    println!("  employees: {} 行", rows.len());
    println!("  departments: {} 行", dept_rows.len());

    // 数据加载（行转列开销）
    println!("\n{}", "─".repeat(72));
    println!("  0. 数据加载 / 格式转换");
    println!("{}", "─".repeat(72));

    let (t_row_load, _) = bench("行存: Vec<Row> 生成", || generate_rows(NUM_ROWS));
    let (t_col_load, col_emp) = bench("列存: 行转列构建", || rows_to_columnar(&rows));

    // 1. 全表扫描 + 聚合
    println!("\n{}", "─".repeat(72));
    println!("  1. 全表扫描 + 简单聚合");
    println!("{}", "─".repeat(72));

    let (t_row_sum, sum_row) = bench("行存 SUM(salary)", || row_sum_salary(&rows));
    let (t_col_sum, sum_col) = bench("列存 SUM(salary)", || col_sum_salary(&col_emp));
    println!("    校验: 行存={:.0}, 列存={:.0}", sum_row, sum_col);

    let (t_row_avg, avg_row) = bench("行存 AVG(age)", || row_avg_age(&rows));
    let (t_col_avg, avg_col) = bench("列存 AVG(age)", || col_avg_age(&col_emp));
    println!("    校验: 行存={:.1}, 列存={:.1}", avg_row, avg_col);

    let (t_row_count, count_row) = bench("行存 COUNT(*)", || rows.len());
    let (t_col_count, count_col) = bench("列存 COUNT(*)", || col_emp.len());

    // 2. 过滤查询
    println!("\n{}", "─".repeat(72));
    println!("  2. 过滤查询（不同选择性）");
    println!("{}", "─".repeat(72));

    let threshold_high = NUM_ROWS as i64 * 99 / 100;
    let threshold_mid = NUM_ROWS as i64 / 2;
    let threshold_low = NUM_ROWS as i64 / 10;

    let (t_row_high, n_row_high) = bench("行存 高选择性过滤(1%)", || row_filter_count_ge(&rows, threshold_high));
    let (t_col_high, n_col_high) = bench("列存 高选择性过滤(1%)", || col_filter_count_ge(&col_emp, threshold_high));
    println!("    匹配: 行存={}, 列存={}", n_row_high, n_col_high);

    let (t_row_mid, _) = bench("行存 中选择性过滤(50%)", || row_filter_count_lt(&rows, threshold_mid));
    let (t_col_mid, _) = bench("列存 中选择性过滤(50%)", || col_filter_count_lt(&col_emp, threshold_mid));

    let (t_row_low, _) = bench("行存 低选择性过滤(90%)", || row_filter_count_ge(&rows, threshold_low));
    let (t_col_low, _) = bench("列存 低选择性过滤(90%)", || col_filter_count_ge(&col_emp, threshold_low));

    // 3. GROUP BY
    println!("\n{}", "─".repeat(72));
    println!("  3. GROUP BY 哈希聚合");
    println!("{}", "─".repeat(72));

    let (t_row_gb_low, gb_row_low) = bench("行存 GROUP BY dept (10组) SUM", || row_group_by_sum(&rows));
    let (t_col_gb_low, gb_col_low) = bench("列存 GROUP BY dept (10组) SUM", || col_group_by_sum(&col_emp.departments, &col_emp.salaries));
    println!("    组数: 行存={}, 列存={}", gb_row_low.len(), gb_col_low.len());

    let (t_row_gb_high, gb_row_high) = bench("行存 GROUP BY city (50组) AVG", || row_group_by_avg_city(&rows));
    let (t_col_gb_high, gb_col_high) = bench("列存 GROUP BY city (50组) AVG", || col_group_by_avg(&col_emp.cities, &col_emp.salaries));
    println!("    组数: 行存={}, 列存={}", gb_row_high.len(), gb_col_high.len());

    // 4. Hash Join
    println!("\n{}", "─".repeat(72));
    println!("  4. Hash Join（100k × 10k）");
    println!("{}", "─".repeat(72));

    let dept_names: Vec<String> = dept_rows.iter().map(|d| d.1.clone()).collect();

    let (t_row_join, join_row) = bench("行存 Hash Join", || row_hash_join(&rows, &dept_rows));
    let (t_col_join, join_col) = bench("列存 Hash Join", || col_hash_join(&col_emp.departments, &dept_names));
    println!("    输出行数: 行存={}, 列存={}", join_row, join_col);

    // 5. 排序 + Top-N
    println!("\n{}", "─".repeat(72));
    println!("  5. 排序 + Top-N");
    println!("{}", "─".repeat(72));

    let top_n = 100;
    let (t_row_sort, sort_row) = bench(&format!("行存 ORDER BY salary LIMIT {}", top_n), || row_sort_topn(&rows, top_n));
    let (t_col_sort, sort_col) = bench(&format!("列存 ORDER BY salary LIMIT {}", top_n), || col_sort_topn(&col_emp.ids, &col_emp.salaries, top_n));
    println!("    Top-1: 行存 id={} sal={:.0}, 列存 id={} sal={:.0}", sort_row[0].0, sort_row[0].1, sort_col[0].0, sort_col[0].1);

    // 6. 点查
    println!("\n{}", "─".repeat(72));
    println!("  6. 点查（线性扫描，无索引）");
    println!("{}", "─".repeat(72));

    let lookup_id = NUM_ROWS as i64 / 2;
    let (t_row_point, _) = bench("行存 线性扫描点查", || row_point_lookup(&rows, lookup_id));
    let (t_col_point, _) = bench("列存 线性扫描点查", || col_point_lookup(&col_emp.ids, lookup_id));

    // ============================================================
    // 汇总
    // ============================================================
    println!("\n\n{}", "=".repeat(72));
    println!("  性能对比汇总（毫秒，越小越好）");
    println!("{}", "=".repeat(72));
    println!("  {:<32} {:>10} {:>10} {:>10}", "测试项", "行存", "列存", "列存/行存");
    println!("  {}", "─".repeat(66));

    let data: [(&str, f64, f64); 13] = [
        ("数据生成/加载", t_row_load, t_col_load),
        ("SUM(salary) 全表聚合", t_row_sum, t_col_sum),
        ("AVG(age) 全表聚合", t_row_avg, t_col_avg),
        ("COUNT(*) 全表扫描", t_row_count as f64, t_col_count as f64),
        ("过滤-高选择性(1%)", t_row_high as f64, t_col_high as f64),
        ("过滤-中选择性(50%)", t_row_mid as f64, t_col_mid as f64),
        ("过滤-低选择性(90%)", t_row_low as f64, t_col_low as f64),
        ("GROUP BY 低基数(10组)", t_row_gb_low, t_col_gb_low),
        ("GROUP BY 高基数(50组)", t_row_gb_high, t_col_gb_high),
        ("Hash Join (100k×10k)", t_row_join as f64, t_col_join as f64),
        (&format!("ORDER BY + LIMIT {}", top_n), t_row_sort, t_col_sort),
        ("点查（线性扫描）", t_row_point as f64, t_col_point as f64),
        ("单字段扫描均值", (t_row_sum + t_row_avg) / 2.0, (t_col_sum + t_col_avg) / 2.0),
    ];

    let mut ratios = Vec::new();
    for (name, r, c) in &data {
        let ratio = if *r > 0.0 { c / r } else { 0.0 };
        ratios.push(ratio);
        let marker = if ratio < 1.0 { " ✓" } else { "" };
        println!("  {:<32} {:>9.3}  {:>9.3}  {:>7.2}x{}", name, r, c, ratio, marker);
    }

    // 几何平均（排除数据加载项，聚焦查询性能）
    let query_ratios: Vec<f64> = ratios.iter().skip(1).take(11).cloned().collect();
    let geo_mean: f64 = (query_ratios.iter().map(|x| x.ln()).sum::<f64>() / query_ratios.len() as f64).exp();

    println!();
    println!("  查询项几何平均(11项): {:>28}  {:>7.2}x", "", geo_mean);
    if geo_mean < 1.0 {
        let speedup = 1.0 / geo_mean;
        println!("  列存综合提速: {:.2}x", speedup);
    }

    println!();
    println!("  说明：");
    println!("  - 全部为 Rust release + LTO 原生编译，无解释器开销");
    println!("  - 行存 = Vec<struct> 逐字段访问（模拟 SQLite/MySQL 行存）");
    println!("  - 列存 = 独立 Vec 列式存储（EngramDB 架构）");
    println!("  - <1.0x 表示列存更快，✓ 标记列存胜出项");
    println!("  - 排序场景两者差异小（都需要构造 pair 数组）");
    println!();
}
