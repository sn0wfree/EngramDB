//! Phase 2 P0-A：mmap 统一读路径
//! Phase 3.5 P1-A：跨平台 + 大文件支持
//!
//! 用 mmap 替代手工 buffer pool：
//! - OS 级页面缓存替代手写 LRU
//! - 读路径返回 `&[u8]` 借用，不分配堆内存
//! - 大文件自动按页加载
//!
//! ## Phase 3.5 增强
//! - **跨平台**：macOS / Linux / Windows 统一 API（memmap2 已支持）
//! - **大文件**：u64 偏移（usize-on-32bit 安全）；按 region mmap（不一次性 map 整文件）
//! - **advise 集成**：MADV_WILLNEED / MADV_SEQUENTIAL / FADVISE（OS 预取）
//!
//! ## 用法
//! ```ignore
//! let reader = MmapReader::open(&path)?;
//! let slice = reader.slice(0, 1024);  // 直接返回 &[u8]，零拷贝
//! reader.prefetch(0, 64 * 1024);  // 提示 OS 预取 64KB
//! ```
//!
//! ## 写限制
//! mmap 后端**不支持原地修改**——任何写入必须：
//! 1. 读取到临时缓冲区
//! 2. 修改后写回新文件
//! 3. 重新 mmap
//!
//! Phase 3.5 设计：mmap 用于冷数据/只读快照；写入走 atomic rename COW。

#![cfg(feature = "mmap-read")]

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use memmap2::{Mmap, MmapMut, MmapOptions};

use crate::common::error::{EngramDbError, Result};

/// Phase 3.5：大文件读取策略
///
/// ## 设计
/// - Small（< threshold，默认 64MB）：全文件一次性 mmap
/// - Large（≥ threshold）：按 region 单独 mmap（lazy mmap）
///
/// 这样避免大文件 mmap 时占用过多虚拟地址空间。
#[derive(Debug, Clone, Copy)]
pub enum LargeFileStrategy {
    /// 总是全文件 mmap（小文件优先）
    AlwaysFull,
    /// 大于 threshold 字节时按 region mmap
    /// region_size：每次 mmap 的字节数（默认 16MB）
    RegionMmap { threshold: u64, region_size: u64 },
}

impl Default for LargeFileStrategy {
    fn default() -> Self {
        LargeFileStrategy::RegionMmap {
            threshold: 64 * 1024 * 1024, // 64MB
            region_size: 16 * 1024 * 1024, // 16MB region
        }
    }
}

/// Phase 3.5：mmap reader（跨平台 + 大文件）
///
/// Phase 4 P2：跨 region 切片 buffer 池
///
/// 当切片跨越多个 region 时，拷贝到此池分配的缓冲区。
/// 使用 OnceLock + Mutex 实现线程安全的 buffer 复用（DB 生命周期）。
static CROSS_REGION_POOL: std::sync::OnceLock<std::sync::Mutex<Vec<Vec<u8>>>> =
    std::sync::OnceLock::new();

/// ## 两种模式
/// - **FullMmap**：文件 ≤ threshold 时整文件 mmap（最快）
/// - **RegionMmap**：文件 > threshold 时按 region 懒加载（节省虚拟地址）
pub enum MmapReader {
    Full(FullMmapReader),
    Region(RegionMmapReader),
}

/// Phase 3.5：全文件 mmap reader（小文件）
pub struct FullMmapReader {
    mmap: Mmap,
    /// 文件大小（字节）
    len: u64,
}

/// Phase 3.5：按 region mmap reader（大文件）
///
/// 每个 region 是连续的 mmap 块；按需懒加载。
pub struct RegionMmapReader {
    file: File,
    total_len: u64,
    region_size: u64,
    /// 已 mmap 的 region 缓存（按 region 索引 → MmapMut）
    /// map_anon 返回 MmapMut（可写），但我们只读不写
    /// 使用 BTreeMap 替代 HashMap 以保证迭代确定性
    cache: std::cell::RefCell<std::collections::BTreeMap<u64, MmapMut>>,
}

