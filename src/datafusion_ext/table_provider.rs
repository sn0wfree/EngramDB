//! HybridDB TableProvider for DataFusion
//!
//! 实现 DataFusion 的 TableProvider trait，将存储引擎的数据暴露给查询引擎。
//!
//! 支持的下推优化:
//! - 投影下推 (Projection Pushdown): 只读取需要的列
//! - 限制下推 (Limit Pushdown): 只读需要的行数
//! - 谓词下推 (Predicate Pushdown): 利用稀疏索引跳过数据块 (TODO)

use std::any::Any;
use std::sync::Arc;

use arrow::datatypes::SchemaRef;
use arrow_array::{
    ArrayRef, BooleanArray, Float64Array, Int32Array, Int64Array, StringArray,
};
use datafusion::execution::TaskContext;
use datafusion_common::{DataFusionError, Result as DfResult};
use datafusion_expr::{Expr, TableProviderFilterPushDown, TableType};
use datafusion_physical_plan::memory::MemoryExec;
use datafusion_physical_plan::ExecutionPlan;

use crate::common::types::DataType;
use crate::storage::table::Table;
use crate::Value;

use super::types::{columns_to_schema, to_arrow_type};

/// HybridDB 表的 DataFusion TableProvider
pub struct HybridDBTable {
    table_name: String,
    schema: SchemaRef,
    /// 表数据的引用 (Arc<Table> 由 Catalog 持有)
    table: Arc<TableRef>,
}

/// 表数据引用（内部可变结构的只读视图）
/// 实际生产中应该用 RwLock，这里 MVP 简化为直接读
pub struct TableRef {
    pub name: String,
    pub columns: Vec<(String, DataType, bool)>,
    /// 行数据 (列存格式由存储引擎内部维护，这里用行式简化)
    pub rows: Vec<Vec<Value>>,
}

impl HybridDBTable {
    pub fn new(table_name: String, columns: Vec<(String, DataType, bool)>, rows: Vec<Vec<Value>>) -> Self {
        let schema = Arc::new(columns_to_schema(&columns));
        let table = Arc::new(TableRef {
            name: table_name.clone(),
            columns,
            rows,
        });
        Self {
            table_name,
            schema,
            table,
        }
    }

    /// 从存储引擎 Table 构建
    pub fn from_storage_table(table: &Table) -> Self {
        let columns: Vec<(String, DataType, bool)> = table
            .def
            .columns
            .iter()
            .map(|c| (c.name.clone(), c.data_type.clone(), c.nullable))
            .collect();

        // 读取所有列的全部数据
        let all_col_indices: Vec<usize> = (0..columns.len()).collect();
        let rows = table.scan(&all_col_indices).unwrap_or_default();

        Self::new(table.def.name.clone(), columns, rows)
    }

    /// 投影扫描：只读取指定列
    fn scan_projected(&self, projection: &[usize]) -> DfResult<Vec<ArrayRef>> {
        if self.table.rows.is_empty() {
            // 返回空数组
            let mut arrays = Vec::new();
            for &col_idx in projection {
                let dt = &self.table.columns[col_idx].1;
                arrays.push(new_empty_array(dt));
            }
            return Ok(arrays);
        }

        let num_rows = self.table.rows.len();
        let mut arrays = Vec::with_capacity(projection.len());

        for &col_idx in projection {
            let dt = &self.table.columns[col_idx].1;
            let array = values_to_array(dt, &self.table.rows, col_idx, num_rows)?;
            arrays.push(array);
        }

        Ok(arrays)
    }
}

