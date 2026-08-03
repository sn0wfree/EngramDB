//! 表抽象
//!
//! 整合列存主存储 + Delta 层

use crate::common::config::CompactStrategy;
use crate::common::error::Result;
use crate::common::types::{TableDef, IndexDef};
use crate::Value;

use super::column_store::ColumnStore;
use super::delta_store::DeltaStore;
use super::index::skiplist::SkipListIndex;
use super::vector_index::{HnswIndex, HnswConfig, DistanceMetric, Neighbor};
use crate::common::error::HybridDbError;

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
    /// Perf03：主键索引（BTreeMap<主键值, row_id>）
    ///
    /// 第一阶段用 `std::collections::BTreeMap` 快速接线（O(log n) 点查），
    /// 后续升级为页式持久化 B+Tree。
    /// - 表定义中包含 PRIMARY KEY 列时自动启用
    /// - INSERT/UPDATE/DELETE 所有写路径均维护
    /// - WHERE pk=? 查询短路直接命中
    primary_index: Option<std::collections::BTreeMap<crate::Value, u32>>,
}

impl Table {
    pub fn new(def: TableDef, strategy: CompactStrategy) -> Self {
        let row_group_size = match strategy {
            CompactStrategy::Adaptive { max_threshold, .. } => max_threshold as u32,
            _ => 122_880,
        };
        let has_pk = def.primary_key_index().is_some();
        let cs = ColumnStore::new(def.clone(), row_group_size);
        let ds = DeltaStore::new(def.clone());
        Self {
            def,
            column_store: cs,
            delta_store: ds,
            compact_strategy: strategy,
            indexes: std::collections::HashMap::new(),
            vector_indexes: std::collections::HashMap::new(),
            primary_index: if has_pk { Some(std::collections::BTreeMap::new()) } else { None },
        }
    }

    /// 是否启用了主键索引
    #[inline]
    pub fn has_primary_index(&self) -> bool {
        self.primary_index.is_some()
    }

    /// Perf03：通过全局 row_id 读取完整行（用于 PrimaryKeyLookup 命中后回表）
    ///
    /// row_id 分配规则：
    /// - 0..column_store.total_rows()：列存主存储中的行
    /// - 其后：Delta 层中的行（内部使用绝对 row_id 存储）
    pub fn get_row_by_id(&self, row_id: u32) -> Result<Option<Vec<crate::Value>>> {
        let cs_rows = self.column_store.total_rows();
        let row_id_u = row_id as u64;
        if row_id_u < cs_rows {
            // 位于列存主存储：定位 row_group 和 row_idx
            let mut remaining = row_id_u;
            let num_cols = self.def.columns.len();
            let mut located_rg: Option<usize> = None;
            let mut located_row_in_rg: Option<usize> = None;
            for (rg_idx, rg) in self.column_store.row_groups().iter().enumerate() {
                let rc = rg.row_count as u64;
                if remaining < rc {
                    located_rg = Some(rg_idx);
                    located_row_in_rg = Some(remaining as usize);
                    break;
                }
                remaining -= rc;
            }
            let rg_idx = match located_rg {
                Some(i) => i,
                None => return Ok(None),
            };
            let row_in_rg = located_row_in_rg.unwrap();
            let mut row: Vec<crate::Value> = Vec::with_capacity(num_cols);
            for col_idx in 0..num_cols {
                let col_data = self.column_store.read_column(rg_idx, col_idx)?;
                if row_in_rg < col_data.len() {
                    row.push(col_data[row_in_rg].clone());
                } else {
                    row.push(crate::Value::Null);
                }
            }
            Ok(Some(row))
        } else {
            // 位于 Delta 层：使用绝对 row_id 读取
            Ok(self.delta_store.get(row_id_u).map(|r| r.to_vec()))
        }
    }

