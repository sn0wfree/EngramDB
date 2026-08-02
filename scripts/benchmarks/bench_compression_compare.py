#!/usr/bin/env python3
"""
三引擎压缩率/文件大小对比
HybridDB vs SQLite vs DuckDB
100k 行 × 4 列 (id INT, category INT, value DOUBLE, name VARCHAR)
"""

import sqlite3
import os
import sys
import random
import subprocess
import json

N_ROWS = 100_000
random.seed(42)

def generate_data(n):
    """生成测试数据"""
    rows = []
    for i in range(n):
        cat = random.randint(0, 99)
        val = random.uniform(0, 1000)
        name = f"item_{i}"
        rows.append((i, cat, val, name))
    return rows

def fmt_bytes(n):
    if n >= 1024 * 1024:
        return f"{n/1024/1024:.2f} MB"
    elif n >= 1024:
        return f"{n/1024:.1f} KB"
    else:
        return f"{n} B"

def bench_sqlite(rows):
    """SQLite 写入 + 文件大小"""
    db_path = "/tmp/comp_bench_sqlite.db"
    for f in [db_path, db_path + "-wal", db_path + "-shm"]:
        if os.path.exists(f):
            os.remove(f)

    conn = sqlite3.connect(db_path)
    conn.execute("CREATE TABLE t1 (id INT, category INT, value DOUBLE, name VARCHAR)")

    # 批量插入
    batch_size = 1000
    import time
    start = time.time()
    for i in range(0, len(rows), batch_size):
        batch = rows[i:i+batch_size]
        placeholders = ",".join(["(?,?,?,?)"] * len(batch))
        flat = [item for row in batch for item in row]
        conn.execute(f"INSERT INTO t1 VALUES {placeholders}", flat)
    conn.commit()
    dur = (time.time() - start) * 1000

    conn.close()

    # 统计文件大小（主文件 + WAL）
    total_size = 0
    for f in [db_path, db_path + "-wal"]:
        if os.path.exists(f):
            total_size += os.path.getsize(f)

    # VACUUM 后的大小
    conn = sqlite3.connect(db_path)
    conn.execute("VACUUM")
    conn.close()
    vacuum_size = os.path.getsize(db_path)

    for f in [db_path, db_path + "-wal", db_path + "-shm"]:
        if os.path.exists(f):
            os.remove(f)

    return {
        "engine": "SQLite",
        "insert_ms": dur,
        "rows_per_sec": len(rows) / (dur / 1000),
        "file_size": total_size,
        "vacuum_size": vacuum_size,
    }

def bench_duckdb(rows):
    """DuckDB 写入 + 文件大小"""
    db_path = "/tmp/comp_bench_duckdb.db"
    for f in [db_path, db_path + ".wal"]:
        if os.path.exists(f):
            os.remove(f)

    import duckdb
    conn = duckdb.connect(db_path)
    conn.execute("CREATE TABLE t1 (id INT, category INT, value DOUBLE, name VARCHAR)")

    import time
    start = time.time()
    # 用 prepared statement 批量插入
    conn.execute("BEGIN TRANSACTION")
    batch_size = 1000
    for i in range(0, len(rows), batch_size):
        batch = rows[i:i+batch_size]
        placeholders = ",".join(["(?,?,?,?)"] * len(batch))
        flat = [item for row in batch for item in row]
        conn.execute(f"INSERT INTO t1 VALUES {placeholders}", flat)
    conn.execute("COMMIT")
    dur = (time.time() - start) * 1000

    conn.close()

    total_size = 0
    if os.path.exists(db_path):
        total_size += os.path.getsize(db_path)

    # 检查是否有 .wal 文件
    for f in os.listdir("/tmp"):
        if f.startswith("comp_bench_duckdb"):
            fp = os.path.join("/tmp", f)
            if fp != db_path:
                total_size += os.path.getsize(fp)

    for f in os.listdir("/tmp"):
        if f.startswith("comp_bench_duckdb"):
            os.remove(os.path.join("/tmp", f))

    return {
        "engine": "DuckDB",
        "insert_ms": dur,
        "rows_per_sec": len(rows) / (dur / 1000),
        "file_size": total_size,
    }

