//! WAL (Write-Ahead Log) 模块
//!
//! 完整的 WAL 实现，支持：
//! - CRC32 校验（检测部分写入/损坏）
//! - 页对齐写入（4KB 页，部分页填充零）
//! - 顺序追加写入 + fsync 持久化
//! - LSN 单调递增
//! - 崩溃恢复：Redo + Undo

pub mod writer;
pub mod reader;
pub mod recovery;

pub use writer::WalWriter;
pub use reader::WalReader;

use crate::common::error::Result;
use crate::Value;

/// WAL 页大小（4KB，与数据库页对齐）
pub const WAL_PAGE_SIZE: u32 = 4096;

/// WAL 记录头部大小：magic(2) + type(1) + txn_id(4) + table_id(4) + payload_len(4) + crc32(4) = 19 字节
/// 注意：LSN 不在记录内，由文件偏移隐式确定
pub const WAL_RECORD_HEADER_SIZE: usize = 19;

/// WAL 记录魔数（用于检测记录边界）
pub const WAL_MAGIC: u16 = 0x5741; // "WA"

/// WAL 记录类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum WalRecordType {
    Insert = 1,
    Update = 2,
    Delete = 3,
    Commit = 4,
    Rollback = 5,
    Checkpoint = 6,
    Begin = 7,
    /// 补偿记录（用于回滚时的反向操作）
    Compensation = 8,
}

impl WalRecordType {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(WalRecordType::Insert),
            2 => Some(WalRecordType::Update),
            3 => Some(WalRecordType::Delete),
            4 => Some(WalRecordType::Commit),
            5 => Some(WalRecordType::Rollback),
            6 => Some(WalRecordType::Checkpoint),
            7 => Some(WalRecordType::Begin),
            8 => Some(WalRecordType::Compensation),
            _ => None,
        }
    }
}

/// WAL 记录
#[derive(Debug, Clone)]
pub struct WalRecord {
    /// 日志序列号（= 文件偏移量）
    pub lsn: u64,
    pub record_type: WalRecordType,
    pub txn_id: u32,
    pub table_id: u32,
    pub payload: Vec<u8>,
}

impl WalRecord {
    /// 记录总大小（含头部）
    pub fn total_size(&self) -> usize {
        WAL_RECORD_HEADER_SIZE + self.payload.len()
    }

    /// 序列化（不含 LSN，LSN 由文件位置隐式确定）
    /// 格式：[magic:2][type:1][txn_id:4][table_id:4][payload_len:4][payload:N][crc32:4]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(self.total_size());

        // magic
        buf.extend_from_slice(&WAL_MAGIC.to_le_bytes());
        // type
        buf.push(self.record_type as u8);
        // txn_id
        buf.extend_from_slice(&self.txn_id.to_le_bytes());
        // table_id
        buf.extend_from_slice(&self.table_id.to_le_bytes());
        // payload_len
        buf.extend_from_slice(&(self.payload.len() as u32).to_le_bytes());
        // payload
        buf.extend_from_slice(&self.payload);

        // crc32 (覆盖 magic 到 payload 的全部内容)
        let crc = crc32(&buf);
        buf.extend_from_slice(&crc.to_le_bytes());

        buf
    }

    /// 从字节切片反序列化
    pub fn from_bytes(data: &[u8]) -> Option<Self> {
        if data.len() < WAL_RECORD_HEADER_SIZE {
            return None;
        }

        // 校验 magic
        let magic = u16::from_le_bytes(data[0..2].try_into().unwrap());
        if magic != WAL_MAGIC {
            return None;
        }

        let record_type = WalRecordType::from_u8(data[2])?;
        let txn_id = u32::from_le_bytes(data[3..7].try_into().unwrap());
        let table_id = u32::from_le_bytes(data[7..11].try_into().unwrap());
        let payload_len = u32::from_le_bytes(data[11..15].try_into().unwrap()) as usize;

        let total_len = WAL_RECORD_HEADER_SIZE + payload_len;
        if data.len() < total_len {
            return None;
        }

        // 校验 CRC32
        let stored_crc = u32::from_le_bytes(data[15 + payload_len..19 + payload_len].try_into().unwrap());
        let computed_crc = crc32(&data[..15 + payload_len]);
        if stored_crc != computed_crc {
            return None; // CRC 不匹配，记录损坏
        }

        let payload = data[15..15 + payload_len].to_vec();

        Some(Self {
            lsn: 0, // 由调用者设置
            record_type,
            txn_id,
            table_id,
            payload,
        })
    }
}

