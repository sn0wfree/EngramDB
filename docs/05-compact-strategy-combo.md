# Compact 策略组合方案设计

**版本**: v0.11.3  
**日期**: 2026-08-02  
**范围**: HybridDB Delta→ColumnStore 合并策略体系

---

## 1. 策略体系总览

HybridDB v0.11.3 实现了 **4 种 Delta 合并策略 + 1 种联动机制**，形成可组合的策略体系：

```
┌─────────────────────────────────────────────────────────────┐
│                    CompactStrategy 枚举                      │
├──────────┬──────────┬──────────────┬────────────────────────┤
│  Manual  │   Full   │ Incremental  │       Adaptive         │
│  (手动)   │ (全量合并) │  (增量式)     │     (自适应分桶)        │
└──────────┴──────────┴──────────────┴────────────────────────┘
                              │
                              ▼
                    ┌──────────────────┐
                    │  sync_wal 联动    │
                    │  (Periodic模式)   │
                    └──────────────────┘
```

### 四种策略对比

| 策略 | 触发方式 | 合并粒度 | 阻塞时间 | 适用场景 |
|------|---------|---------|---------|---------|
| **Manual** | 完全手动 | 全量 | 用户决定 | 批量导入、ETL、高级用户 |
| **Full** | 固定阈值 | 全量 | 不可控（大表长） | 写入量小、延迟不敏感 |
| **Incremental** | 固定阈值 | 固定批次 | 可控（batch_size） | 交互式、延迟敏感 |
| **Adaptive** | 自适应阈值 | 固定批次 | 可控 + 自动适配 | 通用场景（默认） |

---

## 2. 组合方案设计

### 2.1 默认组合（开箱即用）

**策略**: Adaptive（自适应分桶）  
**WAL 模式**: Sync  
**sync_wal 联动**: 开启（但 Sync 模式下不触发）

```rust
// 默认配置等价于：
let config = Config {
    compact_strategy: CompactStrategy::Adaptive {
        min_threshold: 10_000,
        max_threshold: 122_880,   // 1 个 Row Group
        pct_of_table: 0.10,       // 表行数的 10%
        batch_size: 122_880,      // 每次合并 1 个 Row Group
    },
    wal_flush_mode: WalFlushMode::Sync,
    sync_wal_compact: true,
    ..Default::default()
};
```

**设计理由**：
- 自适应阈值：小表（<10 万行）1 万行触发，频繁快合并；大表（>120 万行）120K 行触发，有上限不膨胀
- 增量合并：每次最多合并一个 Row Group，阻塞时间 5-20ms（内存态）
- Sync WAL：默认最安全，数据零丢失

### 2.2 批量导入组合

**策略**: Manual  
**WAL 模式**: BufferFull 或 Periodic  
**sync_wal 联动**: 关闭

```rust
// 1. 导入前切换到手动模式
conn.set_compact_strategy(CompactStrategy::manual());
conn.set_wal_flush_mode(WalFlushMode::BufferFull);

// 2. 批量导入（用 import_columns 零拷贝路径）
conn.import_columns("events", columns)?;

// 3. 导入完成后手动合并 + 切回默认
conn.compact_all()?;
conn.set_compact_strategy(CompactStrategy::default_adaptive(122_880));
conn.set_wal_flush_mode(WalFlushMode::Sync);
```

**设计理由**：
- Manual 策略：导入过程中完全不触发 compact，写入最快
- BufferFull WAL：减少 fsync 次数，导入速度提升显著
- 导入完成后一次性全量合并：总开销最小，列存结构最整齐

### 2.3 高吞吐写入组合

**策略**: Incremental（增量式）  
**WAL 模式**: Periodic  
**sync_wal 联动**: 开启

```rust
let config = Config {
    compact_strategy: CompactStrategy::Incremental {
        threshold: 50_000,      // Delta 到 5 万行触发
        batch_size: 10_000,     // 每次合并 1 万行，阻塞 < 2ms
    },
    wal_flush_mode: WalFlushMode::Periodic,
    wal_buffer_size: 256 * 1024, // 256KB 缓冲区
    sync_wal_compact: true,      // sync_wal 时顺便 compact
    ..Default::default()
};

// 应用层定期调用（比如每秒一次）
conn.sync_wal()?;
```

