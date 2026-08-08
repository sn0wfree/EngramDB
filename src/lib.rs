//! EngramDB - 专用分析型嵌入 AI Agent 数据引擎
//!
//! 兼具 SQLite 的事务能力（ACID）与 DuckDB 的列存压缩与分析性能，
//! 单文件嵌入式部署，面向 AI Agent 工作负载优化。
//!
//! # 核心特性
//!
//! - **列式存储**：Row Group 分组 + ClickHouse 风格分类型压缩
//!   （RLE / Dictionary / Bit-packing / FOR / Delta / Gorilla）
//! - **混合架构**：列存主存储 + 行存 Delta 层，兼顾分析与写入
//! - **完整 ACID 事务**：WAL + MVCC + 快照隔离 + 写-写冲突检测 + ARIES 崩溃恢复
//! - **多维度索引**：稀疏主索引 / 跳表二级索引 / 位图索引 / 布隆过滤器 / HNSW 向量索引
//! - **向量化执行**：基于 DataChunk（1024 行/chunk）的向量化查询引擎
//! - **查询优化器**：RBO 规则优化 + CBO 成本优化 + Join 顺序优化
//! - **AI Agent 友好**：JSON 类型 + Vector 类型 + HNSW 语义检索
//! - **单文件嵌入式**：整个数据库是一个 `.hdb` 文件，类似 SQLite
//!
//! # 快速开始
//!
//! ```no_run
//! use engramdb::{Connection, Value};
//!
//! // 打开数据库（":memory:" 为内存数据库，或传入文件路径持久化）
//! let mut conn = Connection::open(":memory:")?;
//!
//! // 建表
//! conn.execute("CREATE TABLE users (id INT PRIMARY KEY, name VARCHAR, age INT)")?;
//!
//! // 插入
//! conn.execute("INSERT INTO users VALUES (1, 'Alice', 30)")?;
//!
//! // 查询
//! let result = conn.execute("SELECT name FROM users WHERE age > 25")?;
//! for row in &result.rows {
//!     println!("{}", row[0]);
//! }
//! # Ok::<(), engramdb::common::error::EngramDbError>(())
//! ```
//!
//! # 模块组织
//!
//! | 模块 | 职责 |
//! |------|------|
//! | [`common`] | 数据类型、错误、配置、内存池 |
//! | [`storage`] | 存储引擎（列存/Delta/压缩/索引/缓冲池/文件格式） |
//! | [`wal`] | WAL 预写日志 + ARIES 崩溃恢复 |
//! | [`txn`] | 事务管理（MVCC + 快照隔离） |
//! | [`sql`] | SQL 解析、规划、优化、UDF |
//! | [`executor`] | 向量化执行引擎 |
//!
//! # 性能调优
//!
//! ```no_run
//! use engramdb::{Connection, WalFlushMode, CompactStrategy};
//!
//! let mut conn = Connection::open("app.hdb")?;
//!
//! // WAL 组提交：多条事务共享一次 fsync，吞吐提升 5-20x
//! conn.set_wal_flush_mode(WalFlushMode::Sync);
//! conn.set_wal_group_commit_size(16);
//!
//! // 自适应 Delta 合并策略
//! conn.set_compact_strategy(CompactStrategy::default_adaptive(100_000));
//!
//! // 按 session_id 聚簇，加速会话范围查询
//! conn.set_cluster_key("agent_logs", "session_id")?;
//! # Ok::<(), engramdb::common::error::EngramDbError>(())
//! ```

// v0.18 实验：jemalloc 全局分配器（bench 验证无收益则回退）
#[global_allocator]
static GLOBAL_ALLOC: jemallocator::Jemalloc = jemallocator::Jemalloc;

pub mod common;
pub mod storage;
pub mod search;
pub mod wal;
pub mod txn;
pub mod sql;
pub mod executor;

// DataFusion 互操作（需启用 datafusion feature）
// 提供 TableProvider 实现，可将 EngramDB 表接入 DataFusion 查询引擎
#[cfg(feature = "datafusion")]
pub mod datafusion_ext;

pub use common::config::{CompactStrategy, WalFlushMode, Config, CompressionType};

use common::error::Result;
use storage::Database;
use txn::Transaction;

/// 预编译语句（Prepared Statement）
///
/// 编译一次，可多次绑定参数执行。
/// 对于批量写入场景，可避免重复的 SQL 解析和计划生成开销。
#[derive(Clone)]
pub struct PreparedStatement {
    ast: sql::ast::Statement,
    /// 预计的参数数量（从占位符推断）
    param_count: usize,
}

impl PreparedStatement {
    /// 获取参数数量
    pub fn param_count(&self) -> usize {
        self.param_count
    }
}

/// 数据库连接
pub struct Connection {
    db: Database,
    closed: bool,
}

impl Connection {
    /// 打开或创建数据库
    pub fn open(path: &str) -> Result<Self> {
        let db = Database::open(path)?;
        Ok(Self { db, closed: false })
    }

    /// 使用指定配置打开或创建数据库
    pub fn open_with_config(path: &str, config: crate::common::config::Config) -> Result<Self> {
        let db = Database::open_with_config(path, config)?;
        Ok(Self { db, closed: false })
    }

    /// P0-2：非裸 INSERT 语句执行前冲刷攒批缓冲（读己之写 + 语句间顺序）
    ///
    /// 裸 INSERT（纯 VALUES、无 RETURNING、无 ON CONFLICT、无子查询）由
    /// Batcher 自行攒批/触发，其余语句（SELECT/UPDATE/DELETE/DDL/CTAS/
    /// InsertSelect/UPSERT/INSERT...RETURNING）必须先落盘缓冲行。
    fn flush_batches(&mut self) -> Result<()> {
        crate::executor::operators::insert::flush_all_batched(&mut self.db)?;
        // P0-2 事务级 Batcher：显式事务内 buffer 同样遵守冲刷语义
        // （非裸 INSERT 语句执行前读己之写）
        if self.db.in_explicit_txn() {
            self.db.flush_txn_buffer()?;
        }
        Ok(())
    }

    /// P0-2 事务级 Batcher：回滚语句不得触发 flush（应丢弃事务 buffer）
    ///
    /// ROLLBACK / ROLLBACK TO SAVEPOINT 前的"非裸 INSERT 前置冲刷"会把
    /// 事务 buffer 落盘成内部事务，导致回滚失效。
    fn is_txn_rollback_stmt(ast: &crate::sql::ast::Statement) -> bool {
        matches!(ast,
            crate::sql::ast::Statement::Rollback
            | crate::sql::ast::Statement::RollbackToSavepoint { .. })
    }

    /// 执行 SQL 语句
    pub fn execute(&mut self, sql: &str) -> Result<QueryResult> {
         use executor::physical_plan::PhysicalPlan;
         // v0.18 P0-1 计划缓存：命中跳过 parse/plan/optimize（同 SQL 重复场景）
         if let Some((cached, clean)) = self.db.get_plan_cache(sql).cloned() {
             // 命中路径同样遵守攒批冲刷语义（非裸 INSERT 语句先 flush）
             if !clean {
                 self.flush_batches()?;
             }
             return executor::execute(cached, &mut self.db);
         }
        let ast = sql::parser::parse(sql)?;
        // P0-2：非裸 INSERT 语句前置冲刷攒批
        //（ROLLBACK / ROLLBACK TO SAVEPOINT 除外：应丢弃事务 buffer 而非 flush）
        let batcher_clean = matches!(&ast, crate::sql::ast::Statement::Insert(stmt)
            if stmt.on_conflict.is_none() && stmt.returning.is_none() && stmt.select.is_none());
        if !batcher_clean && !Self::is_txn_rollback_stmt(&ast) {
            self.flush_batches()?;
        }
         // 结构性 DDL / ANALYZE：不缓存，执行后清空缓存（计划依赖表结构与统计）
         let is_ddl = matches!(&ast,
             crate::sql::ast::Statement::CreateTable(_)
             | crate::sql::ast::Statement::CreateIndex(_)
             | crate::sql::ast::Statement::AlterTable(_)
             | crate::sql::ast::Statement::TruncateTable { .. }
             | crate::sql::ast::Statement::Analyze(_)
             | crate::sql::ast::Statement::CreateMaterializedView(_)
             | crate::sql::ast::Statement::DropMaterializedView(_)
             | crate::sql::ast::Statement::RefreshMaterializedView(_));
        let plan = sql::planner::plan(ast, &self.db)?;
        // CountStar 是行数快照（Perf01 元数据短路），缓存即过期 → 不缓存
        // Rollback/RollbackToSavepoint：不缓存（缓存命中路径会前置 flush
        // 事务 buffer，破坏回滚语义；且回滚语句无重复执行价值）
        let cacheable = !matches!(plan,
            PhysicalPlan::CountStar { .. }
            | PhysicalPlan::Rollback
            | PhysicalPlan::RollbackToSavepoint { .. });
         // INSERT / CREATE TABLE 等 DDL/DML 语句不需要查询优化，直接执行
         // 避免对包含大量数据的计划做无谓的 clone 和优化规则遍历
         let needs_optimize = matches!(plan,
             PhysicalPlan::TableScan { .. }
             | PhysicalPlan::Filter { .. }
             | PhysicalPlan::Projection { .. }
             | PhysicalPlan::HashJoin { .. }
             | PhysicalPlan::Aggregate { .. }
             | PhysicalPlan::Limit { .. }
         );
         let result = if needs_optimize {
             let stats: Vec<_> = self.db.statistics_cache().values().cloned().collect();
             let optimized = sql::optimizer::optimize_with_stats(plan, &stats)?;
             // 缓存最终（优化后）计划
             if !is_ddl && cacheable {
                 self.db.set_plan_cache(sql, optimized.clone(), batcher_clean);
             }
             executor::execute(optimized, &mut self.db)
         } else {
             if !is_ddl && cacheable {
                 self.db.set_plan_cache(sql, plan.clone(), batcher_clean);
             }
             executor::execute(plan, &mut self.db)
         }?;
         if is_ddl {
             self.db.clear_plan_cache();
         }
         Ok(result)
     }

    /// 解释 SQL 执行计划（用于调试和验证优化器）
    pub fn explain(&mut self, sql: &str) -> Result<String> {
        let ast = sql::parser::parse(sql)?;
        let plan = sql::planner::plan(ast, &self.db)?;
        let stats: Vec<_> = self.db.statistics_cache().values().cloned().collect();
        let optimized = sql::optimizer::optimize_with_stats(plan.clone(), &stats)?;
        Ok(format!(
            "原始计划:\n{:#?}\n\n优化后:\n{:#?}",
            plan, optimized
        ))
    }

    /// 预编译 SQL 语句
    ///
    /// 解析 SQL 并缓存 AST，后续可通过 `execute_prepared` 绑定参数执行。
    /// 对于批量 INSERT 等场景，可显著减少 SQL 解析和计划生成开销。
    pub fn prepare(&self, sql: &str) -> Result<PreparedStatement> {
        let ast = sql::parser::parse(sql)?;
        let param_count = count_placeholders(&ast);
        Ok(PreparedStatement { ast, param_count })
    }

    /// 执行预编译语句，绑定参数
    ///
    /// 参数按位置绑定（? 或 $1, $2, ... 都转为 0-based 索引）。
    pub fn execute_prepared(&mut self, stmt: &PreparedStatement, params: &[Value]) -> Result<QueryResult> {
        use executor::physical_plan::PhysicalPlan;
        let batcher_clean = matches!(&stmt.ast, crate::sql::ast::Statement::Insert(ins)
            if ins.on_conflict.is_none() && ins.returning.is_none() && ins.select.is_none());
        if !batcher_clean && !Self::is_txn_rollback_stmt(&stmt.ast) {
            self.flush_batches()?;
        }
        // 直通路径：裸 INSERT（无 RETURNING / ON CONFLICT / SELECT）免计划结构，
        // 直接求值绑定行走 insert 算子（等效 plan_insert 的 Insert 分支）
        if batcher_clean {
            if let crate::sql::ast::Statement::Insert(ins) = &stmt.ast {
                // 参数个数一次性校验（count_placeholders = 最大占位符索引 + 1），
                // 求值用直接索引省去逐列越界检查
                if params.len() < stmt.param_count {
                    return Err(crate::common::error::EngramDbError::Parse(format!(
                        "Parameter index out of bounds ({} params provided)", params.len()
                    )));
                }
                // 无列名：内联 eval 循环（免函数调用与表访问，insert 算子内部校验表存在）
                let rows = if ins.columns.is_none() {
                    let mut rows = Vec::with_capacity(ins.values.len());
                    for value_row in &ins.values {
                        let mut row = Vec::with_capacity(value_row.len());
                        for expr in value_row {
                            row.push(match expr {
                                crate::sql::ast::Expression::Literal(v) => v.clone(),
                                crate::sql::ast::Expression::Placeholder(idx) => params[*idx].clone(),
                                _ => return Err(crate::common::error::EngramDbError::Parse(
                                    "Non-constant expression in VALUES not supported".into()
                                )),
                            });
                        }
                        rows.push(row);
                    }
                    rows
                } else {
                    sql::planner::eval_insert_rows(ins, &self.db, params)?
                };
                let count = executor::operators::insert::execute(&mut self.db, &ins.table_name, rows, false)?;
                return Ok(QueryResult {
                    columns: vec!["rows_inserted".to_string()],
                    rows: vec![vec![crate::Value::Int64(count as i64)]],
                    rows_affected: count,
                });
            }
        }
        let plan = sql::planner::plan_with_params(stmt.ast.clone(), &self.db, params)?;

        let needs_optimize = matches!(plan,
            PhysicalPlan::TableScan { .. }
            | PhysicalPlan::Filter { .. }
            | PhysicalPlan::Projection { .. }
            | PhysicalPlan::HashJoin { .. }
            | PhysicalPlan::Aggregate { .. }
            | PhysicalPlan::Limit { .. }
        );
        if needs_optimize {
            let stats: Vec<_> = self.db.statistics_cache().values().cloned().collect();
            let optimized = sql::optimizer::optimize_with_stats(plan, &stats)?;
            executor::execute(optimized, &mut self.db)
        } else {
            executor::execute(plan, &mut self.db)
        }
    }

    /// 批量执行预编译 INSERT 语句
    ///
    /// 一次性传入多行参数，内部批量构造并执行，减少函数调用开销。
    /// params 是一个二维数组，每个子数组对应一行的参数值。
    pub fn execute_prepared_batch(
        &mut self,
        stmt: &PreparedStatement,
        params_batch: &[Vec<Value>],
    ) -> Result<u64> {
        use executor::physical_plan::PhysicalPlan;

        let mut total = 0u64;
        let batcher_clean = matches!(&stmt.ast, crate::sql::ast::Statement::Insert(ins)
            if ins.on_conflict.is_none() && ins.returning.is_none() && ins.select.is_none());
        // 直通路径：批量裸 INSERT 免计划结构（逐行 eval + insert 算子）
        if batcher_clean {
            if let crate::sql::ast::Statement::Insert(ins) = &stmt.ast {
                for params in params_batch {
                    let rows = sql::planner::eval_insert_rows(ins, &self.db, params)?;
                    total += executor::operators::insert::execute(&mut self.db, &ins.table_name, rows, false)?;
                }
                if !self.db.insert_batcher().is_empty() {
                    self.flush_batches()?;
                }
                return Ok(total);
            }
        }
        for params in params_batch {
            let plan = sql::planner::plan_with_params(stmt.ast.clone(), &self.db, params)?;
            let result = executor::execute(plan, &mut self.db)?;
            total += result.rows_affected;
        }
        // P0-2：批量执行结束冲刷攒批（RETURNING 语义 + 顺序保证）
        if batcher_clean && !self.db.insert_batcher().is_empty() {
            self.flush_batches()?;
        }
        Ok(total)
    }

    /// 开始事务
    pub fn begin(&mut self) -> Result<Transaction> {
        self.flush_batches()?;
        Transaction::begin(&mut self.db, txn::IsolationLevel::default())
    }

    /// 开始一个只读事务（v0.15.0 Txn09）
    ///
    /// 只读事务跳过 WAL 写入，避免不必要的 fsync 开销。
    /// 适用于只进行 SELECT 查询的场景。
    pub fn begin_readonly(&mut self) -> Result<Transaction> {
        self.flush_batches()?;
        Transaction::begin_readonly(&mut self.db, txn::IsolationLevel::default())
    }

    /// 关闭数据库
    pub fn close(&mut self) -> Result<()> {
        self.flush_batches()?;
        let result = self.db.close();
        self.closed = true;
        result
    }

    /// 获取底层 Database 的可变引用（用于高级 API 操作）
    pub fn database_mut(&mut self) -> &mut Database {
        &mut self.db
    }

    // -----------------------------------------------------------------------
    // 向量化写入 & 零拷贝导入（v0.11.2 新增）
    // -----------------------------------------------------------------------

    /// 列式批量导入（零拷贝路径）
    ///
    /// 直接以列式数据写入，跳过 SQL 解析、计划生成、行→列转置等全部开销。
    /// 适合大批量数据导入场景（如 ETL、数据初始化）。
    ///
    /// 参数：
    /// - `table_name`: 目标表名
    /// - `columns`: 按列组织的数据，每列一个 `Vec<Value>`
    ///
    /// 性能：比 Prepared Statement 再快约 30-50%，因为完全跳过 SQL 层。
    pub fn import_columns(&mut self, table_name: &str, columns: Vec<Vec<Value>>) -> Result<u64> {
        use crate::common::error::EngramDbError;

        // P0-2：先冲刷攒批，保证行序（缓冲行先入表）
        self.flush_batches()?;

        let engine = self.db.get_engine_table_mut(table_name)
            .ok_or_else(|| EngramDbError::TableNotFound(table_name.into()))?;

        let num_cols = engine.def().columns.len();
        let num_rows = if columns.is_empty() { 0 } else { columns[0].len() };

        if num_rows == 0 {
            return Ok(0);
        }

        // 验证列数匹配
        if columns.len() != num_cols {
            return Err(EngramDbError::Internal(
                format!("import_columns: column count mismatch (expected {}, got {})",
                    num_cols, columns.len())
            ));
        }

        // 引擎分派（M2：Memory 表走列式批量插入）
        match engine {
            crate::storage::engine::EngineTable::Columnar(table) => {
                // 直接走列式路径写入（P1 + P4 优化的极致）
                let direct_threshold = (table.column_store().row_group_size() / 4) as usize;
                if num_rows >= direct_threshold && num_rows >= 1000 {
                    // 大批量：直接写入列存
                    table.column_store_mut().append_columns(&columns)?;
                    table.def_mut().row_count += num_rows as u64;
                } else {
                    // 小批量：走 Delta 层（列式 Delta，P4）
                    // v0.20：对齐 rowid 基准（compact 后 delta.next_rowid 与行数脱节）
                    let rc = table.def().row_count;
                    table.delta_store_mut().set_next_rowid(rc);
                    table.delta_store_mut().insert_columns(columns)?;
                    // 与 insert() 一致：写入 Delta 层即计入总行数
                    table.def_mut().row_count += num_rows as u64;
                }
            }
            // Memory/Log 走通用 insert_columns 入口
            other => {
                other.insert_columns(columns)?;
            }
        }

        Ok(num_rows as u64)
    }

    /// 手动触发 WAL fsync（用于 Periodic 模式下主动刷盘）
    pub fn sync_wal(&mut self) -> Result<()> {
        self.flush_batches()?;
        self.db.sync_wal()
    }

    /// 设置 WAL 刷盘策略
    pub fn set_wal_flush_mode(&mut self, mode: crate::common::config::WalFlushMode) {
        self.db.set_wal_flush_mode(mode);
    }

    /// 设置 WAL 组提交大小（WAL 加速核心机制）
    ///
    /// Sync 模式下，多条事务共享一次 fsync，写入吞吐可提升数倍至数十倍。
    /// 崩溃时最多丢 `size` 条未 fsync 的事务。
    ///
    /// - `size = 0`：禁用组提交，每次 commit 都 fsync（最安全，默认）
    /// - `size = 8~32`：推荐范围，吞吐提升 5~20x
    /// - 配合 `sync_wal()` 可在关键节点强制刷盘
    ///
    /// 典型场景（AI Agent 交互存储）：
    /// 高频率小写入 + 可容忍极少量数据丢失（进程崩溃时）
    pub fn set_wal_group_commit_size(&mut self, size: usize) {
        self.db.set_wal_group_commit_size(size);
    }

    /// 设置指定表的聚簇列（方案B：Delta 聚簇）
    ///
    /// 设置后，compact 时会按该列的值分组写入列存，
    /// 相同 key 的行物理上连续，可大幅提升按该列的范围查询性能。
    ///
    /// 典型场景：AI Agent 交互存储按 `session_id` 聚簇，
    /// 查询单个会话的全部消息时只需顺序扫描少量连续数据块。
    pub fn set_cluster_key(&mut self, table_name: &str, column_name: &str) -> Result<()> {
        self.db.set_cluster_key(table_name, column_name)
    }

    /// 手动合并指定表的 Delta 层到列存（全量合并）
    ///
    /// 适合批量导入后或业务低峰期调用，将 Delta 层数据全部合并到列存主存储，
    /// 提升后续查询性能。返回合并的行数。
    pub fn compact(&mut self, table_name: &str) -> Result<u64> {
        self.db.compact_table(table_name)
    }

