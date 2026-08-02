# 归档目录

本目录存放历史版本的重复/废弃文件，保留以备查阅。

**这些文件不参与编译**，仅为保留历史而归档。

## 目录结构

### `examples/`

- `bench_full.rs` — 早期版本的完整基准测试示例
  - **规范版本**：`benches/bench_full.rs`（内容略有差异，以 benches/ 为准）

### `src_bin/`

早期放在 `src/bin/` 下的基准测试二进制，现统一迁移到 `benches/`。

- `compact_strategy_bench.rs` — Compact 策略对比基准
  - **规范版本**：`benches/compact_strategy_bench.rs`
- `compact_strategy_deep_bench.rs` — Compact 策略深度测试
  - **规范版本**：`benches/compact_strategy_deep_bench.rs`
- `write_bench.rs` — 写入性能基准
  - **规范版本**：`benches/write_bench.rs`

## 说明

如需恢复某个文件到主代码树，请从本目录复制并放置到对应位置。
