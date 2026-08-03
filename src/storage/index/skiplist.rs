//! 跳表索引 (Skip List Index)
//!
//! 有序二级索引，支持：
//! - 点查询：O(log n)
//! - 范围查询：O(log n + k)，k 为结果数
//! - 插入：O(log n) 均摊
//! - 覆盖索引：索引中冗余存储高频列，查询不用回表
//!
//! 相比 B+Tree：
//! - 实现更简单（无节点分裂/合并）
//! - 内存友好（节点大小可变，无内部浪费）
//! - 并发性能更好（锁粒度更细）

use crate::Value;
use crate::common::error::{Result, EngramDbError};
use rand::Rng;
use std::cmp::Ordering;

/// 跳表最大层数
const MAX_LEVEL: u8 = 32;

/// 层数概率因子（p = 1/4，类似 Redis 跳表）
const P: f64 = 0.25;

/// 索引条目（行 ID + 覆盖列的值）
#[derive(Debug, Clone)]
pub struct IndexEntry {
    /// 行 ID
    pub row_id: u32,
    /// 覆盖列的值，长度等于索引的 num_included
    /// num_included = 0 时为空 Vec
    pub included: Vec<Value>,
}

/// 跳表节点
#[derive(Debug, Clone)]
struct Node {
    /// 键值（索引列的值）
    key: Value,
    /// 条目列表（非唯一索引可能有多个行）
    entries: Vec<IndexEntry>,
    /// 每层的 forward 指针（下一个节点）
    forward: Vec<Option<usize>>,
}

/// 跳表索引
///
/// 使用 arena（Vec）管理节点，避免指针操作和额外分配开销。
/// 节点 0 是哨兵头节点（key 为最小值）。
///
/// 覆盖索引：
/// - num_included > 0 时，每个索引条目冗余存储 num_included 列的值
/// - 查询这些列时可以直接从索引返回，不需要回表
/// - 典型场景：session_id 索引覆盖 timestamp、role 等高频列
#[derive(Debug, Clone)]
pub struct SkipListIndex {
    /// 节点 arena
    nodes: Vec<Node>,
    /// 当前最大层数（0-based，0 表示只有一层）
    level: u8,
    /// 元素个数（不同 key 的数量）
    len: usize,
    /// 是否唯一索引
    unique: bool,
    /// 覆盖列的数量（0 表示普通索引）
    num_included: usize,
}

impl SkipListIndex {
    /// 创建空的跳表索引（普通索引，无覆盖列）
    pub fn new(unique: bool) -> Self {
        Self::with_included(unique, 0)
    }

    /// 创建带覆盖列的跳表索引
    ///
    /// num_included: 覆盖列的数量。插入时必须提供对应数量的包含列值。
    pub fn with_included(unique: bool, num_included: usize) -> Self {
        let head = Node {
            key: Value::Null, // Null 视为最小
            entries: Vec::new(),
            forward: vec![None; MAX_LEVEL as usize],
        };

        Self {
            nodes: vec![head],
            level: 0,
            len: 0,
            unique,
            num_included,
        }
    }

    /// 获取覆盖列数量
    pub fn num_included(&self) -> usize {
        self.num_included
    }

    /// 随机生成层数
    fn random_level() -> u8 {
        let mut level = 0u8;
        let mut rng = rand::thread_rng();
        while level < MAX_LEVEL - 1 && rng.gen::<f64>() < P {
            level += 1;
        }
        level
    }

    /// 插入一条索引记录（普通索引，兼容旧 API）
    pub fn insert(&mut self, key: Value, row_id: u32) -> bool {
        self.insert_with_included(key, row_id, &[])
    }

    /// 插入一条索引记录（带覆盖列值）
    ///
    /// included_values 长度必须等于 num_included。
    pub fn insert_with_included(&mut self, key: Value, row_id: u32, included_values: &[Value]) -> bool {
        debug_assert_eq!(included_values.len(), self.num_included,
            "included_values length mismatch: expected {}, got {}", self.num_included, included_values.len());

        let mut update = vec![0usize; MAX_LEVEL as usize];
        let mut x = 0;

        for i in (0..=self.level as usize).rev() {
            loop {
                match self.nodes[x].forward[i] {
                    Some(next) if self.key_less(&self.nodes[next].key, &key) => {
                        x = next;
                    }
                    _ => break,
                }
            }
            update[i] = x;
        }

        let x_next = self.nodes[x].forward[0];

        match x_next {
            Some(next_idx) if self.nodes[next_idx].key == key => {
                if self.unique {
                    return false;
                }
                self.nodes[next_idx].entries.push(IndexEntry {
                    row_id,
                    included: included_values.to_vec(),
                });
                true
            }
            _ => {
                let new_level = Self::random_level();

                if new_level > self.level {
                    for i in (self.level as usize + 1)..=new_level as usize {
                        update[i] = 0;
                    }
                    self.level = new_level;
                }

                let new_node = Node {
                    key: key.clone(),
                    entries: vec![IndexEntry {
                        row_id,
                        included: included_values.to_vec(),
                    }],
                    forward: vec![None; (new_level + 1) as usize],
                };

                let new_idx = self.nodes.len();
                self.nodes.push(new_node);

                for i in 0..=new_level as usize {
                    self.nodes[new_idx].forward[i] = self.nodes[update[i]].forward[i];
                    self.nodes[update[i]].forward[i] = Some(new_idx);
                }

                self.len += 1;
                true
            }
        }
    }

