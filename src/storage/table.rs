//! 表抽象
//!
//! 整合列存主存储 + Delta 层

use crate::common::config::CompactStrategy;
use crate::common::error::Result;
use crate::common::types::{TableDef, IndexDef, ColumnDef};
use crate::common::column_data::{ColumnData, ColumnValue};
use crate::common::value_cmp::total_cmp;
use crate::Value;
use crate::executor::vector::{DataChunk, Vector};

use super::column_store::{matches_predicate, matches_predicate_typed, ColumnStore, PredicateOp};
use super::delta_store::DeltaStore;
use super::index::inverted_index::InvertedIndex;
use super::index::skiplist::SkipListIndex;
use super::vector_index::{HnswIndex, HnswConfig, DistanceMetric, Neighbor, SearchTrace};
use crate::common::error::EngramDbError;

/// 按指定列对列式数据做聚簇重排
///
/// 同一键值的行在结果中物理连续。
/// 时间复杂度 O(n)，空间复杂度 O(n)。
fn cluster_columns(columns: &[Vec<Value>], cluster_col_idx: usize) -> Vec<Vec<Value>> {
    let num_rows = columns.first().map(|c| c.len()).unwrap_or(0);
    if num_rows == 0 || cluster_col_idx >= columns.len() {
        return columns.to_vec();
    }

    let num_cols = columns.len();

    let cluster_col = &columns[cluster_col_idx];

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

    let mut permutation = Vec::with_capacity(num_rows);
    for key in &group_order {
        if let Some(indices) = group_map.get(key) {
            permutation.extend_from_slice(indices);
        }
    }

    let mut result = Vec::with_capacity(num_cols);
    for col in columns {
        let mut new_col = Vec::with_capacity(num_rows);
        for &idx in &permutation {
            new_col.push(col[idx].clone());
        }
        result.push(new_col);
    }

    result
}

/// TTL 存活判定：`created_ms >= cutoff` 视为存活（null/非 Timestamp 列 → 存活）。
///
/// 直接用类型化列取值，零 Value 构造；cutoff 由 `TableDef::ttl_cutoff_ms` 一次算出。
#[inline]
fn ttl_col_alive(col: &ColumnData, row: usize, cutoff_ms: i64) -> bool {
    if row < col.len() {
        if let Some(n) = &col.nulls {
            if row < n.len() && n.test(row) {
                return true; // null → 不判定过期
            }
        }
        match &col.values {
            ColumnValue::Timestamp(v) => return v[row] >= cutoff_ms,
            _ => return true,
        }
    }
    true
}

/// 按主键列对列式数据做全排序（分层索引：compact 排序，段内有序 → 稀疏二分）
///
/// 排序键 = 主键列（数值按数值语义：Int32/Int64/Timestamp 互比）。
fn sort_columns_by_key(columns: &mut [Vec<Value>], pk_idx: usize) {
    let num_rows = columns.first().map(|c| c.len()).unwrap_or(0);
    if num_rows == 0 || pk_idx >= columns.len() {
        return;
    }
    let mut perm: Vec<usize> = (0..num_rows).collect();
    perm.sort_unstable_by(|&a, &b| cmp_values_for_sort(&columns[pk_idx][a], &columns[pk_idx][b]));
    let reordered: Vec<Vec<Value>> = columns
        .iter()
        .map(|col| perm.iter().map(|&i| col[i].clone()).collect())
        .collect();
    for (col, new_col) in columns.iter_mut().zip(reordered) {
        *col = new_col;
    }
}

/// Value 全序比较（排序用）
///
/// 委托 `common::value_cmp::total_cmp`（统一 NULL-aware 跨类型语义）。
fn cmp_values_for_sort(a: &Value, b: &Value) -> std::cmp::Ordering {
    total_cmp(a, b)
}

/// 将行列表转置为列格式（行 -> 列式存储）
fn transpose_rows(rows: &[Vec<Value>], num_cols: usize) -> Vec<Vec<Value>> {
    let num_rows = rows.len();
    if num_rows == 0 {
        return vec![vec![]; num_cols];
    }
    let mut columns = vec![Vec::with_capacity(num_rows); num_cols];
    for row in rows {
        for col_idx in 0..num_cols {
            if col_idx < row.len() {
                columns[col_idx].push(row[col_idx].clone());
            } else {
                columns[col_idx].push(Value::Null);
            }
        }
    }
    columns
}

/// 表（整合列存 + Delta）
pub struct Table {
    pub def: TableDef,
    pub column_store: ColumnStore,
    pub delta_store: DeltaStore,
    /// 当前表的 Delta 合并策略
    compact_strategy: CompactStrategy,
    /// 二级索引（跳表实现，支持覆盖索引）
    ///
    /// v0.12.0 新增。key 为索引名，value 为跳表索引实例。
    indexes: std::collections::HashMap<String, SkipListIndex>,
    /// 向量 HNSW 索引（v0.12.0 优先级 3）
    ///
    /// key 为索引名，value 为 (HNSW 索引, hnsw_id -> row_id 映射)。
    /// 用于向量列的近似最近邻搜索，支持 L2/内积/余弦距离。
    vector_indexes: std::collections::HashMap<String, (HnswIndex, Vec<u32>)>,
    /// 全文检索倒排索引（v0.15.0 新增）
    ///
    /// key 为列名，value 为对应列的倒排索引。
    /// 通过 CREATE INDEX ... USING fts(column) 创建。
    fts_indexes: std::collections::HashMap<String, InvertedIndex>,
    /// 分层索引：全表稠密 BTreeMap 主键索引（legacy 模式，默认关闭）
    ///
    /// - false（默认）：主键点查走「Delta 稠密 + 列存稀疏」分层索引，内存节省 ~99.9%
    /// - true：额外维护全表 BTreeMap（v0.17.0 前行为，向后兼容/回退）
    /// 无论开关如何，`lookup_primary_key` 语义完全一致。
    primary_index: Option<std::collections::BTreeMap<crate::Value, u32>>,
    /// 分层索引：compact 时按主键排序写入列存（段内有序 → 稀疏索引二分）
    sort_compact_by_pk: bool,
    /// 分层索引：旧全表 BTreeMap 是否启用
    primary_index_legacy: bool,
    /// 分层索引：列存稀疏索引 granule 行数
    sparse_granule_rows: u32,
}

impl Table {
    /// 估算比较谓词可跳过的 row group 数（Zone Map，M1-6）
    ///
    /// 列存 row group 用 chunk min/max 判断可跳过；Delta 层全读（不可跳过，计 1 组）。
    pub fn estimate_skip_for(
        &self,
        col_idx: usize,
        op: PredicateOp,
        val: &Value,
    ) -> (usize, usize) {
        let (total, skipped) = self.column_store.estimate_skip(col_idx, op, val);
        let delta_total = if self.delta_store.len() > 0 { 1 } else { 0 };
        (total + delta_total, skipped)
    }

    pub fn new(def: TableDef, strategy: CompactStrategy) -> Self {
        let row_group_size = match strategy {
            CompactStrategy::Adaptive { max_threshold, .. } => max_threshold as u32,
            _ => 122_880,
        };
        let cs = ColumnStore::new_with_sparse(def.clone(), row_group_size, 8192);
        let ds = DeltaStore::new(def.clone());
        Self {
            def,
            column_store: cs,
            delta_store: ds,
            compact_strategy: strategy,
            indexes: std::collections::HashMap::new(),
            vector_indexes: std::collections::HashMap::new(),
            fts_indexes: std::collections::HashMap::new(),
            primary_index: None,
            sort_compact_by_pk: true,
            primary_index_legacy: false,
            sparse_granule_rows: 8192,
        }
    }

    /// 分层索引配置（Database 创建表后调用，从 Config 注入）
    ///
    /// - `sort_compact_by_pk`：compact 时按主键排序写入列存（段内有序 → 稀疏二分）
    /// - `legacy`：启用旧全表稠密 BTreeMap（向后兼容/回退）
    /// - `granule_rows`：稀疏索引 granule 行数
    pub fn set_index_config(&mut self, sort_compact_by_pk: bool, legacy: bool, granule_rows: u32) {
        self.sort_compact_by_pk = sort_compact_by_pk;
        self.primary_index_legacy = legacy;
        self.sparse_granule_rows = granule_rows;
        if self.def.primary_key_index().is_some() {
            if legacy && self.primary_index.is_none() {
                self.primary_index = Some(std::collections::BTreeMap::new());
            } else if !legacy {
                self.primary_index = None;
            }
        }
        // 应用 granule 大小（仅在列存为空时生效；已有数据保持原粒度）
        self.column_store.set_sparse_granule(granule_rows);
    }

    /// 是否启用了主键索引
    #[inline]
    pub fn has_primary_index(&self) -> bool {
        self.def.primary_key_index().is_some()
    }

    /// legacy 全表 BTreeMap 主键索引是否启用（表级状态）
    #[inline]
    pub fn primary_index_legacy_enabled(&self) -> bool {
        self.primary_index_legacy
    }

    /// Perf03：通过全局 row_id 读取完整行（用于 PrimaryKeyLookup 命中后回表）
    ///
    /// row_id 分配规则：
    /// - 0..column_store.total_rows()：列存主存储中的行
    /// - 其后：Delta 层中的行（内部使用绝对 row_id 存储）
    pub fn get_row_by_id(&mut self, row_id: u32) -> Result<Option<Vec<crate::Value>>> {
        let cs_rows = self.column_store.total_rows();
        let row_id_u = row_id as u64;
        if row_id_u < cs_rows {
            // 位于列存主存储：定位 row_group 和 row_idx（P3.3：O(1) 算术定位，替代线性扫描）
            // 注意：最后一个 row group 可能不满 row_group_size，需回退校验
            let rg_size = self.column_store.row_group_size() as u64;
            let mut located_rg: Option<usize> = None;
            let mut located_row_in_rg: Option<usize> = None;

            if rg_size > 0 {
                let estimated_rg = (row_id_u / rg_size) as usize;
                let estimated_row = (row_id_u % rg_size) as usize;
                // 校验估算位置是否落在最后一个不满的 group 内
                if let Some(rg) = self.column_store.row_groups().get(estimated_rg) {
                    if (estimated_row as u64) < rg.row_count as u64 {
                        located_rg = Some(estimated_rg);
                        located_row_in_rg = Some(estimated_row);
                    }
                }
            }

            // 估算失效（最后一个 group 不满）时回退线性扫描
            if located_rg.is_none() {
                let mut remaining = row_id_u;
                for (rg_idx, rg) in self.column_store.row_groups().iter().enumerate() {
                    let rc = rg.row_count as u64;
                    if remaining < rc {
                        located_rg = Some(rg_idx);
                        located_row_in_rg = Some(remaining as usize);
                        break;
                    }
                    remaining -= rc;
                }
            }

            let rg_idx = match located_rg {
                Some(i) => i,
                None => return Ok(None),
            };
            let row_in_rg = located_row_in_rg.unwrap();
            let num_cols = self.def.columns.len();
            let mut row: Vec<crate::Value> = Vec::with_capacity(num_cols);
            for col_idx in 0..num_cols {
                let col_data = self.column_store.read_column(rg_idx, col_idx)?;
                if row_in_rg < col_data.len() {
                    row.push(col_data.get(row_in_rg));
                } else {
                    row.push(crate::Value::Null);
                }
            }
            // TTL 过滤：过期行视为不存在
            if self.def.is_expired(&row) {
                return Ok(None);
            }
            Ok(Some(row))
        } else {
            // 位于 Delta 层：使用绝对 row_id 读取
            let delta_row = self.delta_store.get(row_id_u);
            if let Some(r) = delta_row {
                // TTL 过滤：过期行视为不存在
                if self.def.is_expired(&r) {
                    return Ok(None);
                }
                Ok(Some(r.to_vec()))
            } else {
                Ok(None)
            }
        }
    }

    /// 按列索引裁剪读取一行（Perf03 列裁剪加速）
    ///
    /// 只读取 `col_indices` 指定的列，避免无关注列的全行拷贝。
    /// 返回的 `Vec<Vec<Value>>` 是单行（按 `col_indices` 顺序排列）。
    /// 若 row 不存在返回 Ok(vec![])。
    pub fn get_row_by_id_columns(&mut self, row_id: u32, col_indices: &[usize]) -> Result<Vec<Vec<crate::Value>>> {
        let cs_rows = self.column_store.total_rows();
        let row_id_u = row_id as u64;
        let row_opt: Option<Vec<crate::Value>> = if row_id_u < cs_rows {
            // 位于列存主存储：定位 row_group 和 row_idx（P3.3：O(1) 算术定位，替代线性扫描）
            let rg_size = self.column_store.row_group_size() as u64;
            let mut located_rg: Option<usize> = None;
            let mut located_row_in_rg: Option<usize> = None;

            if rg_size > 0 {
                let estimated_rg = (row_id_u / rg_size) as usize;
                let estimated_row = (row_id_u % rg_size) as usize;
                if let Some(rg) = self.column_store.row_groups().get(estimated_rg) {
                    if (estimated_row as u64) < rg.row_count as u64 {
                        located_rg = Some(estimated_rg);
                        located_row_in_rg = Some(estimated_row);
                    }
                }
            }

            // 估算失效（最后一个 group 不满）时回退线性扫描
            if located_rg.is_none() {
                let mut remaining = row_id_u;
                for (rg_idx, rg) in self.column_store.row_groups().iter().enumerate() {
                    let rc = rg.row_count as u64;
                    if remaining < rc {
                        located_rg = Some(rg_idx);
                        located_row_in_rg = Some(remaining as usize);
                        break;
                    }
                    remaining -= rc;
                }
            }

            match located_rg {
                Some(rg_idx) => {
                    let row_in_rg = located_row_in_rg.unwrap();
                    let mut row: Vec<crate::Value> = Vec::with_capacity(col_indices.len());
                    for &col_idx in col_indices {
                        let col_data = self.column_store.read_column(rg_idx, col_idx)?;
                        let v = if row_in_rg < col_data.len() {
                            col_data.get(row_in_rg)
                        } else {
                            crate::Value::Null
                        };
                        row.push(v);
                    }
                    Some(row)
                }
                None => None,
            }
        } else {
            // 位于 Delta 层
            match self.delta_store.get(row_id_u) {
                Some(r) => {
                    let row: Vec<crate::Value> = col_indices.iter().map(|&i| r[i].clone()).collect();
                    Some(row)
                }
                None => None,
            }
        };

        match row_opt {
            Some(row) => {
                // TTL 检查需要全列视图，将未读到的列填 Null
                let mut full_row = vec![crate::Value::Null; self.def.columns.len()];
                for (i, &ci) in col_indices.iter().enumerate() {
                    full_row[ci] = row[i].clone();
                }
                if self.def.is_expired(&full_row) {
                    Ok(Vec::new())
                } else {
                    Ok(vec![row])
                }
            }
            None => Ok(Vec::new()),
        }
    }

