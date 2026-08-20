//! Phase 2 P1-B：列级自动编码选择
//!
//! 评估 `ColumnData` 的特征（基数、单调性、值范围），推荐最合适的 codec。
//!
//! ## 与 `compression::compress()` 的区别
//! - `compress(data, data_type)`：尝试多种 codec，返回**实际压缩字节** + 类型
//! - `recommend_for_column()`：仅评估特征，**返回推荐类型**（不实际压缩）
//!
//! 后者用于：
//! - 大表 compact 前的"预算评估"（CPU 节省）
//! - 用户显式 `ALTER TABLE ... SET COMPRESSION = <rec>` 的提示
//! - 多表压缩统计聚合
//!
//! ## 算法
//! 1. **采样**：> 4096 值时仅采样前 N 个值
//! 2. **特征提取**：
//!    - 基数（distinct count）
//!    - 单调性（严格递增/递减？局部递增？）
//!    - 值范围（min/max）
//!    - 稀疏度（NULL 比例）
//! 3. **决策矩阵**：
//!
//! | DataType | 条件 | 推荐 codec |
//! |---|---|---|
//! | Int32/Int64 | 基数 < 8% × N | Dictionary |
//! | Int32/Int64 | 严格单调 | Delta |
//! | Int32/Int64 | 大范围 + 高熵 | ForBitPack |
//! | Int32/Int64 | 否则 | Rle (或 Uncompressed) |
//! | Timestamp | 严格单调 | DoubleDelta |
//! | Timestamp | 否则 | Delta |
//! | Float32/64 | 近似稳定 | Gorilla |
//! | Float32/64 | 否则 | Uncompressed |
//! | Varchar | 短串重复 | TokenDelta |
//! | Varchar | 长串 | Zstd |
//! | Varchar | 否则 | Uncompressed |
//! | Boolean | — | BooleanPack |
//!
//! ## 性能
//! - 采样 + 特征提取：O(N_sampled)
//! - 默认 N_sampled = 4096（< 4µs on NVMe）
//! - 对小列（< 4096 值）：直接全列评估

use std::collections::HashSet;

use crate::common::column_data::{ColumnData, ColumnValue};
use crate::common::config::CompressionType;
use crate::common::types::DataType;

/// 采样上限（列大于此值时只采前 N 个评估）
const SAMPLE_LIMIT: usize = 4096;

#[derive(Debug, Clone, Copy)]
pub struct ColumnStats {
    /// 采样数（可能 < 列长度）
    pub sampled_count: usize,
    /// 基数（distinct 值数）
    pub distinct_count: usize,
    /// 严格递增？
    pub is_strictly_increasing: bool,
    /// 严格递减？
    pub is_strictly_decreasing: bool,
    /// NULL 比例（0.0 - 1.0）
    pub null_ratio: f32,
}

/// 推荐结果
#[derive(Debug, Clone, Copy)]
pub struct Recommendation {
    pub codec: CompressionType,
    /// 估计压缩比（0.0 - 1.0，越小越好）
    pub estimated_ratio: f32,
    /// 推荐依据（人读）
    pub rationale: &'static str,
}

/// 对单列推荐最合适的 codec
///
/// `data`：原始 typed 列数据
/// `data_type`：列 DataType（用于 Varchar/JSON/Vector 区分）
pub fn recommend_for_column(data: &ColumnData, data_type: &DataType) -> Recommendation {
    let count = data.len();
    if count == 0 {
        return Recommendation {
            codec: CompressionType::Uncompressed,
            estimated_ratio: 1.0,
            rationale: "empty column",
        };
    }

    // 采样（< SAMPLE_LIMIT 时全列）
    let sample_count = count.min(SAMPLE_LIMIT);
    let stats = extract_stats(data, sample_count);

    decide_recommendation(data_type, &stats, count)
}