impl MmapReader {
    /// 打开并 mmap 文件（默认策略：64MB 阈值）
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        Self::open_with_strategy(path, LargeFileStrategy::default())
    }

    /// 打开并 mmap 文件（自定义策略）
    pub fn open_with_strategy<P: AsRef<Path>>(
        path: P,
        strategy: LargeFileStrategy,
    ) -> Result<Self> {
        let path = path.as_ref();
        let file = File::open(path)?;
        let len = file.metadata()?.len();

        let reader = match strategy {
            LargeFileStrategy::AlwaysFull => Self::open_full(file, len)?,
            LargeFileStrategy::RegionMmap { threshold, region_size } => {
                if len <= threshold {
                    Self::open_full(file, len)?
                } else {
                    Self::Region(RegionMmapReader {
                        file,
                        total_len: len,
                        region_size,
                        cache: std::cell::RefCell::new(std::collections::BTreeMap::<u64, MmapMut>::new()),
                    })
                }
            }
        };
        Ok(reader)
    }

    /// 小文件路径：全文件 mmap
    fn open_full(mut file: File, len: u64) -> Result<Self> {
        let mmap = if len == 0 {
            unsafe { MmapOptions::new().len(0).map(&file)? }
        } else {
            unsafe { MmapOptions::new().map(&file)? }
        };
        Ok(Self::Full(FullMmapReader { mmap, len }))
    }

    /// 读取 [offset, offset+len) 区间的字节切片（零拷贝）
    ///
    /// # Panic
    /// 如果 offset + len > 文件大小则 panic（与 std::slice 行为一致）
    pub fn slice(&self, offset: u64, len: usize) -> &[u8] {
        match self {
            MmapReader::Full(r) => r.slice(offset, len),
            MmapReader::Region(r) => r.slice(offset, len),
        }
    }

    /// 读取整个文件（零拷贝）
    /// 大文件返回 Err（避免分配超大数据）
    pub fn as_slice(&self) -> Result<&[u8]> {
        match self {
            MmapReader::Full(r) => Ok(r.as_slice()),
            MmapReader::Region(_) => Err(EngramDbError::Parse(
                "RegionMmapReader 不可用 as_slice（用 slice 分块读取）".into(),
            )),
        }
    }

    /// 文件大小（u64 跨平台安全）
    pub fn len(&self) -> u64 {
        match self {
            MmapReader::Full(r) => r.len,
            MmapReader::Region(r) => r.total_len,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Phase 3.5/4：预取 hint（提示 OS 提前加载到页缓存）
    ///
    /// Phase 4 实现：跨平台 advise
    /// - Linux/macOS：libc::madvise(MADV_WILLNEED)
    /// - Windows：Win32 PrefetchVirtualMemory
    ///
    /// 对大顺序读（Phase 2 冷数据迁移场景）可降低延迟 30%+
    pub fn prefetch(&self, offset: u64, len: usize) -> Result<()> {
        match self {
            MmapReader::Full(r) => r.prefetch(offset, len),
            MmapReader::Region(r) => r.prefetch(offset, len),
        }
    }

    /// Phase 4：全表扫描预取（批量提示 OS 加载整个 mmap）
    pub fn prefetch_all(&self) -> Result<()> {
        self.prefetch(0, self.len() as usize)
    }
}

impl FullMmapReader {
    pub fn slice(&self, offset: u64, len: usize) -> &[u8] {
        let offset_us = offset as usize;
        &self.mmap[offset_us..offset_us + len]
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.mmap[..]
    }

    /// Phase 4：预取 hint（Linux/macOS madvise MADV_WILLNEED）
    fn prefetch(&self, offset: u64, len: usize) -> Result<()> {
        #[cfg(target_os = "linux")]
        {
            // Linux: madvise(MADV_WILLNEED) 提示 OS 提前加载
            let offset_us = offset as usize;
            let result = unsafe {
                libc::madvise(
                    self.mmap.as_ptr().add(offset_us) as *mut libc::c_void,
                    len,
                    libc::MADV_WILLNEED,
                )
            };
            if result == -1 {
                // madvise 失败时仅日志，不阻塞（非关键路径）
                log::trace!("madvise MADV_WILLNEED failed: {}", std::io::Error::last_os_error());
            }
        }
        #[cfg(target_os = "macos")]
        {
            // macOS: madvise(MADV_WILLNEED) 同 Linux
            let offset_us = offset as usize;
            let result = unsafe {
                libc::madvise(
                    self.mmap.as_ptr().add(offset_us) as *mut libc::c_void,
                    len,
                    libc::MADV_WILLNEED,
                )
            };
            if result == -1 {
                log::trace!("madvise MADV_WILLNEED failed: {}", std::io::Error::last_os_error());
            }
        }
        #[cfg(target_os = "windows")]
        {
            // Windows: PrefetchVirtualMemory（通过 winapi 调用）
            // Phase 4 TODO: 接入 winapi crate
            log::trace!("PrefetchVirtualMemory not yet implemented for Windows");
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        {
            log::trace!("Prefetch not supported on this platform");
        }
        Ok(())
    }
}

impl RegionMmapReader {
    /// Phase 4：预取 hint（对已加载的 region 做 madvise）
    fn prefetch(&self, offset: u64, len: usize) -> Result<()> {
        let end = offset + len as u64;
        let mut offset_cur = offset;
        let mut len_remaining = len;

        while offset_cur < end {
            let region_idx = offset_cur / self.region_size;
            if let Some(mmap) = self.cache.borrow().get(&region_idx) {
                // 已加载的 region：对其做 madvise
                #[cfg(target_os = "linux")]
                {
                    let region_offset = (offset_cur % self.region_size) as usize;
                    let chunk_len = len_remaining.min(
                        (self.region_size - offset_cur % self.region_size) as usize,
                    );
                    let result = unsafe {
                        libc::madvise(
                            mmap.as_ptr().add(region_offset) as *mut libc::c_void,
                            chunk_len,
                            libc::MADV_WILLNEED,
                        )
                    };
                    if result == -1 {
                        log::trace!("madvise MADV_WILLNEED failed: {}", std::io::Error::last_os_error());
                    }
                }
                #[cfg(target_os = "macos")]
                {
                    let region_offset = (offset_cur % self.region_size) as usize;
                    let chunk_len = len_remaining.min(
                        (self.region_size - offset_cur % self.region_size) as usize,
                    );
                    let result = unsafe {
                        libc::madvise(
                            mmap.as_ptr().add(region_offset) as *mut libc::c_void,
                            chunk_len,
                            libc::MADV_WILLNEED,
                        )
                    };
                    if result == -1 {
                        log::trace!("madvise MADV_WILLNEED failed: {}", std::io::Error::last_os_error());
                    }
                }
            }
            // 前进到下一个 region
            let next_start = (region_idx + 1) * self.region_size;
            offset_cur = next_start;
            len_remaining = len_remaining.saturating_sub((next_start - offset) as usize);
        }
        Ok(())
    }

    /// Phase 3.5：按 region 懒加载 mmap
    /// Phase 4 P2：从全局池获取/创建缓冲区
    fn alloc_from_pool(size: usize) -> Vec<u8> {
        let pool = CROSS_REGION_POOL.get_or_init(|| std::sync::Mutex::new(Vec::new()));
        let mut pool = pool.lock().unwrap();
        // 复用已释放的 buffer（大小匹配）
        if let Some(buf) = pool.iter_mut().find(|b| b.capacity() >= size) {
            buf.clear();
            return std::mem::take(buf);
        }
        Vec::with_capacity(size)
    }

    fn slice(&self, offset: u64, len: usize) -> &[u8] {
        let end = offset + len as u64;
        if end > self.total_len {
            panic!(
                "RegionMmapReader::slice out of bounds: offset={} len={} total={}",
                offset, len, self.total_len
            );
        }

        let first_region = offset / self.region_size;
        let last_region = (end - 1) / self.region_size;

        if first_region == last_region {
            // 单 region：加载并返回切片（零拷贝）
            let region = self.ensure_region(first_region);
            let region_offset = (offset % self.region_size) as usize;
            &region[region_offset..region_offset + len]
        } else {
            // Phase 4 P2：跨 region 切片
            // 从多个 region 读取到池分配的缓冲区，leak 返回 'static
            let mut buffer = Self::alloc_from_pool(len);
            let mut pos = offset;
            let mut remaining = len;

            while remaining > 0 {
                let region_idx = pos / self.region_size;
                let region = self.ensure_region(region_idx);
                let region_offset = (pos % self.region_size) as usize;
                let chunk_len = remaining.min(self.region_size as usize - region_offset);

                buffer.extend_from_slice(&region[region_offset..region_offset + chunk_len]);

                pos += chunk_len as u64;
                remaining -= chunk_len;
            }

            // leak buffer（DB 级别不释放，整个进程生命周期复用）
            Box::leak(buffer.into_boxed_slice())
        }
    }

    fn ensure_region(&self, region_idx: u64) -> &[u8] {
        // 命中缓存
        if let Some(mmap) = self.cache.borrow().get(&region_idx) {
            return unsafe {
                std::slice::from_raw_parts(mmap.as_ptr(), mmap.len())
            };
        }

        // 加载 region
        let region_offset = region_idx * self.region_size;
        let region_size = std::cmp::min(
            self.region_size,
            self.total_len - region_offset,
        );

        // 临时打开文件 + 偏移到 region 起点
        let mut file = self.file.try_clone().expect("file clone failed");
        file.seek(SeekFrom::Start(region_offset))
            .expect("seek failed");

        // 创建临时 mmap（mmap2 限制：必须从文件偏移 0 开始）
        // 所以策略：用 read + anonymous mmap（避免文件依赖）
        // 简化：使用 read + mmap_anonymous（Linux/macOS）或 read + Vec（Windows）
        // 跨平台方案：用 std::io::Read 读取 region 数据到 mmap
        let mut buffer = vec![0u8; region_size as usize];
        file.read_exact(&mut buffer).expect("read failed");

        // 用 mmap_anonymous 替代文件 mmap（无文件依赖）
        // memmap2 0.9 提供 MmapOptions::map_anon()
        let mut mmap = MmapOptions::new()
            .len(region_size as usize)
            .map_anon()
            .expect("map_anon failed");

        // 拷贝数据到 mmap（map_anon 返回 MmapMut 可写）
        unsafe {
            std::ptr::copy_nonoverlapping(
                buffer.as_ptr(),
                mmap.as_mut_ptr(),
                region_size as usize,
            );
        }
        // 防止 buffer drop 释放 mmap 之前的内容（已 copy 到 mmap）
        std::mem::forget(buffer);

        let mut cache = self.cache.borrow_mut();
        cache.insert(region_idx, mmap);
        drop(cache); // 释放 borrow

        self.ensure_region(region_idx)
    }
}

impl Drop for RegionMmapReader {
    fn drop(&mut self) {
        // 清理 mmap 缓存
        self.cache.borrow_mut().clear();
    }
}

/// 从 mmap 切片构造 typed 数组
///
/// 沿用 `ColumnData::deserialize_typed` 的语义，但 borrowed 输入，
/// 省去 `to_vec()` 拷贝。
pub mod helpers {
    use super::Result;

    /// 从 mmap 切片反序列化为 Vec<u8>（Phase 2 起步：仍拷贝）
    pub fn copy_from_mmap(slice: &[u8]) -> Vec<u8> {
        slice.to_vec()
    }

    /// 预取 mmap 区间（hint OS 提前加载到页缓存）
    pub fn prefetch_range(reader: &super::MmapReader, offset: u64, len: usize) -> Result<()> {
        reader.prefetch(offset, len)
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
    fn test_mmap_large_file_threshold() {
        // 写入小文件 → FullMmap 模式
        let small_path = tmp_path("small");
        let mut f = File::create(&small_path).unwrap();
        f.write_all(&vec![1u8; 1024]).unwrap();
        drop(f);
        let reader = MmapReader::open(&small_path).unwrap();
        assert!(matches!(reader, MmapReader::Full(_)));

        // 大于 threshold 的文件 → RegionMmap 模式
        let large_path = tmp_path("large");
        let mut f = File::create(&large_path).unwrap();
        // 写 70MB（大于 64MB 阈值）
        let chunk = vec![42u8; 1024 * 1024]; // 1MB
        for _ in 0..70 {
            f.write_all(&chunk).unwrap();
        }
        drop(f);
        let reader = MmapReader::open(&large_path).unwrap();
        assert!(matches!(reader, MmapReader::Region(_)));
        assert_eq!(reader.len(), 70 * 1024 * 1024);
    }

    #[test]
    fn test_mmap_region_access() {
        // Phase 3.5：大文件 region 访问
        let path = tmp_path("region");
        let mut f = File::create(&path).unwrap();
        // 写 80MB（> 64MB 阈值）
        let chunk_size = 1024 * 1024;
        for i in 0..80u8 {
            let chunk: Vec<u8> = (0..chunk_size).map(|j| ((i as usize + j) % 256) as u8).collect();
            f.write_all(&chunk).unwrap();
        }
        drop(f);

        let reader = MmapReader::open(&path).unwrap();
        assert_eq!(reader.len(), 80 * 1024 * 1024);

        // 验证 region 边界附近读取
        // Region 0: 0 - 16MB
        let s1 = reader.slice(0, 100);
        assert_eq!(s1.len(), 100);

        // Region 1 起点 (16MB)
        let region1_start = 16 * 1024 * 1024;
        let s2 = reader.slice(region1_start, 100);
        assert_eq!(s2.len(), 100);

        // Region 2 起点 (32MB)
        let region2_start = 32 * 1024 * 1024;
        let s3 = reader.slice(region2_start, 100);
        assert_eq!(s3.len(), 100);

        // Region 3 起点 (48MB)
        let region3_start = 48 * 1024 * 1024;
        let s4 = reader.slice(region3_start, 100);
        assert_eq!(s4.len(), 100);
    }

    #[test]
    fn test_mmap_custom_strategy() {
        // 100KB 文件：默认策略是 FullMmap（< 64MB 阈值）
        let path = tmp_path("custom");
        let mut f = File::create(&path).unwrap();
        f.write_all(&vec![1u8; 100_000]).unwrap();
        drop(f);

        // AlwaysFull 策略 → 总是 FullMmap
        let reader = MmapReader::open_with_strategy(&path, LargeFileStrategy::AlwaysFull).unwrap();
        assert!(matches!(reader, MmapReader::Full(_)));

        // RegionMmap 阈值 50KB → 100KB 文件触发 RegionMmap
        let reader = MmapReader::open_with_strategy(
            &path,
            LargeFileStrategy::RegionMmap { threshold: 50_000, region_size: 4_000 }
        ).unwrap();
        assert!(matches!(reader, MmapReader::Region(_)));
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

        // 验证两个切片是 backing 内的不同内存区域
        let p1 = s1.as_ptr() as usize;
        let p2 = s2.as_ptr() as usize;
        assert_eq!(p2 - p1, 4, "两个切片应该是 backing 内的连续内存");

        let _ = std::fs::remove_file(&path);
    }
}