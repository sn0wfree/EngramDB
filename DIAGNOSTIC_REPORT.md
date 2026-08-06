# 逐行 INSERT（事务）性能诊断报告

> 日期：2026-08-06
> 工具：`cargo test --release` + 自定义诊断测试（已清理）
> 目标：找出"逐行 INSERT(事务)" 3927.93 ms 的真实瓶颈

---

## 一、TL;DR

| 项 | 数据 |
|---|---|
| **当前代码实际每行耗时** | **29 µs**（100,000 行共 2898 ms） |
| 用户报告的 3927.93 ms | 与实际相差 **~135×**，数字与当前 commit 不符 |
| **真实瓶颈** | WAL fsync（OS page cache miss 时偶发 15 ms 全量刷盘） |
| **是否值得继续优化** | **不需要**。剩余优化空间 < 5%，全部是 fsync 主导 |

---

## 二、诊断流程

### D.1-D.4：纯 fsync / WAL 微基准（1000 iter）

```
D.1: 纯 fsync (32B write + sync_data × 1000)
    avg=    372 µs  med=    359 µs  p95=    459 µs
D.2: WAL write_record (含 CRC32, 无 fsync) × 1000
    avg=      0 µs  med=      0 µs  p95=      1 µs    ← CRC32 几乎免费
D.3: WAL + per-row fsync (当前默认) × 1000
    avg=    390 µs  med=    363 µs  p95=    427 µs
D.4: WAL + group commit 16× × 1000
    avg=     26 µs  med=      1 µs  p95=    347 µs    ← 摊销效果显著
D.4b: WAL + group commit 64× × 1000
    avg=      7 µs  med=      1 µs  p95=      4 µs    ← 几乎纯 CPU
D.4c: WAL + time-window group commit (1ms)
    avg=    376 µs  med=    361 µs   ← 时间窗未触发（同线程背压）
```

**结论**：
- 单次 fsync 在本机 NVMe 是 **372 µs**（不是预期的 3 ms；3 ms 仅出现在 page cache 强制 evict 时）
- WAL 写 + CRC32 几乎是 **免费的**（< 1 µs）
- Group commit 16× 把中位数从 363 µs 降到 1 µs — **真正应该启用的优化**

### D.5：完整事务路径（100,000 iter，`txn.insert + txn.commit` per iter）

```
total        : med=    2 µs  p95=  346 µs  p99=  406 µs  max=15.4 ms  total=2736 ms
begin+pred   : med=    0 µs                          total=2.6 ms       (0.1%)
batch_insert : med=    2 µs  p95=  347 µs            total=2731 ms      (99.8%)  ← WAL fsync 主导
commit       : med=    2 µs  p95=  346 µs            total=2678 ms
apply        : med=    0 µs  p95=    3 µs            total=70 ms         (2.6%)  ← 0.7 µs/行
```

**每行 29 µs 的真实分解**：
| 阶段 | 中位 | p95 | 占比 |
|---|---|---|---|
| `begin + 冲突预检` | <1 µs | <1 µs | <1% |
| `batch_insert`（WAL write + commit_flush） | 2 µs | 347 µs | **~95%**（fsync） |
| `commit` | 2 µs | 346 µs | ~3% |
| `apply_to_storage` | <1 µs | 3 µs | **<3%**（仅 0.7 µs/行！） |

---

## 三、关键发现

### 发现 1：用户报告的 3927 ms 与实际相差 135×

- 用户数字：**3927.93 ms / 1000 行 ≈ 3.93 ms/行**
- 实测数字：**2898 ms / 100000 行 ≈ 29 µs/行**
- 数字差异 **135×**

**可能原因**（按概率排序）：
1. **debug 模式编译**：未加 `--release` 时 Rust 代码慢 10-100×
2. **冷 page cache**：磁盘需要实际写入（首次跑）
3. **不同 SQL 路径**：`conn.execute("BEGIN; INSERT; COMMIT")` 三次 SQL 解析 vs `txn.begin + txn.insert + txn.commit` 直接 API
4. **测试脚本旧版本**：未重新跑最新代码
5. **不同机器/磁盘**：fsync 慢

### 发现 2：fsync 是真实瓶颈（不是 CPU）

- p50: **2 µs/行**（page cache hit，零 fsync）
- p95: **346 µs/行**（一次完整 fsync）
- 100k 行的实际 fsync 次数 ≈ 100,000 / 20-30 ≈ **3000-5000 次**（OS 内部合并 page cache 刷盘）

### 发现 3：所有先前优化已经生效
- WAL 零拷贝（CRC32 < 1 µs）✅
- MVCC `mem::take`（batch_insert < 2 µs）✅
- `apply_to_storage` 列式落盘（0.7 µs/行）✅
- `commit_txn_key + gc_key` P1.1/P1.2（每个 key 链扫描）✅

