//! Delta 存储层（列式存储，P4 优化）
//!
//! 吸收随机写入，定期合并到列存主存储。
//! P4 优化：内部采用列式存储，合并到列存时无需行→列转置，compact 速度提升约 2x。

use std::collections::HashMap;
use crate::common::error::Result;
use crate::common::types::TableDef;
use crate::Value;

/// Delta 层（列式内存存储，写入优化 + 快速合并）
pub struct DeltaStore {
    #[allow(dead_code)]
    table_def: TableDef,
    /// 列式存储：每列一个 Vec<Value>
    columns: Vec<Vec<Value>>,
    row_count: usize,
    next_rowid: u64,
    /// row_id -> 位置索引的映射（支持基于 row_id 的操作）
    row_id_to_idx: HashMap<u64, usize>,
    /// 已删除的 row_id 集合（tombstone 标记）
    deleted_ids: std::collections::HashSet<u64>,
}

impl DeltaStore {
    pub fn new(table_def: TableDef) -> Self {
        let num_cols = table_def.columns.len();
        let columns = (0..num_cols).map(|_| Vec::with_capacity(1024)).collect();
        Self {
            table_def,
            columns,
            row_count: 0,
            next_rowid: 1,
            row_id_to_idx: HashMap::new(),
            deleted_ids: std::collections::HashSet::new(),
        }
    }

    /// 插入一行，返回 rowid
    pub fn insert(&mut self, row: Vec<Value>) -> Result<u64> {
        let rowid = self.next_rowid;
        self.next_rowid += 1;
        let row_len = row.len();
        let num_table_cols = self.table_def.columns.len();
        let idx = self.row_count;

        // 按列追加
        for (col_idx, val) in row.into_iter().enumerate() {
            if col_idx < self.columns.len() {
                self.columns[col_idx].push(val);
            } else {
                // 列数不足时补 NULL（前面的行也要补）
                while self.columns.len() <= col_idx {
                    let mut new_col = Vec::with_capacity(self.row_count + 1);
                    for _ in 0..self.row_count {
                        new_col.push(Value::Null);
                    }
                    self.columns.push(new_col);
                }
                self.columns[col_idx].push(val);
            }
        }
        // 如果行的列数少于表列数，补 NULL
        for col_idx in row_len..num_table_cols {
            if col_idx < self.columns.len() {
                self.columns[col_idx].push(Value::Null);
            }
        }

        // 维护 row_id -> idx 映射
        self.row_id_to_idx.insert(rowid, idx);
        self.row_count += 1;
        Ok(rowid)
    }
    
    /// 插入一行（指定 rowid，用于事务提交后应用）
    ///
    /// 与 `insert()` 不同，此方法允许指定 rowid：
    /// - 由事务管理器在 commit 后调用
    /// - rowid 由事务管理器分配（避免重复）
    pub fn insert_row(&mut self, rowid: u32, row: Vec<Value>) -> Result<()> {
        let row_len = row.len();
        let num_table_cols = self.table_def.columns.len();
        let idx = self.row_count;

        // 按列追加（rowid 由事务管理器保证唯一）
        for (col_idx, val) in row.into_iter().enumerate() {
            if col_idx < self.columns.len() {
                self.columns[col_idx].push(val);
            } else {
                // 列数不足时补 NULL（前面的行也要补）
                while self.columns.len() <= col_idx {
                    let mut new_col = Vec::with_capacity(self.row_count + 1);
                    for _ in 0..self.row_count {
                        new_col.push(Value::Null);
                    }
                    self.columns.push(new_col);
                }
                self.columns[col_idx].push(val);
            }
        }
        // 如果行的列数少于表列数，补 NULL
        for col_idx in row_len..num_table_cols {
            if col_idx < self.columns.len() {
                self.columns[col_idx].push(Value::Null);
            }
        }

        // 维护 row_id -> idx 映射
        self.row_id_to_idx.insert(rowid as u64, idx);
        
        // 更新 next_rowid（如果需要）
        self.next_rowid = self.next_rowid.max(rowid as u64 + 1);
        self.row_count += 1;
        Ok(())
    }
    