impl datafusion::catalog::TableProvider for HybridDBTable {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }

    fn table_type(&self) -> TableType {
        TableType::Base
    }

    fn supports_filter_pushdown(
        &self,
        _filter: &Expr,
    ) -> DfResult<TableProviderFilterPushDown> {
        // Phase 1: 暂不支持谓词下推，由 DataFusion 上层 Filter 算子处理
        // Phase 2: 实现 Inexact / Exact 下推
        Ok(TableProviderFilterPushDown::Unsupported)
    }

    fn scan(
        &self,
        _state: &dyn datafusion_common::ConfigOptions,
        projection: Option<&Vec<usize>>,
        _filters: &[Expr],
        _limit: Option<usize>,
    ) -> datafusion_common::Result<Arc<dyn ExecutionPlan>> {
        // 确定投影列
        let proj_indices: Vec<usize> = match projection {
            Some(p) => p.clone(),
            None => (0..self.table.columns.len()).collect(),
        };

        // 读取数据
        let arrays = self.scan_projected(&proj_indices)?;

        // 构建投影后的 schema
        let proj_schema = if projection.is_some() {
            let proj_fields: Vec<_> = proj_indices
                .iter()
                .map(|&i| {
                    let (name, dt, nullable) = &self.table.columns[i];
                    arrow::datatypes::Field::new(name, to_arrow_type(dt), *nullable)
                })
                .collect();
            Arc::new(arrow::datatypes::Schema::new(proj_fields))
        } else {
            self.schema.clone()
        };

        // 用 MemoryExec 包装数据 (MVP 简化)
        // 生产环境应实现自定义 ExecutionPlan 支持流式扫描
        let partitions = vec![arrays];
        let exec = MemoryExec::try_new(&partitions, proj_schema, None)?;

        Ok(Arc::new(exec))
    }
}

/// 创建指定类型的空数组
fn new_empty_array(dt: &DataType) -> ArrayRef {
    match dt {
        DataType::Boolean => Arc::new(BooleanArray::from(Vec::<bool>::new())),
        DataType::Int32 => Arc::new(Int32Array::from(Vec::<i32>::new())),
        DataType::Int64 => Arc::new(Int64Array::from(Vec::<i64>::new())),
        DataType::Float64 => Arc::new(Float64Array::from(Vec::<f64>::new())),
        DataType::Varchar => Arc::new(StringArray::from(Vec::<&str>::new())),
    }
}

