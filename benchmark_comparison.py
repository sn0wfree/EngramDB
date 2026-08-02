#!/usr/bin/env python3
"""
HybridDB vs SQLite vs DuckDB 性能对比基准测试

测试说明：
- HybridDB 用 Python 模拟实现其核心架构（列存 + 向量化 + Hash Join）
  注：HybridDB 原生为 Rust 实现，Python 版仅用于验证架构设计的性能趋势
- SQLite：Python 内置 sqlite3 模块（行存、逐行执行）
- DuckDB：duckdb Python 包（列存、向量化执行，C++ 原生）

测试场景：
1. 数据加载（批量写入）
2. 全表扫描 + 简单聚合
3. 过滤查询（高/中/低选择性）
4. GROUP BY 聚合（低/高基数）
5. Hash Join（两表连接）
6. 排序（ORDER BY）
7. 点查（主键查询）

数据规模：10 万行（默认），可通过命令行参数调整
"""

import time
import random
import sqlite3
import duckdb
import sys
import os
from dataclasses import dataclass, field
from typing import List, Dict, Any, Tuple
from collections import defaultdict

# ============================================================
# 配置
# ============================================================

NUM_ROWS = 100_000  # 默认数据量
NUM_GROUPS_LOW = 10    # 低基数 GROUP BY
NUM_GROUPS_HIGH = 1000 # 高基数 GROUP BY
JOIN_TABLE_SIZE = 10_000  # 连接表大小
SEED = 42

def set_num_rows(n: int):
    global NUM_ROWS
    NUM_ROWS = n

# ============================================================
# 数据生成
# ============================================================

def generate_data(num_rows: int) -> List[Tuple]:
    """生成测试数据：(id, name, age, salary, department, city)"""
    random.seed(SEED)
    departments = [f"dept_{i}" for i in range(NUM_GROUPS_LOW)]
    cities = [f"city_{i}" for i in range(50)]
    
    rows = []
    for i in range(num_rows):
        age = 20 + (i % 45)  # 20-64
        salary = 30000 + (i * 7) % 170000  # 30k-200k
        dept = departments[i % NUM_GROUPS_LOW]
        city = cities[i % 50]
        name = f"user_{i}"
        rows.append((i, name, age, salary, dept, city))
    return rows

def generate_join_data(num_rows: int) -> List[Tuple]:
    """生成连接表数据：(dept_id, dept_name, budget, manager)"""
    random.seed(SEED + 1)
    rows = []
    for i in range(num_rows):
        dept_name = f"dept_{i}"
        budget = 1000000 + random.randint(0, 9000000)
        manager = f"manager_{i}"
        rows.append((i, dept_name, budget, manager))
    return rows

# ============================================================
# HybridDB Python 模拟版（列存 + 向量化）
# ============================================================