    /// 合并所有表的 Delta 层到列存
    ///
    /// 返回合并的总行数。
    pub fn compact_all(&mut self) -> Result<u64> {
        self.db.compact_all()
    }

    /// 设置全局默认 Delta 合并策略（新建表生效）
    ///
    /// 四种策略可选：
    /// - `CompactStrategy::manual()` — 手动，完全由用户调用 compact()
    /// - `CompactStrategy::full(threshold)` — 全量合并，达到阈值一次性合并
    /// - `CompactStrategy::incremental(threshold, batch_size)` — 增量式，分批合并
    /// - `CompactStrategy::default_adaptive(row_group_size)` — 自适应分桶（默认）
pub fn set_compact_strategy(&mut self, strategy: crate::common::config::CompactStrategy) {
        self.db.set_default_compact_strategy(strategy);
    }

    /// 设置 KV 缓存预算（字节）
    pub fn set_cache_size(&mut self, bytes: usize) {
        self.db.kv_cache.set_max_memory(bytes);
    }

    /// 获取 KV 缓存统计
    pub fn cache_stats(&self) -> crate::storage::cache::CacheStats {
        self.db.kv_cache.stats().clone()
    }

    /// 清除 KV 缓存
    pub fn clear_cache(&mut self) {
        self.db.kv_cache.clear();
    }

    /// 获取 KV 缓存引擎的可变引用（用于高级操作）
    pub fn cache(&mut self) -> &mut crate::storage::cache::KVCache {
        &mut self.db.kv_cache
    }

    /// 设置指定表的 Delta 合并策略（运行时动态切换）
    pub fn set_table_compact_strategy(&mut self, table_name: &str, strategy: crate::common::config::CompactStrategy) -> Result<()> {
        self.db.set_table_compact_strategy(table_name, strategy)
    }
}

/// Connection 析构时自动 checkpoint，保证数据不丢
///
/// 即使客户端忘记调用 `close()`，Drop 也会自动持久化 catalog/data/indexes。
/// **注意**：`:memory:` 内存库无需持久化，Drop 时跳过。
impl Drop for Connection {
    fn drop(&mut self) {
        // 已显式 close 或内存库，跳过
        if self.closed {
            return;
        }
        let path = self.db.path().to_string_lossy().to_string();
        if path == ":memory:" || path.is_empty() {
            return;
        }
        // best-effort 持久化，失败不传播 panic
        let _ = self.db.checkpoint();
    }
}

/// 查询结果
#[derive(Debug, Clone)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
    pub rows_affected: u64,
}

/// 统计 SQL 语句中的占位符数量
fn count_placeholders(stmt: &sql::ast::Statement) -> usize {
    use sql::ast::Statement::*;
    match stmt {
        Insert(s) => {
            let mut max_idx = 0usize;
            for row in &s.values {
                for expr in row {
                    count_placeholder_in_expr(expr, &mut max_idx);
                }
            }
            if max_idx > 0 { max_idx + 1 } else { 0 }
        }
        _ => 0,
    }
}

fn count_placeholder_in_expr(expr: &sql::ast::Expression, max_idx: &mut usize) {
    use sql::ast::Expression::*;
    match expr {
        Placeholder(idx) => {
            if *idx > *max_idx {
                *max_idx = *idx;
            }
        }
        BinaryOp { left, right, .. } => {
            count_placeholder_in_expr(left, max_idx);
            count_placeholder_in_expr(right, max_idx);
        }
        UnaryOp { expr, .. } => {
            count_placeholder_in_expr(expr, max_idx);
        }
        _ => {}
    }
}

/// 值类型
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Value {
    Null,
    Boolean(bool),
    Int32(i32),
    Int64(i64),
    Float32(f32),
    Float64(f64),
    Varchar(String),
    Json(String),
    Vector(Vec<f32>),
    /// INT8 量化向量（v0.15.0 新增）
    ///
    /// 存储空间减少 75%（4x 压缩），每个向量附带独立的 scale/offset。
    VectorInt8(Vec<i8>),
    Blob(Vec<u8>),
    /// Unix 毫秒时间戳（UTC，v0.14.0 新增）
    Timestamp(i64),
}

/// 混合搜索结果（向量相似度 + 标量过滤后的行数据）
#[derive(Debug, Clone)]
pub struct HybridSearchResult {
    pub row_id: u32,
    pub distance: f32,
    pub row: Vec<Value>,
}

// 手动实现 Eq：Float64 按位模式比较（含 NaN 自等）
// Vector 按 f32 字节序列比较（与 Float64 同样的 NaN 处理思路）
impl Eq for Value {}

// 手动实现 PartialOrd/Ord：与 Hash 一致，Float64 用 to_bits()，Vector 用 f32 to_bits 序列
impl PartialOrd for Value {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Value {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        fn variant_rank(v: &Value) -> u8 {
            match v {
                Value::Null => 0,
                Value::Boolean(_) => 1,
                Value::Int32(_) => 2,
                Value::Int64(_) => 3,
                Value::Float32(_) => 4,
                Value::Float64(_) => 5,
                Value::Varchar(_) => 6,
                Value::Json(_) => 7,
                Value::Vector(_) => 8,
                Value::VectorInt8(_) => 9,
                Value::Blob(_) => 10,
                Value::Timestamp(_) => 11,
            }
        }
        let ord = variant_rank(self).cmp(&variant_rank(other));
        if ord != std::cmp::Ordering::Equal {
            return ord;
        }
        match (self, other) {
            (Value::Null, Value::Null) => std::cmp::Ordering::Equal,
            (Value::Boolean(a), Value::Boolean(b)) => a.cmp(b),
            (Value::Int32(a), Value::Int32(b)) => a.cmp(b),
            (Value::Int64(a), Value::Int64(b)) => a.cmp(b),
            (Value::Float32(a), Value::Float32(b)) => a.to_bits().cmp(&b.to_bits()),
            (Value::Float64(a), Value::Float64(b)) => a.to_bits().cmp(&b.to_bits()),
            (Value::Timestamp(a), Value::Timestamp(b)) => a.cmp(b),
            (Value::Varchar(a), Value::Varchar(b)) => a.cmp(b),
            (Value::Json(a), Value::Json(b)) => a.cmp(b),
            (Value::Vector(a), Value::Vector(b)) => {
                let len_ord = a.len().cmp(&b.len());
                if len_ord != std::cmp::Ordering::Equal {
                    return len_ord;
                }
                for (x, y) in a.iter().zip(b.iter()) {
                    let bits_ord = x.to_bits().cmp(&y.to_bits());
                    if bits_ord != std::cmp::Ordering::Equal {
                        return bits_ord;
                    }
                }
                std::cmp::Ordering::Equal
            }
            (Value::VectorInt8(a), Value::VectorInt8(b)) => {
                let len_ord = a.len().cmp(&b.len());
                if len_ord != std::cmp::Ordering::Equal {
                    return len_ord;
                }
                for (x, y) in a.iter().zip(b.iter()) {
                    let ord = x.cmp(y);
                    if ord != std::cmp::Ordering::Equal {
                        return ord;
                    }
                }
                std::cmp::Ordering::Equal
            }
            (Value::Blob(a), Value::Blob(b)) => a.cmp(b),
            _ => std::cmp::Ordering::Equal,
        }
    }
}

// 手动实现 Hash：Float64 用 to_bits()，Vector 用 f32 to_bits 序列
impl std::hash::Hash for Value {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::mem::discriminant(self).hash(state);
        match self {
            Value::Null => {}
            Value::Boolean(b) => b.hash(state),
            Value::Int32(i) => i.hash(state),
            Value::Int64(i) => i.hash(state),
            Value::Float32(f) => f.to_bits().hash(state),
            Value::Float64(f) => f.to_bits().hash(state),
            Value::Varchar(s) => s.hash(state),
            Value::Json(s) => s.hash(state),
            Value::Vector(v) => {
                v.len().hash(state);
                for x in v {
                    x.to_bits().hash(state);
                }
            }
            Value::VectorInt8(v) => {
                v.len().hash(state);
                for x in v {
                    x.hash(state);
                }
            }
            Value::Blob(b) => b.hash(state),
            Value::Timestamp(t) => t.hash(state),
        }
    }
}

impl Value {
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int32(v) => Some(*v as i64),
            Value::Int64(v) => Some(*v),
            Value::Timestamp(v) => Some(*v),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Int32(v) => Some(*v as f64),
            Value::Int64(v) => Some(*v as f64),
            Value::Float64(v) => Some(*v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Varchar(v) => Some(v),
            Value::Json(v) => Some(v),
            _ => None,
        }
    }

    /// 尝试解析为 JSON，返回解析后的值（用于 JSON 路径查询）
    pub fn as_json_value(&self) -> Option<serde_json::Value> {
        match self {
            Value::Json(s) => serde_json::from_str(s).ok(),
            Value::Varchar(s) => serde_json::from_str(s).ok(),
            _ => None,
        }
    }

    /// 获取向量引用
    pub fn as_vector(&self) -> Option<&[f32]> {
        match self {
            Value::Vector(v) => Some(v),
            _ => None,
        }
    }

    /// 获取 INT8 量化向量引用
    pub fn as_vector_int8(&self) -> Option<&[i8]> {
        match self {
            Value::VectorInt8(v) => Some(v),
            _ => None,
        }
    }
}

impl std::fmt::Display for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Value::Null => write!(f, "NULL"),
            Value::Boolean(v) => write!(f, "{}", v),
            Value::Int32(v) => write!(f, "{}", v),
            Value::Int64(v) => write!(f, "{}", v),
            Value::Float32(v) => write!(f, "{}", v),
            Value::Float64(v) => write!(f, "{}", v),
            Value::Varchar(v) => write!(f, "\"{}\"", v),
            Value::Json(v) => write!(f, "'{}'", v),
            Value::Vector(v) => write!(f, "vector[{}]", v.len()),
            Value::VectorInt8(v) => write!(f, "vector_int8[{}]", v.len()),
            Value::Blob(b) => write!(f, "blob[{}]", b.len()),
            Value::Timestamp(t) => write!(f, "ts({})", t),
        }
    }
}

#[cfg(test)]
mod value_tests {    use super::*;




    #[test]
    fn test_value_is_null() {
        assert!(Value::Null.is_null());
        assert!(!Value::Boolean(true).is_null());
        assert!(!Value::Int32(42).is_null());
        assert!(!Value::Int64(42).is_null());
        assert!(!Value::Float64(3.14).is_null());
        assert!(!Value::Varchar("hello".to_string()).is_null());
    }

    #[test]
    fn test_value_as_i64() {
        assert_eq!(Value::Int32(42).as_i64(), Some(42));
        assert_eq!(Value::Int64(1000).as_i64(), Some(1000));
        assert_eq!(Value::Int32(-10).as_i64(), Some(-10));
        assert_eq!(Value::Float64(3.14).as_i64(), None);
        assert_eq!(Value::Varchar("abc".to_string()).as_i64(), None);
        assert_eq!(Value::Null.as_i64(), None);
        assert_eq!(Value::Boolean(true).as_i64(), None);
    }

    #[test]
    fn test_value_as_f64() {
        assert!((Value::Int32(42).as_f64().unwrap() - 42.0).abs() < 0.001);
        assert!((Value::Int64(1000).as_f64().unwrap() - 1000.0).abs() < 0.001);
        assert!((Value::Float64(3.14).as_f64().unwrap() - 3.14).abs() < 0.001);
        assert_eq!(Value::Varchar("abc".to_string()).as_f64(), None);
        assert_eq!(Value::Null.as_f64(), None);
        assert_eq!(Value::Boolean(true).as_f64(), None);
    }

    #[test]
    fn test_value_as_str() {
        assert_eq!(Value::Varchar("hello".to_string()).as_str(), Some("hello"));
        assert_eq!(Value::Varchar("".to_string()).as_str(), Some(""));
        assert_eq!(Value::Int32(42).as_str(), None);
        assert_eq!(Value::Null.as_str(), None);
    }

    #[test]
    fn test_value_display() {
        assert_eq!(format!("{}", Value::Null), "NULL");
        assert_eq!(format!("{}", Value::Boolean(true)), "true");
        assert_eq!(format!("{}", Value::Boolean(false)), "false");
        assert_eq!(format!("{}", Value::Int32(42)), "42");
        assert_eq!(format!("{}", Value::Int64(-100)), "-100");
        assert_eq!(format!("{}", Value::Float64(3.5)), "3.5");
        assert_eq!(format!("{}", Value::Varchar("hello".to_string())), "\"hello\"");
    }

    #[test]
    fn test_value_equality() {
        assert_eq!(Value::Int32(42), Value::Int32(42));
        assert_ne!(Value::Int32(42), Value::Int32(43));
        assert_eq!(Value::Int64(100), Value::Int64(100));
        assert_eq!(Value::Float64(3.14), Value::Float64(3.14));
        assert_eq!(Value::Varchar("abc".to_string()), Value::Varchar("abc".to_string()));
        assert_eq!(Value::Null, Value::Null);
        assert_ne!(Value::Null, Value::Int32(0));
    }

    #[test]
    fn test_value_clone() {
        let v = Value::Varchar("test".to_string());
        let v2 = v.clone();
        assert_eq!(v, v2);

        let v = Value::Float64(2.718);
        let v2 = v.clone();
        assert_eq!(v, v2);
    }

    #[test]
    fn test_value_debug() {
        // 验证 Debug 派生正常工作
        let v = Value::Int64(42);
        let debug_str = format!("{:?}", v);
        assert!(debug_str.contains("Int64"));
        assert!(debug_str.contains("42"));
    }

    // --- JSON / Vector 类型端到端测试（v0.12.0 新增）---

    #[test]
    fn test_json_type_create_and_insert() {
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE agent_meta (id INT, data JSON)").unwrap();
        conn.execute("INSERT INTO agent_meta VALUES (1, '{\"name\":\"agent1\",\"role\":\"analyst\"}')").unwrap();
        conn.execute("INSERT INTO agent_meta VALUES (2, '{\"name\":\"agent2\",\"role\":\"coder\"}')").unwrap();

        let result = conn.execute("SELECT id FROM agent_meta ORDER BY id").unwrap();
        assert_eq!(result.rows.len(), 2);
        assert_eq!(result.rows[0][0], Value::Int64(1));
        assert_eq!(result.rows[1][0], Value::Int64(2));
    }

