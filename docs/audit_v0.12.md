# EngramDB v0.12.x 现状审计报告

> 审计日期：2026-08-03
> 审计方法：对 `src/` 全部模块逐文件核查，区分 ✅完整 / 🟡部分 / ❌缺失，附 `file:line` 证据。
> 核心结论：**这是一个"功能拼图齐全但未拼装"的项目**——大量子系统带单测完整实现，却在系统主链路上从未被调用。

---

## 一、向量检索

| 特性 | 状态 | 证据 | 备注 |
|---|---|---|---|
| HNSW 索引构建（M/efCon） | ✅ | `storage/vector_index.rs:73-396` | 论文级，参数可配 |
| L2 / 内积 / 余弦 | ✅ | `vector_index.rs:19-66` | 三种均有测试 |
| ef / TopK 可调 | 🟡 | `table.rs:215` | **ef_search 硬编码 50**，查询时不可临时指定 |
| 增量更新 | ✅ | `vector_index.rs:277` | 在线插入不重建 |
| tombstone 删除 | ✅ | `vector_index.rs:153,409-478` | 含 ef 动态补偿 |
| **混合查询（过滤+向量）** | ❌ | `table.rs:270` | `vector_search` 无 filter 参数，全树无 pre/post-filter |
| 多向量字段 | 🟡（有 bug） | `table.rs:573-574` | 增量维护只认**第一个** Vector 列，多列时索引写错 |
| IVF / 量化（PQ/SQ） | ❌ | — | 只有 HNSW + BruteForce |
| 索引持久化（含 tombstone） | ✅ | `vector_index.rs:527-715` | 含旧格式兼容 |
| **SQL 层向量搜索** | ❌ | `sql/parser.rs:729-734` | 无法从 SQL 建向量索引或发起查询；`VECTOR(dim)` 的 dim 解析为 0 占位 |

**关键失实**：原报告"向量检索 ✅ HNSW"掩盖了 **SQL 层完全缺位**和**混合查询缺失**两个致命点——HNSW 是只能 Rust API 调用的孤岛。

## 二、结构化存储与查询

| 特性 | 状态 | 证据 | 备注 |
|---|---|---|---|
| 关系表 + CRUD | ✅ | `executor/operators/insert.rs`, `storage/table.rs` | 单表完整 |
| 二级索引（跳表） | ✅ | `storage/index/skiplist.rs:56-383` | get/range/持久化齐全 |
| 覆盖索引（INCLUDE） | ✅ | `common/types.rs:86-96`, `skiplist.rs:78-272` | 真实生效 |
| 聚簇键 cluster_key | ✅ | `types.rs:107-113`, `delta_store.rs:239-280` | compact 按列聚簇 |
| 过滤 / 排序 / 分页 | ✅（LIMIT 无 OFFSET） | `executor/operators/filter.rs`, `sort.rs` | OFFSET 未实现 |
| 聚合 COUNT/SUM/AVG/MIN/MAX | ✅ | `aggregate.rs:36-176` | 含 NULL 处理 |
| GROUP BY | ✅ | `sql/planner.rs:259-302` | 仅 ColumnRef 分组，表达式分组被丢弃 |
| **HAVING** | ❌ | `planner.rs:252-253` | 解析后计划阶段**丢弃**，不生效 |
| **SELECT DISTINCT** | ❌ | `sql/parser.rs:332-349` | distinct 字段**未读** |
| **JOIN** | ❌ | `planner.rs:352-367` | parser 只取 `from[0]`，JOIN 语法被丢弃；`hash_join.rs`/`join_order.rs` 是死代码 |
| 子查询 / 窗口 / CTE / UNION | ❌ | `sql/ast.rs` | 全无 |
| 全文检索 | ❌ | — | 无 |
| **JSON 路径查询** | 🟡 | `parser.rs:722-728` | 仅存储类型；无 `->`/`->>`/JSON_EXTRACT 路径语法 |

**关键失实**：`hash_join.rs` 与 `join_order.rs` 的存在会误导——它们是**从未被调用的死代码**。

## 三、嵌入式与部署

| 特性 | 状态 | 证据 | 备注 |
|---|---|---|---|
| 零依赖嵌入式 | ✅ | `Cargo.toml:25-62` | 纯 Rust crate，DataFusion 可选 |
| 进程内运行 | ✅ | `lib.rs` | 无网络 |
| **单文件** | 🟡 | `storage/mod.rs`, `txn/manager.rs:49` | **实际是 `.hdb` + `.hdb-wal` 两文件** |
| 文件膨胀控制 | ❌ | `mod.rs:600-601` | checkpoint 追加写，**无 VACUUM**，文件无限增长 |
| 跨平台 | 🟡 | — | 能编译，但测试硬编码 `/tmp`，Windows 跑不动 |
| **WASM** | ❌ | `Cargo.toml` | 无 wasm32 target、同步 IO |
| **多线程安全** | ❌ | `lib.rs:107`, `common/memory_pool.rs:8` | **单线程 `&mut self` 独占**，非多读单写 |