class HybridDBMock:
    """
    HybridDB 核心架构的 Python 模拟实现。
    
    设计要点：
    - 列式存储（每列独立存储）
    - 向量化执行（批量处理，减少解释开销）
    - Hash Join（build + probe 两阶段）
    - 选择向量（SelectionVector）零拷贝过滤
    """
    
    def __init__(self):
        self.tables: Dict[str, Dict[str, List]] = {}
        self.table_info: Dict[str, Dict] = {}
    
    def create_table(self, name: str, columns: List[Tuple[str, type]]):
        self.tables[name] = {col[0]: [] for col in columns}
        self.table_info[name] = {
            'columns': columns,
            'row_count': 0,
        }
    
    def insert_batch(self, table_name: str, rows: List[Tuple]):
        """批量插入：按列组织数据"""
        cols = self.table_info[table_name]['columns']
        table = self.tables[table_name]
        
        # 转置：行存 → 列存
        num_cols = len(cols)
        col_data = [[] for _ in range(num_cols)]
        for row in rows:
            for i in range(num_cols):
                col_data[i].append(row[i])
        
        for i, (col_name, _) in enumerate(cols):
            table[col_name].extend(col_data[i])
        
        self.table_info[table_name]['row_count'] += len(rows)
    
    def table_scan_aggregate(self, table_name: str, col_name: str, agg_func: str) -> Any:
        """全表扫描 + 聚合（向量化）"""
        col = self.tables[table_name][col_name]
        n = len(col)
        
        if agg_func == 'count':
            return n
        elif agg_func == 'sum':
            # 向量化求和：Python 内置 sum 是 C 实现，接近向量化效果
            return sum(col)
        elif agg_func == 'avg':
            return sum(col) / n if n > 0 else 0
        elif agg_func == 'min':
            return min(col)
        elif agg_func == 'max':
            return max(col)
        return None
    
    def filter_count(self, table_name: str, col_name: str, op: str, value: Any) -> int:
        """过滤查询：返回匹配行数（向量化过滤）"""
        col = self.tables[table_name][col_name]
        
        # 向量化过滤：列表推导式（Python 中比逐行 if 快很多）
        if op == '>':
            return sum(1 for v in col if v > value)
        elif op == '>=':
            return sum(1 for v in col if v >= value)
        elif op == '<':
            return sum(1 for v in col if v < value)
        elif op == '==':
            return sum(1 for v in col if v == value)
        elif op == 'between':
            lo, hi = value
            return sum(1 for v in col if lo <= v <= hi)
        return 0
    
    def group_by_aggregate(self, table_name: str, group_col: str, agg_col: str, agg_func: str) -> Dict:
        """GROUP BY 聚合（向量化哈希聚合）"""
        groups = self.tables[table_name][group_col]
        values = self.tables[table_name][agg_col]
        n = len(groups)
        
        result = defaultdict(list)
        for i in range(n):
            result[groups[i]].append(values[i])
        
        if agg_func == 'sum':
            return {k: sum(v) for k, v in result.items()}
        elif agg_func == 'avg':
            return {k: sum(v)/len(v) for k, v in result.items()}
        elif agg_func == 'count':
            return {k: len(v) for k, v in result.items()}
        return dict(result)
    
    def hash_join(self, left_table: str, right_table: str,
                  left_key: str, right_key: str,
                  left_cols: List[str], right_cols: List[str]) -> List[Tuple]:
        """
        Hash Join：build + probe 两阶段
        选择较小的表作为 build side
        """
        left_data = self.tables[left_table]
        right_data = self.tables[right_table]
        
        left_keys = left_data[left_key]
        right_keys = right_data[right_key]
        
        # Build phase：用较小的表构建哈希表
        if len(right_keys) <= len(left_keys):
            build_keys = right_keys
            build_data = right_data
            build_cols = right_cols
            probe_keys = left_keys
            probe_data = left_data
            probe_cols = left_cols
            build_is_right = True
        else:
            build_keys = left_keys
            build_data = left_data
            build_cols = left_cols
            probe_keys = right_keys
            probe_data = right_data
            probe_cols = right_cols
            build_is_right = False
        
        # 构建哈希表：key -> 行索引列表
        hash_table = defaultdict(list)
        for i, k in enumerate(build_keys):
            hash_table[k].append(i)
        
        # Probe phase
        results = []
        build_col_values = [build_data[c] for c in build_cols]
        probe_col_values = [probe_data[c] for c in probe_cols]
        
        for i, probe_key in enumerate(probe_keys):
            if probe_key in hash_table:
                probe_row = tuple(probe_col_values[j][i] for j in range(len(probe_cols)))
                for build_idx in hash_table[probe_key]:
                    build_row = tuple(build_col_values[j][build_idx] for j in range(len(build_cols)))
                    if build_is_right:
                        results.append(probe_row + build_row)
                    else:
                        results.append(build_row + probe_row)
        
        return results
    
    def sort_by(self, table_name: str, sort_col: str, select_cols: List[str], limit: int = None) -> List[Tuple]:
        """排序（ORDER BY）"""
        data = self.tables[table_name]
        sort_values = data[sort_col]
        n = len(sort_values)
        
        # 创建索引数组并排序（间接排序，避免移动整行数据）
        indices = list(range(n))
        indices.sort(key=lambda i: sort_values[i])
        
        if limit:
            indices = indices[:limit]
        
        # 物化结果
        col_data = [data[c] for c in select_cols]
        results = []
        for idx in indices:
            row = tuple(col_data[j][idx] for j in range(len(select_cols)))
            results.append(row)
        
        return results
    
    def point_lookup(self, table_name: str, key_col: str, key_value: Any,
                     select_cols: List[str]) -> List[Tuple]:
        """点查：等值查找（线性扫描模拟，实际 HybridDB 用索引）"""
        data = self.tables[table_name]
        keys = data[key_col]
        col_data = [data[c] for c in select_cols]
        
        results = []
        for i, k in enumerate(keys):
            if k == key_value:
                row = tuple(col_data[j][i] for j in range(len(select_cols)))
                results.append(row)
        return results