    /// 点查询：查找 key 对应的行号（兼容旧 API）
    pub fn get(&self, key: &Value) -> Option<Vec<u32>> {
        self.get_entries(key).map(|entries| {
            entries.iter().map(|e| e.row_id).collect()
        })
    }

    /// 点查询：返回条目列表（含覆盖列值）
    pub fn get_entries(&self, key: &Value) -> Option<&[IndexEntry]> {
        let mut x = 0;

        for i in (0..=self.level as usize).rev() {
            loop {
                match self.nodes[x].forward[i] {
                    Some(next) if self.key_less(&self.nodes[next].key, key) => {
                        x = next;
                    }
                    _ => break,
                }
            }
        }

        match self.nodes[x].forward[0] {
            Some(next) if &self.nodes[next].key == key => {
                Some(&self.nodes[next].entries)
            }
            _ => None,
        }
    }

    /// 范围查询：[low, high] 闭区间（返回行 ID 列表，兼容旧 API）
    pub fn range(&self, low: &Value, high: &Value) -> Vec<u32> {
        self.range_entries(low, high).into_iter().map(|e| e.row_id).collect()
    }

    /// 范围查询：返回条目列表（含覆盖列值）
    pub fn range_entries(&self, low: &Value, high: &Value) -> Vec<IndexEntry> {
        let mut result = Vec::new();
        let mut x = 0;

        for i in (0..=self.level as usize).rev() {
            loop {
                match self.nodes[x].forward[i] {
                    Some(next) if self.key_less(&self.nodes[next].key, low) => {
                        x = next;
                    }
                    _ => break,
                }
            }
        }

        let mut current = self.nodes[x].forward[0];
        while let Some(idx) = current {
            let node = &self.nodes[idx];
            if self.key_greater(&node.key, high) {
                break;
            }
            result.extend(node.entries.iter().cloned());
            current = node.forward[0];
        }

        result
    }

    /// 大于等于 key 的第一个值（兼容旧 API，返回 key 和行 ID 列表）
    pub fn lower_bound(&self, key: &Value) -> Option<(&Value, Vec<u32>)> {
        self.lower_bound_entries(key).map(|(k, entries)| {
            (k, entries.iter().map(|e| e.row_id).collect())
        })
    }

    /// 大于等于 key 的第一个值（返回条目切片，含覆盖列）
    pub fn lower_bound_entries(&self, key: &Value) -> Option<(&Value, &[IndexEntry])> {
        let mut x = 0;

        for i in (0..=self.level as usize).rev() {
            loop {
                match self.nodes[x].forward[i] {
                    Some(next) if self.key_less(&self.nodes[next].key, key) => {
                        x = next;
                    }
                    _ => break,
                }
            }
        }

        match self.nodes[x].forward[0] {
            Some(next) => Some((&self.nodes[next].key, &self.nodes[next].entries)),
            None => None,
        }
    }

    /// 元素个数（不同 key 的数量）
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// 是否唯一索引
    pub fn is_unique(&self) -> bool {
        self.unique
    }

    /// 删除指定 key 和 row_id 的索引条目（v0.12.0 新增，DELETE/UPDATE 索引维护）
    ///
    /// 返回 true 表示成功删除，false 表示未找到。
    /// 如果删除后该 key 的 entries 为空，则整个节点也被移除。
    pub fn remove(&mut self, key: &Value, row_id: u32) -> bool {
        let mut update = vec![0usize; MAX_LEVEL as usize];
        let mut x = 0;

        // 找到 key 对应的节点
        for i in (0..=self.level as usize).rev() {
            loop {
                match self.nodes[x].forward[i] {
                    Some(next) if self.key_less(&self.nodes[next].key, key) => {
                        x = next;
                    }
                    _ => break,
                }
            }
            update[i] = x;
        }

        let x_next = match self.nodes[x].forward[0] {
            Some(idx) => idx,
            None => return false,
        };

        if self.nodes[x_next].key != *key {
            return false;
        }

        // 在 entries 中查找并移除指定 row_id
        let entries = &mut self.nodes[x_next].entries;
        let before_len = entries.len();
        entries.retain(|e| e.row_id != row_id);
        let removed = entries.len() < before_len;

        if !removed {
            return false;
        }

        // 如果 entries 为空，删除整个跳表节点
        if entries.is_empty() {
            let node_level = self.nodes[x_next].forward.len() - 1;

            // 更新各层的 forward 指针（只到被删节点的 level）
            for i in 0..=node_level {
                self.nodes[update[i]].forward[i] = self.nodes[x_next].forward[i];
            }

            // 回收节点：标记为空 key，清空 entries 和 forward
            self.nodes[x_next].key = Value::Null;
            self.nodes[x_next].entries.clear();
            self.nodes[x_next].forward.clear();

            // 更新 level（如果删除的是最高层节点）
            while self.level > 0 {
                match self.nodes[0].forward.get(self.level as usize) {
                    Some(&Some(_)) => break,
                    _ => self.level -= 1,
                }
            }

            self.len -= 1;
        }

        true
    }

