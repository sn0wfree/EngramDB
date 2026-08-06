//! 缓冲池（Buffer Pool）
//!
//! 管理内存中的页面缓存，减少磁盘 I/O

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::common::error::Result;

/// 页号
pub type PageId = u32;

/// 页面数据
#[derive(Debug, Clone)]
pub struct Page {
    pub id: PageId,
    pub data: Vec<u8>,
    pub is_dirty: bool,
    pub pin_count: u32,
}

impl Page {
    pub fn new(id: PageId, size: usize) -> Self {
        Self {
            id,
            data: vec![0u8; size],
            is_dirty: false,
            pin_count: 0,
        }
    }
}

/// 缓冲池
pub struct BufferPool {
    page_size: usize,
    capacity: usize,
    pages: HashMap<PageId, Page>,
    access_order: Vec<PageId>, // LRU 近似
    file_path: std::path::PathBuf,
}

impl BufferPool {
    pub fn new(page_size: usize, capacity: usize, file_path: &Path) -> Self {
        // 确保数据文件存在（flush/load 依赖；创建不写任何内容）
        if let Some(parent) = file_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(file_path);
        Self {
            page_size,
            capacity,
            pages: HashMap::with_capacity(capacity),
            access_order: Vec::with_capacity(capacity),
            file_path: file_path.to_path_buf(),
        }
    }

    /// 获取页面（如不在内存则从磁盘读取）
    pub fn get_page(&mut self, page_id: PageId) -> Result<&Page> {
        if !self.pages.contains_key(&page_id) {
            self.load_page(page_id)?;
        }
        self.touch(page_id);
        Ok(&self.pages[&page_id])
    }

    /// 获取可变页面
    pub fn get_page_mut(&mut self, page_id: PageId) -> Result<&mut Page> {
        if !self.pages.contains_key(&page_id) {
            self.load_page(page_id)?;
        }
        self.touch(page_id);
        let page = self.pages.get_mut(&page_id).unwrap();
        page.is_dirty = true;
        Ok(page)
    }

    /// 创建新页面
    pub fn new_page(&mut self, page_id: PageId) -> Result<&mut Page> {
        if self.pages.len() >= self.capacity {
            self.evict()?;
        }
        let page = Page::new(page_id, self.page_size);
        self.pages.insert(page_id, page);
        self.access_order.push(page_id);
        Ok(self.pages.get_mut(&page_id).unwrap())
    }

    /// 固定页面（防止被驱逐）
    pub fn pin(&mut self, page_id: PageId) {
        if let Some(page) = self.pages.get_mut(&page_id) {
            page.pin_count += 1;
        }
    }

    /// 取消固定
    pub fn unpin(&mut self, page_id: PageId) {
        if let Some(page) = self.pages.get_mut(&page_id) {
            if page.pin_count > 0 {
                page.pin_count -= 1;
            }
        }
    }

    /// 刷新所有脏页到磁盘
    pub fn flush_all(&mut self) -> Result<()> {
        let dirty_pages: Vec<PageId> = self.pages
            .iter()
            .filter(|(_, p)| p.is_dirty)
            .map(|(id, _)| *id)
            .collect();

        for page_id in dirty_pages {
            self.flush_page(page_id)?;
        }
        Ok(())
    }

    /// 刷新单页
    fn flush_page(&mut self, page_id: PageId) -> Result<()> {
        if let Some(page) = self.pages.get(&page_id) {
            if page.is_dirty {
                let mut file = std::fs::OpenOptions::new()
                    .write(true)
                    .open(&self.file_path)?;
                file.seek(SeekFrom::Start((page_id as u64) * (self.page_size as u64)))?;
                file.write_all(&page.data)?;
                file.sync_all()?;
            }
        }
        Ok(())
    }

    fn load_page(&mut self, page_id: PageId) -> Result<()> {
        if self.pages.len() >= self.capacity {
            self.evict()?;
        }

        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .open(&self.file_path)?;

        let offset = (page_id as u64) * (self.page_size as u64);
        let file_size = file.metadata()?.len();

        let mut page = Page::new(page_id, self.page_size);

        if offset < file_size {
            file.seek(SeekFrom::Start(offset))?;
            let bytes_to_read = std::cmp::min(self.page_size as u64, file_size - offset) as usize;
            file.read_exact(&mut page.data[..bytes_to_read])?;
        }

        self.pages.insert(page_id, page);
        self.access_order.push(page_id);
        Ok(())
    }

    fn evict(&mut self) -> Result<()> {
        // LRU 驱逐：找最久未访问且未被 pin 的页面
        let mut evict_id: Option<PageId> = None;
        for &id in &self.access_order {
            if let Some(page) = self.pages.get(&id) {
                if page.pin_count == 0 {
                    evict_id = Some(id);
                    break;
                }
            }
        }

        if let Some(id) = evict_id {
            self.flush_page(id)?;
            self.pages.remove(&id);
            self.access_order.retain(|&x| x != id);
        }
        Ok(())
    }

    fn touch(&mut self, page_id: PageId) {
        // 移到末尾（最近访问）
        self.access_order.retain(|&x| x != page_id);
        self.access_order.push(page_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("engramdb_bp_{}_{}.bin", name, std::process::id()));
        p
    }

