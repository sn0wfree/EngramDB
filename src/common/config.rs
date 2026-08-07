//! 数据库配置

/// WAL 刷盘策略
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum WalFlushMode {
    /// 每次提交都 fsync（最安全，默认）
    Sync = 0,
    /// 缓冲区满才刷盘，提交不主动 fsync（性能最好，崩溃可能丢最后一个缓冲区的数据）
    BufferFull = 1,
    /// 周期性刷盘（由外部调用 flush 控制，介于两者之间）
    Periodic = 2,
}

/// 事务隔离级别
///
/// 控制事务之间的可见性和隔离程度。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsolationLevel {
    /// 读未提交（当前未实现）
    ReadUncommitted,
    /// 快照隔离（MVCC，当前默认）
    ///
    /// 事务看到的是事务开始时的数据快照，其他事务的修改不可见。
    /// 写写冲突检测：如果两个事务同时修改同一行，后提交者会失败。
    SnapshotIsolation,
    /// 可序列化（当前未实现）
    Serializable,
}

impl Default for IsolationLevel {
    fn default() -> Self {
        IsolationLevel::SnapshotIsolation
    }
}

/// WAL 压缩算法
///
/// 压缩 WAL payload 可以减少 I/O 量，加速 fsync，同时减少 WAL 文件大小。
/// 对于文本密集型负载（如 Agent 消息记录），压缩比可达 3:1 到 5:1。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum WalCompression {
    /// 不压缩（默认）
    None = 0,
    /// Snappy 压缩（速度快，压缩比适中）
    ///
    /// 压缩比：文本 ~2:1，随机数据 ~1:1
    /// 速度：压缩 ~500MB/s，解压 ~1GB/s
    Snappy = 1,
}

/// Delta 合并策略
///
/// 控制 DeltaStore 数据何时、以多大粒度合并到 ColumnStore。
/// 四种策略可按需选择，也可以在运行时动态切换。
#[derive(Debug, Clone, Copy)]
pub enum CompactStrategy {
    /// 手动策略：写入路径完全不触发合并
    ///
    /// 由用户通过 `Connection::compact()` / `compact_all()` 显式调用。
    /// 适合：批量导入、ETL 作业、高级用户完全掌控调度。
    Manual,

    /// 全量合并：Delta 达到阈值时一次性全部合并
    ///
    /// 优点：合并次数最少、总开销最低。
    /// 缺点：大表合并时阻塞时间长（可能几十到几百毫秒）。
    /// 适合：写入量小、对延迟不敏感的场景。
    Full {
        /// 触发阈值（Delta 行数）
        threshold: usize,
    },

    /// 增量式合并：Delta 达到阈值时，每次只合并 batch_size 行
    ///
    /// 优点：单次阻塞时间可控，写入延迟平稳。
    /// 缺点：总合并次数更多，整体开销略高。
    /// 适合：交互式工作负载，对延迟稳定性要求高。
    Incremental {
        /// 触发阈值（Delta 行数）
        threshold: usize,
        /// 每次合并的行数
        batch_size: usize,
    },

    /// 自适应分桶（默认策略）
    ///
    /// 触发阈值随表大小自适应：
    ///   threshold = clamp(row_count * pct_of_table, min_threshold, max_threshold)
    /// 每次合并 batch_size 行（增量式）。
    ///
    /// 优点：自动适配不同表大小，小表频繁快合并、大表有上限不膨胀。
    /// 缺点：实现稍复杂（但对用户透明）。
    /// 适合：通用场景，默认推荐。
    Adaptive {
        /// 最小触发阈值（行数下限）
        min_threshold: usize,
        /// 最大触发阈值（行数上限）
        max_threshold: usize,
        /// 占表总行数的比例（0.0 - 1.0）
        pct_of_table: f64,
        /// 每次合并的行数
        batch_size: usize,
    },
}

impl CompactStrategy {
    /// 默认策略：自适应分桶
    ///
    /// - min_threshold: 10,000 行
    /// - max_threshold: 122,880 行（一个 Row Group）
    /// - pct_of_table: 10%
    /// - batch_size: 122,880 行（一个 Row Group）
    pub fn default_adaptive(row_group_size: usize) -> Self {
        CompactStrategy::Adaptive {
            min_threshold: 10_000,
            max_threshold: row_group_size,
            pct_of_table: 0.10,
            batch_size: row_group_size,
        }
    }

    /// 便捷构造：手动策略
    pub fn manual() -> Self { CompactStrategy::Manual }

    /// 便捷构造：全量合并策略
    pub fn full(threshold: usize) -> Self { CompactStrategy::Full { threshold } }

