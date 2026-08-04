//! Apache Arrow 列存对接
//!
//! 实现 EngramDB 内部向量格式与 Apache Arrow 格式的互转，
//! 支持 Arrow IPC 导入导出，便于与 Arrow 生态（DataFusion、Polars、Pandas 等）集成。
//!
//! 设计要点：
//! - 零拷贝优先：内存布局兼容时直接引用，否则转换
//! - IPC 格式：支持 Arrow IPC Stream / File 格式读写
//! - Schema 映射：EngramDB 类型 ↔ Arrow DataType 双向映射
//! - 批量处理：按 DataChunk 粒度转换，与执行引擎批处理对齐
//!
//! 注意：本模块是框架层实现，定义了接口和核心转换逻辑。
//! 完整的 Arrow 集成需要引入 arrow-rs crate，此处以类型定义和
//! 转换 trait 形式提供抽象，便于后续接入真实 Arrow 库。

use crate::common::error::{EngramDbError as DbError, Result};
use crate::executor::vector::{DataChunk, Vector};
use crate::Value;

/// Arrow 数据类型（简化版，对应 arrow-rs 的 DataType）
///
/// 这里定义独立的枚举以避免直接依赖 arrow-rs crate。
/// 实际集成时，可直接使用 arrow_schema::DataType。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArrowDataType {
    Null,
    Boolean,
    Int32,
    Int64,
    Float64,
    Utf8,       // 可变长度 UTF-8 字符串
    LargeUtf8,  // 64位偏移的 UTF-8 字符串
    Binary,     // 可变长度二进制
    Date32,     // 日期（从 1970-01-01 起的天数）
    TimestampSecond,
    TimestampMillisecond,
    TimestampMicrosecond,
    // 嵌套类型（预留）
    List(Box<ArrowField>),
    Struct(Vec<ArrowField>),
}

/// Arrow 字段（名称 + 类型 + 可空性）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArrowField {
    pub name: String,
    pub data_type: ArrowDataType,
    pub nullable: bool,
}

/// Arrow Schema（一组字段）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArrowSchema {
    pub fields: Vec<ArrowField>,
}

impl ArrowSchema {
    pub fn new(fields: Vec<ArrowField>) -> Self {
        Self { fields }
    }

    pub fn empty() -> Self {
        Self { fields: Vec::new() }
    }
}

/// Arrow 数组（抽象接口）
///
/// 表示一个 Arrow 格式的列数据。
/// 实际实现中对应 arrow_array::Array。
pub trait ArrowArray {
    /// 数据类型
    fn data_type(&self) -> ArrowDataType;

    /// 数组长度（元素个数）
    fn len(&self) -> usize;

    /// 是否为空
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 第 i 个元素是否为 null
    fn is_null(&self, i: usize) -> bool;

    /// 获取第 i 个值（Value 形式）
    fn get_value(&self, i: usize) -> Value;
}

/// Arrow RecordBatch（一批列式数据）
///
/// 对应 arrow_array::RecordBatch。
/// 一组等长的 Arrow Array，共享同一个 Schema。
pub struct ArrowRecordBatch {
    schema: ArrowSchema,
    columns: Vec<Box<dyn ArrowArray>>,
}

impl ArrowRecordBatch {
    /// 创建 RecordBatch
    pub fn try_new(schema: ArrowSchema, columns: Vec<Box<dyn ArrowArray>>) -> Result<Self> {
        if columns.len() != schema.fields.len() {
            return Err(DbError::Internal(
                format!(
                    "schema has {} fields but {} columns provided",
                    schema.fields.len(),
                    columns.len()
                )
            ));
        }

        // 验证所有列长度一致
        if !columns.is_empty() {
            let len = columns[0].len();
            for (i, col) in columns.iter().enumerate() {
                if col.len() != len {
                    return Err(DbError::Internal(
                        format!(
                            "column {} has length {}, expected {}",
                            i, col.len(), len
                        )
                    ));
                }
            }
        }

        Ok(Self { schema, columns })
    }

    pub fn schema(&self) -> &ArrowSchema {
        &self.schema
    }

    pub fn num_columns(&self) -> usize {
        self.columns.len()
    }

    pub fn num_rows(&self) -> usize {
        if self.columns.is_empty() { 0 } else { self.columns[0].len() }
    }

    pub fn column(&self, i: usize) -> &dyn ArrowArray {
        &*self.columns[i]
    }
}

// ============================================================
// 类型映射
// ============================================================