# ============================================================
# 基准测试框架
# ============================================================

@dataclass
class BenchmarkResult:
    name: str
    hybriddb_ms: float = 0
    sqlite_ms: float = 0
    duckdb_ms: float = 0
    note: str = ""
    
    def to_row(self) -> List[str]:
        return [
            self.name,
            f"{self.hybriddb_ms:.2f}",
            f"{self.sqlite_ms:.2f}",
            f"{self.duckdb_ms:.2f}",
            self.note,
        ]

def benchmark(func, *args, name: str = "", **kwargs) -> float:
    """执行基准测试，返回 (毫秒数, 结果)"""
    start = time.perf_counter()
    result = func(*args, **kwargs)
    elapsed = (time.perf_counter() - start) * 1000
    return elapsed, result

def warm_up(func, *args, **kwargs):
    """预热：执行一次让缓存生效"""
    try:
        func(*args, **kwargs)
    except:
        pass

# ============================================================
# SQLite 测试
# ============================================================

class SQLiteBench:
    def __init__(self, db_path: str = ":memory:"):
        self.conn = sqlite3.connect(db_path)
        self.conn.execute("PRAGMA journal_mode = WAL")
        self.conn.execute("PRAGMA synchronous = NORMAL")
    
    def create_table(self):
        self.conn.execute("""
            CREATE TABLE employees (
                id INTEGER PRIMARY KEY,
                name TEXT,
                age INTEGER,
                salary INTEGER,
                department TEXT,
                city TEXT
            )
        """)
        self.conn.execute("""
            CREATE TABLE departments (
                dept_id INTEGER PRIMARY KEY,
                dept_name TEXT,
                budget INTEGER,
                manager TEXT
            )
        """)
        self.conn.commit()
    
    def insert_batch(self, table: str, rows: List[Tuple]):
        if table == 'employees':
            self.conn.executemany(
                "INSERT INTO employees VALUES (?, ?, ?, ?, ?, ?)",
                rows
            )
        elif table == 'departments':
            self.conn.executemany(
                "INSERT INTO departments VALUES (?, ?, ?, ?)",
                rows
            )
        self.conn.commit()
    
    def query_one(self, sql: str) -> Any:
        cur = self.conn.execute(sql)
        return cur.fetchone()[0]
    
    def query_all(self, sql: str) -> List[Tuple]:
        cur = self.conn.execute(sql)
        return cur.fetchall()
    
    def close(self):
        self.conn.close()

# ============================================================
# DuckDB 测试
# ============================================================