    /// 通过主键值查找 row_id（分层索引三段查）
    ///
    /// ① legacy 全表 BTreeMap（primary_index_legacy=true 时）
    /// ② Delta 层稠密索引（新数据热命中）
    /// ③ 列存层稀疏索引（granule 路由 + 段内确认）
    /// 数值主键类型归一化兜底（Int32/Int64/Timestamp 互查）每段都生效。
    pub fn lookup_primary_key(&mut self, key: &crate::Value) -> Option<u32> {
        let pk_idx = self.def.primary_key_index()?;

        // ① legacy 全表 BTreeMap
        if self.primary_index_legacy {
            if let Some(idx) = self.primary_index.as_ref() {
                if let Some(v) = idx.get(key) {
                    return Some(*v);
                }
                // 数值归一化
                use crate::Value::*;
                match key {
                    Int32(v) => idx.get(&Int64(*v as i64)).copied()
                        .or_else(|| idx.get(&Timestamp(*v as i64)).copied())
                        .or_else(|| self.lookup_primary_key_layered(key, pk_idx)),
                    Int64(v) => idx.get(&Int32(*v as i32)).copied()
                        .or_else(|| idx.get(&Timestamp(*v)).copied())
                        .or_else(|| self.lookup_primary_key_layered(key, pk_idx)),
                    Timestamp(v) => idx.get(&Int64(*v)).copied()
                        .or_else(|| idx.get(&Int32(*v as i32)).copied())
                        .or_else(|| self.lookup_primary_key_layered(key, pk_idx)),
                    _ => self.lookup_primary_key_layered(key, pk_idx),
                }
            } else {
                self.lookup_primary_key_layered(key, pk_idx)
            }
        } else {
            self.lookup_primary_key_layered(key, pk_idx)
        }
    }

    /// 分层主键点查：Delta 稠密 → 列存稀疏（各含数值归一化）
    fn lookup_primary_key_layered(&mut self, key: &crate::Value, pk_idx: usize) -> Option<u32> {
        // ② Delta 层稠密索引（含数值归一化）
        if let Some(rid) = self.delta_store.pk_lookup_normalized(key) {
            return Some(rid);
        }
        // ③ 列存层稀疏索引
        if let Ok(Some(rid)) = self.column_store.locate_pk(key) {
            return Some(rid);
        }
        // 列存稀疏：数值类型归一化展开（Int32/Int64/Timestamp 互查）
        use crate::Value::*;
        match key {
            Int32(v) => [Int64(*v as i64), Timestamp(*v as i64)]
                .iter().find_map(|k| self.column_store.locate_pk(k).ok().flatten()),
            Int64(v) => [Int32(*v as i32), Timestamp(*v)]
                .iter().find_map(|k| self.column_store.locate_pk(k).ok().flatten()),
            Timestamp(v) => [Int64(*v), Int32(*v as i32)]
                .iter().find_map(|k| self.column_store.locate_pk(k).ok().flatten()),
            _ => {
                let _ = pk_idx;
                None
            }
        }
    }

    /// 获取主键索引引用（用于 planner/executor 检测）
    pub fn primary_index(&self) -> Option<&std::collections::BTreeMap<crate::Value, u32>> {
        self.primary_index.as_ref()
    }

    // ---------- Perf03：主键索引内部维护方法 ----------

    /// 向主键索引插入单条 (pk_value, row_id)
    #[inline]
    fn primary_index_insert(&mut self, row: &[crate::Value], row_id: u32) {
        if let Some(pk_idx) = self.def.primary_key_index() {
            if let Some(pk_val) = row.get(pk_idx).cloned() {
                if let Some(idx) = self.primary_index.as_mut() {
                    idx.insert(pk_val, row_id);
                }
            }
        }
    }

    /// 从主键索引删除单条 (pk_value, _)
    #[inline]
    fn primary_index_remove(&mut self, row: &[crate::Value]) {
        if let Some(pk_idx) = self.def.primary_key_index() {
            if let Some(pk_val) = row.get(pk_idx) {
                if let Some(idx) = self.primary_index.as_mut() {
                    idx.remove(pk_val);
                }
            }
        }
    }

    /// 向主键索引批量插入多行（使用 base_row_id 递增编号）
    #[inline]
    fn primary_index_insert_batch(&mut self, rows: &[Vec<crate::Value>], base_row_id: u32) {
        if let (Some(pk_idx), Some(idx)) = (self.def.primary_key_index(), self.primary_index.as_mut()) {
            for (i, row) in rows.iter().enumerate() {
                if let Some(pk_val) = row.get(pk_idx).cloned() {
                    idx.insert(pk_val, base_row_id + i as u32);
                }
            }
        }
    }

    /// 从主键索引批量删除多行
    #[inline]
    fn primary_index_remove_batch(&mut self, rows: &[Vec<crate::Value>]) {
        if let (Some(pk_idx), Some(idx)) = (self.def.primary_key_index(), self.primary_index.as_mut()) {
            for row in rows {
                if let Some(pk_val) = row.get(pk_idx) {
                    idx.remove(pk_val);
                }
            }
        }
    }

    /// 从列存 + Delta 层重建主键索引（重启后恢复用）
    pub fn rebuild_primary_index(&mut self) -> Result<()> {
        let pk_idx = match self.def.primary_key_index() {
            Some(i) => i,
            None => return Ok(()),
        };
        // M1-7：无条件重建（索引缺失时也从零构建，而非直接返回）
        let mut idx = std::collections::BTreeMap::new();

        // 从列存遍历
        let mut row_id = 0u32;
        for rg_idx in 0..self.column_store.row_group_count() {
            let col_data = self.column_store.read_column(rg_idx, pk_idx)?;
            for val in col_data.iter_values() {
                idx.insert(val, row_id);
                row_id += 1;
            }
        }

        // 从 Delta 层遍历
        for (delta_row_id, row) in self.delta_store.all_rows() {
            if let Some(pk_val) = row.get(pk_idx) {
                idx.insert(pk_val.clone(), delta_row_id as u32);
            }
        }

        self.primary_index = Some(idx);
        Ok(())
    }

    /// 创建覆盖索引（v0.12.0 新增）
    ///
    /// 遍历现有数据构建索引。键列只支持单列（首列），
    /// 覆盖列冗余存储在索引条目中，查询时免回表。
    pub fn create_index(&mut self, index_name: &str, key_cols: &[usize], included_cols: &[usize], unique: bool) -> Result<()> {
        if self.indexes.contains_key(index_name) {
            return Err(EngramDbError::ConstraintViolation(
                format!("Index '{}' already exists", index_name)
            ));
        }
        for &k in key_cols {
            if k >= self.def.columns.len() {
                return Err(EngramDbError::ColumnNotFound(
                    format!("index key column index {} out of bounds", k)
                ));
            }
        }

        let mut skiplist = SkipListIndex::with_included(unique, included_cols.len());
        let mut next_row_id: u32 = 0;

        let make_key = |row: &[Value]| -> Value {
            if key_cols.len() == 1 {
                row[key_cols[0]].clone()
            } else {
                let parts: Vec<String> = key_cols.iter().map(|&k| format!("{:?}", row[k])).collect();
                Value::Varchar(parts.join("|"))
            }
        };

        let num_row_groups = self.column_store.row_group_count();
        for rg_idx in 0..num_row_groups {
            let mut key_data: Vec<Vec<Value>> = Vec::with_capacity(key_cols.len());
            for &k in key_cols {
                key_data.push(self.column_store.read_column(rg_idx, k)?.to_values());
            }
            let mut included_data: Vec<Vec<Value>> = Vec::with_capacity(included_cols.len());
            for &col_idx in included_cols {
                included_data.push(self.column_store.read_column(rg_idx, col_idx)?.to_values());
            }
            let row_count = key_data[0].len();
            for row_idx in 0..row_count {
                let row_vals: Vec<Value> = key_data.iter().map(|col| col[row_idx].clone()).collect();
                let key = if key_cols.len() == 1 { row_vals[0].clone() } else {
                    Value::Varchar(key_cols.iter().map(|&k| format!("{:?}", row_vals[key_cols.iter().position(|&x| x == k).unwrap_or(0)])).collect::<Vec<_>>().join("|"))
                };
                let mut inc_vals = Vec::with_capacity(included_cols.len());
                for col in &included_data {
                    inc_vals.push(col[row_idx].clone());
                }
                let inserted = skiplist.insert_with_included(key, next_row_id, &inc_vals);
                if unique && !inserted {
                    return Err(EngramDbError::ConstraintViolation(
                        format!("Duplicate key in unique index '{}'", index_name)
                    ));
                }
                next_row_id += 1;
            }
        }

        let delta_data = self.delta_store.all_rows();
        for (_rowid, row) in &delta_data {
            let key = make_key(row);
            let mut inc_vals = Vec::with_capacity(included_cols.len());
            for &col_idx in included_cols {
                inc_vals.push(row[col_idx].clone());
            }
            let inserted = skiplist.insert_with_included(key, next_row_id, &inc_vals);
            if unique && !inserted {
                return Err(EngramDbError::ConstraintViolation(
                    format!("Duplicate key in unique index '{}'", index_name)
                ));
            }
            next_row_id += 1;
        }

        if !self.def.indexes.iter().any(|i| i.name == index_name) {
            let index_def = IndexDef {
                name: index_name.to_string(),
                key_columns: key_cols.to_vec(),
                included_columns: included_cols.to_vec(),
                unique,
                index_type: "skiplist".to_string(),
            };
            self.def.indexes.push(index_def);
        }
        self.indexes.insert(index_name.to_string(), skiplist);

        Ok(())
    }

    /// 获取索引引用
    pub fn get_index(&self, name: &str) -> Option<&SkipListIndex> {
        self.indexes.get(name)
    }

    /// 获取所有索引
    pub fn indexes(&self) -> &std::collections::HashMap<String, SkipListIndex> {
        &self.indexes
    }

    /// 创建向量 HNSW 索引（v0.12.0 优先级 3）
    ///
    /// 遍历现有数据构建 HNSW 近似最近邻索引。
    /// 列必须是 Vector 类型。
    ///
    /// - `index_name`: 索引名称
    /// - `col_idx`: 向量列索引
    /// - `metric`: 距离度量（L2 / InnerProduct / Cosine）
    /// - `m`: 每层最大连接数（默认 16）
    /// - `ef_construction`: 构建时搜索宽度（默认 100）
    pub fn create_vector_index(&mut self, index_name: &str, col_idx: usize, metric: DistanceMetric, m: usize, ef_construction: usize) -> Result<()> {
        use crate::common::types::DataType;

        if self.vector_indexes.contains_key(index_name) {
            return Err(EngramDbError::ConstraintViolation(
                format!("Vector index '{}' already exists", index_name)
            ));
        }
        if col_idx >= self.def.columns.len() {
            return Err(EngramDbError::ColumnNotFound(
                format!("column index {} out of bounds", col_idx)
            ));
        }

        // 验证列类型
        let col_def = &self.def.columns[col_idx];
        let dim = match &col_def.data_type {
            DataType::Vector { dim } => *dim,
            DataType::VectorInt8 { dim } => *dim,
            _ => return Err(EngramDbError::InvalidFormat(
                format!("column '{}' is not a vector type", col_def.name)
            )),
        };

        if dim == 0 {
            return Err(EngramDbError::InvalidFormat(
                "vector column dimension is 0".into()
            ));
        }

        // VectorInt8 列自动启用量化存储
        let is_vector_int8 = matches!(col_def.data_type, DataType::VectorInt8 { .. });

        let config = HnswConfig {
            dim,
            m,
            m_max0: m * 2,
            ef_construction,
            ef_search: 50,
            metric,
            quantize: is_vector_int8,
        };
        let mut hnsw = HnswIndex::new(config);
        let mut id_mapping = Vec::new();

        // 从列存主存储加载向量数据
        let mut current_row_id = 0u32;
        let num_row_groups = self.column_store.row_group_count();
        for rg_idx in 0..num_row_groups {
            let col_data = self.column_store.read_column(rg_idx, col_idx)?;
            for val in col_data.iter_values() {
                if let Value::Vector(v) = val {
                    let hnsw_id = hnsw.insert(v)?;
                    // 确保映射向量足够大
                    if hnsw_id as usize >= id_mapping.len() {
                        id_mapping.resize(hnsw_id as usize + 1, 0);
                    }
                    id_mapping[hnsw_id as usize] = current_row_id;
                } else if let Value::VectorInt8(v) = val {
                    let f32_vec: Vec<f32> = v.iter().map(|x| *x as f32).collect();
                    let hnsw_id = hnsw.insert(f32_vec)?;
                    if hnsw_id as usize >= id_mapping.len() {
                        id_mapping.resize(hnsw_id as usize + 1, 0);
                    }
                    id_mapping[hnsw_id as usize] = current_row_id;
                }
                current_row_id += 1;
            }
        }

        // 从 Delta 层加载向量数据
        let delta_data = self.delta_store.all_rows();
        for (_rowid, row) in &delta_data {
            if let Value::Vector(v) = &row[col_idx] {
                let hnsw_id = hnsw.insert(v.clone())?;
                if hnsw_id as usize >= id_mapping.len() {
                    id_mapping.resize(hnsw_id as usize + 1, 0);
                }
                id_mapping[hnsw_id as usize] = current_row_id;
            } else if let Value::VectorInt8(v) = &row[col_idx] {
                let f32_vec: Vec<f32> = v.iter().map(|x| *x as f32).collect();
                let hnsw_id = hnsw.insert(f32_vec)?;
                if hnsw_id as usize >= id_mapping.len() {
                    id_mapping.resize(hnsw_id as usize + 1, 0);
                }
                id_mapping[hnsw_id as usize] = current_row_id;
            }
            current_row_id += 1;
        }

        self.vector_indexes.insert(index_name.to_string(), (hnsw, id_mapping));

        Ok(())
    }

