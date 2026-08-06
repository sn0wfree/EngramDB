//! Delta 存储层（列式存储，P4 优化）
//!
//! 吸收随机写入，定期合并到列存主存储。
//! P4 优化：内部采用列式存储，合并到列存时无需行→列转置，compact 速度提升约 2x。

use std::collections::HashMap;
use crate::common::error::Result;
use crate::common::types::TableDef;
use crate::Value;

/// 连续 rowid 区间（S1.4）
///
/// 批量插入时 rowid 连续（start_rowid + i），无需逐个维护 HashMap 映射。
/// 区间按 base_rowid 升序存储；命中区间时 idx = base_idx + (rowid - base_rowid)。
#[derive(Debug, Clone, Copy)]
struct SparseRun {
    base_rowid: u64,
    base_idx: usize,
    count: u32,
}

/// Delta 层（列式内存存储，写入优化 + 快速合并）
pub struct DeltaStore {
    #[allow(dead_code)]
    table_def: TableDef,
    /// 列式存储：每列一个 Vec<Value>
    columns: Vec<Vec<Value>>,
    row_count: usize,
    next_rowid: u64,
    /// row_id -> 位置索引的映射（散插行 / 已展开的区间行）
    row_id_to_idx: HashMap<u64, usize>,
    /// 连续 rowid 区间（批量插入，O(1) 记录；删除不展开——get 时跳过 deleted_ids）
    sparse_runs: Vec<SparseRun>,
    /// 已删除的 row_id 集合（tombstone 标记）
    deleted_ids: std::collections::HashSet<u64>,
    /// 分层索引：Delta 层稠密主键索引（主键值 -> row_id）
    ///
    /// 只覆盖 Delta 层内的行（列存行由稀疏索引负责），行数受 compact
    /// 阈值约束，内存有界；compact 合并后随 clear() 一起清空。
    pk_index: Option<std::collections::BTreeMap<crate::Value, u32>>,
}

impl DeltaStore {
    pub fn new(table_def: TableDef) -> Self {
        let num_cols = table_def.columns.len();
        let columns = (0..num_cols).map(|_| Vec::with_capacity(1024)).collect();
        let has_pk = table_def.primary_key_index().is_some();
        Self {
            table_def,
            columns,
            row_count: 0,
            next_rowid: 0,
            row_id_to_idx: HashMap::new(),
            sparse_runs: Vec::new(),
            deleted_ids: std::collections::HashSet::new(),
            pk_index: if has_pk { Some(std::collections::BTreeMap::new()) } else { None },
        }
    }

    /// 主键列索引（有主键时 Some）
    fn pk_col_idx(&self) -> Option<usize> {
        self.table_def.primary_key_index()
    }

    /// 向稠密主键索引插入 (pk_value, row_id)
    #[inline]
    fn pk_insert(&mut self, row: &[crate::Value], row_id: u32) {
        if let (Some(pk_idx), Some(idx)) = (self.pk_col_idx(), self.pk_index.as_mut()) {
            if let Some(v) = row.get(pk_idx).cloned() {
                idx.insert(v, row_id);
            }
        }
    }

    /// 从稠密主键索引移除 (pk_value, _)
    #[inline]
    fn pk_remove(&mut self, row: &[crate::Value]) {
        if let (Some(pk_idx), Some(idx)) = (self.pk_col_idx(), self.pk_index.as_mut()) {
            if let Some(v) = row.get(pk_idx) {
                idx.remove(v);
            }
        }
    }

    /// 稠密主键索引精确点查（只查 Delta 层内的行）
    pub fn pk_lookup(&self, key: &crate::Value) -> Option<u32> {
        self.pk_index.as_ref()?.get(key).copied()
    }

    /// 稠密主键索引点查（数值类型归一化：Int32/Int64/Timestamp 互查）
    pub fn pk_lookup_normalized(&self, key: &crate::Value) -> Option<u32> {
        if let Some(v) = self.pk_lookup(key) {
            return Some(v);
        }
        let idx = self.pk_index.as_ref()?;
        use crate::Value::*;
        match key {
            Int32(v) => idx.get(&Int64(*v as i64)).copied()
                .or_else(|| idx.get(&Timestamp(*v as i64)).copied()),
            Int64(v) => idx.get(&Int32(*v as i32)).copied()
                .or_else(|| idx.get(&Timestamp(*v)).copied()),
            Timestamp(v) => idx.get(&Int64(*v)).copied()
                .or_else(|| idx.get(&Int32(*v as i32)).copied()),
            _ => None,
        }
    }

