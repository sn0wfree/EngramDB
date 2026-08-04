# Implementation Plan: V08 INT8 向量量化

## Step 1: `DataType` add `VectorInt8 { dim: usize }` — `src/common/types.rs`
- `name()` → `"VECTOR_INT8"`
- `fixed_size()` → `None`

## Step 2: `Value` add `VectorInt8(Vec<i8>)` — `src/lib.rs`
- variant_rank → 11
- Ord: compare by length, then element-wise
- Hash: hash length, then each i8
- Display: `"vector_int8[{}]"` with len
- `as_vector()` → also return `Some` for VectorInt8 (for SQL function compatibility)

## Step 3: Quantization core — `src/storage/vector_index.rs`
- `quantize_to_int8(v: &[f32]) -> (Vec<i8>, f32, f32)` — MinMax quantization
- `dequantize_to_f32(q: &[i8], scale: f32, offset: f32) -> Vec<f32>`

## Step 4: HNSW — `src/storage/vector_index.rs`
- `HnswConfig.quantize: bool` (default false)
- `HnswNode.quantized: Vec<i8>`, `scale: f32`, `offset: f32`
- `insert()` — quantize if config.quantize is true
- `distance()` — detect quantized node, dequantize on-the-fly
- `to_bytes()` — new magic `HNSW_IDX2`, per-node quantized flag
- `from_bytes()` — read both HNSW_IDX1 and HNSW_IDX2

## Step 5: Parser — `src/sql/parser.rs`
- `VECTOR_INT8(dim)` → `DataType::VectorInt8 { dim: 0 }`

## Step 6: Table — `src/storage/table.rs`
- `create_vector_index()` accept `DataType::VectorInt8` columns, set `config.quantize = true`
- Row insertion for VectorInt8 values

## Step 7: Tests
- quantize/dequantize round-trip
- HNSW search with quantized index
- Recall comparison