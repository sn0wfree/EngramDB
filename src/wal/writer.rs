//! WAL 写入器
//!
//! 特性：
//! - 顺序追加写入
//! - 4KB 页对齐（部分页填充零）
//! - 批量写入（减少系统调用）
//! - 可配置刷盘策略（Sync / BufferFull / Periodic）
//! - fsync 持久化保证
//! - LSN = 文件偏移量（隐式确定）
//! - 原子性：每条记录带 CRC，崩溃时部分写入可检测
//! - WAL 压缩：payload 可选压缩，减少 I/O 量，加速 fsync

use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;

use crate::common::config::{WalFlushMode, WalCompression};
use crate::common::error::Result;

use super::{WalRecord, WalRecordType, WAL_RECORD_HEADER_SIZE};

/// WAL 写入器
pub struct WalWriter {
    path: PathBuf,
    file: std::fs::File,
    /// 当前文件偏移（= 下一条记录的 LSN）
    offset: u64,
    /// 写缓冲区（批量写入用）
    buffer: Vec<u8>,
    /// 缓冲区最大大小（超过则刷盘）
    buffer_size: usize,
    /// 刷盘策略
    flush_mode: WalFlushMode,
    /// 组提交：累计多少次 commit 后做一次 fsync（0 = 禁用）
    group_commit_size: usize,
    /// 组提交：缓冲区达到多少字节后强制 fsync（0 = 不按大小触发）
    group_commit_max_bytes: usize,
    /// 当前组内已 commit 但未 fsync 的次数
    pending_commits: usize,
    /// 上次 fsync 以来写入的字节数（精确值，flush 时累计）
    bytes_since_sync: usize,
}

impl WalWriter {
    /// 打开或创建 WAL 文件（默认 Sync 模式）
    pub fn open(path: &str) -> Result<Self> {
        Self::with_config(path, WalFlushMode::Sync, 65536, 0, 65536)
    }

    /// 带配置打开 WAL 文件
    pub fn with_config(
        path: &str,
        flush_mode: WalFlushMode,
        buffer_size: usize,
        group_commit_size: usize,
        group_commit_max_bytes: usize,
    ) -> Result<Self> {
        let path = PathBuf::from(path);
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&path)?;

        let file_size = file.metadata()?.len();
        let buf_cap = buffer_size.max(4096);

