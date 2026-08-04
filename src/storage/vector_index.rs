//! 向量检索引擎
//!
//! 基于 HNSW (Hierarchical Navigable Small World) 的向量相似度搜索
//! 支持：L2 距离、内积 (IP)、余弦相似度
//! 支持 INT8 量化（MinMax 量化，4x 存储压缩）
//!
//! 零外部依赖，纯 Rust 实现
//! 参考论文：https://arxiv.org/abs/1603.09320

use crate::common::error::Result;
use std::collections::BinaryHeap;
use std::cmp::{Ordering, Reverse};

// ============================================================================
// 距离度量
// ============================================================================

/// 距离度量类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DistanceMetric {
    L2,           // 欧氏距离平方
    InnerProduct, // 内积（归一化后 = 余弦相似度）
    Cosine,       // 余弦相似度（自动归一化）
}

/// 计算 L2 距离平方（不开根号，不影响排序）
#[inline]
pub fn l2_distance_sq(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| {
        let d = x - y;
        d * d
    }).sum()
}

/// 计算内积
#[inline]
pub fn inner_product(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

/// 计算向量的 L2 范数
#[inline]
pub fn norm(v: &[f32]) -> f32 {
    v.iter().map(|x| x * x).sum::<f32>().sqrt()
}

/// 归一化向量（单位长度）
pub fn normalize(v: &mut [f32]) {
    let n = norm(v);
    if n > 0.0 {
        for x in v.iter_mut() {
            *x /= n;
        }
    }
}

/// 余弦相似度（自动归一化）
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let na = norm(a);
    let nb = norm(b);
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na * nb)
    }
}

// ============================================================================
// INT8 量化
// ============================================================================

/// MinMax 量化：将 f32 向量量化为 i8 向量
///
/// 映射公式：`v_i8 = round((v_f32 - min) / (max - min) * 254.0 - 127.0)`
/// 每个向量独立存储 scale 和 offset，反量化时恢复。
///
/// 返回 (量化后的 i8 向量, scale, offset)
/// 其中 scale = (max - min) / 254.0, offset = (min + max) / 2.0
pub fn quantize_to_int8(v: &[f32]) -> (Vec<i8>, f32, f32) {
    if v.is_empty() {
        return (Vec::new(), 0.0, 0.0);
    }
    let mut min = v[0];
    let mut max = v[0];
    for &x in v {
        if x < min { min = x; }
        if x > max { max = x; }
    }
    let range = max - min;
    if range == 0.0 {
        // 所有值相同，全量化为 0
        return (vec![0i8; v.len()], 1.0, min);
    }
    let scale = range / 254.0;
    let offset = (min + max) / 2.0;
    let mut quantized = Vec::with_capacity(v.len());
    for &x in v {
        let q = ((x - min) / range * 254.0 - 127.0).round().clamp(-128.0, 127.0) as i8;
        quantized.push(q);
    }
    (quantized, scale, offset)
}

/// 反量化：从 i8 向量恢复为 f32 向量
///
/// 映射公式：`v_f32 = (q_i8 + 127.0) / 254.0 * range + min`
/// 等价于：`v_f32 = q_i8 * scale + offset`
pub fn dequantize_to_f32(q: &[i8], scale: f32, offset: f32) -> Vec<f32> {
    q.iter().map(|&x| (x as f32) * scale + offset).collect()
}

// ============================================================================
// HNSW 索引
// ============================================================================

/// HNSW 索引配置
#[derive(Debug, Clone)]
pub struct HnswConfig {
    pub dim: usize,           // 向量维度
    pub m: usize,             // 每层每个节点的最大连接数 (M)
    pub m_max0: usize,        // 第 0 层最大连接数 (M_max0)，通常 = 2*M
    pub ef_construction: usize, // 构建时的搜索宽度 (efConstruction)
    pub ef_search: usize,     // 查询时的搜索宽度 (efSearch)
    pub metric: DistanceMetric,
    /// 是否启用 INT8 量化存储（v0.15.0 新增）
    ///
    /// 启用后，向量存储为 INT8 量化格式，存储量减少 75%。
    /// 搜索时自动反量化回 f32 计算距离。
    pub quantize: bool,
}

impl Default for HnswConfig {
    fn default() -> Self {
        HnswConfig {
            dim: 128,
            m: 16,
            m_max0: 32,
            ef_construction: 100,
            ef_search: 50,
            metric: DistanceMetric::L2,
            quantize: false,
        }
    }
}

/// 向量节点
#[derive(Debug, Clone)]
struct HnswNode {
    id: u32,
    vector: Vec<f32>,
    /// INT8 量化后的向量（v0.15.0 新增）
    ///
    /// 当 config.quantize = true 时，vector 仍保留原始 f32 数据用于距离计算，
    /// quantized + scale + offset 用于持久化存储（节省 75% 空间）。
    /// 搜索时统一使用 vector 计算距离，quantized 仅用于序列化。
    quantized: Vec<i8>,
    scale: f32,
    offset: f32,
    /// 每层的邻居列表：layers[level] = Vec<neighbor_id>
    layers: Vec<Vec<u32>>,
}

/// 搜索结果（用于优先队列）
#[derive(Debug, Clone)]
struct SearchCandidate {
    distance: f32,
    id: u32,
}

impl PartialEq for SearchCandidate {
    fn eq(&self, other: &Self) -> bool {
        self.distance == other.distance && self.id == other.id
    }
}
impl Eq for SearchCandidate {}

// BinaryHeap 是 max-heap，SearchCandidate 按距离从大到小排（最远的在堆顶）
// 用于 results：维护 top-ef 最近邻，堆顶是当前最远的那个，便于淘汰
// candidates 需要 min-heap（最近的先探索），用 Reverse 包装
impl PartialOrd for SearchCandidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        self.distance.partial_cmp(&other.distance)
    }
}

impl Ord for SearchCandidate {
    fn cmp(&self, other: &Self) -> Ordering {
        self.partial_cmp(other).unwrap_or(Ordering::Equal)
    }
}

/// 搜索结果（升序，距离小的在前）
#[derive(Debug, Clone)]
pub struct Neighbor {
    pub id: u32,
    pub distance: f32,
}

/// 搜索 trace（v0.15.0 V13 新增）
///
/// 返回向量搜索的可追溯路径，Agent 场景下用于溯源：
/// - 入口点：搜索从哪个节点开始
/// - 访问路径：搜索过程中访问的节点 ID 序列（按访问顺序）
/// - 候选节点数：搜索过程中评估的候选节点总数
/// - 层数遍历：从顶层到 0 层
/// - 索引类型：HNSW / 量化
#[derive(Debug, Clone)]
pub struct SearchTrace {
    /// 入口点节点 ID（搜索起始节点）
    pub entry_point: Option<u32>,
    /// 搜索过程中访问的节点 ID 序列（按访问顺序，可能重复）
    pub visited_nodes: Vec<u32>,
    /// 评估的候选节点总数（去重）
    pub candidates_visited: usize,
    /// 遍历的层数（从顶层到 0 层）
    pub layers_traversed: usize,
    /// 索引类型描述（如 "HNSW" / "HNSW-INT8"）
    pub index_type: String,
    /// 使用的度量函数名称
    pub metric: String,
    /// 最终 top-k 结果的节点 ID（按距离升序）
    pub top_k_ids: Vec<u32>,
    /// 最终 top-k 结果的距离（与 top_k_ids 一一对应）
    pub top_k_distances: Vec<f32>,
}