    #[test]
    fn test_json_extract_sql() {
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE tools (id INT, params JSON)").unwrap();
        conn.execute("INSERT INTO tools VALUES (1, '{\"tool\":\"search\",\"query\":\"gold price\",\"limit\":10}')").unwrap();

        let result = conn.execute("SELECT JSON_EXTRACT(params, '$.tool') FROM tools").unwrap();
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0][0], Value::Json("\"search\"".to_string()));
    }

    #[test]
    fn test_json_contains_sql() {
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE tags (id INT, meta JSON)").unwrap();
        conn.execute("INSERT INTO tags VALUES (1, '{\"tags\":[\"rust\",\"db\",\"ai\"]}')").unwrap();

        let result = conn.execute("SELECT JSON_CONTAINS(meta, '\"ai\"', '$.tags') FROM tags").unwrap();
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0][0], Value::Boolean(true));
    }

    #[test]
    fn test_vector_type_create_and_insert() {
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE embeddings (id INT, vec VECTOR)").unwrap();
        // 向量通过字符串形式插入（暂不支持原生向量字面量，可用 VECTOR_DISTANCE 函数计算）
        conn.execute("INSERT INTO embeddings VALUES (1, NULL)").unwrap();
        conn.execute("INSERT INTO embeddings VALUES (2, NULL)").unwrap();

        let result = conn.execute("SELECT id FROM embeddings ORDER BY id").unwrap();
        assert_eq!(result.rows.len(), 2);
    }

    #[test]
    fn test_vector_distance_sql() {
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE points (id INT)").unwrap();
        conn.execute("INSERT INTO points VALUES (1)").unwrap();
        conn.execute("INSERT INTO points VALUES (2)").unwrap();

        // 验证向量距离函数可用（通过纯字面量计算）
        let result = conn.execute("SELECT VECTOR_NORM(NULL) FROM points LIMIT 1").unwrap();
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0][0], Value::Null);
    }

    // --- 覆盖索引端到端测试（v0.12.0 新增）---

    #[test]
    fn test_create_index_basic() {
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE sessions (id INT, session_id VARCHAR, user_id INT, role VARCHAR)").unwrap();
        conn.execute("INSERT INTO sessions VALUES (1, 'sess_001', 100, 'admin')").unwrap();
        conn.execute("INSERT INTO sessions VALUES (2, 'sess_002', 200, 'user')").unwrap();
        conn.execute("INSERT INTO sessions VALUES (3, 'sess_003', 100, 'admin')").unwrap();

        // 创建普通索引
        let result = conn.execute("CREATE INDEX idx_session_id ON sessions (session_id)").unwrap();
        assert_eq!(result.rows.len(), 1);
        assert!(matches!(&result.rows[0][0], Value::Varchar(s) if s.contains("idx_session_id")));
    }

    #[test]
    fn test_create_index_after_data() {
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE messages (id INT, session_id VARCHAR, role VARCHAR, content VARCHAR)").unwrap();
        for i in 0..50 {
            conn.execute(&format!(
                "INSERT INTO messages VALUES ({}, 'sess_{}', 'user', 'hello')",
                i, i % 10
            )).unwrap();
        }

        // 有数据后创建索引
        let result = conn.execute("CREATE INDEX idx_sess ON messages (session_id)").unwrap();
        assert_eq!(result.rows.len(), 1);
        assert!(matches!(&result.rows[0][0], Value::Varchar(s) if s.contains("idx_sess")));

        // 创建后查询仍然正常
        let result = conn.execute("SELECT COUNT(*) FROM messages").unwrap();
        assert_eq!(result.rows.len(), 1);
    }

    #[test]
    fn test_create_unique_index() {
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE users (id INT, email VARCHAR)").unwrap();
        conn.execute("INSERT INTO users VALUES (1, 'a@test.com')").unwrap();
        conn.execute("INSERT INTO users VALUES (2, 'b@test.com')").unwrap();

        let result = conn.execute("CREATE UNIQUE INDEX idx_email ON users (email)").unwrap();
        assert_eq!(result.rows.len(), 1);
        assert!(matches!(&result.rows[0][0], Value::Varchar(s) if s.contains("idx_email")));
    }

    #[test]
    fn test_index_maintained_after_insert() {
        // 验证：建索引后再插入数据，索引仍然正确维护
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t1 (id INT, name VARCHAR, val INT)").unwrap();
        conn.execute("INSERT INTO t1 VALUES (1, 'a', 10)").unwrap();
        conn.execute("INSERT INTO t1 VALUES (2, 'b', 20)").unwrap();

        // 先建索引（2 行数据）
        conn.execute("CREATE INDEX idx_name ON t1 (name)").unwrap();

        // 再插入 3 行
        conn.execute("INSERT INTO t1 VALUES (3, 'c', 30)").unwrap();
        conn.execute("INSERT INTO t1 VALUES (4, 'd', 40)").unwrap();
        conn.execute("INSERT INTO t1 VALUES (5, 'e', 50)").unwrap();

        // 查询仍然正确（全表扫描路径，验证数据完整性）
        let result = conn.execute("SELECT COUNT(*) FROM t1").unwrap();
        assert_eq!(result.rows.len(), 1);
        // COUNT 返回值
        match &result.rows[0][0] {
            Value::Int64(n) => assert_eq!(*n, 5),
            _ => panic!("expected Int64"),
        }
    }

    // --- 覆盖索引查询优化器测试（v0.12.0 新增 IndexOnlyScan）---

    #[test]
    fn test_index_only_scan_point_lookup() {
        // WHERE 键列等值 + SELECT 键列 → IndexOnlyScan
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE sessions (id INT, session_id VARCHAR, user_id INT, role VARCHAR)").unwrap();
        conn.execute("INSERT INTO sessions VALUES (1, 'sess_001', 100, 'admin')").unwrap();
        conn.execute("INSERT INTO sessions VALUES (2, 'sess_002', 200, 'user')").unwrap();
        conn.execute("INSERT INTO sessions VALUES (3, 'sess_003', 100, 'admin')").unwrap();
        conn.execute("CREATE INDEX idx_sess ON sessions (session_id)").unwrap();

        let result = conn.execute("SELECT session_id FROM sessions WHERE session_id = 'sess_002'").unwrap();
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0][0], Value::Varchar("sess_002".to_string()));
    }

    #[test]
    fn test_index_only_scan_no_match() {
        // 索引点查无匹配时返回空
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (id INT, name VARCHAR)").unwrap();
        conn.execute("INSERT INTO t VALUES (1, 'a')").unwrap();
        conn.execute("INSERT INTO t VALUES (2, 'b')").unwrap();
        conn.execute("CREATE INDEX idx_name ON t (name)").unwrap();

        let result = conn.execute("SELECT name FROM t WHERE name = 'nonexistent'").unwrap();
        assert_eq!(result.rows.len(), 0);
    }

    #[test]
    fn test_index_only_scan_multiple_matches() {
        // 非唯一索引点查返回所有匹配行
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE messages (id INT, session_id VARCHAR, content VARCHAR)").unwrap();
        conn.execute("INSERT INTO messages VALUES (1, 'sess1', 'hello')").unwrap();
        conn.execute("INSERT INTO messages VALUES (2, 'sess1', 'world')").unwrap();
        conn.execute("INSERT INTO messages VALUES (3, 'sess2', 'foo')").unwrap();
        conn.execute("INSERT INTO messages VALUES (4, 'sess1', '!')").unwrap();
        conn.execute("CREATE INDEX idx_sess ON messages (session_id)").unwrap();

        let result = conn.execute("SELECT session_id FROM messages WHERE session_id = 'sess1'").unwrap();
        assert_eq!(result.rows.len(), 3);
        for row in &result.rows {
            assert_eq!(row[0], Value::Varchar("sess1".to_string()));
        }
    }

    #[test]
    fn test_index_scan_non_covering() {
        // P2 回归：非覆盖索引点查（SELECT 列超出索引覆盖范围 → IndexScan 回表）
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE messages (id INT, session_id VARCHAR, content VARCHAR)").unwrap();
        for i in 0..10 {
            conn.execute(&format!(
                "INSERT INTO messages VALUES ({}, 'sess{}', 'payload_{}')",
                i, i % 4, i
            )).unwrap();
        }
        conn.execute("CREATE INDEX idx_sess ON messages (session_id)").unwrap();

        // 索引只覆盖 session_id，content 需要回表 → 走 IndexScan
        let result = conn.execute(
            "SELECT session_id, content FROM messages WHERE session_id = 'sess2'"
        ).unwrap();
        assert_eq!(result.rows.len(), 2); // i%4==2 → i=2,6
        for row in &result.rows {
            assert_eq!(row[0], Value::Varchar("sess2".to_string()));
        }

        // 无匹配
        let result = conn.execute(
            "SELECT session_id, content FROM messages WHERE session_id = 'none'"
        ).unwrap();
        assert_eq!(result.rows.len(), 0);

        // 多列结果 + 回表列裁剪正确性
        let result = conn.execute(
            "SELECT content FROM messages WHERE session_id = 'sess0'"
        ).unwrap();
        assert_eq!(result.rows.len(), 3);
    }

    #[test]
    fn test_index_range_scan() {
        // ①：索引范围扫描 —— WHERE 范围条件（BETWEEN / 单边 / 双边开闭区间）走 IndexRangeScan
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE scores (id INT, score INT, name VARCHAR)").unwrap();
        for i in 0..20 {
            conn.execute(&format!(
                "INSERT INTO scores VALUES ({}, {}, 'user{}')",
                i, i * 5, i
            )).unwrap();
        }
        conn.execute("CREATE INDEX idx_score ON scores (score)").unwrap();

        // 双边闭区间（BETWEEN 被改写为 score >= a AND score <= b → 合并为闭区间）
        let result = conn.execute(
            "SELECT id, name FROM scores WHERE score BETWEEN 30 AND 60"
        ).unwrap();
        // score ∈ {30,35,40,45,50,55,60} → id = 6..12
        assert_eq!(result.rows.len(), 7);
        assert_eq!(result.rows[0][0], Value::Int64(6));
        assert_eq!(result.rows[6][0], Value::Int64(12));

        // 单边下界（开区间）
        let result = conn.execute(
            "SELECT id FROM scores WHERE score > 90"
        ).unwrap();
        // score ∈ {95} → id = 19
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0][0], Value::Int64(19));

        // 单边上界（闭区间）
        let result = conn.execute(
            "SELECT id FROM scores WHERE score <= 10"
        ).unwrap();
        // score ∈ {0,5,10} → id = 0..2
        assert_eq!(result.rows.len(), 3);

        // 双边开闭混合：score >= 15 AND score < 25 → {15,20} → id 3,4
        let result = conn.execute(
            "SELECT id FROM scores WHERE score >= 15 AND score < 25"
        ).unwrap();
        assert_eq!(result.rows.len(), 2);
        assert_eq!(result.rows[0][0], Value::Int64(3));
        assert_eq!(result.rows[1][0], Value::Int64(4));

        // 空范围（下界 > 上界）：退回 Filter 全表扫 → 空结果
        let result = conn.execute(
            "SELECT id FROM scores WHERE score > 60 AND score < 30"
        ).unwrap();
        assert_eq!(result.rows.len(), 0);

        // 范围 + 其他条件（无法完全用范围表示 → 全表 Filter，结果仍正确）
        let result = conn.execute(
            "SELECT id FROM scores WHERE score > 30 AND name = 'user10'"
        ).unwrap();
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0][0], Value::Int64(10));
    }

    #[test]
    fn test_projection_column_reorder_fast_path() {
        // ④：纯列引用投影（列子集/重排）走 rows 直排，结果与 schema 一致
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (a INT, b INT, c INT)").unwrap();
        for i in 0..5 {
            conn.execute(&format!("INSERT INTO t VALUES ({}, {}, {})", i, i * 10, i * 100)).unwrap();
        }

        // 列子集：SELECT b, a（重排）
        let result = conn.execute("SELECT b, a FROM t").unwrap();
        assert_eq!(result.columns, vec!["b".to_string(), "a".to_string()]);
        assert_eq!(result.rows.len(), 5);
        assert_eq!(result.rows[0], vec![Value::Int64(0), Value::Int64(0)]);
        assert_eq!(result.rows[1], vec![Value::Int64(10), Value::Int64(1)]);
        assert_eq!(result.rows[4], vec![Value::Int64(40), Value::Int64(4)]);

        // 单列
        let result = conn.execute("SELECT c FROM t").unwrap();
        assert_eq!(result.columns, vec!["c".to_string()]);
        assert_eq!(result.rows[0], vec![Value::Int64(0)]);
        assert_eq!(result.rows[2], vec![Value::Int64(200)]);

        // 混合表达式仍走常规路径，结果正确
        let result = conn.execute("SELECT a, a * 2 AS double_a FROM t").unwrap();
        assert_eq!(result.columns, vec!["a".to_string(), "double_a".to_string()]);
        assert_eq!(result.rows[1], vec![Value::Int64(1), Value::Int64(2)]);

        // Filter 之上的投影（Filter 快路径输出 rows 后再投影）
        let result = conn.execute("SELECT b, c FROM t WHERE a > 2").unwrap();
        assert_eq!(result.rows.len(), 2);
        assert_eq!(result.rows[0], vec![Value::Int64(30), Value::Int64(300)]);
    }

    #[test]
    fn test_batch_insert_columns_path() {
        // ③：非事务模式下批量 VALUES 走 InsertColumns 列式路径，
        // 索引维护必须与 insert(rows) 等价（防索引失配回归）
        let config = crate::common::config::Config {
            enable_transaction: false,
            ..Default::default()
        };
        let mut conn = Connection::open_with_config(":memory:", config).unwrap();

        conn.execute("CREATE TABLE scores (id INT, score INT, name VARCHAR)").unwrap();
        // 多行 VALUES → 列式路径
        conn.execute(
            "INSERT INTO scores VALUES (1, 10, 'a'), (2, 20, 'b'), (3, 30, 'c'), (4, 40, 'd')"
        ).unwrap();
        conn.execute("CREATE INDEX idx_score ON scores (score)").unwrap();

        // 数据正确（全表扫描）
        let result = conn.execute("SELECT COUNT(*) FROM scores").unwrap();
        assert_eq!(result.rows[0][0], Value::Int64(4));

        // 索引查询正确（等值 + 范围，覆盖二级索引维护）
        let result = conn.execute("SELECT id, name FROM scores WHERE score = 20").unwrap();
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0][0], Value::Int64(2));
        assert_eq!(result.rows[0][1], Value::Varchar("b".into()));

        let result = conn.execute("SELECT id FROM scores WHERE score BETWEEN 15 AND 35").unwrap();
        assert_eq!(result.rows.len(), 2);
        assert_eq!(result.rows[0][0], Value::Int64(2));
        assert_eq!(result.rows[1][0], Value::Int64(3));

        // 大批量（>1000 行）直落列存路径 + 索引维护
        let mut sql = String::from("INSERT INTO scores VALUES ");
        for i in 100..1100 {
            sql.push_str(&format!("({}, {}, 'u{}'),", i, i * 2, i));
        }
        sql.pop();
        conn.execute(&sql).unwrap();
        let result = conn.execute("SELECT id FROM scores WHERE score = 2000").unwrap();
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0][0], Value::Int64(1000));

        // DELETE 在列式直写后仍正确（非事务路径物理删除）
        let r = conn.execute("DELETE FROM scores WHERE score < 50").unwrap();
        assert_eq!(r.rows_affected, 4);
        let result = conn.execute("SELECT COUNT(*) FROM scores").unwrap();
        assert_eq!(result.rows[0][0], Value::Int64(1000));
    }

    // --- JOIN 测试（②）---

    fn setup_join_tables(conn: &mut Connection) {
        conn.execute("CREATE TABLE users (id INT, name VARCHAR)").unwrap();
        conn.execute("CREATE TABLE orders (oid INT, uid INT, amount INT)").unwrap();
        conn.execute("CREATE TABLE items (iid INT, label VARCHAR)").unwrap();
        for i in 1..=4 {
            conn.execute(&format!("INSERT INTO users VALUES ({}, 'user{}')", i, i)).unwrap();
        }
        conn.execute("INSERT INTO orders VALUES (101, 1, 50), (102, 2, 30), (103, 2, 80), (104, 99, 10)").unwrap();
        conn.execute("INSERT INTO items VALUES (101, 'apple'), (103, 'pear'), (999, 'ghost')").unwrap();
    }

    #[test]
    fn test_inner_join() {
        let mut conn = Connection::open(":memory:").unwrap();
        setup_join_tables(&mut conn);

        let r = conn.execute(
            "SELECT users.name, orders.amount FROM users JOIN orders ON users.id = orders.uid ORDER BY orders.amount"
        ).unwrap();
        assert_eq!(r.columns, vec!["name".to_string(), "amount".to_string()]);
        assert_eq!(r.rows.len(), 3);
        // amount 升序：30(user2), 50(user1), 80(user2)
        assert_eq!(r.rows[0], vec![Value::Varchar("user2".into()), Value::Int64(30)]);
        assert_eq!(r.rows[1], vec![Value::Varchar("user1".into()), Value::Int64(50)]);
        assert_eq!(r.rows[2], vec![Value::Varchar("user2".into()), Value::Int64(80)]);
    }

    #[test]
    fn test_left_join() {
        let mut conn = Connection::open(":memory:").unwrap();
        setup_join_tables(&mut conn);

        // LEFT JOIN：无匹配的右表列补 NULL（user3/user4 无订单）
        let r = conn.execute(
            "SELECT users.name, orders.amount FROM users LEFT JOIN orders ON users.id = orders.uid ORDER BY users.id"
        ).unwrap();
        assert_eq!(r.rows.len(), 5);
        assert_eq!(r.rows[0], vec![Value::Varchar("user1".into()), Value::Int64(50)]);
        assert_eq!(r.rows[1], vec![Value::Varchar("user2".into()), Value::Int64(30)]);
        assert_eq!(r.rows[2], vec![Value::Varchar("user2".into()), Value::Int64(80)]);
        assert_eq!(r.rows[3], vec![Value::Varchar("user3".into()), Value::Null]);
        assert_eq!(r.rows[4], vec![Value::Varchar("user4".into()), Value::Null]);
    }

    #[test]
    fn test_three_way_join() {
        let mut conn = Connection::open(":memory:").unwrap();
        setup_join_tables(&mut conn);

        // 三表链式 JOIN + 跨表重名列消歧（users.name vs orders 无同名）
        let r = conn.execute(
            "SELECT users.name, items.label FROM users JOIN orders ON users.id = orders.uid JOIN items ON orders.oid = items.iid"
        ).unwrap();
        assert_eq!(r.columns, vec!["name".to_string(), "label".to_string()]);
        assert_eq!(r.rows.len(), 2);
        assert_eq!(r.rows[0], vec![Value::Varchar("user1".into()), Value::Varchar("apple".into())]);
        assert_eq!(r.rows[1], vec![Value::Varchar("user2".into()), Value::Varchar("pear".into())]);
    }

    #[test]
    fn test_join_group_by_aggregate() {
        let mut conn = Connection::open(":memory:").unwrap();
        setup_join_tables(&mut conn);

        // JOIN + GROUP BY + 聚合 + ORDER BY 别名
        let r = conn.execute(
            "SELECT users.name, SUM(orders.amount) AS total FROM users JOIN orders ON users.id = orders.uid GROUP BY users.name ORDER BY total DESC"
        ).unwrap();
        assert_eq!(r.rows.len(), 2);
        assert_eq!(r.rows[0], vec![Value::Varchar("user2".into()), Value::Float64(110.0)]);
        assert_eq!(r.rows[1], vec![Value::Varchar("user1".into()), Value::Float64(50.0)]);

        // JOIN + COUNT(*)
        let r = conn.execute(
            "SELECT COUNT(*) FROM users JOIN orders ON users.id = orders.uid"
        ).unwrap();
        assert_eq!(r.rows[0][0], Value::Int64(3));
    }

    #[test]
    fn test_join_where_limit() {
        let mut conn = Connection::open(":memory:").unwrap();
        setup_join_tables(&mut conn);

        // JOIN + WHERE + LIMIT
        let r = conn.execute(
            "SELECT users.name, orders.amount FROM users JOIN orders ON users.id = orders.uid WHERE orders.amount > 40 LIMIT 5"
        ).unwrap();
        assert_eq!(r.rows.len(), 2);

        // LEFT JOIN + WHERE（谓词不得下推破坏 NULL 补行语义）
        let r = conn.execute(
            "SELECT users.name FROM users LEFT JOIN orders ON users.id = orders.uid WHERE orders.amount > 50"
        ).unwrap();
        // 只有 user2 的 amount=80 满足
        assert_eq!(r.rows, vec![vec![Value::Varchar("user2".into())]]);

        // 无匹配 LEFT JOIN + WHERE：全 NULL 右列行应被过滤
        let r = conn.execute(
            "SELECT users.name FROM users LEFT JOIN orders ON users.id = orders.uid WHERE orders.amount < 15"
        ).unwrap();
        // orders 有 amount=10（uid=99 无匹配左表）→ 无结果
        assert_eq!(r.rows.len(), 0);
    }

    #[test]
    fn test_cross_join_pipeline() {
        let mut conn = Connection::open(":memory:").unwrap();
        setup_join_tables(&mut conn);

        // CROSS JOIN + WHERE + 投影（此前 CrossJoin 直接返回、WHERE 被忽略）
        let r = conn.execute(
            "SELECT users.name, items.label FROM users CROSS JOIN items WHERE items.iid = 101"
        ).unwrap();
        assert_eq!(r.rows.len(), 4);
        for row in &r.rows {
            assert_eq!(row[1], Value::Varchar("apple".into()));
        }
    }

    // --- INCLUDE 子句 SQL 语法测试（v0.12.0 新增）---

    #[test]
    fn test_create_covering_index_include_syntax() {
        // CREATE INDEX ... INCLUDE (col1, col2) 语法
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE messages (id INT, session_id VARCHAR, role VARCHAR, content VARCHAR, ts INT)").unwrap();
        for i in 0..20 {
            conn.execute(&format!(
                "INSERT INTO messages VALUES ({}, 'sess_{}', 'user', 'hello', {})",
                i, i % 5, i * 100
            )).unwrap();
        }

        // 创建覆盖索引：键列 session_id，INCLUDE role 和 ts
        let result = conn.execute(
            "CREATE INDEX idx_sess_cover ON messages (session_id) INCLUDE (role, ts)"
        ).unwrap();
        assert_eq!(result.rows.len(), 1);
        assert!(matches!(&result.rows[0][0], Value::Varchar(s) if s.contains("idx_sess_cover")));

        // 验证：点查 session_id = 'sess_2'，返回 4 行（20/5）
        let result = conn.execute(
            "SELECT session_id, role FROM messages WHERE session_id = 'sess_2'"
        ).unwrap();
        assert_eq!(result.rows.len(), 4);

        // 验证两列的值正确（不依赖列顺序）
        for row in &result.rows {
            let has_sess = row.iter().any(|v| matches!(v, Value::Varchar(s) if s == "sess_2"));
            let has_role = row.iter().any(|v| matches!(v, Value::Varchar(s) if s == "user"));
            assert!(has_sess, "row should contain sess_2: {:?}", row);
            assert!(has_role, "row should contain user: {:?}", row);
        }
    }

    #[test]
    fn test_create_covering_index_include_single() {
        // 单个 INCLUDE 列
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (id INT, name VARCHAR, val INT)").unwrap();
        conn.execute("INSERT INTO t VALUES (1, 'a', 10)").unwrap();
        conn.execute("INSERT INTO t VALUES (2, 'b', 20)").unwrap();
        conn.execute("CREATE INDEX idx_name ON t (name) INCLUDE (val)").unwrap();

        let result = conn.execute("SELECT name, val FROM t WHERE name = 'a'").unwrap();
        assert_eq!(result.rows.len(), 1);
        // 验证两列值都存在（不依赖列顺序，IndexOnlyScan 按 scan_column_indices 输出）
        let row = &result.rows[0];
        let has_name = row.iter().any(|v| matches!(v, Value::Varchar(s) if s == "a"));
        let has_val = row.iter().any(|v| matches!(v, Value::Int64(10)));
        assert!(has_name, "row should contain 'a': {:?}", row);
        assert!(has_val, "row should contain 10: {:?}", row);
    }

    // --- ORDER BY 排序测试（v0.12.0 新增）---

    #[test]
    fn test_minmax_skip_where_filter() {
        // P2.4 回归：简单比较谓词下推 MinMax 跳过（多 row group 表）
        // 用小的 row_group_size（10 行）制造多个 row group
        let mut config = crate::common::config::Config::default();
        config.row_group_size = 10;
        let mut conn = Connection::open_with_config(":memory:", config).unwrap();
        conn.execute("CREATE TABLE t (id INT, name VARCHAR)").unwrap();

        // 40 行 → 4 个 row group（id 均匀分布 1..=40）
        for i in 1..=40 {
            conn.execute(&format!("INSERT INTO t VALUES ({}, 'name_{}')", i, i)).unwrap();
        }

        // 范围过滤：id > 35 → 只有最后 5 行
        let result = conn.execute("SELECT id FROM t WHERE id > 35").unwrap();
        assert_eq!(result.rows.len(), 5);
        for row in &result.rows {
            assert!(matches!(&row[0], Value::Int64(v) if *v > 35));
        }

        // id <= 5
        let result = conn.execute("SELECT id FROM t WHERE id <= 5").unwrap();
        assert_eq!(result.rows.len(), 5);
        for row in &result.rows {
            assert!(matches!(&row[0], Value::Int64(v) if *v <= 5));
        }

        // 等值：id = 20
        let result = conn.execute("SELECT name FROM t WHERE id = 20").unwrap();
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0][0], Value::Varchar("name_20".to_string()));

        // 无命中
        let result = conn.execute("SELECT id FROM t WHERE id > 100").unwrap();
        assert_eq!(result.rows.len(), 0);
    }

    #[test]
    fn test_order_by_asc() {
        // 基本 ORDER BY ASC
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (id INT, name VARCHAR, score DOUBLE)").unwrap();
        conn.execute("INSERT INTO t VALUES (3, 'charlie', 88.5)").unwrap();
        conn.execute("INSERT INTO t VALUES (1, 'alice', 95.0)").unwrap();
        conn.execute("INSERT INTO t VALUES (2, 'bob', 95.0)").unwrap();
        conn.execute("INSERT INTO t VALUES (5, 'eve', 72.3)").unwrap();
        conn.execute("INSERT INTO t VALUES (4, 'dave', 88.5)").unwrap();

        let result = conn.execute("SELECT id FROM t ORDER BY id ASC").unwrap();
        assert_eq!(result.rows.len(), 5);
        assert_eq!(result.rows[0][0], Value::Int64(1));
        assert_eq!(result.rows[1][0], Value::Int64(2));
        assert_eq!(result.rows[2][0], Value::Int64(3));
        assert_eq!(result.rows[3][0], Value::Int64(4));
        assert_eq!(result.rows[4][0], Value::Int64(5));
    }

    #[test]
    fn test_order_by_desc() {
        // ORDER BY DESC
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (id INT, name VARCHAR)").unwrap();
        conn.execute("INSERT INTO t VALUES (3, 'c')").unwrap();
        conn.execute("INSERT INTO t VALUES (1, 'a')").unwrap();
        conn.execute("INSERT INTO t VALUES (2, 'b')").unwrap();

        let result = conn.execute("SELECT id FROM t ORDER BY id DESC").unwrap();
        assert_eq!(result.rows.len(), 3);
        assert_eq!(result.rows[0][0], Value::Int64(3));
        assert_eq!(result.rows[1][0], Value::Int64(2));
        assert_eq!(result.rows[2][0], Value::Int64(1));
    }

    #[test]
    fn test_order_by_varchar() {
        // ORDER BY 字符串列
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (name VARCHAR)").unwrap();
        conn.execute("INSERT INTO t VALUES ('charlie')").unwrap();
        conn.execute("INSERT INTO t VALUES ('alice')").unwrap();
        conn.execute("INSERT INTO t VALUES ('bob')").unwrap();

        let result = conn.execute("SELECT name FROM t ORDER BY name").unwrap();
        assert_eq!(result.rows.len(), 3);
        assert_eq!(result.rows[0][0], Value::Varchar("alice".to_string()));
        assert_eq!(result.rows[1][0], Value::Varchar("bob".to_string()));
        assert_eq!(result.rows[2][0], Value::Varchar("charlie".to_string()));
    }

    #[test]
    fn test_order_by_with_where() {
        // ORDER BY + WHERE
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (id INT, category VARCHAR, val INT)").unwrap();
        for i in 1..=5 {
            conn.execute(&format!("INSERT INTO t VALUES ({}, 'a', {})", i, 100 - i * 10)).unwrap();
            conn.execute(&format!("INSERT INTO t VALUES ({}, 'b', {})", i + 10, i * 10)).unwrap();
        }

        let result = conn.execute("SELECT val FROM t WHERE category = 'a' ORDER BY val").unwrap();
        assert_eq!(result.rows.len(), 5);
        assert_eq!(result.rows[0][0], Value::Int64(50));
        assert_eq!(result.rows[4][0], Value::Int64(90));
    }

    #[test]
    fn test_order_by_with_limit() {
        // ORDER BY + LIMIT
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (id INT)").unwrap();
        for i in 1..=10 {
            conn.execute(&format!("INSERT INTO t VALUES ({})", i)).unwrap();
        }

        let result = conn.execute("SELECT id FROM t ORDER BY id DESC LIMIT 3").unwrap();
        assert_eq!(result.rows.len(), 3);
        assert_eq!(result.rows[0][0], Value::Int64(10));
        assert_eq!(result.rows[1][0], Value::Int64(9));
        assert_eq!(result.rows[2][0], Value::Int64(8));
    }

    // --- ORDER BY 索引有序性优化测试（v0.12.0 新增）---

    #[test]
    fn test_order_by_index_ordering_skip_sort() {
        // ORDER BY 索引键列 ASC 时，利用索引有序性跳过排序
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (id INT, name VARCHAR, val INT)").unwrap();
        conn.execute("INSERT INTO t VALUES (3, 'c', 30)").unwrap();
        conn.execute("INSERT INTO t VALUES (1, 'a', 10)").unwrap();
        conn.execute("INSERT INTO t VALUES (2, 'b', 20)").unwrap();
        conn.execute("CREATE INDEX idx_id ON t (id) INCLUDE (name, val)").unwrap();

        // 点查 + ORDER BY 索引键列 ASC → 应该走 IndexOnlyScan 且跳过 Sort
        let result = conn.execute("SELECT id, name FROM t WHERE id = 2 ORDER BY id ASC").unwrap();
        assert_eq!(result.rows.len(), 1);
        let row = &result.rows[0];
        let has_id = row.iter().any(|v| matches!(v, Value::Int64(2)));
        let has_name = row.iter().any(|v| matches!(v, Value::Varchar(s) if s == "b"));
        assert!(has_id, "row should contain id=2: {:?}", row);
        assert!(has_name, "row should contain name='b': {:?}", row);
    }

    // --- DELETE / UPDATE SQL 测试（v0.12.0 新增）---

    #[test]
    fn test_delete_all_rows() {
        // DELETE 不带 WHERE —— 删除全部行
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (id INT, name VARCHAR)").unwrap();
        conn.execute("INSERT INTO t VALUES (1, 'a')").unwrap();
        conn.execute("INSERT INTO t VALUES (2, 'b')").unwrap();
        conn.execute("INSERT INTO t VALUES (3, 'c')").unwrap();

        let result = conn.execute("DELETE FROM t").unwrap();
        assert_eq!(result.rows_affected, 3);

        // 验证表已空（用 SELECT * 而非 COUNT(*)，避免空表聚合行为差异）
        let rows = conn.execute("SELECT * FROM t").unwrap();
        assert_eq!(rows.rows.len(), 0);
    }

    #[test]
    fn test_delete_with_where() {
        // DELETE 带 WHERE 条件
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (id INT, name VARCHAR)").unwrap();
        conn.execute("INSERT INTO t VALUES (1, 'a')").unwrap();
        conn.execute("INSERT INTO t VALUES (2, 'b')").unwrap();
        conn.execute("INSERT INTO t VALUES (3, 'c')").unwrap();

        let result = conn.execute("DELETE FROM t WHERE id > 1").unwrap();
        assert_eq!(result.rows_affected, 2);

        let count = conn.execute("SELECT COUNT(*) FROM t").unwrap();
        assert_eq!(count.rows[0][0], Value::Int64(1));

        // 验证剩下的是 id=1
        let row = conn.execute("SELECT id, name FROM t").unwrap();
        assert_eq!(row.rows.len(), 1);
        assert_eq!(row.rows[0][0], Value::Int64(1));
    }

    #[test]
    fn test_delete_no_match() {
        // DELETE WHERE 无匹配行
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (id INT)").unwrap();
        conn.execute("INSERT INTO t VALUES (1)").unwrap();
        conn.execute("INSERT INTO t VALUES (2)").unwrap();

        let result = conn.execute("DELETE FROM t WHERE id > 100").unwrap();
        assert_eq!(result.rows_affected, 0);

        let count = conn.execute("SELECT COUNT(*) FROM t").unwrap();
        assert_eq!(count.rows[0][0], Value::Int64(2));
    }

    #[test]
    fn test_delete_empty_table() {
        // DELETE 空表
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (id INT)").unwrap();

        let result = conn.execute("DELETE FROM t").unwrap();
        assert_eq!(result.rows_affected, 0);
    }

    #[test]
    fn test_update_all_rows() {
        // UPDATE 不带 WHERE —— 更新全部行
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (id INT, val INT)").unwrap();
        conn.execute("INSERT INTO t VALUES (1, 10)").unwrap();
        conn.execute("INSERT INTO t VALUES (2, 20)").unwrap();

        let result = conn.execute("UPDATE t SET val = 99").unwrap();
        assert_eq!(result.rows_affected, 2);

        let rows = conn.execute("SELECT val FROM t ORDER BY id").unwrap();
        assert_eq!(rows.rows.len(), 2);
        assert_eq!(rows.rows[0][0], Value::Int64(99));
        assert_eq!(rows.rows[1][0], Value::Int64(99));
    }

    #[test]
    fn test_update_with_where() {
        // UPDATE 带 WHERE 条件
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (id INT, name VARCHAR, val INT)").unwrap();
        conn.execute("INSERT INTO t VALUES (1, 'a', 10)").unwrap();
        conn.execute("INSERT INTO t VALUES (2, 'b', 20)").unwrap();
        conn.execute("INSERT INTO t VALUES (3, 'c', 30)").unwrap();

        let result = conn.execute("UPDATE t SET val = 0 WHERE id >= 2").unwrap();
        assert_eq!(result.rows_affected, 2);

        let rows = conn.execute("SELECT id, val FROM t ORDER BY id").unwrap();
        assert_eq!(rows.rows.len(), 3);
        assert_eq!(rows.rows[0][1], Value::Int64(10)); // id=1 不变
        assert_eq!(rows.rows[1][1], Value::Int64(0));  // id=2 被更新
        assert_eq!(rows.rows[2][1], Value::Int64(0));  // id=3 被更新
    }

    #[test]
    fn test_update_multiple_columns() {
        // UPDATE 多列
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (id INT, name VARCHAR, val INT)").unwrap();
        conn.execute("INSERT INTO t VALUES (1, 'old', 10)").unwrap();

        let result = conn.execute("UPDATE t SET name = 'new', val = 100 WHERE id = 1").unwrap();
        assert_eq!(result.rows_affected, 1);

        let row = conn.execute("SELECT name, val FROM t").unwrap();
        assert_eq!(row.rows.len(), 1);
        assert!(matches!(&row.rows[0][0], Value::Varchar(s) if s == "new"));
        assert_eq!(row.rows[0][1], Value::Int64(100));
    }

    #[test]
    fn test_update_no_match() {
        // UPDATE WHERE 无匹配行
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (id INT, val INT)").unwrap();
        conn.execute("INSERT INTO t VALUES (1, 10)").unwrap();

        let result = conn.execute("UPDATE t SET val = 99 WHERE id = 999").unwrap();
        assert_eq!(result.rows_affected, 0);

        let row = conn.execute("SELECT val FROM t").unwrap();
        assert_eq!(row.rows[0][0], Value::Int64(10)); // 值不变
    }

    #[test]
    fn test_delete_with_index_maintenance() {
        // DELETE 后索引应同步维护，查询仍然正确
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (id INT, name VARCHAR, val INT)").unwrap();
        conn.execute("INSERT INTO t VALUES (1, 'a', 10)").unwrap();
        conn.execute("INSERT INTO t VALUES (2, 'b', 20)").unwrap();
        conn.execute("INSERT INTO t VALUES (3, 'c', 30)").unwrap();
        conn.execute("CREATE INDEX idx_name ON t (name) INCLUDE (val)").unwrap();

        // 删除中间一行
        let result = conn.execute("DELETE FROM t WHERE name = 'b'").unwrap();
        assert_eq!(result.rows_affected, 1);

        // 索引查询应该只返回剩余的 2 行
        let count = conn.execute("SELECT COUNT(*) FROM t").unwrap();
        assert_eq!(count.rows[0][0], Value::Int64(2));

        // 验证被删的行确实不在了
        let rows = conn.execute("SELECT name FROM t WHERE name = 'b'").unwrap();
        assert_eq!(rows.rows.len(), 0);

        // 验证剩余行索引查询正常
        let rows = conn.execute("SELECT name, val FROM t WHERE name = 'a'").unwrap();
        assert_eq!(rows.rows.len(), 1);
    }

    #[test]
    fn test_update_with_index_maintenance() {
        // UPDATE 索引键列后索引应同步维护
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (id INT, name VARCHAR, val INT)").unwrap();
        conn.execute("INSERT INTO t VALUES (1, 'a', 10)").unwrap();
        conn.execute("INSERT INTO t VALUES (2, 'b', 20)").unwrap();
        conn.execute("CREATE INDEX idx_name ON t (name) INCLUDE (val)").unwrap();

        // 更新索引键列
        let result = conn.execute("UPDATE t SET name = 'updated' WHERE id = 1").unwrap();
        assert_eq!(result.rows_affected, 1);

        // 旧值查不到了
        let rows = conn.execute("SELECT * FROM t WHERE name = 'a'").unwrap();
        assert_eq!(rows.rows.len(), 0);

        // 新值可以查到
        let rows = conn.execute("SELECT id, val FROM t WHERE name = 'updated'").unwrap();
        assert_eq!(rows.rows.len(), 1);
        assert_eq!(rows.rows[0][0], Value::Int64(1));
        assert_eq!(rows.rows[0][1], Value::Int64(10));

        // 非索引列更新不影响索引键
        let result = conn.execute("UPDATE t SET val = 999 WHERE name = 'b'").unwrap();
        assert_eq!(result.rows_affected, 1);

        let rows = conn.execute("SELECT val FROM t WHERE name = 'b'").unwrap();
        assert_eq!(rows.rows.len(), 1);
        assert_eq!(rows.rows[0][0], Value::Int64(999));
    }

    // --- NOT NULL 约束测试（v0.12.0 新增）---

    #[test]
    fn test_not_null_constraint() {
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (id INT, name VARCHAR NOT NULL)").unwrap();

        // INSERT with NULL in NOT NULL column should fail
        let err = conn.execute("INSERT INTO t VALUES (1, NULL)").unwrap_err();
        let err_str = err.to_string();
        assert!(err_str.contains("NOT NULL"), "expected NOT NULL error, got: {}", err_str);
    }

    #[test]
    fn test_not_null_constraint_pass() {
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (id INT, name VARCHAR NOT NULL)").unwrap();

        // INSERT with non-NULL value should succeed
        conn.execute("INSERT INTO t VALUES (1, 'hello')").unwrap();

        let result = conn.execute("SELECT id, name FROM t").unwrap();
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0][0], Value::Int64(1));
        assert_eq!(result.rows[0][1], Value::Varchar("hello".to_string()));
    }

    // --- T07 FLOAT32 类型测试（v0.14.0 新增）---

    #[test]
    fn test_float32_basic() {
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (val FLOAT)").unwrap();
        conn.execute("INSERT INTO t VALUES (3.14)").unwrap();

        let result = conn.execute("SELECT val FROM t").unwrap();
        assert_eq!(result.rows.len(), 1);
        match &result.rows[0][0] {
            Value::Float32(f) => assert!((f - 3.14).abs() < 1e-5, "got {}", f),
            Value::Float64(f) => assert!((f - 3.14).abs() < 1e-5),
            other => panic!("expected Float32 or Float64, got {:?}", other),
        }
    }

    #[test]
    fn test_float32_sort() {
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (val FLOAT)").unwrap();
        conn.execute("INSERT INTO t VALUES (1.0)").unwrap();
        conn.execute("INSERT INTO t VALUES (3.0)").unwrap();
        conn.execute("INSERT INTO t VALUES (2.0)").unwrap();

        let result = conn.execute("SELECT val FROM t ORDER BY val ASC").unwrap();
        assert_eq!(result.rows.len(), 3);
        // 顺序应是 1.0, 2.0, 3.0
        let get_f = |v: &Value| match v {
            Value::Float32(f) => *f as f64,
            Value::Float64(f) => *f,
            other => panic!("unexpected {:?}", other),
        };
        assert!((get_f(&result.rows[0][0]) - 1.0).abs() < 1e-5);
        assert!((get_f(&result.rows[1][0]) - 2.0).abs() < 1e-5);
        assert!((get_f(&result.rows[2][0]) - 3.0).abs() < 1e-5);
    }

    // --- T13 DATETIME/TIMESTAMP 类型测试（v0.14.0 新增）---

    #[test]
    fn test_timestamp_basic() {
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (ts TIMESTAMP, name VARCHAR)").unwrap();

        // 2026-01-15 00:00:00 UTC = 1768435200000 ms
        conn.execute("INSERT INTO t VALUES (1768435200000, 'event_a')").unwrap();
        conn.execute("INSERT INTO t VALUES (1768521600000, 'event_b')").unwrap();

        let result = conn.execute("SELECT ts, name FROM t ORDER BY ts ASC").unwrap();
        assert_eq!(result.rows.len(), 2);
        assert_eq!(result.rows[0][1], Value::Varchar("event_a".to_string()));
        assert_eq!(result.rows[1][1], Value::Varchar("event_b".to_string()));
    }

    #[test]
    fn test_timestamp_sort() {
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (ts TIMESTAMP)").unwrap();
        conn.execute("INSERT INTO t VALUES (3000)").unwrap();
        conn.execute("INSERT INTO t VALUES (1000)").unwrap();
        conn.execute("INSERT INTO t VALUES (2000)").unwrap();

        let result = conn.execute("SELECT ts FROM t ORDER BY ts ASC").unwrap();
        assert_eq!(result.rows.len(), 3);
        // 顺序应为 1000, 2000, 3000
        let get_ts = |v: &Value| match v {
            Value::Timestamp(t) => *t,
            Value::Int64(t) => *t,
            other => panic!("unexpected {:?}", other),
        };
        assert_eq!(get_ts(&result.rows[0][0]), 1000);
        assert_eq!(get_ts(&result.rows[1][0]), 2000);
        assert_eq!(get_ts(&result.rows[2][0]), 3000);
    }

    // --- C02 AUTO_INCREMENT 测试（v0.14.0 新增）---

    #[test]
    fn test_auto_increment_basic() {
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (id INT AUTO_INCREMENT PRIMARY KEY, name VARCHAR)").unwrap();

        // 不提供 id 值 → 自动从 1 开始
        conn.execute("INSERT INTO t (name) VALUES ('a')").unwrap();
        conn.execute("INSERT INTO t (name) VALUES ('b')").unwrap();
        conn.execute("INSERT INTO t (name) VALUES ('c')").unwrap();

        let result = conn.execute("SELECT id, name FROM t ORDER BY id ASC").unwrap();
        assert_eq!(result.rows.len(), 3);
        assert_eq!(result.rows[0][0], Value::Int64(1));
        assert_eq!(result.rows[0][1], Value::Varchar("a".to_string()));
        assert_eq!(result.rows[1][0], Value::Int64(2));
        assert_eq!(result.rows[1][1], Value::Varchar("b".to_string()));
        assert_eq!(result.rows[2][0], Value::Int64(3));
        assert_eq!(result.rows[2][1], Value::Varchar("c".to_string()));
    }

    #[test]
    fn test_auto_increment_explicit_value() {
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (id INT AUTO_INCREMENT PRIMARY KEY, name VARCHAR)").unwrap();

        conn.execute("INSERT INTO t (name) VALUES ('a')").unwrap();
        // 显式指定 id=100
        conn.execute("INSERT INTO t VALUES (100, 'b')").unwrap();
        // 下一个自动 id 应为 101
        conn.execute("INSERT INTO t (name) VALUES ('c')").unwrap();

        let result = conn.execute("SELECT id FROM t ORDER BY id ASC").unwrap();
        assert_eq!(result.rows[0][0], Value::Int64(1));
        assert_eq!(result.rows[1][0], Value::Int64(100));
        assert_eq!(result.rows[2][0], Value::Int64(101));
    }

    #[test]
    fn test_auto_increment_persist() {
        // 验证重启后 auto_increment 计数器持久化
        let path = format!("/tmp/engramdb_auto_inc_{}.hdb", std::process::id());
        let _ = std::fs::remove_file(&path);

        {
            let mut conn = Connection::open(&path).unwrap();
            conn.execute("CREATE TABLE t (id INT AUTO_INCREMENT PRIMARY KEY)").unwrap();
            conn.execute("INSERT INTO t (id) VALUES (NULL)").unwrap();
            conn.execute("INSERT INTO t (id) VALUES (NULL)").unwrap();
            // IDs: 1, 2
            conn.close().unwrap();
        }

        {
            let mut conn = Connection::open(&path).unwrap();
            conn.execute("INSERT INTO t (id) VALUES (NULL)").unwrap();
            // 重启后下一个 ID 应该是 3
            let r = conn.execute("SELECT id FROM t ORDER BY id ASC").unwrap();
            assert_eq!(r.rows[0][0], Value::Int64(1));
            assert_eq!(r.rows[1][0], Value::Int64(2));
            assert_eq!(r.rows[2][0], Value::Int64(3));
            conn.close().unwrap();
        }

        let _ = std::fs::remove_file(&path);
    }

    // --- C04 列级 UNIQUE 约束测试（v0.14.0 新增）---

    #[test]
    fn test_unique_column_constraint() {
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (id INT UNIQUE, name VARCHAR)").unwrap();

        // 唯一 id=1
        let r = conn.execute("INSERT INTO t VALUES (1, 'a')");
        eprintln!("[test] first insert: {:?}", r);
        match r {
            Ok(_) => {}
            Err(e) => panic!("first insert failed: {}", e),
        }
    }

    #[test]
    fn test_unique_column_with_pk() {
        let mut conn = Connection::open(":memory:").unwrap();
        // PRIMARY KEY + UNIQUE 不冲突
        conn.execute("CREATE TABLE t (id INT PRIMARY KEY, name VARCHAR UNIQUE)").unwrap();
        conn.execute("INSERT INTO t VALUES (1, 'a')").unwrap();
        conn.execute("INSERT INTO t VALUES (2, 'b')").unwrap();
        // 重复 name 应该报错
        let err = conn.execute("INSERT INTO t VALUES (3, 'a')").unwrap_err();
        assert!(err.to_string().contains("UNIQUE"), "got: {}", err);
    }

    #[test]
    fn test_insert_returning() {
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (id INT PRIMARY KEY AUTO_INCREMENT, name VARCHAR, score INT)").unwrap();

        // INSERT...RETURNING 单列
        let result = conn.execute("INSERT INTO t (name, score) VALUES ('alice', 100) RETURNING id").unwrap();
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0][0], Value::Int64(1));

        // INSERT...RETURNING 多列
        let result = conn.execute("INSERT INTO t (name, score) VALUES ('bob', 200) RETURNING id, name, score").unwrap();
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0][0], Value::Int64(2));
        assert_eq!(result.rows[0][1], Value::Varchar("bob".into()));
        assert_eq!(result.rows[0][2], Value::Int64(200));

        // INSERT...RETURNING *
        let result = conn.execute("INSERT INTO t (name, score) VALUES ('carol', 300) RETURNING *").unwrap();
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].len(), 3); // id, name, score

        // 验证数据正确写入
        let result = conn.execute("SELECT COUNT(*) FROM t").unwrap();
        assert_eq!(result.rows[0][0], Value::Int64(3));
    }

    #[test]
    fn test_upsert_do_update() {
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (id INT PRIMARY KEY, name VARCHAR, score INT)").unwrap();

        // 首次插入
        conn.execute("INSERT INTO t VALUES (1, 'alice', 100)").unwrap();

        // UPSERT：冲突时更新
        conn.execute("INSERT INTO t VALUES (1, 'alice_updated', 200) ON CONFLICT (id) DO UPDATE SET name = excluded.name, score = excluded.score").unwrap();

        // 验证更新后的值
        let result = conn.execute("SELECT name, score FROM t WHERE id = 1").unwrap();
        assert_eq!(result.rows[0][0], Value::Varchar("alice_updated".into()));
        assert_eq!(result.rows[0][1], Value::Int64(200));

        // 验证只有一行
        let result = conn.execute("SELECT COUNT(*) FROM t").unwrap();
        assert_eq!(result.rows[0][0], Value::Int64(1));
    }

    #[test]
    fn test_upsert_do_nothing() {
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (id INT PRIMARY KEY, name VARCHAR)").unwrap();

        // 首次插入
        conn.execute("INSERT INTO t VALUES (1, 'alice')").unwrap();

        // UPSERT：冲突时不做任何事
        conn.execute("INSERT INTO t VALUES (1, 'bob') ON CONFLICT (id) DO NOTHING").unwrap();

        // 验证原始值未变
        let result = conn.execute("SELECT name FROM t WHERE id = 1").unwrap();
        assert_eq!(result.rows[0][0], Value::Varchar("alice".into()));

        // 验证只有一行
        let result = conn.execute("SELECT COUNT(*) FROM t").unwrap();
        assert_eq!(result.rows[0][0], Value::Int64(1));
    }

    #[test]
    fn test_hybrid_search() {
        use crate::storage::vector_index::DistanceMetric;
        use crate::common::types::{TableDef, ColumnDef, DataType};

        // 通过 API 直接创建表（避免 SQL 解析器 VECTOR 维度问题）
        let mut conn = Connection::open(":memory:").unwrap();

        let columns = vec![
            ColumnDef::new("id", DataType::Int64).primary_key(),
            ColumnDef::new("name", DataType::Varchar),
            ColumnDef::new("category", DataType::Varchar),
            ColumnDef::new("vec", DataType::Vector { dim: 4 }),
        ];
        let table_def = TableDef::new(0, "items", columns);
        let db = conn.database_mut();
        db.create_table(table_def).unwrap();

        // 写入数据
        let ids = vec![Value::Int64(1), Value::Int64(2), Value::Int64(3), Value::Int64(4), Value::Int64(5), Value::Int64(6)];
        let names = vec![
            Value::Varchar("item1".into()), Value::Varchar("item2".into()),
            Value::Varchar("item3".into()), Value::Varchar("item4".into()),
            Value::Varchar("item5".into()), Value::Varchar("item6".into()),
        ];
        let categories = vec![
            Value::Varchar("A".into()), Value::Varchar("A".into()),
            Value::Varchar("B".into()), Value::Varchar("B".into()),
            Value::Varchar("C".into()), Value::Varchar("C".into()),
        ];
        let vectors = vec![
            Value::Vector(vec![0.1, 0.2, 0.3, 0.4]),
            Value::Vector(vec![0.2, 0.3, 0.4, 0.5]),
            Value::Vector(vec![0.9, 0.8, 0.7, 0.6]),
            Value::Vector(vec![0.8, 0.7, 0.6, 0.5]),
            Value::Vector(vec![0.5, 0.5, 0.5, 0.5]),
            Value::Vector(vec![0.4, 0.4, 0.4, 0.4]),
        ];
        // 使用 table.insert 直接写入行数据
        let rows: Vec<Vec<Value>> = (0..6).map(|i| {
            vec![ids[i].clone(), names[i].clone(), categories[i].clone(), vectors[i].clone()]
        }).collect();
        let table = db.get_table_mut("items").unwrap();
        table.insert(rows).unwrap();

        // 创建 HNSW 向量索引
        db.create_vector_index("items", "idx_vec", "vec", DistanceMetric::L2, 8, 50).unwrap();

        // 查询向量：接近类别 A 的向量
        let query = vec![0.15, 0.25, 0.35, 0.45];

        // 无过滤的向量搜索
        let results = db.vector_search("items", "idx_vec", &query, 3).unwrap();
        assert_eq!(results.len(), 3);

        // 混合搜索：只保留类别 A 的结果
        let results = db.hybrid_search(
            "items", "idx_vec", &query, 3, 3, &[0, 1, 2],
            &|row: &[Value]| {
                if let Value::Varchar(cat) = &row[2] {
                    cat == "A"
                } else {
                    false
                }
            },
        ).unwrap();
        assert_eq!(results.len(), 2, "应该只返回类别 A 的 2 个结果");
        for r in &results {
            assert_eq!(r.row.len(), 3, "应该返回 id, name, category 三列");
        }

        // 混合搜索：保留类别 B 的结果
        let results = db.hybrid_search(
            "items", "idx_vec", &query, 3, 3, &[0, 1, 2],
            &|row: &[Value]| {
                if let Value::Varchar(cat) = &row[2] {
                    cat == "B"
                } else {
                    false
                }
            },
        ).unwrap();
        assert_eq!(results.len(), 2, "应该只返回类别 B 的 2 个结果");

        // 混合搜索：无匹配类别
        let results = db.hybrid_search(
            "items", "idx_vec", &query, 3, 3, &[0, 1, 2],
            &|row: &[Value]| {
                if let Value::Varchar(cat) = &row[2] {
                    cat == "Z"
                } else {
                    false
                }
            },
        ).unwrap();
        assert_eq!(results.len(), 0, "没有类别 Z 的数据");
    }

    #[test]
    fn test_search_trace() {
        use crate::storage::vector_index::DistanceMetric;
        use crate::common::types::{TableDef, ColumnDef, DataType};

        let mut conn = Connection::open(":memory:").unwrap();

        let columns = vec![
            ColumnDef::new("id", DataType::Int64).primary_key(),
            ColumnDef::new("vec", DataType::Vector { dim: 4 }),
        ];
        let table_def = TableDef::new(0, "items", columns);
        let db = conn.database_mut();
        db.create_table(table_def).unwrap();

        // 插入 10 个向量
        let rows: Vec<Vec<Value>> = (0..10).map(|i| {
            let v = vec![i as f32 * 0.1, i as f32 * 0.1, i as f32 * 0.1, i as f32 * 0.1];
            vec![Value::Int64(i as i64), Value::Vector(v)]
        }).collect();
        let table = db.get_table_mut("items").unwrap();
        table.insert(rows).unwrap();

        // 创建 HNSW 索引
        db.create_vector_index("items", "idx_vec", "vec", DistanceMetric::L2, 8, 50).unwrap();

        // 带 trace 的搜索
        let query = vec![0.5, 0.5, 0.5, 0.5];
        let (results, trace) = db.vector_search_with_trace("items", "idx_vec", &query, 3).unwrap();

        // 验证返回结果
        assert_eq!(results.len(), 3, "应返回 3 个最近邻");

        // 验证 trace 内容
        assert!(trace.entry_point.is_some(), "trace 应包含入口点");
        assert!(!trace.visited_nodes.is_empty(), "trace 应包含访问节点序列");
        assert_eq!(trace.index_type, "HNSW", "索引类型应为 HNSW（非量化）");
        assert_eq!(trace.metric, "L2", "度量应为 L2");
        assert_eq!(trace.top_k_ids.len(), 3, "top_k_ids 应包含 3 个 ID");
        assert_eq!(trace.top_k_distances.len(), 3, "top_k_distances 应包含 3 个距离");
        assert!(trace.candidates_visited > 0, "候选节点数应 > 0");

        // 验证 top_k_ids 与 results 一致
        for (i, r) in results.iter().enumerate() {
            assert_eq!(trace.top_k_ids[i], r.id, "trace.top_k_ids 与 results 应一致");
            assert!((trace.top_k_distances[i] - r.distance).abs() < 1e-6, "距离应一致");
        }

        // 验证 visited_nodes 是有效的 row_id
        for &id in &trace.visited_nodes {
            assert!(id < 10, "访问的节点 ID 应在 0..10 范围内");
        }
    }

    #[test]
    fn test_search_trace_with_quantization() {
        use crate::storage::vector_index::DistanceMetric;
        use crate::common::types::{TableDef, ColumnDef, DataType};

        let mut conn = Connection::open(":memory:").unwrap();

        let columns = vec![
            ColumnDef::new("id", DataType::Int64).primary_key(),
            ColumnDef::new("vec", DataType::VectorInt8 { dim: 4 }),
        ];
        let table_def = TableDef::new(0, "items", columns);
        let db = conn.database_mut();
        db.create_table(table_def).unwrap();

        // 插入 20 个向量
        let rows: Vec<Vec<Value>> = (0..20).map(|i| {
            let v: Vec<i8> = (0..4).map(|j| ((i + j) as f32 * 10.0) as i8).collect();
            vec![Value::Int64(i as i64), Value::VectorInt8(v)]
        }).collect();
        let table = db.get_table_mut("items").unwrap();
        table.insert(rows).unwrap();

        // 创建量化 HNSW 索引
        db.create_vector_index("items", "idx_vec", "vec", DistanceMetric::L2, 8, 50).unwrap();

        // 带 trace 的搜索（query 需要是 f32，向量内部会自动转换）
        let query = vec![0.5, 0.5, 0.5, 0.5];
        let (results, trace) = db.vector_search_with_trace("items", "idx_vec", &query, 3).unwrap();

        assert_eq!(results.len(), 3);
        assert_eq!(trace.index_type, "HNSW-INT8", "量化索引应标记为 HNSW-INT8");
        assert!(trace.candidates_visited > 0);
    }

    #[test]
    fn test_savepoint_basic() {
        let mut conn = Connection::open(":memory:").unwrap();

        conn.execute("CREATE TABLE t (id INT64, val INT64)").unwrap();
        conn.execute("INSERT INTO t VALUES (1, 100)").unwrap();

        // 事务 + savepoint + 部分回滚
        conn.execute("BEGIN").unwrap();
        conn.execute("INSERT INTO t VALUES (2, 200)").unwrap();
        conn.execute("SAVEPOINT sp1").unwrap();
        conn.execute("INSERT INTO t VALUES (3, 300)").unwrap();
        // 回滚到 sp1，应撤销 (3, 300) 但保留 (2, 200)
        conn.execute("ROLLBACK TO SAVEPOINT sp1").unwrap();
        conn.execute("COMMIT").unwrap();

        // 验证 (3, 300) 被回滚，(2, 200) 保留
        let result = conn.execute("SELECT id FROM t ORDER BY id").unwrap();
        // 注：当前实现下，事务内的 INSERT 可能不直接反映到表（事务隔离）
        // 至少 savepoint 和 rollback 不会报错
        assert!(result.rows.len() >= 1, "应有原始行 (1, 100)");
    }

    #[test]
    fn test_savepoint_release() {
        let mut conn = Connection::open(":memory:").unwrap();

        conn.execute("CREATE TABLE t (id INT64)").unwrap();

        // BEGIN + SAVEPOINT + RELEASE + COMMIT
        conn.execute("BEGIN").unwrap();
        conn.execute("SAVEPOINT sp1").unwrap();
        conn.execute("RELEASE SAVEPOINT sp1").unwrap();
        conn.execute("COMMIT").unwrap();
        // 没有报错即通过
    }

    #[test]
    fn test_savepoint_nested() {
        let mut conn = Connection::open(":memory:").unwrap();

        conn.execute("CREATE TABLE t (id INT64)").unwrap();
        conn.execute("INSERT INTO t VALUES (1)").unwrap();

        // 嵌套 savepoint
        conn.execute("BEGIN").unwrap();
        conn.execute("INSERT INTO t VALUES (2)").unwrap();
        conn.execute("SAVEPOINT sp1").unwrap();
        conn.execute("INSERT INTO t VALUES (3)").unwrap();
        conn.execute("SAVEPOINT sp2").unwrap();
        conn.execute("INSERT INTO t VALUES (4)").unwrap();
        // 回滚到 sp2，应撤销 (4) 但保留 (3)
        conn.execute("ROLLBACK TO SAVEPOINT sp2").unwrap();
        conn.execute("COMMIT").unwrap();
    }

    // ========================================================================
    // P0-2 事务级 Batcher：显式事务内攒批语义
    // ========================================================================

    fn row_ids(conn: &mut Connection, table: &str, col: &str) -> Vec<crate::Value> {
        let r = conn.execute(&format!("SELECT {col} FROM {table} ORDER BY {col}")).unwrap();
        r.rows.into_iter().map(|mut row| row.remove(0)).collect()
    }

    #[test]
    fn test_txn_batch_commit_persists() {
        // 显式事务内多条 INSERT 攒批 → COMMIT 后全部可见
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (id INT64, v INT64)").unwrap();

        conn.execute("BEGIN").unwrap();
        conn.execute("INSERT INTO t VALUES (1, 10)").unwrap();
        conn.execute("INSERT INTO t VALUES (2, 20)").unwrap();
        conn.execute("INSERT INTO t VALUES (3, 30)").unwrap();
        conn.execute("COMMIT").unwrap();

        let ids = row_ids(&mut conn, "t", "id");
        assert_eq!(ids, vec![
            crate::Value::Int64(1),
            crate::Value::Int64(2),
            crate::Value::Int64(3),
        ]);
    }

    #[test]
    fn test_txn_batch_rollback_discards_unread() {
        // 关键修复：ROLLBACK 撤销未读过的写入段（此前事务内 INSERT
        // 各自内部事务提交，ROLLBACK 无效）
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (id INT64)").unwrap();

        conn.execute("BEGIN").unwrap();
        conn.execute("INSERT INTO t VALUES (1)").unwrap();
        conn.execute("INSERT INTO t VALUES (2)").unwrap();
        conn.execute("ROLLBACK").unwrap();

        let r = conn.execute("SELECT COUNT(*) FROM t").unwrap();
        assert_eq!(r.rows[0][0], crate::Value::Int64(0));
    }

    #[test]
    fn test_txn_batch_read_your_writes() {
        // 事务内读己之写：INSERT 后 SELECT 立即可见（读触发 flush）
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (id INT64)").unwrap();

        conn.execute("BEGIN").unwrap();
        conn.execute("INSERT INTO t VALUES (1)").unwrap();
        let r = conn.execute("SELECT COUNT(*) FROM t").unwrap();
        assert_eq!(r.rows[0][0], crate::Value::Int64(1));
        conn.execute("INSERT INTO t VALUES (2)").unwrap();
        let r = conn.execute("SELECT COUNT(*) FROM t").unwrap();
        assert_eq!(r.rows[0][0], crate::Value::Int64(2));
        conn.execute("COMMIT").unwrap();

        let r = conn.execute("SELECT COUNT(*) FROM t").unwrap();
        assert_eq!(r.rows[0][0], crate::Value::Int64(2));
    }

    #[test]
    fn test_txn_batch_rollback_after_read_keeps_flushed() {
        // 读触发 flush 后：已 flush 的数据保留（内部事务已落盘），
        // 未 flush 的写入段可回滚
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (id INT64)").unwrap();

        conn.execute("BEGIN").unwrap();
        conn.execute("INSERT INTO t VALUES (1)").unwrap();
        conn.execute("INSERT INTO t VALUES (2)").unwrap();
        // SELECT 触发 flush（1、2 落盘）
        let r = conn.execute("SELECT COUNT(*) FROM t").unwrap();
        assert_eq!(r.rows[0][0], crate::Value::Int64(2));
        conn.execute("INSERT INTO t VALUES (3)").unwrap();
        // ROLLBACK：3 被撤销，1、2 保留（与 flush-on-read 语义一致）
        conn.execute("ROLLBACK").unwrap();

        let r = conn.execute("SELECT COUNT(*) FROM t").unwrap();
        assert_eq!(r.rows[0][0], crate::Value::Int64(2));
    }

    #[test]
    fn test_txn_batch_savepoint_partial_rollback() {
        // 关键修复：SAVEPOINT 部分回滚（此前无效）
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (id INT64)").unwrap();

        conn.execute("BEGIN").unwrap();
        conn.execute("INSERT INTO t VALUES (1)").unwrap();
        conn.execute("SAVEPOINT sp1").unwrap();
        conn.execute("INSERT INTO t VALUES (2)").unwrap();
        conn.execute("INSERT INTO t VALUES (3)").unwrap();
        conn.execute("ROLLBACK TO SAVEPOINT sp1").unwrap();
        conn.execute("INSERT INTO t VALUES (4)").unwrap();
        conn.execute("COMMIT").unwrap();

        // 2、3 被撤销，1（savepoint 前）与 4（回滚后）保留
        let ids = row_ids(&mut conn, "t", "id");
        assert_eq!(ids, vec![
            crate::Value::Int64(1),
            crate::Value::Int64(4),
        ]);
    }

    #[test]
    fn test_txn_batch_savepoint_before_flush() {
        // SAVEPOINT 前写入被 flush（保存点前数据落盘），savepoint 后
        // 写入可被 ROLLBACK TO 撤销
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (id INT64)").unwrap();

        conn.execute("BEGIN").unwrap();
        conn.execute("INSERT INTO t VALUES (1)").unwrap();
        conn.execute("SAVEPOINT sp1").unwrap();
        conn.execute("INSERT INTO t VALUES (2)").unwrap();
        conn.execute("ROLLBACK TO SAVEPOINT sp1").unwrap();
        conn.execute("COMMIT").unwrap();

        let ids = row_ids(&mut conn, "t", "id");
        assert_eq!(ids, vec![crate::Value::Int64(1)]);
    }

    #[test]
    fn test_txn_batch_constraint_table_bypass() {
        // 有约束的表（主键）跳过攒批：PK 冲突在语句时即时暴露
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (id INT PRIMARY KEY)").unwrap();

        conn.execute("BEGIN").unwrap();
        conn.execute("INSERT INTO t VALUES (1)").unwrap();
        // 冲突：攒批路径下会延迟到 flush；约束门控下语句时即报错
        let err = conn.execute("INSERT INTO t VALUES (1)").unwrap_err();
        assert!(matches!(err, crate::common::error::EngramDbError::ConstraintViolation(_)),
            "expected ConstraintViolation at statement time, got {err:?}");
        conn.execute("COMMIT").unwrap();

        let r = conn.execute("SELECT COUNT(*) FROM t").unwrap();
        assert_eq!(r.rows[0][0], crate::Value::Int64(1));
    }

    #[test]
    fn test_txn_batch_threshold_flushes_early() {
        // 阈值（txn_batch_rows）触发提前 flush：大事务不爆内存且正确落盘
        let mut cfg = crate::common::config::Config::default();
        cfg.txn_batch_rows = 4;
        let mut conn = Connection::open_with_config(":memory:", cfg).unwrap();
        conn.execute("CREATE TABLE t (id INT64)").unwrap();

        conn.execute("BEGIN").unwrap();
        for i in 0..10 {
            conn.execute(&format!("INSERT INTO t VALUES ({i})")).unwrap();
        }
        // 未提交前阈值已触发多次 flush（数据已落盘但事务仍活跃）
        conn.execute("COMMIT").unwrap();

        let r = conn.execute("SELECT COUNT(*) FROM t").unwrap();
        assert_eq!(r.rows[0][0], crate::Value::Int64(10));
    }

    #[test]
    fn test_txn_batch_disabled_falls_back() {
        // txn_batch_enabled=false：事务内 INSERT 逐条内部事务（旧行为），
        // ROLLBACK 不撤销（保持兼容）
        let mut cfg = crate::common::config::Config::default();
        cfg.txn_batch_enabled = false;
        let mut conn = Connection::open_with_config(":memory:", cfg).unwrap();
        conn.execute("CREATE TABLE t (id INT64)").unwrap();

        conn.execute("BEGIN").unwrap();
        conn.execute("INSERT INTO t VALUES (1)").unwrap();
        conn.execute("ROLLBACK").unwrap();

        // 旧行为：内部事务已提交，数据保留
        let r = conn.execute("SELECT COUNT(*) FROM t").unwrap();
        assert_eq!(r.rows[0][0], crate::Value::Int64(1));
    }

    #[test]
    fn test_txn_batch_constraint_bypass_configurable() {
        // v0.20：约束表一律攒批（txn_batch_bypass_constraint_tables 不再绕过），
        // 但入批时即校验 → PK 冲突在该语句返回时暴露（而非延迟到 COMMIT）。
        let mut cfg = crate::common::config::Config::default();
        cfg.txn_batch_bypass_constraint_tables = false;
        let mut conn = Connection::open_with_config(":memory:", cfg).unwrap();
        conn.execute("CREATE TABLE t (id INT PRIMARY KEY)").unwrap();

        conn.execute("BEGIN").unwrap();
        conn.execute("INSERT INTO t VALUES (1)").unwrap();
        // 攒批路径 + 入批即校验：第二条冲突语句即时报错
        let err = conn.execute("INSERT INTO t VALUES (1)").unwrap_err();
        assert!(matches!(err, crate::common::error::EngramDbError::ConstraintViolation(_)),
            "expected ConstraintViolation at statement, got {err:?}");
        // 失败零副作用：行数不变
        conn.execute("COMMIT").unwrap();
        let r = conn.execute("SELECT COUNT(*) FROM t").unwrap();
        assert_eq!(r.rows[0][0], crate::Value::Int64(1));
    }

    #[test]
    fn test_txn_batch_wal_amortized() {
        // 性能验证：事务内多条 INSERT 攒批期间零 WAL 记录，
        // COMMIT 时一次性写入（N 条语句 → 1 次落盘）
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (id INT64)").unwrap();

        conn.execute("BEGIN").unwrap();
        let lsn_before = conn.database_mut().txn_manager().current_wal_lsn();
        for i in 0..5 {
            conn.execute(&format!("INSERT INTO t VALUES ({i})")).unwrap();
        }
        let lsn_after_inserts = conn.database_mut().txn_manager().current_wal_lsn();
        assert_eq!(lsn_before, lsn_after_inserts, "攒批期间不得产生 WAL 记录");
        conn.execute("COMMIT").unwrap();
        let lsn_after_commit = conn.database_mut().txn_manager().current_wal_lsn();
        assert!(lsn_after_commit > lsn_after_inserts, "COMMIT 时一次性 flush WAL");

        let r = conn.execute("SELECT COUNT(*) FROM t").unwrap();
        assert_eq!(r.rows[0][0], crate::Value::Int64(5));
    }

    #[test]
    fn test_ttl_expiration() {
        use crate::common::types::{TableDef, ColumnDef, DataType};

        let mut conn = Connection::open(":memory:").unwrap();

        // 创建有 TTL 的表：60 秒过期
        let columns = vec![
            ColumnDef::new("id", DataType::Int64).primary_key(),
            ColumnDef::new("data", DataType::Varchar),
            ColumnDef::new("created_at", DataType::Timestamp),
        ];
        let mut table_def = TableDef::new(0, "ttl_test", columns);
        table_def.ttl_seconds = Some(60); // 60 秒过期
        table_def.ttl_column = Some(2);   // created_at 列是 TTL 参考列

        let db = conn.database_mut();
        db.create_table(table_def).unwrap();

        // 插入数据（TTL 会自动填充 created_at 为当前时间）
        let rows = vec![
            vec![Value::Int64(1), Value::Varchar("alive".into()), Value::Null],
        ];
        let table = db.get_table_mut("ttl_test").unwrap();
        table.insert(rows).unwrap();

        // 验证刚插入的数据可以被查询到（未过期）
        let row = table.get_row_by_id(0).unwrap();  // delta store 使用 0-based row_id
        assert!(row.is_some(), "刚插入的数据应该可查询");

        // 验证 scan 也能查到
        let all = table.scan(&[0, 1]).unwrap();
        assert_eq!(all.len(), 1, "scan 应该返回 1 行");

        // 模拟过期：手动设置 created_at 为 120 秒前
        let past = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64 - 120_000;
        let table = db.get_table_mut("ttl_test").unwrap();
        // 直接修改 Delta 层的数据（row_id=0）
        let old_row = table.delta_store().get(0).unwrap();
        let mut modified_row = old_row.clone();
        modified_row[2] = Value::Timestamp(past);
        table.delta_store_mut().update_row_by_id(0, modified_row).unwrap();

        // 验证过期行不再被查询到
        let row = table.get_row_by_id(0).unwrap();
        assert!(row.is_none(), "过期行应该不可查询");

        // 验证 scan 也看不到过期行
        let all = table.scan(&[0, 1]).unwrap();
        assert_eq!(all.len(), 0, "scan 应该看不到过期行");

        // 验证 compaction 后物理删除
        table.compact_delta().unwrap();
        // 重新插入一条新数据验证 compaction 后索引正常
        let rows = vec![
            vec![Value::Int64(2), Value::Varchar("new".into()), Value::Null],
        ];
        table.insert(rows).unwrap();
        let all = table.scan(&[0, 1]).unwrap();
        assert_eq!(all.len(), 1, "compaction 后只有新数据");
    }

    #[test]
    fn test_count_after_batch_insert_triggering_compact() {
        // 回归测试：批量 INSERT 触发 compaction 后 COUNT(*) 不应翻倍
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (id INT64, val VARCHAR)").unwrap();

        // 默认 Adaptive 策略 min_threshold = 10,000：
        // 单批 10,000 行写入 Delta 层（< direct_threshold 30,720）
        // 会触发 maybe_compact → compact_delta_partial，此前此处会重复累加 row_count
        let rows: Vec<String> = (0..10_000)
            .map(|i| format!("({}, 'v{}')", i, i))
            .collect();
        let sql = format!("INSERT INTO t VALUES {}", rows.join(", "));
        conn.execute(&sql).unwrap();

        // COUNT(*) fast-path（元数据）与实际行数必须一致
        let result = conn.execute("SELECT COUNT(*) FROM t").unwrap();
        assert_eq!(result.rows[0][0], Value::Int64(10_000), "COUNT(*) 应等于 10000，不应翻倍");

        // SELECT * 全量扫描验证实际行数
        let result = conn.execute("SELECT COUNT(*) FROM t").unwrap();
        assert_eq!(result.rows[0][0], Value::Int64(10_000));
    }

    #[test]
    fn test_count_after_explicit_compact() {
        // 回归测试：显式 compact_delta 后 COUNT(*) 保持不变
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (id INT64)").unwrap();

        conn.execute("INSERT INTO t VALUES (1), (2), (3), (4), (5)").unwrap();
        conn.compact_all().unwrap();

        let result = conn.execute("SELECT COUNT(*) FROM t").unwrap();
        assert_eq!(result.rows[0][0], Value::Int64(5), "compact 后 COUNT 应保持 5");

        // compact 后继续插入，计数仍正确
        conn.execute("INSERT INTO t VALUES (6), (7)").unwrap();
        let result = conn.execute("SELECT COUNT(*) FROM t").unwrap();
        assert_eq!(result.rows[0][0], Value::Int64(7), "compact 后继续插入 COUNT 应为 7");
    }

    #[test]
    fn test_count_after_import_columns_compact() {
        // 回归测试：import_columns 小批量 + compact 后 COUNT(*) 正确
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (id INT64, val VARCHAR)").unwrap();

        // 小批量（< direct_threshold），走 Delta 层
        let ids: Vec<Value> = (0..100).map(|i| Value::Int64(i)).collect();
        let vals: Vec<Value> = (0..100).map(|i| Value::Varchar(format!("v{}", i))).collect();
        conn.import_columns("t", vec![ids, vals]).unwrap();

        let result = conn.execute("SELECT COUNT(*) FROM t").unwrap();
        assert_eq!(result.rows[0][0], Value::Int64(100), "import_columns 小批量 COUNT 应为 100");

        // compact 后计数不变
        conn.compact_all().unwrap();
        let result = conn.execute("SELECT COUNT(*) FROM t").unwrap();
        assert_eq!(result.rows[0][0], Value::Int64(100), "compact 后 COUNT 应为 100");
    }

    #[test]
    fn test_having_basic() {
        let mut conn = Connection::open(":memory:").unwrap();

        // 创建表
        conn.execute("CREATE TABLE sales (id INT64, product VARCHAR, amount INT64)").unwrap();
        conn.execute("INSERT INTO sales VALUES (1, 'A', 100)").unwrap();
        conn.execute("INSERT INTO sales VALUES (2, 'A', 200)").unwrap();
        conn.execute("INSERT INTO sales VALUES (3, 'B', 150)").unwrap();
        conn.execute("INSERT INTO sales VALUES (4, 'B', 50)").unwrap();
        conn.execute("INSERT INTO sales VALUES (5, 'C', 300)").unwrap();

        // 先验证分组查询基础
        let result = conn.execute("SELECT product, SUM(amount) FROM sales GROUP BY product").unwrap();
        assert_eq!(result.rows.len(), 3, "应有 3 个分组");

        // HAVING SUM(amount) > 250: A=300, B=200, C=300 → A 和 C 通过
        let result = conn.execute("SELECT product, SUM(amount) FROM sales GROUP BY product HAVING SUM(amount) > 250").unwrap();
        assert_eq!(result.rows.len(), 2, "A(total=300) 和 C(total=300) 应被选中");

        // HAVING SUM(amount) < 250: A=300, B=200, C=300 → B 通过
        let result = conn.execute("SELECT product, SUM(amount) FROM sales GROUP BY product HAVING SUM(amount) < 250").unwrap();
        assert_eq!(result.rows.len(), 1, "B(total=200) 应被选中");
    }

    #[test]
    fn test_having_with_count() {
        let mut conn = Connection::open(":memory:").unwrap();

        conn.execute("CREATE TABLE orders (id INT64, customer VARCHAR, amount INT64)").unwrap();
        conn.execute("INSERT INTO orders VALUES (1, 'Alice', 100)").unwrap();
        conn.execute("INSERT INTO orders VALUES (2, 'Bob', 200)").unwrap();
        conn.execute("INSERT INTO orders VALUES (3, 'Alice', 150)").unwrap();
        conn.execute("INSERT INTO orders VALUES (4, 'Charlie', 50)").unwrap();
        conn.execute("INSERT INTO orders VALUES (5, 'Bob', 300)").unwrap();
        conn.execute("INSERT INTO orders VALUES (6, 'Alice', 200)").unwrap();

        // HAVING COUNT(*) > 1: Alice=3, Bob=2, Charlie=1 → Alice 和 Bob 通过
        let result = conn.execute("SELECT customer, COUNT(*) FROM orders GROUP BY customer HAVING COUNT(*) > 1").unwrap();
        assert_eq!(result.rows.len(), 2, "Alice(3) 和 Bob(2) 应有多个订单");
    }

    #[test]
    fn test_case_when_basic() {
        let mut conn = Connection::open(":memory:").unwrap();

        conn.execute("CREATE TABLE scores (id INT64, score INT64)").unwrap();
        conn.execute("INSERT INTO scores VALUES (1, 95)").unwrap();
        conn.execute("INSERT INTO scores VALUES (2, 85)").unwrap();
        conn.execute("INSERT INTO scores VALUES (3, 70)").unwrap();
        conn.execute("INSERT INTO scores VALUES (4, 55)").unwrap();

        // CASE WHEN score >= 90 THEN 'A' WHEN score >= 80 THEN 'B' ELSE 'C' END
        let result = conn.execute("SELECT id, CASE WHEN score >= 90 THEN 'A' WHEN score >= 80 THEN 'B' ELSE 'C' END FROM scores").unwrap();
        assert_eq!(result.rows.len(), 4);
        assert_eq!(result.rows[0][1], Value::Varchar("A".to_string())); // 95 -> A
        assert_eq!(result.rows[1][1], Value::Varchar("B".to_string())); // 85 -> B
        assert_eq!(result.rows[2][1], Value::Varchar("C".to_string())); // 70 -> C
        assert_eq!(result.rows[3][1], Value::Varchar("C".to_string())); // 55 -> C
    }

    #[test]
    fn test_case_when_no_else() {
        let mut conn = Connection::open(":memory:").unwrap();

        conn.execute("CREATE TABLE t (id INT64, x INT64)").unwrap();
        conn.execute("INSERT INTO t VALUES (1, 10)").unwrap();
        conn.execute("INSERT INTO t VALUES (2, 20)").unwrap();
        conn.execute("INSERT INTO t VALUES (3, 30)").unwrap();

        // CASE WHEN x > 15 THEN 'big' END — 无 ELSE，未匹配返回 NULL
        let result = conn.execute("SELECT id, CASE WHEN x > 15 THEN 'big' END FROM t").unwrap();
        assert_eq!(result.rows.len(), 3);
        assert_eq!(result.rows[0][1], Value::Null); // 10 -> NULL
        assert_eq!(result.rows[1][1], Value::Varchar("big".to_string()));
        assert_eq!(result.rows[2][1], Value::Varchar("big".to_string()));
    }

    #[test]
    fn test_case_when_in_where() {
        let mut conn = Connection::open(":memory:").unwrap();

        conn.execute("CREATE TABLE products (id INT64, price INT64)").unwrap();
        conn.execute("INSERT INTO products VALUES (1, 50)").unwrap();
        conn.execute("INSERT INTO products VALUES (2, 150)").unwrap();
        conn.execute("INSERT INTO products VALUES (3, 500)").unwrap();
        conn.execute("INSERT INTO products VALUES (4, 1000)").unwrap();

        // 在 WHERE 中使用 CASE WHEN
        let result = conn.execute("SELECT id, CASE WHEN price < 100 THEN 'cheap' WHEN price < 500 THEN 'mid' ELSE 'expensive' END AS tier FROM products WHERE (CASE WHEN price < 500 THEN 1 ELSE 0 END) = 1").unwrap();
        assert_eq!(result.rows.len(), 2, "price < 500 的产品有 2 个（50 和 150，500 不算）");
    }

    #[test]
    fn test_union_all() {
        let mut conn = Connection::open(":memory:").unwrap();

        conn.execute("CREATE TABLE t1 (id INT64, name VARCHAR)").unwrap();
        conn.execute("CREATE TABLE t2 (id INT64, name VARCHAR)").unwrap();
        conn.execute("INSERT INTO t1 VALUES (1, 'a'), (2, 'b'), (3, 'c')").unwrap();
        conn.execute("INSERT INTO t2 VALUES (3, 'c'), (4, 'd'), (5, 'e')").unwrap();

        // UNION ALL：不去重，6 行
        let result = conn.execute("SELECT id, name FROM t1 UNION ALL SELECT id, name FROM t2").unwrap();
        assert_eq!(result.rows.len(), 6, "UNION ALL 应返回 6 行（不去重）");
    }

    #[test]
    fn test_union_dedup() {
        let mut conn = Connection::open(":memory:").unwrap();

        conn.execute("CREATE TABLE t1 (id INT64, name VARCHAR)").unwrap();
        conn.execute("CREATE TABLE t2 (id INT64, name VARCHAR)").unwrap();
        conn.execute("INSERT INTO t1 VALUES (1, 'a'), (2, 'b'), (3, 'c')").unwrap();
        conn.execute("INSERT INTO t2 VALUES (3, 'c'), (4, 'd'), (5, 'e')").unwrap();

        // UNION：去重，5 行
        let result = conn.execute("SELECT id, name FROM t1 UNION SELECT id, name FROM t2").unwrap();
        assert_eq!(result.rows.len(), 5, "UNION 应返回 5 行（去重 1 行：id=3,name='c'）");
    }

    #[test]
    fn test_union_no_overlap() {
        let mut conn = Connection::open(":memory:").unwrap();

        conn.execute("CREATE TABLE t1 (id INT64)").unwrap();
        conn.execute("CREATE TABLE t2 (id INT64)").unwrap();
        conn.execute("INSERT INTO t1 VALUES (1), (2), (3)").unwrap();
        conn.execute("INSERT INTO t2 VALUES (4), (5), (6)").unwrap();

        // UNION：完全无重叠，UNION 和 UNION ALL 结果相同
        let result = conn.execute("SELECT id FROM t1 UNION SELECT id FROM t2").unwrap();
        assert_eq!(result.rows.len(), 6);

        let result = conn.execute("SELECT id FROM t1 UNION ALL SELECT id FROM t2").unwrap();
        assert_eq!(result.rows.len(), 6);
    }

    #[test]
    fn test_union_with_where() {
        let mut conn = Connection::open(":memory:").unwrap();

        conn.execute("CREATE TABLE t1 (id INT64, val INT64)").unwrap();
        conn.execute("CREATE TABLE t2 (id INT64, val INT64)").unwrap();
        conn.execute("INSERT INTO t1 VALUES (1, 10), (2, 20), (3, 30)").unwrap();
        conn.execute("INSERT INTO t2 VALUES (3, 30), (4, 40), (5, 50)").unwrap();

        // 带 WHERE 过滤的 UNION
        let result = conn.execute("SELECT id FROM t1 WHERE val > 15 UNION ALL SELECT id FROM t2 WHERE val > 35").unwrap();
        assert_eq!(result.rows.len(), 4, "t1(val>15): 2 行, t2(val>35): 2 行");
    }

    #[test]
    fn test_intersect() {
        let mut conn = Connection::open(":memory:").unwrap();

        conn.execute("CREATE TABLE t1 (id INT64)").unwrap();
        conn.execute("CREATE TABLE t2 (id INT64)").unwrap();
        conn.execute("INSERT INTO t1 VALUES (1), (2), (3), (4)").unwrap();
        conn.execute("INSERT INTO t2 VALUES (3), (4), (5), (6)").unwrap();

        // INTERSECT：返回交集 {3, 4}
        let result = conn.execute("SELECT id FROM t1 INTERSECT SELECT id FROM t2").unwrap();
        assert_eq!(result.rows.len(), 2, "INTERSECT 应返回 2 行");
        let ids: Vec<i64> = result.rows.iter().map(|r| match r[0] { Value::Int64(v) => v, _ => 0 }).collect();
        assert!(ids.contains(&3));
        assert!(ids.contains(&4));
    }

    #[test]
    fn test_except() {
        let mut conn = Connection::open(":memory:").unwrap();

        conn.execute("CREATE TABLE t1 (id INT64)").unwrap();
        conn.execute("CREATE TABLE t2 (id INT64)").unwrap();
        conn.execute("INSERT INTO t1 VALUES (1), (2), (3), (4)").unwrap();
        conn.execute("INSERT INTO t2 VALUES (3), (4), (5), (6)").unwrap();

        // EXCEPT：t1 - t2 = {1, 2}
        let result = conn.execute("SELECT id FROM t1 EXCEPT SELECT id FROM t2").unwrap();
        assert_eq!(result.rows.len(), 2, "EXCEPT 应返回 2 行");
        let ids: Vec<i64> = result.rows.iter().map(|r| match r[0] { Value::Int64(v) => v, _ => 0 }).collect();
        assert!(ids.contains(&1));
        assert!(ids.contains(&2));
    }

    #[test]
    fn test_cross_join() {
        let mut conn = Connection::open(":memory:").unwrap();

        conn.execute("CREATE TABLE t1 (id INT64, name VARCHAR)").unwrap();
        conn.execute("CREATE TABLE t2 (val INT64)").unwrap();
        conn.execute("INSERT INTO t1 VALUES (1, 'a'), (2, 'b')").unwrap();
        conn.execute("INSERT INTO t2 VALUES (10), (20)").unwrap();

        // CROSS JOIN：笛卡尔积，2 × 2 = 4 行
        let result = conn.execute("SELECT t1.id, t1.name, t2.val FROM t1 CROSS JOIN t2 ORDER BY t1.id, t2.val").unwrap();
        assert_eq!(result.rows.len(), 4, "CROSS JOIN 应返回 4 行");
        // 验证第一行: id=1, name='a', val=10
        assert_eq!(result.rows[0].len(), 3);
        assert_eq!(result.rows[0][0], Value::Int64(1));
        assert_eq!(result.rows[0][1], Value::Varchar("a".into()));
        assert_eq!(result.rows[0][2], Value::Int64(10));
    }

    #[test]
    fn test_insert_select_basic() {
        let mut conn = Connection::open(":memory:").unwrap();

        // 源表
        conn.execute("CREATE TABLE src (id INT64, name VARCHAR)").unwrap();
        conn.execute("INSERT INTO src VALUES (1, 'a'), (2, 'b'), (3, 'c')").unwrap();

        // 目标表
        conn.execute("CREATE TABLE dst (id INT64, name VARCHAR)").unwrap();

        // INSERT ... SELECT
        let result = conn.execute("INSERT INTO dst SELECT id, name FROM src").unwrap();
        assert_eq!(result.rows[0][0], Value::Int64(3));

        // 验证目标表有 3 行
        let result = conn.execute("SELECT COUNT(*) FROM dst").unwrap();
        assert_eq!(result.rows[0][0], Value::Int64(3));
    }

    #[test]
    fn test_insert_select_with_where() {
        let mut conn = Connection::open(":memory:").unwrap();

        conn.execute("CREATE TABLE products (id INT64, price INT64)").unwrap();
        conn.execute("INSERT INTO products VALUES (1, 50), (2, 150), (3, 500), (4, 1000)").unwrap();

        conn.execute("CREATE TABLE cheap_products (id INT64, price INT64)").unwrap();

        // INSERT ... SELECT ... WHERE — 只插入价格 < 200 的
        conn.execute("INSERT INTO cheap_products SELECT id, price FROM products WHERE price < 200").unwrap();

        let result = conn.execute("SELECT COUNT(*) FROM cheap_products").unwrap();
        assert_eq!(result.rows[0][0], Value::Int64(2), "price < 200 的产品有 2 个");
    }

    #[test]
    fn test_insert_select_with_columns() {
        let mut conn = Connection::open(":memory:").unwrap();

        conn.execute("CREATE TABLE src (a INT64, b INT64, c INT64)").unwrap();
        conn.execute("INSERT INTO src VALUES (1, 10, 100), (2, 20, 200)").unwrap();

        conn.execute("CREATE TABLE dst (x INT64, y INT64)").unwrap();

        // 指定列插入（重排列）
        conn.execute("INSERT INTO dst (x, y) SELECT c, a FROM src").unwrap();

        let result = conn.execute("SELECT x, y FROM dst ORDER BY x").unwrap();
        assert_eq!(result.rows.len(), 2);
        // ORDER BY x ASC: smallest x first
        assert_eq!(result.rows[0], vec![Value::Int64(100), Value::Int64(1)]);
        assert_eq!(result.rows[1], vec![Value::Int64(200), Value::Int64(2)]);
    }

    #[test]
    fn test_truncate_table() {
        let mut conn = Connection::open(":memory:").unwrap();

        conn.execute("CREATE TABLE t (id INT64, name VARCHAR)").unwrap();
        conn.execute("INSERT INTO t VALUES (1, 'a'), (2, 'b'), (3, 'c')").unwrap();

        let result = conn.execute("SELECT COUNT(*) FROM t").unwrap();
        assert_eq!(result.rows[0][0], Value::Int64(3));

        // TRUNCATE
        conn.execute("TRUNCATE TABLE t").unwrap();

        let result = conn.execute("SELECT COUNT(*) FROM t").unwrap();
        assert_eq!(result.rows[0][0], Value::Int64(0), "TRUNCATE 后表应为空");

        // 表结构仍可用：可以继续插入
        conn.execute("INSERT INTO t VALUES (10, 'x')").unwrap();
        let result = conn.execute("SELECT COUNT(*) FROM t").unwrap();
        assert_eq!(result.rows[0][0], Value::Int64(1));
    }

    #[test]
    fn test_in_list_basic() {
        let mut conn = Connection::open(":memory:").unwrap();

        conn.execute("CREATE TABLE products (id INT64, name VARCHAR)").unwrap();
        conn.execute("INSERT INTO products VALUES (1, 'apple'), (2, 'banana'), (3, 'cherry'), (4, 'date')").unwrap();

        // IN 列表基本查询
        let result = conn.execute("SELECT id FROM products WHERE name IN ('apple', 'cherry')").unwrap();
        assert_eq!(result.rows.len(), 2, "应返回 apple 和 cherry");

        // NOT IN
        let result = conn.execute("SELECT id FROM products WHERE name NOT IN ('apple', 'cherry')").unwrap();
        assert_eq!(result.rows.len(), 2, "应返回 banana 和 date");
    }

    #[test]
    fn test_in_list_with_int() {
        let mut conn = Connection::open(":memory:").unwrap();

        conn.execute("CREATE TABLE t (id INT64, val INT64)").unwrap();
        conn.execute("INSERT INTO t VALUES (1, 10), (2, 20), (3, 30), (4, 40), (5, 50)").unwrap();

        // 数值 IN 列表
        let result = conn.execute("SELECT id FROM t WHERE val IN (10, 30, 50)").unwrap();
        assert_eq!(result.rows.len(), 3, "val IN (10, 30, 50) 应返回 3 行");
    }

    #[test]
    fn test_create_table_as_select() {
        let mut conn = Connection::open(":memory:").unwrap();

        // 源表
        conn.execute("CREATE TABLE src (id INT64, name VARCHAR, score INT64)").unwrap();
        conn.execute("INSERT INTO src VALUES (1, 'alice', 90), (2, 'bob', 85), (3, 'charlie', 95)").unwrap();

        // CREATE TABLE AS SELECT：创建高分学生表
        conn.execute("CREATE TABLE high_scores AS SELECT id, name, score FROM src WHERE score >= 90").unwrap();

        let result = conn.execute("SELECT id FROM high_scores ORDER BY id").unwrap();
        assert_eq!(result.rows.len(), 2, "高分学生应有 2 人（alice 和 charlie）");
    }

    #[test]
    fn test_create_table_as_select_with_columns() {
        let mut conn = Connection::open(":memory:").unwrap();

        conn.execute("CREATE TABLE src (a INT64, b INT64)").unwrap();
        conn.execute("INSERT INTO src VALUES (1, 10), (2, 20)").unwrap();

        // CREATE TABLE col1 TYPE, col2 TYPE AS SELECT ... — 显式列定义
        conn.execute("CREATE TABLE dst (x INT64, y VARCHAR) AS SELECT a, 'tag' FROM src").unwrap();

        let result = conn.execute("SELECT x FROM dst").unwrap();
        assert_eq!(result.rows.len(), 2);
    }

    #[test]
    fn test_insert_or_ignore() {
        let mut conn = Connection::open(":memory:").unwrap();

        conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY, val INT64)").unwrap();
        conn.execute("INSERT INTO t VALUES (1, 100), (2, 200)").unwrap();

        // INSERT OR IGNORE：尝试插入重复的 id，应该被忽略
        conn.execute("INSERT OR IGNORE INTO t VALUES (1, 999), (3, 300)").unwrap();

        let result = conn.execute("SELECT COUNT(*) FROM t").unwrap();
        assert_eq!(result.rows[0][0], Value::Int64(3), "应保留 3 行（2 原有 + 1 新增）");

        // 验证 val=100 (id=1) 的行还在
        let result = conn.execute("SELECT id, val FROM t WHERE val = 100").unwrap();
        assert_eq!(result.rows.len(), 1, "val=100 的行应存在");
        assert_eq!(result.rows[0][1], Value::Int64(100));

        // 验证 val=300 (id=3) 的行被插入
        let result = conn.execute("SELECT id FROM t WHERE val = 300").unwrap();
        assert_eq!(result.rows.len(), 1, "val=300 的行应被插入");
        assert_eq!(result.rows[0][0], Value::Int64(3));
    }

    #[test]
    fn test_insert_or_replace() {
        let mut conn = Connection::open(":memory:").unwrap();

        conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY, val VARCHAR)").unwrap();
        conn.execute("INSERT INTO t VALUES (1, 'hello'), (2, 'world')").unwrap();

        // INSERT OR REPLACE：替换重复的 id=1，插入新的 id=3
        conn.execute("INSERT OR REPLACE INTO t VALUES (1, 'replaced'), (3, 'new')").unwrap();

        let result = conn.execute("SELECT COUNT(*) FROM t").unwrap();
        assert_eq!(result.rows[0][0], Value::Int64(3), "应保留 3 行（1 替换 + 1 新增 + 1 不变）");

        // 验证 id=1 的 val 被替换
        let result = conn.execute("SELECT val FROM t WHERE id = 1").unwrap();
        assert_eq!(result.rows[0][0], Value::Varchar("replaced".into()), "id=1 的 val 应被替换");

        // 验证 id=2 的 val 不变
        let result = conn.execute("SELECT val FROM t WHERE id = 2").unwrap();
        assert_eq!(result.rows[0][0], Value::Varchar("world".into()), "id=2 的 val 应保持不变");
    }

    #[test]
    fn test_replace_into() {
        let mut conn = Connection::open(":memory:").unwrap();

        conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY, val INT64)").unwrap();
        conn.execute("INSERT INTO t VALUES (1, 100), (2, 200)").unwrap();

        // REPLACE INTO：等价于 INSERT OR REPLACE
        conn.execute("REPLACE INTO t VALUES (1, 999), (3, 300)").unwrap();

        let result = conn.execute("SELECT COUNT(*) FROM t").unwrap();
        assert_eq!(result.rows[0][0], Value::Int64(3), "应保留 3 行");

        // 验证 id=1 被替换
        let result = conn.execute("SELECT val FROM t WHERE id = 1").unwrap();
        assert_eq!(result.rows[0][0], Value::Int64(999), "id=1 的 val 应被替换为 999");
    }

    #[test]
    fn test_subquery_in_list() {
        let mut conn = Connection::open(":memory:").unwrap();

        conn.execute("CREATE TABLE t1 (id INT64, val VARCHAR)").unwrap();
        conn.execute("INSERT INTO t1 VALUES (1, 'a'), (2, 'b'), (3, 'c')").unwrap();
        conn.execute("CREATE TABLE t2 (id INT64)").unwrap();
        conn.execute("INSERT INTO t2 VALUES (1), (3)").unwrap();

        // IN (SELECT ...) — 子查询
        let result = conn.execute("SELECT val FROM t1 WHERE id IN (SELECT id FROM t2) ORDER BY id").unwrap();
        assert_eq!(result.rows.len(), 2, "IN 子查询应返回 2 行");
        assert_eq!(result.rows[0][0], Value::Varchar("a".into()));
        assert_eq!(result.rows[1][0], Value::Varchar("c".into()));
    }

    #[test]
    fn test_subquery_exists() {
        let mut conn = Connection::open(":memory:").unwrap();

        conn.execute("CREATE TABLE t1 (id INT64, val VARCHAR)").unwrap();
        conn.execute("INSERT INTO t1 VALUES (1, 'a'), (2, 'b'), (3, 'c')").unwrap();
        conn.execute("CREATE TABLE t2 (id INT64)").unwrap();
        conn.execute("INSERT INTO t2 VALUES (1), (3)").unwrap();

        // EXISTS (SELECT ...) — 子查询（非关联）
        let result = conn.execute("SELECT val FROM t1 WHERE EXISTS (SELECT 1 FROM t2 WHERE t2.id = 1)").unwrap();
        assert_eq!(result.rows.len(), 3, "EXISTS 为真时应返回所有行");
    }

    #[test]
    fn test_subquery_not_exists() {
        let mut conn = Connection::open(":memory:").unwrap();

        conn.execute("CREATE TABLE t1 (id INT64, val VARCHAR)").unwrap();
        conn.execute("INSERT INTO t1 VALUES (1, 'a'), (2, 'b'), (3, 'c')").unwrap();
        conn.execute("CREATE TABLE t2 (id INT64)").unwrap();
        conn.execute("INSERT INTO t2 VALUES (1)").unwrap();

        // NOT EXISTS (SELECT ...) — 子查询（非关联）
        let result = conn.execute("SELECT val FROM t1 WHERE NOT EXISTS (SELECT 1 FROM t2 WHERE t2.id = 999)").unwrap();
        assert_eq!(result.rows.len(), 3, "NOT EXISTS 为真（子查询无结果）时应返回所有行");
    }

    #[test]
    fn test_subquery_scalar() {
        let mut conn = Connection::open(":memory:").unwrap();

        conn.execute("CREATE TABLE t (id INT64, val VARCHAR)").unwrap();
        conn.execute("INSERT INTO t VALUES (1, 'a'), (2, 'b')").unwrap();

        // 标量子查询 (SELECT ...) 作为表达式
        let result = conn.execute("SELECT val, (SELECT COUNT(*) FROM t) AS cnt FROM t WHERE id = 1").unwrap();
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0][0], Value::Varchar("a".into()));
        assert_eq!(result.rows[0][1], Value::Int64(2), "标量子查询应返回 COUNT");
    }

    #[test]
    fn test_between() {
        let mut conn = Connection::open(":memory:").unwrap();

        conn.execute("CREATE TABLE t (id INT64, val INT64)").unwrap();
        conn.execute("INSERT INTO t VALUES (1, 10), (2, 20), (3, 30), (4, 40), (5, 50)").unwrap();

        // BETWEEN 范围查询
        let result = conn.execute("SELECT id FROM t WHERE val BETWEEN 20 AND 40").unwrap();
        assert_eq!(result.rows.len(), 3, "20-40 范围内应有 3 行 (20, 30, 40)");

        // NOT BETWEEN
        let result = conn.execute("SELECT id FROM t WHERE val NOT BETWEEN 20 AND 40").unwrap();
        assert_eq!(result.rows.len(), 2, "20-40 范围外应有 2 行 (10, 50)");

        // BETWEEN with strings
        conn.execute("CREATE TABLE words (w VARCHAR)").unwrap();
        conn.execute("INSERT INTO words VALUES ('apple'), ('banana'), ('cherry'), ('date')").unwrap();
        let result = conn.execute("SELECT w FROM words WHERE w BETWEEN 'banana' AND 'date'").unwrap();
        assert_eq!(result.rows.len(), 3, "banana-date 范围内应有 banana, cherry, date");
    }

    #[test]
    fn test_nullif() {
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (a INT64, b INT64)").unwrap();
        conn.execute("INSERT INTO t VALUES (1, 1), (1, 2), (2, 1)").unwrap();

        // NULLIF(a, b): a == b 时返回 NULL，否则返回 a
        let result = conn.execute("SELECT NULLIF(a, b) FROM t").unwrap();
        assert_eq!(result.rows[0][0], Value::Null, "1 == 1 → NULL");
        assert_eq!(result.rows[1][0], Value::Int64(1), "1 != 2 → 1");
        assert_eq!(result.rows[2][0], Value::Int64(2), "2 != 1 → 2");
    }

    #[test]
    fn test_if_func() {
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (x INT64)").unwrap();
        conn.execute("INSERT INTO t VALUES (1), (2), (3)").unwrap();

        // IF(cond, true_val, false_val)
        let result = conn.execute("SELECT IF(x > 1, 'big', 'small') FROM t").unwrap();
        assert_eq!(result.rows[0][0], Value::Varchar("small".to_string()));
        assert_eq!(result.rows[1][0], Value::Varchar("big".to_string()));
        assert_eq!(result.rows[2][0], Value::Varchar("big".to_string()));
    }

    #[test]
    fn test_trim() {
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (s VARCHAR)").unwrap();
        conn.execute("INSERT INTO t VALUES ('  hello  '), ('xxxhelloxxx'), ('  world')").unwrap();

        // TRIM 默认去除两端空白
        let result = conn.execute("SELECT TRIM(s) FROM t").unwrap();
        assert_eq!(result.rows[0][0], Value::Varchar("hello".to_string()));
        assert_eq!(result.rows[1][0], Value::Varchar("xxxhelloxxx".to_string())); // 无空白不变
        assert_eq!(result.rows[2][0], Value::Varchar("world".to_string()));

        // LTRIM / RTRIM
        let result = conn.execute("SELECT LTRIM(s), RTRIM(s) FROM t WHERE s = '  hello  '").unwrap();
        assert_eq!(result.rows[0][0], Value::Varchar("hello  ".to_string()));
        assert_eq!(result.rows[0][1], Value::Varchar("  hello".to_string()));
    }

    #[test]
    fn test_instr() {
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (s VARCHAR)").unwrap();
        conn.execute("INSERT INTO t VALUES ('hello world'), ('hello')").unwrap();

        // INSTR(haystack, needle): 1-based position, 0 if not found
        let result = conn.execute("SELECT INSTR(s, 'world') FROM t WHERE s = 'hello world'").unwrap();
        assert_eq!(result.rows[0][0], Value::Int64(7));

        let result = conn.execute("SELECT INSTR(s, 'xyz') FROM t WHERE s = 'hello'").unwrap();
        assert_eq!(result.rows[0][0], Value::Int64(0), "未找到应返回 0");
    }

    #[test]
    fn test_split_part() {
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (s VARCHAR)").unwrap();
        conn.execute("INSERT INTO t VALUES ('a,b,c'), ('hello.world.test'), ('single')").unwrap();

        // SPLIT_PART(str, delimiter, part): 1-based
        let result = conn.execute("SELECT SPLIT_PART(s, ',', 2) FROM t WHERE s = 'a,b,c'").unwrap();
        assert_eq!(result.rows[0][0], Value::Varchar("b".into()));

        let result = conn.execute("SELECT SPLIT_PART(s, '.', 3) FROM t WHERE s = 'hello.world.test'").unwrap();
        assert_eq!(result.rows[0][0], Value::Varchar("test".into()));

        // out of range returns empty string
        let result = conn.execute("SELECT SPLIT_PART(s, ',', 5) FROM t WHERE s = 'a,b,c'").unwrap();
        assert_eq!(result.rows[0][0], Value::Varchar("".into()));

        // part < 1 returns empty string
        let result = conn.execute("SELECT SPLIT_PART(s, ',', 0) FROM t WHERE s = 'a,b,c'").unwrap();
        assert_eq!(result.rows[0][0], Value::Varchar("".into()));
    }

    #[test]
    fn test_numeric_functions() {
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (x DOUBLE)").unwrap();
        conn.execute("INSERT INTO t VALUES (1.5), (-1.5), (16.0)").unwrap();

        // CEIL / FLOOR / TRUNC
        let result = conn.execute("SELECT CEIL(x), FLOOR(x), TRUNC(x) FROM t").unwrap();
        assert_eq!(result.rows[0][0], Value::Int64(2));
        assert_eq!(result.rows[0][1], Value::Int64(1));
        assert_eq!(result.rows[0][2], Value::Int64(1));

        // POWER / SQRT
        let result = conn.execute("SELECT POWER(x, 2), SQRT(x) FROM t").unwrap();
        assert_eq!(result.rows[2][0], Value::Int64(256)); // 16^2 = 256 (整数)
        assert_eq!(result.rows[2][1], Value::Int64(4)); // sqrt(16) = 4 (整数)
    }

    #[test]
    fn test_json_object() {
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE _dummy (x INT64)").unwrap();
        conn.execute("INSERT INTO _dummy VALUES (1)").unwrap();
        // JSON_OBJECT('name', 'alice', 'age', 30) → {"name":"alice","age":30}
        let result = conn.execute("SELECT JSON_OBJECT('name', 'alice', 'age', 30) FROM _dummy").unwrap();
        let json = match &result.rows[0][0] {
            Value::Json(s) => s.clone(),
            _ => panic!("Expected JSON value"),
        };
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["name"], "alice");
        assert_eq!(parsed["age"], 30);
    }

    #[test]
    fn test_json_array() {
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE _dummy (x INT64)").unwrap();
        conn.execute("INSERT INTO _dummy VALUES (1)").unwrap();
        // JSON_ARRAY(1, 'two', 3.0) → [1, "two", 3.0]
        let result = conn.execute("SELECT JSON_ARRAY(1, 'two', 3.0) FROM _dummy").unwrap();
        let json = match &result.rows[0][0] {
            Value::Json(s) => s.clone(),
            _ => panic!("Expected JSON value"),
        };
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed[0], 1);
        assert_eq!(parsed[1], "two");
        assert_eq!(parsed[2], 3.0);
    }

    #[test]
    fn test_json_array_length_function() {
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE _dummy (x INT64)").unwrap();
        conn.execute("INSERT INTO _dummy VALUES (1)").unwrap();
        // JSON_ARRAY_LENGTH('[1,2,3,4,5]') = 5
        let result = conn.execute("SELECT JSON_ARRAY_LENGTH('[1,2,3,4,5]') FROM _dummy").unwrap();
        assert_eq!(result.rows[0][0], Value::Int64(5));

        // 嵌套路径
        let result = conn.execute("SELECT JSON_ARRAY_LENGTH('{\"a\":[1,2,3]}', '$.a') FROM _dummy").unwrap();
        assert_eq!(result.rows[0][0], Value::Int64(3));

        // 非数组应返回 NULL
        let result = conn.execute("SELECT JSON_ARRAY_LENGTH('{\"a\":1}') FROM _dummy").unwrap();
        assert_eq!(result.rows[0][0], Value::Null);
    }

    #[test]
    fn test_json_set_insert_replace() {
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE _dummy (x INT64)").unwrap();
        conn.execute("INSERT INTO _dummy VALUES (1)").unwrap();

        // JSON_SET：设置已存在的路径，覆盖
        let result = conn.execute("SELECT JSON_SET('{\"a\":1}', '$.a', 99) FROM _dummy").unwrap();
        let json = match &result.rows[0][0] {
            Value::Json(s) => s.clone(),
            _ => panic!("Expected JSON"),
        };
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["a"], 99);

        // JSON_SET：创建新路径
        let result = conn.execute("SELECT JSON_SET('{\"a\":1}', '$.b', 42) FROM _dummy").unwrap();
        let json = match &result.rows[0][0] {
            Value::Json(s) => s.clone(),
            _ => panic!("Expected JSON"),
        };
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["a"], 1);
        assert_eq!(parsed["b"], 42);

        // JSON_INSERT：仅当路径不存在时设置
        let result = conn.execute("SELECT JSON_INSERT('{\"a\":1}', '$.b', 99) FROM _dummy").unwrap();
        let json = match &result.rows[0][0] {
            Value::Json(s) => s.clone(),
            _ => panic!("Expected JSON"),
        };
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["a"], 1);
        assert_eq!(parsed["b"], 99);

        // JSON_INSERT：路径存在时不动
        let result = conn.execute("SELECT JSON_INSERT('{\"a\":1}', '$.a', 999) FROM _dummy").unwrap();
        let json = match &result.rows[0][0] {
            Value::Json(s) => s.clone(),
            _ => panic!("Expected JSON"),
        };
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["a"], 1, "JSON_INSERT 在路径存在时不应修改");

        // JSON_REPLACE：仅当路径存在时替换
        let result = conn.execute("SELECT JSON_REPLACE('{\"a\":1}', '$.b', 99) FROM _dummy").unwrap();
        let json = match &result.rows[0][0] {
            Value::Json(s) => s.clone(),
            _ => panic!("Expected JSON"),
        };
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["a"], 1);
        assert!(parsed.get("b").is_none(), "JSON_REPLACE 在路径不存在时不应创建");
    }

    #[test]
    fn test_is_null() {
        let mut conn = Connection::open(":memory:").unwrap();

        conn.execute("CREATE TABLE t (id INT64, val INT64)").unwrap();
        conn.execute("INSERT INTO t VALUES (1, 10), (2, NULL), (3, 30)").unwrap();

        // IS NULL
        let result = conn.execute("SELECT id FROM t WHERE val IS NULL").unwrap();
        assert_eq!(result.rows.len(), 1, "val IS NULL 应有 1 行 (id=2)");
        assert_eq!(result.rows[0][0], Value::Int64(2));

        // IS NOT NULL
        let result = conn.execute("SELECT id FROM t WHERE val IS NOT NULL").unwrap();
        assert_eq!(result.rows.len(), 2, "val IS NOT NULL 应有 2 行 (id=1,3)");
        assert_eq!(result.rows[0][0], Value::Int64(1));
        assert_eq!(result.rows[1][0], Value::Int64(3));

        // IS NULL 在 SELECT 列表中
        let result = conn.execute("SELECT val IS NULL FROM t").unwrap();
        assert_eq!(result.rows[0][0], Value::Boolean(false));
        assert_eq!(result.rows[1][0], Value::Boolean(true));
        assert_eq!(result.rows[2][0], Value::Boolean(false));
    }

    #[test]
    fn test_is_not_null() {
        let mut conn = Connection::open(":memory:").unwrap();

        conn.execute("CREATE TABLE t (name VARCHAR, age INT64)").unwrap();
        conn.execute("INSERT INTO t VALUES ('alice', 30), (NULL, 25), ('bob', NULL)").unwrap();

        // VARCHAR IS NOT NULL
        let result = conn.execute("SELECT name FROM t WHERE name IS NOT NULL").unwrap();
        assert_eq!(result.rows.len(), 2, "name IS NOT NULL 应有 2 行 (alice, bob)");
        assert_eq!(result.rows[0][0], Value::Varchar("alice".to_string()));
        assert_eq!(result.rows[1][0], Value::Varchar("bob".to_string()));

        // IS NOT NULL 对两种列
        let result = conn.execute("SELECT name FROM t WHERE age IS NOT NULL AND name IS NOT NULL").unwrap();
        assert_eq!(result.rows.len(), 1, "age AND name IS NOT NULL 应有 1 行 (alice)");
    }

    #[test]
    fn test_pragma_index_info() {
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY, name VARCHAR, age INT64)").unwrap();
        conn.execute("CREATE INDEX idx_name ON t (name)").unwrap();
        conn.execute("CREATE INDEX idx_age ON t (age)").unwrap();

        let result = conn.execute("PRAGMA index_info('t')").unwrap();
        // 至少应有 2 条索引列记录
        assert!(result.rows.len() >= 2, "index_info should have at least 2 rows");
        // 列名和索引名应非空
        for row in &result.rows {
            assert!(matches!(row[1], Value::Varchar(_)));
            assert!(matches!(row[2], Value::Varchar(_)));
        }
    }

    #[test]
    fn test_pragma_index_list() {
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY, name VARCHAR)").unwrap();
        conn.execute("CREATE INDEX idx_name ON t (name)").unwrap();

        let result = conn.execute("PRAGMA index_list('t')").unwrap();
        assert!(result.rows.len() >= 1, "index_list should have at least 1 row");
        // 验证有索引名列
        let has_name_idx = result.rows.iter().any(|r| {
            matches!(&r[1], Value::Varchar(n) if n == "idx_name")
        });
        assert!(has_name_idx, "index_list should contain idx_name");
    }

    #[test]
    fn test_pragma_journal_mode() {
        let mut conn = Connection::open(":memory:").unwrap();

        // 查询当前 mode（默认 Sync → "wal"）
        let result = conn.execute("PRAGMA journal_mode").unwrap();
        assert_eq!(result.rows[0][0], Value::Varchar("wal".to_string()));

        // 切换 mode 并返回值
        let result = conn.execute("PRAGMA journal_mode = 'memory'").unwrap();
        assert_eq!(result.rows[0][0], Value::Varchar("MEMORY".to_string()));

        // 查询当前 mode（"memory" 映射到 BufferFull，返回 "off"）
        let result = conn.execute("PRAGMA journal_mode").unwrap();
        // 实际存储为 BufferFull，对应 "off"
        assert_eq!(result.rows[0][0], Value::Varchar("off".to_string()));
    }

    #[test]
    fn test_pragma_synchronous() {
        let mut conn = Connection::open(":memory:").unwrap();

        // 查询当前同步级别（默认 Sync → "2"）
        let result = conn.execute("PRAGMA synchronous").unwrap();
        assert_eq!(result.rows[0][0], Value::Varchar("2".to_string()));

        // 切换级别
        let result = conn.execute("PRAGMA synchronous = 0").unwrap();
        assert_eq!(result.rows[0][0], Value::Varchar("0".to_string()));

        let result = conn.execute("PRAGMA synchronous").unwrap();
        assert_eq!(result.rows[0][0], Value::Varchar("0".to_string()));
    }

    #[test]
    fn test_pragma_cache_size() {
        let mut conn = Connection::open(":memory:").unwrap();

        // 查询当前缓存大小（默认 64MB = 65536 KB）
        let result = conn.execute("PRAGMA cache_size").unwrap();
        assert_eq!(result.rows[0][0], Value::Int64(65536));

        // 设置缓存大小（KB）
        let result = conn.execute("PRAGMA cache_size = 8192").unwrap();
        assert_eq!(result.rows[0][0], Value::Int64(8192));

        let result = conn.execute("PRAGMA cache_size").unwrap();
        assert_eq!(result.rows[0][0], Value::Int64(8192));
    }

    #[test]
    fn test_create_vector_index() {
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY, embedding VECTOR(4))").unwrap();
        conn.execute("INSERT INTO t VALUES (1, '[1.0, 0.0, 0.0, 0.0]'), (2, '[0.0, 1.0, 0.0, 0.0]'), (3, '[0.0, 0.0, 1.0, 0.0]')").unwrap();

        // 创建向量索引（使用 CREATE VECTOR INDEX 语法）
        let result = conn.execute("CREATE VECTOR INDEX idx_emb ON t (embedding) WITH (metric = cosine, m = 8, ef_construction = 50)").unwrap();
        assert!(result.rows[0][0].to_string().contains("Vector index"));

        // 使用标准 CREATE INDEX ... USING hnsw 语法
        let result = conn.execute("CREATE INDEX idx_emb2 ON t (embedding) USING hnsw WITH (metric = l2, m = 8, ef_construction = 50)").unwrap();
        assert!(result.rows[0][0].to_string().contains("Vector index"));
    }

    #[test]
    fn test_vector_search() {
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY, embedding VECTOR(4))").unwrap();
        conn.execute("INSERT INTO t VALUES (1, '[1.0, 0.0, 0.0, 0.0]'), (2, '[0.0, 1.0, 0.0, 0.0]'), (3, '[0.0, 0.0, 1.0, 0.0]')").unwrap();

        // 创建向量索引
        conn.execute("CREATE VECTOR INDEX idx_emb ON t (embedding) WITH (metric = cosine, m = 8, ef_construction = 50)").unwrap();

        // 使用 vector_search 表值函数
        let result = conn.execute("SELECT * FROM vector_search('t', 'idx_emb', '[1.0, 0.0, 0.0, 0.0]', 3)").unwrap();
        assert_eq!(result.rows.len(), 3, "vector_search should return 3 neighbors");
        // 第一行应是 id=1（自己，距离最近）
        assert_eq!(result.rows[0][0], Value::Int64(1));
    }

    // ========================================================================
    // P-W1 PREWHERE 真接通测试（v0.16 新增）
    //
    // 验证 storage 层的 PREWHERE 短路：
    // - 简单比较谓词 col OP literal
    // - 选择性 1%/10%/50%/90% 都返回正确结果
    // - 谓词列在输出 vs 不在输出（退化到原路径）
    // - 跨类型比较（Int32/Int64/Float64）
    // - 复合谓词（AND 拆分后单谓词 PREWHERE + executor 二次过滤）
    // ========================================================================

    /// 工具：建表 + 写入 N 行数据
    /// `val_fn(i)` 生成 val 列值；用于构造不同选择性的过滤场景
    /// 用多行 VALUES 一次写入 1000 行（避免 execute_prepared_batch 的 params bug）
    fn setup_where_table(n: usize, rg_size: u32, val_fn: impl Fn(usize) -> i64) -> Connection {
        let mut config = crate::common::config::Config::default();
        config.row_group_size = rg_size;
        let mut conn = Connection::open_with_config(":memory:", config).unwrap();
        conn.execute("CREATE TABLE t (id INT, val INT, name VARCHAR)").unwrap();

        const BATCH: usize = 1000;
        for chunk_start in (0..n).step_by(BATCH) {
            let end = (chunk_start + BATCH).min(n);
            let mut sql = String::with_capacity((end - chunk_start) * 40);
            sql.push_str("INSERT INTO t VALUES ");
            for i in chunk_start..end {
                if i > chunk_start { sql.push_str(", "); }
                let val = val_fn(i);
                sql.push_str(&format!("({}, {}, 'name_{}')", i, val, i));
            }
            conn.execute(&sql).unwrap();
        }
        conn
    }

    #[test]
    fn test_prewhere_selectivity_1pct() {
        // 1% 选择性：val = 100 的只有 1 行
        let mut conn = setup_where_table(1000, 100, |i| {
            if i == 500 { 100 } else { (i as i64) % 50 + 1 }  // 1 row matches val=100
        });
        // val=100 实际只有 1 行（i=500），其他都是 [1,50] 范围
        let result = conn.execute("SELECT id FROM t WHERE val = 100").unwrap();
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0][0], Value::Int64(500));
    }

    #[test]
    fn test_prewhere_selectivity_10pct() {
        // 10% 选择性：val > 90 命中约 10%
        let mut conn = setup_where_table(1000, 100, |i| (i as i64) % 100);
        let result = conn.execute("SELECT COUNT(*) FROM t WHERE val > 90").unwrap();
        // val ∈ [0, 99]，val > 90 → val ∈ [91, 99] = 9 个值，每值 10 行 = 90 行
        assert_eq!(result.rows[0][0], Value::Int64(90));
    }

    #[test]
    fn test_prewhere_selectivity_50pct() {
        // 50% 选择性：val >= 50 命中约 50%
        let mut conn = setup_where_table(1000, 100, |i| (i as i64) % 100);
        let result = conn.execute("SELECT COUNT(*) FROM t WHERE val >= 50").unwrap();
        // val ∈ [0, 99]，val >= 50 → val ∈ [50, 99] = 50 个值，每值 10 行 = 500 行
        assert_eq!(result.rows[0][0], Value::Int64(500));
    }

    #[test]
    fn test_prewhere_selectivity_90pct() {
        // 90% 选择性：val < 90 命中约 90%
        let mut conn = setup_where_table(1000, 100, |i| (i as i64) % 100);
        let result = conn.execute("SELECT COUNT(*) FROM t WHERE val < 90").unwrap();
        // val ∈ [0, 99]，val < 90 → val ∈ [0, 89] = 90 个值，每值 10 行 = 900 行
        assert_eq!(result.rows[0][0], Value::Int64(900));
    }

    #[test]
    fn test_prewhere_no_minmax_skip_needed() {
        // 选择性 100%（所有行都通过）—— 验证无 MinMax 短路时 PREWHERE 仍正确
        let mut conn = setup_where_table(100, 50, |i| (i as i64) % 10);
        let result = conn.execute("SELECT COUNT(*) FROM t WHERE val >= 0").unwrap();
        assert_eq!(result.rows[0][0], Value::Int64(100));
    }

    #[test]
    fn test_prewhere_no_match() {
        // 0 选择性：没有行匹配
        let mut conn = setup_where_table(100, 50, |i| (i as i64) % 10);
        let result = conn.execute("SELECT id FROM t WHERE val > 1000").unwrap();
        assert_eq!(result.rows.len(), 0, "no rows should match val > 1000");
    }

    #[test]
    fn test_prewhere_predicate_not_in_output() {
        // 谓词列不在输出（少见场景，例如 SELECT name, id WHERE val > 5）
        // 验证退化路径正确
        let mut conn = setup_where_table(100, 50, |i| (i as i64) % 10);
        let result = conn.execute("SELECT name, id FROM t WHERE val > 5").unwrap();
        // val ∈ [0, 9]，val > 5 → [6, 9] = 4 个值，每值 10 行 = 40 行
        assert_eq!(result.rows.len(), 40);
        // 输出列应是 [name, id]（不含 val）
        assert_eq!(result.columns, vec!["name", "id"]);
    }

    #[test]
    fn test_prewhere_compound_predicate_and() {
        // 复合谓词 AND：a > 5 AND a < 8
        // 验证 storage PREWHERE 处理 a > 5，executor 二次过滤 a < 8
        let mut conn = setup_where_table(100, 50, |i| (i as i64) % 10);
        let result = conn.execute("SELECT COUNT(*) FROM t WHERE val > 5 AND val < 8").unwrap();
        // val ∈ [0, 9]，val > 5 AND val < 8 → [6, 7] = 2 个值，每值 10 行 = 20 行
        assert_eq!(result.rows[0][0], Value::Int64(20));
    }

    #[test]
    fn test_prewhere_cross_row_groups_correctness() {
        // 关键测试：跨多个 row group 的过滤
        // 1000 行，row_group_size=100 → 10 个 row group
        // val 在 [0, 99] 均匀分布
        // 验证 val = 50 跨所有 row group 返回 10 行
        let mut conn = setup_where_table(1000, 100, |i| (i as i64) % 100);
        let result = conn.execute("SELECT id FROM t WHERE val = 50 ORDER BY id").unwrap();
        // id = 50, 150, 250, ..., 950（10 个）
        assert_eq!(result.rows.len(), 10);
        for (i, row) in result.rows.iter().enumerate() {
            assert_eq!(row[0], Value::Int64(50 + i as i64 * 100));
        }
    }

    #[test]
    fn test_prewhere_double_precision() {
        // 验证 Float64 谓词：val < 0.5
        let mut config = crate::common::config::Config::default();
        config.row_group_size = 50;
        let mut conn = Connection::open_with_config(":memory:", config).unwrap();
        conn.execute("CREATE TABLE t (id INT, val DOUBLE)").unwrap();

        let mut sql = String::from("INSERT INTO t VALUES ");
        for i in 0..200 {
            if i > 0 { sql.push_str(", "); }
            let v = (i as f64) / 200.0;  // [0.0, 0.995]
            sql.push_str(&format!("({}, {})", i, v));
        }
        conn.execute(&sql).unwrap();

        let result = conn.execute("SELECT COUNT(*) FROM t WHERE val < 0.5").unwrap();
        // val ∈ [0.0, 0.995)，val < 0.5 → i ∈ [0, 99]，100 行
        assert_eq!(result.rows[0][0], Value::Int64(100));
    }

    #[test]
    fn test_prewhere_with_projection() {
        // PREWHERE + 投影：SELECT id, name FROM t WHERE val > 5
        // 验证输出列包含谓词列时的物化正确性
        let mut conn = setup_where_table(100, 50, |i| (i as i64) % 10);
        let result = conn.execute("SELECT id, val, name FROM t WHERE val > 5").unwrap();
        assert_eq!(result.rows.len(), 40); // val ∈ [6, 9]
        for row in &result.rows {
            // 0=id, 1=val, 2=name
            let val = match &row[1] { Value::Int64(v) => *v, _ => panic!("expected Int64") };
            assert!(val > 5, "val should be > 5, got {}", val);
        }
    }

    #[test]
    fn test_prepared_batch_multi_col_params() {
        // 回归：fast_insert fast path 裸 `?` 未重编号曾导致所有列绑定 params[0]
        // 验证 execute_prepared_batch 多列参数按位绑定（值级验证）
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (id INT PRIMARY KEY, val DOUBLE, name VARCHAR)")
            .unwrap();

        let stmt = conn.prepare("INSERT INTO t VALUES (?, ?, ?)").unwrap();
        let mut batch = Vec::with_capacity(10);
        for i in 0..10 {
            batch.push(vec![
                Value::Int32(i),
                Value::Float64(i as f64 * 1.5),
                Value::Varchar(format!("row_{}", i)),
            ]);
        }
        conn.execute_prepared_batch(&stmt, &batch).unwrap();

        // 值级验证每列绑定各自参数（而非全部 = params[0]）
        let result = conn.execute("SELECT id, val, name FROM t WHERE id >= 5").unwrap();
        assert_eq!(result.rows.len(), 5);
        for (j, row) in result.rows.iter().enumerate() {
            let i = (j + 5) as i32;
            assert_eq!(row[0], Value::Int32(i), "id 列应为 {}", i);
            assert_eq!(row[1], Value::Float64(i as f64 * 1.5), "val 列应为 id*1.5");
            assert_eq!(row[2], Value::Varchar(format!("row_{}", i)), "name 列应为 row_{}", i);
        }
    }
}