class DuckDBBench:
    def __init__(self):
        self.conn = duckdb.connect()
    
    def create_table(self):
        self.conn.execute("""
            CREATE TABLE employees (
                id INTEGER,
                name VARCHAR,
                age INTEGER,
                salary DOUBLE,
                department VARCHAR,
                city VARCHAR
            )
        """)
        self.conn.execute("""
            CREATE TABLE departments (
                dept_id INTEGER,
                dept_name VARCHAR,
                budget DOUBLE,
                manager VARCHAR
            )
        """)
    
    def insert_batch(self, table: str, rows: List[Tuple]):
        # DuckDB 高效批量导入：通过 pandas DataFrame（列式内存布局）
        import pandas as pd
        if not rows:
            return
        if table == 'employees':
            df = pd.DataFrame(rows, columns=['id', 'name', 'age', 'salary', 'department', 'city'])
            self.conn.execute("INSERT INTO employees SELECT * FROM df")
        elif table == 'departments':
            df = pd.DataFrame(rows, columns=['dept_id', 'dept_name', 'budget', 'manager'])
            self.conn.execute("INSERT INTO departments SELECT * FROM df")
    
    def query_one(self, sql: str) -> Any:
        return self.conn.execute(sql).fetchone()[0]
    
    def query_all(self, sql: str) -> List[Tuple]:
        return self.conn.execute(sql).fetchall()
    
    def close(self):
        self.conn.close()

# ============================================================
# 完整基准测试
# ============================================================