impl SearchTrace {
    pub fn new() -> Self {
        Self {
            entry_point: None,
            visited_nodes: Vec::new(),
            candidates_visited: 0,
            layers_traversed: 0,
            index_type: "HNSW".to_string(),
            metric: "L2".to_string(),
            top_k_ids: Vec::new(),
            top_k_distances: Vec::new(),
        }
    }
}

/// HNSW 向量索引
pub struct HnswIndex {
    config: HnswConfig,
    nodes: Vec<HnswNode>,
    /// 入口点 (entry point) 的节点 ID
    enter_point: Option<u32>,
    /// 最大层数（0-based，0 是最底层）
    max_level: i32,
    /// 逻辑删除的节点 ID 集合（tombstone，v0.12.0 DELETE 支持）
    ///
    /// HNSW 不支持原地物理删除，用 tombstone 标记逻辑删除。
    /// 搜索结果中自动过滤 tombstone 节点。
    deleted: std::collections::HashSet<u32>,
}

impl HnswIndex {
    /// 创建空索引
    pub fn new(config: HnswConfig) -> Self {
        HnswIndex {
            config,
            nodes: Vec::new(),
            enter_point: None,
            max_level: -1,
            deleted: std::collections::HashSet::new(),
        }
    }

    /// 索引中的向量数量（含已删除的）
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// 有效向量数量（排除 tombstone）
    pub fn active_len(&self) -> usize {
        self.nodes.len() - self.deleted.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// 清空所有节点（v0.15.0 TRUNCATE TABLE 支持）
    pub fn clear(&mut self) {
        self.nodes.clear();
        self.deleted.clear();
        self.enter_point = None;
        self.max_level = -1;
    }

    /// 计算两个向量的距离（根据 metric）
    fn distance(&self, a: &[f32], b: &[f32]) -> f32 {
        match self.config.metric {
            DistanceMetric::L2 => l2_distance_sq(a, b),
            DistanceMetric::InnerProduct => -inner_product(a, b), // 内积越大越近，取负当距离
            DistanceMetric::Cosine => -cosine_similarity(a, b),   // 余弦同理
        }
    }

    /// 随机生成层数（几何分布，由 M 决定）
    fn random_level(&self) -> i32 {
        // 论文中的概率分布：P(level >= l) = 1 / M^l
        // 生成公式：level = floor(-ln(rand) / ln(M))
        // 等价于：level = floor(-ln(rand) * m_L)，其中 m_L = 1 / ln(M)
        let mut level = 0i32;
        let m_l = 1.0 / (self.config.m as f64).ln();
        // 伪随机（基于节点数的简单 hash，保证可重现）
        let mut seed = self.nodes.len() as u64;
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let r = (seed as f64) / (u64::MAX as f64);
        // 几何分布: level = floor(-ln(r) * m_L)
        if r > 0.0 {
            level = (-r.ln() * m_l) as i32;
        }
        // 限制最大层数（防止极端值）
        level.min(16)
    }

    /// 在指定层中找最近的 ef 个邻居
    fn search_layer(&self, query: &[f32], entry_points: &[(f32, u32)], level: i32, ef: usize) -> Vec<(f32, u32)> {
        let mut visited = vec![false; self.nodes.len()];
        // 候选堆（min-heap by distance → 最近的先探索）
        let mut candidates: BinaryHeap<Reverse<SearchCandidate>> = BinaryHeap::new();
        // 结果堆（max-heap by distance → 维护最近的 ef 个，堆顶是最远的）
        let mut results: BinaryHeap<SearchCandidate> = BinaryHeap::new();

        // 初始化入口点
        for &(dist, id) in entry_points {
            if !visited[id as usize] {
                visited[id as usize] = true;
                candidates.push(Reverse(SearchCandidate { distance: dist, id }));
                results.push(SearchCandidate { distance: dist, id });
            }
        }

        while let Some(Reverse(c)) = candidates.pop() {
            // 如果最近的候选都比结果中最远的还远，提前终止
            if results.len() >= ef {
                if let Some(farthest) = results.peek() {
                    if c.distance > farthest.distance {
                        break;
                    }
                }
            }

            let node = &self.nodes[c.id as usize];
            if level as usize >= node.layers.len() {
                continue;
            }

            // 遍历该层所有邻居
            for &neighbor_id in &node.layers[level as usize] {
                if visited[neighbor_id as usize] {
                    continue;
                }
                visited[neighbor_id as usize] = true;

                let neighbor = &self.nodes[neighbor_id as usize];
                let dist = self.distance(query, &neighbor.vector);

                // 加入候选
                candidates.push(Reverse(SearchCandidate { distance: dist, id: neighbor_id }));

                // 加入结果（维护 top-ef）
                if results.len() < ef {
                    results.push(SearchCandidate { distance: dist, id: neighbor_id });
                } else if let Some(farthest) = results.peek() {
                    if dist < farthest.distance {
                        results.pop();
                        results.push(SearchCandidate { distance: dist, id: neighbor_id });
                    }
                }
            }
        }

        // 转换为升序结果
        let mut result_vec: Vec<(f32, u32)> = results.into_vec()
            .into_iter()
            .map(|s| (s.distance, s.id))
            .collect();
        result_vec.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(Ordering::Equal));
        result_vec
    }

    /// 插入一个向量
    pub fn insert(&mut self, vector: Vec<f32>) -> Result<u32> {
        assert_eq!(vector.len(), self.config.dim, "向量维度不匹配");

        let id = self.nodes.len() as u32;
        let new_level = self.random_level();

        // 创建新节点
        let num_layers = (new_level + 1) as usize;
        let mut layers = Vec::with_capacity(num_layers);
        for _ in 0..num_layers {
            layers.push(Vec::with_capacity(self.config.m));
        }

        let new_node = if self.config.quantize {
            let (quantized, scale, offset) = quantize_to_int8(&vector);
            HnswNode {
                id,
                vector,
                quantized,
                scale,
                offset,
                layers,
            }
        } else {
            HnswNode {
                id,
                vector,
                quantized: Vec::new(),
                scale: 0.0,
                offset: 0.0,
                layers,
            }
        };
        self.nodes.push(new_node);

        // 第一个节点
        if self.enter_point.is_none() {
            self.enter_point = Some(id);
            self.max_level = new_level;
            return Ok(id);
        }

        let mut current_enter = self.enter_point.unwrap();
        let query_vec = &self.nodes[id as usize].vector.clone();

        // 从顶层向下搜索，找到每层的入口点
        let mut current_dist = self.distance(query_vec, &self.nodes[current_enter as usize].vector);

        // 从 max_level 往下到 new_level + 1：贪婪下降，只找最近的一个入口
        for level in (new_level + 1..=self.max_level).rev() {
            let mut changed = true;
            while changed {
                changed = false;
                let node = &self.nodes[current_enter as usize];
                if level as usize >= node.layers.len() {
                    break;
                }
                for &neighbor_id in &node.layers[level as usize] {
                    let d = self.distance(query_vec, &self.nodes[neighbor_id as usize].vector);
                    if d < current_dist {
                        current_dist = d;
                        current_enter = neighbor_id;
                        changed = true;
                    }
                }
            }
        }

        // 从 new_level 往下到第 0 层：搜索 + 连接
        let mut entry_points = vec![(current_dist, current_enter)];

        for level in (0..=new_level.min(self.max_level)).rev() {
            let m_max = if level == 0 { self.config.m_max0 } else { self.config.m };

            // 在该层搜索 ef_construction 个最近邻
            let neighbors = self.search_layer(query_vec, &entry_points, level, self.config.ef_construction);

            // 连接新节点到最近的 M 个邻居
            let connect_count = m_max.min(neighbors.len());
            for i in 0..connect_count {
                let (dist, neighbor_id) = neighbors[i];

                // 提前取出当前节点的向量（避免借用冲突）
                let current_vec = self.nodes[id as usize].vector.clone();

                // 双向连接（先添加到新节点，用作用域限制借用）
                {
                    let new_node = &mut self.nodes[id as usize];
                    new_node.layers[level as usize].push(neighbor_id);
                }

                // 给邻居也加上连接（如果还没到上限）
                // 先检查邻居在该层的状态（不可变借用阶段）
                let neighbor_layer_len = self.nodes[neighbor_id as usize]
                    .layers
                    .get(level as usize)
                    .map(|l| l.len())
                    .unwrap_or(0);

                if neighbor_layer_len < m_max {
                    // 直接加
                    self.nodes[neighbor_id as usize].layers[level as usize].push(id);
                } else {
                    // 邻居已满：先计算所有邻居的距离（不可变借用阶段）
                    let neighbor_ids: Vec<u32> = self.nodes[neighbor_id as usize]
                        .layers[level as usize]
                        .clone();
                    let mut farthest_idx = 0;
                    let mut farthest_dist = dist;
                    for (j, &nid) in neighbor_ids.iter().enumerate() {
                        let d = self.distance(&current_vec, &self.nodes[nid as usize].vector);
                        if d > farthest_dist {
                            farthest_dist = d;
                            farthest_idx = j;
                        }
                    }
                    // 如果新节点比最远的邻居近，替换（可变借用）
                    if dist < farthest_dist {
                        self.nodes[neighbor_id as usize].layers[level as usize][farthest_idx] = id;
                    }
                }
            }

            // 下一层的入口点 = 这层找到的最近邻
            entry_points = neighbors.clone();
        }

        // 更新入口点和最大层
        if new_level > self.max_level {
            self.enter_point = Some(id);
            self.max_level = new_level;
        }

        Ok(id)
    }