**关键失实**：原报告"单文件 ✅""多线程安全 P1 待实现"——前者实际两文件且会膨胀，后者当前是**单线程独占**，并发模型根本没建立。

## 四、事务与可靠性

| 特性 | 状态 | 证据 | 备注 |
|---|---|---|---|
| **ACID 事务** | 🟡（脱节） | `txn/manager.rs:68-146` | **SQL INSERT 绕过 txn_manager**；事务数据写进独立 HashMap，SELECT 读不到 |
| WAL 写入器本体 | ✅ | `wal/writer.rs:44-244` | CRC32 + 组提交 + 三种刷盘 |
| **WAL 接入 SQL** | ❌ | `executor/operators/insert.rs:8-17` | SQL 路径不写 WAL |
| MVCC 快照隔离 | 🟡（脱节） | `txn/mvcc.rs:43-308` | 版本链完整，但仅 txn 内部闭环 |
| 写写冲突检测 | 🟡（脱节） | `mvcc.rs:86-110` | 仅对 txn API 有效 |
| **崩溃恢复** | ❌ | `wal/recovery.rs:43-147` | `recover()` **只统计不应用**；`open_existing()` 启动**不调用** recover |
| 增量备份 / PITR | ❌ | — | 无 |
| 数据校验 | 🟡 | `wal/mod.rs:145-158` | **仅 WAL 有 CRC**，存储数据页/索引/catalog 无校验 |

**关键失实（最严重）**：原报告"事务能力 ⭐⭐⭐⭐ 接近 SQLite"——实际**事务子系统与存储/SQL 是平行世界**：组件内有实现，系统级 ACID/WAL/MVCC/恢复**均未生效**。SQL 写入既不写 WAL 也不走 MVCC，崩溃后已提交事务不会被 redo。

## 五、Agent 记忆体系 / 六、工具与任务状态

| 类别 | 校准状态 |
|---|---|
| 短期/长期/工作/episodic 记忆、记忆检索/遗忘/整合 | ❌ 全无 |
| 任务队列、工具调用日志、计划步骤、重试记录 | ❌ 全无 |
| 对话历史结构化存储 | ❌ 无专用接口（仅可用普通表手动存） |

原报告这两类标"待实现"正确，是纯净新增工作。

## 七、多模态 / 八、性能与优化

| 特性 | 校准状态 | 证据 | 备注 |
|---|---|---|---|
| 文本 / 数值 | ✅ | `common/types.rs` | 完整 |
| 向量类型 | 🟡 | `types.rs:16-20` | 类型有，SQL 无字面量语法，只能 API 写入 |
| JSON 类型 | 🟡 | `types.rs:11-15` | 存储 + 部分函数，无路径语法 |
| Blob | ❌ | — | 无 |
| 列式存储 | ✅ | `storage/column_store.rs` | RowGroup + MinMax 写入时维护 |
| **压缩默认启用** | ❌ | `column_store.rs:249-281`, `mod.rs:388` | `compress_all` **从未被调用**；compact 不压缩；持久化时反而 decompress——压缩代码运行时完全无效 |
| **PREWHERE 跳读** | ❌ | `executor/operators/table_scan.rs:46-78` | 占位实现，注释明写 TODO，实际全表扫描 |
| **MinMax 跳过** | ❌ | `table_scan.rs:103-113` | `estimate_skipping` 返回 (0,0) 占位 |
| 稀疏索引 | 🟡 | `storage/sparse_index.rs` | 实现完整**从未被引用** |
| 位图 / 布隆索引 | 🟡 | `index/bitmap.rs`, `index/bloom.rs` | 实现完整**从未被引用** |
| 缓冲池 | 🟡 | `storage/buffer_pool.rs` | 实现完整**从未实例化**，用裸 File IO |
| 索引持久化 | ✅ | `mod.rs:318-418` | 跳表 + HNSW 均持久化 |
| 向量化执行 | 🟡 | `executor/expression.rs:110-155` | API 批量，内部 to_flat + 逐元素循环，**无 SIMD、无类型特化** |
| 执行模型 | 🟡 | `executor/physical_plan.rs`, `executor.rs` | **递归全物化**，非 Volcano pull/push pipeline，每算子行↔列转置 |
| 多核并行 | ❌ | — | 无 rayon/crossbeam，单线程 |
| TopN 优化 | ❌ | `executor.rs:247-256` | Limit 在 Sort 之上 take，无提前终止 |
| 外排 | ❌ | `sort.rs:3-4` | 全内存 |
| CBO | 🟡 | `sql/optimizer.rs:28-30` | 代价模型完整，但**永远传空统计**；ANALYZE 不持久化 |
| 物化视图 | 🟡 | `sql/materialized_view.rs:134-146` | 元数据全，**查询重写永远返回 None**，executor 不物化 |
| UDF | 🟡 | `sql/udf.rs:143-225` | Registry 全，**未接入表达式执行器** |
| Arrow 集成 | 🟡 | `sql/arrow_integration.rs:383-426` | 自研抽象，writer 返回描述字符串，reader 返空，**未接 arrow-rs** |