    /// 便捷构造：增量式策略
    pub fn incremental(threshold: usize, batch_size: usize) -> Self {
        CompactStrategy::Incremental { threshold, batch_size }
    }
}

/// 数据库配置
#[derive(Debug, Clone)]
pub struct Config {
    /// 页大小（字节）
    pub page_size: u32,
    /// 块大小（字节）
    pub block_size: u32,
    /// 缓冲池大小（页数）
    pub buffer_pool_size: usize,
    /// Row Group 行数
    pub row_group_size: u32,
    /// WAL 自动 Checkpoint 阈值（字节）
    pub wal_checkpoint_threshold: u64,
    /// WAL 刷盘策略
    pub wal_flush_mode: WalFlushMode,
    /// WAL 缓冲区大小（字节）
    pub wal_buffer_size: usize,
    /// WAL 压缩算法（预留，暂未实现）
    pub wal_compression: WalCompression,
    /// WAL 组提交：每 N 次 commit 做一次 fsync（0 = 禁用，即每次 commit 都 fsync）
    ///
    /// 组提交是 Sync 模式下的核心 WAL 加速机制：
    /// 多条事务共享一次 fsync，写入吞吐可提升数倍至数十倍，
    /// 同时保持「已 fsync 的数据绝对不丢」的持久化保证。
    ///
    /// Perf04：默认 16 次提交 fsync 一次（配合 64KB 字节阈值兜底），
    /// 实测事务写入吞吐提升 5~10×。
    ///
    /// - 崩溃时最多丢 `wal_group_commit_size` 条未 fsync 的事务；
    /// - 要最严格 100% 不丢数据：设为 0（每次 fsync）；
    /// - 关键节点可主动调用 `Connection::sync_wal()` 强制刷盘。
    pub wal_group_commit_size: usize,
    /// WAL 组提交：缓冲区达到多少字节时强制 fsync（0 表示不按大小触发）
    ///
    /// 与 wal_group_commit_size 配合，任一条件满足即触发 fsync。
    /// 默认 64KB，平衡延迟与吞吐。
    pub wal_group_commit_max_bytes: usize,
    /// WAL 组提交：距上次 fsync 超过该毫秒数后，下次 commit 强制 fsync（0 = 禁用）
    ///
    /// P0-3 时间窗兜底：低流量场景下 count/bytes 阈值迟迟不触发时，
    /// 数据停留在 page cache 的时间被限定在约该毫秒数内（延迟有界）。
    /// 默认 10ms：高吞吐时提交间隔远小于 10ms，不干扰 count/bytes 触发。
    pub wal_group_commit_timeout_ms: u64,
    /// P0-2 INSERT 攒批合并总开关（autocommit 逐行 INSERT 合批落盘）
    pub wal_batch_insert: bool,
    /// P0-2 Batcher：单表缓冲行数阈值（达到即 flush 该表）
    pub insert_batch_rows: usize,
    /// P0-2 Batcher：单表缓冲字节估算阈值（达到即 flush 该表）
    pub insert_batch_bytes: usize,
    /// P0-2 Batcher：时间窗（首行入批起算，达到即 flush 该表；0 = 禁用时间触发）
    pub insert_batch_timeout_ms: u64,
    /// P0-2 事务级 Batcher 总开关（显式事务内 INSERT 攒批，COMMIT/读时 flush）
    ///
    /// 开启后：显式事务内连续 INSERT 攒入事务私有 buffer（零 WAL/MVCC/Delta
    /// 开销），在非裸 INSERT 语句 / SAVEPOINT / COMMIT 前一次性 flush 为
    /// 单个内部批量事务（1 条 WAL InsertBatch + 1 次 MVCC batch_write）。
    /// ROLLBACK / ROLLBACK TO SAVEPOINT 直接丢弃 buffer（未读过的写入段可回滚）。
    /// 关闭后：事务内每条 INSERT 各自走内部事务（v0.18 前行为）。
    pub txn_batch_enabled: bool,
    /// 事务级 Batcher：事务 buffer 总行数阈值（达到即提前 flush，防内存无界）
    pub txn_batch_rows: usize,
    /// 事务级 Batcher：有约束的表（NOT NULL / 主键 / 唯一索引 / 自增 /
    /// TTL / 外键）是否跳过攒批（true = 跳过，约束错误在语句时即时暴露）
    ///
    /// 约束错误（如 PK 冲突）在攒批路径下会推迟到 flush 时暴露；
    /// 需要语句时即时报错的应用应保持 true。
    pub txn_batch_bypass_constraint_tables: bool,
    /// P1-5 LogEngine 块行数（0 = 默认 8192；MinMax 跳读粒度 / 序列化块头摊销）
    ///
    /// 大块（32K/64K）减少冻结/序列化次数与块头开销，但时间范围跳读粒度变粗。
    /// 注意：仅影响新建块；已落盘块按原格式读取，块大小不写入文件。
    pub log_block_rows: usize,
    /// 默认压缩算法
    pub default_compression: CompressionType,
    /// v0.21：统一 Tokenizer 词表文件路径（配置后 Varchar 列启用 TokenDelta 压缩）
    pub tokenizer_path: Option<String>,
    /// 列存持久化时是否启用压缩（v0.12.x 压缩接线）
    ///
    /// 开启后：checkpoint 时对列存调用 `compress_all`（内存中按 RowGroup 压缩），
    /// `save_data` 将压缩后的字节直接落盘；`load_data` 以压缩态惰性加载，
    /// `read_column` 在首次访问时解压。`append_*` 在追加到已压缩 RowGroup 前自动解压。
    /// 关闭后：保持旧行为（裸存，运行时压缩率 1.0x）。
    pub compress_on_persist: bool,
    /// Delta 合并策略（数据库级默认）
    ///
    /// 新建的表默认使用此策略，也可以在表级别单独设置。
    pub compact_strategy: CompactStrategy,
    /// Periodic WAL 模式下，sync_wal() 是否联动触发 compact
    ///
    /// 仅在 wal_flush_mode = Periodic 时生效。
    /// 开启后，sync_wal() 会在刷盘后检查所有表是否需要合并。
    pub sync_wal_compact: bool,
    /// 分层索引：compact 时是否按主键排序写入列存（默认 true）
    ///
    /// 开启后固化段内主键有序，稀疏索引段内可二分定位（点查 ~100ns 级）；
    /// 关闭后段内无序，稀疏索引段内线性确认（点查 ~2-5µs，但 compact 零排序开销）。
    pub sort_compact_by_pk: bool,
    /// 分层索引：保留旧的全表稠密 BTreeMap 主键索引（默认 false）
    ///
    /// false（默认）：主键点查走「Delta 稠密 + 列存稀疏」分层索引，内存节省 ~99.9%
    /// （1 亿行主键索引 4.6GB → <1MB）；true：额外维护全表 BTreeMap（向后兼容/回退用）。
    pub primary_index_legacy: bool,
    /// 分层索引：列存稀疏索引 granule 行数（默认 8192，与 ClickHouse 一致）
    pub sparse_index_granule_rows: u32,
    
