//! LSM 清单管理（Step 2.2）
//!
//! Manifest 记录所有活跃段文件的元数据。
//! 使用 JSON 格式，原子替换（临时文件 + rename）。
//!
//! 清单路径：`{db_dir}/MANIFEST`

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::common::error::{EngramDbError, Result};
use crate::storage::bloom_filter::ColumnBloom;
use crate::storage::segment::SegmentEntry;

/// 清单（段文件注册表）
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Manifest {
    /// 清单版本号（每次改写 +1）
    pub version: u64,
    /// 段条目
    pub segments: Vec<ManifestSegment>,
}

/// 清单中的段条目（序列化格式）
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ManifestSegment {
    /// 段文件路径（相对于 db_dir）
    pub path: String,
    /// 表 ID
    pub table_id: u32,
    /// 行范围 [start, end)
    pub row_range: (u64, u64),
    /// 总行数
    pub total_rows: u32,
    /// 文件大小
    pub size_bytes: u64,
}

impl Manifest {
    /// 创建空清单
    pub fn new() -> Self {
        Manifest {
            version: 1,
            segments: Vec::new(),
        }
    }

    /// 从文件加载清单
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::new());
        }
        let data = std::fs::read_to_string(path)?;
        let manifest: Manifest =
            serde_json::from_str(&data).map_err(|e| EngramDbError::Serialization(e.to_string()))?;
        Ok(manifest)
    }

    /// 原子保存清单（临时文件 + rename）
    pub fn save(&self, path: &Path) -> Result<()> {
        let tmp_path = path.with_extension("tmp");
        let json = serde_json::to_string_pretty(self).map_err(|e| EngramDbError::Serialization(e.to_string()))?;
        std::fs::write(&tmp_path, json)?;
        std::fs::rename(&tmp_path, path)?;
        Ok(())
    }

    /// 添加段
    pub fn add_segment(&mut self, entry: ManifestSegment) {
        self.version += 1;
        self.segments.push(entry);
    }

    /// 移除段（合并后 GC 旧段）
    pub fn remove_segments(&mut self, paths: &[&str]) {
        self.version += 1;
        self.segments.retain(|s| !paths.contains(&s.path.as_str()));
    }

    /// 获取指定表的所有段
    pub fn table_segments(&self, table_id: u32) -> Vec<&ManifestSegment> {
        self.segments.iter().filter(|s| s.table_id == table_id).collect()
    }

    /// 获取指定表的总行数
    pub fn table_row_count(&self, table_id: u32) -> u64 {
        self.table_segments(table_id).iter().map(|s| s.total_rows as u64).sum()
    }

    /// 从 ManifestSegment 构建 SegmentEntry（加载 zone map 和 Bloom 后填充）
    pub fn to_segment_entry(&self, seg: &ManifestSegment) -> SegmentEntry {
        SegmentEntry {
            path: PathBuf::from(&seg.path),
            table_id: seg.table_id,
            row_range: seg.row_range,
            total_rows: seg.total_rows,
            size_bytes: seg.size_bytes,
            zone_map_summary: Vec::new(), // 加载时填充
            bloom_summary: Vec::new(),    // 加载时填充
        }
    }
}

impl Default for Manifest {
    fn default() -> Self {
        Self::new()
    }
}
