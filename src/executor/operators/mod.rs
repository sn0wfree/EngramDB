//! 执行算子

pub mod table_scan;
pub mod filter;
pub mod projection;
pub mod aggregate;
pub mod hash_join;
pub mod insert;
pub mod delete;
pub mod update;
pub mod sort;
pub mod alter_table;
pub mod pragma;
pub mod window;