    /// 基于 row_id 删除单行（tombstone 标记，实际物理删除在 compact 时）
    ///
    /// 用于事务路径提交后的应用阶段
    pub fn delete_row(&mut self, rowid: u32) -> Result<()> {
        let rowid_64 = rowid as u64;
        // 标记为已删除
        self.deleted_ids.insert(rowid_64);
        // 从映射中移除
        self.row_id_to_idx.remove(&rowid_64);
        Ok(())
    }
    
    /// 基于 row_id 更新单行
    ///
    /// 用于事务路径提交后的应用阶段
    pub fn update_row_by_id(&mut self, rowid: u32, new_row: Vec<Value>) -> Result<()> {
        let rowid_64 = rowid as u64;
        
        // 检查是否已删除
        if self.deleted_ids.contains(&rowid_64) {
            return Err(crate::common::error::HybridDbError::InvalidFormat(
                format!("row {} has been deleted", rowid)
            ));
        }
        
        // 获取位置索引
        let idx = self.row_id_to_idx.get(&rowid_64).copied()
            .ok_or_else(|| {
                crate::common::error::HybridDbError::InvalidFormat(
                    format!("row {} not found in delta store", rowid)
                )
            })?;
        
        let num_table_cols = self.table_def.columns.len();
        let row_len = new_row.len();
        
        // 更新各列
        for (col_idx, val) in new_row.into_iter().enumerate() {
            if col_idx < self.columns.len() && idx < self.columns[col_idx].len() {
                self.columns[col_idx][idx] = val;
            }
        }
        
        // 如果行的列数少于表列数，补 NULL
        for col_idx in row_len..num_table_cols {
            if col_idx < self.columns.len() && idx < self.columns[col_idx].len() {
                self.columns[col_idx][idx] = Value::Null;
            }
        }
        
        Ok(())
    }

    /// 批量插入（主要优化路径）
    pub fn insert_batch(&mut self, batch: Vec<Vec<Value>>) -> Result<u64> {
        let count = batch.len() as u64;
        if count == 0 {
            return Ok(0);
        }

        let num_cols = self.columns.len();
        let new_rows = batch.len();
        let start_idx = self.row_count;
        let start_rowid = self.next_rowid;

        // 预分配每列
        for col in &mut self.columns {
            col.reserve(new_rows);
        }

        // 按列追加：对每一列，遍历所有行
        for col_idx in 0..num_cols {
            let col = &mut self.columns[col_idx];
            for row in &batch {
                if col_idx < row.len() {
                    col.push(row[col_idx].clone());
                } else {
                    col.push(Value::Null);
                }
            }
        }

        // 维护 row_id -> idx 映射
        for i in 0..new_rows {
            let rowid = start_rowid + i as u64;
            let idx = start_idx + i;
            self.row_id_to_idx.insert(rowid, idx);
        }

        self.row_count += new_rows;
        self.next_rowid += count;
        Ok(count)
    }

    /// 列式批量插入（零拷贝路径）
    ///
    /// 直接以列式数据写入，跳过行→列转置。
    /// 输入：每列一个 Vec<Value>，所有列长度必须一致。
    pub fn insert_columns(&mut self, columns: Vec<Vec<Value>>) -> Result<u64> {
        if columns.is_empty() {
            return Ok(0);
        }

        let num_rows = columns[0].len();
        if num_rows == 0 {
            return Ok(0);
        }

        let num_cols = self.columns.len();
        let input_cols = columns.len();
        let start_idx = self.row_count;
        let start_rowid = self.next_rowid;

        // 验证所有输入列长度一致
        for col in &columns {
            if col.len() != num_rows {
                return Err(crate::common::error::HybridDbError::Internal(
                    "insert_columns: all columns must have the same length".into()
                ));
            }
        }

        // 如果输入列数多于现有列数，扩展列
        if input_cols > num_cols {
            for _ in num_cols..input_cols {
                let mut new_col = Vec::with_capacity(self.row_count + num_rows);
                for _ in 0..self.row_count {
                    new_col.push(Value::Null);
                }
                self.columns.push(new_col);
            }
        }

        // 直接按列追加（核心：零转置开销）
        for (col_idx, src_col) in columns.into_iter().enumerate() {
            if col_idx < self.columns.len() {
                self.columns[col_idx].extend(src_col);
            }
        }

        // 如果输入列数少于表列数，剩余列补 NULL
        for col_idx in input_cols..self.columns.len() {
            for _ in 0..num_rows {
                self.columns[col_idx].push(Value::Null);
            }
        }

        // 维护 row_id -> idx 映射
        for i in 0..num_rows {
            let rowid = start_rowid + i as u64;
            let idx = start_idx + i;
            self.row_id_to_idx.insert(rowid, idx);
        }

        self.row_count += num_rows;
        self.next_rowid += num_rows as u64;
        Ok(num_rows as u64)
    }

