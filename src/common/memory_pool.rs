//! 内存池与对象复用（P8 优化）
//!
//! 为高频分配的 `Vec<Value>` 和 `Vec<u8>` 提供对象池，
//! 减少 allocator 压力和 page fault，提升写入吞吐。
//!
//! 设计原则：
//! - 简单高效：固定大小池，超出容量直接丢弃让 GC
//! - 线程安全：用 RefCell（单线程场景），避免 Mutex 开销
//! - 透明接入：通过 `take()` / `give_back()` 接口使用

use crate::Value;
use std::cell::RefCell;

/// 值向量池（Vec<Value>）
///
/// 用于 INSERT 批量写入时的行数据缓存，
/// 避免每次分配新 Vec 造成的 allocator 压力。
pub struct ValueVecPool {
    inner: RefCell<Vec<Vec<Value>>>,
    capacity: usize,
    /// 每个向量的预分配元素数
    vec_capacity: usize,
}

impl ValueVecPool {
    pub fn new(pool_capacity: usize, vec_capacity: usize) -> Self {
        Self {
            inner: RefCell::new(Vec::with_capacity(pool_capacity)),
            capacity: pool_capacity,
            vec_capacity,
        }
    }

    /// 从池中取出一个向量
    ///
    /// 池空时分配新向量。
    pub fn take(&self) -> Vec<Value> {
        let mut pool = self.inner.borrow_mut();
        pool.pop().unwrap_or_else(|| Vec::with_capacity(self.vec_capacity))
    }

    /// 归还向量到池中
    ///
    /// 向量会被清空但保留分配的内存。
    /// 池满时直接丢弃。
    pub fn give_back(&self, mut vec: Vec<Value>) {
        let mut pool = self.inner.borrow_mut();
        if pool.len() < self.capacity {
            vec.clear();
            // 限制单个向量的容量，防止超大向量常驻
            if vec.capacity() > self.vec_capacity * 4 {
                vec.shrink_to(self.vec_capacity);
            }
            pool.push(vec);
        }
    }

    /// 池大小
    pub fn len(&self) -> usize {
        self.inner.borrow().len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.borrow().is_empty()
    }
}

/// 字节缓冲区池（Vec<u8>）
///
/// 用于 WAL 写入、序列化等场景的临时缓冲区。
pub struct ByteBufPool {
    inner: RefCell<Vec<Vec<u8>>>,
    capacity: usize,
    buf_capacity: usize,
}

impl ByteBufPool {
    pub fn new(pool_capacity: usize, buf_capacity: usize) -> Self {
        Self {
            inner: RefCell::new(Vec::with_capacity(pool_capacity)),
            capacity: pool_capacity,
            buf_capacity,
        }
    }

    pub fn take(&self) -> Vec<u8> {
        let mut pool = self.inner.borrow_mut();
        pool.pop().unwrap_or_else(|| Vec::with_capacity(self.buf_capacity))
    }

    pub fn give_back(&self, mut buf: Vec<u8>) {
        let mut pool = self.inner.borrow_mut();
        if pool.len() < self.capacity {
            buf.clear();
            if buf.capacity() > self.buf_capacity * 4 {
                buf.shrink_to(self.buf_capacity);
            }
            pool.push(buf);
        }
    }

    pub fn len(&self) -> usize {
        self.inner.borrow().len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.borrow().is_empty()
    }
}

/// 批量行数据池（Vec<Vec<Value>>）
///
/// 用于 INSERT 批量写入时的整批数据容器。
pub struct RowBatchPool {
    inner: RefCell<Vec<Vec<Vec<Value>>>>,
    capacity: usize,
    batch_size: usize,
    row_width: usize,
}

impl RowBatchPool {
    pub fn new(pool_capacity: usize, batch_size: usize, row_width: usize) -> Self {
        Self {
            inner: RefCell::new(Vec::with_capacity(pool_capacity)),
            capacity: pool_capacity,
            batch_size,
            row_width,
        }
    }

    /// 取出一个预分配好的行批次
    ///
    /// 注意：返回的 Vec 是空的，但已预分配容量。
    pub fn take(&self) -> Vec<Vec<Value>> {
        let mut pool = self.inner.borrow_mut();
        pool.pop().unwrap_or_else(|| Vec::with_capacity(self.batch_size))
    }

    /// 归还行批次
    pub fn give_back(&self, mut batch: Vec<Vec<Value>>) {
        let mut pool = self.inner.borrow_mut();
        if pool.len() < self.capacity {
            // 清空所有行向量并回收
            for row in batch.drain(..) {
                // 单行直接丢弃（由 Value 的 drop 释放），
                // 池只保留外层 Vec 结构
                let _ = row;
            }
            batch.clear();
            if batch.capacity() > self.batch_size * 4 {
                batch.shrink_to(self.batch_size);
            }
            pool.push(batch);
        }
    }

    pub fn len(&self) -> usize {
        self.inner.borrow().len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.borrow().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_value_vec_pool() {
        let pool = ValueVecPool::new(4, 16);
        assert!(pool.is_empty());

        let v1 = pool.take();
        assert_eq!(v1.capacity(), 16);
        assert_eq!(pool.len(), 0);

        pool.give_back(v1);
        assert_eq!(pool.len(), 1);

        let v2 = pool.take();
        assert_eq!(v2.capacity(), 16);
        assert!(v2.is_empty());
        assert_eq!(pool.len(), 0);
    }

    #[test]
    fn test_value_vec_pool_overflow() {
        let pool = ValueVecPool::new(2, 8);
        let v1 = pool.take();
        let v2 = pool.take();
        let v3 = pool.take();

        pool.give_back(v1);
        pool.give_back(v2);
        pool.give_back(v3); // 超出容量，丢弃

        assert_eq!(pool.len(), 2);
    }

    #[test]
    fn test_byte_buf_pool() {
        let pool = ByteBufPool::new(4, 1024);
        let buf = pool.take();
        assert_eq!(buf.capacity(), 1024);
        pool.give_back(buf);
        assert_eq!(pool.len(), 1);
    }

    #[test]
    fn test_row_batch_pool() {
        let pool = RowBatchPool::new(2, 1000, 5);
        let batch = pool.take();
        assert_eq!(batch.capacity(), 1000);
        pool.give_back(batch);
        assert_eq!(pool.len(), 1);
    }
}
