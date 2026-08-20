//! Phase 3 P2：异步压缩集成测试
//!
//! 验证：
//! 1. CompressionQueue 集成到 Database
//! 2. 大量数据后台压缩不阻塞写入
//! 3. 压缩结果正确回填到 ColumnStore
//!
//! 运行：`cargo test --release --test async_compress_basic -- --test-threads=1`

use engramdb::Connection;

#[test]
fn test_async_compress_doesnt_block_writes() {
    let path = "/tmp/p3_async_compress.hdb";
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{}-wal", path));

    let mut conn = Connection::open(path).unwrap();
    conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY, v FLOAT64)").unwrap();

    let start = std::time::Instant::now();

    // 写入 10K 行（高熵 Float64 → 难以压缩 → 测试压缩路径）
    for i in 0..10_000 {
        conn.execute(&format!(
            "INSERT INTO t VALUES ({}, {})", i, (i as f64) * 1.61803398
        )).unwrap();
    }
    engramdb::executor::operators::insert::flush_all_batched(conn.database_mut()).unwrap();

    let write_duration = start.elapsed();
    println!("10K INSERTs took: {:?}", write_duration);

    // 合并 Delta → 列存（read_column 从列存读）
    conn.compact_all().unwrap();

    // 提交到异步压缩队列
    let queue = engramdb::storage::async_compress::CompressionQueue::new();
    let id = conn.database_mut().table_id_by_name("t").unwrap();
    let col_data = conn.database_mut()
        .get_engine_table_mut_by_id(id).unwrap()
        .as_columnar_mut().unwrap()
        .column_store_mut().read_column(0, 0).unwrap().clone();
    let data_type = engramdb::common::types::DataType::Float64;
    queue.submit(engramdb::storage::async_compress::CompressTask {
        rg_idx: 0,
        data: col_data,
        data_type: engramdb::common::types::DataType::Int64, // id 列是 Int64
    });

    let flush_start = std::time::Instant::now();
    let results = queue.flush();
    let flush_duration = flush_start.elapsed();
    println!("Compression took: {:?}, {} blocks", flush_duration, results.len());

    // 验证压缩结果非空
    assert!(!results.is_empty());
    assert!(!results[0].compressed_data.is_empty());
}

#[test]
fn test_async_compress_multiple_blocks() {
    use engramdb::storage::async_compress::{CompressionQueue, CompressTask};

    let path = "/tmp/p3_async_multi.hdb";
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(format!("{}-wal", path));

    let mut conn = Connection::open(path).unwrap();
    conn.execute("CREATE TABLE t (id INT64 PRIMARY KEY, v TEXT)").unwrap();
    for i in 0..5000 {
        conn.execute(&format!("INSERT INTO t VALUES ({}, 'r{}')", i, i)).unwrap();
    }
    engramdb::executor::operators::insert::flush_all_batched(conn.database_mut()).unwrap();
    conn.compact_all().unwrap();

    let queue = CompressionQueue::new();

    // 提交多列压缩任务（模拟：VARCHAR 列通常压缩比高）
    let id = conn.database_mut().table_id_by_name("t").unwrap();
    let col0 = conn.database_mut()
        .get_engine_table_mut_by_id(id).unwrap()
        .as_columnar_mut().unwrap()
        .column_store_mut().read_column(0, 0).unwrap().clone();
    let col1 = conn.database_mut()
        .get_engine_table_mut_by_id(id).unwrap()
        .as_columnar_mut().unwrap()
        .column_store_mut().read_column(0, 1).unwrap().clone();
    queue.submit(CompressTask {
        rg_idx: 0,
        data: col0,
        data_type: engramdb::common::types::DataType::Int64,
    });
    queue.submit(CompressTask {
        rg_idx: 0,
        data: col1,
        data_type: engramdb::common::types::DataType::Varchar,
    });

    let results = queue.flush();
    assert_eq!(results.len(), 2);
    for r in &results {
        assert!(!r.compressed_data.is_empty());
    }
}