//! WAL 读取器
//!
//! 支持：
//! - 顺序读取所有有效记录
//! - CRC 校验自动跳过损坏记录（部分写入）
//! - 按 LSN 定位
//! - 迭代器模式

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

use crate::common::error::Result;

use super::{WalRecord, WAL_MAGIC, WAL_RECORD_HEADER_SIZE};

/// WAL 读取器
pub struct WalReader {
    #[allow(dead_code)]
    path: PathBuf,
    file: File,
    /// 当前读取位置
    position: u64,
}

impl WalReader {
    pub fn open(path: &str) -> Result<Self> {
        let path = PathBuf::from(path);
        let file = File::open(&path)?;
        Ok(Self {
            path,
            file,
            position: 0,
        })
    }

    /// 读取下一条有效记录
    /// 返回 Ok(None) 表示到达文件末尾
    pub fn read_next(&mut self) -> Result<Option<WalRecord>> {
        loop {
            // 读取头部
            let mut header_buf = [0u8; 15]; // magic(2) + type(1) + txn_id(4) + table_id(4) + payload_len(4)
            match self.file.read(&mut header_buf) {
                Ok(0) => return Ok(None), // EOF
                Ok(n) if n < 15 => {
                    // 不完整的头部，到达文件末尾
                    self.position += n as u64;
                    return Ok(None);
                }
                Ok(_) => {}
                Err(e) => return Err(e.into()),
            }

            // 检查 magic
            let magic = u16::from_le_bytes(header_buf[0..2].try_into().unwrap());
            if magic != WAL_MAGIC {
                // magic 不匹配，可能是部分写入或损坏
                // 尝试向前搜索下一个 magic
                self.position += 1;
                self.file.seek(SeekFrom::Start(self.position))?;
                continue;
            }

            // 解析 payload 长度
            let payload_len = u32::from_le_bytes(header_buf[11..15].try_into().unwrap()) as usize;
            let total_size = WAL_RECORD_HEADER_SIZE + payload_len;

            // 读取剩余部分（payload + crc）
            let mut rest = vec![0u8; total_size - 15];
            match self.file.read_exact(&mut rest) {
                Ok(_) => {}
                Err(_) => {
                    // 不完整的记录，到达文件末尾
                    self.position += 15;
                    return Ok(None);
                }
            }

            // 组装完整记录
            let mut full_record = header_buf.to_vec();
            full_record.extend_from_slice(&rest);

            // 解析（含 CRC 校验）
            match WalRecord::from_bytes(&full_record) {
                Some(mut rec) => {
                    rec.lsn = self.position;
                    self.position += total_size as u64;
                    return Ok(Some(rec));
                }
                None => {
                    // CRC 校验失败，跳过 1 字节继续找
                    self.position += 1;
                    self.file.seek(SeekFrom::Start(self.position))?;
                    continue;
                }
            }
        }
    }

    /// 读取所有有效记录
    pub fn read_all(&mut self) -> Result<Vec<WalRecord>> {
        let mut records = Vec::new();
        loop {
            match self.read_next()? {
                Some(rec) => records.push(rec),
                None => break,
            }
        }
        Ok(records)
    }

    /// 重置到文件开头
    pub fn reset(&mut self) -> Result<()> {
        self.file.seek(SeekFrom::Start(0))?;
        self.position = 0;
        Ok(())
    }

    /// 定位到指定 LSN
    pub fn seek_to(&mut self, lsn: u64) -> Result<()> {
        self.file.seek(SeekFrom::Start(lsn))?;
        self.position = lsn;
        Ok(())
    }