**设计理由**：
- 小批次增量合并：每次只合并 1 万行，阻塞时间极短（< 2ms）
- Periodic WAL + sync_wal 联动：把 WAL 刷盘和 compact 合并到同一个同步点，减少 I/O 次数
- 应用层控制节奏：sync_wal 的频率决定了持久化粒度和 compact 频率

### 2.4 读多写少组合

**策略**: Full（全量合并）  
**WAL 模式**: Sync  
**sync_wal 联动**: 开启

```rust
conn.set_compact_strategy(CompactStrategy::full(50_000));
```

**设计理由**：
- 写入少，合并次数少，全量合并总开销最低
- 全量合并后列存结构最整齐，查询性能最优
- 因为写入少，即使合并时阻塞稍长也不影响体验

### 2.5 混合负载组合（多表不同策略）

**策略**: 按表设置不同策略  
**WAL 模式**: Sync（默认）

```rust
// 日志表：写入量大，用增量式
conn.set_table_compact_strategy("access_log",
    CompactStrategy::incremental(30_000, 10_000))?;

// 维度表：写入极少，用全量
conn.set_table_compact_strategy("dim_product",
    CompactStrategy::full(10_000))?;

// 临时表：完全手动管理
conn.set_table_compact_strategy("tmp_import_batch",
    CompactStrategy::manual())?;

// 其他表：默认自适应
```

**设计理由**：
- 不同表的读写特征不同，一刀切的策略不是最优
- 表级策略切换成本为零（只是改一个枚举值）
- 支持运行时动态切换，可以根据业务时段调整

---

## 3. 策略切换矩阵

运行时动态切换策略的行为保证：

| 从 \ 到 | Manual | Full | Incremental | Adaptive |
|---------|--------|------|-------------|----------|
| **Manual** | - | 下次写入触发检查 | 下次写入触发检查 | 下次写入触发检查 |
| **Full** | 停止自动触发 | - | 下次写入按新阈值+批次 | 下次写入按自适应逻辑 |
| **Incremental** | 停止自动触发 | 下次写入按新阈值全量 | - | 下次写入按自适应逻辑 |
| **Adaptive** | 停止自动触发 | 下次写入按新阈值全量 | 下次写入按新阈值+批次 | - |

**注意**：
- 切换策略是即时生效的，不影响已有的 Delta 数据
- 从"自动"切到"手动"后，已积累的 Delta 不会自动合并，需要手动调用 `compact()`
- 从"手动"切到"自动"后，下次写入时会检查阈值，如果 Delta 已经超过阈值会立即触发合并

---

## 4. 与 WAL 策略的联动矩阵

| WAL 模式 | sync_wal_compact | 联动行为 |
|---------|-----------------|---------|
| **Sync** | true / false | 无影响（每次 commit 都 fsync，compact 独立调度） |
| **BufferFull** | true / false | 无影响（缓冲区满自动 flush，compact 独立调度） |
| **Periodic** | **true** | **sync_wal() 时先刷盘，再检查所有表的 compact** |
| **Periodic** | false | 无联动，compact 只在写入路径触发 |

### Periodic + sync_wal_compact 的工作流程

```
应用层调用 sync_wal()
    │
    ├─► WAL 刷盘 + fsync（保证持久性）
    │
    └─► 遍历所有表
            │
            ├─► 表1: maybe_compact() → 未达阈值 → 跳过
            ├─► 表2: maybe_compact() → 达到阈值 → 合并 batch_size 行
            └─► 表3: maybe_compact() → 达到阈值 → 合并 batch_size 行
```

**优势**：
1. **I/O 合并**：WAL 刷盘和 compact 落盘在同一个调用中，减少磁盘同步点
2. **调度集中**：所有"慢操作"都在 sync_wal() 中，应用层只需管理一个定时点
3. **可控性强**：应用层决定 sync_wal 的频率，间接控制 compact 频率

---

## 5. 性能特征对比