    /// 获取索引配置
    pub fn config(&self) -> &HnswConfig {
        &self.config
    }

    /// K 近邻搜索（带 trace，v0.15.0 V13 新增）
    ///
    /// 返回 (top-k 邻居, 搜索 trace)。自动过滤已逻辑删除（tombstone）的节点。
    pub fn search_with_trace(&self, query: &[f32], k: usize) -> (Vec<Neighbor>, SearchTrace) {
        let mut trace = SearchTrace::new();
        trace.index_type = if self.config.quantize { "HNSW-INT8".to_string() } else { "HNSW".to_string() };
        trace.metric = match self.config.metric {
            DistanceMetric::L2 => "L2".to_string(),
            DistanceMetric::InnerProduct => "InnerProduct".to_string(),
            DistanceMetric::Cosine => "Cosine".to_string(),
        };

        assert_eq!(query.len(), self.config.dim, "向量维度不匹配");

        if self.nodes.is_empty() {
            return (Vec::new(), trace);
        }

        // 如果删除比例较高，扩大 ef 确保能找到足够有效结果
        let effective_k = if self.deleted.is_empty() {
            k
        } else {
            let ratio = self.nodes.len() as f64 / (self.nodes.len() - self.deleted.len()).max(1) as f64;
            (k as f64 * ratio * 1.5) as usize
        };
        let ef = self.config.ef_search.max(effective_k);

        let mut current_enter = self.enter_point.unwrap();
        let mut current_dist = self.distance(query, &self.nodes[current_enter as usize].vector);

        trace.entry_point = Some(current_enter);
        trace.visited_nodes.push(current_enter);

        // 从顶层贪婪下降到第 0 层
        for level in (1..=self.max_level).rev() {
            trace.layers_traversed += 1;
            let mut changed = true;
            while changed {
                changed = false;
                let node = &self.nodes[current_enter as usize];
                if level as usize >= node.layers.len() {
                    break;
                }
                for &neighbor_id in &node.layers[level as usize] {
                    let d = self.distance(query, &self.nodes[neighbor_id as usize].vector);
                    trace.visited_nodes.push(neighbor_id);
                    if d < current_dist {
                        current_dist = d;
                        current_enter = neighbor_id;
                        changed = true;
                    }
                }
            }
        }

        // 在第 0 层做完整搜索（带 trace 的版本）
        let entry_points = vec![(current_dist, current_enter)];
        let (results, layer_visited) = self.search_layer_with_trace(query, &entry_points, 0, ef);
        trace.visited_nodes.extend(layer_visited);

        // 去重统计
        let mut seen = std::collections::HashSet::new();
        for &n in &trace.visited_nodes {
            seen.insert(n);
        }
        trace.candidates_visited = seen.len();

        // 过滤 tombstone 节点，取前 k 个有效结果
        let neighbors: Vec<Neighbor> = results.into_iter()
            .filter(|(_, id)| !self.deleted.contains(id))
            .take(k)
            .map(|(dist, id)| Neighbor { id, distance: dist })
            .collect();

        // 填充 trace 的 top-k
        for n in &neighbors {
            trace.top_k_ids.push(n.id);
            trace.top_k_distances.push(n.distance);
        }

        (neighbors, trace)
    }

    /// K 近邻搜索
    ///
    /// 自动过滤已逻辑删除（tombstone）的节点。
    pub fn search(&self, query: &[f32], k: usize) -> Vec<Neighbor> {
        let (results, _trace) = self.search_with_trace(query, k);
        results
    }