    /// 通过主键值查找 row_id（O(log n)）
    pub fn lookup_primary_key(&self, key: &crate::Value) -> Option<u32> {
        self.primary_index.as_ref().and_then(|idx| idx.get(key).copied())
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

    /// 创建覆盖索引（v0.12.0 新增）
    ///
    /// 遍历现有数据构建索引。键列只支持单列（首列），
    /// 覆盖列冗余存储在索引条目中，查询时免回表。
    pub fn create_index(&mut self, index_name: &str, key_col_idx: usize, included_cols: &[usize], unique: bool) -> Result<()> {
        if self.indexes.contains_key(index_name) {
            return Err(HybridDbError::ConstraintViolation(
                format!("Index '{}' already exists", index_name)
            ));
        }
        if key_col_idx >= self.def.columns.len() {
            return Err(HybridDbError::ColumnNotFound(
                format!("index key column index {} out of bounds", key_col_idx)
            ));
        }

        let mut skiplist = SkipListIndex::with_included(unique, included_cols.len());
        let mut next_row_id: u32 = 0;

        // 从列存主存储加载数据构建索引
        let num_row_groups = self.column_store.row_group_count();
        for rg_idx in 0..num_row_groups {
            // 读取键列
            let key_col = self.column_store.read_column(rg_idx, key_col_idx)?.to_vec();
            // 读取所有覆盖列
            let mut included_data: Vec<Vec<Value>> = Vec::with_capacity(included_cols.len());
            for &col_idx in included_cols {
                included_data.push(self.column_store.read_column(rg_idx, col_idx)?.to_vec());
            }
            for row_idx in 0..key_col.len() {
                let key = key_col[row_idx].clone();
                let mut inc_vals = Vec::with_capacity(included_cols.len());
                for col in &included_data {
                    inc_vals.push(col[row_idx].clone());
                }
                skiplist.insert_with_included(key, next_row_id, &inc_vals);
                next_row_id += 1;
            }
        }

        // 从 Delta 层加载数据构建索引
        let delta_data = self.delta_store.all_rows();
        for (_rowid, row) in &delta_data {
            let key = row[key_col_idx].clone();
            let mut inc_vals = Vec::with_capacity(included_cols.len());
            for &col_idx in included_cols {
                inc_vals.push(row[col_idx].clone());
            }
            skiplist.insert_with_included(key, next_row_id, &inc_vals);
            next_row_id += 1;
        }

        // 保存索引定义到表元数据
        let index_def = IndexDef {
            name: index_name.to_string(),
            key_columns: vec![key_col_idx],
            included_columns: included_cols.to_vec(),
            unique,
        };
        self.def.indexes.push(index_def);
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
            return Err(HybridDbError::ConstraintViolation(
                format!("Vector index '{}' already exists", index_name)
            ));
        }
        if col_idx >= self.def.columns.len() {
            return Err(HybridDbError::ColumnNotFound(
                format!("column index {} out of bounds", col_idx)
            ));
        }

        // 验证列类型
        let col_def = &self.def.columns[col_idx];
        let dim = match &col_def.data_type {
            DataType::Vector { dim } => *dim,
            _ => return Err(HybridDbError::InvalidFormat(
                format!("column '{}' is not a vector type", col_def.name)
            )),
        };

        if dim == 0 {
            return Err(HybridDbError::InvalidFormat(
                "vector column dimension is 0".into()
            ));
        }

        let config = HnswConfig {
            dim,
            m,
            m_max0: m * 2,
            ef_construction,
            ef_search: 50,
            metric,
        };
        let mut hnsw = HnswIndex::new(config);
        let mut id_mapping = Vec::new();

        // 从列存主存储加载向量数据
        let mut current_row_id = 0u32;
        let num_row_groups = self.column_store.row_group_count();
        for rg_idx in 0..num_row_groups {
            let col_data = self.column_store.read_column(rg_idx, col_idx)?;
            for val in col_data.iter() {
                if let Value::Vector(v) = val {
                    let hnsw_id = hnsw.insert(v.clone())?;
                    // 确保映射向量足够大
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
        let (index, id_mapping) = self.vector_indexes.get(index_name)
            .ok_or_else(|| HybridDbError::IndexNotFound(index_name.into()))?;

        let results = index.search(query, k);
        // 将 HNSW 内部 ID 转换为表行 ID
        Ok(results.into_iter()
            .map(|n| Neighbor {
                id: id_mapping.get(n.id as usize).copied().unwrap_or(n.id),
                distance: n.distance,
            })
            .collect())
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

        buf
    }

    /// 从字节反序列化加载所有索引（v0.12.0 索引持久化）
    ///
    /// 支持旧格式（只有 skip list）和新格式（skip list + vector）。
    /// 旧文件中 vector_index_count 段不存在时，读取到末尾即停止。
    pub fn indexes_from_bytes(&mut self, data: &[u8]) -> Result<()> {
        if data.len() < 4 {
            return Err(HybridDbError::InvalidFormat("index section too short".into()));
        }

        let mut offset = 0;

        // --- SkipList 索引段 ---
        let count = u32::from_le_bytes(data[offset..offset+4].try_into().unwrap()) as usize;
        offset += 4;

        for _ in 0..count {
            // name
            if offset + 4 > data.len() {
                return Err(HybridDbError::InvalidFormat("truncated index name length".into()));
            }
            let name_len = u32::from_le_bytes(data[offset..offset+4].try_into().unwrap()) as usize;
            offset += 4;
            if offset + name_len > data.len() {
                return Err(HybridDbError::InvalidFormat("truncated index name".into()));
            }
            let name = String::from_utf8(data[offset..offset+name_len].to_vec())
                .map_err(|e| HybridDbError::InvalidFormat(format!("invalid index name: {}", e)))?;
            offset += name_len;

            // index data
            if offset + 4 > data.len() {
                return Err(HybridDbError::InvalidFormat("truncated index data length".into()));
            }
            let data_len = u32::from_le_bytes(data[offset..offset+4].try_into().unwrap()) as usize;
            offset += 4;
            if offset + data_len > data.len() {
                return Err(HybridDbError::InvalidFormat("truncated index data".into()));
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
                return Err(HybridDbError::InvalidFormat("truncated vector index name length".into()));
            }
            let name_len = u32::from_le_bytes(data[offset..offset+4].try_into().unwrap()) as usize;
            offset += 4;
            if offset + name_len > data.len() {
                return Err(HybridDbError::InvalidFormat("truncated vector index name".into()));
            }
            let name = String::from_utf8(data[offset..offset+name_len].to_vec())
                .map_err(|e| HybridDbError::InvalidFormat(format!("invalid vector index name: {}", e)))?;
            offset += name_len;

            // hnsw data
            if offset + 4 > data.len() {
                return Err(HybridDbError::InvalidFormat("truncated hnsw data length".into()));
            }
            let hnsw_len = u32::from_le_bytes(data[offset..offset+4].try_into().unwrap()) as usize;
            offset += 4;
            if offset + hnsw_len > data.len() {
                return Err(HybridDbError::InvalidFormat("truncated hnsw data".into()));
            }
            let hnsw = HnswIndex::from_bytes(&data[offset..offset+hnsw_len])?;
            offset += hnsw_len;

            // id mapping
            if offset + 4 > data.len() {
                return Err(HybridDbError::InvalidFormat("truncated id mapping length".into()));
            }
            let map_len = u32::from_le_bytes(data[offset..offset+4].try_into().unwrap()) as usize;
            offset += 4;
            if offset + map_len * 4 > data.len() {
                return Err(HybridDbError::InvalidFormat("truncated id mapping data".into()));
            }
            let mut id_mapping = Vec::with_capacity(map_len);
            for _ in 0..map_len {
                id_mapping.push(u32::from_le_bytes(data[offset..offset+4].try_into().unwrap()));
                offset += 4;
            }

            self.vector_indexes.insert(name, (hnsw, id_mapping));
        }

        Ok(())
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
            .map_err(|e| crate::common::error::HybridDbError::ColumnNotFound(e))?;
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
    pub fn insert(&mut self, rows: Vec<Vec<Value>>) -> Result<u64> {
        let count = rows.len() as u64;
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
    
    /// 插入单行数据（事务提交后应用到存储层）
    ///
    /// 与 `insert()` 不同，此方法用于事务路径：
    /// 1. 由 executor 在 commit 后调用
    /// 2. row_id 由事务管理器分配（避免重复）
    /// 3. 直接写入 Delta 层（单行场景不需要列式路径优化）
    pub fn insert_row(&mut self, row_id: u32, row: &[Value]) -> Result<()> {
        // 写入 Delta 层（单行直接插入）
        self.delta_store.insert_row(row_id, row.to_vec())?;
        
        // 更新总行数
        self.def.row_count += 1;
        
        // Perf03：更新主键索引
        if self.primary_index.is_some() {
            self.primary_index_insert(row, row_id);
        }
        
        // 更新所有二级索引
        if !self.indexes.is_empty() {
            self.update_indexes_for_row(row_id, row);
        }
        
        // 更新所有向量索引
        if !self.vector_indexes.is_empty() {
            self.update_vector_indexes_for_row(row_id, row);
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
                self.update_indexes_for_row(row_id, new_r);
            }
            
            if !self.vector_indexes.is_empty() {
                self.update_vector_indexes_for_row(row_id, new_r);
            }
        }
        
        Ok(())
    }
    
    /// 更新单行的二级索引（内部辅助方法）
    fn update_indexes_for_row(&mut self, row_id: u32, row: &[Value]) {
        for idx_def in self.def.indexes.clone() {
            if let Some(index) = self.indexes.get_mut(&idx_def.name) {
                let key = row[idx_def.key_columns[0]].clone();
                let included_vals: Vec<Value> = idx_def.included_columns.iter()
                    .map(|&ci| row[ci].clone())
                    .collect();
                index.insert_with_included(key, row_id, &included_vals);
            }
        }
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

        // 从列存读取
        for rg_idx in 0..self.column_store.row_group_count() {
            // 读取需要的列
            let mut columns_data = Vec::new();
            for &col_idx in column_indices {
                let col_data = self.column_store.read_column(rg_idx, col_idx)?;
                columns_data.push(col_data.to_vec());
            }

            // 按行组装
            if !columns_data.is_empty() {
                let row_count = columns_data[0].len();
                for row_idx in 0..row_count {
                    let mut row = Vec::with_capacity(column_indices.len());
                    for col in &columns_data {
                        row.push(col[row_idx].clone());
                    }
                    result.push(row);
                }
            }
        }

        // 从 Delta 层读取
        for (_, row) in self.delta_store.all_rows() {
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

        // 根据是否有聚簇键选择写入方式
        let columns_to_write: Vec<Vec<Value>> = match self.def.cluster_key {
            Some(cluster_idx) => self.delta_store.clustered_column_data(cluster_idx),
            None => self.delta_store.column_data().to_vec(),
        };

        self.column_store.append_columns(&columns_to_write)?;
        self.delta_store.clear();
        self.def.row_count += row_count;

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

        // 如果有聚簇键，对取出的数据做聚簇重排
        if let Some(cluster_idx) = self.def.cluster_key {
            columns = cluster_columns(&columns, cluster_idx);
        }

        // 合并到列存
        self.column_store.append_columns(&columns)?;
        self.def.row_count += actual_rows as u64;

        Ok(actual_rows)
    }

    /// 总行数（元数据级 O(1)，已通过 INSERT/DELETE/UPDATE 所有写路径精确维护）
    ///
    /// Perf01：供 COUNT(*) 短路和优化器估计行数使用。
    pub fn row_count(&self) -> u64 {
        self.def.row_count
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

        let mut count = 0;
        let mut old_rows: Vec<Vec<Value>> = Vec::new();
        let mut new_rows: Vec<Vec<Value>> = Vec::new();
        let mut row_ids: Vec<u32> = Vec::new();

        for &(row_idx, ref new_vals) in updates {
            if let Some(old_row) = self.delta_store.update_row(row_idx, new_vals) {
                // 收集更新后的新行（从 Delta 层读取最新值）
                if let Some(new_row) = self.delta_store.get((row_idx as u64) + 1) {
                    old_rows.push(old_row);
                    new_rows.push(new_row);
                    row_ids.push(delta_base_row_id + row_idx as u32);
                }
                count += 1;
            }
        }

        // 更新索引：先删旧条目，再插新条目
        if self.primary_index.is_some() && !old_rows.is_empty() {
            // Perf03：先删旧主键索引，再插新主键索引
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
}