    // --- 比较辅助函数 ---

    fn key_less(&self, a: &Value, b: &Value) -> bool {
        match (a, b) {
            (Value::Null, Value::Null) => false,
            (Value::Null, _) => true,
            (_, Value::Null) => false,
            (Value::Int64(x), Value::Int64(y)) => x < y,
            (Value::Int32(x), Value::Int32(y)) => x < y,
            (Value::Float64(x), Value::Float64(y)) => x < y,
            (Value::Varchar(x), Value::Varchar(y)) => x < y,
            (Value::Boolean(x), Value::Boolean(y)) => !*x && *y,
            _ => false,
        }
    }

    fn key_greater(&self, a: &Value, b: &Value) -> bool {
        self.key_less(b, a)
    }

    /// 迭代器：按顺序遍历所有 key（兼容旧 API）
    pub fn iter(&self) -> SkipListIter<'_> {
        SkipListIter {
            list: self,
            current: self.nodes[0].forward[0],
        }
    }
}

/// 跳表迭代器
pub struct SkipListIter<'a> {
    list: &'a SkipListIndex,
    current: Option<usize>,
}

impl<'a> Iterator for SkipListIter<'a> {
    type Item = (&'a Value, Vec<u32>);

    fn next(&mut self) -> Option<Self::Item> {
        match self.current {
            Some(idx) => {
                let node = &self.list.nodes[idx];
                self.current = node.forward[0];
                let row_ids: Vec<u32> = node.entries.iter().map(|e| e.row_id).collect();
                Some((&node.key, row_ids))
            }
            None => None,
        }
    }
}

// ============================================================================
// 持久化：序列化 / 反序列化
// ============================================================================

/// 索引段魔数
const INDEX_MAGIC: &[u8; 8] = b"SKIPIDX1";

/// Value 类型标签（单值自描述序列化）
mod value_tag {
    pub const NULL: u8 = 0;
    pub const BOOLEAN: u8 = 1;
    pub const INT32: u8 = 2;
    pub const INT64: u8 = 3;
    pub const FLOAT64: u8 = 4;
    pub const VARCHAR: u8 = 5;
    pub const JSON: u8 = 6;
    pub const VECTOR: u8 = 7;
    pub const BLOB: u8 = 8;
}

/// 将单个 Value 编码为自描述字节（type_tag + data）
fn encode_value(v: &Value) -> Vec<u8> {
    let mut buf = Vec::new();
    match v {
        Value::Null => {
            buf.push(value_tag::NULL);
        }
        Value::Boolean(b) => {
            buf.push(value_tag::BOOLEAN);
            buf.push(if *b { 1 } else { 0 });
        }
        Value::Int32(i) => {
            buf.push(value_tag::INT32);
            buf.extend_from_slice(&i.to_le_bytes());
        }
        Value::Int64(i) => {
            buf.push(value_tag::INT64);
            buf.extend_from_slice(&i.to_le_bytes());
        }
        Value::Float64(f) => {
            buf.push(value_tag::FLOAT64);
            buf.extend_from_slice(&f.to_le_bytes());
        }
        Value::Varchar(s) => {
            buf.push(value_tag::VARCHAR);
            buf.extend_from_slice(&(s.len() as u32).to_le_bytes());
            buf.extend_from_slice(s.as_bytes());
        }
        Value::Json(s) => {
            buf.push(value_tag::JSON);
            buf.extend_from_slice(&(s.len() as u32).to_le_bytes());
            buf.extend_from_slice(s.as_bytes());
        }
        Value::Vector(vec) => {
            buf.push(value_tag::VECTOR);
            buf.extend_from_slice(&(vec.len() as u32).to_le_bytes());
            for f in vec {
                buf.extend_from_slice(&f.to_le_bytes());
            }
        }
        Value::Blob(b) => {
            buf.push(value_tag::BLOB);
            buf.extend_from_slice(&(b.len() as u32).to_le_bytes());
            buf.extend_from_slice(b);
        }
    }
    buf
}

/// 从字节解码单个 Value，返回 (value, bytes_consumed)
fn decode_value(data: &[u8]) -> Result<(Value, usize)> {
    if data.is_empty() {
        return Err(EngramDbError::InvalidFormat("empty value data".into()));
    }
    let tag = data[0];
    let mut offset = 1;

    let value = match tag {
        value_tag::NULL => Value::Null,
        value_tag::BOOLEAN => {
            if offset >= data.len() {
                return Err(EngramDbError::InvalidFormat("truncated boolean value".into()));
            }
            let b = data[offset] != 0;
            offset += 1;
            Value::Boolean(b)
        }
        value_tag::INT32 => {
            if offset + 4 > data.len() {
                return Err(EngramDbError::InvalidFormat("truncated int32 value".into()));
            }
            let i = i32::from_le_bytes(data[offset..offset+4].try_into().unwrap());
            offset += 4;
            Value::Int32(i)
        }
        value_tag::INT64 => {
            if offset + 8 > data.len() {
                return Err(EngramDbError::InvalidFormat("truncated int64 value".into()));
            }
            let i = i64::from_le_bytes(data[offset..offset+8].try_into().unwrap());
            offset += 8;
            Value::Int64(i)
        }
        value_tag::FLOAT64 => {
            if offset + 8 > data.len() {
                return Err(EngramDbError::InvalidFormat("truncated float64 value".into()));
            }
            let f = f64::from_le_bytes(data[offset..offset+8].try_into().unwrap());
            offset += 8;
            Value::Float64(f)
        }
        value_tag::VARCHAR => {
            if offset + 4 > data.len() {
                return Err(EngramDbError::InvalidFormat("truncated varchar length".into()));
            }
            let len = u32::from_le_bytes(data[offset..offset+4].try_into().unwrap()) as usize;
            offset += 4;
            if offset + len > data.len() {
                return Err(EngramDbError::InvalidFormat("truncated varchar data".into()));
            }
            let s = String::from_utf8(data[offset..offset+len].to_vec())
                .map_err(|e| EngramDbError::InvalidFormat(format!("invalid utf8: {}", e)))?;
            offset += len;
            Value::Varchar(s)
        }
        value_tag::JSON => {
            if offset + 4 > data.len() {
                return Err(EngramDbError::InvalidFormat("truncated json length".into()));
            }
            let len = u32::from_le_bytes(data[offset..offset+4].try_into().unwrap()) as usize;
            offset += 4;
            if offset + len > data.len() {
                return Err(EngramDbError::InvalidFormat("truncated json data".into()));
            }
            let s = String::from_utf8(data[offset..offset+len].to_vec())
                .map_err(|e| EngramDbError::InvalidFormat(format!("invalid utf8: {}", e)))?;
            offset += len;
            Value::Json(s)
        }
        value_tag::VECTOR => {
            if offset + 4 > data.len() {
                return Err(EngramDbError::InvalidFormat("truncated vector dim".into()));
            }
            let dim = u32::from_le_bytes(data[offset..offset+4].try_into().unwrap()) as usize;
            offset += 4;
            let byte_len = dim * 4;
            if offset + byte_len > data.len() {
                return Err(EngramDbError::InvalidFormat("truncated vector data".into()));
            }
            let mut vec = Vec::with_capacity(dim);
            for i in 0..dim {
                let start = offset + i * 4;
                let f = f32::from_le_bytes(data[start..start+4].try_into().unwrap());
                vec.push(f);
            }
            offset += byte_len;
            Value::Vector(vec)
        }
        value_tag::BLOB => {
            if offset + 4 > data.len() {
                return Err(EngramDbError::InvalidFormat("truncated blob length".into()));
            }
            let len = u32::from_le_bytes(data[offset..offset+4].try_into().unwrap()) as usize;
            offset += 4;
            if offset + len > data.len() {
                return Err(EngramDbError::InvalidFormat("truncated blob data".into()));
            }
            let blob = data[offset..offset+len].to_vec();
            offset += len;
            Value::Blob(blob)
        }
        _ => return Err(EngramDbError::InvalidFormat(format!("unknown value tag: {}", tag))),
    };

    Ok((value, offset))
}

impl SkipListIndex {
    /// 序列化为字节（v0.12.0 索引持久化）
    ///
    /// 格式：按有序键值对序列化，反序列化时重建跳表结构。
    /// 跳表是概率数据结构，重建后层数可能不同但数据完全一致。
    ///
    /// 二进制格式：
    /// - magic: [u8; 8] = "SKIPIDX1"
    /// - flags: u8 (bit0 = unique)
    /// - num_included: u32
    /// - key_count: u32
    /// - 重复 key_count 次：
    ///   - key: encode_value(...)
    ///   - entry_count: u32
    ///   - 重复 entry_count 次：
    ///     - row_id: u32
    ///     - 重复 num_included 次：
    ///       - included_value: encode_value(...)
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(1024 + self.len * 32);