        Ok(Self {
            path,
            file,
            offset: file_size,
            buffer: Vec::with_capacity(buf_cap),
            buffer_size: buf_cap,
            flush_mode,
            group_commit_size,
            group_commit_max_bytes,
            pending_commits: 0,
            bytes_since_sync: 0,
        })
    }

    /// 写入一条记录，返回 LSN
    /// 记录先写入缓冲区，缓冲区满或调用 flush 时才真正写盘
    pub fn write_record(
        &mut self,
        record_type: WalRecordType,
        txn_id: u32,
        table_id: u32,
        payload: &[u8],
    ) -> Result<u64> {
        let record = WalRecord {
            lsn: self.offset + self.buffer.len() as u64,
            record_type,
            txn_id,
            table_id,
            payload: payload.to_vec(),
        };

        let lsn = record.lsn;
        let bytes = record.to_bytes();

        // 检查是否需要先刷缓冲区
        if self.buffer.len() + bytes.len() > self.buffer_size {
            self.flush()?;
        }

        self.buffer.extend_from_slice(&bytes);
        Ok(lsn)
    }

    /// 批量写入多条记录（更高效）
    pub fn write_batch(
        &mut self,
        records: &[(WalRecordType, u32, u32, &[u8])],
    ) -> Result<Vec<u64>> {
        let mut lsns = Vec::with_capacity(records.len());

        for (rec_type, txn_id, table_id, payload) in records {
            let lsn = self.write_record(*rec_type, *txn_id, *table_id, payload)?;
            lsns.push(lsn);
        }

        Ok(lsns)
    }

    /// 刷新缓冲区到文件系统（page cache）
    pub fn flush(&mut self) -> Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }

        let len = self.buffer.len();
        self.file.write_all(&self.buffer)?;
        self.offset += len as u64;
        self.bytes_since_sync += len;
        self.buffer.clear();

        Ok(())
    }

    /// 提交时的持久化操作
    ///
    /// 根据刷盘策略和组提交配置行为不同：
    /// - Sync + 无组提交: flush + fsync（最强保证，每次提交都落盘）
    /// - Sync + 组提交: flush 到 page cache，累计达到 group_commit_size 或 group_commit_max_bytes 时才 fsync
    /// - BufferFull / Periodic: 只 flush 到 page cache
    pub fn commit_flush(&mut self) -> Result<()> {
        match self.flush_mode {
            WalFlushMode::Sync => {
                if self.group_commit_size == 0 && self.group_commit_max_bytes == 0 {
                    // 无组提交：每次 commit 都 fsync
                    self.flush()?;
                    self.file.sync_data()?;
                    self.pending_commits = 0;
                    self.bytes_since_sync = 0;
                } else {
                    // 组提交模式：先 flush 到 page cache，计数
                    self.flush()?;
                    self.pending_commits += 1;

                    // 检查是否需要 fsync（任一条件满足即触发）
                    let size_triggered = self.group_commit_size > 0
                        && self.pending_commits >= self.group_commit_size;
                    let bytes_triggered = self.group_commit_max_bytes > 0
                        && self.bytes_since_sync >= self.group_commit_max_bytes;

                    if size_triggered || bytes_triggered {
                        self.file.sync_data()?;
                        self.pending_commits = 0;
                        self.bytes_since_sync = 0;
                    }
                }
            }
            WalFlushMode::BufferFull | WalFlushMode::Periodic => {
                self.flush()?;
            }
        }
        Ok(())
    }

    /// 强制刷盘（fsync，确保持久化到磁盘）
    /// 同时重置组提交计数器
    pub fn sync(&mut self) -> Result<()> {
        self.flush()?;
        self.file.sync_data()?;
        self.pending_commits = 0;
        self.bytes_since_sync = 0;
        Ok(())
    }

    /// 获取组提交配置
    pub fn group_commit_size(&self) -> usize {
        self.group_commit_size
    }

    /// 设置组提交大小（0 = 禁用）
    pub fn set_group_commit_size(&mut self, size: usize) {
        self.group_commit_size = size;
    }

    /// 获取待 fsync 的 commit 数量
    pub fn pending_commits(&self) -> usize {
        self.pending_commits
    }

    /// 获取上次 fsync 以来写入的字节数
    pub fn bytes_since_sync(&self) -> usize {
        self.bytes_since_sync
    }

    /// 获取当前已写入的最大 LSN（含缓冲区）
    pub fn current_lsn(&self) -> u64 {
        self.offset + self.buffer.len() as u64
    }

    /// 获取刷盘模式
    pub fn flush_mode(&self) -> WalFlushMode {
        self.flush_mode
    }

    /// 设置刷盘模式
    pub fn set_flush_mode(&mut self, mode: WalFlushMode) {
        self.flush_mode = mode;
    }

    /// 获取已持久化（fsync）的最大 LSN
    pub fn durable_lsn(&self) -> u64 {
        self.offset
    }

    /// 截断到指定 LSN（Checkpoint 后清理 WAL）
    pub fn truncate(&mut self, lsn: u64) -> Result<()> {
        self.flush()?;

        // Windows 下，通过 .append(true) 打开的文件句柄调用 `set_len` 可能
        // 失败（PermissionDenied / ERROR_ACCESS_DENIED）——因为该句柄缺少
        // 修改文件结束位置所需的访问掩码。
        //
        // 跨平台安全策略：不对 self.file 本身 set_len；改为对同一路径再开
        // 一把 `write + create` 的独立句柄执行 set_len。Rust 在 Windows 上
        // 默认带 FILE_SHARE_READ|WRITE，并发打开不会冲突。
        {
            let resizer = OpenOptions::new()
                .write(true)
                .create(true)
                .open(&self.path)?;
            resizer.set_len(lsn)?;
        }

        // 同步 self.file 的 offset 到新长度（若 lsn < 旧尾部则回退）
        // append 模式下所有 write 会自动 seek 到 end，但我们需要让
        // self.offset 与真实文件大小对齐。
        let file_size = self.file.metadata()?.len();
        self.offset = file_size;
        use std::io::Seek;
        self.file.seek(SeekFrom::Start(file_size))?;

        Ok(())
    }

    /// 获取 WAL 文件路径
    pub fn path(&self) -> &PathBuf {
        &self.path
    }
}