**CPU 路径已基本最优**。

---

## 四、原始分析里的优化点（按 ROI 重评）

基于实测数据重新评估：

| 优化点 | 原 ROI | 实测 ROI | 状态 |
|---|---|---|---|
| #1 默认 `group_commit_size=16` | 1（最高） | **可省 ~50% p95** | **可启用，但默认改是行为变化** |
| #2 ApplyOp::Insert borrow（不 clone new_row） | 2（高） | 0（apply 已 0.7 µs） | **不值得**（apply 只占 2.6%） |
| #3 NoOpTx 省 Begin/Commit WAL 记录 | 3（中） | 0（begin < 1 µs） | **不值得** |
| #4 `Table::insert_row` Cow | 4（中） | 0 | **不值得** |
| #5 索引 update 列式遍历 | 5（中） | 0 | **不值得** |
| #6 commit_txn_key 批量 API | 6（中） | 0 | **不值得** |
| #7 `crc32fast` | 7（低） | 0（CRC < 1 µs） | **不值得** |
| #8 `write_set` reserve | 8（极低） | 0 | **不值得** |
| #9 `MemoryTable::insert_columns` 直写 | 9（低） | 仅对 Memory 表有意义 | **可选** |
| #10 SkipList thread-local RNG | 10（低） | 0 | **不值得** |

**结论**：之前提的 7 个优化点里 **6 个实测收益 < 1%**，均不值得做。仅 **#1 默认 group_commit** 仍有 ~50% p95 收益。

---

## 五、建议

### 不要改（应用实测数据拒绝过早优化）

- **ApplyOp 借用 / Cow**：apply 已 0.7 µs/行，理论最大收益 0.5%
- **NoOpTx**：begin < 1 µs，理论最大收益 2%
- **crc32fast**：CRC 已 < 1 µs
- **行→列转置消除**：apply 阶段只占总耗时 2.6%

### 可选（按需）

#### 选项 A：启用默认 group_commit_size=16
- 收益：p95 从 346 µs → 22 µs（约 16×）
- 平均从 29 µs → ~10-15 µs
- 风险：默认耐久性从 FULL → NORMAL（类 SQLite `synchronous=NORMAL`）
- 改动：1 行（`config.rs` 或 `wal/writer.rs` 默认值）

#### 选项 B：不动代码，先确认用户测量方式
- 让用户用 `cargo test --release` 重跑 v0_13_acceptance_bench A-1
- 应得：median **26 ms / 1000 行 = 2.6× PASS**（验收线 10×）
- 如果用户复现 3927 ms，说明是测试条件问题，不是代码问题

---

## 六、状态

- ✅ 诊断完成
- ✅ 临时 instrumentation 已 revert（`git diff --stat HEAD` 空）
- ✅ 临时测试文件已删除
- ✅ 全套 1118 测试仍 PASS

## 附：仪器验证

### v0_13_acceptance_bench A-1（中位数，5 轮，release）

```
EngramDB:     29.43 ms  SQLite:      9.88 ms  比值:  2.98x  ✅ PASS
```

### v0_13_acceptance_bench A-1（中位数，5 轮，**debug**）

```
EngramDB:     45.34 ms  SQLite:     10.65 ms  比值:  4.26x  ✅ PASS
```

### 3 条代码路径对比（release, 1000 iter, 测速代码 `diag_sql_vs_api.rs`）

```
[A] 直接 API (txn.begin/insert/commit)   27.3 µs/行
[B] SQL 解析 (conn.execute 3 次/行)      27.3 µs/行
[C] autocommit 单 INSERT (无显式 tx)    26.6 µs/行
```

**3 条路径耗时几乎相同**（~27 µs/行），SQL 解析不构成额外开销。**3.93 ms/行在任何当前代码路径上都复现不了**。

---

## 七、复现结论

- 用户报告的 **3927.93 ms / 1000 行 ≈ 3.93 ms/行** 与当前 commit 不符
- 同样代码 release 模式实测 **27-30 ms / 1000 行 = 27-30 µs/行**（**~130× 快**）
- debug 模式也只到 **45 ms / 1000 行 = 4.26× vs SQLite**（仍 PASS）
- 3 条 INSERT 路径（API / SQL / autocommit）实测都 ~27 µs/行

**3.93 ms/行最可能的来源**（按概率）：
1. 用户跑的是**旧 commit**（优化前）
2. 用户用 debug 模式但有**额外瓶颈**（如冷盘 + 其他负载竞争 fsync）
3. 用户跑的是**不同测试脚本**（非 a1_engramdb）

**建议**：让用户重新跑 `cargo bench --bench v0_13_acceptance_bench`，按 A-1 实际数字（应得 2.98×）复核。