    /// 获取向量索引引用
    pub fn get_vector_index(&self, name: &str) -> Option<&HnswIndex> {
        self.vector_indexes.get(name).map(|(idx, _)| idx)
    }

    /// 获取所有向量索引
    pub fn vector_indexes(&self) -> &std::collections::HashMap<String, (HnswIndex, Vec<u32>)> {
        &self.vector_indexes
    }

    /// 向量相似度搜索（v0.12.0 优先级 3）
    ///
    /// 返回 top-k 最近邻的行 ID 和距离（行 ID 为表内行号）。
    pub fn vector_search(&self, index_name: &str, query: &[f32], k: usize) -> Result<Vec<Neighbor>> {
        let (results, _trace) = self.vector_search_with_trace(index_name, query, k)?;
        Ok(results)
    }

    /// 向量相似度搜索 + 搜索 trace（v0.15.0 V13 新增）
    ///
    /// 返回 (top-k 最近邻, 搜索 trace)。trace 包含访问路径、入口点、候选节点数等
    /// 可追溯信息，Agent 场景下用于溯源推理路径。
    pub fn vector_search_with_trace(
        &self,
        index_name: &str,
        query: &[f32],
        k: usize,
    ) -> Result<(Vec<Neighbor>, SearchTrace)> {
        let (index, id_mapping) = self.vector_indexes.get(index_name)
            .ok_or_else(|| EngramDbError::IndexNotFound(index_name.into()))?;

        let (hnsw_results, mut trace) = index.search_with_trace(query, k);
        // 将 HNSW 内部 ID 转换为表行 ID
        let neighbors: Vec<Neighbor> = hnsw_results.into_iter()
            .map(|n| Neighbor {
                id: id_mapping.get(n.id as usize).copied().unwrap_or(n.id),
                distance: n.distance,
            })
            .collect();

        // 更新 trace 中的 top_k_ids 为表行 ID（而非 HNSW 内部 ID）
        trace.top_k_ids = neighbors.iter().map(|n| n.id).collect();
        // 距离不变（id_mapping 只影响 ID，不影响距离）

        Ok((neighbors, trace))
    }

    /// 序列化所有索引为字节（v0.12.0 索引持久化）
    ///
    /// 格式（向后兼容，旧版本只读取 skip list 段）：
    /// - skiplist_count: u32
    /// - 重复 skiplist_count 次：
    ///   - name_len: u32
    ///   - name: [u8; name_len]
    ///   - index_data_len: u32
    ///   - index_data: [u8; index_data_len]
    /// - vector_index_count: u32  （v0.12.0 优先级 3 新增）
    /// - 重复 vector_index_count 次：
    ///   - name_len: u32
    ///   - name: [u8; name_len]
    ///   - hnsw_data_len: u32
    ///   - hnsw_data: [u8; hnsw_data_len]
    ///   - id_mapping_len: u32
    ///   - id_mapping: [u32; id_mapping_len]
    pub fn indexes_to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::new();

        // --- SkipList 索引段（与旧格式完全一致）---
        let count = self.indexes.len() as u32;
        buf.extend_from_slice(&count.to_le_bytes());

        for (name, index) in &self.indexes {
            // name
            buf.extend_from_slice(&(name.len() as u32).to_le_bytes());
            buf.extend_from_slice(name.as_bytes());

            // index data
            let index_bytes = index.to_bytes();
            buf.extend_from_slice(&(index_bytes.len() as u32).to_le_bytes());
            buf.extend_from_slice(&index_bytes);
        }

        // --- 向量 HNSW 索引段（v0.12.0 优先级 3 新增）---
        let vec_count = self.vector_indexes.len() as u32;
        buf.extend_from_slice(&vec_count.to_le_bytes());

        for (name, (hnsw, id_mapping)) in &self.vector_indexes {
            // name
            buf.extend_from_slice(&(name.len() as u32).to_le_bytes());
            buf.extend_from_slice(name.as_bytes());

            // hnsw data
            let hnsw_bytes = hnsw.to_bytes();
            buf.extend_from_slice(&(hnsw_bytes.len() as u32).to_le_bytes());
            buf.extend_from_slice(&hnsw_bytes);

            // id mapping
            buf.extend_from_slice(&(id_mapping.len() as u32).to_le_bytes());
            for &rid in id_mapping {
                buf.extend_from_slice(&rid.to_le_bytes());
            }
        }

        // --- 主键 Mark Index 段（v0.17.0 M1-7 新增）---
        // 完整主键索引持久化（mark 间隔=1）：重启免全行扫描重建，
        // 点查保持 O(log N) 二分精确定位。旧文件无此段 → 读取时重建兜底。
        // 分层索引：仅 legacy 模式（primary_index_legacy=true）写入；
        // 格式：[mark_len:u32][bincode(BTreeMap<Value, u32>)]
        match &self.primary_index {
            Some(idx) => {
                let bytes = bincode::serialize(idx)
                    .expect("BTreeMap<Value, u32> serialization cannot fail");
                buf.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
                buf.extend_from_slice(&bytes);
            }
            None => buf.extend_from_slice(&0u32.to_le_bytes()),
        }

        // --- 稀疏主键索引段（分层索引 v0.19 新增）---
        // 格式：[sparse_len:u32][bincode((Vec<IndexGranule>, sorted))]
        match self.column_store.sparse_state() {
            Some((bytes, sorted)) => {
                let payload = bincode::serialize(&(bytes, sorted))
                    .expect("sparse index serialization cannot fail");
                buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
                buf.extend_from_slice(&payload);
            }
            None => buf.extend_from_slice(&0u32.to_le_bytes()),
        }