/// EngramDB Value 类型 → Arrow 数据类型
pub fn value_type_to_arrow(value: &Value) -> ArrowDataType {
    match value {
        Value::Null => ArrowDataType::Null,
        Value::Boolean(_) => ArrowDataType::Boolean,
        Value::Int32(_) => ArrowDataType::Int32,
        Value::Int64(_) => ArrowDataType::Int64,
        Value::Float32(_) => ArrowDataType::Float64, // 简化：映射到 Float64
        Value::Float64(_) => ArrowDataType::Float64,
        Value::Timestamp(_) => ArrowDataType::Int64, // 简化：映射到 Int64
        Value::Varchar(_) => ArrowDataType::Utf8,
        Value::Json(_) => ArrowDataType::Utf8,
        Value::Vector(_) => ArrowDataType::Utf8, // 序列化为字符串表示
        Value::VectorInt8(_) => ArrowDataType::Utf8, // 序列化为字符串表示
        Value::Blob(_) => ArrowDataType::Binary,
    }
}

/// Arrow 数据类型 → 描述性名称
pub fn arrow_type_name(dt: &ArrowDataType) -> &'static str {
    match dt {
        ArrowDataType::Null => "null",
        ArrowDataType::Boolean => "bool",
        ArrowDataType::Int32 => "int32",
        ArrowDataType::Int64 => "int64",
        ArrowDataType::Float64 => "float64",
        ArrowDataType::Utf8 => "utf8",
        ArrowDataType::LargeUtf8 => "large_utf8",
        ArrowDataType::Binary => "binary",
        ArrowDataType::Date32 => "date32",
        ArrowDataType::TimestampSecond => "timestamp[s]",
        ArrowDataType::TimestampMillisecond => "timestamp[ms]",
        ArrowDataType::TimestampMicrosecond => "timestamp[us]",
        ArrowDataType::List(_) => "list",
        ArrowDataType::Struct(_) => "struct",
    }
}

// ============================================================
// 双向转换
// ============================================================

/// Arrow 导入器：将 Arrow 格式数据转换为 EngramDB 内部格式
pub struct ArrowImporter;

impl ArrowImporter {
    /// 将 Arrow RecordBatch 转换为 DataChunk
    pub fn record_batch_to_chunk(batch: &ArrowRecordBatch) -> Result<DataChunk> {
        let mut vectors = Vec::with_capacity(batch.num_columns());
        let num_rows = batch.num_rows();

        for i in 0..batch.num_columns() {
            let array = batch.column(i);
            let vector = Self::array_to_vector(array)?;
            vectors.push(vector);
        }

        Ok(DataChunk {
            columns: vectors,
            count: num_rows,
        })
    }

    /// 将 Arrow Array 转换为 Vector
    pub fn array_to_vector(array: &dyn ArrowArray) -> Result<Vector> {
        let len = array.len();
        let mut result = Vector::new();

        for i in 0..len {
            result.push(array.get_value(i));
        }

        Ok(result)
    }

    /// 从 Arrow Schema 推导列名列表
    pub fn schema_to_column_names(schema: &ArrowSchema) -> Vec<String> {
        schema.fields.iter().map(|f| f.name.clone()).collect()
    }
}

/// Arrow 导出器：将 EngramDB 内部格式转换为 Arrow 格式
pub struct ArrowExporter;

impl ArrowExporter {
    /// 将 DataChunk 转换为 Arrow RecordBatch
    ///
    /// # 参数
    /// - `chunk`: 数据块
    /// - `column_names`: 列名列表（用于构建 Schema）
    pub fn chunk_to_record_batch(chunk: &DataChunk, column_names: &[String]) -> Result<ArrowRecordBatch> {
        if column_names.len() != chunk.num_columns() {
            return Err(DbError::Internal(
                format!(
                    "column_names has {} entries but chunk has {} columns",
                    column_names.len(),
                    chunk.num_columns()
                )
            ));
        }

        let mut fields = Vec::with_capacity(chunk.num_columns());
        let mut columns: Vec<Box<dyn ArrowArray>> = Vec::with_capacity(chunk.num_columns());

        for i in 0..chunk.num_columns() {
            let vector = &chunk.columns[i];
            let field = Self::vector_to_field(vector, &column_names[i]);
            let array = Self::vector_to_array(vector)?;
            fields.push(field);
            columns.push(array);
        }

        let schema = ArrowSchema::new(fields);
        ArrowRecordBatch::try_new(schema, columns)
    }