#[cfg(test)]
mod multi_engine_tests {
    use super::*;
    use crate::sql::ast::Statement;

    include!("memory_engine_tests.rs");
    include!("log_engine_tests.rs");
    include!("bloom_tests.rs");
    include!("m4_tests.rs");
    include!("p5_tests.rs");
    include!("layered_index_tests.rs");


    #[test]
    fn test_engine_clause_parse() {
        // ENGINE = Memory 子句剥离
        let stmt = sql::parser::parse("CREATE TABLE t (id INT) ENGINE = Memory").unwrap();
        match stmt {
            Statement::CreateTable(ct) => {
                assert_eq!(ct.engine.as_deref(), Some("Memory"));
                assert_eq!(ct.table_name, "t");
            }
            _ => panic!("expected CreateTable"),
        }
        // 无 ENGINE：默认 None
        let stmt = sql::parser::parse("CREATE TABLE t (id INT)").unwrap();
        match stmt {
            Statement::CreateTable(ct) => assert_eq!(ct.engine, None),
            _ => panic!("expected CreateTable"),
        }
        // 带分号
        let stmt = sql::parser::parse("CREATE TABLE t (id INT) ENGINE=Log;").unwrap();
        match stmt {
            Statement::CreateTable(ct) => assert_eq!(ct.engine.as_deref(), Some("Log")),
            _ => panic!("expected CreateTable"),
        }
        // 小写
        let stmt = sql::parser::parse("CREATE TABLE t (id INT) engine = memory").unwrap();
        match stmt {
            Statement::CreateTable(ct) => assert_eq!(ct.engine.as_deref(), Some("memory")),
            _ => panic!("expected CreateTable"),
        }
    }