    #[test]
    fn test_new_page_get_roundtrip() {
        let path = tmp("roundtrip");
        let _ = std::fs::remove_file(&path);
        {
            let mut bp = BufferPool::new(64, 8, &path);
            let page = bp.new_page(1).unwrap();
            page.data[0..5].copy_from_slice(b"hello");
            let p2 = bp.get_page(1).unwrap();
            assert_eq!(&p2.data[..5], b"hello");
            assert_eq!(p2.id, 1);
            assert!(!p2.is_dirty, "new_page 不标脏");
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_get_page_mut_marks_dirty() {
        let path = tmp("dirty");
        let _ = std::fs::remove_file(&path);
        {
            let mut bp = BufferPool::new(64, 8, &path);
            bp.new_page(1).unwrap();
            let p = bp.get_page_mut(1).unwrap();
            p.data[0] = 42;
            assert!(p.is_dirty);
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_persist_and_reload() {
        let path = tmp("persist");
        let _ = std::fs::remove_file(&path);
        {
            let mut bp = BufferPool::new(64, 8, &path);
            bp.new_page(3).unwrap();
            {
                let p = bp.get_page_mut(3).unwrap();
                p.data[0..4].copy_from_slice(&[9, 8, 7, 6]);
            }
            bp.flush_all().unwrap();
        }
        // 新缓冲池从磁盘加载
        {
            let mut bp2 = BufferPool::new(64, 8, &path);
            let p = bp2.get_page(3).unwrap();
            assert_eq!(&p.data[..4], &[9, 8, 7, 6], "脏页应已落盘");
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_page_offset_correctness() {
        // page_id 偏移：page_id * page_size
        let path = tmp("offset");
        let _ = std::fs::remove_file(&path);
        {
            let mut bp = BufferPool::new(64, 8, &path);
            bp.new_page(0).unwrap();
            bp.new_page(1).unwrap();
            bp.new_page(2).unwrap();
            {
                let p = bp.get_page_mut(2).unwrap();
                p.data[0..3].copy_from_slice(b"p2!");
            }
            bp.flush_all().unwrap();
        }
        {
            let mut bp2 = BufferPool::new(64, 8, &path);
            let p2 = bp2.get_page(2).unwrap();
            assert_eq!(&p2.data[..3], b"p2!", "页 2 应从偏移 128 读取");
            let p0 = bp2.get_page(0).unwrap();
            assert_eq!(&p0.data[..3], b"\0\0\0", "页 0 未写数据应为零");
        }
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_lru_eviction() {
        let path = tmp("lru");
        let _ = std::fs::remove_file(&path);
        let mut bp = BufferPool::new(64, 3, &path);
        bp.new_page(1).unwrap();
        bp.new_page(2).unwrap();
        bp.new_page(3).unwrap();
        // 访问 1、2（3 是最久未访问）
        bp.get_page(1).unwrap();
        bp.get_page(2).unwrap();
        // 新页 4 → 驱逐 3
        bp.new_page(4).unwrap();
        assert!(!bp.pages.contains_key(&3), "LRU 应驱逐最久未访问的页 3");
        assert!(bp.pages.contains_key(&1) && bp.pages.contains_key(&2) && bp.pages.contains_key(&4));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_pin_prevents_eviction() {
        let path = tmp("pin");
        let _ = std::fs::remove_file(&path);
        let mut bp = BufferPool::new(64, 3, &path);
        bp.new_page(1).unwrap();
        bp.new_page(2).unwrap();
        bp.new_page(3).unwrap();
        bp.pin(3);
        bp.new_page(4).unwrap(); // 3 被 pin：驱逐最老的 1
        assert!(bp.pages.contains_key(&3), "pin 的页不得被驱逐");
        assert!(!bp.pages.contains_key(&1));
        // unpin 后 3 变为可驱逐：后续填充时被逐出
        bp.unpin(3);
        bp.new_page(5).unwrap(); // 驱逐 2
        bp.new_page(6).unwrap(); // 驱逐 3
        assert!(!bp.pages.contains_key(&3), "unpin 后可被驱逐");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_unpin_never_negative() {
        let path = tmp("unpin");
        let _ = std::fs::remove_file(&path);
        let mut bp = BufferPool::new(64, 8, &path);
        bp.new_page(1).unwrap();
        bp.pin(1);
        bp.pin(1);
        bp.unpin(1);
        bp.unpin(1);
        assert_eq!(bp.pages[&1].pin_count, 0);
        bp.unpin(1); // 过量 unpin 不产生负数
        assert_eq!(bp.pages[&1].pin_count, 0);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_missing_page_loads_from_disk() {
        let path = tmp("load");
        let _ = std::fs::remove_file(&path);
        {
            let mut bp = BufferPool::new(64, 8, &path);
            bp.new_page(7).unwrap();
            {
                let p = bp.get_page_mut(7).unwrap();
                p.data[0] = 77;
            }
            bp.flush_all().unwrap();
        }
        // 完全新的缓冲池：get_page 触发磁盘加载
        {
            let mut bp = BufferPool::new(64, 8, &path);
            let p = bp.get_page(7).unwrap();
            assert_eq!(p.data[0], 77);
            // 不存在的页：加载零页
            let p9 = bp.get_page(9).unwrap();
            assert!(p9.data.iter().all(|&b| b == 0));
        }
        let _ = std::fs::remove_file(&path);
    }
}
