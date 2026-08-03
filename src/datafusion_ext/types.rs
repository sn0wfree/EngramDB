//! 类型转换：EngramDB 类型 ↔ Arrow 类型 ↔ DataFusion 类型

use crate::common::types::DataType;
use arrow::datatypes::DataType as ArrowDataType;
use arrow::datatypes::Field;
use datafusion_common::Result as DfResult;

/// EngramDB DataType → Arrow DataType
pub fn to_arrow_type(dt: &DataType) -> ArrowDataType {
    match dt {
        DataType::Boolean => ArrowDataType::Boolean,
        DataType::Int8 => ArrowDataType::Int8,
        DataType::Int16 => ArrowDataType::Int16,
        DataType::Int32 => ArrowDataType::Int32,
        DataType::Int64 => ArrowDataType::Int64,
        DataType::UInt8 => ArrowDataType::UInt8,
        DataType::UInt16 => ArrowDataType::UInt16,
        DataType::UInt32 => ArrowDataType::UInt32,
        DataType::UInt64 => ArrowDataType::UInt64,
        DataType::Float32 => ArrowDataType::Float32,
        DataType::Float64 => ArrowDataType::Float64,
        DataType::Varchar => ArrowDataType::Utf8,
        DataType::Timestamp => ArrowDataType::Int64, // 以 i64 微秒存储
        DataType::Blob => ArrowDataType::Binary,
        DataType::Null => ArrowDataType::Null,
    }
}

/// Arrow DataType → EngramDB DataType
pub fn from_arrow_type(at: &ArrowDataType) -> Option<DataType> {
    match at {
        ArrowDataType::Boolean => Some(DataType::Boolean),
        ArrowDataType::Int8 => Some(DataType::Int8),
        ArrowDataType::Int16 => Some(DataType::Int16),
        ArrowDataType::Int32 => Some(DataType::Int32),
        ArrowDataType::Int64 => Some(DataType::Int64),
        ArrowDataType::UInt8 => Some(DataType::UInt8),
        ArrowDataType::UInt16 => Some(DataType::UInt16),
        ArrowDataType::UInt32 => Some(DataType::UInt32),
        ArrowDataType::UInt64 => Some(DataType::UInt64),
        ArrowDataType::Float32 => Some(DataType::Float32),
        ArrowDataType::Float64 => Some(DataType::Float64),
        ArrowDataType::Utf8 => Some(DataType::Varchar),
        ArrowDataType::Binary => Some(DataType::Blob),
        ArrowDataType::Null => Some(DataType::Null),
        _ => None,
    }
}

/// 创建 Arrow Field
pub fn make_field(name: &str, dt: &DataType, nullable: bool) -> Field {
    Field::new(name, to_arrow_type(dt), nullable)
}

/// 将 EngramDB 列定义转为 Arrow Schema
pub fn columns_to_schema(
    columns: &[(String, DataType, bool)],
) -> arrow::datatypes::Schema {
    let fields: Vec<Field> = columns
        .iter()
        .map(|(name, dt, nullable)| make_field(name, dt, *nullable))
        .collect();
    arrow::datatypes::Schema::new(fields)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_type_conversion_roundtrip() {
        let types = vec![
            DataType::Boolean,
            DataType::Int32,
            DataType::Int64,
            DataType::Float64,
            DataType::Varchar,
        ];
        for t in &types {
            let arrow = to_arrow_type(t);
            let back = from_arrow_type(&arrow).unwrap();
            assert_eq!(t, &back);
        }
    }
}
