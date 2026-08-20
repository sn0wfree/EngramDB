# Phase 2 执行计划 — 架构与冷热分层

> **版本**：v0.23.0 Phase 2
> **基线**：v0.22.0 Phase 1（commit `030f7b6`，2026-09-04）
> **分支**：`phase2/architecture`
> **工期**：4-6 周（按 ROI 优先 P0 → P1 → P1 → P1）

---

## 一、Phase 1 收获与 Phase 2 起点

### 1.1 Phase 1 已达成

- WAL 组提交验证（机制完整，1.26× on NVMe）
- 列存扫描零拷贝（语义化 + Delta 借用迭代）
- Arena 查询分配器（**+71.5% 低选择性过滤**、+10.4% 列存写入，默认 ON）
- 全量回归 1215+ 测试通过

### 1.2 Phase 1 残留未实现项（来自调研文档）

| 调研项 | Phase 1 状态 | Phase 2 安排 |
|---|---|---|
| mmap 统一读路径 | ❌ | **P0** |
| 冷热数据自动流动 | ❌ | **P0** |
| PREWHERE + Late Materialization 深度优化 | ⚠️ 部分 | **P1** |
| 列级自动编码选择 | ⚠️ 部分 | **P1** |
| LogEngine 写入路径极致优化 | ❌ | **P1** |
| 电源感知模式 | ❌ | Phase 3 |
| 单线程事件循环模式 | ❌ | Phase 3 |
| Buffer pool 处置 | 🪦 死代码 | **Phase 2 顺手清理** |

---

## 二、Phase 2 目标与 KPI

### 2.1 阶段目标

聚焦"**架构升级**"——从单引擎（Columnar 为主）演进到**冷热分层 + mmap 直读 + 自动编码**的多层存储体系：

1. **mmap 统一读路径**（P0）：列存热数据 mmap 直读，OS 级 LRU 替代手工 buffer pool
2. **冷热数据自动流动**（P0）：Engine = Auto，MemoryEngine ↔ ColumnarEngine ↔ Log Engine 调度
4. **PREWHERE + Late Materialization 深度优化**（P1）：列裁剪 + Zone Map 增量
6. **列级自动编码选择**（P1）：基于 DataType + 抽样自动选最优 codec
7. **LogEngine 写入路径极致优化**（P1）：mmap WAL 直写、块压缩异步化

### 2.2 关键 KPI（10 轮 P50）

| KPI | Phase 1 末基线 | Phase 2 目标 | 备注 |
|---|---|---|---|
| mmap 热数据读延迟 | 1.0× | **≥ 3×** 加速 | 子 μs 级 |
| 冷数据压缩比 | 1.0× | **≥ +30%** 压缩率 | 自动编码 + Gorilla |
| 写入放大 | ~3× | **≤ 1.5×** | 减少 Log→Columnar 冗余迁移 |
| 列存扫描内存峰值 | 1.0× | **≥ -50%** | mmap 直读 0 拷贝 |
| 自动分层决策延迟 | — | **< 100ms** | 决策 + 迁移调度 |
| 编码选择 CPU 开销 | — | **< 5%** 写入吞吐损失 | 抽样 + 阈值剪枝 |

---

## 三、当前代码校准（v0.22.0 实测）

| 模块 | 现状 | Phase 2 行动 |
|---|---|---|
| `EngineType` 枚举 | 3 变体：Columnar / Memory / Log | **新增 `Auto`** 驱动自动分层 |
| 列存 typed arrays | `Vec<T>` 内存堆 | **改 `Arc<[T]>` + mmap 后端** |
| `buffer_pool.rs` | 死代码（仅自测试用） | **删除或标注 deprecated** |
| `compress(data, data_type)` | 类型级推荐，无列级选择 | **扩展列级 `recommend()` API** |
| `CompactStrategy` | 4 种（Manual/Full/Incremental/Adaptive） | **新增 `Auto` + heat 跟踪** |
| LogEngine WAL | 每事务 fsync | **mmap 直写 + 异步块压缩** |
| LogEngine `append_columns` | 0 中间分配已实现 | **进一步：异步压缩 + MinMax 增量** |
| 列存 Zone Map | 行组级 Min/Max | **+ Bloom Filter 列级跳读** |