**关键失实**：原报告"压缩 ⭐⭐⭐⭐""PREWHERE 8.85x"——压缩运行时**完全无效**，PREWHERE 是空壳。benchmarks 里的 8.85x 是**纯算法微基准**，不反映端到端真实收益。

---

## 原报告主要失实之处（重点修正）

1. **事务能力虚高最严重**：原 ⭐⭐⭐⭐ → 校准为**组件完整但系统未接通**，SQL 路径不写 WAL/不走 MVCC/崩溃不恢复。实际系统级事务能力接近 ⭐⭐。
2. **压缩"已实现"≠生效**：7 算法 + 自动选择代码质量高，但 `compress_all` 从未被调用，compact 后仍裸存，持久化反向 decompress。运行时压缩率实际为 **1.0x**。
3. **PREWHERE 是空壳**：table_scan 的 pushdown 版回退全表扫描，MinMax 统计虽维护但消费侧缺失。
4. **JOIN 是死代码**：`hash_join.rs`/`join_order.rs` 存在但 planner 不解析 JOIN 语法，永不产生。
5. **单线程独占**：非"待加并行"，而是 `&mut self` 独占，无任何并发。
6. **两文件非单文件**：且 checkpoint 追加导致无限膨胀，无 VACUUM。
7. **一堆"实现未接线"**：sparse_index / bitmap / bloom / buffer_pool / 物化视图重写 / UDF / Arrow —— 都带单测但从未进主链路。

## 修正后的完成度与优先级

原报告估"~40% 完成"。校准后：**剔除接线断点，实际可用能力约 25–30%**——单表 CRUD + 聚合 + 跳表索引 + 列存 + HNSW（Rust API）+ WAL 写入器本体是真正可用的，其余多为"组件就绪未拼装"。

**真正最该先做的（按"接线收益"排序，而非"新功能"）**：

| 优先级 | 工作 | 理由 |
|---|---|---|
| **P0** | 接通压缩到 compact + 持久化 | 代码已写好，接线即生效，存储立省 |
| **P0** | 接通事务到 SQL 写入路径 + 启动恢复 | 当前 ACID 系统级失效，是可信度底线 |
| **P0** | SQL 层接入向量搜索 + 混合查询 | HNSW 底子已就绪，Agent 检索刚需 |
| P1 | 接通 PREWHERE/MinMax 到 table_scan | 占位代码已写，补消费侧即 8x 级收益 |
| P1 | 接通 sparse/bitmap/bloom 到优化器 | 决定保留还是删除，避免死代码堆积 |
| P1 | JOIN 语法接入 planner（复活 hash_join） | Agent 实体关系刚需，算子已实现 |
| P2 | 多线程（RwLock 改造）/ TopN / SIMD | 纯性能，可后置 |

---

## 附：审计覆盖的子系统

| 子系统 | 关键文件 | 审计结论 |
|---|---|---|
| 存储/压缩/索引 | `storage/{column_store,delta_store,table,sparse_index,buffer_pool,file_format,catalog}.rs`, `compression/*`, `index/*` | 主链路可工作；压缩/sparse/bitmap/bloom/buffer_pool 未接线 |
| 向量检索 | `storage/vector_index.rs`, `executor/vector.rs` | HNSW 内核 production-ready；缺 SQL 接入与混合查询 |
| SQL | `sql/{parser,planner,optimizer,cost_model,statistics,join_order,materialized_view,udf,fast_insert,arrow_integration,ast}.rs` | 单表 CRUD + RBO 可用；多表/高级特性多为占位 |
| 执行器 | `executor/{executor,physical_plan,expression,table_scan,filter,projection,aggregate,sort,hash_join,insert,vector}.rs` | 算子语义齐全；递归全物化、无 SIMD/并行、PREWHERE 空壳 |
| 事务/WAL/类型/并发 | `txn/*`, `wal/*`, `common/{types,error,config,memory_pool}.rs`, `lib.rs` | 组件完整但与 SQL/存储脱节；单线程独占 |

---

## 附录：P0「接通压缩到 compact」实施记录（2026-08-03）

### 改动清单