    /// 稠密主键索引行数（统计用）
    pub fn pk_index_len(&self) -> usize {
        self.pk_index.as_ref().map(|m| m.len()).unwrap_or(0)
    }

    /// 二分查找包含 rowid 的连续区间
    fn find_run(&self, rowid: u64) -> Option<usize> {
        let pos = self.sparse_runs.partition_point(|r| r.base_rowid <= rowid);
        if pos == 0 {
            return None;
        }
        let idx = pos - 1;
        let run = &self.sparse_runs[idx];
        if rowid < run.base_rowid + run.count as u64 {
            Some(idx)
        } else {
            None
        }
    }

    /// 将区间行全部搬入 HashMap（insert_row 落在区间内时保持映射一致）
    fn expand_run(&mut self, run_idx: usize) {
        let run = self.sparse_runs.remove(run_idx);
        for i in 0..run.count as usize {
            self.row_id_to_idx
                .insert(run.base_rowid + i as u64, run.base_idx + i);
        }
    }

    /// row_id → 位置索引：先查连续区间（O(log N) 二分），再查 HashMap
    fn idx_lookup(&self, rowid: u64) -> Option<usize> {
        if let Some(run_idx) = self.find_run(rowid) {
            let run = &self.sparse_runs[run_idx];
            return Some(run.base_idx + (rowid - run.base_rowid) as usize);
        }
        self.row_id_to_idx.get(&rowid).copied()
    }