    /// 当前位置
    pub fn position(&self) -> u64 {
        self.position
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wal::{WalWriter, WalRecordType};

    fn tmp(name: &str) -> String {
        let mut p = std::env::temp_dir();
        let tid = format!("{:?}", std::thread::current().id())
            .replace('(', "_").replace(')', "")
            .replace([':', ' '], "_");
        p.push(format!("engramdb_wal_{}_{}_{}.hdb-wal", name, std::process::id(), tid));
        p.to_string_lossy().to_string()
    }

    #[test]
    fn test_reader_basic() {
        let tmp = tmp("reader_basic");
        let _ = std::fs::remove_file(&tmp);

        // 写入
        {
            let mut writer = WalWriter::open(&tmp).unwrap();
            writer.write_record(WalRecordType::Begin, 1, 0, &[]).unwrap();
            writer.write_record(WalRecordType::Insert, 1, 1, &[10, 20, 30]).unwrap();
            writer.write_record(WalRecordType::Commit, 1, 0, &[]).unwrap();
            writer.sync().unwrap();
        }

        // 读取
        let mut reader = WalReader::open(&tmp).unwrap();
        let records = reader.read_all().unwrap();
        assert_eq!(records.len(), 3);
        assert_eq!(records[0].record_type, WalRecordType::Begin);
        assert_eq!(records[1].record_type, WalRecordType::Insert);
        assert_eq!(records[2].record_type, WalRecordType::Commit);
        assert_eq!(records[1].payload, vec![10, 20, 30]);

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn test_reader_empty_file() {
        let tmp = tmp("reader_empty");
        let _ = std::fs::remove_file(&tmp);
        std::fs::File::create(&tmp).unwrap();

        let mut reader = WalReader::open(&tmp).unwrap();
        let records = reader.read_all().unwrap();
        assert!(records.is_empty());

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn test_reader_seek_and_position() {
        let tmp = tmp("reader_seek");
        let _ = std::fs::remove_file(&tmp);

        {
            let mut writer = WalWriter::open(&tmp).unwrap();
            writer.write_record(WalRecordType::Begin, 1, 0, &[]).unwrap();
            writer.write_record(WalRecordType::Insert, 1, 1, &[1, 2]).unwrap();
            writer.write_record(WalRecordType::Commit, 1, 0, &[]).unwrap();
            writer.sync().unwrap();
        }

        let mut reader = WalReader::open(&tmp).unwrap();
        assert_eq!(reader.position(), 0);

        // 读第一条
        let rec1 = reader.read_next().unwrap().unwrap();
        assert_eq!(rec1.record_type, WalRecordType::Begin);
        let after_rec1 = reader.position();
        assert!(after_rec1 > 0);

        // 读第二条
        let rec2 = reader.read_next().unwrap().unwrap();
        assert_eq!(rec2.record_type, WalRecordType::Insert);

        // seek 回到第一条
        reader.seek_to(0).unwrap();
        assert_eq!(reader.position(), 0);
        let rec1_again = reader.read_next().unwrap().unwrap();
        assert_eq!(rec1_again.record_type, WalRecordType::Begin);

        // seek 到第二条位置
        reader.seek_to(after_rec1).unwrap();
        let rec2_again = reader.read_next().unwrap().unwrap();
        assert_eq!(rec2_again.record_type, WalRecordType::Insert);

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn test_reader_reset() {
        let tmp = tmp("reader_reset");
        let _ = std::fs::remove_file(&tmp);

        {
            let mut writer = WalWriter::open(&tmp).unwrap();
            writer.write_record(WalRecordType::Begin, 1, 0, &[]).unwrap();
            writer.write_record(WalRecordType::Commit, 1, 0, &[]).unwrap();
            writer.sync().unwrap();
        }

        let mut reader = WalReader::open(&tmp).unwrap();
        let first = reader.read_all().unwrap();
        assert_eq!(first.len(), 2);

        // reset 后能重新读
        reader.reset().unwrap();
        let second = reader.read_all().unwrap();
        assert_eq!(second.len(), 2);
        assert_eq!(second[0].record_type, first[0].record_type);

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn test_reader_partial_write_at_end() {
        let tmp = tmp("reader_partial");
        let _ = std::fs::remove_file(&tmp);

        // 写入一条完整记录 + 半条（模拟崩溃时的部分写入）
        {
            use std::io::Write;
            let mut writer = WalWriter::open(&tmp).unwrap();
            writer.write_record(WalRecordType::Begin, 1, 0, &[]).unwrap();
            writer.sync().unwrap();
        }

        // 追加垃圾数据模拟部分写入
        {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new().append(true).open(&tmp).unwrap();
            file.write_all(&[0x57, 0x41, 0x01, 0x00]).unwrap(); // 只有 magic + type + 部分 txn_id
        }

        let mut reader = WalReader::open(&tmp).unwrap();
        let records = reader.read_all().unwrap();
        // 应该能读到第一条完整的，第二条部分写入的被跳过
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].record_type, WalRecordType::Begin);

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn test_reader_many_records() {
        let tmp = tmp("reader_many");
        let _ = std::fs::remove_file(&tmp);

        let n = 1000;
        {
            let mut writer = WalWriter::open(&tmp).unwrap();
            for i in 0..n {
                let payload = vec![i as u8; 20];
                writer.write_record(WalRecordType::Insert, i, 1, &payload).unwrap();
            }
            writer.sync().unwrap();
        }

        let mut reader = WalReader::open(&tmp).unwrap();
        let records = reader.read_all().unwrap();
        assert_eq!(records.len(), n as usize);

        for (i, rec) in records.iter().enumerate() {
            assert_eq!(rec.txn_id, i as u32);
            assert_eq!(rec.payload.len(), 20);
            assert_eq!(rec.payload[0], i as u8);
        }

        let _ = std::fs::remove_file(&tmp);
    }
}