        // Magic
        buf.extend_from_slice(INDEX_MAGIC);

        // Flags
        let mut flags: u8 = 0;
        if self.unique {
            flags |= 1 << 0;
        }
        buf.push(flags);

        // num_included
        buf.extend_from_slice(&(self.num_included as u32).to_le_bytes());

        // key_count
        buf.extend_from_slice(&(self.len as u32).to_le_bytes());

        // 直接遍历 level-0 链表，按键顺序序列化所有 key + entries
        let mut current = self.nodes[0].forward[0];
        while let Some(idx) = current {
            let node = &self.nodes[idx];

            // key
            buf.extend_from_slice(&encode_value(&node.key));

            // entry_count
            buf.extend_from_slice(&(node.entries.len() as u32).to_le_bytes());

            // entries
            for entry in &node.entries {
                // row_id
                buf.extend_from_slice(&entry.row_id.to_le_bytes());

                // included values
                debug_assert_eq!(entry.included.len(), self.num_included);
                for inc_val in &entry.included {
                    buf.extend_from_slice(&encode_value(inc_val));
                }
            }

            current = node.forward[0];
        }

        buf
    }

    /// 从字节反序列化重建跳表索引（v0.12.0 索引持久化）
    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        if data.len() < 8 {
            return Err(EngramDbError::InvalidFormat("index data too short".into()));
        }

