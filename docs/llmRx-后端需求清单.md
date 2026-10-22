# llmRx 后端需求清单

> 用途：量任何候选存储后端是否能接进 llmRx。每条钉在 llmRx 代码上。
> 结论先行：llmRx 要的不是一个「更快的数据库」，而是一个能假装自己是 SQLite 的数据库——外加能假装自己是列式分析引擎的能力。

## 最小可用后端画像

能接进 llmRx，最少做到 6 件事：

1. 完整 `database/sql/driver`——含真事务 + 多连接并发
2. `?` 占位符 + `LastInsertId` **或** `RETURNING id`（二选一）
3. SQLite 方言 DDL 与约束语义（20 表 / 17 自增 / 21 UNIQUE / 150 NOT NULL DEFAULT）
4. 聚合 + GROUP BY + ORDER BY + LIMIT/OFFSET
5. 单二进制 + 单文件 + 崩溃安全
6. （仅分析层）列式批量导入 + 按天分片 + 跨片扇出 + 压缩

---

## A 驱动契约（9 条）

硬性最集中的一组。

| # | 需求 | 逼出它的代码 |
|---|------|-------------|
| A1 | Go `database/sql/driver` 完整实现 | `sql.Open` 全路径 |
| A2 | `?` 占位符 + query/exec/rows 接口 | 全库 SQL |
| A3 | `LastInsertId` **或** `RETURNING id` | ⚠️ `redeem_sqlite.go:146,161,188` 3 处绕过 dialect 直接调 `res.LastInsertId()`（核销=钱的路径） |
| A4 | 真事务，Rollback 真的撤销 | 9 处 `db.Begin/BeginTx`：redeem ×2、model_costs、sqlite ×3、logstore ×2、ratelimit |
| A5 | 多连接并发 ≥16 | `sqlite.go:93` `SetMaxOpenConns(16)`；`postgres.go:43` →50 |
| A6 | `*sql.DB` 可外泄（RawDB） | `server.go:366` 响应缓存与主库共享同一个 `*sql.DB`（每 LLM 请求热路径） |
| A7 | 驱动级 WAL / busy_timeout 等 pragma | `sqlite.go:74` DSN-level pragmas |
| A8 | 驱动可注册多个 DSN（主库 + 分析库） | store / logstore 分离 |
| A9 | 驱动错误可映射到 `database/sql` 错误语义 | `sql.ErrNoRows`、约束冲突 |

## B SQL 语义（12 条）

EngramDB 满足 9/12。缺：NOT NULL DEFAULT 是否真强制、AUTOINCREMENT 拼写（未实测）、部分方言细节。

| # | 需求 | 备注 |
|---|------|------|
| B1 | `CREATE TABLE IF NOT EXISTS` | 主库 18+ 表 |
| B2 | `INTEGER PRIMARY KEY AUTOINCREMENT` | 17 处 |
| B3 | `UNIQUE` 约束真拒绝 | EngramDB ✅ |
| B4 | `NOT NULL DEFAULT` 真强制 | 150 处；EngramDB 未实测 |
| B5 | `AUTOINCREMENT` 拼写（非 `AUTO_INCREMENT`） | EngramDB 两者都接受；llmRx 写死 AUTOINCREMENT |
| B6 | `?` 绑定 + 位置参数 | — |
| B7 | 聚合：COUNT/SUM/MIN/MAX/AVG | 分析层 + 运维查询 |
| B8 | `GROUP BY` + `HAVING` | thompson / 成本统计 |
| B9 | `ORDER BY` + `LIMIT/OFFSET` | 分页 |
| B10 | `INSERT ... ON CONFLICT DO UPDATE/NOTHING` | `model_costs_sqlite.go:76,131` |
| B11 | `RETURNING id`（或 A3 的 LastInsertId） | PG 路径 |
| B12 | `ALTER TABLE ADD COLUMN` 幂等 | `addColumnIfMissing` |

## C 完整性（4 条）