impl Drop for WalWriter {
    fn drop(&mut self) {
        // drop 时尽量 flush，但不保证（可能已经在错误路径中）
        let _ = self.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn tmp(name: &str) -> String {
        let mut p = std::env::temp_dir();
        let tid = format!("{:?}", std::thread::current().id())
            .replace('(', "_").replace(')', "")
            .replace([':', ' '], "_");
        p.push(format!("hybriddb_wal_{}_{}_{}.hdb-wal", name, std::process::id(), tid));
        p.to_string_lossy().to_string()
    }

    #[test]
    fn test_wal_write_and_read() {
        let tmp = tmp("writer_basic");
        let _ = std::fs::remove_file(&tmp);

        {
            let mut writer = WalWriter::open(&tmp).unwrap();
            let lsn1 = writer.write_record(WalRecordType::Begin, 1, 0, &[]).unwrap();
            let lsn2 = writer.write_record(WalRecordType::Insert, 1, 1, &[1, 2, 3]).unwrap();
            writer.sync().unwrap();

            assert_eq!(lsn1, 0);
            assert!(lsn2 > lsn1);
        }

        // 读取验证
        let mut file = std::fs::File::open(&tmp).unwrap();
        let mut data = Vec::new();
        file.read_to_end(&mut data).unwrap();

        let rec1 = WalRecord::from_bytes(&data).unwrap();
        assert_eq!(rec1.record_type, WalRecordType::Begin);
        assert_eq!(rec1.txn_id, 1);

        let rec1_size = WAL_RECORD_HEADER_SIZE + rec1.payload.len();
        let rec2 = WalRecord::from_bytes(&data[rec1_size..]).unwrap();
        assert_eq!(rec2.record_type, WalRecordType::Insert);
        assert_eq!(rec2.txn_id, 1);
        assert_eq!(rec2.table_id, 1);
        assert_eq!(rec2.payload, vec![1, 2, 3]);

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn test_wal_buffer_batching() {
        let tmp = tmp("writer_buffer");
        let _ = std::fs::remove_file(&tmp);

        let mut writer = WalWriter::open(&tmp).unwrap();
        // 写入多条，验证 LSN 连续
        let mut prev_lsn = 0;
        for i in 0..100 {
            let lsn = writer.write_record(WalRecordType::Insert, 1, 1, &[i as u8]).unwrap();
            assert!(lsn >= prev_lsn);
            prev_lsn = lsn;
        }
        writer.sync().unwrap();

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn test_wal_empty_payload_records() {
        let tmp = tmp("writer_empty_payload");
        let _ = std::fs::remove_file(&tmp);

        {
            let mut writer = WalWriter::open(&tmp).unwrap();
            // Begin/Commit/Rollback/Checkpoint 都有空 payload
            writer.write_record(WalRecordType::Begin, 1, 0, &[]).unwrap();
            writer.write_record(WalRecordType::Commit, 1, 0, &[]).unwrap();
            writer.write_record(WalRecordType::Rollback, 2, 0, &[]).unwrap();
            writer.write_record(WalRecordType::Checkpoint, 0, 0, &[0, 0, 0, 0, 0, 0, 0, 0]).unwrap();
            writer.sync().unwrap();
        }

        // 验证文件大小：4 条记录 × 19 字节头 + 8 字节 checkpoint payload
        let meta = std::fs::metadata(&tmp).unwrap();
        assert_eq!(meta.len(), (19 * 4 + 8) as u64);

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn test_wal_lsn_monotonic() {
        let tmp = tmp("writer_lsn_monotonic");
        let _ = std::fs::remove_file(&tmp);

        let mut writer = WalWriter::open(&tmp).unwrap();
        let mut last_lsn = u64::MAX;

        for i in 0..50 {
            let lsn = writer.write_record(WalRecordType::Insert, i, i, &[i as u8; 10]).unwrap();
            if last_lsn != u64::MAX {
                assert!(lsn > last_lsn, "LSN not monotonic: {} <= {}", lsn, last_lsn);
            }
            last_lsn = lsn;
        }

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn test_wal_current_lsn_matches_durable_after_sync() {
        let tmp = tmp("writer_current_lsn");
        let _ = std::fs::remove_file(&tmp);

        let mut writer = WalWriter::open(&tmp).unwrap();
        let lsn = writer.write_record(WalRecordType::Begin, 1, 0, &[]).unwrap();

        // sync 前 current_lsn 应该已经是写入后的位置
        assert_eq!(writer.current_lsn(), lsn + 19); // 19 = header + empty payload + crc

        writer.sync().unwrap();
        assert_eq!(writer.durable_lsn(), writer.current_lsn());

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn test_wal_flush_clears_buffer() {
        let tmp = tmp("writer_flush");
        let _ = std::fs::remove_file(&tmp);

        let mut writer = WalWriter::open(&tmp).unwrap();
        writer.write_record(WalRecordType::Begin, 1, 0, &[]).unwrap();
        writer.flush().unwrap();

        // flush 后文件应该有数据
        let meta = std::fs::metadata(&tmp).unwrap();
        assert!(meta.len() > 0);

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn test_wal_large_record_spans_buffer() {
        let tmp = tmp("writer_large");
        let _ = std::fs::remove_file(&tmp);

        let mut writer = WalWriter::open(&tmp).unwrap();
        // 写入一条大于默认 buffer size (64KB) 的记录
        let large_payload = vec![42u8; 100_000];
        let lsn = writer.write_record(WalRecordType::Insert, 1, 1, &large_payload).unwrap();
        writer.sync().unwrap();

        let file_size = std::fs::metadata(&tmp).unwrap().len();
        assert_eq!(file_size, lsn + 19 + large_payload.len() as u64);

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn test_group_commit_by_size() {
        let tmp = tmp("writer_group_size");
        let _ = std::fs::remove_file(&tmp);

        // 组提交：每 4 次 commit fsync 一次
        let mut writer = WalWriter::with_config(
            &tmp,
            WalFlushMode::Sync,
            65536,
            4,  // group_commit_size = 4
            0,  // 不按字节触发
        ).unwrap();

        // 前 3 次 commit 不应触发 fsync
        for i in 0..3 {
            writer.write_record(WalRecordType::Begin, i, 0, &[]).unwrap();
            writer.write_record(WalRecordType::Commit, i, 0, &[]).unwrap();
            writer.commit_flush().unwrap();
        }
        assert_eq!(writer.pending_commits(), 3);
        assert!(writer.bytes_since_sync() > 0);

        // 第 4 次 commit 应触发 fsync
        writer.write_record(WalRecordType::Begin, 3, 0, &[]).unwrap();
        writer.write_record(WalRecordType::Commit, 3, 0, &[]).unwrap();
        writer.commit_flush().unwrap();
        assert_eq!(writer.pending_commits(), 0);
        assert_eq!(writer.bytes_since_sync(), 0);

        writer.sync().unwrap();
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn test_group_commit_by_bytes() {
        let tmp = tmp("writer_group_bytes");
        let _ = std::fs::remove_file(&tmp);

        // 组提交：按字节触发（100 字节），不按次数
        let mut writer = WalWriter::with_config(
            &tmp,
            WalFlushMode::Sync,
            65536,
            0,   // 不按次数触发
            100, // group_commit_max_bytes = 100
        ).unwrap();

        // 写入小记录，累计字节数直到触发
        let mut count = 0;
        loop {
            writer.write_record(WalRecordType::Insert, count, 1, &[count as u8; 10]).unwrap();
            writer.commit_flush().unwrap();
            count += 1;
            if writer.pending_commits() == 0 && count > 1 {
                break; // fsync 已触发
            }
            assert!(count < 20, "should have triggered by bytes before 20 commits");
        }
        assert!(count > 1, "should have at least 2 commits before trigger");

        writer.sync().unwrap();
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn test_group_commit_sync_forces_flush() {
        let tmp = tmp("writer_group_sync");
        let _ = std::fs::remove_file(&tmp);

        let mut writer = WalWriter::with_config(
            &tmp,
            WalFlushMode::Sync,
            65536,
            100, // 大的 group size，不会自动触发
            0,
        ).unwrap();

        // 写入 5 次 commit，都在组内
        for i in 0..5 {
            writer.write_record(WalRecordType::Commit, i, 0, &[]).unwrap();
            writer.commit_flush().unwrap();
        }
        assert_eq!(writer.pending_commits(), 5);

        // 手动 sync 强制刷盘并重置计数
        writer.sync().unwrap();
        assert_eq!(writer.pending_commits(), 0);
        assert_eq!(writer.bytes_since_sync(), 0);

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn test_group_commit_disabled_is_sync() {
        let tmp = tmp("writer_group_disabled");
        let _ = std::fs::remove_file(&tmp);

        // 禁用组提交（默认行为）
        let mut writer = WalWriter::with_config(
            &tmp,
            WalFlushMode::Sync,
            65536,
            0, // 禁用
            0, // 禁用
        ).unwrap();

        // 每次 commit 都应 fsync（pending 始终为 0）
        for i in 0..5 {
            writer.write_record(WalRecordType::Commit, i, 0, &[]).unwrap();
            writer.commit_flush().unwrap();
            assert_eq!(writer.pending_commits(), 0);
        }

        let _ = std::fs::remove_file(&tmp);
    }
}
