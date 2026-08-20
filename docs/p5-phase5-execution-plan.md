# Phase 5 执行计划 — Bloom 优化 + mmap 写路径

> **版本**：v0.28.0 Phase 5
> **基线**：v0.27.0 Phase 4（commit `e85c867`，2026-09-04）
> **分支**：`phase5/bloom-mmap-compact`
> **工期**：2-3 周

---

## 一、阶段目标

1. **Bloom 优化（P0）**：紧缩序列化格式 + 增量构建（避免全列重建）
2. **mmap 写路径（P0）**：大表 compact 走 mmap 写入（不阻塞读路径）
3. **综合验收**：Phase 5 完整回归

---

## 二、关键 KPI

| KPI | Phase 4 末 | Phase 5 目标 |
|---|---|---|
| Bloom 序列化大小 | bit_width*8+12 bytes | **≤ 70%**（紧凑格式） |
| Bloom 重建速度 | 5ms/100K | **< 2ms**（增量） |
| mmap 写入阻塞 | 无 mmap 写 | **非阻塞读路径** |
| compact + mmap 吞吐 | 1.0× | **≥ 50%** 提升 |

---

## 三、详细排期

### P0：Bloom 优化（D1-D4）

| 日期 | 任务 | 产出 |
|---|---|---|
| D1 | Bloom 紧缩序列化：VarInt + packed bits | `src/storage/bloom_filter.rs` |
| D2 | Bloom 增量构建：从已有 bloom 增量添加（跳过全列扫描） | 同上 |
| D3 | ColumnStore compact 中使用增量 Bloom | `src/storage/column_store.rs` |
| D4 | 测试 + bench | `tests/bloom_compact.rs` |

### P0：mmap 写路径（D5-D9）

| 日期 | 任务 | 产出 |
|---|---|---|
| D5 | MmapWriter：append-only mmap 写入 | `src/storage/mmap_writer.rs` |
| D6 | ColumnStore compact 走 mmap 写入 | `src/storage/column_store.rs` |
| D7 | 读写路径分离：compact 不阻塞并发读 | 架构改造 |
| D8 | 测试 + bench | `tests/mmap_write.rs` |
| D9 | 集成 + 报告 | `docs/phase5-report.md` |

---

## 四、决策日志

| 日期 | 决策 | 理由 |
|---|---|---|
| 2026-09-04 | Bloom 紧缩格式用 VarInt 编码 | bit_width/hash_count/count 都是小整数 |
| 2026-09-04 | mmap 写路径用 append-only 模式 | 简单可靠，避免 COW 复杂度 |
| 2026-09-04 | compact + mmap 分阶段：先读路径，再写路径 | 降低风险 |

---

## 五、状态追踪

| 阶段 | 状态 |
|---|---|
| P0 Bloom 优化 | 待启动 |
| P0 mmap 写路径 | 待启动 |