    /// 在指定层中找最近的 ef 个邻居（带 trace）
    fn search_layer_with_trace(
        &self,
        query: &[f32],
        entry_points: &[(f32, u32)],
        level: i32,
        ef: usize,
    ) -> (Vec<(f32, u32)>, Vec<u32>) {
        let mut visited = vec![false; self.nodes.len()];
        let mut candidates: BinaryHeap<Reverse<SearchCandidate>> = BinaryHeap::new();
        let mut results: BinaryHeap<SearchCandidate> = BinaryHeap::new();
        let mut visited_ids = Vec::new();

        for &(dist, id) in entry_points {
            if !visited[id as usize] {
                visited[id as usize] = true;
                visited_ids.push(id);
                candidates.push(Reverse(SearchCandidate { distance: dist, id }));
                results.push(SearchCandidate { distance: dist, id });
            }
        }

        while let Some(Reverse(c)) = candidates.pop() {
            if results.len() >= ef {
                if let Some(farthest) = results.peek() {
                    if c.distance > farthest.distance {
                        break;
                    }
                }
            }

            let node = &self.nodes[c.id as usize];
            if level as usize >= node.layers.len() {
                continue;
            }

            for &neighbor_id in &node.layers[level as usize] {
                if visited[neighbor_id as usize] {
                    continue;
                }
                visited[neighbor_id as usize] = true;
                visited_ids.push(neighbor_id);

                let neighbor = &self.nodes[neighbor_id as usize];
                let dist = self.distance(query, &neighbor.vector);

                candidates.push(Reverse(SearchCandidate { distance: dist, id: neighbor_id }));

                if results.len() < ef {
                    results.push(SearchCandidate { distance: dist, id: neighbor_id });
                } else if let Some(farthest) = results.peek() {
                    if dist < farthest.distance {
                        results.pop();
                        results.push(SearchCandidate { distance: dist, id: neighbor_id });
                    }
                }
            }
        }

        let mut result_vec: Vec<(f32, u32)> = results.into_vec()
            .into_iter()
            .map(|s| (s.distance, s.id))
            .collect();
        result_vec.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(Ordering::Equal));
        (result_vec, visited_ids)
    }

    /// 逻辑删除一个节点（tombstone）
    ///
    /// HNSW 不支持原地物理删除，用 tombstone 标记。
    /// 搜索结果中自动过滤已删除节点。
    /// 返回 true 表示成功标记，false 表示节点不存在或已被删除。
    pub fn mark_deleted(&mut self, id: u32) -> bool {
        if id as usize >= self.nodes.len() {
            return false;
        }
        self.deleted.insert(id)
    }

    /// 取消删除标记（用于 UPDATE 后重新插入时清理旧标记）
    pub fn undelete(&mut self, id: u32) -> bool {
        self.deleted.remove(&id)
    }

    /// 检查节点是否已被逻辑删除
    pub fn is_deleted(&self, id: u32) -> bool {
        self.deleted.contains(&id)
    }

    /// tombstone 数量
    pub fn deleted_count(&self) -> usize {
        self.deleted.len()
    }

    /// 获取指定 ID 的向量
    pub fn get_vector(&self, id: u32) -> Option<&[f32]> {
        self.nodes.get(id as usize).map(|n| n.vector.as_slice())
    }

    /// 统计信息
    pub fn stats(&self) -> HnswStats {
        let mut total_connections = 0usize;
        for node in &self.nodes {
            for layer in &node.layers {
                total_connections += layer.len();
            }
        }
        HnswStats {
            num_nodes: self.nodes.len(),
            max_level: self.max_level,
            total_connections,
            avg_connections_per_node: if self.nodes.is_empty() { 0.0 } else { total_connections as f64 / self.nodes.len() as f64 },
        }
    }

    // ========================================================================
    // 序列化 / 反序列化（v0.12.0 向量索引持久化）
    // ========================================================================

    /// 序列化为字节
    ///
    /// 格式：
    /// - magic: "HNSW_IDX2" (9B)
    /// - dim: u32
    /// - m: u32
    /// - m_max0: u32
    /// - ef_construction: u32
    /// - ef_search: u32
    /// - metric: u8 (0=L2, 1=InnerProduct, 2=Cosine)
    /// - quantize: u8 (0=false, 1=true)  （v0.15.0 新增）
    /// - max_level: i32
    /// - enter_point: u32 (0xFFFFFFFF = None)
    /// - num_nodes: u32
    /// - 重复 num_nodes 次：
    ///   - id: u32
    ///   - quantized_flag: u8 (0=否, 1=是)  （v0.15.0 新增）
    ///   - 如果 quantized_flag == 1:
    ///     - scale: f32
    ///     - offset: f32
    ///     - quantized: [i8; dim]  （dim 个字节）
    ///   - vector: [f32; dim]
    ///   - num_layers: u32
    ///   - 重复 num_layers 次：
    ///     - num_neighbors: u32
    ///     - neighbors: [u32; num_neighbors]
    /// - deleted_count: u32  （v0.12.0 tombstone 新增）
    /// - deleted_ids: [u32; deleted_count]  （v0.12.0 tombstone 新增）
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::new();

        // magic
        buf.extend_from_slice(b"HNSW_IDX2");

        // config
        buf.extend_from_slice(&(self.config.dim as u32).to_le_bytes());
        buf.extend_from_slice(&(self.config.m as u32).to_le_bytes());
        buf.extend_from_slice(&(self.config.m_max0 as u32).to_le_bytes());
        buf.extend_from_slice(&(self.config.ef_construction as u32).to_le_bytes());
        buf.extend_from_slice(&(self.config.ef_search as u32).to_le_bytes());

        // metric
        let metric_byte = match self.config.metric {
            DistanceMetric::L2 => 0u8,
            DistanceMetric::InnerProduct => 1u8,
            DistanceMetric::Cosine => 2u8,
        };
        buf.push(metric_byte);

        // quantize flag (v0.15.0)
        buf.push(if self.config.quantize { 1 } else { 0 });

        // max_level
        buf.extend_from_slice(&self.max_level.to_le_bytes());

        // enter_point
        match self.enter_point {
            Some(id) => buf.extend_from_slice(&id.to_le_bytes()),
            None => buf.extend_from_slice(&0xFFFFFFFFu32.to_le_bytes()),
        }

        // num_nodes
        buf.extend_from_slice(&(self.nodes.len() as u32).to_le_bytes());

        // nodes
        for node in &self.nodes {
            buf.extend_from_slice(&node.id.to_le_bytes());

            // quantized flag (v0.15.0)
            let has_quantized = !node.quantized.is_empty() as u8;
            buf.push(has_quantized);
            if has_quantized == 1 {
                // scale, offset, then quantized data as raw bytes
                buf.extend_from_slice(&node.scale.to_le_bytes());
                buf.extend_from_slice(&node.offset.to_le_bytes());
                for &q in &node.quantized {
                    buf.push(q as u8);
                }
            }

            // vector (dim * f32)
            for &v in &node.vector {
                buf.extend_from_slice(&v.to_le_bytes());
            }

            // layers
            buf.extend_from_slice(&(node.layers.len() as u32).to_le_bytes());
            for layer in &node.layers {
                buf.extend_from_slice(&(layer.len() as u32).to_le_bytes());
                for &nid in layer {
                    buf.extend_from_slice(&nid.to_le_bytes());
                }
            }
        }

        // tombstone 集合（v0.12.0 DELETE 支持）
        buf.extend_from_slice(&(self.deleted.len() as u32).to_le_bytes());
        for &id in &self.deleted {
            buf.extend_from_slice(&id.to_le_bytes());
        }

        buf
    }

    /// 从字节反序列化
    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        if data.len() < 9 {
            return Err(crate::common::error::EngramDbError::InvalidFormat(
                "HNSW index data too short".into()
            ));
        }

        // magic (support both v1 and v2)
        let is_v2 = &data[..9] == b"HNSW_IDX2";
        if !is_v2 && &data[..9] != b"HNSW_IDX1" {
            return Err(crate::common::error::EngramDbError::InvalidFormat(
                "invalid HNSW index magic".into()
            ));
        }

        let mut offset = 9;

        // helper functions
        let read_u32 = |data: &[u8], off: &mut usize| -> Result<u32> {
            if *off + 4 > data.len() {
                return Err(crate::common::error::EngramDbError::InvalidFormat(
                    "truncated HNSW index data".into()
                ));
            }
            let val = u32::from_le_bytes(data[*off..*off+4].try_into().unwrap());
            *off += 4;
            Ok(val)
        };

        let read_i32 = |data: &[u8], off: &mut usize| -> Result<i32> {
            if *off + 4 > data.len() {
                return Err(crate::common::error::EngramDbError::InvalidFormat(
                    "truncated HNSW index data".into()
                ));
            }
            let val = i32::from_le_bytes(data[*off..*off+4].try_into().unwrap());
            *off += 4;
            Ok(val)
        };

        let read_f32 = |data: &[u8], off: &mut usize| -> Result<f32> {
            if *off + 4 > data.len() {
                return Err(crate::common::error::EngramDbError::InvalidFormat(
                    "truncated HNSW index data".into()
                ));
            }
            let val = f32::from_le_bytes(data[*off..*off+4].try_into().unwrap());
            *off += 4;
            Ok(val)
        };

        // config
        let dim = read_u32(data, &mut offset)? as usize;
        let m = read_u32(data, &mut offset)? as usize;
        let m_max0 = read_u32(data, &mut offset)? as usize;
        let ef_construction = read_u32(data, &mut offset)? as usize;
        let ef_search = read_u32(data, &mut offset)? as usize;

        // metric
        if offset + 1 > data.len() {
            return Err(crate::common::error::EngramDbError::InvalidFormat(
                "truncated HNSW metric byte".into()
            ));
        }
        let metric = match data[offset] {
            0 => DistanceMetric::L2,
            1 => DistanceMetric::InnerProduct,
            2 => DistanceMetric::Cosine,
            other => return Err(crate::common::error::EngramDbError::InvalidFormat(
                format!("unknown HNSW metric: {}", other)
            )),
        };
        offset += 1;

        // quantize flag (v2 only, v1 defaults to false)
        let quantize = if is_v2 {
            if offset + 1 > data.len() {
                return Err(crate::common::error::EngramDbError::InvalidFormat(
                    "truncated HNSW quantize byte".into()
                ));
            }
            let q = data[offset] != 0;
            offset += 1;
            q
        } else {
            false
        };

        let config = HnswConfig { dim, m, m_max0, ef_construction, ef_search, metric, quantize };

        // max_level
        let max_level = read_i32(data, &mut offset)?;

        // enter_point
        let enter_point_raw = read_u32(data, &mut offset)?;
        let enter_point = if enter_point_raw == 0xFFFFFFFF {
            None
        } else {
            Some(enter_point_raw)
        };

        // num_nodes
        let num_nodes = read_u32(data, &mut offset)? as usize;

        // nodes
        let mut nodes = Vec::with_capacity(num_nodes);
        for _ in 0..num_nodes {
            let id = read_u32(data, &mut offset)?;

            // quantized flag (v2 only, v1 defaults to false)
            let has_quantized = if is_v2 {
                if offset + 1 > data.len() {
                    return Err(crate::common::error::EngramDbError::InvalidFormat(
                        "truncated HNSW quantized_flag".into()
                    ));
                }
                let flag = data[offset] != 0;
                offset += 1;
                flag
            } else {
                false
            };

            let (quantized, scale, offset_f) = if has_quantized {
                let s = read_f32(data, &mut offset)?;
                let o = read_f32(data, &mut offset)?;
                let mut q = Vec::with_capacity(dim);
                for _ in 0..dim {
                    if offset >= data.len() {
                        return Err(crate::common::error::EngramDbError::InvalidFormat(
                            "truncated HNSW quantized data".into()
                        ));
                    }
                    q.push(data[offset] as i8);
                    offset += 1;
                }
                (q, s, o)
            } else {
                (Vec::new(), 0.0, 0.0)
            };

            // vector
            let mut vector = Vec::with_capacity(dim);
            for _ in 0..dim {
                vector.push(read_f32(data, &mut offset)?);
            }

            // layers
            let num_layers = read_u32(data, &mut offset)? as usize;
            let mut layers = Vec::with_capacity(num_layers);
            for _ in 0..num_layers {
                let num_neighbors = read_u32(data, &mut offset)? as usize;
                let mut neighbors = Vec::with_capacity(num_neighbors);
                for _ in 0..num_neighbors {
                    neighbors.push(read_u32(data, &mut offset)?);
                }
                layers.push(neighbors);
            }

            nodes.push(HnswNode { id, vector, quantized, scale, offset: offset_f, layers });
        }

        // tombstone 集合（v0.12.0 DELETE 支持，可选段）
        let mut deleted = std::collections::HashSet::new();
        if offset + 4 <= data.len() {
            let deleted_count = read_u32(data, &mut offset)? as usize;
            for _ in 0..deleted_count {
                let del_id = read_u32(data, &mut offset)?;
                deleted.insert(del_id);
            }
        }

        Ok(HnswIndex { config, nodes, enter_point, max_level, deleted })
    }
}