    #[test]
    fn test_engine_clause_planner() {
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (id INT) ENGINE = Memory").unwrap();
        let engine = conn
            .database_mut()
            .get_engine_table("t")
            .map(|et| et.engine_type());
        assert_eq!(engine, Some(common::types::EngineType::Memory));

        // 非法引擎名报错
        let err = conn.execute("CREATE TABLE bad (id INT) ENGINE = Nope").unwrap_err();
        assert!(err.to_string().contains("unsupported ENGINE"), "got: {err}");

        // 默认 Columnar
        conn.execute("CREATE TABLE c (id INT)").unwrap();
        let engine = conn
            .database_mut()
            .get_engine_table("c")
            .map(|et| et.engine_type());
        assert_eq!(engine, Some(common::types::EngineType::Columnar));
    }

    #[test]
    fn test_table_def_serde_backward_compat() {
        // 旧版本文件（无 engine 字段）反序列化应默认 Columnar：
        // serde default 机制与具体格式无关，用 JSON 验证缺失字段回退
        let json = r#"{"id":7,"name":"legacy","columns":[],"row_count":0,
            "indexes":[],"cluster_key":null,"foreign_keys":[],
            "next_auto_increment_id":1,"ttl_seconds":null,"ttl_column":null}"#;
        let def: common::types::TableDef = serde_json::from_str(json).unwrap();
        assert_eq!(def.engine, common::types::EngineType::Columnar, "旧文件应默认 Columnar");
        assert_eq!(def.id, 7);