        // 校验魔数
        if &data[..8] != INDEX_MAGIC {
            return Err(EngramDbError::InvalidFormat(
                "invalid index magic number".into()
            ));
        }
        let mut offset = 8;

        // Flags
        if offset >= data.len() {
            return Err(EngramDbError::InvalidFormat("truncated index flags".into()));
        }
        let flags = data[offset];
        let unique = (flags & 1) != 0;
        offset += 1;

        // num_included
        if offset + 4 > data.len() {
            return Err(EngramDbError::InvalidFormat("truncated num_included".into()));
        }
        let num_included = u32::from_le_bytes(data[offset..offset+4].try_into().unwrap()) as usize;
        offset += 4;

        // key_count
        if offset + 4 > data.len() {
            return Err(EngramDbError::InvalidFormat("truncated key_count".into()));
        }
        let key_count = u32::from_le_bytes(data[offset..offset+4].try_into().unwrap()) as usize;
        offset += 4;

        // 构建跳表
        let mut sl = SkipListIndex::with_included(unique, num_included);

        for _ in 0..key_count {
            // key
            let (key, consumed) = decode_value(&data[offset..])?;
            offset += consumed;

            // entry_count
            if offset + 4 > data.len() {
                return Err(EngramDbError::InvalidFormat("truncated entry_count".into()));
            }
            let entry_count = u32::from_le_bytes(data[offset..offset+4].try_into().unwrap()) as usize;
            offset += 4;

            for _ in 0..entry_count {
                // row_id
                if offset + 4 > data.len() {
                    return Err(EngramDbError::InvalidFormat("truncated row_id".into()));
                }
                let row_id = u32::from_le_bytes(data[offset..offset+4].try_into().unwrap());
                offset += 4;

                // included values
                let mut included_vals = Vec::with_capacity(num_included);
                for _ in 0..num_included {
                    let (val, consumed) = decode_value(&data[offset..])?;
                    offset += consumed;
                    included_vals.push(val);
                }

                // 插入跳表
                sl.insert_with_included(key.clone(), row_id, &included_vals);
            }
        }

        Ok(sl)
    }
}