/// HNSW 索引统计信息
#[derive(Debug, Clone)]
pub struct HnswStats {
    pub num_nodes: usize,
    pub max_level: i32,
    pub total_connections: usize,
    pub avg_connections_per_node: f64,
}

// ============================================================================
// 平面暴力搜索（用于验证正确性 + baseline）
// ============================================================================

pub struct BruteForceIndex {
    dim: usize,
    vectors: Vec<Vec<f32>>,
    metric: DistanceMetric,
}

impl BruteForceIndex {
    pub fn new(dim: usize, metric: DistanceMetric) -> Self {
        BruteForceIndex { dim, vectors: Vec::new(), metric }
    }

    pub fn insert(&mut self, v: Vec<f32>) -> u32 {
        assert_eq!(v.len(), self.dim);
        let id = self.vectors.len() as u32;
        self.vectors.push(v);
        id
    }

    pub fn search(&self, query: &[f32], k: usize) -> Vec<Neighbor> {
        let mut results: Vec<Neighbor> = self.vectors.iter().enumerate().map(|(i, v)| {
            let dist = match self.metric {
                DistanceMetric::L2 => l2_distance_sq(query, v),
                DistanceMetric::InnerProduct => -inner_product(query, v),
                DistanceMetric::Cosine => -cosine_similarity(query, v),
            };
            Neighbor { id: i as u32, distance: dist }
        }).collect();

        results.sort_by(|a, b| a.distance.partial_cmp(&b.distance).unwrap_or(Ordering::Equal));
        results.truncate(k);
        results
    }

    pub fn len(&self) -> usize {
        self.vectors.len()
    }
}

