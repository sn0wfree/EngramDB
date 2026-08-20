//! Phase 5 P0：mmap 写路径
//!
//! 目标：compact 操作写入到 mmap 后端文件，不阻塞并发读路径。
//!
//! ## 设计
//! - MmapWriter：append-only mmap 写入（类似 log 文件）
//! - 写入后立即可读（通过 MmapReader 打开同一文件）
//! - compact 完成时原子 rename（COW）
//!
//! ## 与 MmapReader 的关系
//! - MmapWriter 写入完成后，MmapReader 打开同一文件读取
//! - 不阻塞并发读：旧文件仍可 mmap，新文件在 compact 完成后替换

#![cfg(feature = "mmap-read")]

use std::fs::{File, OpenOptions};
use std::io::{Read, Write, Seek, SeekFrom};

use crate::common::error::Result;

/// Phase 5 P0：mmap 写入器（append-only）
///
/// 用于 compact 操作写入大文件：
/// - 直接 write_all（让 OS 缓冲，不需要 mmap 写）
/// - 完成后通过 atomic rename 替换旧文件
/// - 写入期间旧文件仍可 mmap 读
pub struct MmapWriter {
    path: std::path::PathBuf,
    file: File,
    /// 当前写入位置（字节偏移）
    offset: u64,
    /// 是否已 fsync
    synced: bool,
}

impl MmapWriter {
    /// 创建新写入器（覆盖写入）
    pub fn create(path: &std::path::Path) -> Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            file,
            offset: 0,
            synced: false,
        })
    }

    /// 追加模式打开（写入已有文件）
    pub fn append(path: &std::path::Path) -> Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        let offset = file.metadata()?.len();
        Ok(Self {
            path: path.to_path_buf(),
            file,
            offset,
            synced: false,
        })
    }

    /// 写入数据（零拷贝，直接借用）
    pub fn write(&mut self, data: &[u8]) -> Result<()> {
        self.file.write_all(data)?;
        self.offset += data.len() as u64;
        self.synced = false;
        Ok(())
    }

    /// 写入单个字节
    pub fn push(&mut self, byte: u8) -> Result<()> {
        self.write(&[byte])
    }

    /// 写入 u32 长度前缀
    pub fn write_u32(&mut self, value: u32) -> Result<()> {
        self.write(&value.to_le_bytes())
    }

    /// 写入 u64 长度前缀
    pub fn write_u64(&mut self, value: u64) -> Result<()> {
        self.write(&value.to_le_bytes())
    }

    /// 当前写入位置
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// 是否已 fsync
    pub fn is_synced(&self) -> bool {
        self.synced
    }

    /// fsync 到磁盘
    pub fn sync(&mut self) -> Result<()> {
        self.file.sync_data()?;
        self.synced = true;
        Ok(())
    }

    /// 读取整个文件（完成写入后调用）
    pub fn into_bytes(self) -> Result<Vec<u8>> {
        drop(self.file);
        Ok(std::fs::read(&self.path)?)
    }

    /// 关闭写入器并返回写入的总字节数
    pub fn finish(self) -> Result<u64> {
        Ok(self.offset)
    }
}

impl std::io::Write for MmapWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let written = self.file.write(buf)?;
        self.offset += written as u64;
        self.synced = false;
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

/// Phase 5 P0：atomic 替换（COW 写入）
///
/// 将 src 原子 rename 到 dst，不阻塞并发读取。
pub fn atomic_replace(src: &std::path::Path, dst: &std::path::Path) -> Result<()> {
    std::fs::rename(src, dst).map_err(|e| crate::common::error::EngramDbError::Io(
        std::io::Error::new(std::io::ErrorKind::Other, format!("atomic rename failed: {}", e)),
    ))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp_path(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        let tid = format!("{:?}", std::thread::current().id()).replace(['(', ')', ':', ' '], "_");
        p.push(format!("engramdb_mmapw_{}_{}_{}.hdb", name, std::process::id(), tid));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn test_mmap_write_basic() {
        let path = tmp_path("basic");
        let mut writer = MmapWriter::create(&path).unwrap();
        writer.write(b"hello").unwrap();
        writer.write(b" world").unwrap();
        assert_eq!(writer.offset(), 11);
        writer.sync().unwrap();

        // 验证文件内容
        let data = std::fs::read(&path).unwrap();
        assert_eq!(data, b"hello world");
    }

    #[test]
    fn test_mmap_write_u32() {
        let path = tmp_path("u32");
        let mut writer = MmapWriter::create(&path).unwrap();
        writer.write_u32(0xDEADBEEF).unwrap();
        writer.sync().unwrap();

        let data = std::fs::read(&path).unwrap();
        assert_eq!(data.len(), 4);
        assert_eq!(u32::from_le_bytes(data[0..4].try_into().unwrap()), 0xDEADBEEF);
    }

    #[test]
    fn test_mmap_write_into_bytes() {
        let path = tmp_path("into_bytes");
        let mut writer = MmapWriter::create(&path).unwrap();
        writer.write(b"test data").unwrap();

        let data = writer.into_bytes().unwrap();
        assert_eq!(data, b"test data");
    }

    #[test]
    fn test_atomic_replace() {
        let src = tmp_path("src");
        let dst = tmp_path("dst");

        std::fs::write(&src, b"src content").unwrap();
        std::fs::write(&dst, b"old content").unwrap();

        atomic_replace(&src, &dst).unwrap();

        let data = std::fs::read(&dst).unwrap();
        assert_eq!(data, b"src content");
        assert!(!src.exists());
    }
}