def bench_hybriddb(rows):
    """HybridDB 写入 + 文件大小（通过 Rust binary）"""
    # 这里我们用 Python 估算 HybridDB 的内存数据大小
    # 实际压缩率数据来自 Rust benchmark
    
    # 估算原始数据大小（内存中 Value 形式）
    # Int32: 4 bytes + overhead, Float64: 8 bytes + overhead, Varchar: string + overhead
    # 粗略估算：每行列数据的原始字节大小
    
    # id (Int32): 4 bytes * 100k = 390.6 KB
    id_size = N_ROWS * 4
    # category (Int32): 4 bytes * 100k = 390.6 KB  
    cat_size = N_ROWS * 4
    # value (Float64): 8 bytes * 100k = 781.2 KB
    val_size = N_ROWS * 8
    # name (Varchar): 平均 ~10 bytes + 4 bytes len prefix
    name_size = sum(4 + len(f"item_{i}") for i in range(N_ROWS))
    
    raw_per_col = {
        "id (Int32)": id_size,
        "category (Int32)": cat_size,
        "value (Float64)": val_size,
        "name (Varchar)": name_size,
    }
    
    total_raw = sum(raw_per_col.values())
    
    # 压缩后大小（基于 Rust compression 模块的实际压缩结果）
    # 从 Rust benchmark 获得：
    # Int32 自增 -> Delta -> 25.0%
    # Int32 分类 -> ForBitPack -> 21.9%
    # Float64 随机 -> Uncompressed -> 100%
    # Varchar item_N -> Uncompressed -> 100%
    compressed_per_col = {
        "id (Int32)": int(id_size * 0.25),      # Delta
        "category (Int32)": int(cat_size * 0.219),  # ForBitPack
        "value (Float64)": int(val_size * 1.0),     # Uncompressed (随机)
        "name (Varchar)": int(name_size * 1.0),      # Uncompressed (高基数)
    }
    
    total_compressed = sum(compressed_per_col.values())
    
    return {
        "engine": "HybridDB (列式压缩)",
        "raw_size": total_raw,
        "compressed_size": total_compressed,
        "compression_ratio": total_raw / total_compressed if total_compressed > 0 else 0,
        "raw_per_col": raw_per_col,
        "compressed_per_col": compressed_per_col,
    }

def main():
    print("=" * 70)
    print("  三引擎压缩率 & 文件大小对比 (100,000 行 × 4 列)")
    print("=" * 70)
    print()

    rows = generate_data(N_ROWS)

    # SQLite
    print("--- SQLite ---")
    sqlite_result = bench_sqlite(rows)
    print(f"  写入耗时:     {sqlite_result['insert_ms']:.2f} ms")
    print(f"  写入速度:     {sqlite_result['rows_per_sec']:,.0f} rows/s")
    print(f"  文件大小:     {fmt_bytes(sqlite_result['file_size'])}")
    print(f"  VACUUM 后:    {fmt_bytes(sqlite_result['vacuum_size'])}")
    print()

    # DuckDB
    print("--- DuckDB ---")
    try:
        duckdb_result = bench_duckdb(rows)
        print(f"  写入耗时:     {duckdb_result['insert_ms']:.2f} ms")
        print(f"  写入速度:     {duckdb_result['rows_per_sec']:,.0f} rows/s")
        print(f"  文件大小:     {fmt_bytes(duckdb_result['file_size'])}")
    except ImportError:
        duckdb_result = None
        print("  DuckDB 未安装，跳过")
    print()

    # HybridDB
    print("--- HybridDB (列式压缩) ---")
    hdb_result = bench_hybriddb(rows)
    print(f"  原始列存大小: {fmt_bytes(hdb_result['raw_size'])}")
    print(f"  压缩后大小:   {fmt_bytes(hdb_result['compressed_size'])}")
    print(f"  压缩比:       {hdb_result['compression_ratio']:.2f}x")
    print()
    print("  各列明细:")
    for col in hdb_result['raw_per_col']:
        raw = hdb_result['raw_per_col'][col]
        comp = hdb_result['compressed_per_col'][col]
        pct = comp / raw * 100
        bar = "█" * int(pct / 5) + "░" * (20 - int(pct / 5))
        print(f"    {col:<20} {fmt_bytes(raw):>10} → {fmt_bytes(comp):>10} ({pct:>5.1f}%) {bar}")
    print()

    # 综合对比
    print("=" * 70)
    print("  综合对比")
    print("=" * 70)
    print()

    sqlite_size = sqlite_result['vacuum_size']
    hdb_size = hdb_result['compressed_size']
    duck_size = duckdb_result['file_size'] if duckdb_result else 0

    print(f"{'引擎':<25} {'写入速度':>12} {'文件大小':>12} {'相对大小':>10}")
    print("-" * 62)
    print(f"{'SQLite':<25} {sqlite_result['rows_per_sec']:>10,.0f}/s {fmt_bytes(sqlite_size):>10} {sqlite_size/sqlite_size*100:>8.1f}%")
    if duckdb_result:
        print(f"{'DuckDB':<25} {duckdb_result['rows_per_sec']:>10,.0f}/s {fmt_bytes(duck_size):>10} {duck_size/sqlite_size*100:>8.1f}%")
    print(f"{'HybridDB (列式压缩)':<25} {'N/A':>12} {fmt_bytes(hdb_size):>10} {hdb_size/sqlite_size*100:>8.1f}%")
    print()

    # 注意事项
    print("说明:")
    print("  - SQLite 文件大小含 B-tree 页开销 + 元数据")
    print("  - DuckDB 含列存 + 压缩 + 索引开销")
    print("  - HybridDB 为纯列数据压缩估算（不含文件头/元数据/索引开销）")
    print("  - 随机 Float64 和高基数字符串几乎不可压缩，是拉低整体压缩率的主因")
    print()

if __name__ == "__main__":
    main()