### 5.1 写入延迟分布

（假设 10 列 Int64，行大小 ~80 字节）

| 策略 | P50 延迟 | P99 延迟 | 最大延迟 | 说明 |
|------|---------|---------|---------|------|
| Manual | ~1µs | ~2µs | ~5µs | 写入路径零额外开销 |
| Full | ~1µs | ~2µs | **100-500ms** | 平时很快，触发时卡很久 |
| Incremental | ~1µs | ~500µs | ~5ms | 小批次合并，抖动小 |
| Adaptive | ~1µs | ~500µs | ~20ms | 自适应阈值，大表有上限 |

### 5.2 总开销对比

| 策略 | 合并次数 | 单次开销 | 总开销 | 空间利用率 |
|------|---------|---------|--------|-----------|
| Manual | 最少（用户决定） | 最高（全量） | 最低 | 最高 |
| Full | 最少 | 最高 | 最低 | 最高 |
| Incremental | 最多 | 最低 | 略高 | 略低 |
| Adaptive | 适中 | 低 | 适中 | 高 |

---

## 6. 选型决策树

```
开始
  │
  ├─► 写入量很大（> 10K 行/秒）？
  │     ├─ 是 ──► 能接受周期刷盘吗？
  │     │           ├─ 是 ──► 【高吞吐组合】Incremental + Periodic + sync_wal联动
  │     │           └─ 否 ──► 【延迟敏感组合】Incremental + Sync
  │     └─ 否 ──► 下一个问题
  │
  ├─► 是批量导入场景吗？
  │     ├─ 是 ──► 【批量导入组合】Manual + BufferFull + 导入后手动compact
  │     └─ 否 ──► 下一个问题
  │
  ├─► 写入量很小（< 100 行/秒）？
  │     ├─ 是 ──► 【读多写少组合】Full(threshold=10K) + Sync
  │     └─ 否 ──► 下一个问题
  │
  └─► 不确定 / 混合负载 ──► 【默认组合】Adaptive + Sync
```

---

## 7. 监控与调优

### 7.1 关键指标

建议监控以下指标来判断策略是否合适：

| 指标 | 获取方式 | 说明 |
|------|---------|------|
| Delta 行数 | `table.delta_store().len()` | 过高说明合并跟不上 |
| 合并频率 | 统计 maybe_compact 返回 > 0 的次数 | 过高说明阈值太小 |
| 单次合并行数 | `maybe_compact()` 返回值 | 应该接近 batch_size |
| 写入延迟 P99 | 应用层统计 | 应该稳定在预期范围内 |
| 列存 Row Group 数 | `table.column_store().row_group_count()` | 过多说明合并太碎 |

### 7.2 调优指南

**症状：P99 写入延迟太高**
- 检查是否 Full 策略 → 换成 Incremental 或 Adaptive
- 如果是 Incremental → 减小 batch_size
- 如果是 Adaptive → 减小 max_threshold 和 batch_size

**症状：Delta 层一直很大，查询慢**
- 检查是否 Manual 策略 → 调用 compact()
- 如果是 Full 策略 → 降低 threshold
- 如果是 Incremental → 降低 threshold 或 增大 batch_size
- 如果是 Adaptive → 降低 min_threshold 或增大 pct_of_table

**症状：合并太频繁，CPU 占用高**
- 增大 threshold（Full/Incremental）
- 增大 min_threshold 或 pct_of_table（Adaptive）
- 换成 Full 策略（合并次数最少）

**症状：Periodic 模式下 sync_wal() 太慢**
- 关闭 sync_wal_compact，让 compact 回到写入路径触发
- 或增大各表的阈值，减少 sync_wal 时实际合并的表数
- 或减小 batch_size，让每次合并更快

---

## 8. 代码示例

### 8.1 默认用法（开箱即用）

```rust
use hybriddb::Connection;

let mut conn = Connection::open("data.hdb")?;
// 默认就是 Adaptive + Sync，什么都不用配
conn.execute("CREATE TABLE t (id INT, name TEXT)")?;
conn.execute("INSERT INTO t VALUES (1, 'a')")?;
```

### 8.2 批量导入