    /// 按 rowid 查找
    ///
    /// 使用 row_id_to_idx 映射定位行，跳过已删除的行
    pub fn get(&self, rowid: u64) -> Option<Vec<Value>> {
        // 检查是否已删除
        if self.deleted_ids.contains(&rowid) {
            return None;
        }
        
        // 使用映射获取位置索引
        let idx = self.row_id_to_idx.get(&rowid).copied()?;
        
        if idx < self.row_count {
            let row: Vec<Value> = self.columns.iter()
                .map(|col| col[idx].clone())
                .collect();
            Some(row)
        } else {
            None
        }
    }

    /// 获取所有行（按 rowid 排序，跳过已删除的行）
    pub fn all_rows(&self) -> Vec<(u64, Vec<Value>)> {
        let mut rows = Vec::new();
        
        for (rowid, idx) in &self.row_id_to_idx {
            if !self.deleted_ids.contains(rowid) && *idx < self.row_count {
                let row: Vec<Value> = self.columns.iter()
                    .map(|col| col[*idx].clone())
                    .collect();
                rows.push((*rowid, row));
            }
        }
        
        // 按 rowid 排序
        rows.sort_by_key(|(rowid, _)| *rowid);
        rows
    }

    /// 行数（包括已删除的行，用于位置索引计算）
    pub fn len(&self) -> usize {
        self.row_count
    }
    
    /// 有效行数（不包括已删除的行）
    pub fn active_len(&self) -> usize {
        self.row_count - self.deleted_ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.active_len() == 0
    }

    /// 清空（合并到列存后调用）
    pub fn clear(&mut self) {
        for col in &mut self.columns {
            col.clear();
        }
        self.row_count = 0;
        self.row_id_to_idx.clear();
        self.deleted_ids.clear();
    }

    /// 取出所有行数据（行式，用于兼容旧接口）
    pub fn drain_all_rows(&mut self) -> Vec<Vec<Value>> {
        if self.row_count == 0 {
            return Vec::new();
        }

        let num_cols = self.columns.len();
        let num_rows = self.row_count;

        // 转置：列式 → 行式
        let mut rows = Vec::with_capacity(num_rows);
        for i in 0..num_rows {
            let mut row = Vec::with_capacity(num_cols);
            for col in &self.columns {
                row.push(col[i].clone());
            }
            rows.push(row);
        }

        self.clear();
        rows
    }

    /// P4 优化：直接获取列式数据引用（用于快速合并）
    ///
    /// 合并到列存时无需转置，直接将列数据追加到 ColumnStore。
    pub fn column_data(&self) -> &[Vec<Value>] {
        &self.columns
    }