---

## 四、详细排期

### P0-A：mmap 统一读路径（1.5 周）

| 日期 | 任务 | 产出 |
|---|---|---|
| D1 | 引入 `memmap2 = "0.9"` 依赖 | Cargo.toml |
| D1 | `src/storage/mmap_reader.rs`：`MmapReader { file: File, mmap: Mmap, len: usize }` + `slice(offset, len) -> &[u8]` | 新模块 |
| D2 | `ColumnData::from_mmap_slice()`：从 mmap 切片直接构造 typed 数组（零拷贝） | `src/common/column_data.rs` |
| D2 | 列存热路径改造：`ColumnStore` 持有 `Option<MmapReader>` 替代内堆 typed Vec | `src/storage/column_store.rs` |
| D3 | bench 对比：mmap 读 vs 内堆读（10 轮 P50） | bench 数字 |
| D4 | 删除或 deprecated `buffer_pool.rs`（dead code 清理） | `src/storage/buffer_pool.rs` |
| D5 | 持久化路径改造：`persist_to_file` mmap 后端 + `load_from_file` 零拷贝加载 | `src/storage/column_store.rs` |
| D6 | 全量回归 + 集成测试 + bench 数字 | `docs/p2-p0a-mmap-report.md` |

**KPI 验证**：
- 热数据点查延迟 ≤ 1µs（mmap 后命中页缓存）
- 列存扫描内存峰值 -50%
- `cargo test --release --lib` 全绿

### P0-B：冷热数据自动流动（2 周）

| 日期 | 任务 | 产出 |
|---|---|---|
| D1 | `EngineType::Auto` 变体 + 解析 | `src/common/types.rs` |
| D1 | `HeatTracker` 模块：每表访问计数 + 时间衰减 | `src/storage/heat_tracker.rs` |
| D2 | `AutoEngine` 调度器：根据 heat 选 Memory/Columnar/Log | `src/storage/auto_engine.rs` |
| D3 | `migrate_table(table_id, from, to)` API + 数据迁移路径 | `src/storage/migration.rs` |
| D4 | 迁移策略：高频小表→Memory、温表→Columnar、冷表→Log | `src/storage/tier_policy.rs` |
| D5 | 后台 heat 收集 + 自动触发迁移（在 compact 路径钩子） | `src/storage/compact_strategy.rs` |
| D6-D8 | 端到端测试 + bench | `docs/p2-p0b-tier-report.md` |

**KPI 验证**：
- 自动决策延迟 < 100ms
- 高频表自动迁移至 Memory（点查 -50%）
- 冷表自动迁移至 Log（压缩 +30%）

### P1-A：PREWHERE + Late Materialization 深度优化（1 周）

| 日期 | 任务 | 产出 |
|---|---|---|
| D1 | 谓词下推审计：当前 PREWHERE 覆盖范围 | `docs/p2-p1a-prewhere-audit.md` |
| D2 | Zone Map 列级优化：稀疏主键 + Bloom Filter 列索引 | `src/storage/column_store.rs` |
| D3 | Late Materialization：scan 只输出幸存行号 + SELECT 列裁剪 | `src/executor/operators/table_scan.rs` |
| D4 | 选择性 1% bench 对比（10 轮 P50） | bench 数字 |
| D5 | 全量回归 | `docs/p2-p1a-report.md` |

### P1-B：列级自动编码选择（1 周）