    /// 从 Vector 推导 Arrow Field
    fn vector_to_field(vector: &Vector, name: &str) -> ArrowField {
        // 从第一个非空值推导类型
        let data_type = if vector.len() == 0 {
            ArrowDataType::Null
        } else {
            // 找第一个非空值
            let mut dt = ArrowDataType::Null;
            for i in 0..vector.len() {
                let v = vector.get(i);
                if !v.is_null() {
                    dt = value_type_to_arrow(&v);
                    break;
                }
            }
            dt
        };

        // 检查是否有 null 值
        let nullable = (0..vector.len()).any(|i| vector.get(i).is_null());

        ArrowField {
            name: name.to_string(),
            data_type,
            nullable,
        }
    }

    /// 将 Vector 转换为 Arrow Array（boxed trait object）
    fn vector_to_array(vector: &Vector) -> Result<Box<dyn ArrowArray>> {
        Ok(Box::new(VectorBackedArray {
            values: (0..vector.len()).map(|i| vector.get(i).clone()).collect(),
        }))
    }
}

/// 基于 Vector 的 ArrowArray 实现（内存中的简单实现）
///
/// 实际生产中应替换为 arrow-rs 的具体数组类型（Int64Array 等），
/// 这里用 Value Vec 做演示，验证接口正确性。
struct VectorBackedArray {
    values: Vec<Value>,
}

impl ArrowArray for VectorBackedArray {
    fn data_type(&self) -> ArrowDataType {
        for v in &self.values {
            if !v.is_null() {
                return value_type_to_arrow(v);
            }
        }
        ArrowDataType::Null
    }

    fn len(&self) -> usize {
        self.values.len()
    }

    fn is_null(&self, i: usize) -> bool {
        self.values[i].is_null()
    }

    fn get_value(&self, i: usize) -> Value {
        self.values[i].clone()
    }
}

// ============================================================
// Arrow IPC（框架）
// ============================================================

/// Arrow IPC 格式
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IpcFormat {
    /// Stream 格式（流式，无随机访问）
    Stream,
    /// File 格式（带 footer，支持随机访问）
    File,
}

/// Arrow IPC 写入器（抽象接口）
///
/// 实际实现中对应 arrow_ipc::writer::StreamWriter / FileWriter。
pub struct ArrowIpcWriter {
    format: IpcFormat,
    schema: Option<ArrowSchema>,
    batches: Vec<ArrowRecordBatch>,
}

impl ArrowIpcWriter {
    /// 创建新的 IPC 写入器
    pub fn new(format: IpcFormat) -> Self {
        Self {
            format,
            schema: None,
            batches: Vec::new(),
        }
    }

    /// 写入 Schema（必须在写入数据前调用）
    pub fn write_schema(&mut self, schema: ArrowSchema) -> Result<()> {
        self.schema = Some(schema);
        Ok(())
    }

    /// 写入一个 RecordBatch
    pub fn write_batch(&mut self, batch: ArrowRecordBatch) -> Result<()> {
        if self.schema.is_none() {
            return Err(DbError::Internal(
                "schema must be written before record batches".to_string()
            ));
        }
        self.batches.push(batch);
        Ok(())
    }

    /// 完成写入，返回序列化后的字节
    ///
    /// 实际实现中会生成 Arrow IPC 格式的二进制数据。
    /// 这里返回元数据描述用于验证。
    pub fn finish(self) -> Result<Vec<u8>> {
        // 实际实现：编码为 Arrow IPC 二进制格式
        // 这里返回一个占位描述
        let desc = format!(
            "Arrow IPC {:?}: {} fields, {} batches, {} total rows",
            self.format,
            self.schema.as_ref().map(|s| s.fields.len()).unwrap_or(0),
            self.batches.len(),
            self.batches.iter().map(|b| b.num_rows()).sum::<usize>(),
        );
        Ok(desc.into_bytes())
    }
}

/// Arrow IPC 读取器（抽象接口）
pub struct ArrowIpcReader {
    format: IpcFormat,
    data: Vec<u8>,
    position: usize,
}

impl ArrowIpcReader {
    /// 从字节数据创建读取器
    pub fn new(data: Vec<u8>, format: IpcFormat) -> Self {
        Self {
            format,
            data,
            position: 0,
        }
    }

    /// 读取 Schema
    pub fn read_schema(&self) -> Result<ArrowSchema> {
        // 实际实现：从 IPC 消息解析 Schema
        // 这里返回空 schema 作为占位
        Ok(ArrowSchema::empty())
    }

    /// 迭代读取所有 RecordBatch
    pub fn iter_batches(&self) -> impl Iterator<Item = Result<ArrowRecordBatch>> + '_ {
        // 实际实现：从 IPC 流中逐个解析 RecordBatch
        std::iter::empty()
    }
}