def run_benchmarks(num_rows: int = NUM_ROWS) -> List[BenchmarkResult]:
    print(f"\n{'='*70}")
    print(f"  性能对比基准测试  |  数据量: {num_rows:,} 行")
    print(f"{'='*70}")
    print(f"  HybridDB: Python 模拟版（列存 + 向量化 + Hash Join）")
    print(f"  SQLite:    Python sqlite3（行存 + 逐行执行）")
    print(f"  DuckDB:    duckdb Python（列存 + 向量化，C++ 原生）")
    print(f"{'='*70}\n")
    
    results = []
    
    # ---- 生成数据 ----
    print("生成测试数据...")
    employees_data = generate_data(num_rows)
    dept_data = generate_join_data(JOIN_TABLE_SIZE)
    print(f"  employees: {len(employees_data):,} 行")
    print(f"  departments: {len(dept_data):,} 行")
    
    # ============================================================
    # 1. 数据加载
    # ============================================================
    print(f"\n{'─'*70}")
    print("  1. 数据加载（批量写入）")
    print(f"{'─'*70}")
    
    # HybridDB
    hdb = HybridDBMock()
    hdb.create_table('employees', [
        ('id', int), ('name', str), ('age', int),
        ('salary', int), ('department', str), ('city', str)
    ])
    hdb.create_table('departments', [
        ('dept_id', int), ('dept_name', str),
        ('budget', int), ('manager', str)
    ])
    
    t_hdb_load, _ = benchmark(lambda: (
        hdb.insert_batch('employees', employees_data),
        hdb.insert_batch('departments', dept_data),
    ), name="hdb_insert")
    print(f"  HybridDB: {t_hdb_load:.2f} ms")
    
    # SQLite
    sq = SQLiteBench()
    sq.create_table()
    t_sq_load, _ = benchmark(lambda: (
        sq.insert_batch('employees', employees_data),
        sq.insert_batch('departments', dept_data),
    ), name="sq_insert")
    print(f"  SQLite:   {t_sq_load:.2f} ms")
    
    # DuckDB
    ddb = DuckDBBench()
    ddb.create_table()
    t_ddb_load, _ = benchmark(lambda: (
        ddb.insert_batch('employees', employees_data),
        ddb.insert_batch('departments', dept_data),
    ), name="ddb_insert")
    print(f"  DuckDB:   {t_ddb_load:.2f} ms")
    
    results.append(BenchmarkResult(
        name="数据加载（批量写入）",
        hybriddb_ms=t_hdb_load,
        sqlite_ms=t_sq_load,
        duckdb_ms=t_ddb_load,
        note=f"{num_rows:,} 行 + {JOIN_TABLE_SIZE:,} 行",
    ))
    
    # ============================================================
    # 2. 全表扫描 + 简单聚合
    # ============================================================
    print(f"\n{'─'*70}")
    print("  2. 全表扫描 + 简单聚合")
    print(f"{'─'*70}")
    
    # COUNT
    warm_up(lambda: hdb.table_scan_aggregate('employees', 'id', 'count'))
    t_hdb, r_hdb = benchmark(lambda: hdb.table_scan_aggregate('employees', 'id', 'count'))
    t_sq, r_sq = benchmark(lambda: sq.query_one("SELECT COUNT(*) FROM employees"))
    t_ddb, r_ddb = benchmark(lambda: ddb.query_one("SELECT COUNT(*) FROM employees"))
    print(f"  COUNT(*):")
    print(f"    HybridDB: {t_hdb:.2f} ms  (result={r_hdb})")
    print(f"    SQLite:   {t_sq:.2f} ms  (result={r_sq})")
    print(f"    DuckDB:   {t_ddb:.2f} ms  (result={r_ddb})")
    results.append(BenchmarkResult("COUNT(*) 全表扫描", t_hdb, t_sq, t_ddb, f"结果={r_hdb}"))
    
    # SUM
    warm_up(lambda: hdb.table_scan_aggregate('employees', 'salary', 'sum'))
    t_hdb, r_hdb = benchmark(lambda: hdb.table_scan_aggregate('employees', 'salary', 'sum'))
    t_sq, r_sq = benchmark(lambda: sq.query_one("SELECT SUM(salary) FROM employees"))
    t_ddb, r_ddb = benchmark(lambda: ddb.query_one("SELECT SUM(salary) FROM employees"))
    print(f"  SUM(salary):")
    print(f"    HybridDB: {t_hdb:.2f} ms")
    print(f"    SQLite:   {t_sq:.2f} ms")
    print(f"    DuckDB:   {t_ddb:.2f} ms")
    results.append(BenchmarkResult("SUM(salary) 全表聚合", t_hdb, t_sq, t_ddb, ""))
    
    # AVG
    t_hdb, r_hdb = benchmark(lambda: hdb.table_scan_aggregate('employees', 'age', 'avg'))
    t_sq, r_sq = benchmark(lambda: sq.query_one("SELECT AVG(age) FROM employees"))
    t_ddb, r_ddb = benchmark(lambda: ddb.query_one("SELECT AVG(age) FROM employees"))
    print(f"  AVG(age):")
    print(f"    HybridDB: {t_hdb:.2f} ms  (avg={r_hdb:.1f})")
    print(f"    SQLite:   {t_sq:.2f} ms  (avg={r_sq:.1f})")
    print(f"    DuckDB:   {t_ddb:.2f} ms  (avg={r_ddb:.1f})")
    results.append(BenchmarkResult("AVG(age) 全表聚合", t_hdb, t_sq, t_ddb, ""))
    
    # ============================================================
    # 3. 过滤查询
    # ============================================================
    print(f"\n{'─'*70}")
    print("  3. 过滤查询（不同选择性）")
    print(f"{'─'*70}")
    
    # 高选择性（~1% 行匹配）
    threshold_high = int(num_rows * 0.99)
    t_hdb, r_hdb = benchmark(lambda: hdb.filter_count('employees', 'id', '>=', threshold_high))
    t_sq, r_sq = benchmark(lambda: sq.query_one(f"SELECT COUNT(*) FROM employees WHERE id >= {threshold_high}"))
    t_ddb, r_ddb = benchmark(lambda: ddb.query_one(f"SELECT COUNT(*) FROM employees WHERE id >= {threshold_high}"))
    selectivity_high = r_hdb / num_rows * 100
    print(f"  高选择性（~{selectivity_high:.1f}% 匹配）:")
    print(f"    HybridDB: {t_hdb:.2f} ms  (rows={r_hdb})")
    print(f"    SQLite:   {t_sq:.2f} ms  (rows={r_sq})")
    print(f"    DuckDB:   {t_ddb:.2f} ms  (rows={r_ddb})")
    results.append(BenchmarkResult(f"过滤-高选择性({selectivity_high:.0f}%)", t_hdb, t_sq, t_ddb, "id >= 99%"))
    
    # 中选择性（~50% 行匹配）
    threshold_mid = num_rows // 2
    t_hdb, r_hdb = benchmark(lambda: hdb.filter_count('employees', 'id', '<', threshold_mid))
    t_sq, r_sq = benchmark(lambda: sq.query_one(f"SELECT COUNT(*) FROM employees WHERE id < {threshold_mid}"))
    t_ddb, r_ddb = benchmark(lambda: ddb.query_one(f"SELECT COUNT(*) FROM employees WHERE id < {threshold_mid}"))
    selectivity_mid = r_hdb / num_rows * 100
    print(f"  中选择性（~{selectivity_mid:.1f}% 匹配）:")
    print(f"    HybridDB: {t_hdb:.2f} ms  (rows={r_hdb})")
    print(f"    SQLite:   {t_sq:.2f} ms  (rows={r_sq})")
    print(f"    DuckDB:   {t_ddb:.2f} ms  (rows={r_ddb})")
    results.append(BenchmarkResult(f"过滤-中选择性({selectivity_mid:.0f}%)", t_hdb, t_sq, t_ddb, "id < 50%"))
    
    # 低选择性（~90% 行匹配）
    threshold_low = int(num_rows * 0.1)
    t_hdb, r_hdb = benchmark(lambda: hdb.filter_count('employees', 'id', '>=', threshold_low))
    t_sq, r_sq = benchmark(lambda: sq.query_one(f"SELECT COUNT(*) FROM employees WHERE id >= {threshold_low}"))
    t_ddb, r_ddb = benchmark(lambda: ddb.query_one(f"SELECT COUNT(*) FROM employees WHERE id >= {threshold_low}"))
    selectivity_low = r_hdb / num_rows * 100
    print(f"  低选择性（~{selectivity_low:.1f}% 匹配）:")
    print(f"    HybridDB: {t_hdb:.2f} ms  (rows={r_hdb})")
    print(f"    SQLite:   {t_sq:.2f} ms  (rows={r_sq})")
    print(f"    DuckDB:   {t_ddb:.2f} ms  (rows={r_ddb})")
    results.append(BenchmarkResult(f"过滤-低选择性({selectivity_low:.0f}%)", t_hdb, t_sq, t_ddb, "id >= 10%"))
    
    # ============================================================
    # 4. GROUP BY 聚合
    # ============================================================
    print(f"\n{'─'*70}")
    print("  4. GROUP BY 聚合")
    print(f"{'─'*70}")
    
    # 低基数（10 组）
    t_hdb, r_hdb = benchmark(lambda: hdb.group_by_aggregate('employees', 'department', 'salary', 'sum'))
    t_sq, r_sq = benchmark(lambda: sq.query_all("SELECT department, SUM(salary) FROM employees GROUP BY department"))
    t_ddb, r_ddb = benchmark(lambda: ddb.query_all("SELECT department, SUM(salary) FROM employees GROUP BY department"))
    print(f"  低基数 GROUP BY（{NUM_GROUPS_LOW} 组）:")
    print(f"    HybridDB: {t_hdb:.2f} ms  (groups={len(r_hdb)})")
    print(f"    SQLite:   {t_sq:.2f} ms  (groups={len(r_sq)})")
    print(f"    DuckDB:   {t_ddb:.2f} ms  (groups={len(r_ddb)})")
    results.append(BenchmarkResult(f"GROUP BY 低基数({NUM_GROUPS_LOW}组)", t_hdb, t_sq, t_ddb, "SUM(salary)"))
    
    # 高基数（50 组 - city）
    t_hdb, r_hdb = benchmark(lambda: hdb.group_by_aggregate('employees', 'city', 'salary', 'avg'))
    t_sq, r_sq = benchmark(lambda: sq.query_all("SELECT city, AVG(salary) FROM employees GROUP BY city"))
    t_ddb, r_ddb = benchmark(lambda: ddb.query_all("SELECT city, AVG(salary) FROM employees GROUP BY city"))
    print(f"  高基数 GROUP BY（50 组）:")
    print(f"    HybridDB: {t_hdb:.2f} ms  (groups={len(r_hdb)})")
    print(f"    SQLite:   {t_sq:.2f} ms  (groups={len(r_sq)})")
    print(f"    DuckDB:   {t_ddb:.2f} ms  (groups={len(r_ddb)})")
    results.append(BenchmarkResult("GROUP BY 高基数(50组)", t_hdb, t_sq, t_ddb, "AVG(salary)"))
    
    # ============================================================
    # 5. Hash Join
    # ============================================================
    print(f"\n{'─'*70}")
    print("  5. Hash Join（两表连接）")
    print(f"{'─'*70}")
    
    t_hdb, r_hdb = benchmark(lambda: hdb.hash_join(
        'employees', 'departments',
        'department', 'dept_name',
        ['id', 'name', 'salary', 'department'],
        ['dept_name', 'budget', 'manager']
    ))
    t_sq, r_sq = benchmark(lambda: sq.query_all("""
        SELECT e.id, e.name, e.salary, e.department, d.dept_name, d.budget, d.manager
        FROM employees e
        JOIN departments d ON e.department = d.dept_name
    """))
    t_ddb, r_ddb = benchmark(lambda: ddb.query_all("""
        SELECT e.id, e.name, e.salary, e.department, d.dept_name, d.budget, d.manager
        FROM employees e
        JOIN departments d ON e.department = d.dept_name
    """))
    print(f"  INNER JOIN（{num_rows:,} × {JOIN_TABLE_SIZE:,}）:")
    print(f"    HybridDB: {t_hdb:.2f} ms  (rows={len(r_hdb)})")
    print(f"    SQLite:   {t_sq:.2f} ms  (rows={len(r_sq)})")
    print(f"    DuckDB:   {t_ddb:.2f} ms  (rows={len(r_ddb)})")
    results.append(BenchmarkResult(
        f"Hash Join（{num_rows//1000}k×{JOIN_TABLE_SIZE//1000}k）",
        t_hdb, t_sq, t_ddb, "INNER JOIN"
    ))
    
    # ============================================================
    # 6. 排序
    # ============================================================
    print(f"\n{'─'*70}")
    print("  6. 排序（ORDER BY）")
    print(f"{'─'*70}")
    
    # 全量排序
    sort_limit = 100
    t_hdb, r_hdb = benchmark(lambda: hdb.sort_by(
        'employees', 'salary',
        ['id', 'name', 'salary', 'department'],
        limit=sort_limit
    ))
    t_sq, r_sq = benchmark(lambda: sq.query_all(
        f"SELECT id, name, salary, department FROM employees ORDER BY salary LIMIT {sort_limit}"
    ))
    t_ddb, r_ddb = benchmark(lambda: ddb.query_all(
        f"SELECT id, name, salary, department FROM employees ORDER BY salary LIMIT {sort_limit}"
    ))
    print(f"  ORDER BY + LIMIT {sort_limit}:")
    print(f"    HybridDB: {t_hdb:.2f} ms")
    print(f"    SQLite:   {t_sq:.2f} ms")
    print(f"    DuckDB:   {t_ddb:.2f} ms")
    results.append(BenchmarkResult(f"ORDER BY + LIMIT {sort_limit}", t_hdb, t_sq, t_ddb, "全量排序取前N"))
    
    # ============================================================
    # 7. 点查
    # ============================================================
    print(f"\n{'─'*70}")
    print("  7. 点查（主键查询）")
    print(f"{'─'*70}")
    
    lookup_id = num_rows // 2
    t_hdb, r_hdb = benchmark(lambda: hdb.point_lookup(
        'employees', 'id', lookup_id,
        ['id', 'name', 'salary', 'department']
    ))
    t_sq, r_sq = benchmark(lambda: sq.query_all(
        f"SELECT id, name, salary, department FROM employees WHERE id = {lookup_id}"
    ))
    t_ddb, r_ddb = benchmark(lambda: ddb.query_all(
        f"SELECT id, name, salary, department FROM employees WHERE id = {lookup_id}"
    ))
    print(f"  主键等值查询（id={lookup_id}）:")
    print(f"    HybridDB: {t_hdb:.2f} ms  (rows={len(r_hdb)})")
    print(f"    SQLite:   {t_sq:.2f} ms  (rows={len(r_sq)})")
    print(f"    DuckDB:   {t_ddb:.2f} ms  (rows={len(r_ddb)})")
    results.append(BenchmarkResult("点查（主键等值）", t_hdb, t_sq, t_ddb, "注: HybridDB未用索引"))
    
    # 清理
    sq.close()
    ddb.close()
    
    return results