    /// 获取按聚簇列分组后的列式数据
    ///
    /// 同一聚簇键值的行在结果中物理连续。
    /// 用于 compact 时实现聚簇写入，提升按聚簇列查询的性能。
    ///
    /// 算法：
    /// 1. 扫描聚簇列，构建 value -> Vec<row_idx> 映射
    /// 2. 按值的首次出现顺序遍历，生成排列索引 permutation
    /// 3. 每列按 permutation 重排
    pub fn clustered_column_data(&self, cluster_col_idx: usize) -> Vec<Vec<Value>> {
        if self.row_count == 0 || cluster_col_idx >= self.columns.len() {
            return self.columns.clone();
        }

        let num_rows = self.row_count;
        let num_cols = self.columns.len();
        let cluster_col = &self.columns[cluster_col_idx];

        // 构建 value -> 行索引列表的映射，保持首次出现顺序
        use std::collections::HashMap;
        let mut group_map: HashMap<&Value, Vec<usize>> = HashMap::new();
        let mut group_order: Vec<&Value> = Vec::new();

        for row_idx in 0..num_rows {
            let key = &cluster_col[row_idx];
            if group_map.get(key).is_none() {
                group_order.push(key);
            }
            group_map.entry(key).or_default().push(row_idx);
        }

        // 构建排列索引
        let mut permutation = Vec::with_capacity(num_rows);
        for key in &group_order {
            if let Some(indices) = group_map.get(key) {
                permutation.extend_from_slice(indices);
            }
        }

        // 按排列索引重排每一列
        let mut result = Vec::with_capacity(num_cols);
        for col in &self.columns {
            let mut new_col = Vec::with_capacity(num_rows);
            for &idx in &permutation {
                new_col.push(col[idx].clone());
            }
            result.push(new_col);
        }

        result
    }

    /// 从 Delta 头部取出 n 行数据（列式返回）
    ///
    /// 用于增量合并：每次只取一部分数据合并到列存，
    /// 控制单次 compact 的阻塞时间。
    pub fn drain_front(&mut self, n: usize) -> Vec<Vec<Value>> {
        let n = n.min(self.row_count);
        if n == 0 {
            return Vec::new();
        }

        let num_cols = self.columns.len();
        let mut result = Vec::with_capacity(num_cols);

        for col in &mut self.columns {
            // 取出前 n 个元素
            let drained: Vec<Value> = col.drain(0..n).collect();
            result.push(drained);
        }

        self.row_count -= n;
        result
    }

    /// 列数
    pub fn num_columns(&self) -> usize {
        self.columns.len()
    }

    /// 按位置删除多行（v0.12.0 新增，DELETE 支持）
    ///
    /// indices 必须是升序排列的行索引（0-based）。
    /// 采用从后往前删除的方式，避免索引偏移问题。
    ///
    /// 返回被删除的行（按原始顺序），用于索引维护。
    pub fn delete_rows(&mut self, indices: &[usize]) -> Vec<Vec<Value>> {
        if indices.is_empty() || self.row_count == 0 {
            return Vec::new();
        }

        let num_cols = self.columns.len();
        let mut deleted_rows: Vec<Vec<Value>> = Vec::with_capacity(indices.len());

        // 先收集被删除的行（按原始顺序）
        for &idx in indices {
            if idx < self.row_count {
                let row: Vec<Value> = self.columns.iter()
                    .map(|col| col[idx].clone())
                    .collect();
                deleted_rows.push(row);
            }
        }

        // 从后往前删除，避免索引偏移
        let mut sorted_indices: Vec<usize> = indices.to_vec();
        sorted_indices.sort_unstable_by(|a, b| b.cmp(a)); // 降序

        for &idx in &sorted_indices {
            if idx < self.row_count {
                for col in &mut self.columns {
                    col.remove(idx);
                }
                self.row_count -= 1;
            }
        }

        deleted_rows
    }

    /// 按位置更新单行（v0.12.0 新增，UPDATE 支持）
    ///
    /// 返回更新前的旧行（用于索引维护）。
    pub fn update_row(&mut self, idx: usize, new_values: &[(usize, Value)]) -> Option<Vec<Value>> {
        if idx >= self.row_count {
            return None;
        }

        // 收集旧行
        let old_row: Vec<Value> = self.columns.iter()
            .map(|col| col[idx].clone())
            .collect();

        // 应用更新
        for &(col_idx, ref new_val) in new_values {
            if col_idx < self.columns.len() {
                self.columns[col_idx][idx] = new_val.clone();
            }
        }

        Some(old_row)
    }
}