| 日期 | 任务 | 产出 |
|---|---|---|
| D1 | 列级 `recommend(data, data_type) -> CompressionType` API | `src/storage/compression/mod.rs` |
| D2 | DataType → 默认候选集：时间戳→Delta+DoubleDelta、Int64→For+Bitpack、低基数→Dict | 新模块 |
| D3 | 抽样 + 多 codec 对比 → 选最小者 | `src/storage/compression/sampler.rs` |
| D4 | 集成到 `compact` 与 `merge_delta`：列存落盘走自动编码 | `src/storage/column_store.rs` |
| D5 | bench 数字 + 全量回归 | `docs/p2-p1b-report.md` |

### P1-C：LogEngine 写入路径极致优化（1 周）

| 日期 | 任务 | 产出 |
|---|---|---|
| D1 | LogEngine WAL 路径审计 | `docs/p2-p1c-log-audit.md` |
| D2 | 跳过 WAL（LogEngine 即 append-only，自身即数据） | `src/storage/log_engine.rs` |
| D3 | 块压缩异步化：未压缩块先落盘，后台线程压缩 | `src/storage/async_compress.rs` |
| D4 | MinMax 索引增量构建（不阻塞写入） | `src/storage/log_engine.rs` |
| D5 | bench + 全量回归 | `docs/p2-p1c-report.md` |

### P1-D：综合集成 + Phase 2 报告（0.5 周）

| 任务 | 产出 |
|---|---|
| 所有 P0/P1 项目联动测试 | `docs/p2-integration-report.md` |
| 重跑 D0 + 所有 bench 收集数字 | bench 数据 |
| 综合验收报告 | `docs/phase2-report.md` |

---

## 五、风险与缓解

| 风险 | 概率 | 影响 | 缓解 |
|---|---|---|---|
| mmap 在大文件上 munmap 慢 | 中 | 中 | 用 `MmapOptions::map()` lazy + mmap resize on persist |
| `EngineType::Auto` 与现有写路径兼容 | 中 | 高 | 默认按现有 Columnar 行为，仅显式 `Auto` 时启动调度 |
| 列级编码选择增加 CPU 开销 | 低 | 中 | 抽样（首 1024 值）而非全列对比 |
| 自动分层迁移阻塞写入 | 中 | 高 | 迁移在后台线程异步执行；用户显式 `compact` 同步路径保留 |
| LogEngine 跳过 WAL 引发崩溃恢复问题 | 中 | 高 | 保留 WAL 选项（`log_skip_wal: bool` 配置），默认 ON 跳过 |

---

## 六、决策日志

| 日期 | 决策 | 理由 |
|---|---|---|
| 2026-09-04 | Phase 2 主线走 mmap + 冷热分层 + 自动编码 | Phase 1 已释放性能上限，需架构升级 |
| 2026-09-04 | `EngineType::Auto` 仅显式启用（默认走用户指定引擎） | 避免隐式迁移导致的数据丢失风险 |
| 2026-09-04 | Buffer pool 整体删除（dead code） | 节省维护成本 + 简化代码路径 |
| 2026-09-04 | 列级编码：抽样对比，候选集按 DataType 过滤 | CPU 开销可控 |

---

## 七、范围外（推迟到 Phase 3）

- 电源感知模式（移动端电池场景）
- 单线程事件循环模式（agent 场景）
- 跨进程共享 mmap（POSIX shm）

---

## 八、状态追踪

| 里程碑 | 状态 | 起始 | 截止 | 验收 |
|---|---|---|---|---|
| P0-A mmap 统一读路径 | 待启动 | TBD | TBD | 热数据点查 ≤ 1µs；内存峰值 -50% |
| P0-B 冷热数据自动流动 | 待启动 | TBD | TBD | 自动决策 < 100ms |
| P1-A PREWHERE 深度优化 | 待启动 | TBD | TBD | 选择性 1% 查询再降 30% |
| P1-B 列级自动编码 | 待启动 | TBD | TBD | 冷数据压缩 +30% |
| P1-C LogEngine 极致优化 | 待启动 | TBD | TBD | 写入吞吐 +50% |
| P1-D 综合集成 | 待启动 | TBD | TBD | Phase 2 报告 |