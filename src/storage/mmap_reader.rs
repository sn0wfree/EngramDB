//! Phase 2 P0-A：mmap 统一读路径
//!
//! 用 mmap 替代手工 buffer pool：
//! - OS 级页面缓存替代手写 LRU
//! - 读路径返回 `&[u8]` 借用，不分配堆内存
//! - 大文件自动按页加载
//!
//! ## 用法
//! ```ignore
//! let reader = MmapReader::open(&path)?;
//! let slice = reader.slice(0, 1024);  // 直接返回 &[u8]，零拷贝
//! ```
//!
//! ## 写限制
//! mmap 后端**不支持原地修改**——任何写入必须：
//! 1. 读取到临时缓冲区
//! 2. 修改后写回新文件
//! 3. 重新 mmap
//!
//! Phase 2 当前设计：mmap 仅用于冷数据/只读快照；写入仍走原路径。
//! 未来可扩展 COW 后端（Phase 3）。

#![cfg(feature = "mmap-read")]

use std::fs::File;
use std::path::Path;

use memmap2::{Mmap, MmapOptions};

use crate::common::error::Result;

/// mmap 读路径封装
///
/// 持有 `Mmap`（由 memmap2 crate 管理），提供 `slice(offset, len) -> &[u8]` 接口。
/// 所有切片直接借用 mmap 内存，零拷贝。
pub struct MmapReader {
    mmap: Mmap,
    /// 文件大小（字节），与 `mmap.len()` 一致
    len: usize,
}

impl MmapReader {
    /// 打开并 mmap 一个文件
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let file = File::open(path.as_ref())?;
        let len = file.metadata()?.len() as usize;
        // 长度为 0 时 memmap2 在某些平台会 panic，做防御
        let mmap = if len == 0 {
            // 空文件：尝试 mmap 一个空 backing（Linux 上需要 0 长度特殊处理）
            // 直接返回 len=0 的 reader
            return Ok(Self {
                mmap: unsafe { MmapOptions::new().len(0).map(&file)? },
                len: 0,
            });
        } else {
            unsafe { MmapOptions::new().map(&file)? }
        };
        Ok(Self { mmap, len })
    }

    /// 读取 [offset, offset+len) 区间的字节切片（零拷贝）
    ///
    /// # Panic
    /// 如果 offset + len > 文件大小则 panic（与 std::slice 行为一致）
    pub fn slice(&self, offset: usize, len: usize) -> &[u8] {
        &self.mmap[offset..offset + len]
    }

    /// 读取整个文件（零拷贝）
    pub fn as_slice(&self) -> &[u8] {
        &self.mmap[..]
    }

    /// 文件大小
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// 从 mmap 切片构造 typed 数组
///
/// 沿用 `ColumnData::deserialize_typed` 的语义，但 borrowed 输入，
/// 省去 `to_vec()` 拷贝。Phase 2 起步版本：仍调 `deserialize_typed`（内部会拷贝）。
/// 后续优化点：增加 `deserialize_typed_borrowed()` 直接 borrowed 路径。
pub mod helpers {
    use super::Result;

    /// 从 mmap 切片反序列化为 Vec<u8>（Phase 2 起步：仍拷贝）
    ///
    /// 真正零拷贝需要 typed 数组支持 borrowed variant，本期留接口位。
    pub fn copy_from_mmap(slice: &[u8]) -> Vec<u8> {
        slice.to_vec()
    }

    /// 预取 mmap 区间（hint OS 提前加载到页缓存）
    pub fn prefetch_range(_reader: &super::MmapReader, _offset: usize, _len: usize) -> Result<()> {
        // TODO: 接入 memmap2::Mmap::advise 或 MADV_WILLNEED
        // memmap2 0.9 未提供 advise API，需 raw syscall
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmp_path(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        let tid = format!("{:?}", std::thread::current().id());
        let safe = tid.replace(['(', ')', ':', ' '], "_");
        p.push(format!(
            "engramdb_mmap_{}_{}_{}.hdb",
            name,
            std::process::id(),
            safe
        ));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn test_mmap_basic_read() {
        let path = tmp_path("basic");
        let mut f = File::create(&path).unwrap();
        f.write_all(b"hello world").unwrap();
        drop(f);

        let reader = MmapReader::open(&path).unwrap();
        assert_eq!(reader.len(), 11);
        assert_eq!(reader.slice(0, 5), b"hello");
        assert_eq!(reader.slice(6, 5), b"world");
        assert_eq!(reader.as_slice(), b"hello world");
    }

    #[test]
    fn test_mmap_empty_file() {
        let path = tmp_path("empty");
        File::create(&path).unwrap();
        let reader = MmapReader::open(&path).unwrap();
        assert_eq!(reader.len(), 0);
        assert!(reader.is_empty());
    }

    #[test]
    fn test_mmap_large_file() {
        // 写入 1MB 数据
        let path = tmp_path("large");
        let mut f = File::create(&path).unwrap();
        let data: Vec<u8> = (0..1_048_576).map(|i| (i % 256) as u8).collect();
        f.write_all(&data).unwrap();
        drop(f);

        let reader = MmapReader::open(&path).unwrap();
        assert_eq!(reader.len(), 1_048_576);

        // 验证随机访问
        assert_eq!(reader.slice(0, 4), &[0, 1, 2, 3]);
        assert_eq!(reader.slice(1_048_572, 4), &[252, 253, 254, 255]);
        assert_eq!(reader.slice(500_000, 4), &[
            (500_000 % 256) as u8,
            ((500_000 + 1) % 256) as u8,
            ((500_000 + 2) % 256) as u8,
            ((500_000 + 3) % 256) as u8,
        ]);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_mmap_slice_zero_copy() {
        let path = tmp_path("zerocopy");
        let mut f = File::create(&path).unwrap();
        f.write_all(&[1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
        drop(f);

        let reader = MmapReader::open(&path).unwrap();
        let s1 = reader.slice(0, 4);
        let s2 = reader.slice(4, 4);

        // 验证两个切片是 mmap 内的不同内存区域（同一 backing）
        let p1 = s1.as_ptr() as usize;
        let p2 = s2.as_ptr() as usize;
        assert_eq!(p2 - p1, 4, "两个切片应该是 mmap 内的连续内存");

        let _ = std::fs::remove_file(&path);
    }
}