/// 简单 CRC32 实现（避免外部依赖）
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFFFFFF;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            if crc & 1 == 1 {
                crc = (crc >> 1) ^ 0xEDB88320;
            } else {
                crc >>= 1;
            }
        }
    }
    !crc
}

// ============================================================================
// Payload 格式
// ============================================================================

/// INSERT payload: [rowid:8][num_cols:4][col_data...]
pub fn make_insert_payload(rowid: u64, row: &[Value]) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&rowid.to_le_bytes());
    buf.extend_from_slice(&(row.len() as u32).to_le_bytes());
    for val in row {
        serialize_value(val, &mut buf);
    }
    buf
}

/// 解析 INSERT payload
pub fn parse_insert_payload(data: &[u8]) -> Option<(u64, Vec<Value>)> {
    if data.len() < 12 {
        return None;
    }
    let rowid = u64::from_le_bytes(data[0..8].try_into().unwrap());
    let num_cols = u32::from_le_bytes(data[8..12].try_into().unwrap()) as usize;

    let mut offset = 12;
    let mut row = Vec::with_capacity(num_cols);
    for _ in 0..num_cols {
        let (val, consumed) = deserialize_value(&data[offset..])?;
        row.push(val);
        offset += consumed;
    }

    Some((rowid, row))
}

/// UPDATE payload: [rowid:8][num_cols:4][old_col_data...][new_col_data...]
pub fn make_update_payload(rowid: u64, old_row: &[Value], new_row: &[Value]) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&rowid.to_le_bytes());
    buf.extend_from_slice(&(old_row.len() as u32).to_le_bytes());
    for val in old_row {
        serialize_value(val, &mut buf);
    }
    for val in new_row {
        serialize_value(val, &mut buf);
    }
    buf
}

/// DELETE payload: [rowid:8][num_cols:4][col_data...]
pub fn make_delete_payload(rowid: u64, old_row: &[Value]) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&rowid.to_le_bytes());
    buf.extend_from_slice(&(old_row.len() as u32).to_le_bytes());
    for val in old_row {
        serialize_value(val, &mut buf);
    }
    buf
}

/// CHECKPOINT payload: [checkpoint_lsn:8]
pub fn make_checkpoint_payload(checkpoint_lsn: u64) -> Vec<u8> {
    checkpoint_lsn.to_le_bytes().to_vec()
}

/// 解析 UPDATE payload
pub fn parse_update_payload(data: &[u8]) -> Option<(u64, Vec<Value>, Vec<Value>)> {
    if data.len() < 12 {
        return None;
    }
    let rowid = u64::from_le_bytes(data[0..8].try_into().unwrap());
    let num_cols = u32::from_le_bytes(data[8..12].try_into().unwrap()) as usize;

    let mut offset = 12;
    let mut old_row = Vec::with_capacity(num_cols);
    for _ in 0..num_cols {
        let (val, consumed) = deserialize_value(&data[offset..])?;
        old_row.push(val);
        offset += consumed;
    }

    let mut new_row = Vec::with_capacity(num_cols);
    for _ in 0..num_cols {
        let (val, consumed) = deserialize_value(&data[offset..])?;
        new_row.push(val);
        offset += consumed;
    }

    Some((rowid, old_row, new_row))
}

/// 解析 DELETE payload
pub fn parse_delete_payload(data: &[u8]) -> Option<(u64, Vec<Value>)> {
    if data.len() < 12 {
        return None;
    }
    let rowid = u64::from_le_bytes(data[0..8].try_into().unwrap());
    let num_cols = u32::from_le_bytes(data[8..12].try_into().unwrap()) as usize;

    let mut offset = 12;
    let mut row = Vec::with_capacity(num_cols);
    for _ in 0..num_cols {
        let (val, consumed) = deserialize_value(&data[offset..])?;
        row.push(val);
        offset += consumed;
    }

    Some((rowid, row))
}

/// 解析 CHECKPOINT payload
pub fn parse_checkpoint_payload(data: &[u8]) -> Option<u64> {
    if data.len() < 8 {
        return None;
    }
    Some(u64::from_le_bytes(data[0..8].try_into().unwrap()))
}