| # | 需求 | 逼出它的代码 | EngramDB |
|---|------|-------------|----------|
| C1 | `FOREIGN KEY` 真强制 | 5 处（keys→channels、alert_events→alerts、mcp_tools→mcp_servers、mcp_tool_pricing→mcp_tools 等）`sqlite.go:202,272,362,369` | ❌ 静默失效 |
| C2 | `CHECK` 真强制 | 1 处 `sqlite.go:276` `id INTEGER PRIMARY KEY CHECK (id = 1)` | ❌ 静默失效 |
| C3 | `UNIQUE` 真拒绝 | 21 处 | ✅ |
| C4 | 主键不复用 | AUTOINCREMENT 语义 | 未验 |

## D 部署运维（7 条）

| # | 需求 | 逼出它的代码 | 难度 |
|---|------|-------------|------|
| D1 | 单二进制 + 单文件（或少量文件） | 部署形态 | — |
| D2 | 崩溃安全 / WAL | sqlite.go:74 | — |
| D3 | 主密钥加密（SecretsProvider） | `store.go:88` SecretsProvider → `internal/secrets`；`server.go:270` | 硬 |
| D4 | 多副本共享 + 变更通知 | `main.go:509` `pg_notify('llmrx_reload')` + thompson_state 共享 | 硬 |
| D5 | 按天分片 + 跨片查询 | SQLite `ATTACH` 挂最多 8 个日期文件（`sqlite_driver.go:553`、`driver.go:136` MaxAttachFiles） | 硬——EngramDB 无 ATTACH 等价物 |
| D6 | 保留策略：按天 drop | logstore retention | — |
| D7 | 连接池调参 | SetMaxOpenConns 16/50 | — |

## E 分析层专属（7 条）

| # | 需求 | 备注 | EngramDB |
|---|------|------|----------|
| E1 | 列式批量导入 | `import_columns` 类路径 | ✅ 快 30~50% |
| E2 | 按天分片写入 | 一天一文件/一片 | ❌ |
| E3 | 跨天扇出 + 分页 | QueryAcross / ATTACH union | ❌ |
| E4 | 压缩（列级/段级） | 日志体量 | ✅ |
| E5 | 丢弃语义：队列满丢日志、请求路径绝不阻塞 | `manager.go:26-35,139` DropOnFull=true（默认）；换后端必须保持 | 行为契约 |
| E6 | 严格模式 / 类型约束 | 导入质量 | — |
| E7 | 聚合下推 | 大扫描性能 | 部分 |

## F 差异化加分（3 条）

| # | 需求 | 价值 |
|---|------|------|
| F1 | Vector + HNSW | llmRx 现在完全没有向量存储 → 语义响应缓存 |
| F2 | 全文检索 | 日志/审计查询 |
| F3 | 列存 + 向量同库 | 一次扫描喂分析+检索 |

---

## EngramDB 对照结论

| 组 | 判定 |
|----|------|
| A 驱动契约 | ❌ 主存储硬阻塞：A4 真回滚 / A5 并发 |
| B SQL 语义 | 🟡 9/12；NOT NULL DEFAULT、AUTOINCREMENT 未实测 |
| C 完整性 | ❌ C1 FK + C2 CHECK 静默失效 |
| D 部署运维 | ❌ D3 主密钥 / D4 多副本通知 / D5 ATTACH 等价 |
| E 分析层 | 🟡 E1 导入 ✅ / E2 E3 分片扇出 ❌ / E4 压缩 ✅ |
| F 差异化 | ✅ Vector+HNSW 是唯一独特卖点 |

**一句话**：EngramDB 只会列式分析，不会假装 SQLite。主存储缺的 A4/A5/C1/C2/D5 正好就是上一轮那 4 个主存储硬阻塞——完全自洽。

定位建议：**分析层 / 向量层**，不要当主存储。主存储继续用 SQLite/PG。

---

## Phase 0 验收闸门（7 项，纯 Rust，llmRx 零投入）

1. `AUTOINCREMENT` 建表可执行
2. `NOT NULL DEFAULT` 真强制（违反报错，非静默 NULL）
3. `UNIQUE` 真拒绝（二次插入报错）
4. `ON CONFLICT DO UPDATE / DO NOTHING` 语义一致
5. `RETURNING id` 返回值正确（或明确不支持 → 依赖 A3 LastInsertId 路径）
6. `ALTER TABLE ADD COLUMN` 幂等（重复执行不炸）
7. 明确记录 FK+CHECK 不生效，并定补偿方案（应用层校验 or 路线图）

全过 → 可继续谈分析层对接；不过 → 停，别投 llmRx 主存储。