// ============================================================================
// 测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_empty() {
        let sl = SkipListIndex::new(false);
        assert!(sl.is_empty());
        assert_eq!(sl.len(), 0);
        assert!(!sl.is_unique());
        assert_eq!(sl.num_included(), 0);
    }

    #[test]
    fn test_with_included() {
        let sl = SkipListIndex::with_included(false, 2);
        assert_eq!(sl.num_included(), 2);
        assert!(sl.is_empty());
    }

    #[test]
    fn test_insert_single() {
        let mut sl = SkipListIndex::new(false);
        assert!(sl.insert(Value::Int64(42), 100));
        assert_eq!(sl.len(), 1);
        assert!(!sl.is_empty());
    }

    #[test]
    fn test_get_existing() {
        let mut sl = SkipListIndex::new(false);
        sl.insert(Value::Int64(42), 100);
        let rows = sl.get(&Value::Int64(42)).unwrap();
        assert_eq!(rows, vec![100]);
    }

    #[test]
    fn test_get_nonexistent() {
        let sl = SkipListIndex::new(false);
        assert!(sl.get(&Value::Int64(42)).is_none());
    }

    #[test]
    fn test_non_unique_multiple_rows() {
        let mut sl = SkipListIndex::new(false);
        sl.insert(Value::Int64(42), 100);
        sl.insert(Value::Int64(42), 200);
        sl.insert(Value::Int64(42), 300);
        assert_eq!(sl.len(), 1);
        let rows = sl.get(&Value::Int64(42)).unwrap();
        assert_eq!(rows, vec![100, 200, 300]);
    }

    #[test]
    fn test_unique_conflict() {
        let mut sl = SkipListIndex::new(true);
        assert!(sl.insert(Value::Int64(42), 100));
        assert!(!sl.insert(Value::Int64(42), 200));
        assert_eq!(sl.len(), 1);
    }

    #[test]
    fn test_range_query() {
        let mut sl = SkipListIndex::new(false);
        for i in 0..10u32 {
            sl.insert(Value::Int64(i as i64 * 10), i);
        }

        let result = sl.range(&Value::Int64(30), &Value::Int64(60));
        assert_eq!(result.len(), 4);
        assert_eq!(result[0], 3);
        assert_eq!(result[3], 6);
    }

    #[test]
    fn test_range_empty() {
        let mut sl = SkipListIndex::new(false);
        sl.insert(Value::Int64(10), 1);
        sl.insert(Value::Int64(20), 2);

        let result = sl.range(&Value::Int64(30), &Value::Int64(40));
        assert!(result.is_empty());
    }

    #[test]
    fn test_lower_bound() {
        let mut sl = SkipListIndex::new(false);
        sl.insert(Value::Int64(10), 1);
        sl.insert(Value::Int64(30), 3);
        sl.insert(Value::Int64(50), 5);

        let (key, rows) = sl.lower_bound(&Value::Int64(20)).unwrap();
        assert_eq!(key, &Value::Int64(30));
        assert_eq!(rows, vec![3]);
    }

    #[test]
    fn test_lower_bound_exact() {
        let mut sl = SkipListIndex::new(false);
        sl.insert(Value::Int64(10), 1);
        sl.insert(Value::Int64(20), 2);

        let (key, _) = sl.lower_bound(&Value::Int64(10)).unwrap();
        assert_eq!(key, &Value::Int64(10));
    }

    #[test]
    fn test_iter_ordered() {
        let mut sl = SkipListIndex::new(false);
        sl.insert(Value::Int64(30), 3);
        sl.insert(Value::Int64(10), 1);
        sl.insert(Value::Int64(20), 2);

        let keys: Vec<i64> = sl.iter()
            .map(|(k, _)| match k { Value::Int64(v) => *v, _ => 0 })
            .collect();
        assert_eq!(keys, vec![10, 20, 30]);
    }

    #[test]
    fn test_varchar_keys() {
        let mut sl = SkipListIndex::new(false);
        sl.insert(Value::Varchar("banana".into()), 2);
        sl.insert(Value::Varchar("apple".into()), 1);
        sl.insert(Value::Varchar("cherry".into()), 3);

        let keys: Vec<String> = sl.iter()
            .map(|(k, _)| match k { Value::Varchar(v) => v.clone(), _ => String::new() })
            .collect();
        assert_eq!(keys, vec!["apple", "banana", "cherry"]);
    }

    #[test]
    fn test_boolean_keys() {
        let mut sl = SkipListIndex::new(false);
        sl.insert(Value::Boolean(true), 1);
        sl.insert(Value::Boolean(false), 0);

        let keys: Vec<bool> = sl.iter()
            .map(|(k, _)| match k { Value::Boolean(v) => *v, _ => false })
            .collect();
        assert_eq!(keys, vec![false, true]);
    }

    #[test]
    fn test_large_insert() {
        let mut sl = SkipListIndex::new(false);
        for i in 0..1000 {
            sl.insert(Value::Int64(i), i as u32);
        }
        assert_eq!(sl.len(), 1000);

        for i in &[0, 1, 500, 999, 50, 777] {
            let rows = sl.get(&Value::Int64(*i)).unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0], *i as u32);
        }
    }

    #[test]
    fn test_null_key() {
        let mut sl = SkipListIndex::new(false);
        sl.insert(Value::Null, 0);
        sl.insert(Value::Int64(1), 1);

        let result = sl.range(&Value::Null, &Value::Int64(10));
        assert_eq!(result.len(), 2);
        assert_eq!(result[0], 0);
    }

    #[test]
    fn test_range_all() {
        let mut sl = SkipListIndex::new(false);
        for i in 0..100 {
            sl.insert(Value::Int64(i), i as u32);
        }
        let result = sl.range(&Value::Int64(0), &Value::Int64(99));
        assert_eq!(result.len(), 100);
    }

    // --- 覆盖索引测试 ---

    #[test]
    fn test_covering_insert_and_get() {
        let mut sl = SkipListIndex::with_included(false, 2);
        assert_eq!(sl.num_included(), 2);

        sl.insert_with_included(
            Value::Varchar("session_1".into()), 100,
            &[Value::Int64(1000), Value::Varchar("user".into())]
        );
        sl.insert_with_included(
            Value::Varchar("session_1".into()), 101,
            &[Value::Int64(2000), Value::Varchar("assistant".into())]
        );
        sl.insert_with_included(
            Value::Varchar("session_2".into()), 200,
            &[Value::Int64(3000), Value::Varchar("user".into())]
        );

        let entries = sl.get_entries(&Value::Varchar("session_1".into())).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].row_id, 100);
        assert_eq!(entries[0].included[0], Value::Int64(1000));
        assert_eq!(entries[0].included[1], Value::Varchar("user".into()));
        assert_eq!(entries[1].row_id, 101);
        assert_eq!(entries[1].included[0], Value::Int64(2000));
        assert_eq!(entries[1].included[1], Value::Varchar("assistant".into()));
    }

    #[test]
    fn test_covering_range() {
        let mut sl = SkipListIndex::with_included(false, 1);

        for i in 0..5u32 {
            sl.insert_with_included(
                Value::Int64(i as i64 * 10), i,
                &[Value::Int64(i as i64 * 100)]
            );
        }

        let entries = sl.range_entries(&Value::Int64(10), &Value::Int64(30));
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].row_id, 1);
        assert_eq!(entries[0].included[0], Value::Int64(100));
        assert_eq!(entries[2].row_id, 3);
        assert_eq!(entries[2].included[0], Value::Int64(300));
    }

    #[test]
    fn test_covering_lower_bound() {
        let mut sl = SkipListIndex::with_included(false, 1);
        sl.insert_with_included(Value::Int64(10), 1, &[Value::Int64(100)]);
        sl.insert_with_included(Value::Int64(30), 3, &[Value::Int64(300)]);

        let (key, entries) = sl.lower_bound_entries(&Value::Int64(20)).unwrap();
        assert_eq!(key, &Value::Int64(30));
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].included[0], Value::Int64(300));
    }

    #[test]
    fn test_covering_zero_included_is_normal() {
        // num_included = 0 时行为应与普通索引完全一致
        let mut sl = SkipListIndex::with_included(false, 0);
        sl.insert_with_included(Value::Int64(42), 100, &[]);
        sl.insert_with_included(Value::Int64(42), 200, &[]);

        let entries = sl.get_entries(&Value::Int64(42)).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].row_id, 100);
        assert!(entries[0].included.is_empty());

        // 旧 API 也能用
        let rows = sl.get(&Value::Int64(42)).unwrap();
        assert_eq!(rows, vec![100, 200]);
    }

    #[test]
    fn test_random_level_bounds() {
        for _ in 0..100 {
            let level = SkipListIndex::random_level();
            assert!(level < MAX_LEVEL);
        }
    }

    // --- 持久化测试（v0.12.0） ---

    #[test]
    fn test_serialize_empty_index() {
        let sl = SkipListIndex::new(false);
        let bytes = sl.to_bytes();
        let restored = SkipListIndex::from_bytes(&bytes).unwrap();
        assert!(restored.is_empty());
        assert_eq!(restored.len(), 0);
        assert!(!restored.is_unique());
        assert_eq!(restored.num_included(), 0);
    }

    #[test]
    fn test_serialize_unique_index() {
        let sl = SkipListIndex::new(true);
        let bytes = sl.to_bytes();
        let restored = SkipListIndex::from_bytes(&bytes).unwrap();
        assert!(restored.is_unique());
    }

    #[test]
    fn test_serialize_single_key() {
        let mut sl = SkipListIndex::new(false);
        sl.insert(Value::Int64(42), 100);
        let bytes = sl.to_bytes();
        let restored = SkipListIndex::from_bytes(&bytes).unwrap();
        assert_eq!(restored.len(), 1);
        let rows = restored.get(&Value::Int64(42)).unwrap();
        assert_eq!(rows, vec![100]);
    }

    #[test]
    fn test_serialize_multiple_keys() {
        let mut sl = SkipListIndex::new(false);
        for i in 0..10u32 {
            sl.insert(Value::Int64(i as i64 * 10), i);
        }
        let bytes = sl.to_bytes();
        let restored = SkipListIndex::from_bytes(&bytes).unwrap();
        assert_eq!(restored.len(), 10);

        // 验证范围查询
        let result = restored.range(&Value::Int64(30), &Value::Int64(60));
        assert_eq!(result.len(), 4);
        assert_eq!(result[0], 3);
        assert_eq!(result[3], 6);
    }

    #[test]
    fn test_serialize_non_unique() {
        let mut sl = SkipListIndex::new(false);
        sl.insert(Value::Int64(42), 100);
        sl.insert(Value::Int64(42), 200);
        sl.insert(Value::Int64(42), 300);
        let bytes = sl.to_bytes();
        let restored = SkipListIndex::from_bytes(&bytes).unwrap();
        assert_eq!(restored.len(), 1);
        let rows = restored.get(&Value::Int64(42)).unwrap();
        assert_eq!(rows, vec![100, 200, 300]);
    }

    #[test]
    fn test_serialize_varchar_keys() {
        let mut sl = SkipListIndex::new(false);
        sl.insert(Value::Varchar("banana".into()), 2);
        sl.insert(Value::Varchar("apple".into()), 1);
        sl.insert(Value::Varchar("cherry".into()), 3);
        let bytes = sl.to_bytes();
        let restored = SkipListIndex::from_bytes(&bytes).unwrap();

        let keys: Vec<String> = restored.iter()
            .map(|(k, _)| match k { Value::Varchar(v) => v.clone(), _ => String::new() })
            .collect();
        assert_eq!(keys, vec!["apple", "banana", "cherry"]);
    }

    #[test]
    fn test_serialize_covering_index() {
        let mut sl = SkipListIndex::with_included(false, 2);
        sl.insert_with_included(
            Value::Varchar("session_1".into()), 100,
            &[Value::Int64(1000), Value::Varchar("user".into())]
        );
        sl.insert_with_included(
            Value::Varchar("session_1".into()), 101,
            &[Value::Int64(2000), Value::Varchar("assistant".into())]
        );
        sl.insert_with_included(
            Value::Varchar("session_2".into()), 200,
            &[Value::Int64(3000), Value::Varchar("user".into())]
        );

        let bytes = sl.to_bytes();
        let restored = SkipListIndex::from_bytes(&bytes).unwrap();
        assert_eq!(restored.num_included(), 2);
        assert_eq!(restored.len(), 2);

        let entries = restored.get_entries(&Value::Varchar("session_1".into())).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].row_id, 100);
        assert_eq!(entries[0].included[0], Value::Int64(1000));
        assert_eq!(entries[0].included[1], Value::Varchar("user".into()));
        assert_eq!(entries[1].row_id, 101);
        assert_eq!(entries[1].included[0], Value::Int64(2000));
        assert_eq!(entries[1].included[1], Value::Varchar("assistant".into()));
    }

    #[test]
    fn test_serialize_null_key() {
        let mut sl = SkipListIndex::new(false);
        sl.insert(Value::Null, 0);
        sl.insert(Value::Int64(1), 1);
        let bytes = sl.to_bytes();
        let restored = SkipListIndex::from_bytes(&bytes).unwrap();
        assert_eq!(restored.len(), 2);

        let result = restored.range(&Value::Null, &Value::Int64(10));
        assert_eq!(result.len(), 2);
        assert_eq!(result[0], 0);
    }

    #[test]
    fn test_serialize_boolean_keys() {
        let mut sl = SkipListIndex::new(false);
        sl.insert(Value::Boolean(true), 1);
        sl.insert(Value::Boolean(false), 0);

        let bytes = sl.to_bytes();
        let restored = SkipListIndex::from_bytes(&bytes).unwrap();
        assert_eq!(restored.len(), 2);

        let rows_t = restored.get(&Value::Boolean(true)).unwrap();
        assert_eq!(rows_t, vec![1]);
        let rows_f = restored.get(&Value::Boolean(false)).unwrap();
        assert_eq!(rows_f, vec![0]);
    }

    #[test]
    fn test_serialize_float_keys() {
        let mut sl = SkipListIndex::new(false);
        sl.insert(Value::Float64(3.14), 100);
        sl.insert(Value::Float64(2.718), 200);
        sl.insert(Value::Float64(1.414), 300);

        let bytes = sl.to_bytes();
        let restored = SkipListIndex::from_bytes(&bytes).unwrap();
        assert_eq!(restored.len(), 3);

        let rows = restored.get(&Value::Float64(3.14)).unwrap();
        assert_eq!(rows, vec![100]);

        // 验证顺序
        let keys: Vec<f64> = restored.iter()
            .map(|(k, _)| match k { Value::Float64(v) => *v, _ => 0.0 })
            .collect();
        assert_eq!(keys, vec![1.414, 2.718, 3.14]);
    }

    #[test]
    fn test_serialize_json_and_vector() {
        let mut sl = SkipListIndex::with_included(false, 1);
        sl.insert_with_included(
            Value::Json(r#"{"key":"value"}"#.into()), 1,
            &[Value::Vector(vec![1.0, 2.0, 3.0])]
        );

        let bytes = sl.to_bytes();
        let restored = SkipListIndex::from_bytes(&bytes).unwrap();
        assert_eq!(restored.len(), 1);
        assert_eq!(restored.num_included(), 1);

        let entries = restored.get_entries(&Value::Json(r#"{"key":"value"}"#.into())).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].row_id, 1);
        match &entries[0].included[0] {
            Value::Vector(v) => assert_eq!(v, &vec![1.0, 2.0, 3.0]),
            _ => panic!("expected vector value"),
        }
    }

    #[test]
    fn test_serialize_large_index() {
        let mut sl = SkipListIndex::new(false);
        for i in 0..1000 {
            sl.insert(Value::Int64(i), i as u32);
        }
        let bytes = sl.to_bytes();
        let restored = SkipListIndex::from_bytes(&bytes).unwrap();
        assert_eq!(restored.len(), 1000);

        // 抽样验证
        for &i in &[0, 1, 500, 999, 50, 777] {
            let rows = restored.get(&Value::Int64(i)).unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0], i as u32);
        }
    }

    #[test]
    fn test_deserialize_invalid_magic() {
        let bad = b"NOTINDEX";
        let result = SkipListIndex::from_bytes(bad);
        assert!(result.is_err());
    }

    #[test]
    fn test_deserialize_truncated() {
        let data = b"SKIPIDX1\x00\x00\x00\x00\x00"; // magic + flags + partial
        let result = SkipListIndex::from_bytes(data);
        assert!(result.is_err());
    }
}