/// Value 编码（测试用包装）
pub fn encode_value(val: &Value) -> Vec<u8> {
    let mut buf = Vec::new();
    serialize_value(val, &mut buf);
    buf
}

/// Value 解码（测试用包装）
pub fn decode_value(data: &[u8]) -> Option<(Value, usize)> {
    deserialize_value(data)
}

// ============================================================================
// Value 序列化
// ============================================================================

fn serialize_value(val: &Value, buf: &mut Vec<u8>) {
    match val {
        Value::Null => {
            buf.push(0);
        }
        Value::Boolean(b) => {
            buf.push(1);
            buf.push(if *b { 1 } else { 0 });
        }
        Value::Int32(i) => {
            buf.push(2);
            buf.extend_from_slice(&i.to_le_bytes());
        }
        Value::Int64(i) => {
            buf.push(3);
            buf.extend_from_slice(&i.to_le_bytes());
        }
        Value::Float64(f) => {
            buf.push(4);
            buf.extend_from_slice(&f.to_le_bytes());
        }
        Value::Varchar(s) => {
            buf.push(5);
            buf.extend_from_slice(&(s.len() as u32).to_le_bytes());
            buf.extend_from_slice(s.as_bytes());
        }
        Value::Json(s) => {
            buf.push(6);
            buf.extend_from_slice(&(s.len() as u32).to_le_bytes());
            buf.extend_from_slice(s.as_bytes());
        }
        Value::Vector(v) => {
            buf.push(7);
            buf.extend_from_slice(&(v.len() as u32).to_le_bytes());
            for f in v {
                buf.extend_from_slice(&f.to_le_bytes());
            }
        }
        Value::Blob(b) => {
            buf.push(8);
            buf.extend_from_slice(&(b.len() as u32).to_le_bytes());
            buf.extend_from_slice(b);
        }
        Value::Float32(f) => {
            buf.push(9);
            buf.extend_from_slice(&f.to_le_bytes());
        }
        Value::Timestamp(t) => {
            buf.push(10);
            buf.extend_from_slice(&t.to_le_bytes());
        }
        Value::VectorInt8(v) => {
            buf.push(11);
            buf.extend_from_slice(&(v.len() as u32).to_le_bytes());
            for b in v {
                buf.push(*b as u8);
            }
        }
    }
}