| 文件 | 改动 | 说明 |
|---|---|---|
| `common/config.rs` | 新增 `compress_on_persist: bool` 字段（默认 `true`） | 压缩接线总开关 |
| `storage/column_store.rs` | 新增 `ensure_rg_decompressed`；`data_to_bytes(compress)` 按开关压缩；`data_from_bytes` 惰性加载压缩态；`read_column` 解压后清空 `compressed_data` | 压缩↔持久化往返核心 |
| `storage/mod.rs` | `checkpoint` 在 compact 后调用 `compress_all`；`save_data` 传 `compress` 开关 | 接通压缩到主链路 |
| `storage/compression/mod.rs` | `decompress` 签名加 `data_type` 参数；修复 Dictionary/Delta/ForBitPack 解压 | 修复 3 个解压 bug |
| `storage/compression/rle.rs` | encode 转义 `0xFF` 首字节单次块；decode `<=`→`<` 防 short tail 误判 | 修复 RLE 标记碰撞 |

### 修复的 Bug

1. **Dictionary 解压空实现**：原 `decompress` 对 `CompressionType::Dictionary` 直接返回 `data.to_vec()`（序列化的字典字节），`deserialize_values` 无法消费。→ 新增 `decompress_dictionary`，反向 `serialize_dictionary` 重建 Varchar 列 `[len][bytes]...` 格式。

2. **Int32 Delta/ForBitPack 宽度混淆**：`encode_i32` 内部转 i64 编码，`decompress_delta` 统一返回 8 字节/值，但 `deserialize_values` 对 Int32 按 4 字节步长读取 → 数据错位。→ `decompress` 新增 `data_type` 参数，Int32 列按 4 字节输出。ForBitPack 同理（min_val 宽度 + 输出宽度）。

3. **RLE `0xFF` 标记碰撞**：非重复 8 字节块首字节为 `0xFF` 时，解码器误判为 RLE 段。→ encode 对此类块用 `count=1` 的 RLE 段转义；decode `i+5 < len`（而非 `<=`）确保 value 部分至少 1 字节。

4. **`read_column` 解压后未清空 `compressed_data`**：导致后续 `append` / `data_to_bytes` 使用陈旧压缩数据。→ 解压后 `clear()` + `shrink_to_fit()` + 重置 `compression = Uncompressed`。

5. **未压缩列 `uncompressed_count` 恒为 0**：`data_to_bytes` 对未压缩列写 `uncompressed_count = 0`，导致 `data_from_bytes` 反序列化 0 行。→ 始终写列真实行数。

### 数据流（压缩开启时）

```
checkpoint()
  ├── compact_all()          # Delta → 列存（append_columns → ensure_rg_decompressed）
  ├── compress_all()         # 列存按 RowGroup 压缩（values 清空，compressed_data 填充）
  ├── save_catalog()         # schema 落盘
  ├── save_data()            # data_to_bytes(true) → 压缩字节直接落盘
  └── save_indexes()         # 二级索引落盘

open_existing()
  ├── load_catalog()         # 恢复 schema
  ├── load_data()            # data_from_bytes → 压缩态惰性加载（values=[], compressed_data=[...])
  └── load_indexes()         # 恢复索引

read_column(rg, col)         # 首次访问 → decompress → 填充 values → 清空 compressed_data
append_rows/append_columns   # ensure_rg_decompressed → 解压旧数据 → 追加新数据
```

### 测试

| 测试 | 文件 | 验证点 |
|---|---|---|
| `test_compression_persistence_roundtrip` | `storage/mod.rs` | 4 类型列压缩落盘 → 重启 → scan 惰性解压 → 数据一致 |
| `test_append_after_compressed_load` | `storage/mod.rs` | 压缩态加载 → 追加 50 行 → compact → 250 行全正确 |
| `test_compression_disabled_persist` | `storage/mod.rs` | `compress_on_persist=false` → 裸存往返 |
| `test_varchar_dictionary_compress` | `compression/mod.rs` | Dictionary 压缩 → 解压 roundtrip（原缺） |
| `test_int32_delta_compress` | `compression/mod.rs` | Int32 Delta → 解压 4B/值（原 8B 截断 workaround） |
| `test_int32_for_bitpack_compress` | `compression/mod.rs` | Int32 ForBitPack roundtrip（新增） |
| `test_rle_value_starting_with_0xff` | `compression/rle.rs` | `0xFF` 首字节单次块 roundtrip（新增） |
| `test_rle_mixed_with_0xff_prefixes` | `compression/rle.rs` | 混合场景：重复 + 0xFF + 普通（新增） |

---

*本报告由代码审计生成，作为 v0.12.x 后续迭代的基线。P0「接通压缩到 compact + 持久化」已完成实施，待 `cargo test` 验证。*