        buf
    }

    /// 从字节反序列化加载所有索引（v0.12.0 索引持久化）
    ///
    /// 支持旧格式（只有 skip list）和新格式（skip list + vector）。
    /// 旧文件中 vector_index_count 段不存在时，读取到末尾即停止。
    pub fn indexes_from_bytes(&mut self, data: &[u8]) -> Result<()> {
        if data.len() < 4 {
            return Err(EngramDbError::InvalidFormat("index section too short".into()));
        }

        let mut offset = 0;

        // --- SkipList 索引段 ---
        let count = u32::from_le_bytes(data[offset..offset+4].try_into().unwrap()) as usize;
        offset += 4;

        for _ in 0..count {
            // name
            if offset + 4 > data.len() {
                return Err(EngramDbError::InvalidFormat("truncated index name length".into()));
            }
            let name_len = u32::from_le_bytes(data[offset..offset+4].try_into().unwrap()) as usize;
            offset += 4;
            if offset + name_len > data.len() {
                return Err(EngramDbError::InvalidFormat("truncated index name".into()));
            }
            let name = String::from_utf8(data[offset..offset+name_len].to_vec())
                .map_err(|e| EngramDbError::InvalidFormat(format!("invalid index name: {}", e)))?;
            offset += name_len;

            // index data
            if offset + 4 > data.len() {
                return Err(EngramDbError::InvalidFormat("truncated index data length".into()));
            }
            let data_len = u32::from_le_bytes(data[offset..offset+4].try_into().unwrap()) as usize;
            offset += 4;
            if offset + data_len > data.len() {
                return Err(EngramDbError::InvalidFormat("truncated index data".into()));
            }
            let index = SkipListIndex::from_bytes(&data[offset..offset+data_len])?;
            offset += data_len;

            self.indexes.insert(name, index);
        }

        // --- 向量 HNSW 索引段（可选，旧格式文件可能没有）---
        if offset + 4 > data.len() {
            return Ok(()); // 旧格式，没有向量索引段
        }

        let vec_count = u32::from_le_bytes(data[offset..offset+4].try_into().unwrap()) as usize;
        offset += 4;

        for _ in 0..vec_count {
            // name
            if offset + 4 > data.len() {
                return Err(EngramDbError::InvalidFormat("truncated vector index name length".into()));
            }
            let name_len = u32::from_le_bytes(data[offset..offset+4].try_into().unwrap()) as usize;
            offset += 4;
            if offset + name_len > data.len() {
                return Err(EngramDbError::InvalidFormat("truncated vector index name".into()));
            }
            let name = String::from_utf8(data[offset..offset+name_len].to_vec())
                .map_err(|e| EngramDbError::InvalidFormat(format!("invalid vector index name: {}", e)))?;
            offset += name_len;

            // hnsw data
            if offset + 4 > data.len() {
                return Err(EngramDbError::InvalidFormat("truncated hnsw data length".into()));
            }
            let hnsw_len = u32::from_le_bytes(data[offset..offset+4].try_into().unwrap()) as usize;
            offset += 4;
            if offset + hnsw_len > data.len() {
                return Err(EngramDbError::InvalidFormat("truncated hnsw data".into()));
            }
            let hnsw = HnswIndex::from_bytes(&data[offset..offset+hnsw_len])?;
            offset += hnsw_len;

            // id mapping
            if offset + 4 > data.len() {
                return Err(EngramDbError::InvalidFormat("truncated id mapping length".into()));
            }
            let map_len = u32::from_le_bytes(data[offset..offset+4].try_into().unwrap()) as usize;
            offset += 4;
            if offset + map_len * 4 > data.len() {
                return Err(EngramDbError::InvalidFormat("truncated id mapping data".into()));
            }
            let mut id_mapping = Vec::with_capacity(map_len);
            for _ in 0..map_len {
                id_mapping.push(u32::from_le_bytes(data[offset..offset+4].try_into().unwrap()));
                offset += 4;
            }

            self.vector_indexes.insert(name, (hnsw, id_mapping));
        }

        // --- 主键 Mark Index 段（v0.17.0 M1-7 新增，可选）---
        // 旧格式文件（v0.17.0 之前）没有该段：剩余 0 字节 → 返回，由
        // 调用方（Database::load_indexes）做全量重建兜底。
        if offset + 4 > data.len() {
            return Ok(()); // 旧格式，无主键段
        }
        let mark_len = u32::from_le_bytes(data[offset..offset+4].try_into().unwrap()) as usize;
        offset += 4;
        if offset + mark_len > data.len() {
            return Err(EngramDbError::InvalidFormat("truncated primary mark index".into()));
        }
        if mark_len > 0 {
            let idx: std::collections::BTreeMap<crate::Value, u32> =
                bincode::deserialize(&data[offset..offset+mark_len]).map_err(|e| {
                    crate::common::error::EngramDbError::Serialization(e.to_string())
                })?;
            self.primary_index = Some(idx);
        }
        // 注意：mark_len == 0 表示表无主键索引（primary_index 保持 None）
        offset += mark_len;

        // --- 稀疏主键索引段（分层索引 v0.19 新增，可选）---
        // v0.19 之前文件无此段 → 剩余 0 字节 → 由调用方重建兜底。
        if offset + 4 > data.len() {
            return Ok(()); // 旧格式，无稀疏段
        }
        let sparse_len = u32::from_le_bytes(data[offset..offset+4].try_into().unwrap()) as usize;
        offset += 4;
        if sparse_len > 0 {
            if offset + sparse_len > data.len() {
                return Err(EngramDbError::InvalidFormat("truncated sparse index".into()));
            }
            let (bytes, sorted): (Vec<u8>, bool) =
                bincode::deserialize(&data[offset..offset+sparse_len]).map_err(|e| {
                    crate::common::error::EngramDbError::Serialization(e.to_string())
                })?;
            if !self.column_store.sparse_restore(&bytes, sorted) {
                return Err(EngramDbError::InvalidFormat("invalid sparse index data".into()));
            }
        }

        Ok(())
    }

    /// 测试辅助：清空内存主键索引（模拟无持久化段的旧文件）
    #[cfg(test)]
    pub fn clear_primary_index_for_test(&mut self) {
        self.primary_index = None;
    }

    /// 获取当前合并策略
    pub fn compact_strategy(&self) -> CompactStrategy {
        self.compact_strategy
    }

    /// 设置合并策略（运行时动态切换）
    pub fn set_compact_strategy(&mut self, strategy: CompactStrategy) {
        self.compact_strategy = strategy;
    }

    /// 列存主存储引用
    pub fn column_store(&self) -> &ColumnStore {
        &self.column_store
    }

    /// 列存主存储可变引用
    pub fn column_store_mut(&mut self) -> &mut ColumnStore {
        &mut self.column_store
    }

    /// Delta 层引用
    pub fn delta_store(&self) -> &DeltaStore {
        &self.delta_store
    }

    /// Delta 层可变引用
    pub fn delta_store_mut(&mut self) -> &mut DeltaStore {
        &mut self.delta_store
    }

    /// 表定义引用
    pub fn def(&self) -> &TableDef {
        &self.def
    }

    /// 表定义可变引用
    pub fn def_mut(&mut self) -> &mut TableDef {
        &mut self.def
    }

    /// 同步列存列的 data_type（从 TableDef 修正 Vector dim 等）
    pub fn sync_column_data_types(&mut self) {
        self.column_store.sync_data_types(&self.def);
    }

    /// 设置聚簇列（方案B：Delta 聚簇）
    ///
    /// 设置后，compact 时会按该列的值分组写入列存，
    /// 相同 key 的行物理上连续，可大幅提升按该列的范围查询性能。
    ///
    /// 典型场景：AI Agent 交互存储按 session_id 聚簇，
    /// 查询单个会话的全部消息时只需顺序扫描少量连续数据块。
    pub fn set_cluster_key(&mut self, column_name: &str) -> Result<()> {
        self.def.set_cluster_key(column_name)
            .map_err(|e| crate::common::error::EngramDbError::ColumnNotFound(e))?;
        Ok(())
    }

    /// 获取聚簇列索引
    pub fn cluster_key(&self) -> Option<usize> {
        self.def.cluster_key
    }

    /// 插入数据
    ///
    /// 优化：大批量数据直接写入列存（P1），小批量走 Delta 层
    /// 阈值：超过 row_group_size 的 1/4 时直接走列式路径，避免行存→列存转换开销
    ///
    /// Compact 调度：写入 Delta 后根据策略决定是否触发合并
    /// 主键唯一性检查（对齐 MemoryEngine 语义；无主键表零开销）
    ///
    /// 检查已存在主键 + 同批内自重复。位于落盘前，失败时零副作用。
    /// 分层模式：Delta 稠密 + 列存稀疏跨层检查（legacy BTreeMap 开关兼容）。
    #[inline]
    fn check_pk_uniqueness(&mut self, rows: &[Vec<crate::Value>]) -> Result<()> {
        let Some(pk_idx) = self.def.primary_key_index() else {
            return Ok(());
        };
        let mut seen = std::collections::HashSet::new();
        for row in rows {
            if let Some(cell) = row.get(pk_idx) {
                let exists = self.pk_exists(cell)?;
                if exists || !seen.insert(cell.clone()) {
                    return Err(EngramDbError::ConstraintViolation(format!(
                        "UNIQUE constraint failed: {}={:?}",
                        self.def.columns[pk_idx].name, cell
                    )));
                }
            }
        }
        Ok(())
    }

    /// 主键唯一性检查（单行版，insert_row apply 路径用）
    #[inline]
    fn check_pk_uniqueness_one(&mut self, row: &[crate::Value]) -> Result<()> {
        let Some(pk_idx) = self.def.primary_key_index() else {
            return Ok(());
        };
        if let Some(cell) = row.get(pk_idx) {
            if self.pk_exists(cell)? {
                return Err(EngramDbError::ConstraintViolation(format!(
                    "UNIQUE constraint failed: {}={:?}",
                    self.def.columns[pk_idx].name, cell
                )));
            }
        }
        Ok(())
    }

    /// 跨层主键存在性检查（分层索引）
    #[inline]
    fn pk_exists(&mut self, cell: &crate::Value) -> Result<bool> {
        if self.primary_index_legacy {
            if let Some(idx) = self.primary_index.as_ref() {
                if idx.contains_key(cell) {
                    return Ok(true);
                }
            }
        }
        if self.delta_store.pk_lookup_normalized(cell).is_some() {
            return Ok(true);
        }
        Ok(self.column_store.locate_pk(cell)?.is_some())
    }

    /// 跨层主键占用检查（排除自身 row_id；UPDATE 主键迁移用）
    #[inline]
    fn pk_occupied_by_other(&mut self, cell: &crate::Value, self_row_id: u32) -> Result<bool> {
        if self.primary_index_legacy {
            if let Some(idx) = self.primary_index.as_ref() {
                if let Some(&r) = idx.get(cell) {
                    if r != self_row_id {
                        return Ok(true);
                    }
                }
            }
        }
        if let Some(r) = self.delta_store.pk_lookup_normalized(cell) {
            if r != self_row_id {
                return Ok(true);
            }
        }
        if let Some(r) = self.column_store.locate_pk(cell)? {
            if r != self_row_id {
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub fn insert(&mut self, mut rows: Vec<Vec<Value>>) -> Result<u64> {
        let count = rows.len() as u64;

        // 类型强转：Varchar → Vector（处理 JSON 字面量）
        for row in rows.iter_mut() {
            for (col_idx, val) in row.iter_mut().enumerate() {
                self.coerce_value_for_column(col_idx, val)?;
            }
        }

        // AUTO_INCREMENT 自增分配（v0.14.0）
        // 用户未提供 auto_increment 列的值（或提供 0/NULL）时，自动分配并递增
        let auto_inc_cols: Vec<(usize, &ColumnDef)> = self.def.columns.iter().enumerate()
            .filter(|(_, c)| c.auto_increment)
            .collect();
        if !auto_inc_cols.is_empty() {
            for row in rows.iter_mut() {
                for (col_idx, col_def) in &auto_inc_cols {
                    if *col_idx >= row.len() {
                        if *col_idx >= row.len() {
                            row.resize(*col_idx + 1, Value::Null);
                        }
                        row[*col_idx] = Value::Int64(self.def.next_auto_increment_id as i64);
                        self.def.next_auto_increment_id += 1;
                        continue;
                    }
                    match &row[*col_idx] {
                        Value::Null => {
                            row[*col_idx] = Value::Int64(self.def.next_auto_increment_id as i64);
                            self.def.next_auto_increment_id += 1;
                        }
                        Value::Int64(n) => {
                            if *n == 0 {
                                row[*col_idx] = Value::Int64(self.def.next_auto_increment_id as i64);
                                self.def.next_auto_increment_id += 1;
                            } else if (*n as u64) >= self.def.next_auto_increment_id {
                                self.def.next_auto_increment_id = (*n as u64) + 1;
                            }
                        }
                        Value::Int32(n) => {
                            if *n == 0 {
                                row[*col_idx] = Value::Int64(self.def.next_auto_increment_id as i64);
                                self.def.next_auto_increment_id += 1;
                            } else if (*n as u64) >= self.def.next_auto_increment_id {
                                self.def.next_auto_increment_id = (*n as u64) + 1;
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        // TTL 时间戳自动填充（v0.15.0）
        // 当表有 TTL 且指定了 ttl_column 时，如果该列的值为 Null 或空，自动填充当前时间
        if let Some(ttl_col) = self.def.ttl_column {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as i64;
            for row in rows.iter_mut() {
                if ttl_col >= row.len() || row[ttl_col].is_null() {
                    if ttl_col >= row.len() {
                        row.resize(ttl_col + 1, Value::Null);
                    }
                    row[ttl_col] = Value::Timestamp(now_ms);
                }
            }
        }

        // NOT NULL 约束检查
        for (col_idx, col_def) in self.def.columns.iter().enumerate() {
            if !col_def.nullable {
                for (row_idx, row) in rows.iter().enumerate() {
                    if row_idx < row.len() && row[col_idx].is_null() {
                        return Err(EngramDbError::ConstraintViolation(
                            format!("NOT NULL constraint failed: column '{}'", col_def.name)
                        ));
                    }
                }
            }
        }

        // 主键唯一性检查（已存在 + 同批自重复；失败时零副作用）
        self.check_pk_uniqueness(&rows)?;

        let direct_threshold = (self.column_store.row_group_size() / 4) as usize;

        // 计算插入前的总行数，用于索引 row_id 计算
        let base_row_id = self.def.row_count as u32;

        if rows.len() >= direct_threshold && rows.len() >= 1000 {
            // P1: 大批量直接走列式路径，跳过 Delta 层
            self.column_store.append_rows(&rows)?;
        } else {
            // 小批量写入 Delta 层
            self.delta_store.insert_batch(rows.clone())?;
            // 根据策略决定是否触发合并
            let _ = self.maybe_compact()?;
        }

        // 更新总行数
        self.def.row_count += count;

        // Perf03：更新主键索引
        if self.primary_index.is_some() {
            self.primary_index_insert_batch(&rows, base_row_id);
        }

        // 更新所有二级索引（v0.12.0 覆盖索引）
        if !self.indexes.is_empty() {
            self.update_indexes_for_rows(&rows, base_row_id);
        }

        // 更新所有向量索引（v0.12.0 优先级 3）
        if !self.vector_indexes.is_empty() {
            self.update_vector_indexes_for_rows(&rows, base_row_id);
        }

        Ok(count)
    }

    /// 列式批量插入（③：批量 INSERT 列式路径）
    ///
    /// 语义与 `insert(rows)` 完全一致：类型强转 / AUTO_INCREMENT / TTL /
    /// NOT NULL 约束 / 主键 + 二级 + 向量索引维护，仅落盘走列式
    /// `append_columns`（比 append_rows 快约 2x，跳过行→列转置）。
    ///
    /// 输入：每列一个 Vec<Value>，列数与表定义一致、每列等长。
    /// 仅适用于完整行（所有列都提供）的批量写入场景。
    pub fn insert_columns(&mut self, mut columns: Vec<Vec<Value>>) -> Result<u64> {
        let num_rows = if columns.is_empty() { 0 } else { columns[0].len() };
        if num_rows == 0 {
            return Ok(0);
        }

        // 防御：列数与表定义一致
        if columns.len() != self.def.columns.len() {
            return Err(EngramDbError::ConstraintViolation(format!(
                "INSERT column count mismatch: expected {}, got {}",
                self.def.columns.len(),
                columns.len()
            )));
        }

        // 类型强转：Varchar → Vector（处理 JSON 字面量）
        for (col_idx, col) in columns.iter_mut().enumerate() {
            for val in col.iter_mut() {
                self.coerce_value_for_column(col_idx, val)?;
            }
        }

        // AUTO_INCREMENT 自增分配（与 insert() 相同的分配规则）
        let auto_inc_cols: Vec<usize> = self.def.columns.iter().enumerate()
            .filter(|(_, c)| c.auto_increment)
            .map(|(i, _)| i)
            .collect();
        for col_idx in &auto_inc_cols {
            for val in columns[*col_idx].iter_mut() {
                match val {
                    Value::Null => {
                        *val = Value::Int64(self.def.next_auto_increment_id as i64);
                        self.def.next_auto_increment_id += 1;
                    }
                    Value::Int64(n) => {
                        if *n == 0 {
                            *val = Value::Int64(self.def.next_auto_increment_id as i64);
                            self.def.next_auto_increment_id += 1;
                        } else if (*n as u64) >= self.def.next_auto_increment_id {
                            self.def.next_auto_increment_id = (*n as u64) + 1;
                        }
                    }
                    Value::Int32(n) => {
                        if *n == 0 {
                            *val = Value::Int64(self.def.next_auto_increment_id as i64);
                            self.def.next_auto_increment_id += 1;
                        } else if (*n as u64) >= self.def.next_auto_increment_id {
                            self.def.next_auto_increment_id = (*n as u64) + 1;
                        }
                    }
                    _ => {}
                }
            }
        }

        // TTL 时间戳自动填充（与 insert() 相同规则）
        if let Some(ttl_col) = self.def.ttl_column {
            if ttl_col < columns.len() {
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as i64;
                for val in columns[ttl_col].iter_mut() {
                    if val.is_null() {
                        *val = Value::Timestamp(now_ms);
                    }
                }
            }
        }

        // NOT NULL 约束检查
        for (col_idx, col_def) in self.def.columns.iter().enumerate() {
            if !col_def.nullable {
                for val in &columns[col_idx] {
                    if val.is_null() {
                        return Err(EngramDbError::ConstraintViolation(
                            format!("NOT NULL constraint failed: column '{}'", col_def.name)
                        ));
                    }
                }
            }
        }

        // 主键唯一性检查（列式：转置为行后检查，语义与 insert() 一致）
        if self.def.primary_key_index().is_some() {
            let rows = transpose_columns_to_rows(&columns, num_rows);
            self.check_pk_uniqueness(&rows)?;
        }

        let direct_threshold = (self.column_store.row_group_size() / 4) as usize;
        let base_row_id = self.def.row_count as u32;

        // 落盘：列式直接写入（大批量直落列存，小批量走 Delta）
        if num_rows >= direct_threshold && num_rows >= 1000 {
            self.column_store.append_columns(&columns)?;
        } else {
            self.delta_store.insert_columns(columns.clone())?;
            let _ = self.maybe_compact()?;
        }

        // 更新总行数
        self.def.row_count += num_rows as u64;

        // 索引维护（与 insert() 一致；仅在需要时才转置为行）
        if self.def.primary_key_index().is_some() || !self.indexes.is_empty() || !self.vector_indexes.is_empty() {
            let rows = transpose_columns_to_rows(&columns, num_rows);
            if self.primary_index_legacy {
                self.primary_index_insert_batch(&rows, base_row_id);
            }
            if !self.indexes.is_empty() {
                self.update_indexes_for_rows(&rows, base_row_id);
            }
            if !self.vector_indexes.is_empty() {
                self.update_vector_indexes_for_rows(&rows, base_row_id);
            }
        }

        Ok(num_rows as u64)
    }

    /// Varchar → Vector/VectorInt8 类型强转（v0.16.0）
    ///
    /// 当目标列是 VECTOR/VectorInt8 类型时，将 Varchar 字符串字面量
    /// 解析为 JSON 数组并转换为对应的向量类型。
    /// 支持格式: '[1.0, 2.0, 3.0]' 或 [1.0, 2.0, 3.0]
    fn coerce_value_for_column(&self, col_idx: usize, value: &mut Value) -> Result<()> {
        if col_idx >= self.def.columns.len() {
            return Ok(());
        }
        let col_def = &self.def.columns[col_idx];

        match col_def.data_type {
            crate::common::types::DataType::Vector { .. }
            | crate::common::types::DataType::VectorInt8 { .. } => {
                if let Value::Varchar(s) = value {
                    // 去除首尾空白和引号（如果存在）
                    let trimmed = s.trim().trim_matches('\'').trim_matches('"');
                    // 去除数组括号
                    let inner = trimmed
                        .trim_start_matches('[')
                        .trim_end_matches(']');

                    // 解析为 f32 数组
                    let floats: Vec<f32> = inner
                        .split(',')
                        .map(|v| v.trim().parse::<f32>())
                        .collect::<std::result::Result<Vec<f32>, _>>()
                        .map_err(|e| crate::common::error::EngramDbError::Parse(format!(
                            "Failed to parse vector literal: {}", e
                        )))?;

                    // 根据列类型转换
                    match col_def.data_type {
                        crate::common::types::DataType::Vector { .. } => {
                            *value = Value::Vector(floats);
                        }
                        crate::common::types::DataType::VectorInt8 { .. } => {
                            // INT8 量化：将 f32 转换为 i8
                            let int8_vec: Vec<i8> = floats.iter()
                                .map(|f| (*f * 127.0) as i8) // 简单量化
                                .collect();
                            *value = Value::VectorInt8(int8_vec);
                        }
                        _ => unreachable!(),
                    }
                }
            }
            _ => {} // 其他类型不做转换
        }
        Ok(())
    }

    /// 插入单行数据（事务提交后应用到存储层）
    ///
    /// 与 `insert()` 不同，此方法用于事务路径：
    /// 1. 由 executor 在 commit 后调用
    /// 2. row_id 由事务管理器分配（避免重复）
    /// 3. 直接写入 Delta 层（单行场景不需要列式路径优化）
    pub fn insert_row(&mut self, row_id: u32, row: &[Value]) -> Result<()> {
        // NOT NULL 约束检查 + AUTO_INCREMENT 自动分配
        let mut owned_row: Vec<Value> = row.to_vec();

        // 类型强转：Varchar → Vector（处理 JSON 字面量）
        for (col_idx, val) in owned_row.iter_mut().enumerate() {
            self.coerce_value_for_column(col_idx, val)?;
        }

        for (col_idx, col_def) in self.def.columns.iter().enumerate() {
            if col_def.auto_increment && col_idx < owned_row.len() {
                match &owned_row[col_idx] {
                    Value::Null => {
                        owned_row[col_idx] = Value::Int64(self.def.next_auto_increment_id as i64);
                        self.def.next_auto_increment_id += 1;
                    }
                    Value::Int64(n) => {
                        if *n == 0 {
                            owned_row[col_idx] = Value::Int64(self.def.next_auto_increment_id as i64);
                            self.def.next_auto_increment_id += 1;
                        } else if (*n as u64) >= self.def.next_auto_increment_id {
                            // 显式提供大于当前计数器的值，更新计数器
                            self.def.next_auto_increment_id = (*n as u64) + 1;
                        }
                    }
                    Value::Int32(n) => {
                        if *n == 0 {
                            owned_row[col_idx] = Value::Int64(self.def.next_auto_increment_id as i64);
                            self.def.next_auto_increment_id += 1;
                        } else if (*n as u64) >= self.def.next_auto_increment_id {
                            self.def.next_auto_increment_id = (*n as u64) + 1;
}
            }
            _ => {}
        }
    }
    if !col_def.nullable && col_idx < owned_row.len() && owned_row[col_idx].is_null() {
                return Err(EngramDbError::ConstraintViolation(
                    format!("NOT NULL constraint failed: column '{}'", col_def.name)
                ));
            }
        }

        // TTL 时间戳自动填充（v0.15.0）
        if let Some(ttl_col) = self.def.ttl_column {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as i64;
            if ttl_col >= owned_row.len() || owned_row[ttl_col].is_null() {
                if ttl_col >= owned_row.len() {
                    owned_row.resize(ttl_col + 1, Value::Null);
                }
                owned_row[ttl_col] = Value::Timestamp(now_ms);
            }
        }

        // 主键唯一性检查（apply 路径对齐 MemoryEngine）
        self.check_pk_uniqueness_one(&owned_row)?;

        // 写入 Delta 层（单行直接插入）
        self.delta_store.insert_row(row_id, owned_row)?;
        
        // 更新总行数
        self.def.row_count += 1;
        
        // Perf03：更新主键索引
        if self.primary_index.is_some() {
            self.primary_index_insert(row, row_id);
        }
        
        // 更新所有二级索引
        if !self.indexes.is_empty() {
            self.update_indexes_for_row(row_id, row)?;
        }
        
        // 更新所有向量索引
        if !self.vector_indexes.is_empty() {
            self.update_vector_indexes_for_row(row_id, row);
        }
        
        // 更新所有全文索引
        if !self.fts_indexes.is_empty() {
            self.update_fts_indexes_for_row(row_id, row);
        }
        
        Ok(())
    }
    
    /// 删除单行数据（事务提交后应用到存储层）
    ///
    /// 参数：row_id - 要删除的行 ID
    ///
    /// 流程：
    /// 1. 从 Delta 层删除指定 row_id 的行
    /// 2. 更新二级索引（删除对应条目）
    /// 3. 更新向量索引（tombstone 标记）
    /// 4. 更新总行数
    pub fn delete_row(&mut self, row_id: u32) -> Result<()> {
        // 从 Delta 层获取行数据（用于索引维护）
        let row_id_64 = row_id as u64;
        let old_row = self.delta_store.get(row_id_64);
        
        // 从 Delta 层删除
        self.delta_store.delete_row(row_id)?;
        
        // 更新总行数
        self.def.row_count = self.def.row_count.saturating_sub(1);
        
        // 如果有旧行数据，更新索引
        if let Some(ref row) = old_row {
            // Perf03：删除主键索引条目
            if self.primary_index.is_some() {
                self.primary_index_remove(row);
            }
            
            // 删除二级索引中的对应条目
            if !self.indexes.is_empty() {
                self.remove_indexes_for_rows(&[row.clone()], &[row_id]);
            }
            
            // 标记向量索引中的删除
            if !self.vector_indexes.is_empty() {
                self.remove_vector_indexes_for_rows(&[row_id]);
            }
            
            // 删除全文索引
            if !self.fts_indexes.is_empty() {
                self.remove_fts_indexes_for_row(row_id, row);
            }
        }
        
        Ok(())
    }
    
    /// 更新单行数据（事务提交后应用到存储层）
    ///
    /// 参数：
    /// - row_id - 要更新的行 ID
    /// - new_row - 更新后的新行数据
    ///
    /// 流程：
    /// 1. 获取旧行数据
    /// 2. 更新 Delta 层中的行
    /// 3. 更新二级索引（删旧条目 + 插新条目）
    /// 4. 更新向量索引
    pub fn update_row(&mut self, row_id: u32, new_row: &[Value]) -> Result<()> {
        // 获取旧行数据
        let row_id_64 = row_id as u64;
        let old_row = self.delta_store.get(row_id_64);
        
        // 更新 Delta 层
        self.delta_store.update_row_by_id(row_id, new_row.to_vec())?;
        
        // 从 Delta 层读取更新后的新行
        let updated_row = self.delta_store.get(row_id_64);
        
        // 如果有旧行数据，更新索引
        if let Some(ref old_r) = old_row {
            // Perf03：删除旧主键索引条目
            if self.primary_index.is_some() {
                self.primary_index_remove(old_r);
            }
            
            // 删除旧索引条目
            if !self.indexes.is_empty() {
                self.remove_indexes_for_rows(&[old_r.clone()], &[row_id]);
            }
            
            // 标记向量索引中的旧条目为 tombstone
            if !self.vector_indexes.is_empty() {
                self.remove_vector_indexes_for_rows(&[row_id]);
            }
        }
        
        // 插入新索引条目
        if let Some(ref new_r) = updated_row {
            // Perf03：插入新主键索引条目
            if self.primary_index.is_some() {
                self.primary_index_insert(new_r, row_id);
            }
            
            if !self.indexes.is_empty() {
                self.update_indexes_for_row(row_id, new_r)?;
            }
            
            if !self.vector_indexes.is_empty() {
                self.update_vector_indexes_for_row(row_id, new_r);
            }
        }
        
        Ok(())
    }
    
    /// 更新单行的二级索引（内部辅助方法）
    fn update_indexes_for_row(&mut self, row_id: u32, row: &[Value]) -> Result<()> {
        for idx_def in self.def.indexes.clone() {
            if let Some(index) = self.indexes.get_mut(&idx_def.name) {
                let key = row[idx_def.key_columns[0]].clone();
                let included_vals: Vec<Value> = idx_def.included_columns.iter()
                    .map(|&ci| row[ci].clone())
                    .collect();
                if idx_def.unique && !index.insert_with_included(key.clone(), row_id, &included_vals) {
                    return Err(EngramDbError::ConstraintViolation(
                        format!("UNIQUE constraint failed: index '{}'", idx_def.name)
                    ));
                }
                if !idx_def.unique {
                    index.insert_with_included(key, row_id, &included_vals);
                }
            }
        }
        Ok(())
    }
    
    /// 更新单行的向量索引（内部辅助方法）
    fn update_vector_indexes_for_row(&mut self, _row_id: u32, row: &[Value]) {
        // 向量索引更新逻辑：遍历 vector_indexes，尝试插入
        // vector_indexes 类型: HashMap<String, (HnswIndex, Vec<u32>)>
        for (_idx_name, (hnsw_idx, _row_ids)) in self.vector_indexes.iter_mut() {
            // 查找第一个向量类型的列
            for val in row.iter() {
                if let Value::Vector(ref vec) = val {
                    // 插入向量到 HnswIndex（返回分配的 ID）
                    // TODO: 将分配的 ID 与 row_id 关联存储
                    let _ = hnsw_idx.insert(vec.clone());
                    break; // 只处理第一个向量列
                } else if let Value::VectorInt8(ref vec) = val {
                    let f32_vec: Vec<f32> = vec.iter().map(|x| *x as f32).collect();
                    let _ = hnsw_idx.insert(f32_vec);
                    break;
                }
            }
        }
    }

    /// 为一批行更新所有二级索引（内部辅助方法）
    fn update_indexes_for_rows(&mut self, rows: &[Vec<Value>], base_row_id: u32) {
        for (row_idx, row) in rows.iter().enumerate() {
            let row_id = base_row_id + row_idx as u32;
            // 遍历所有索引，逐个更新
            for idx_def in self.def.indexes.clone() {
                if let Some(index) = self.indexes.get_mut(&idx_def.name) {
                    // 取键列值（目前只支持单列键）
                    let key = row[idx_def.key_columns[0]].clone();
                    // 取覆盖列值
                    let included_vals: Vec<Value> = idx_def.included_columns.iter()
                        .map(|&ci| row[ci].clone())
                        .collect();
                    index.insert_with_included(key, row_id, &included_vals);
                }
            }
        }
    }

    /// 为一批行更新所有向量索引（内部辅助方法，v0.12.0 优先级 3）
    fn update_vector_indexes_for_rows(&mut self, rows: &[Vec<Value>], base_row_id: u32) {
        // 收集需要更新的索引名（避免借用冲突）
        let index_names: Vec<String> = self.vector_indexes.keys().cloned().collect();

        for index_name in index_names {
            // 找到该索引对应的列（通过搜索表定义中的向量列）
            // 简化实现：遍历所有列，找到 Vector 类型的列
            // 注意：更精确的方式是在创建索引时记录列索引，
            // 这里为了简化，假设每个向量索引对应第一个 Vector 列
            let col_idx = self.def.columns.iter()
                .position(|c| matches!(c.data_type, crate::common::types::DataType::Vector { .. }));

            if let Some(col_idx) = col_idx {
                if let Some((hnsw, id_mapping)) = self.vector_indexes.get_mut(&index_name) {
                    for (row_idx, row) in rows.iter().enumerate() {
                        let row_id = base_row_id + row_idx as u32;
                        if let Value::Vector(v) = &row[col_idx] {
                            if let Ok(hnsw_id) = hnsw.insert(v.clone()) {
                                if hnsw_id as usize >= id_mapping.len() {
                                    id_mapping.resize(hnsw_id as usize + 1, 0);
                                }
                                id_mapping[hnsw_id as usize] = row_id;
                            }
                        } else if let Value::VectorInt8(v) = &row[col_idx] {
                            let f32_vec: Vec<f32> = v.iter().map(|x| *x as f32).collect();
                            if let Ok(hnsw_id) = hnsw.insert(f32_vec) {
                                if hnsw_id as usize >= id_mapping.len() {
                                    id_mapping.resize(hnsw_id as usize + 1, 0);
                                }
                                id_mapping[hnsw_id as usize] = row_id;
                            }
                        }
                    }
                }
            }
        }
    }

    /// 根据当前策略检查是否需要合并，需要则执行
    ///
    /// 返回本次合并的行数（0 表示未触发）。
    pub fn maybe_compact(&mut self) -> Result<usize> {
        let delta_len = self.delta_store.len();
        if delta_len == 0 {
            return Ok(0);
        }

        match self.compact_strategy {
            CompactStrategy::Manual => Ok(0),

            CompactStrategy::Full { threshold } => {
                if delta_len >= threshold {
                    let rows = delta_len;
                    self.compact_delta()?;
                    Ok(rows)
                } else {
                    Ok(0)
                }
            }

            CompactStrategy::Incremental { threshold, batch_size } => {
                if delta_len >= threshold {
                    self.compact_delta_partial(batch_size)
                } else {
                    Ok(0)
                }
            }

            CompactStrategy::Adaptive { min_threshold, max_threshold, pct_of_table, batch_size } => {
                let base_rows = self.def.row_count as f64;
                let pct_based = (base_rows * pct_of_table) as usize;
                let threshold = pct_based.clamp(min_threshold, max_threshold);

                if delta_len >= threshold {
                    self.compact_delta_partial(batch_size)
                } else {
                    Ok(0)
                }
            }
        }
    }

    /// 全表扫描（合并 Delta + 列存）
    pub fn scan(&mut self, column_indices: &[usize]) -> Result<Vec<Vec<Value>>> {
        let mut result = Vec::new();
        let full_row_for_ttl = self.def.has_ttl();
        // TTL 截止线一次算好（避免逐行 SystemTime::now()）
        let ttl_cutoff: Option<i64> = self.def.ttl_cutoff_ms();

        // 从列存读取
        for rg_idx in 0..self.column_store.row_group_count() {
            // T-TTL: 整个 RowGroup 全部过期（无存活行）→ 整组跳过
            if let (Some(ttl_col), Some(cut)) = (self.def.ttl_column, ttl_cutoff) {
                let cutoff_val = Value::Timestamp(cut);
                if self.column_store.can_skip_predicate(rg_idx, ttl_col, PredicateOp::GtEq, &cutoff_val) {
                    continue;
                }
            }

            // 读取需要的列
            let mut columns_data = Vec::new();
            for &col_idx in column_indices {
                let col_data = self.column_store.read_column(rg_idx, col_idx)?;
                columns_data.push(col_data.to_values());
            }

            // 按行组装
            if !columns_data.is_empty() {
                let row_count = columns_data[0].len();
                // TTL 列：每 RG 读一次克隆（O(N)，替代逐行重建完整行 O(N×M)）
                let ttl_col_data: Option<ColumnData> = match (full_row_for_ttl, self.def.ttl_column) {
                    (true, Some(ttl_col)) => {
                        let cd = self.column_store.read_column(rg_idx, ttl_col)?.clone();
                        Some(cd)
                    }
                    _ => None,
                };
                for row_idx in 0..row_count {
                    let mut row = Vec::with_capacity(column_indices.len());
                    for col in &columns_data {
                        row.push(col[row_idx].clone());
                    }
                    // TTL 过滤：跳过过期行（一次读 TTL 列 + 一次截up）
                    if let (Some(ttl_col), Some(cut)) = (&ttl_col_data, ttl_cutoff) {
                        if !ttl_col_alive(ttl_col, row_idx, cut) {
                            continue;
                        }
                    }
                    result.push(row);
                }
            }
        }

        // 从 Delta 层读取
        for (_, row) in self.delta_store.all_rows() {
            // TTL 过滤：跳过过期行
            if self.def.is_expired(&row) {
                continue;
            }
            let mut projected = Vec::with_capacity(column_indices.len());
            for &col_idx in column_indices {
                if col_idx < row.len() {
                    projected.push(row[col_idx].clone());
                } else {
                    projected.push(Value::Null);
                }
            }
            result.push(projected);
        }

        Ok(result)
    }

    /// 列存直传 DataChunk（性能优化版，跳过 row→column→row 转置）
    ///
    /// 与 `scan` 不同：直接返回 `Vec<DataChunk>`，避免调用方在 TableScan 算子中再做
    /// `DataChunk::from_rows` 和 `chunks_to_rows` 的来回转置（每次转置都做 cell 级 clone）。
    ///
    /// 实现要点：
    /// - 每个 Row Group 输出一组 `DataChunk`，每 chunk `VECTOR_SIZE=2048` 行
    /// - 跳过 `to_vec()` 全列拷贝，直接用 `read_column` 返回的 `&[Value]` 构造 Vector
    /// - 跨 Row Group 的边界 chunk 自动处理（最后一个 chunk 可能 < 2048 行）
    /// - Delta 层走原 `scan` 路径（数据量小，开销可忽略）
    pub fn scan_to_chunks(&mut self, column_indices: &[usize]) -> Result<Vec<DataChunk>> {
        self.scan_to_chunks_impl(column_indices, None)
    }

    /// 带 MinMax 跳过索引的全表扫描（P2.4）
    ///
    /// 当查询带简单比较谓词（如 `col > 100` / `col = 'x'`）时，
    /// 先对每个 row group 用 `can_skip_predicate` 判断其 [min, max] 是否
    /// 与谓词区间无交集；无交集则整个 row group 跳过，不解压不扫描。
    ///
    /// `skip_pred` 为 `(过滤列索引, 谓词操作符, 比较值)`。
    /// 注意：跳过索引只做粗粒度裁剪，结果仍可能包含不满足条件的行，
    /// 调用方仍需执行 Filter 算子做精确过滤。
    pub fn scan_to_chunks_with_skip(
        &mut self,
        column_indices: &[usize],
        skip_pred: Option<(usize, PredicateOp, Value)>,
    ) -> Result<Vec<DataChunk>> {
        self.scan_to_chunks_impl(column_indices, skip_pred)
    }

    fn scan_to_chunks_impl(
        &mut self,
        column_indices: &[usize],
        skip_pred: Option<(usize, PredicateOp, Value)>,
    ) -> Result<Vec<DataChunk>> {
        const BATCH_SIZE: usize = 2048; // 与 executor::vector::VECTOR_SIZE 对齐

        let mut chunks: Vec<DataChunk> = Vec::new();
        let full_row_for_ttl = self.def.has_ttl();
        // TTL 截止线一次算好（避免逐行 SystemTime::now()）
        let ttl_cutoff: Option<i64> = self.def.ttl_cutoff_ms();

        for rg_idx in 0..self.column_store.row_group_count() {
            // P2.4：MinMax 跳过索引 —— 整个 row group 可跳过时不解压
            if let Some((col_idx, op, val)) = &skip_pred {
                if self.column_store.can_skip_predicate(rg_idx, *col_idx, *op, val) {
                    continue;
                }
            }
            // T-TTL: 整个 RowGroup 全部过期（created < cutoff 无存活行）→ 整组跳过
            if let (Some(ttl_col), Some(cut)) = (self.def.ttl_column, ttl_cutoff) {
                let cutoff_val = Value::Timestamp(cut);
                if self.column_store.can_skip_predicate(rg_idx, ttl_col, PredicateOp::GtEq, &cutoff_val) {
                    continue;
                }
            }

            // 1. 读取需要的所有列（S2-M2：克隆类型化列，scan 直出 Vector::Typed）
            let mut col_owned: Vec<ColumnData> = Vec::with_capacity(column_indices.len());
            for &col_idx in column_indices {
                let col_data = self.column_store.read_column(rg_idx, col_idx)?;
                col_owned.push(col_data.clone());
            }

            if col_owned.is_empty() {
                continue;
            }

            let row_count = col_owned[0].len();

            // PREWHERE：谓词列在输出列时，batch 内用 Typed 谓词直扫筛幸存行
            // （零 Value 构造，只物化幸存行——1% 选择性只输出 1% 行）
            let pred_pos: Option<usize> = skip_pred
                .as_ref()
                .and_then(|(ci, _, _)| column_indices.iter().position(|&c| c == *ci));

            // TTL 列：可能不在输出列，每 RG 克隆一份（O(N) 一次，
            // 替代旧的逐行重建完整行 O(N×M) + 逐行 SystemTime::now()）
            let ttl_col_data: Option<ColumnData> = match (full_row_for_ttl, self.def.ttl_column) {
                (true, Some(ttl_col)) => {
                    let cd = self.column_store.read_column(rg_idx, ttl_col)?.clone();
                    Some(cd)
                }
                _ => None,
            };
            // 2. 按 batch_size 分块，逐 chunk 构造
            // S2-M2：take_front 移出类型化子列 → Vector::Typed（零 Value 转换）
            let mut batch_start = 0;
            while batch_start < row_count {
                let batch_len = BATCH_SIZE.min(row_count - batch_start);

                // PREWHERE + TTL 合流筛选：谓词幸存行 ∩ TTL 存活行
                // TTL 之前走 O(N×M) 逐行重建完整行 + 逐行 SystemTime::now()，
                // 现在并入同一 survivors 通道：只读一次 TTL 列（O(N)），cutoff 一次算好
                let survivors: Option<Vec<usize>> = {
                    let mut sel: Vec<usize> = match (pred_pos, &skip_pred) {
                        (Some(pos), Some((_, op, val))) => (0..batch_len)
                            .filter(|&j| matches_predicate_typed(&col_owned[pos], j, *op, val))
                            .collect(),
                        _ => (0..batch_len).collect(),
                    };
                    if let (Some(ttl_col), Some(cut)) = (&ttl_col_data, ttl_cutoff) {
                        sel.retain(|&j| ttl_col_alive(ttl_col, batch_start + j, cut));
                    }
                    if ttl_col_data.is_some() {
                        if sel.len() == batch_len {
                            None // 全部存活 → 走原始无滤波快路径
                        } else {
                            Some(sel)
                        }
                    } else if pred_pos.is_some() {
                        Some(sel)
                    } else {
                        None
                    }
                };

                let mut columns: Vec<Vector> = Vec::with_capacity(col_owned.len());
                if let Some(sel) = &survivors {
                    if sel.is_empty() {
                        // 本 batch 全过滤：消费列后跳过
                        for col in &mut col_owned {
                            col.take_front(batch_len);
                        }
                        batch_start += batch_len;
                        continue;
                    }
                    // 先 take_front 取本 batch，再按相对索引 gather 幸存行
                    for col in &mut col_owned {
                        let batch_col = col.take_front(batch_len);
                        columns.push(Vector::Typed(batch_col.gather(sel)));
                    }
                } else {
                    for col in &mut col_owned {
                        columns.push(Vector::Typed(col.take_front(batch_len)));
                    }
                }

                let out_count = survivors.as_ref().map_or(batch_len, |sel| sel.len());
                chunks.push(DataChunk {
                    count: out_count,
                    columns,
                });
                batch_start += batch_len;
            }
        }

        // Delta 层：转成单行 DataChunk（量小，开销可忽略）
        for (_, row) in self.delta_store.all_rows() {
            if self.def.is_expired(&row) {
                continue;
            }
            // PREWHERE：Delta 行级筛选（匹配 scan 层语义）
            if let Some((ci, op, val)) = &skip_pred {
                if row.get(*ci).map_or(true, |cell| !matches_predicate(cell, *op, val)) {
                    continue;
                }
            }
            let mut columns: Vec<Vector> = Vec::with_capacity(column_indices.len());
            for &col_idx in column_indices {
                let v = if col_idx < row.len() { row[col_idx].clone() } else { Value::Null };
                columns.push(Vector::Flat(vec![v]));
            }
            chunks.push(DataChunk {
                count: 1,
                columns,
            });
        }

        Ok(chunks)
    }

    /// 直接输出 `Vec<Vec<Value>>`（最末端的 TableScan 专用路径）
    ///
    /// 与 `scan_to_chunks` + `chunks_to_rows` 组合相比：
    /// - 跳过中间 `Vec<DataChunk>` 分配（chunk 数量 = row_count / 2048）
    /// - 直接填充最终 rows Vec，避免对 `DataChunk` 内部 Vector 再次拆分
    /// - 节省 1 轮 ~4M cell 克隆（VARCHAR 列特别显著）
    ///
    /// 调用方仍需 `column_names`（从 schema 派生），无需再做 chunks_to_rows。
    pub fn scan_to_rows_direct(&mut self, column_indices: &[usize]) -> Result<Vec<Vec<Value>>> {
        self.scan_to_rows_direct_impl(column_indices, None)
    }

    /// 带 MinMax 跳过索引的 `scan_to_rows_direct`（P3.2）
    ///
    /// 谓词语义与 `scan_to_chunks_with_skip` 相同。
    pub fn scan_to_rows_direct_with_skip(
        &mut self,
        column_indices: &[usize],
        skip_pred: Option<(usize, PredicateOp, Value)>,
    ) -> Result<Vec<Vec<Value>>> {
        self.scan_to_rows_direct_impl(column_indices, skip_pred)
    }

    fn scan_to_rows_direct_impl(
        &mut self,
        column_indices: &[usize],
        skip_pred: Option<(usize, PredicateOp, Value)>,
    ) -> Result<Vec<Vec<Value>>> {
        use super::column_store::{matches_predicate, matches_predicate_typed};

        // P-W1 PREWHERE：按 batch 处理，过滤在前、物化在后。
        // 1% 选择性场景：未 MinMax 跳过的 row group 中，1% 行被物化 → 节省 99% cell 克隆
        const BATCH_SIZE: usize = 2048;

        let mut rows: Vec<Vec<Value>> = Vec::new();
        let full_row_for_ttl = self.def.has_ttl();
        // TTL 截止线一次算好（避免逐行 SystemTime::now()）
        let ttl_cutoff: Option<i64> = self.def.ttl_cutoff_ms();

        // 谓词列在 output column_indices 中的位置
        // - Some(pos)：谓词列是输出列之一，可做 PREWHERE 短路
        // - None：谓词列不在输出（少见，例如 WHERE 仅引用非 SELECT 列），退化为全量扫描
        let pred_col_pos_in_output: Option<usize> = skip_pred.as_ref().and_then(|(col_idx, _, _)| {
            column_indices.iter().position(|&c| c == *col_idx)
        });

        for rg_idx in 0..self.column_store.row_group_count() {
            // P2.4/P3.2：MinMax 跳过索引 —— 整个 row group 可跳过时不解压
            if let Some((col_idx, op, val)) = &skip_pred {
                if self.column_store.can_skip_predicate(rg_idx, *col_idx, *op, val) {
                    continue;
                }
            }
            // T-TTL: 整个 RowGroup 全部过期（created < cutoff 无存活行）→ 整组跳过
            if let (Some(ttl_col), Some(cut)) = (self.def.ttl_column, ttl_cutoff) {
                let cutoff_val = Value::Timestamp(cut);
                if self.column_store.can_skip_predicate(rg_idx, ttl_col, PredicateOp::GtEq, &cutoff_val) {
                    continue;
                }
            }

            // S2-M3：克隆类型化列（比 to_values 便宜 4x；谓词列直扫）
            let mut col_owned: Vec<ColumnData> = Vec::with_capacity(column_indices.len());
            for &col_idx in column_indices {
                let col_data = self.column_store.read_column(rg_idx, col_idx)?;
                col_owned.push(col_data.clone());
            }
            if col_owned.is_empty() {
                continue;
            }

            let row_count = col_owned[0].len();

            // TTL 列：每 RG 克隆一份（O(N) 一次，替代逐行重建完整行 O(N×M)）
            let ttl_col_data: Option<ColumnData> = match (full_row_for_ttl, self.def.ttl_column) {
                (true, Some(ttl_col)) => {
                    let cd = self.column_store.read_column(rg_idx, ttl_col)?.clone();
                    Some(cd)
                }
                _ => None,
            };

            // 按 batch 处理：先按谓词列筛掉绝大多数行，再为幸存行构造完整 row
            for batch_start in (0..row_count).step_by(BATCH_SIZE) {
                let batch_end = std::cmp::min(batch_start + BATCH_SIZE, row_count);

                // 1. 找出 batch 内通过谓词的行索引（Typed 谓词列直扫，零 Value 构造）
                let mut survivors: Vec<usize> = match (pred_col_pos_in_output, &skip_pred) {
                    (Some(pos), Some((_, op, val))) => {
                        let col = &col_owned[pos];
                        (batch_start..batch_end)
                            .filter(|&i| i < col.len() && matches_predicate_typed(col, i, *op, val))
                            .collect()
                    }
                    _ => (batch_start..batch_end).collect(),
                };
                // 2. TTL 过滤：按 TTL 列存活集剔过期行（绝对行号，零完整行重建）
                if let (Some(ttl_col), Some(cut)) = (&ttl_col_data, ttl_cutoff) {
                    survivors.retain(|&row_idx| ttl_col_alive(ttl_col, row_idx, cut));
                }
                // 3. 为幸存行构造完整 row（避免对被过滤行分配 Vec + 克隆 cell）
                for row_idx in survivors {
                    let mut row: Vec<Value> = Vec::with_capacity(column_indices.len());
                    for col in &col_owned {
                        row.push(if row_idx < col.len() { col.get(row_idx) } else { Value::Null });
                    }
                    rows.push(row);
                }
            }
        }

        // Delta 层
        for (_, row) in self.delta_store.all_rows() {
            if self.def.is_expired(&row) {
                continue;
            }
            let mut projected = Vec::with_capacity(column_indices.len());
            for &col_idx in column_indices {
                projected.push(if col_idx < row.len() { row[col_idx].clone() } else { Value::Null });
            }
            rows.push(projected);
        }

        Ok(rows)
    }

    /// 合并 Delta 到列存
    ///
    /// P4 优化：DeltaStore 已改为列式存储，直接列对列追加，
    /// 省去行→列转置开销，compact 速度提升约 2x。
    ///
    /// v0.11.4 聚簇优化：如果表设置了 cluster_key，合并时按聚簇列分组写入，
    /// 同值行物理连续，提升按聚簇列查询的性能。
    pub fn compact_delta(&mut self) -> Result<()> {
        if self.delta_store.is_empty() {
            return Ok(());
        }

        let row_count = self.delta_store.len() as u64;

        // 如果有 TTL，过滤掉过期行
        if self.def.has_ttl() {
            let all_rows = self.delta_store.all_rows();
            let mut alive_rows: Vec<Vec<Value>> = Vec::new();
            for (_, row) in all_rows {
                if !self.def.is_expired(&row) {
                    alive_rows.push(row);
                }
            }
            // 清空 Delta 并用存活行重建
            self.delta_store.clear();
            // 重新插入未过期的行作为列式数据
            let alive_count = alive_rows.len();
            if alive_count > 0 {
                let mut columns = transpose_rows(&alive_rows, self.def.columns.len());
                if self.sort_compact_by_pk {
                    if let Some(pk_idx) = self.def.primary_key_index() {
                        sort_columns_by_key(&mut columns, pk_idx);
                    }
                }
                self.column_store.append_columns_sorted(&columns)?;
            }
            // 只减去被 TTL 淘汰的行数（存活行仍计入总数）
            self.def.row_count = self.def.row_count.saturating_sub(row_count - alive_count as u64);
            return Ok(());
        }

        // 根据是否有聚簇键选择写入方式；分层索引：有主键时优先按主键排序
        // （段内有序 → 稀疏索引二分定位；聚簇键排序仅在无主键时保留）
        let mut columns_to_write: Vec<Vec<Value>> = match self.def.cluster_key {
            Some(cluster_idx) => self.delta_store.clustered_column_data(cluster_idx),
            None => self.delta_store.column_data().to_vec(),
        };
        if self.sort_compact_by_pk {
            if let Some(pk_idx) = self.def.primary_key_index() {
                sort_columns_by_key(&mut columns_to_write, pk_idx);
            }
        }

        self.column_store.append_columns_sorted(&columns_to_write)?;
        self.delta_store.clear();
        // 注意：不更新 row_count —— Delta → 列存只是数据迁移，总行数不变。
        // 行数在 insert/insert_row/execute_columns 写入 Delta 时已累加。

        Ok(())
    }

    /// 增量合并 Delta 到列存（部分合并）
    ///
    /// 只合并最多 max_rows 行，控制单次阻塞时间。
    /// 用于自适应分桶合并策略。
    /// 返回实际合并的行数。
    ///
    /// v0.11.4：支持聚簇写入（有 cluster_key 时按聚簇列分组）
    pub fn compact_delta_partial(&mut self, max_rows: usize) -> Result<usize> {
        if self.delta_store.is_empty() {
            return Ok(0);
        }

        let actual_rows = max_rows.min(self.delta_store.len());
        if actual_rows == 0 {
            return Ok(0);
        }

        // 从 Delta 头部取出数据
        let mut columns = self.delta_store.drain_front(actual_rows);

        // 分层索引：有主键时优先按主键排序（段内有序 → 稀疏二分）
        if self.sort_compact_by_pk {
            if let Some(pk_idx) = self.def.primary_key_index() {
                sort_columns_by_key(&mut columns, pk_idx);
            }
        } else if let Some(cluster_idx) = self.def.cluster_key {
            // 无主键排序时保留聚簇重排
            columns = cluster_columns(&columns, cluster_idx);
        }

        // 合并到列存
        self.column_store.append_columns_sorted(&columns)?;
        // 注意：不更新 row_count —— Delta → 列存只是数据迁移，总行数不变。

        Ok(actual_rows)
    }

    /// 总行数（元数据级 O(1)，已通过 INSERT/DELETE/UPDATE 所有写路径精确维护）
    ///
    /// Perf01：供 COUNT(*) 短路和优化器估计行数使用。
    pub fn row_count(&self) -> u64 {
        self.def.row_count
    }

    /// TRUNCATE TABLE：清空所有数据（v0.15.0 新增）
    ///
    /// 清空 ColumnStore 和 DeltaStore，重置 row_count 为 0。
    /// 保留表结构、索引定义、HNSW 索引定义。
    pub fn truncate(&mut self) -> Result<()> {
        self.column_store.clear();
        self.delta_store.clear();
        self.def.row_count = 0;
        // 清空主键索引
        if let Some(idx) = &mut self.primary_index {
            idx.clear();
        }
        // 清空二级索引
        for index in self.indexes.values_mut() {
            index.clear();
        }
        // HNSW 索引：清空节点（保留配置）
        for (_, (hnsw, id_mapping)) in self.vector_indexes.iter_mut() {
            hnsw.clear();
            id_mapping.clear();
        }
        // 清空 FTS 索引
        for fts in self.fts_indexes.values_mut() {
            fts.clear();
        }
        Ok(())
    }

    /// 删除行（v0.12.0 新增，DELETE 支持）
    ///
    /// 当前实现：只支持删除 Delta 层的行（通过行索引定位）。
    /// 列存中的行暂不支持原地删除（LSM 风格，后续通过 tombstone + compact 实现）。
    ///
    /// 参数：delta_row_indices - Delta 层中行的索引（0-based，升序）
    /// 返回：被删除的行数
    pub fn delete_delta_rows(&mut self, delta_row_indices: &[usize]) -> Result<usize> {
        if delta_row_indices.is_empty() {
            return Ok(0);
        }

        // 计算 Delta 行的全局 row_id 基准（删除前）
        let delta_base_row_id = (self.def.row_count - self.delta_store.len() as u64) as u32;
        let row_ids: Vec<u32> = delta_row_indices.iter()
            .map(|&i| delta_base_row_id + i as u32)
            .collect();

        // 先收集被删除的行（用于索引维护）
        let deleted_rows = self.delta_store.delete_rows(delta_row_indices);
        let count = deleted_rows.len();

        // Perf03：更新主键索引（删除条目）
        if self.primary_index.is_some() && !deleted_rows.is_empty() {
            self.primary_index_remove_batch(&deleted_rows);
        }

        // 更新所有二级索引：删除对应的条目
        if !self.indexes.is_empty() && !deleted_rows.is_empty() {
            self.remove_indexes_for_rows(&deleted_rows, &row_ids);
        }

        // 更新所有向量索引：tombstone 标记删除
        if !self.vector_indexes.is_empty() && !row_ids.is_empty() {
            self.remove_vector_indexes_for_rows(&row_ids);
        }

        // 更新总行数
        self.def.row_count -= count as u64;

        Ok(count)
    }

    /// 更新 Delta 层的行（v0.12.0 新增，UPDATE 支持）
    ///
    /// 参数：updates - Vec<(delta_row_idx, Vec<(col_idx, new_value)>)>
    /// 返回：更新的行数
    pub fn update_delta_rows(&mut self, updates: &[(usize, Vec<(usize, Value)>)]) -> Result<usize> {
        if updates.is_empty() {
            return Ok(0);
        }

        // 计算 Delta 行的全局 row_id 基准
        let delta_base_row_id = (self.def.row_count - self.delta_store.len() as u64) as u32;

        // 主键迁移冲突前置检查（更新应用前，基于旧索引状态；含同批自重复）
        if let Some(pk_idx) = self.def.primary_key_index() {
            let mut seen = std::collections::HashSet::new();
            for &(row_idx, ref new_vals) in updates {
                let self_rid = delta_base_row_id + row_idx as u32;
                for &(col_idx, ref new_val) in new_vals {
                    if col_idx == pk_idx {
                        let occupied = self.pk_occupied_by_other(new_val, self_rid)?;
                        if occupied || !seen.insert(new_val.clone()) {
                            return Err(EngramDbError::ConstraintViolation(format!(
                                "UNIQUE constraint failed: {}={:?}",
                                self.def.columns[pk_idx].name, new_val
                            )));
                        }
                    }
                }
            }
        }

        let mut count = 0;
        let mut old_rows: Vec<Vec<Value>> = Vec::new();
        let mut new_rows: Vec<Vec<Value>> = Vec::new();
        let mut row_ids: Vec<u32> = Vec::new();

        for &(row_idx, ref new_vals) in updates {
            if let Some(old_row) = self.delta_store.update_row(row_idx, new_vals) {
                // 收集更新后的新行（从 Delta 层读取最新值）
                if let Some(new_row) = self.delta_store.get(row_idx as u64) {
                    old_rows.push(old_row);
                    new_rows.push(new_row);
                    row_ids.push(delta_base_row_id + row_idx as u32);
                }
                count += 1;
            }
        }

        // 更新索引：先删旧条目，再插新条目（delta 层 pk_index 已由 update_row 内部维护）
        if !old_rows.is_empty() {
            // 先删旧主键索引，再插新主键索引
            self.primary_index_remove_batch(&old_rows);
            for (i, new_row) in new_rows.iter().enumerate() {
                self.primary_index_insert(new_row, row_ids[i]);
            }
        }

        if !self.indexes.is_empty() && !old_rows.is_empty() {
            self.remove_indexes_for_rows(&old_rows, &row_ids);
            // 重新插入更新后的行
            for (i, new_row) in new_rows.iter().enumerate() {
                let row_id = row_ids[i];
                for idx_def in self.def.indexes.clone() {
                    if let Some(index) = self.indexes.get_mut(&idx_def.name) {
                        let key = new_row[idx_def.key_columns[0]].clone();
                        let included_vals: Vec<Value> = idx_def.included_columns.iter()
                            .map(|&ci| new_row[ci].clone())
                            .collect();
                        index.insert_with_included(key, row_id, &included_vals);
                    }
                }
            }
        }

        // 更新向量索引：旧向量 tombstone + 新向量插入（v0.12.0 优先级 3）
        if !self.vector_indexes.is_empty() && !old_rows.is_empty() {
            // 先标记旧向量为 tombstone
            self.remove_vector_indexes_for_rows(&row_ids);

            // 再插入新向量
            let index_names: Vec<String> = self.vector_indexes.keys().cloned().collect();
            for index_name in index_names {
                // 找到该索引对应的向量列
                let col_idx = self.def.columns.iter()
                    .position(|c| matches!(c.data_type, crate::common::types::DataType::Vector { .. }));

                if let Some(col_idx) = col_idx {
                    if let Some((hnsw, id_mapping)) = self.vector_indexes.get_mut(&index_name) {
                        for (i, new_row) in new_rows.iter().enumerate() {
                            let row_id = row_ids[i];
                            if let Value::Vector(v) = &new_row[col_idx] {
                                if let Ok(hnsw_id) = hnsw.insert(v.clone()) {
                                    if hnsw_id as usize >= id_mapping.len() {
                                        id_mapping.resize(hnsw_id as usize + 1, 0);
                                    }
                                    id_mapping[hnsw_id as usize] = row_id;
                                }
                            } else if let Value::VectorInt8(v) = &new_row[col_idx] {
                                let f32_vec: Vec<f32> = v.iter().map(|x| *x as f32).collect();
                                if let Ok(hnsw_id) = hnsw.insert(f32_vec) {
                                    if hnsw_id as usize >= id_mapping.len() {
                                        id_mapping.resize(hnsw_id as usize + 1, 0);
                                    }
                                    id_mapping[hnsw_id as usize] = row_id;
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(count)
    }

    /// 从所有二级索引中移除一批行（内部辅助方法）
    fn remove_indexes_for_rows(&mut self, rows: &[Vec<Value>], row_ids: &[u32]) {
        for (i, row) in rows.iter().enumerate() {
            let row_id = row_ids[i];
            for idx_def in self.def.indexes.clone() {
                if let Some(index) = self.indexes.get_mut(&idx_def.name) {
                    let key = row[idx_def.key_columns[0]].clone();
                    let _ = index.remove(&key, row_id);
                }
            }
        }
    }

    /// 从所有向量索引中移除一批行（内部辅助方法，v0.12.0 优先级 3）
    ///
    /// 使用 tombstone 标记逻辑删除，不物理移除 HNSW 节点。
    fn remove_vector_indexes_for_rows(&mut self, row_ids: &[u32]) {
        if self.vector_indexes.is_empty() {
            return;
        }

        // 对每个向量索引，找到 row_id 对应的 hnsw_id 并标记删除
        let index_names: Vec<String> = self.vector_indexes.keys().cloned().collect();
        for index_name in index_names {
            if let Some((hnsw, id_mapping)) = self.vector_indexes.get_mut(&index_name) {
                for &row_id in row_ids {
                    // 在映射中反向查找 hnsw_id
                    // 注意：row_id 是全局行号，映射是 hnsw_id -> row_id
                    // 这里用线性查找（删除操作不频繁，可接受）
                    if let Some(hnsw_id) = id_mapping.iter()
                        .position(|&rid| rid == row_id)
                        .map(|idx| idx as u32)
                    {
                        hnsw.mark_deleted(hnsw_id);
                    }
                }
            }
        }
    }

    /// 添加全文检索索引
    pub fn add_fts_index(&mut self, column_name: &str) -> Result<()> {
        // 检查列是否存在
        if self.def.column_index(column_name).is_none() {
            return Err(EngramDbError::Parse(format!("Column '{}' not found", column_name)));
        }
        // 检查列类型是否为 Varchar
        let col_idx = self.def.column_index(column_name).unwrap();
        if self.def.columns[col_idx].data_type != crate::common::types::DataType::Varchar {
            return Err(EngramDbError::Parse(format!("FTS index requires VARCHAR column, got {:?}", self.def.columns[col_idx].data_type)));
        }
        self.fts_indexes.insert(column_name.to_string(), InvertedIndex::new(column_name));
        Ok(())
    }

    /// 全文检索搜索
    pub fn search_fts(&self, column_name: &str, query: &str) -> Vec<u32> {
        if let Some(idx) = self.fts_indexes.get(column_name) {
            idx.search(query)
        } else {
            Vec::new()
        }
    }

    /// 更新全文索引（单行插入时）
    fn update_fts_indexes_for_row(&mut self, row_id: u32, row: &[Value]) {
        let col_names: Vec<String> = self.fts_indexes.keys().cloned().collect();
        for col_name in col_names {
            if let Some(col_idx) = self.def.column_index(&col_name) {
                if col_idx < row.len() {
                    if let Value::Varchar(text) = &row[col_idx] {
                        if let Some(idx) = self.fts_indexes.get_mut(&col_name) {
                            idx.add_document(row_id, text);
                        }
                    }
                }
            }
        }
    }

    /// 删除全文索引条目（单行删除时）
    fn remove_fts_indexes_for_row(&mut self, row_id: u32, row: &[Value]) {
        let col_names: Vec<String> = self.fts_indexes.keys().cloned().collect();
        for col_name in col_names {
            if let Some(col_idx) = self.def.column_index(&col_name) {
                if col_idx < row.len() {
                    if let Value::Varchar(text) = &row[col_idx] {
                        if let Some(idx) = self.fts_indexes.get_mut(&col_name) {
                            idx.remove_document(row_id, text);
                        }
                    }
                }
            }
        }
    }

    /// 获取 FTS 索引列表
    pub fn fts_indexes(&self) -> &std::collections::HashMap<String, InvertedIndex> {
        &self.fts_indexes
    }

    /// 获取 FTS 索引的可变引用
    pub fn fts_indexes_mut(&mut self) -> &mut std::collections::HashMap<String, InvertedIndex> {
        &mut self.fts_indexes
    }
}

/// 列式数据转置为行式（用于 insert_columns 的索引维护）
///
/// 仅在有索引需要维护时才调用；无索引的批量导入完全走列式路径。
fn transpose_columns_to_rows(columns: &[Vec<Value>], num_rows: usize) -> Vec<Vec<Value>> {
    let num_cols = columns.len();
    let mut rows: Vec<Vec<Value>> = Vec::with_capacity(num_rows);
    for r in 0..num_rows {
        let mut row = Vec::with_capacity(num_cols);
        for col in columns {
            row.push(col[r].clone());
        }
        rows.push(row);
    }
    rows
}

impl crate::storage::engine::EngineTableOps for Table {
    fn engine_type(&self) -> crate::common::types::EngineType {
        crate::common::types::EngineType::Columnar
    }

    fn def(&self) -> &crate::common::types::TableDef {
        &self.def
    }

    fn insert_rows(&mut self, rows: Vec<Vec<crate::Value>>) -> crate::common::error::Result<u64> {
        self.insert(rows)
    }

    fn insert_row(
        &mut self,
        row_id: u32,
        row: &[crate::Value],
    ) -> crate::common::error::Result<()> {
        self.insert_row(row_id, row)
    }

    fn update_row(
        &mut self,
        row_id: u32,
        new_row: &[crate::Value],
    ) -> crate::common::error::Result<()> {
        self.update_row(row_id, new_row)
    }

    fn delete_row(&mut self, row_id: u32) -> crate::common::error::Result<()> {
        self.delete_row(row_id)
    }

    fn truncate(&mut self) -> crate::common::error::Result<()> {
        self.truncate()
    }

    fn scan_to_chunks(
        &mut self,
        column_indices: &[usize],
        skip_pred: Option<(usize, crate::storage::column_store::PredicateOp, crate::Value)>,
    ) -> crate::common::error::Result<Vec<crate::executor::vector::DataChunk>> {
        self.scan_to_chunks_with_skip(column_indices, skip_pred)
    }

    fn get_row_by_id(
        &mut self,
        row_id: u32,
    ) -> crate::common::error::Result<Option<Vec<crate::Value>>> {
        self.get_row_by_id(row_id)
    }

    fn lookup_primary_key(&mut self, pk: &crate::Value) -> Option<u32> {
        Table::lookup_primary_key(self, pk)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::types::{ColumnDef, DataType, EngineType, TableDef};

    fn make_def(pk: bool) -> TableDef {
        TableDef {
            id: 1,
            engine: EngineType::Columnar,
            name: "t".to_string(),
            columns: vec![
                ColumnDef {
                    name: "id".into(),
                    data_type: DataType::Int64,
                    nullable: !pk,
                    is_primary_key: pk,
                    default_value: None,
                    auto_increment: false,
                },
                ColumnDef::new("v", DataType::Varchar),
            ],
            indexes: vec![],
            cluster_key: None,
            foreign_keys: vec![],
            next_auto_increment_id: 0,
            ttl_seconds: None,
            ttl_column: None,
            row_count: 0,
        }
    }

    fn row(id: i64, v: &str) -> Vec<Value> {
        vec![Value::Int64(id), Value::Varchar(v.to_string())]
    }

    #[test]
    fn test_insert_and_row_count() {
        let mut t = Table::new(make_def(false), CompactStrategy::manual());
        t.insert(vec![row(1, "a"), row(2, "b")]).unwrap();
        assert_eq!(t.row_count(), 2);
        assert_eq!(t.get_row_by_id(1).unwrap().unwrap(), row(2, "b"));
        assert!(t.get_row_by_id(5).unwrap().is_none());
    }

    #[test]
    fn test_insert_columns_and_pk_index() {
        let mut t = Table::new(make_def(true), CompactStrategy::manual());
        let cols = vec![
            vec![Value::Int64(1), Value::Int64(2), Value::Int64(3)],
            vec![Value::Varchar("a".into()), Value::Varchar("b".into()), Value::Varchar("c".into())],
        ];
        t.insert_columns(cols).unwrap();
        assert_eq!(t.row_count(), 3);
        assert_eq!(t.lookup_primary_key(&Value::Int64(2)), Some(1), "批量插入维护主键索引");
        assert_eq!(t.get_row_by_id(2).unwrap().unwrap(), row(3, "c"));
    }

    #[test]
    fn test_compact_preserves_row_id_read() {
        // 核心：compact（Delta→列存迁移）后按绝对 row_id 读取必须正确
        let mut t = Table::new(make_def(false), CompactStrategy::manual());
        let mut rows = Vec::new();
        for i in 0..100 {
            rows.push(row(i, &format!("r{}", i)));
        }
        t.insert(rows).unwrap();
        // 全部在 Delta 层
        assert_eq!(t.get_row_by_id(30).unwrap().unwrap()[0], Value::Int64(30));
        t.compact_delta_partial(30).unwrap();
        assert_eq!(t.row_count(), 100, "compact 只迁移数据，行数不变");
        // 前 30 行在列存，后 70 行在 Delta——绝对 row_id 读取必须稳定
        for i in [0usize, 29, 30, 60, 99] {
            assert_eq!(t.get_row_by_id(i as u32).unwrap().unwrap()[0], Value::Int64(i as i64),
                "compact 后 row_id={} 读取错位", i);
        }
        // 再次 compact 至全部迁移
        t.compact_delta_partial(1000).unwrap();
        assert_eq!(t.row_count(), 100);
        assert_eq!(t.get_row_by_id(99).unwrap().unwrap()[0], Value::Int64(99));
    }

    #[test]
    fn test_compact_delta_eviction_threshold() {
        // Incremental 策略：超过 threshold 后写入路径自动触发合并
        let def = make_def(false);
        let mut t = Table::new(def.clone(), CompactStrategy::incremental(10, 5));
        let mut rows = Vec::new();
        for i in 0..30 {
            rows.push(row(i, &format!("r{}", i)));
        }
        t.insert(rows).unwrap();
        assert_eq!(t.row_count(), 30);
        // 数据正确性（无论迁移发生在何处）
        for i in [0usize, 7, 15, 29] {
            assert_eq!(t.get_row_by_id(i as u32).unwrap().unwrap()[0], Value::Int64(i as i64));
        }
    }

    #[test]
    fn test_delete_delta_rows_updates_pk() {
        let mut t = Table::new(make_def(true), CompactStrategy::manual());
        let mut rows = Vec::new();
        for i in 0..10 {
            rows.push(row(i, &format!("r{}", i)));
        }
        t.insert(rows).unwrap();
        t.delete_delta_rows(&[2, 5]).unwrap();
        assert_eq!(t.row_count(), 8);
        assert_eq!(t.lookup_primary_key(&Value::Int64(2)), None, "删除行主键移除");
        assert_eq!(t.lookup_primary_key(&Value::Int64(5)), None);
        assert_eq!(t.lookup_primary_key(&Value::Int64(3)), Some(3), "未删行主键保留");
    }

    #[test]
    fn test_update_delta_rows_migrates_pk() {
        let mut t = Table::new(make_def(true), CompactStrategy::manual());
        let mut rows = Vec::new();
        for i in 0..5 {
            rows.push(row(i, &format!("r{}", i)));
        }
        t.insert(rows).unwrap();
        let n = t.update_delta_rows(&[(0, vec![(0, Value::Int64(100))])]).unwrap();
        assert_eq!(n, 1);
        assert_eq!(t.lookup_primary_key(&Value::Int64(100)), Some(0), "主键迁移生效");
        assert_eq!(t.lookup_primary_key(&Value::Int64(0)), None, "旧主键清除");
        let r = t.get_row_by_id(0).unwrap().unwrap();
        assert_eq!(r[0], Value::Int64(100));
    }

    #[test]
    fn test_update_pk_migration_conflict_rejected() {
        let mut t = Table::new(make_def(true), CompactStrategy::manual());
        let mut rows = Vec::new();
        for i in 0..5 {
            rows.push(row(i, &format!("r{}", i)));
        }
        t.insert(rows).unwrap();
        // 行 0 迁移主键到 3（已被行 3 占用）→ 报错
        let err = t.update_delta_rows(&[(0, vec![(0, Value::Int64(3))])]).unwrap_err();
        assert!(matches!(err, crate::common::error::EngramDbError::ConstraintViolation(_)));
        // 同批自重复迁移
        let err2 = t.update_delta_rows(&[
            (1, vec![(0, Value::Int64(50))]),
            (2, vec![(0, Value::Int64(50))]),
        ]).unwrap_err();
        assert!(matches!(err2, crate::common::error::EngramDbError::ConstraintViolation(_)));
        // 原主键未被破坏
        assert_eq!(t.lookup_primary_key(&Value::Int64(3)), Some(3));
        assert_eq!(t.row_count(), 5);
    }

    #[test]
    fn test_duplicate_pk_rejected_on_insert() {
        let mut t = Table::new(make_def(true), CompactStrategy::manual());
        t.insert(vec![row(1, "a")]).unwrap();
        let err = t.insert(vec![row(1, "dup")]).unwrap_err();
        assert!(matches!(err, crate::common::error::EngramDbError::ConstraintViolation(_)));
        assert_eq!(t.row_count(), 1, "冲突插入零副作用");
    }

    fn make_ttl_def(pk: bool) -> TableDef {
        let mut def = make_def(pk);
        def.ttl_seconds = Some(60);
        def.ttl_column = Some(2); // created 列
        def.columns.push(ColumnDef::new("created", DataType::Timestamp));
        def
    }

    fn ttl_row(id: i64, v: &str, created_ms: i64) -> Vec<Value> {
        vec![Value::Int64(id), Value::Varchar(v.to_string()), Value::Timestamp(created_ms)]
    }

    fn now_ms_ago(millis: i64) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64
            - millis
    }

    /// P2.4 递进：TTL 过滤必须在 scan_to_chunks / scan_to_rows_direct / scan 三路一致
    /// （旧实现 chunk 路径部分过期时"简化不过滤"导致过期行泄漏）
    ///
    /// 需要超过 direct_threshold（row_group_size/4 = 30720）行触发直接列式路径，
    /// 让数据真正落在列存 RowGroup 上，从而测到 chunk 的 TTL 合流筛选。
    #[test]
    fn test_ttl_partial_expiry_filtered_across_scan_paths() {
        let mut t = Table::new(make_ttl_def(false), CompactStrategy::manual());
        // 直接列式路径（>=30720 行），成批进列存；过期 created 显式给出
        let mut rows = Vec::new();
        for i in 0..32_000 {
            rows.push(
                if i % 3 == 0 {
                    // 过期：120 秒前（ttl=60s）
                    ttl_row(i, &format!("expired{}", i), now_ms_ago(120_000))
                } else {
                    ttl_row(i, &format!("ok{}", i), now_ms_ago(1))
                },
            );
        }
        t.insert(rows).unwrap();

        // 列存路径（批量插入直接进列，不走 Delta）
        assert_eq!(
            t.column_store.row_group_count(),
            1,
            "列存 RowGroup 已建立，扫描走列存",
        );

        // chunks 路径：只有存活行（2/3 = 21334 行）
        let chunks = t.scan_to_chunks(&[0, 1]).unwrap();
        let alive = chunks.iter().map(|c| c.count).sum::<usize>();
        assert_eq!(alive, 21_333, "scan_to_chunks 部分过期必须只输出存活行");

        // rows 直出路径
        let rows_out = t.scan_to_rows_direct(&[0, 1]).unwrap();
        assert_eq!(rows_out.len(), 21_333, "scan_to_rows_direct 部分过期");

        // 旧 scan 路径
        let scan_out = t.scan(&[0, 1]).unwrap();
        assert_eq!(scan_out.len(), 21_333, "scan 部分过期");

        // 单组全过期 → 整组被 TTL RG 级跳过（可跳过索引）
        let mut t2 = Table::new(make_ttl_def(false), CompactStrategy::manual());
        let mut rows2 = Vec::new();
        for i in 0..32_000 {
            rows2.push(ttl_row(i, &format!("old{}", i), now_ms_ago(120_000)));
        }
        t2.insert(rows2).unwrap();
        let chunks2 = t2.scan_to_chunks(&[0, 1]).unwrap();
        assert!(chunks2.iter().all(|c| c.count == 0), "全过期 RG 返回空");
        let out2 = t2.scan_to_rows_direct(&[0, 1]).unwrap();
        assert!(out2.is_empty(), "全过期 RG rows 直出为空");
    }
}