    /// 插入一行，返回 rowid
    pub fn insert(&mut self, row: Vec<Value>) -> Result<u64> {
        let rowid = self.next_rowid;
        self.next_rowid += 1;
        let row_len = row.len();
        let num_table_cols = self.table_def.columns.len();
        let idx = self.row_count;

        // 分层索引：稠密主键索引先维护（row 即将被消费）
        self.pk_insert(&row, rowid as u32);

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

        // S1.4：连续 rowid 并入末尾区间（O(1)）；无可并入区间时创建新区间
        match self.sparse_runs.last_mut() {
            Some(last) if last.base_rowid + last.count as u64 == rowid => {
                last.count += 1;
            }
            _ => {
                // 单行 insert 的 rowid 来自 next_rowid 递增，与既有区间无重叠
                self.sparse_runs.push(SparseRun {
                    base_rowid: rowid,
                    base_idx: idx,
                    count: 1,
                });
            }
        }
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

        // 分层索引：稠密主键索引先维护（row 即将被消费）
        self.pk_insert(&row, rowid);

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

        // S1.4：rowid 可能落在既有连续区间内（事务乱序分配）→ 展开该区间，
        // 保持区间映射与 HashMap 一致
        if let Some(run_idx) = self.find_run(rowid as u64) {
            self.expand_run(run_idx);
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
        // 分层索引：先读旧主键值移除（此时映射尚未移除）
        if let Some(pk_idx) = self.pk_col_idx() {
            if let Some(idx) = self.idx_lookup(rowid_64) {
                let old_pk = self.columns[pk_idx][idx].clone();
                if let Some(m) = self.pk_index.as_mut() {
                    m.remove(&old_pk);
                }
            }
        }
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
            return Err(crate::common::error::EngramDbError::InvalidFormat(
                format!("row {} has been deleted", rowid)
            ));
        }
        
        // 获取位置索引（区间二分 / HashMap）
        let idx = self.idx_lookup(rowid_64).ok_or_else(|| {
            crate::common::error::EngramDbError::InvalidFormat(
                format!("row {} not found in delta store", rowid)
            )
        })?;
        
        let num_table_cols = self.table_def.columns.len();
        let row_len = new_row.len();

        // 分层索引：记录旧主键值（更新后可能变化）
        let old_pk = match self.pk_col_idx() {
            Some(pk_idx) if idx < self.columns[pk_idx].len() => {
                Some(self.columns[pk_idx][idx].clone())
            }
            _ => None,
        };

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

        // 分层索引：主键值变化时更新映射
        if let (Some(pk_idx), Some(m)) = (self.pk_col_idx(), self.pk_index.as_mut()) {
            if let Some(old) = old_pk {
                let cur = self.columns[pk_idx][idx].clone();
                if cur != old {
                    m.remove(&old);
                    m.insert(cur, rowid);
                }
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

        // S1.4：连续 rowid → O(1) 区间记录（替代逐个 HashMap insert）
        self.sparse_runs.push(SparseRun {
            base_rowid: start_rowid,
            base_idx: start_idx,
            count: new_rows as u32,
        });

        // 分层索引：逐行维护稠密主键索引
        if self.pk_index.is_some() {
            if let Some(pk_idx) = self.pk_col_idx() {
                if let Some(m) = self.pk_index.as_mut() {
                    for (i, row) in batch.iter().enumerate() {
                        if let Some(v) = row.get(pk_idx) {
                            m.insert(v.clone(), (start_rowid + i as u64) as u32);
                        }
                    }
                }
            }
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
                return Err(crate::common::error::EngramDbError::Internal(
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

        // 分层索引：主键列整列逐行维护稠密主键索引（columns 消费前）
        if self.pk_index.is_some() {
            if let Some(pk_idx) = self.pk_col_idx() {
                let pk_col: Vec<Value> = columns.get(pk_idx)
                    .map(|c| c.clone())
                    .unwrap_or_else(|| vec![Value::Null; num_rows]);
                if let Some(m) = self.pk_index.as_mut() {
                    for (i, v) in pk_col.iter().enumerate() {
                        m.insert(v.clone(), (start_rowid + i as u64) as u32);
                    }
                }
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

        // S1.4：连续 rowid → O(1) 区间记录（替代逐个 HashMap insert）
        self.sparse_runs.push(SparseRun {
            base_rowid: start_rowid,
            base_idx: start_idx,
            count: num_rows as u32,
        });

        self.row_count += num_rows;
        self.next_rowid += num_rows as u64;
        Ok(num_rows as u64)
    }

    /// 按 rowid 查找
    ///
    /// 先查连续区间（二分），再查 HashMap，跳过已删除的行
    pub fn get(&self, rowid: u64) -> Option<Vec<Value>> {
        // 检查是否已删除
        if self.deleted_ids.contains(&rowid) {
            return None;
        }

        // 获取位置索引（区间二分 / HashMap）
        let idx = self.idx_lookup(rowid)?;

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
        let mut entries: Vec<(u64, usize)> = Vec::with_capacity(self.row_count);

        // S1.4：连续区间（有序），跳过已删除
        for run in &self.sparse_runs {
            for i in 0..run.count as usize {
                let rid = run.base_rowid + i as u64;
                if self.deleted_ids.contains(&rid) {
                    continue;
                }
                let idx = run.base_idx + i;
                if idx < self.row_count {
                    entries.push((rid, idx));
                }
            }
        }

        // HashMap 散行
        for (rowid, idx) in &self.row_id_to_idx {
            if !self.deleted_ids.contains(rowid) && *idx < self.row_count {
                entries.push((*rowid, *idx));
            }
        }

        // 按 rowid 排序
        entries.sort_by_key(|(rowid, _)| *rowid);
        entries
            .into_iter()
            .map(|(rowid, idx)| {
                let row: Vec<Value> = self.columns.iter()
                    .map(|col| col[idx].clone())
                    .collect();
                (rowid, row)
            })
            .collect()
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
        self.next_rowid = 1;
        self.row_id_to_idx.clear();
        self.sparse_runs.clear();
        self.deleted_ids.clear();
        if let Some(m) = self.pk_index.as_mut() {
            m.clear();
        }
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

        // 同步稀疏区间：剩余行列位置整体前移 n；被取走行从区间头部剔除
        let mut taken = 0usize;
        let mut new_runs = Vec::with_capacity(self.sparse_runs.len());
        for mut run in std::mem::take(&mut self.sparse_runs) {
            if taken >= n {
                // 整个 run 保留：仅列位置前移
                run.base_idx = run.base_idx.saturating_sub(n);
                new_runs.push(run);
                continue;
            }
            let take = (n - taken).min(run.count as usize);
            taken += take;
            run.count -= take as u32;
            if run.count > 0 {
                // 区间头部 take 行被取走：首行 rowid 后移，列位置前移
                run.base_rowid += take as u64;
                run.base_idx = run.base_idx.saturating_sub(n);
                new_runs.push(run);
            }
        }
        self.sparse_runs = new_runs;

        // 同步散行映射：被取走位置删除，剩余位置前移
        if !self.row_id_to_idx.is_empty() {
            self.row_id_to_idx = self.row_id_to_idx
                .drain()
                .filter(|(_, idx)| *idx >= n)
                .map(|(rid, idx)| (rid, idx - n))
                .collect();
        }

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
    /// 位置索引 → 全局 row_id 反查（位置语义 API 维护 pk_index 用）
    fn rowid_for_idx(&self, idx: usize) -> Option<u32> {
        for run in &self.sparse_runs {
            let base = run.base_idx as usize;
            if idx >= base && idx < base + run.count as usize {
                return Some((run.base_rowid + (idx - base) as u64) as u32);
            }
        }
        for (rowid, &i) in &self.row_id_to_idx {
            if i == idx {
                return Some(*rowid as u32);
            }
        }
        None
    }

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

        // 分层索引：先移除被删行的主键（主键值唯一，按值删除；列尚未删）
        if let (Some(pk_idx), Some(m)) = (self.pk_col_idx(), self.pk_index.as_mut()) {
            for row in &deleted_rows {
                if let Some(v) = row.get(pk_idx) {
                    m.remove(v);
                }
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

        // 分层索引：主键值变化时更新映射（rowid 用位置反查，与调用方语义一致）
        if let Some(pk_idx) = self.pk_col_idx() {
            let old_pk = old_row.get(pk_idx).cloned();
            let new_pk = self.columns.get(pk_idx).and_then(|c| c.get(idx)).cloned();
            if old_pk != new_pk {
                if let Some(new) = new_pk {
                    let rid = self.rowid_for_idx(idx).unwrap_or(idx as u32);
                    if let Some(m) = self.pk_index.as_mut() {
                        if let Some(old) = old_pk {
                            m.remove(&old);
                        }
                        m.insert(new, rid);
                    }
                }
            }
        }

        Some(old_row)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::types::{ColumnDef, DataType, TableDef};

    fn make_table_def() -> TableDef {
        TableDef {
            id: 1,
            engine: crate::common::types::EngineType::Columnar,
            name: "t".to_string(),
            columns: vec![
                ColumnDef::new("id", DataType::Int64),
                ColumnDef::new("name", DataType::Varchar),
            ],
            row_count: 0,
            indexes: vec![],
            cluster_key: None,
            foreign_keys: vec![],
            next_auto_increment_id: 0,
            ttl_seconds: None,
            ttl_column: None,
        }
    }

    fn row(id: i64, name: &str) -> Vec<Value> {
        vec![Value::Int64(id), Value::Varchar(name.to_string())]
    }

    #[test]
    fn test_insert_batch_uses_sparse_runs() {
        let mut ds = DeltaStore::new(make_table_def());
        let mut batch = Vec::new();
        for i in 0..1000 {
            batch.push(row(i, &format!("r{}", i)));
        }
        ds.insert_batch(batch).unwrap();

        // 区间记录：O(1) 一条，而非 1000 条 HashMap
        assert_eq!(ds.sparse_runs.len(), 1);
        assert_eq!(ds.sparse_runs[0].base_rowid, 0);
        assert_eq!(ds.sparse_runs[0].count, 1000);
        assert!(ds.row_id_to_idx.is_empty());

        // get 命中区间
        let r = ds.get(500).unwrap();
        assert_eq!(r[0], Value::Int64(500));
        assert_eq!(r[1], Value::Varchar("r500".into()));

        // 不存在
        assert!(ds.get(1000).is_none());

        // 批量 + 批量：两个区间
        let mut batch2 = Vec::new();
        for i in 1000..1500 {
            batch2.push(row(i, &format!("r{}", i)));
        }
        ds.insert_batch(batch2).unwrap();
        assert_eq!(ds.sparse_runs.len(), 2);
        assert_eq!(ds.get(1200).unwrap()[0], Value::Int64(1200));
    }

    #[test]
    fn test_single_insert_extends_last_run() {
        let mut ds = DeltaStore::new(make_table_def());
        ds.insert(row(0, "a")).unwrap();
        ds.insert(row(1, "b")).unwrap();
        ds.insert(row(2, "c")).unwrap();
        // 连续单行并入区间
        assert_eq!(ds.sparse_runs.len(), 1);
        assert_eq!(ds.sparse_runs[0].count, 3);
        assert_eq!(ds.get(1).unwrap()[1], Value::Varchar("b".into()));
    }

    #[test]
    fn test_insert_row_collides_with_run() {
        let mut ds = DeltaStore::new(make_table_def());
        let mut batch = Vec::new();
        for i in 0..100 {
            batch.push(row(i, &format!("r{}", i)));
        }
        ds.insert_batch(batch).unwrap();

        // 事务乱序插入 rowid=50（落在区间内）→ 区间展开，get 仍正确
        ds.insert_row(50, row(999, "txn")).unwrap();
        assert_eq!(ds.get(50).unwrap()[0], Value::Int64(999));
        assert_eq!(ds.get(49).unwrap()[0], Value::Int64(49));
        assert_eq!(ds.get(51).unwrap()[0], Value::Int64(51));
        // 唯一区间被展开 → 全部搬入 HashMap
        assert!(ds.sparse_runs.is_empty());
        assert_eq!(ds.row_id_to_idx.len(), 100);
    }

    #[test]
    fn test_delete_row_in_run() {
        let mut ds = DeltaStore::new(make_table_def());
        let mut batch = Vec::new();
        for i in 0..100 {
            batch.push(row(i, &format!("r{}", i)));
        }
        ds.insert_batch(batch).unwrap();

        ds.delete_row(30).unwrap();
        assert!(ds.get(30).is_none());
        assert_eq!(ds.get(31).unwrap()[0], Value::Int64(31));
        assert_eq!(ds.active_len(), 99);

        // all_rows 跳过已删除
        let all = ds.all_rows();
        assert_eq!(all.len(), 99);
        assert!(all.iter().all(|(rid, _)| *rid != 30));
        // 排序
        assert!(all.windows(2).all(|w| w[0].0 < w[1].0));
    }

    #[test]
    fn test_all_rows_merges_runs_and_hashmap() {
        let mut ds = DeltaStore::new(make_table_def());
        // 批量区间 + 散行（insert_row 乱序）
        let mut batch = Vec::new();
        for i in 0..50 {
            batch.push(row(i, &format!("r{}", i)));
        }
        ds.insert_batch(batch).unwrap();
        ds.insert_row(200, row(200, "scattered")).unwrap(); // next_rowid → 201
        ds.insert(row(50, "after")).unwrap();               // rowid = 201

        let all = ds.all_rows();
        assert_eq!(all.len(), 52);
        assert!(all.windows(2).all(|w| w[0].0 < w[1].0));
        assert_eq!(all[0].1[1], Value::Varchar("r0".into()));
        assert_eq!(all[50].1[1], Value::Varchar("scattered".into())); // rowid 200
        assert_eq!(all[51].1[1], Value::Varchar("after".into()));    // rowid 201
    }

    #[test]
    fn test_update_row_by_id_in_run() {
        let mut ds = DeltaStore::new(make_table_def());
        let mut batch = Vec::new();
        for i in 0..100 {
            batch.push(row(i, &format!("r{}", i)));
        }
        ds.insert_batch(batch).unwrap();

        ds.update_row_by_id(42, row(42, "updated")).unwrap();
        assert_eq!(ds.get(42).unwrap()[1], Value::Varchar("updated".into()));
        assert_eq!(ds.get(43).unwrap()[1], Value::Varchar("r43".into()));
    }

    #[test]
    fn test_clear_resets_runs() {
        let mut ds = DeltaStore::new(make_table_def());
        let mut batch = Vec::new();
        for i in 0..100 {
            batch.push(row(i, &format!("r{}", i)));
        }
        ds.insert_batch(batch).unwrap();
        ds.clear();
        assert!(ds.sparse_runs.is_empty());
        assert!(ds.row_id_to_idx.is_empty());
        assert_eq!(ds.len(), 0);
    }

    #[test]
    fn test_insert_columns_sparse_run() {
        let mut ds = DeltaStore::new(make_table_def());
        let cols = vec![
            (0..1000i64).collect::<Vec<_>>().into_iter().map(Value::Int64).collect(),
            (0..1000i64).map(|i| Value::Varchar(format!("r{}", i))).collect(),
        ];
        ds.insert_columns(cols).unwrap();
        // 列式直写：单条稀疏区间 + 零 HashMap
        assert_eq!(ds.sparse_runs.len(), 1);
        assert_eq!(ds.sparse_runs[0].count, 1000);
        assert!(ds.row_id_to_idx.is_empty());
        assert_eq!(ds.get(500).unwrap()[0], Value::Int64(500));
        assert_eq!(ds.get(500).unwrap()[1], Value::Varchar("r500".into()));
        assert_eq!(ds.len(), 1000);
    }

    #[test]
    fn test_insert_columns_pad_and_extend() {
        let mut ds = DeltaStore::new(make_table_def());
        // 输入只有 1 列：第 2 列补 NULL
        let cols = vec![vec![Value::Int64(1), Value::Int64(2)]];
        ds.insert_columns(cols).unwrap();
        assert_eq!(ds.get(0).unwrap(), vec![Value::Int64(1), Value::Null]);
        assert_eq!(ds.get(1).unwrap(), vec![Value::Int64(2), Value::Null]);

        // 输入 3 列（超出表定义 2 列）：扩展列
        let mut ds2 = DeltaStore::new(make_table_def());
        let cols3 = vec![
            vec![Value::Int64(1)],
            vec![Value::Varchar("a".into())],
            vec![Value::Float64(1.5)],
        ];
        ds2.insert_columns(cols3).unwrap();
        assert_eq!(ds2.num_columns(), 3);
        assert_eq!(ds2.get(0).unwrap(), vec![Value::Int64(1), Value::Varchar("a".into()), Value::Float64(1.5)]);
    }

    #[test]
    fn test_insert_columns_length_mismatch() {
        let mut ds = DeltaStore::new(make_table_def());
        let cols = vec![
            vec![Value::Int64(1), Value::Int64(2)],
            vec![Value::Int64(1)], // 长度不一致
        ];
        assert!(ds.insert_columns(cols).is_err());
        assert_eq!(ds.len(), 0, "失败时零副作用");
    }

    #[test]
    fn test_drain_front_columns() {
        let mut ds = DeltaStore::new(make_table_def());
        let mut batch = Vec::new();
        for i in 0..100 {
            batch.push(row(i, &format!("r{}", i)));
        }
        ds.insert_batch(batch).unwrap();

        let front = ds.drain_front(30);
        assert_eq!(front.len(), 2, "列式返回：每列一个 Vec");
        assert_eq!(front[0].len(), 30);
        assert_eq!(front[0][0], Value::Int64(0));
        assert_eq!(front[0][29], Value::Int64(29));
        assert_eq!(ds.len(), 70);
        assert_eq!(ds.get(30).unwrap()[0], Value::Int64(30), "剩余行 rowid 语义保持");

        // drain_front 超过行数：取全部
        let rest = ds.drain_front(999);
        assert_eq!(rest[0].len(), 70);
        assert_eq!(ds.len(), 0);
        assert!(ds.is_empty());
    }

    #[test]
    fn test_delete_rows_and_update_row() {
        let mut ds = DeltaStore::new(make_table_def());
        let mut batch = Vec::new();
        for i in 0..10 {
            batch.push(row(i, &format!("r{}", i)));
        }
        ds.insert_batch(batch).unwrap();

        // delete_rows：按位置批量删除（乱序索引），返回被删行（位置语义 API）
        let deleted = ds.delete_rows(&[0, 5, 9]);
        assert_eq!(deleted.len(), 3);
        assert_eq!(deleted[0][0], Value::Int64(0));
        assert_eq!(deleted[1][0], Value::Int64(5));
        assert_eq!(ds.len(), 7);

        // update_row：按位置更新，返回旧行（删除 0 后位置 2 = 原行 3）
        let old = ds.update_row(2, &[(1, Value::Varchar("updated".into()))]);
        assert_eq!(old.unwrap()[1], Value::Varchar("r3".into()));
        assert_eq!(ds.get(2).unwrap()[1], Value::Varchar("updated".into()));
        // 越界更新返回 None
        assert!(ds.update_row(999, &[]).is_none());
    }

    #[test]
    fn test_drain_all_rows_transposes_and_clears() {
        let mut ds = DeltaStore::new(make_table_def());
        let mut batch = Vec::new();
        for i in 0..5 {
            batch.push(row(i, &format!("r{}", i)));
        }
        ds.insert_batch(batch).unwrap();

        let all = ds.drain_all_rows();
        assert_eq!(all.len(), 5);
        assert_eq!(all[3], vec![Value::Int64(3), Value::Varchar("r3".into())]);
        assert!(ds.is_empty());
        assert!(ds.drain_all_rows().is_empty(), "重复 drain 得空");
    }
}