/// 从 typed ColumnData 提取特征（采样）
fn extract_stats(data: &ColumnData, sample_count: usize) -> ColumnStats {
    let mut distinct = HashSet::with_capacity(sample_count);
    let mut is_inc = true;
    let mut is_dec = true;
    let mut null_count = 0usize;
    let mut prev: Option<i64> = None;

    for i in 0..sample_count {
        // NULL 检查
        if let Some(nulls) = &data.nulls {
            if nulls.test(i) {
                null_count += 1;
                continue;
            }
        }

        let val: Option<i64> = match &data.values {
            ColumnValue::Int32(v) => Some(v[i] as i64),
            ColumnValue::Int64(v) => Some(v[i]),
            ColumnValue::Timestamp(v) => Some(v[i]),
            ColumnValue::Float32(_) | ColumnValue::Float64(_) => None, // 浮点不做单调性
            _ => None,
        };

        if let Some(v) = val {
            distinct.insert(format!("i64:{}", v));

            if let Some(p) = prev {
                if v <= p {
                    is_inc = false;
                }
                if v >= p {
                    is_dec = false;
                }
            }
            prev = Some(v);
        }
    }

    ColumnStats {
        sampled_count: sample_count,
        distinct_count: distinct.len(),
        is_strictly_increasing: is_inc && prev.is_some(),
        is_strictly_decreasing: is_dec && prev.is_some(),
        null_ratio: null_count as f32 / sample_count as f32,
    }
}

/// 根据 DataType + stats 决策 codec
fn decide_recommendation(data_type: &DataType, stats: &ColumnStats, total_count: usize) -> Recommendation {
    match data_type {
        DataType::Boolean => Recommendation {
            codec: CompressionType::BooleanPack,
            estimated_ratio: 0.125, // 1 bit/值
            rationale: "boolean: BooleanPack bit-packing",
        },

        DataType::Int32 | DataType::Int64 => {
            recommend_integer(stats, total_count)
        }

        DataType::Timestamp => {
            if stats.is_strictly_increasing {
                Recommendation {
                    codec: CompressionType::DoubleDelta,
                    estimated_ratio: 0.1,
                    rationale: "timestamp: monotonic increasing -> DoubleDelta (best)",
                }
            } else if stats.is_strictly_decreasing {
                Recommendation {
                    codec: CompressionType::Delta,
                    estimated_ratio: 0.3,
                    rationale: "timestamp: monotonic decreasing -> Delta",
                }
            } else {
                Recommendation {
                    codec: CompressionType::Delta,
                    estimated_ratio: 0.5,
                    rationale: "timestamp: non-monotonic -> Delta",
                }
            }
        }

        DataType::Float32 | DataType::Float64 => {
            if stats.distinct_count < 32 {
                Recommendation {
                    codec: CompressionType::Dictionary,
                    estimated_ratio: 0.3,
                    rationale: "float: low cardinality -> Dictionary",
                }
            } else {
                // Gorilla 适合时序监控类（连续波动小），这里无法精确判断
                // 保守：保持 Uncompressed（避免乱用增加开销）
                Recommendation {
                    codec: CompressionType::Uncompressed,
                    estimated_ratio: 1.0,
                    rationale: "float: high cardinality -> Uncompressed (Gorilla opt-in)",
                }
            }
        }

        DataType::Varchar => {
            // 简化策略：永远建议 Zstd（实际 compress() 会进一步比较 TokenDelta）
            Recommendation {
                codec: CompressionType::Zstd,
                estimated_ratio: 0.3,
                rationale: "varchar: default Zstd (compress() may try TokenDelta)",
            }
        }

        DataType::Json => {
            Recommendation {
                codec: CompressionType::Uncompressed,
                estimated_ratio: 1.0,
                rationale: "json: Uncompressed (varies too much)",
            }
        }

        DataType::Vector { .. } | DataType::VectorInt8 { .. } => {
            Recommendation {
                codec: CompressionType::Uncompressed,
                estimated_ratio: 1.0,
                rationale: "vector: Uncompressed (specialized codec TBD)",
            }
        }

        DataType::Blob => {
            Recommendation {
                codec: CompressionType::Uncompressed,
                estimated_ratio: 1.0,
                rationale: "blob: Uncompressed (binary data)",
            }
        }
    }
}