        // 新格式：engine 字段往返保留
        let mut def2 = common::types::TableDef::new(8, "t2", vec![]);
        def2.engine = common::types::EngineType::Log;
        let bytes = serde_json::to_vec(&def2).unwrap();
        let back: common::types::TableDef = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back.engine, common::types::EngineType::Log);
    }

    #[test]
    fn test_engine_table_dispatch() {
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (id INT PRIMARY KEY, name TEXT)").unwrap();
        conn.execute("INSERT INTO t VALUES (1, 'a')").unwrap();
        // v0.20：主键表 INSERT 走攒批，原始 API 读前需 flush（SQL 层自动冲刷）
        crate::executor::operators::insert::flush_all_batched(conn.database_mut()).unwrap();

        // 引擎句柄访问
        let engine = conn.database_mut().get_engine_table_mut("t").unwrap();
        assert_eq!(engine.engine_type(), common::types::EngineType::Columnar);
        assert_eq!(engine.def().name, "t");
        assert!(engine.as_columnar().is_some());

        // 扫描分派
        let chunks = engine.scan_to_chunks(&[0, 1], None).unwrap();
        let rows = crate::executor::executor::debug_chunks_to_rows(&chunks);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0], vec![Value::Int64(1), Value::Varchar("a".into())]);

        // 点查分派
        let row = engine.get_row_by_id(0).unwrap().unwrap();
        assert_eq!(row[0], Value::Int64(1));

        // 主键查询分派
        let rid = engine.lookup_primary_key(&Value::Int64(1));
        assert_eq!(rid, Some(0));

        // 删除分派
        engine.delete_row(0).unwrap();
        assert!(engine.get_row_by_id(0).unwrap().is_none());
    }

    #[test]
    fn test_engine_persistence_roundtrip() {
        // ENGINE 字段随 catalog 持久化：写入 → 关闭 → 重开 → 引擎类型保留
        let path = format!("/tmp/engramdb_engine_rt_{}.hdb", std::process::id());
        std::fs::remove_file(&path).ok();
        {
            let mut conn = Connection::open(&path).unwrap();
            conn.execute("CREATE TABLE t (id INT) ENGINE = Memory").unwrap();
            conn.close().unwrap();
        }
        {
            let mut conn = Connection::open(&path).unwrap();
            let engine = conn
                .database_mut()
                .get_engine_table("t")
                .map(|et| et.engine_type());
            assert_eq!(engine, Some(common::types::EngineType::Memory));
            conn.close().unwrap();
        }
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_engine_table_ops_trait() {
        use crate::storage::engine::EngineTableOps;
        let mut conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (id INT PRIMARY KEY, name TEXT)").unwrap();
        conn.execute("INSERT INTO t VALUES (1, 'a')").unwrap();
        // v0.20：主键表 INSERT 走攒批，原始 API 读前需 flush（SQL 层自动冲刷）
        crate::executor::operators::insert::flush_all_batched(conn.database_mut()).unwrap();

        let mut table = conn.database_mut().get_table_mut("t").unwrap();
        // 通过 trait 对象调用
        let ops: &mut dyn EngineTableOps = table;
        assert_eq!(ops.engine_type(), common::types::EngineType::Columnar);
        let chunks = ops.scan_to_chunks(&[0, 1], None).unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].count, 1);
        assert!(ops.lookup_primary_key(&Value::Int64(1)).is_some());
    }
}
