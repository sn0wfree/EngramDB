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
    /// WAL 组提交：每 N 次 commit 做一次 fsync（0 表示禁用，即每次 commit 都 fsync）
    ///
    /// 组提交是 Sync 模式下的核心 WAL 加速机制：
    /// 多条事务共享一次 fsync，写入吞吐可提升数倍至数十倍，
    /// 同时保持「已 fsync 的数据绝对不丢」的持久化保证。
    ///
    /// 典型场景（AI Agent 交互存储）：
    /// - group_commit_size = 8~32，吞吐提升 5~20x
    /// - 崩溃时最多丢 group_commit_size 条未 fsync 的事务
    /// - 配合 sync_wal() 可在关键节点强制刷盘
    pub wal_group_commit_size: usize,
    /// WAL 组提交：缓冲区达到多少字节时强制 fsync（0 表示不按大小触发）
    ///
    /// 与 wal_group_commit_size 配合，任一条件满足即触发 fsync。
    /// 默认 64KB，平衡延迟与吞吐。
    pub wal_group_commit_max_bytes: usize,
    /// 默认压缩算法
    pub default_compression: CompressionType,
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
            wal_group_commit_size: 0, // 默认关闭，每次 commit 都 fsync（最安全）
            wal_group_commit_max_bytes: 65536, // 64KB，按大小兜底
            default_compression: CompressionType::Uncompressed,
            compress_on_persist: true,
            compact_strategy: CompactStrategy::default_adaptive(row_group_size as usize),
            sync_wal_compact: true,
        }
    }
}