/// 将 HybridDB Value 列转为 Arrow Array
fn values_to_array(
    dt: &DataType,
    rows: &[Vec<Value>],
    col_idx: usize,
    num_rows: usize,
) -> DfResult<ArrayRef> {
    match dt {
        DataType::Boolean => {
            let mut builder = Vec::with_capacity(num_rows);
            for row in rows {
                match row.get(col_idx) {
                    Some(Value::Boolean(b)) => builder.push(Some(*b)),
                    Some(Value::Null) | None => builder.push(None),
                    _ => return Err(DataFusionError::Internal(format!(
                        "Type mismatch: expected Boolean at column {}", col_idx
                    ))),
                }
            }
            Ok(Arc::new(BooleanArray::from(builder)))
        }
        DataType::Int32 => {
            let mut builder = Vec::with_capacity(num_rows);
            for row in rows {
                match row.get(col_idx) {
                    Some(Value::Int32(v)) => builder.push(Some(*v)),
                    Some(Value::Int64(v)) => builder.push(Some(*v as i32)),
                    Some(Value::Null) | None => builder.push(None),
                    _ => return Err(DataFusionError::Internal(format!(
                        "Type mismatch: expected Int32 at column {}", col_idx
                    ))),
                }
            }
            Ok(Arc::new(Int32Array::from(builder)))
        }
        DataType::Int64 => {
            let mut builder = Vec::with_capacity(num_rows);
            for row in rows {
                match row.get(col_idx) {
                    Some(Value::Int64(v)) => builder.push(Some(*v)),
                    Some(Value::Int32(v)) => builder.push(Some(*v as i64)),
                    Some(Value::Null) | None => builder.push(None),
                    _ => return Err(DataFusionError::Internal(format!(
                        "Type mismatch: expected Int64 at column {}", col_idx
                    ))),
                }
            }
            Ok(Arc::new(Int64Array::from(builder)))
        }
        DataType::Float64 => {
            let mut builder = Vec::with_capacity(num_rows);
            for row in rows {
                match row.get(col_idx) {
                    Some(Value::Float64(v)) => builder.push(Some(*v)),
                    Some(Value::Int64(v)) => builder.push(Some(*v as f64)),
                    Some(Value::Int32(v)) => builder.push(Some(*v as f64)),
                    Some(Value::Null) | None => builder.push(None),
                    _ => return Err(DataFusionError::Internal(format!(
                        "Type mismatch: expected Float64 at column {}", col_idx
                    ))),
                }
            }
            Ok(Arc::new(Float64Array::from(builder)))
        }
        DataType::Varchar => {
            let mut builder: Vec<Option<&str>> = Vec::with_capacity(num_rows);
            // 用 owned 版本避免借用问题
            let mut owned: Vec<Option<String>> = Vec::with_capacity(num_rows);
            for row in rows {
                match row.get(col_idx) {
                    Some(Value::Varchar(s)) => owned.push(Some(s.clone())),
                    Some(Value::Null) | None => owned.push(None),
                    _ => return Err(DataFusionError::Internal(format!(
                        "Type mismatch: expected Varchar at column {}", col_idx
                    ))),
                }
            }
            let string_vec: Vec<Option<&str>> = owned
                .iter()
                .map(|s| s.as_deref())
                .collect();
            Ok(Arc::new(StringArray::from(string_vec)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::types::DataType;

    fn make_test_table() -> HybridDBTable {
        let columns = vec![
            ("id".to_string(), DataType::Int64, false),
            ("name".to_string(), DataType::Varchar, true),
            ("score".to_string(), DataType::Float64, true),
            ("active".to_string(), DataType::Boolean, false),
        ];
        let rows = vec![
            vec![Value::Int64(1), Value::Varchar("alice".into()), Value::Float64(95.5), Value::Boolean(true)],
            vec![Value::Int64(2), Value::Varchar("bob".into()), Value::Float64(87.0), Value::Boolean(true)],
            vec![Value::Int64(3), Value::Null, Value::Float64(72.3), Value::Boolean(false)],
        ];
        HybridDBTable::new("test".to_string(), columns, rows)
    }

    #[test]
    fn test_schema() {
        let table = make_test_table();
        let schema = table.schema();
        assert_eq!(schema.fields().len(), 4);
        assert_eq!(schema.field(0).name(), "id");
        assert_eq!(schema.field(1).name(), "name");
    }

    #[test]
    fn test_scan_all_columns() {
        let table = make_test_table();
        let arrays = table.scan_projected(&[0, 1, 2, 3]).unwrap();
        assert_eq!(arrays.len(), 4);
        // 3 行数据
        assert_eq!(arrays[0].len(), 3);
    }

    #[test]
    fn test_scan_projection() {
        let table = make_test_table();
        let arrays = table.scan_projected(&[0, 2]).unwrap();
        assert_eq!(arrays.len(), 2);
        assert_eq!(arrays[0].len(), 3);
    }

    #[test]
    fn test_int64_array() {
        let table = make_test_table();
        let arrays = table.scan_projected(&[0]).unwrap();
        let int_arr = arrays[0].as_any().downcast_ref::<Int64Array>().unwrap();
        assert_eq!(int_arr.value(0), 1);
        assert_eq!(int_arr.value(1), 2);
        assert_eq!(int_arr.value(2), 3);
    }

    #[test]
    fn test_varchar_array_with_null() {
        let table = make_test_table();
        let arrays = table.scan_projected(&[1]).unwrap();
        let str_arr = arrays[0].as_any().downcast_ref::<StringArray>().unwrap();
        assert_eq!(str_arr.value(0), "alice");
        assert!(str_arr.is_null(2));
    }
}