    /// 是否启用事务支持（默认 true）
    ///
    /// - true: SQL INSERT/UPDATE/DELETE 走 WAL + MVCC 事务路径，保证 ACID
    /// - false: 直接写存储层，跳过 WAL/MVCC，性能最优（适合批量导入、临时分析）
    ///
    /// **适用场景**：
    /// - enable_transaction=true：生产环境、需要崩溃恢复、多事务并发
    /// - enable_transaction=false：批量导入、离线分析、临时数据库
    ///
    /// CLI 控制：通过 `--no-transaction` 参数可在启动时关闭事务。
    pub enable_transaction: bool,
    
    /// 事务隔离级别（仅 enable_transaction=true 时生效）
    pub default_isolation_level: IsolationLevel,
}

/// 压缩类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[repr(u8)]
pub enum CompressionType {
    Uncompressed = 0,
    Rle = 1,
    BitPacking = 2,
    Dictionary = 3,
    For = 4,
    Delta = 5,
    Zstd = 6,
    Gorilla = 7,
    ForBitPack = 8,
    BooleanPack = 9,
    DoubleDelta = 10,
    /// TokenDelta（统一 Tokenizer + 前缀 delta + 熵编码，v0.21）
    TokenDelta = 11,
}

impl Default for Config {
    fn default() -> Self {
        let row_group_size = 122_880; // 120K rows
        Self {
            page_size: 4096,
            block_size: 262_144, // 256KB
            buffer_pool_size: 1024, // 4MB (1024 * 4KB)
            row_group_size,
            wal_checkpoint_threshold: 16 * 1024 * 1024, // 16MB
            wal_flush_mode: WalFlushMode::Sync,
            wal_buffer_size: 65536, // 64KB
            wal_compression: WalCompression::None,
            wal_group_commit_size: 16, // Perf04：默认组提交，吞吐提升 5~10×（可通过 0 关闭）
            wal_group_commit_max_bytes: 65536, // 64KB，按大小兜底
            wal_group_commit_timeout_ms: 10, // P0-3：距上次 fsync 超 10ms 则下次 commit 强制 sync（低流量延迟有界）
            wal_batch_insert: true, // P0-2：autocommit INSERT 攒批合并（可通过 0 关闭）
            insert_batch_rows: 1024, // P0-2：满 1024 行 flush
            insert_batch_bytes: 65536, // P0-2：满 64KB 估算字节 flush
            insert_batch_timeout_ms: 10, // P0-2：首行入批 10ms 后 flush（低流量延迟有界）
            txn_batch_enabled: true, // P0-2 事务级 Batcher：默认开启
            txn_batch_rows: 8192, // 事务 buffer 满 8192 行提前 flush（内存保护）
            txn_batch_bypass_constraint_tables: true, // 有约束的表跳过攒批（错误即时暴露）
            log_block_rows: 0, // P1-5：0 = 默认 8192 行/块
            default_compression: CompressionType::Uncompressed,
            compress_on_persist: true,
            tokenizer_path: None, // v0.21：统一 Tokenizer 词表文件路径（配置后 Varchar 列启用 TokenDelta 压缩）
            compact_strategy: CompactStrategy::default_adaptive(row_group_size as usize),
            sync_wal_compact: true,
            sort_compact_by_pk: true,
            primary_index_legacy: false,
            sparse_index_granule_rows: 8192,
            enable_transaction: true,
            default_isolation_level: IsolationLevel::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config_values() {
        let cfg = Config::default();
        assert_eq!(cfg.page_size, 4096);
        assert_eq!(cfg.block_size, 262_144);
        assert_eq!(cfg.buffer_pool_size, 1024);
        assert_eq!(cfg.row_group_size, 122_880);
        assert_eq!(cfg.wal_flush_mode, WalFlushMode::Sync);
        assert_eq!(cfg.wal_group_commit_size, 16);
        assert_eq!(cfg.wal_group_commit_max_bytes, 65536);
        assert_eq!(cfg.wal_group_commit_timeout_ms, 10);
        assert!(cfg.wal_batch_insert);
        assert_eq!(cfg.insert_batch_rows, 1024);
        assert_eq!(cfg.insert_batch_timeout_ms, 10);
        assert!(cfg.txn_batch_enabled);
        assert_eq!(cfg.txn_batch_rows, 8192);
        assert!(cfg.txn_batch_bypass_constraint_tables);
        assert_eq!(cfg.log_block_rows, 0);
        assert_eq!(cfg.default_compression, CompressionType::Uncompressed);
        assert!(cfg.compress_on_persist);
        assert!(cfg.enable_transaction);
        assert_eq!(cfg.default_isolation_level, IsolationLevel::SnapshotIsolation);
    }

    #[test]
    fn test_config_custom_values_applied() {
        let mut cfg = Config::default();
        cfg.wal_batch_insert = false;
        cfg.insert_batch_rows = 4096;
        cfg.insert_batch_timeout_ms = 0;
        cfg.txn_batch_enabled = false;
        cfg.txn_batch_rows = 512;
        cfg.txn_batch_bypass_constraint_tables = false;
        cfg.log_block_rows = 32768;
        cfg.wal_group_commit_size = 0;
        assert!(!cfg.wal_batch_insert);
        assert_eq!(cfg.insert_batch_rows, 4096);
        assert_eq!(cfg.insert_batch_timeout_ms, 0);
        assert!(!cfg.txn_batch_enabled);
        assert_eq!(cfg.txn_batch_rows, 512);
        assert!(!cfg.txn_batch_bypass_constraint_tables);
        assert_eq!(cfg.log_block_rows, 32768);
        assert_eq!(cfg.wal_group_commit_size, 0);
        // clone 后修改不影响原配置
        let mut cloned = cfg.clone();
        cloned.wal_batch_insert = true;
        assert!(!cfg.wal_batch_insert);
        assert!(cloned.wal_batch_insert);
    }

    #[test]
    fn test_isolation_level_default() {
        assert_eq!(IsolationLevel::default(), IsolationLevel::SnapshotIsolation);
    }

    #[test]
    fn test_compact_strategy_constructors() {
        let adaptive = CompactStrategy::default_adaptive(8192);
        match adaptive {
            CompactStrategy::Adaptive { min_threshold, max_threshold, pct_of_table, batch_size } => {
                assert_eq!(min_threshold, 10_000);
                assert_eq!(max_threshold, 8192);
                assert!((pct_of_table - 0.10).abs() < 1e-9);
                assert_eq!(batch_size, 8192);
            }
            _ => panic!("default 应为 Adaptive"),
        }
        assert!(matches!(CompactStrategy::manual(), CompactStrategy::Manual));
        assert!(matches!(CompactStrategy::full(100), CompactStrategy::Full { threshold: 100 }));
        assert!(matches!(CompactStrategy::incremental(100, 10), CompactStrategy::Incremental { threshold: 100, batch_size: 10 }));
    }
}