/// 反序列化一个 Value，返回 (value, bytes_consumed)
fn deserialize_value(data: &[u8]) -> Option<(Value, usize)> {
    if data.is_empty() {
        return None;
    }

    match data[0] {
        0 => Some((Value::Null, 1)),
        1 => {
            if data.len() < 2 { return None; }
            Some((Value::Boolean(data[1] != 0), 2))
        }
        2 => {
            if data.len() < 5 { return None; }
            let v = i32::from_le_bytes(data[1..5].try_into().unwrap());
            Some((Value::Int32(v), 5))
        }
        3 => {
            if data.len() < 9 { return None; }
            let v = i64::from_le_bytes(data[1..9].try_into().unwrap());
            Some((Value::Int64(v), 9))
        }
        4 => {
            if data.len() < 9 { return None; }
            let v = f64::from_le_bytes(data[1..9].try_into().unwrap());
            Some((Value::Float64(v), 9))
        }
        9 => {
            if data.len() < 5 { return None; }
            let v = f32::from_le_bytes(data[1..5].try_into().unwrap());
            Some((Value::Float32(v), 5))
        }
        10 => {
            if data.len() < 9 { return None; }
            let v = i64::from_le_bytes(data[1..9].try_into().unwrap());
            Some((Value::Timestamp(v), 9))
        }
        11 => {
            if data.len() < 5 { return None; }
            let dim = u32::from_le_bytes(data[1..5].try_into().unwrap()) as usize;
            let byte_len = dim;
            if 5 + byte_len > data.len() { return None; }
            let mut vec = Vec::with_capacity(dim);
            for i in 0..dim {
                vec.push(data[5 + i] as i8);
            }
            Some((Value::VectorInt8(vec), 5 + byte_len))
        }
        5 => {
            if data.len() < 5 { return None; }
            let len = u32::from_le_bytes(data[1..5].try_into().unwrap()) as usize;
            if data.len() < 5 + len { return None; }
            let s = String::from_utf8_lossy(&data[5..5 + len]).to_string();
            Some((Value::Varchar(s), 5 + len))
        }
        6 => {
            if data.len() < 5 { return None; }
            let len = u32::from_le_bytes(data[1..5].try_into().unwrap()) as usize;
            if data.len() < 5 + len { return None; }
            let s = String::from_utf8_lossy(&data[5..5 + len]).to_string();
            Some((Value::Json(s), 5 + len))
        }
        7 => {
            if data.len() < 5 { return None; }
            let dim = u32::from_le_bytes(data[1..5].try_into().unwrap()) as usize;
            let byte_len = dim * 4;
            if data.len() < 5 + byte_len { return None; }
            let mut vec = Vec::with_capacity(dim);
            for i in 0..dim {
                let start = 5 + i * 4;
                let f = f32::from_le_bytes(data[start..start + 4].try_into().unwrap());
                vec.push(f);
            }
            Some((Value::Vector(vec), 5 + byte_len))
        }
        8 => {
            if data.len() < 5 { return None; }
            let len = u32::from_le_bytes(data[1..5].try_into().unwrap()) as usize;
            if data.len() < 5 + len { return None; }
            Some((Value::Blob(data[5..5 + len].to_vec()), 5 + len))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- CRC32 ---

    #[test]
    fn test_crc32_empty() {
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn test_crc32_known_value() {
        // 标准测试向量 "123456789"
        assert_eq!(crc32(b"123456789"), 0xCBF43926);
    }

    #[test]
    fn test_crc32_single_byte() {
        // 单字节 CRC
        let c = crc32(b"a");
        assert_ne!(c, 0);
        // 相同输入相同输出
        assert_eq!(c, crc32(b"a"));
    }

    #[test]
    fn test_crc32_different_inputs_different_outputs() {
        assert_ne!(crc32(b"hello"), crc32(b"world"));
    }

    #[test]
    fn test_crc32_long_data() {
        let data: Vec<u8> = (0..1000).map(|i| (i % 256) as u8).collect();
        let c1 = crc32(&data);
        let c2 = crc32(&data);
        assert_eq!(c1, c2);
        assert_ne!(c1, 0);
    }

    // --- 记录序列化 ---

    #[test]
    fn test_record_serialization_basic() {
        let record = WalRecord {
            lsn: 0,
            record_type: WalRecordType::Insert,
            txn_id: 42,
            table_id: 1,
            payload: vec![1, 2, 3, 4, 5],
        };

        let bytes = record.to_bytes();
        let parsed = WalRecord::from_bytes(&bytes).unwrap();

        assert_eq!(parsed.record_type, WalRecordType::Insert);
        assert_eq!(parsed.txn_id, 42);
        assert_eq!(parsed.table_id, 1);
        assert_eq!(parsed.payload, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn test_record_serialization_empty_payload() {
        let record = WalRecord {
            lsn: 0,
            record_type: WalRecordType::Begin,
            txn_id: 100,
            table_id: 0,
            payload: vec![],
        };

        let bytes = record.to_bytes();
        // 19 字节头部 + 0 负载 = 19 字节
        assert_eq!(bytes.len(), 19);
        let parsed = WalRecord::from_bytes(&bytes).unwrap();
        assert_eq!(parsed.record_type, WalRecordType::Begin);
        assert_eq!(parsed.txn_id, 100);
        assert!(parsed.payload.is_empty());
    }

    #[test]
    fn test_record_serialization_large_payload() {
        let payload: Vec<u8> = (0..10000).map(|i| (i % 256) as u8).collect();
        let record = WalRecord {
            lsn: 0,
            record_type: WalRecordType::Insert,
            txn_id: 1,
            table_id: 1,
            payload: payload.clone(),
        };

        let bytes = record.to_bytes();
        let parsed = WalRecord::from_bytes(&bytes).unwrap();
        assert_eq!(parsed.payload, payload);
    }

    #[test]
    fn test_record_all_types() {
        let types = vec![
            WalRecordType::Begin,
            WalRecordType::Commit,
            WalRecordType::Rollback,
            WalRecordType::Insert,
            WalRecordType::Update,
            WalRecordType::Delete,
            WalRecordType::Checkpoint,
            WalRecordType::Compensation,
        ];

        for t in types {
            let rec = WalRecord {
                lsn: 0,
                record_type: t,
                txn_id: 7,
                table_id: 3,
                payload: vec![10, 20],
            };
            let bytes = rec.to_bytes();
            let parsed = WalRecord::from_bytes(&bytes).unwrap();
            assert_eq!(parsed.record_type, t);
        }
    }

    #[test]
    fn test_record_crc_corruption_payload() {
        let record = WalRecord {
            lsn: 0,
            record_type: WalRecordType::Insert,
            txn_id: 1,
            table_id: 1,
            payload: vec![10, 20, 30],
        };

        let mut bytes = record.to_bytes();
        bytes[15] = 99; // 篡改 payload
        assert!(WalRecord::from_bytes(&bytes).is_none());
    }

    #[test]
    fn test_record_crc_corruption_header() {
        let record = WalRecord {
            lsn: 0,
            record_type: WalRecordType::Insert,
            txn_id: 1,
            table_id: 1,
            payload: vec![10, 20, 30],
        };

        let mut bytes = record.to_bytes();
        bytes[2] = 0xFF; // 篡改 record_type
        assert!(WalRecord::from_bytes(&bytes).is_none());
    }

    #[test]
    fn test_record_crc_corruption_txn_id() {
        let record = WalRecord {
            lsn: 0,
            record_type: WalRecordType::Insert,
            txn_id: 1,
            table_id: 1,
            payload: vec![10, 20, 30],
        };

        let mut bytes = record.to_bytes();
        bytes[3] ^= 0xAA; // 篡改 txn_id
        assert!(WalRecord::from_bytes(&bytes).is_none());
    }

    #[test]
    fn test_record_truncated() {
        let record = WalRecord {
            lsn: 0,
            record_type: WalRecordType::Insert,
            txn_id: 1,
            table_id: 1,
            payload: vec![1, 2, 3],
        };

        let bytes = record.to_bytes();
        // 截断到只有 magic + type
        assert!(WalRecord::from_bytes(&bytes[..3]).is_none());
        // 截断到头部中间
        assert!(WalRecord::from_bytes(&bytes[..10]).is_none());
        // 截断到刚好头部（缺 payload + CRC）
        assert!(WalRecord::from_bytes(&bytes[..15]).is_none());
    }

    // --- Insert payload ---

    #[test]
    fn test_insert_payload_mixed_types() {
        let row = vec![
            Value::Int64(1),
            Value::Varchar("hello".into()),
            Value::Float64(3.14),
        ];
        let payload = make_insert_payload(100, &row);
        let (rowid, parsed) = parse_insert_payload(&payload).unwrap();
        assert_eq!(rowid, 100);
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0], Value::Int64(1));
        assert_eq!(parsed[1], Value::Varchar("hello".into()));
        match &parsed[2] {
            Value::Float64(f) => assert!((f - 3.14).abs() < 0.001),
            _ => panic!("wrong type"),
        }
    }

    #[test]
    fn test_insert_payload_empty_row() {
        let payload = make_insert_payload(1, &[]);
        let (rowid, parsed) = parse_insert_payload(&payload).unwrap();
        assert_eq!(rowid, 1);
        assert!(parsed.is_empty());
    }

    #[test]
    fn test_insert_payload_boolean() {
        let row = vec![Value::Boolean(true), Value::Boolean(false)];
        let payload = make_insert_payload(42, &row);
        let (rowid, parsed) = parse_insert_payload(&payload).unwrap();
        assert_eq!(rowid, 42);
        assert_eq!(parsed[0], Value::Boolean(true));
        assert_eq!(parsed[1], Value::Boolean(false));
    }

    #[test]
    fn test_insert_payload_large_varchar() {
        let s = "x".repeat(10000);
        let row = vec![Value::Varchar(s.clone())];
        let payload = make_insert_payload(1, &row);
        let (_, parsed) = parse_insert_payload(&payload).unwrap();
        assert_eq!(parsed[0], Value::Varchar(s));
    }

    #[test]
    fn test_insert_payload_invalid_data() {
        // 随机垃圾数据应该解析失败
        let garbage = vec![0xFF, 0xFE, 0xFD, 0xFC, 0xFB];
        assert!(parse_insert_payload(&garbage).is_none());
    }

    // --- Update payload ---

    #[test]
    fn test_update_payload_roundtrip() {
        let old_row = vec![Value::Int64(1), Value::Varchar("old".into())];
        let new_row = vec![Value::Int64(1), Value::Varchar("new".into())];
        let payload = make_update_payload(10, &old_row, &new_row);
        let (rowid, parsed_old, parsed_new) = parse_update_payload(&payload).unwrap();
        assert_eq!(rowid, 10);
        assert_eq!(parsed_old, old_row);
        assert_eq!(parsed_new, new_row);
    }

    // --- Delete payload ---

    #[test]
    fn test_delete_payload_roundtrip() {
        let old_row = vec![Value::Int64(99), Value::Boolean(true)];
        let payload = make_delete_payload(5, &old_row);
        let (rowid, parsed) = parse_delete_payload(&payload).unwrap();
        assert_eq!(rowid, 5);
        assert_eq!(parsed, old_row);
    }

    // --- Checkpoint payload ---

    #[test]
    fn test_checkpoint_payload() {
        let lsn = 123456789;
        let payload = make_checkpoint_payload(lsn);
        let parsed = parse_checkpoint_payload(&payload).unwrap();
        assert_eq!(parsed, lsn);
    }

    #[test]
    fn test_checkpoint_payload_zero() {
        let payload = make_checkpoint_payload(0);
        assert_eq!(parse_checkpoint_payload(&payload).unwrap(), 0);
    }

    #[test]
    fn test_checkpoint_payload_max() {
        let payload = make_checkpoint_payload(u64::MAX);
        assert_eq!(parse_checkpoint_payload(&payload).unwrap(), u64::MAX);
    }

    // --- Value 编码 ---

    #[test]
    fn test_encode_decode_int64() {
        let v = Value::Int64(-123456789);
        let encoded = encode_value(&v);
        let (decoded, _) = decode_value(&encoded).unwrap();
        assert_eq!(decoded, v);
    }

    #[test]
    fn test_encode_decode_float64() {
        let v = Value::Float64(-0.001);
        let encoded = encode_value(&v);
        let (decoded, _) = decode_value(&encoded).unwrap();
        match (decoded, &v) {
            (Value::Float64(a), Value::Float64(b)) => assert_eq!(a, *b),
            _ => panic!("type mismatch"),
        }
    }

    #[test]
    fn test_encode_decode_boolean_true() {
        let v = Value::Boolean(true);
        let encoded = encode_value(&v);
        let (decoded, _) = decode_value(&encoded).unwrap();
        assert_eq!(decoded, v);
    }

    #[test]
    fn test_encode_decode_boolean_false() {
        let v = Value::Boolean(false);
        let encoded = encode_value(&v);
        let (decoded, _) = decode_value(&encoded).unwrap();
        assert_eq!(decoded, v);
    }

    #[test]
    fn test_encode_decode_empty_varchar() {
        let v = Value::Varchar(String::new());
        let encoded = encode_value(&v);
        let (decoded, _) = decode_value(&encoded).unwrap();
        assert_eq!(decoded, v);
    }

    #[test]
    fn test_encode_decode_unicode_varchar() {
        let v = Value::Varchar("你好世界 🌍".to_string());
        let encoded = encode_value(&v);
        let (decoded, _) = decode_value(&encoded).unwrap();
        assert_eq!(decoded, v);
    }

    #[test]
    fn test_decode_value_truncated() {
        // 截断的 int64（type=3，需要 9 字节，只给 3 字节）
        assert!(decode_value(&[3, 1, 2]).is_none());
        // 截断的 varchar（有长度但数据不够）
        let mut data = vec![5u8, 10, 0, 0, 0]; // type=Varchar, len=10
        assert!(decode_value(&data).is_none());
    }

    #[test]
    fn test_record_total_size() {
        let rec = WalRecord {
            lsn: 0,
            record_type: WalRecordType::Insert,
            txn_id: 1,
            table_id: 1,
            payload: vec![0; 100],
        };
        // magic(2) + type(1) + txn_id(4) + table_id(4) + payload_len(4) + payload(100) + crc(4)
        assert_eq!(rec.total_size(), 2 + 1 + 4 + 4 + 4 + 100 + 4);
    }
}
