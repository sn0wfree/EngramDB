# Phase 4 执行计划 — MVCC深度集成 + mmap advise + Bloom持久化 + 大文件切片

> **版本**：v0.27.0 Phase 4
> **基线**：v0.26.0 Phase 3.5（commit `d55a623`，2026-09-04）
> **分支**：`phase4/deep-integration`
> **工期**：2 周

---

## 一、阶段目标

完成4项关键改进：

1. **Auto 迁移 MVCC 深度集成**（P0）— 清理旧版本链 + 重建
2. **mmap OS advise**（P1）— MADV_WILLNEED / PrefetchVirtualMemory
3. **Bloom 磁盘持久化**（P1）— 避免重启重建
4. **mmap 跨 region 切片**（P2）— 大文件友好

---

## 二、关键 KPI

| KPI | Phase 3.5 末 | Phase 4 目标 |
|---|---|---|
| 迁移后 MVCC 版本链状态 | 旧链残留 | **干净（cleared）** |
| 迁移后新写入 MVCC | 正常 | **正常 + 零孤儿版本** |
| mmap advise 延迟降低 | 无 | **≥ 30%** 大顺序读 |
| Bloom 持久化重建时间 | 5ms/100K | **< 1ms**（缓存） |
| mmap 跨 region 切片 | panic | **正常返回数据** |

---

## 三、详细排期

### P0：MVCC 深度集成（D1-D3）

| 日期 | 任务 | 产出 |
|---|---|---|
| D1 | MvccStore::clear() + TransactionManager::clear_mvcc_table() | `src/txn/mvcc.rs` + `src/txn/manager.rs` |
| D2 | 迁移前清旧版本 + 迁移后写路径正确性验证 | `tests/migration_mvcc_deep.rs` |
| D3 | 集成 + 报告 | `docs/p4-p0-mvcc-deep-report.md` |

### P1-A：mmap OS advise（D4-D5）

| 日期 | 任务 | 产出 |
|---|---|---|
| D4 | MmapReader::prefetch 实现：Linux madvise | `src/storage/mmap_reader.rs` |
| D5 | macOS + Windows 平台分支 | 同上 + 测试 |

### P1-B：Bloom 磁盘持久化（D6-D8）

| 日期 | 任务 | 产出 |
|---|---|---|
| D6 | ColumnBloom::to_bytes() / from_bytes() | `src/storage/bloom_filter.rs` |
| D7 | ColumnChunk 序列化/反序列化中加入 bloom | `src/storage/column_store.rs` |
| D8 | 集成 + 测试 | `tests/bloom_persist.rs` |

### P2：mmap 跨 region 切片（D9-D11）

| 日期 | 任务 | 产出 |
|---|---|---|
| D9 | RegionMmapReader 跨 region 切片实现 | `src/storage/mmap_reader.rs` |
| D10 | 测试 80MB+ 文件跨 region 读取 | `tests/mmap_region_slice.rs` |
| D11 | 报告 | `docs/phase4-report.md` |

---

## 四、决策日志

| 日期 | 决策 | 理由 |
|---|---|---|
| 2026-09-04 | P0 迁移清旧 MVCC：`mvcc.clear(table_id)` | 比逐版本重建简单得多，迁移只保留已提交数据 |
| 2026-09-04 | P1-A advise 用 libc 而非 memmap2 API | memmap2 0.9 未提供 advise |
| 2026-09-04 | P1-B Bloom 序列化用简单格式（bit_width + count + bits） | 向后兼容，小体积 |
| 2026-09-04 | P2 跨 region 切片用读入临时缓冲区 | 简单可靠，不引入额外 mmap 依赖 |

---

## 五、范围外

- 并发迁移（Phase 3.5 单线程模型不变）
- Bloom 增量构建（每次全量重建）
- 写路径 mmap（保持现有内堆 Vec）

---

## 六、状态追踪

| 阶段 | 状态 |
|---|---|
| P0 MVCC | 待启动 |
| P1-A advise | 待启动 |
| P1-B Bloom 持久化 | 待启动 |
| P2 跨 region | 待启动 |