# ============================================================
# 结果汇总
# ============================================================

def print_summary(results: List[BenchmarkResult]):
    print(f"\n\n{'='*70}")
    print(f"  性能对比汇总表（单位：毫秒，越小越好）")
    print(f"{'='*70}")
    
    header = f"{'测试项':<30} {'HybridDB':>10} {'SQLite':>10} {'DuckDB':>10} {'备注':<20}"
    print(header)
    print("─" * 70)
    
    for r in results:
        print(f"{r.name:<30} {r.hybriddb_ms:>9.2f}  {r.sqlite_ms:>9.2f}  {r.duckdb_ms:>9.2f}  {r.note:<20}")
    
    # 计算相对性能
    print(f"\n{'='*70}")
    print(f"  相对性能（以 SQLite 为基准 = 1.0，越大越慢）")
    print(f"{'='*70}")
    print(f"{'测试项':<30} {'HybridDB':>10} {'SQLite':>10} {'DuckDB':>10}")
    print("─" * 70)
    
    for r in results:
        if r.sqlite_ms > 0:
            h_ratio = r.hybriddb_ms / r.sqlite_ms
            d_ratio = r.duckdb_ms / r.sqlite_ms
            print(f"{r.name:<30} {h_ratio:>9.2f}x {1.0:>9.2f}x {d_ratio:>9.2f}x")
    
    # 几何平均
    print(f"\n{'='*70}")
    print(f"  几何平均相对性能（SQLite = 1.0）")
    print(f"{'='*70}")
    
    import math
    h_ratios = []
    d_ratios = []
    for r in results:
        if r.sqlite_ms > 0:
            h_ratios.append(r.hybriddb_ms / r.sqlite_ms)
            d_ratios.append(r.duckdb_ms / r.sqlite_ms)
    
    h_geo = math.exp(sum(math.log(x) for x in h_ratios) / len(h_ratios))
    d_geo = math.exp(sum(math.log(x) for x in d_ratios) / len(d_ratios))
    
    print(f"  HybridDB 几何平均: {h_geo:.2f}x （相对于 SQLite）")
    print(f"  DuckDB 几何平均:   {d_geo:.2f}x （相对于 SQLite）")
    print()
    print(f"  注：HybridDB 为 Python 模拟版，Rust 原生版预计快 10-50x")
    print(f"      DuckDB 为 C++ 原生，是业界 OLAP 性能标杆")
    print(f"      SQLite 为行存事务型数据库，OLAP 非其强项")

# ============================================================
# 主函数
# ============================================================

if __name__ == "__main__":
    num_rows = NUM_ROWS
    if len(sys.argv) > 1:
        try:
            num_rows = int(sys.argv[1])
        except ValueError:
            pass
    
    set_num_rows(num_rows)
    results = run_benchmarks(num_rows)
    print_summary(results)