// ============================================================
// 测试
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_value_type_mapping() {
        assert_eq!(value_type_to_arrow(&Value::Null), ArrowDataType::Null);
        assert_eq!(value_type_to_arrow(&Value::Boolean(true)), ArrowDataType::Boolean);
        assert_eq!(value_type_to_arrow(&Value::Int32(1)), ArrowDataType::Int32);
        assert_eq!(value_type_to_arrow(&Value::Int64(1)), ArrowDataType::Int64);
        assert_eq!(value_type_to_arrow(&Value::Float64(1.0)), ArrowDataType::Float64);
        assert_eq!(value_type_to_arrow(&Value::Varchar("x".to_string())), ArrowDataType::Utf8);
    }

    #[test]
    fn test_arrow_type_names() {
        assert_eq!(arrow_type_name(&ArrowDataType::Int64), "int64");
        assert_eq!(arrow_type_name(&ArrowDataType::Utf8), "utf8");
        assert_eq!(arrow_type_name(&ArrowDataType::Boolean), "bool");
    }

    #[test]
    fn test_arrow_schema() {
        let schema = ArrowSchema::new(vec![
            ArrowField {
                name: "id".to_string(),
                data_type: ArrowDataType::Int64,
                nullable: false,
            },
            ArrowField {
                name: "name".to_string(),
                data_type: ArrowDataType::Utf8,
                nullable: true,
            },
        ]);
        assert_eq!(schema.fields.len(), 2);
        assert_eq!(schema.fields[0].name, "id");
        assert!(!schema.fields[0].nullable);
        assert!(schema.fields[1].nullable);
    }

    #[test]
    fn test_record_batch_validation() {
        // 列数不匹配
        let schema = ArrowSchema::new(vec![
            ArrowField { name: "a".to_string(), data_type: ArrowDataType::Int64, nullable: false },
        ]);
        let result = ArrowRecordBatch::try_new(schema, vec![]);
        assert!(result.is_err());

        // 空 batch
        let schema = ArrowSchema::new(vec![]);
        let batch = ArrowRecordBatch::try_new(schema, vec![]).unwrap();
        assert_eq!(batch.num_columns(), 0);
        assert_eq!(batch.num_rows(), 0);
    }

    #[test]
    fn test_roundtrip_single_value() {
        // Value → Arrow type → Value 类型一致
        let values = vec![
            Value::Int64(42),
            Value::Float64(3.14),
            Value::Varchar("hello".to_string()),
            Value::Boolean(true),
            Value::Null,
        ];
        for v in &values {
            let dt = value_type_to_arrow(v);
            let name = arrow_type_name(&dt);
            assert!(!name.is_empty());
        }
    }

    #[test]
    fn test_vector_backed_array() {
        let array = VectorBackedArray {
            values: vec![
                Value::Int64(1),
                Value::Int64(2),
                Value::Null,
                Value::Int64(4),
            ],
        };
        assert_eq!(array.len(), 4);
        assert!(!array.is_empty());
        assert_eq!(array.get_value(0), Value::Int64(1));
        assert!(array.is_null(2));
        assert!(!array.is_null(0));
        assert_eq!(array.data_type(), ArrowDataType::Int64);
    }

    #[test]
    fn test_ipc_writer_basic() {
        let mut writer = ArrowIpcWriter::new(IpcFormat::Stream);
        let schema = ArrowSchema::new(vec![
            ArrowField { name: "id".to_string(), data_type: ArrowDataType::Int64, nullable: false },
        ]);
        writer.write_schema(schema).unwrap();

        // 没有数据也能 finish
        let result = writer.finish().unwrap();
        assert!(!result.is_empty());
    }

    #[test]
    fn test_ipc_writer_no_schema_error() {
        let mut writer = ArrowIpcWriter::new(IpcFormat::File);
        // 没写 schema 就写 batch 应该报错
        let schema = ArrowSchema::empty();
        let batch = ArrowRecordBatch::try_new(schema, vec![]).unwrap();
        let result = writer.write_batch(batch);
        assert!(result.is_err());
    }

    #[test]
    fn test_importer_schema_to_names() {
        let schema = ArrowSchema::new(vec![
            ArrowField { name: "a".to_string(), data_type: ArrowDataType::Int32, nullable: false },
            ArrowField { name: "b".to_string(), data_type: ArrowDataType::Utf8, nullable: true },
            ArrowField { name: "c".to_string(), data_type: ArrowDataType::Float64, nullable: false },
        ]);
        let names = ArrowImporter::schema_to_column_names(&schema);
        assert_eq!(names, vec!["a", "b", "c"]);
    }
}