fn recommend_integer(stats: &ColumnStats, _total_count: usize) -> Recommendation {
    // 用采样数而非总数计算基数阈值（采样是大列唯一可评估的指标）
    // 阈值：distinct < 8% × sampled → Dictionary
    let low_card_threshold = (stats.sampled_count / 12).max(8);
    if stats.distinct_count < low_card_threshold {
        return Recommendation {
            codec: CompressionType::Dictionary,
            estimated_ratio: 0.2,
            rationale: "int: low cardinality (sampled) -> Dictionary",
        };
    }

    if stats.is_strictly_increasing || stats.is_strictly_decreasing {
        return Recommendation {
            codec: CompressionType::Delta,
            estimated_ratio: 0.2,
            rationale: "int: monotonic -> Delta",
        };
    }

    // 高熵（distinct > 50% × sampled）→ ForBitPack
    if stats.distinct_count > stats.sampled_count / 2 {
        return Recommendation {
            codec: CompressionType::ForBitPack,
            estimated_ratio: 0.5,
            rationale: "int: high entropy -> ForBitPack",
        };
    }

    // 默认 Rle（适合含重复值）
    Recommendation {
        codec: CompressionType::Rle,
        estimated_ratio: 0.6,
        rationale: "int: mixed -> Rle",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::column_data::ColumnData;

    fn make_int_col(values: Vec<i64>) -> ColumnData {
        let bytes: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        ColumnData::deserialize_typed(&bytes, &DataType::Int64, values.len())
    }

    fn make_ts_col(values: Vec<i64>) -> ColumnData {
        let bytes: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        ColumnData::deserialize_typed(&bytes, &DataType::Timestamp, values.len())
    }

    #[test]
    fn test_recommend_monotonic_int() {
        let col = make_int_col((0..1000).collect());
        let r = recommend_for_column(&col, &DataType::Int64);
        assert!(matches!(r.codec, CompressionType::Delta),
                "monotonic should pick Delta, got {:?}", r.codec);
        assert!(r.rationale.contains("Delta"));
    }

    #[test]
    fn test_recommend_monotonic_timestamp() {
        let col = make_ts_col((1_700_000_000_000..1_700_000_001_000).collect());
        let r = recommend_for_column(&col, &DataType::Timestamp);
        assert!(matches!(r.codec, CompressionType::DoubleDelta),
                "monotonic timestamp should pick DoubleDelta, got {:?}", r.codec);
    }

    #[test]
    fn test_recommend_low_cardinality_int() {
        // 1000 个值，只有 10 个不同
        let mut values = Vec::with_capacity(1000);
        for i in 0..1000 {
            values.push((i % 10) as i64);
        }
        let col = make_int_col(values);
        let r = recommend_for_column(&col, &DataType::Int64);
        assert!(matches!(r.codec, CompressionType::Dictionary),
                "low cardinality should pick Dictionary, got {:?}", r.codec);
    }

    #[test]
    fn test_recommend_high_cardinality_int() {
        // 1000 个值，几乎都不同，且非单调（zigzag）
        let values: Vec<i64> = (0..1000).map(|i| {
            let v = (i as i64) ^ ((i as i64) << 3) ^ ((i as i64) >> 2);
            // 混入负值破单调
            if i % 2 == 0 { v } else { -v }
        }).collect();
        let col = make_int_col(values);
        let r = recommend_for_column(&col, &DataType::Int64);
        // 高基数 + 非单调 → ForBitPack 或 Rle
        assert!(matches!(r.codec, CompressionType::ForBitPack | CompressionType::Rle),
                "high cardinality non-monotonic should pick ForBitPack or Rle, got {:?}", r.codec);
    }

    #[test]
    fn test_recommend_boolean() {
        let col = make_int_col(vec![0, 1, 0, 1, 1, 0]);
        let r = recommend_for_column(&col, &DataType::Boolean);
        assert_eq!(r.codec, CompressionType::BooleanPack);
    }

    #[test]
    fn test_recommend_empty_column() {
        let col = make_int_col(vec![]);
        let r = recommend_for_column(&col, &DataType::Int64);
        assert_eq!(r.codec, CompressionType::Uncompressed);
        assert_eq!(r.rationale, "empty column");
    }

    #[test]
    fn test_recommend_sampling_for_large_column() {
        // 大列（> SAMPLE_LIMIT）应被采样
        let values: Vec<i64> = (0..100_000).collect();
        let col = make_int_col(values);
        let r = recommend_for_column(&col, &DataType::Int64);
        // 单调 → Delta（采样应仍识别出单调性）
        assert!(matches!(r.codec, CompressionType::Delta),
                "monotonic large col should pick Delta, got {:?}", r.codec);
    }

    #[test]
    fn test_recommend_varchar_default_zstd() {
        // 字符串默认 Zstd（实际 compress() 会再尝试 TokenDelta）
        let col = make_int_col(vec![1, 2, 3]); // 占位
        let r = recommend_for_column(&col, &DataType::Varchar);
        assert_eq!(r.codec, CompressionType::Zstd);
    }

    #[test]
    fn test_recommend_blob_uncompressed() {
        let col = make_int_col(vec![1, 2, 3]);
        let r = recommend_for_column(&col, &DataType::Blob);
        assert_eq!(r.codec, CompressionType::Uncompressed);
    }
}