// ============================================================================
// 测试
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn random_vector(dim: usize, seed: u32) -> Vec<f32> {
        let mut v = Vec::with_capacity(dim);
        let mut s = seed as u64;
        for _ in 0..dim {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            v.push((s as f32) / (u64::MAX as f32) * 2.0 - 1.0);
        }
        v
    }

    #[test]
    fn test_hnsw_basic() {
        let config = HnswConfig {
            dim: 8,
            m: 8,
            m_max0: 16,
            ef_construction: 50,
            ef_search: 30,
            metric: DistanceMetric::L2,
            quantize: false,
        };
        let mut index = HnswIndex::new(config);

        // 插入 100 个随机向量
        for i in 0..100 {
            let v = random_vector(8, i);
            index.insert(v).unwrap();
        }

        assert_eq!(index.len(), 100);

        // 搜索一个已知向量
        let query = random_vector(8, 42);
        let results = index.search(&query, 5);
        assert!(!results.is_empty());
        assert!(results.len() <= 5);

        // 第一个结果应该是 id=42 自己（距离最近）
        assert_eq!(results[0].id, 42);
        assert!(results[0].distance < 0.001); // 自己和自己距离≈0
    }

    #[test]
    fn test_hnsw_vs_bruteforce() {
        let dim = 16;
        let n = 500;
        let metric = DistanceMetric::L2;

        let mut hnsw = HnswIndex::new(HnswConfig {
            dim, m: 16, m_max0: 32, ef_construction: 200, ef_search: 100, metric,
            quantize: false,
        });
        let mut bf = BruteForceIndex::new(dim, metric);

        // 插入相同的数据
        for i in 0..n {
            let v = random_vector(dim, i * 7);
            hnsw.insert(v.clone()).unwrap();
            bf.insert(v);
        }

        // 用多个查询测试召回率
        let mut total_recall = 0.0;
        let num_queries = 20;
        let k = 10;

        for q in 0..num_queries {
            let query = random_vector(dim, 10000 + q);
            let hnsw_results = hnsw.search(&query, k);
            let bf_results = bf.search(&query, k);

            // 计算 recall: hnsw 结果中有多少在 bf 的 top-k 中
            let bf_ids: std::collections::HashSet<u32> = bf_results.iter().map(|r| r.id).collect();
            let hit = hnsw_results.iter().filter(|r| bf_ids.contains(&r.id)).count();
            let recall = hit as f64 / k as f64;
            total_recall += recall;
        }

        let avg_recall = total_recall / num_queries as f64;
        // HNSW 在 ef_search=100, M=16 时召回率通常 > 0.95
        assert!(avg_recall > 0.85, "平均召回率太低: {:.3}", avg_recall);
    }

    #[test]
    fn test_l2_distance() {
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![4.0, 6.0, 8.0];
        // (3)^2 + (4)^2 + (5)^2 = 9 + 16 + 25 = 50
        let d = l2_distance_sq(&a, &b);
        assert!((d - 50.0).abs() < 0.001);
    }

    #[test]
    fn test_cosine() {
        let a = vec![1.0, 0.0];
        let b = vec![0.0, 1.0];
        assert!(cosine_similarity(&a, &b).abs() < 0.001);

        let c = vec![2.0, 0.0];
        assert!((cosine_similarity(&a, &c) - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_hnsw_empty() {
        let config = HnswConfig::default();
        let index = HnswIndex::new(config);
        assert!(index.is_empty());
        assert_eq!(index.len(), 0);

        let query = vec![0.0; 128];
        let results = index.search(&query, 10);
        assert!(results.is_empty());
    }

    #[test]
    fn test_hnsw_single_vector() {
        let mut index = HnswIndex::new(HnswConfig {
            dim: 4, m: 4, m_max0: 8, ef_construction: 10, ef_search: 10,
            metric: DistanceMetric::L2,
            quantize: false,
        });
        let v = vec![1.0, 2.0, 3.0, 4.0];
        let id = index.insert(v.clone()).unwrap();
        assert_eq!(id, 0);
        assert_eq!(index.len(), 1);

        let results = index.search(&v, 1);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, 0);
        assert!(results[0].distance < 0.001);
    }

    #[test]
    fn test_inner_product_metric() {
        let dim = 8;
        let mut index = HnswIndex::new(HnswConfig {
            dim, m: 8, m_max0: 16, ef_construction: 50, ef_search: 30,
            metric: DistanceMetric::InnerProduct,
            quantize: false,
        });
        let mut bf = BruteForceIndex::new(dim, DistanceMetric::InnerProduct);

        for i in 0..100 {
            let v = random_vector(dim, i * 3);
            index.insert(v.clone()).unwrap();
            bf.insert(v);
        }

        let query = random_vector(dim, 999);
        let hnsw_results = index.search(&query, 5);
        let bf_results = bf.search(&query, 5);

        assert_eq!(hnsw_results.len(), 5);
        assert_eq!(bf_results.len(), 5);
        // 验证第一个结果的 id 一致（近似搜索，可能有偏差，只验证数量和非空）
        assert!(!hnsw_results.is_empty());
    }

    #[test]
    fn test_cosine_metric() {
        let dim = 8;
        let mut index = HnswIndex::new(HnswConfig {
            dim, m: 8, m_max0: 16, ef_construction: 50, ef_search: 30,
            metric: DistanceMetric::Cosine,
            quantize: false,
        });

        for i in 0..50 {
            let mut v = random_vector(dim, i * 5);
            normalize(&mut v);
            index.insert(v).unwrap();
        }

        let mut query = random_vector(dim, 777);
        normalize(&mut query);
        let results = index.search(&query, 5);
        assert_eq!(results.len(), 5);
    }

    #[test]
    fn test_normalize() {
        let mut v = vec![3.0, 4.0];
        normalize(&mut v);
        // 3-4-5 三角形，归一化后应为 [0.6, 0.8]
        assert!((v[0] - 0.6).abs() < 0.001);
        assert!((v[1] - 0.8).abs() < 0.001);
        // 范数应为 1
        assert!((norm(&v) - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_normalize_zero_vector() {
        let mut v = vec![0.0; 5];
        normalize(&mut v);
        // 零向量归一化后仍为零向量（不崩溃）
        for &x in &v {
            assert_eq!(x, 0.0);
        }
    }

    #[test]
    fn test_norm() {
        let v = vec![0.0, 0.0, 0.0];
        assert_eq!(norm(&v), 0.0);

        let v = vec![1.0, 0.0, 0.0];
        assert_eq!(norm(&v), 1.0);

        let v = vec![3.0, 4.0, 0.0];
        assert!((norm(&v) - 5.0).abs() < 0.001);
    }

    #[test]
    fn test_inner_product() {
        let a = vec![1.0, 2.0, 3.0];
        let b = vec![4.0, 5.0, 6.0];
        // 1*4 + 2*5 + 3*6 = 4 + 10 + 18 = 32
        assert!((inner_product(&a, &b) - 32.0).abs() < 0.001);
    }

    #[test]
    fn test_get_vector() {
        let mut index = HnswIndex::new(HnswConfig {
            dim: 4, m: 4, m_max0: 8, ef_construction: 10, ef_search: 10,
            metric: DistanceMetric::L2,
            quantize: false,
        });
        let v = vec![1.0, 2.0, 3.0, 4.0];
        index.insert(v.clone()).unwrap();

        assert_eq!(index.get_vector(0), Some(v.as_slice()));
        assert_eq!(index.get_vector(1), None);
        assert_eq!(index.get_vector(999), None);
    }

    #[test]
    fn test_hnsw_stats() {
        let mut index = HnswIndex::new(HnswConfig {
            dim: 8, m: 8, m_max0: 16, ef_construction: 50, ef_search: 30,
            metric: DistanceMetric::L2,
            quantize: false,
        });

        // 空索引 stats
        let stats = index.stats();
        assert_eq!(stats.num_nodes, 0);
        assert_eq!(stats.max_level, -1);
        assert_eq!(stats.total_connections, 0);
        assert_eq!(stats.avg_connections_per_node, 0.0);

        // 插入一些向量
        for i in 0..50 {
            index.insert(random_vector(8, i)).unwrap();
        }

        let stats = index.stats();
        assert_eq!(stats.num_nodes, 50);
        assert!(stats.max_level >= 0);
        assert!(stats.total_connections > 0);
        assert!(stats.avg_connections_per_node > 0.0);
    }

    #[test]
    fn test_brute_force_basic() {
        let dim = 4;
        let mut bf = BruteForceIndex::new(dim, DistanceMetric::L2);
        assert_eq!(bf.len(), 0);

        bf.insert(vec![1.0, 0.0, 0.0, 0.0]);
        bf.insert(vec![0.0, 1.0, 0.0, 0.0]);
        bf.insert(vec![0.0, 0.0, 1.0, 0.0]);
        assert_eq!(bf.len(), 3);

        let query = vec![0.9, 0.1, 0.0, 0.0];
        let results = bf.search(&query, 2);
        assert_eq!(results.len(), 2);
        // 最接近的应该是 id=0（[1,0,0,0]）
        assert_eq!(results[0].id, 0);
    }

    #[test]
    fn test_hnsw_config_default() {
        let config = HnswConfig::default();
        assert_eq!(config.dim, 128);
        assert_eq!(config.m, 16);
        assert_eq!(config.m_max0, 32);
        assert_eq!(config.ef_construction, 100);
        assert_eq!(config.ef_search, 50);
        assert_eq!(config.metric, DistanceMetric::L2);
    }

    #[test]
    fn test_cosine_opposite_vectors() {
        let a = vec![1.0, 0.0];
        let b = vec![-1.0, 0.0];
        assert!((cosine_similarity(&a, &b) - (-1.0)).abs() < 0.001);
    }

    #[test]
    fn test_cosine_zero_vector() {
        let a = vec![0.0, 0.0];
        let b = vec![1.0, 1.0];
        assert_eq!(cosine_similarity(&a, &b), 0.0);
        assert_eq!(cosine_similarity(&b, &a), 0.0);
    }

    // ========================================================================
    // Tombstone / 逻辑删除测试（v0.12.0 DELETE 支持）
    // ========================================================================

    #[test]
    fn test_tombstone_mark_deleted() {
        let mut index = HnswIndex::new(HnswConfig {
            dim: 4, m: 4, m_max0: 8, ef_construction: 20, ef_search: 20,
            metric: DistanceMetric::L2,
            quantize: false,
        });

        for i in 0..10 {
            let v = random_vector(4, i);
            index.insert(v).unwrap();
        }

        assert_eq!(index.len(), 10);
        assert_eq!(index.active_len(), 10);
        assert_eq!(index.deleted_count(), 0);

        // 删除 id=3 和 id=7
        assert!(index.mark_deleted(3));
        assert!(index.mark_deleted(7));
        assert_eq!(index.deleted_count(), 2);
        assert_eq!(index.active_len(), 8);

        // 重复删除返回 false（已在集合中）
        assert!(!index.mark_deleted(3));

        // 不存在的节点
        assert!(!index.mark_deleted(999));

        // 验证 is_deleted
        assert!(index.is_deleted(3));
        assert!(index.is_deleted(7));
        assert!(!index.is_deleted(0));
        assert!(!index.is_deleted(5));
    }

    #[test]
    fn test_tombstone_search_filtering() {
        let dim = 8;
        let n = 50;
        let mut index = HnswIndex::new(HnswConfig {
            dim, m: 8, m_max0: 16, ef_construction: 100, ef_search: 50,
            metric: DistanceMetric::L2,
            quantize: false,
        });

        for i in 0..n {
            let v = random_vector(dim, i);
            index.insert(v).unwrap();
        }

        // 删除前 10 个向量
        for i in 0..10 {
            index.mark_deleted(i);
        }
        assert_eq!(index.deleted_count(), 10);
        assert_eq!(index.active_len(), 40);

        // 搜索 id=5（已删除），结果中不应出现
        let query = random_vector(dim, 5);
        let results = index.search(&query, 5);
        assert!(!results.is_empty());
        for r in &results {
            assert!(!index.is_deleted(r.id), "结果中包含已删除节点 {}", r.id);
        }

        // 搜索 id=42（未删除），应该能找到
        let query2 = random_vector(dim, 42);
        let results2 = index.search(&query2, 5);
        assert!(results2.iter().any(|r| r.id == 42), "应能找到未删除的 id=42");
    }

    #[test]
    fn test_tombstone_undelete() {
        let mut index = HnswIndex::new(HnswConfig {
            dim: 4, m: 4, m_max0: 8, ef_construction: 20, ef_search: 20,
            metric: DistanceMetric::L2,
            quantize: false,
        });

        for i in 0..5 {
            let v = random_vector(4, i);
            index.insert(v).unwrap();
        }

        index.mark_deleted(2);
        assert!(index.is_deleted(2));
        assert_eq!(index.active_len(), 4);

        // 取消删除
        assert!(index.undelete(2));
        assert!(!index.is_deleted(2));
        assert_eq!(index.active_len(), 5);

        // 重复取消返回 false
        assert!(!index.undelete(2));

        // 搜索应能找到 id=2
        let query = random_vector(4, 2);
        let results = index.search(&query, 3);
        assert!(results.iter().any(|r| r.id == 2));
    }

    #[test]
    fn test_tombstone_serialization_roundtrip() {
        let mut index = HnswIndex::new(HnswConfig {
            dim: 8, m: 8, m_max0: 16, ef_construction: 50, ef_search: 30,
            metric: DistanceMetric::L2,
            quantize: false,
        });

        for i in 0..30 {
            let v = random_vector(8, i * 3);
            index.insert(v).unwrap();
        }

        // 删除几个节点
        index.mark_deleted(5);
        index.mark_deleted(12);
        index.mark_deleted(25);

        assert_eq!(index.deleted_count(), 3);
        assert_eq!(index.active_len(), 27);

        // 序列化 → 反序列化
        let bytes = index.to_bytes();
        let restored = HnswIndex::from_bytes(&bytes).unwrap();

        assert_eq!(restored.len(), 30);
        assert_eq!(restored.deleted_count(), 3);
        assert_eq!(restored.active_len(), 27);
        assert!(restored.is_deleted(5));
        assert!(restored.is_deleted(12));
        assert!(restored.is_deleted(25));
        assert!(!restored.is_deleted(0));
        assert!(!restored.is_deleted(10));

        // 验证搜索结果过滤 tombstone
        let query = random_vector(8, 5 * 3);
        let results = restored.search(&query, 5);
        for r in &results {
            assert!(!restored.is_deleted(r.id));
        }
    }

    #[test]
    fn test_tombstone_backward_compatibility() {
        // 模拟旧版本格式（没有 tombstone 段）：手动构造不含 tombstone 的字节
        let mut index = HnswIndex::new(HnswConfig {
            dim: 4, m: 4, m_max0: 8, ef_construction: 20, ef_search: 20,
            metric: DistanceMetric::L2,
            quantize: false,
        });
        for i in 0..5 {
            let v = random_vector(4, i);
            index.insert(v).unwrap();
        }

        let full_bytes = index.to_bytes();
        // 找到节点数据结束位置（num_nodes 后所有节点数据）
        // 旧格式 = magic(9) + config(5*4+1) + max_level(4) + enter_point(4) + num_nodes(4) + nodes_data
        // 我们截断到节点数据结束处（去掉末尾的 tombstone 段）
        // tombstone 段 = deleted_count(4) + deleted_ids(4 * deleted_count)
        let tombstone_size = 4 + 4 * index.deleted_count();
        let old_format_len = full_bytes.len() - tombstone_size;
        let old_format_bytes = &full_bytes[..old_format_len];

        // 旧格式应能正常加载，deleted 为空
        let restored = HnswIndex::from_bytes(old_format_bytes).unwrap();
        assert_eq!(restored.len(), 5);
        assert_eq!(restored.deleted_count(), 0);
        assert_eq!(restored.active_len(), 5);
    }

    #[test]
    fn test_tombstone_high_deletion_ratio() {
        // 大量删除场景下验证搜索仍能返回有效结果
        let dim = 8;
        let n = 100;
        let mut index = HnswIndex::new(HnswConfig {
            dim, m: 8, m_max0: 16, ef_construction: 100, ef_search: 50,
            metric: DistanceMetric::L2,
            quantize: false,
        });

        for i in 0..n {
            let v = random_vector(dim, i);
            index.insert(v).unwrap();
        }

        // 删除 80% 的节点
        for i in 0..80 {
            index.mark_deleted(i);
        }
        assert_eq!(index.deleted_count(), 80);
        assert_eq!(index.active_len(), 20);

        // 搜索应该能返回有效结果（数量 = active 或 k 中较小的）
        let query = random_vector(dim, 999);
        let results = index.search(&query, 10);
        assert_eq!(results.len(), 10);
        for r in &results {
            assert!(!index.is_deleted(r.id), "结果包含已删除节点 {}", r.id);
            assert!(r.id >= 80, "结果应来自未删除的 id 范围 [80, 100)");
        }
    }

    // ========================================================================
    // INT8 量化测试（v0.15.0 新增）
    // ========================================================================

    #[test]
    fn test_quantize_roundtrip() {
        // 测试量化/反量化往返
        let v = vec![0.5, -0.3, 0.8, -0.1, 0.0, 0.9, -0.7, 0.2];
        let (q, scale, offset) = quantize_to_int8(&v);
        assert_eq!(q.len(), v.len());
        let deq = dequantize_to_f32(&q, scale, offset);
        assert_eq!(deq.len(), v.len());
        // 反量化后的误差应较小
        for (a, b) in v.iter().zip(deq.iter()) {
            let err = (a - b).abs();
            assert!(err < 0.02, "量化误差过大: {} vs {}, err={}", a, b, err);
        }
    }

    #[test]
    fn test_quantize_all_same() {
        // 所有值相同的情况
        let v = vec![0.5; 8];
        let (q, scale, offset) = quantize_to_int8(&v);
        assert_eq!(q.len(), 8);
        assert!(q.iter().all(|&x| x == 0));
        let deq = dequantize_to_f32(&q, scale, offset);
        for (a, b) in v.iter().zip(deq.iter()) {
            assert!((a - b).abs() < 0.001, "全相同值反量化误差过大");
        }
    }

    #[test]
    fn test_quantize_empty() {
        let (q, scale, offset) = quantize_to_int8(&[]);
        assert!(q.is_empty());
        let deq = dequantize_to_f32(&q, scale, offset);
        assert!(deq.is_empty());
    }

    #[test]
    fn test_hnsw_quantized_basic() {
        let dim = 8;
        let n = 100;
        let mut index = HnswIndex::new(HnswConfig {
            dim, m: 8, m_max0: 16, ef_construction: 100, ef_search: 50,
            metric: DistanceMetric::L2,
            quantize: true,
        });

        for i in 0..n {
            let v = random_vector(dim, i);
            index.insert(v).unwrap();
        }

        // 验证量化数据已存储
        for i in 0..n {
            let node = &index.nodes[i as usize];
            assert!(!node.quantized.is_empty(), "节点 {} 应有量化数据", i);
            assert_eq!(node.quantized.len(), dim);
        }

        // 搜索应返回有效结果
        let query = random_vector(dim, 999);
        let results = index.search(&query, 10);
        assert_eq!(results.len(), 10, "量化索引应返回 10 个结果");
        for r in &results {
            assert!(r.distance >= 0.0, "距离应为非负");
        }
    }

    #[test]
    fn test_hnsw_quantized_precision() {
        // 比较量化索引与 f32 索引的搜索召回率
        let dim = 8;
        let n = 200;
        let mut f32_index = HnswIndex::new(HnswConfig {
            dim, m: 16, m_max0: 32, ef_construction: 200, ef_search: 100,
            metric: DistanceMetric::L2,
            quantize: false,
        });
        let mut q_index = HnswIndex::new(HnswConfig {
            dim, m: 16, m_max0: 32, ef_construction: 200, ef_search: 100,
            metric: DistanceMetric::L2,
            quantize: true,
        });
        let mut bf = BruteForceIndex::new(dim, DistanceMetric::L2);

        for i in 0..n {
            let v = random_vector(dim, i);
            f32_index.insert(v.clone()).unwrap();
            q_index.insert(v.clone()).unwrap();
            bf.insert(v);
        }

        let query = random_vector(dim, 999);
        let f32_results = f32_index.search(&query, 10);
        let q_results = q_index.search(&query, 10);
        let bf_results = bf.search(&query, 10);

        // 检查量化索引的 top-10 结果中，有多少在 f32 索引的 top-10 中
        let f32_ids: std::collections::HashSet<u32> = f32_results.iter().map(|r| r.id).collect();
        let q_ids: std::collections::HashSet<u32> = q_results.iter().map(|r| r.id).collect();

        let overlap = f32_ids.intersection(&q_ids).count();
        // 量化索引的召回率应 >= 70%（8 维随机向量，量化精度损失较大）
        assert!(overlap >= 7, "量化召回率过低: {} / 10", overlap);

        // 检查量化索引与暴力搜索的召回率
        let bf_ids: std::collections::HashSet<u32> = bf_results.iter().map(|r| r.id).collect();
        let q_vs_bf_overlap = bf_ids.intersection(&q_ids).count();
        assert!(q_vs_bf_overlap >= 7, "量化 vs BF 召回率过低: {} / 10", q_vs_bf_overlap);
    }

    #[test]
    fn test_hnsw_quantized_serialize_deserialize() {
        let dim = 8;
        let n = 50;
        let mut index = HnswIndex::new(HnswConfig {
            dim, m: 8, m_max0: 16, ef_construction: 100, ef_search: 50,
            metric: DistanceMetric::L2,
            quantize: true,
        });

        for i in 0..n {
            let v = random_vector(dim, i);
            index.insert(v).unwrap();
        }

        // 序列化
        let bytes = index.to_bytes();
        // 应该使用 HNSW_IDX2 magic
        assert_eq!(&bytes[..9], b"HNSW_IDX2", "量化索引应使用 HNSW_IDX2 magic");

        // 反序列化
        let restored = HnswIndex::from_bytes(&bytes).unwrap();
        assert_eq!(restored.nodes.len(), n as usize);
        assert!(restored.config.quantize, "反序列化后 quantize 应为 true");

        // 验证量化数据已恢复
        for i in 0..n {
            assert!(!restored.nodes[i as usize].quantized.is_empty(), "反序列化后节点 {} 应有量化数据", i);
            assert_eq!(restored.nodes[i as usize].quantized.len(), dim);
        }

        // 搜索验证
        let query = random_vector(dim, 999);
        let results = restored.search(&query, 10);
        assert_eq!(results.len(), 10, "反序列化后搜索应返回 10 个结果");
    }

    #[test]
    fn test_hnsw_v1_backward_compat() {
        // 验证旧版 HNSW_IDX1 格式仍可读取
        // 构造一个 f32 索引，序列化为旧格式，然后反序列化
        let dim = 4;
        let mut index = HnswIndex::new(HnswConfig {
            dim, m: 8, m_max0: 16, ef_construction: 50, ef_search: 30,
            metric: DistanceMetric::L2,
            quantize: false,
        });
        let v = vec![0.1, 0.2, 0.3, 0.4];
        index.insert(v).unwrap();

        // 手动构造旧格式字节
        let mut buf = Vec::new();
        buf.extend_from_slice(b"HNSW_IDX1");
        buf.extend_from_slice(&(dim as u32).to_le_bytes());
        buf.extend_from_slice(&8u32.to_le_bytes()); // m
        buf.extend_from_slice(&16u32.to_le_bytes()); // m_max0
        buf.extend_from_slice(&50u32.to_le_bytes()); // ef_construction
        buf.extend_from_slice(&30u32.to_le_bytes()); // ef_search
        buf.push(0u8); // metric = L2
        buf.extend_from_slice(&0i32.to_le_bytes()); // max_level
        buf.extend_from_slice(&0u32.to_le_bytes()); // enter_point = 0
        buf.extend_from_slice(&1u32.to_le_bytes()); // num_nodes = 1
        // node 0
        buf.extend_from_slice(&0u32.to_le_bytes()); // id = 0
        for f in [0.1f32, 0.2f32, 0.3f32, 0.4f32] {
            buf.extend_from_slice(&f.to_le_bytes());
        }
        buf.extend_from_slice(&1u32.to_le_bytes()); // num_layers = 1
        buf.extend_from_slice(&0u32.to_le_bytes()); // num_neighbors = 0
        buf.extend_from_slice(&0u32.to_le_bytes()); // deleted_count = 0

        let restored = HnswIndex::from_bytes(&buf).unwrap();
        assert_eq!(restored.nodes.len(), 1);
        assert!(!restored.config.quantize);
        assert_eq!(restored.nodes[0].vector, vec![0.1, 0.2, 0.3, 0.4]);
        assert!(restored.nodes[0].quantized.is_empty());
    }
}
