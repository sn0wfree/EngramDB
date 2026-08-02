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