```rust
use hybriddb::{Connection, CompactStrategy, WalFlushMode};

let mut conn = Connection::open("data.hdb")?;

// 切换到导入模式
conn.set_compact_strategy(CompactStrategy::manual());
conn.set_wal_flush_mode(WalFlushMode::BufferFull);

// 批量导入（零拷贝列式路径）
let columns = vec![
    (0..1_000_000).map(|i| Value::Int64(i)).collect(),
    (0..1_000_000).map(|i| Value::Text(format!("name_{}", i))).collect(),
];
conn.import_columns("big_table", columns)?;

// 导入完成：合并 + 切回默认
conn.compact_all()?;
conn.set_compact_strategy(CompactStrategy::default_adaptive(122_880));
conn.set_wal_flush_mode(WalFlushMode::Sync);
```

### 8.3 高吞吐 + Periodic 模式

```rust
use hybriddb::{Connection, CompactStrategy, WalFlushMode, Config};

let config = Config {
    compact_strategy: CompactStrategy::incremental(50_000, 10_000),
    wal_flush_mode: WalFlushMode::Periodic,
    wal_buffer_size: 256 * 1024,
    sync_wal_compact: true,
    ..Default::default()
};

let mut conn = Connection::open_with_config("data.hdb", config)?;

// 应用层主循环
loop {
    // 处理一批写入
    for _ in 0..1000 {
        conn.execute("INSERT INTO metrics VALUES (...)")?;
    }
    // 每秒刷一次盘 + 合并
    conn.sync_wal()?;
    std::thread::sleep(Duration::from_secs(1));
}
```

### 8.4 按表设置不同策略

```rust
use hybriddb::{Connection, CompactStrategy};

let mut conn = Connection::open("data.hdb")?;

// 日志表：增量式，小批次快合并
conn.set_table_compact_strategy("access_log",
    CompactStrategy::incremental(30_000, 10_000))?;

// 配置表：写入极少，全量合并
conn.set_table_compact_strategy("config",
    CompactStrategy::full(5_000))?;

// 临时表：手动管理
conn.set_table_compact_strategy("temp_stage",
    CompactStrategy::manual())?;
```

---

## 9. 未来扩展方向

### 9.1 已规划（近期）

- **事务感知 compact**：检查活跃事务快照，只合并不影响快照一致性的数据
- **Compact 统计信息**：记录每次合并的时间、行数、耗时，支持查询历史

### 9.2 中期

- **异步 I/O 模式**：可选 feature，使用 tokio 支持 compact 异步执行
- **后台线程模式**：可选 feature，开启后台 compaction 线程（服务器部署场景）

### 9.3 远期

- **分级 compact**：类似 LSM-Tree 的多层结构，Delta → L1 → L2，每层不同的合并策略
- **自适应策略调整**：根据监控指标自动调整策略参数（类似数据库的 auto-tuning）

---

## 附录：API 速查表

### 配置层（Config）

```rust
config.compact_strategy    // CompactStrategy，全局默认策略
config.sync_wal_compact    // bool，sync_wal 是否联动 compact
config.wal_flush_mode      // WalFlushMode，WAL 刷盘策略
config.wal_buffer_size     // usize，WAL 缓冲区大小
```

### 连接层（Connection）

```rust
conn.compact(table_name)           // 手动合并指定表（全量）
conn.compact_all()                 // 手动合并所有表（全量）
conn.set_compact_strategy(s)       // 设置全局默认策略（新建表生效）
conn.set_table_compact_strategy(t, s)  // 设置指定表的策略
conn.sync_wal()                    // 手动 WAL 刷盘（Periodic 模式下联动 compact）
conn.set_wal_flush_mode(mode)      // 设置 WAL 刷盘策略
conn.import_columns(table, cols)   // 零拷贝列式导入
```

### 策略构造

```rust
CompactStrategy::manual()                                    // 手动
CompactStrategy::full(threshold)                             // 全量合并
CompactStrategy::incremental(threshold, batch_size)          // 增量式
CompactStrategy::default_adaptive(row_group_size)            // 自适应分桶（推荐）
```
