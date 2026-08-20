# Phase 3 执行计划 — Auto 迁移 + mmap + Bloom 扩展 + 异步压缩

> **版本**：v0.25.0 Phase 3
> **基线**：v0.24.0 Phase 2.5（commit `4e071b1`，2026-09-04）
> **分支**：`phase3/data-migration`
> **工期**：3-4 周（按 ROI 优先 P0 → P1 → P1 → P2）

---

## 一、阶段目标

完成 Phase 2.5 推迟的四项关键改进：

1. **Auto 表数据迁移（P0）**：实际搬迁数据，而非仅决策
2. **列存 mmap 全集成（P1）**：冷数据 mmap 直读 + COW 写入
3. **Float/Varchar Bloom（P1）**：扩展 hash 函数支持所有类型
4. **异步块压缩（P2）**：rayon 后台线程压缩未压缩块

---

## 二、关键 KPI（10 轮 P50）

| KPI | Phase 2.5 末 | Phase 3 目标 |
|---|---|---|
| Auto 表迁移延迟（10K 行） | N/A | **< 50ms** |
| mmap 冷数据读延迟 | N/A | **< 5µs**（页缓存命中） |
| mmap 写放大 | N/A | **≤ 1.2×**（COW） |
| Float Bloom Filter | N/A | **< 5%** 假阳性 |
| Varchar Bloom Filter | N/A | **< 5%** 假阳性 |
| 异步压缩吞吐 | N/A | **不阻塞**写入 |
| 列存全扫（mmap 后端） | 1.0× | **≥ 3×**（零拷贝） |

---

## 三、详细排期

### P0：Auto 表数据迁移（D1-D8）

| 日期 | 任务 | 产出 |
|---|---|---|
| D1 | `migrate_table()` 框架 API + 数据搬迁抽象 | `src/storage/migration.rs` |
| D2 | Columnar → Memory 迁移（全部 in-memory） | 同上 + 测试 |
| D3 | Memory → Columnar 迁移 | 同上 + 测试 |
| D4 | Columnar → Log 迁移（块格式转换） | 同上 + 测试 |
| D5 | Log → Columnar 迁移（紧凑读全列） | 同上 + 测试 |
| D6-D7 | 集成到 `on_commit()` 自动触发 + 迁移调度 | `src/storage/tier_migration.rs` |
| D8 | 全量集成 + 报告 | `docs/p3-p0-migration-report.md` |

### P1-A：Float/Varchar Bloom（D9-D11）

| 日期 | 任务 | 产出 |
|---|---|---|
| D9 | `bloomable_key()` 支持 Float32/Float64（bits 转 i64） | `src/storage/bloom_filter.rs` |
| D10 | `bloomable_key()` 支持 Varchar/Json（hash 字符串） | 同上 |
| D11 | `build_bloom_from_column()` 扩展类型支持 + 测试 | `tests/bloom_extended.rs` |

### P1-B：列存 mmap 全集成（D12-D18）

| 日期 | 任务 | 产出 |
|---|---|---|
| D12-D13 | 列存 typed 数组支持 mmap 后端（仅冷 RG） | `src/storage/column_store.rs` |
| D14-D15 | `persist_to_file` mmap 写路径 + `load_from_file` mmap 读路径 | 同上 |
| D16-D17 | COW 写入：mmap 区段 + 新数据追加，原子切换 | 同上 |
| D18 | 集成 + 报告 | `docs/p3-p1-mmap-report.md` |

### P2：异步块压缩（D19-D22）

| 日期 | 任务 | 产出 |
|---|---|---|
| D19 | rayon 引入 + 后台线程池 | `Cargo.toml` |
| D20 | 异步压缩队列：未压缩块 → 后台 → 压缩态 | `src/storage/async_compress.rs` |
| D21 | 读路径回退：压缩未完成时解压原数据 | 同上 |
| D22 | 集成 + 报告 | `docs/p3-p2-async-compress-report.md` |

### P3：综合验收 + 最终报告（D23-D25）

| 日期 | 任务 | 产出 |
|---|---|---|
| D23 | 重跑所有 bench 收集 Phase 3 数字 | bench 数据 |
| D24 | 全量回归测试 | 1250+ 测试通过 |
| D25 | Phase 3 综合报告 | `docs/phase3-report.md` |

---

## 四、决策日志

| 日期 | 决策 | 理由 |
|---|---|---|
| 2026-09-04 | P0 优先级最高 | Phase 2.5 仅决策不执行，用户价值未闭环 |
| 2026-09-04 | Float Bloom 用 bits 转 i64（IEEE 754 → u64 → i64） | 与现有 hash 路径复用 |
| 2026-09-04 | Varchar Bloom 用 FxHash 字符串 hash | 简单稳定，无额外 hash 函数依赖 |
| 2026-09-04 | 列存 mmap 仅冷 RG（active 仍用内堆） | 热数据性能优先 + mmap COW 复杂度高 |
| 2026-09-04 | 异步压缩用 rayon（而非 tokio） | rayon 同步 API，简化错误处理 |

---

## 五、范围外（推迟到 Phase 4）

- 跨进程 mmap（POSIX shm）
- 自适应压缩级别（zstd level 动态调整）
- 远程持久化（S3 / NFS）

---

## 六、状态追踪

| 阶段 | 状态 | 起始 | 截止 |
|---|---|---|---|
| P0 Auto 迁移 | 待启动 | TBD | TBD |
| P1-A Bloom 扩展 | 待启动 | TBD | TBD |
| P1-B mmap 列存 | 待启动 | TBD | TBD |
| P2 异步压缩 | 待启动 | TBD | TBD |
| P3 综合验收 | 待启动 | TBD | TBD |