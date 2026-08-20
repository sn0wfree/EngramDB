//! Phase 2.5 P2：Auto 表分层调度（迁移决策 + 标记）
//!
//! 根据 HeatTracker + 表大小决定 Auto 表的目标引擎：
//! - 高频小表（score > 100, row_count < 10_000）→ Memory
//! - 冷大表（score < 0.5, row_count > 100_000）→ Log
//! - 其他 → Columnar
//!
//! ## 当前 Phase 2.5 范围
//! **决策 + 标记**：调用 `tick()` 后：
//! 1. 遍历所有标记为 `Auto` 的表
//! 2. 计算 heat_score（带衰减）
//! 3. 调用 `suggest_target_engine()` 得目标引擎
//! 4. 若与当前引擎不同 → 在表上 `mark_target_engine(target)`
//! 5. 真正数据迁移推迟到 Phase 3（约 1 周工作）
//!
//! 这样用户能看到迁移决策日志，确认引擎已切换。
//!
//! ## 触发机制
//! `Connection` 入口的 `on_commit()` 每 1000 次 commit 触发一次 tick。
//! 用户也可手动调用 `Connection::tier_migration_tick()` 强制 tick。

use crate::common::types::EngineType;
use crate::storage::Database;

/// Phase 2.5 P2：迁移决策结果
#[derive(Debug, Clone, Copy)]
pub struct MigrationDecision {
    pub table_id: u32,
    pub from_engine: EngineType,
    pub to_engine: EngineType,
    pub heat_score: f64,
    pub row_count: u64,
}

/// Phase 2.5 P2：tick 全部 Auto 表的迁移决策
///
/// 返回所有"应迁移"的决策列表。
/// 真正执行数据搬迁需要 1+ 周工作（表重建 + MVCC 调整），
/// 当前仅返回决策列表，用户可通过日志 / 调试观察。
pub fn tick_decisions(db: &mut Database) -> Vec<MigrationDecision> {
    let mut decisions = Vec::new();

    // 收集所有 Auto 表（用 def.engine == Auto 识别）
    let auto_tables: Vec<(u32, EngineType, u64)> = {
        let table_names: Vec<(u32, String)> = db.table_names_internal()
            .iter().map(|(k, v)| (*v, k.clone())).collect();
        let mut auto = Vec::new();
        for (id, _name) in table_names {
            if let Some(table) = db.get_engine_table_mut_by_id(id) {
                if table.def().engine == EngineType::Auto {
                    let rc = table.def().row_count;
                    // 当前引擎（Auto 标记后实际走 Columnar 路径）
                    let engine = match table {
                        crate::storage::engine::EngineTable::Columnar(_) => EngineType::Columnar,
                        crate::storage::engine::EngineTable::Memory(_) => EngineType::Memory,
                        crate::storage::engine::EngineTable::Log(_) => EngineType::Log,
                    };
                    auto.push((id, engine, rc));
                }
            }
        }
        auto
    };

    // 计算每个 Auto 表的决策
    for (id, current_engine, row_count) in auto_tables {
        let heat_score = db.heat_tracker_mut().heat_score(id);
        let target = super::heat_tracker::suggest_target_engine(heat_score, row_count);

        if target != current_engine && target != EngineType::Auto {
            decisions.push(MigrationDecision {
                table_id: id,
                from_engine: current_engine,
                to_engine: target,
                heat_score,
                row_count,
            });
        }
    }

    decisions
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Connection;

    fn setup() -> Connection {
        let tid = format!("{:?}", std::thread::current().id())
            .replace(['(', ')', ':', ' '], "_");
        let path = format!("/tmp/p25_tier_{}_{}_{}.hdb", std::process::id(), tid, line!());
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{}-wal", path));
        Connection::open(&path).unwrap()
    }

    #[test]
    fn test_tick_decisions_on_empty_database() {
        let mut conn = setup();
        let decisions = tick_decisions(conn.database_mut());
        assert_eq!(decisions.len(), 0);
    }

    #[test]
    fn test_tick_decisions_on_auto_table_high_freq_small() {
        let mut conn = setup();
        conn.execute("CREATE TABLE hot (id INT64 PRIMARY KEY, v TEXT) ENGINE = Auto").unwrap();
        conn.execute("INSERT INTO hot VALUES (1, 'a')").unwrap();

        let id = conn.database_mut().table_id_by_name("hot").unwrap();
        // 200 次访问模拟高频
        for _ in 0..200 {
            conn.database_mut().record_access(id);
        }

        let decisions = tick_decisions(conn.database_mut());
        assert_eq!(decisions.len(), 1);
        let d = &decisions[0];
        assert_eq!(d.from_engine, EngineType::Columnar);
        assert_eq!(d.to_engine, EngineType::Memory);
        assert!(d.heat_score > 100.0);
    }

    #[test]
    fn test_tick_decisions_on_auto_table_cold_large() {
        let mut conn = setup();
        conn.execute("CREATE TABLE cold (id INT64 PRIMARY KEY, v TEXT) ENGINE = Auto").unwrap();
        // 模拟 200K 行（无需真插入，仅修改 def.row_count 测试决策逻辑）
        let id = conn.database_mut().table_id_by_name("cold").unwrap();
        {
            let table = conn.database_mut().get_engine_table_mut_by_id(id).unwrap();
            if let crate::storage::engine::EngineTable::Columnar(t) = table {
                t.def_mut().row_count = 200_000;
            }
        }
        // 不做任何访问 → score ≈ 0
        let decisions = tick_decisions(conn.database_mut());
        assert_eq!(decisions.len(), 1);
        let d = &decisions[0];
        assert_eq!(d.to_engine, EngineType::Log);
    }

    #[test]
    fn test_tick_decisions_no_migration_for_columnar() {
        let mut conn = setup();
        // 显式 Columnar 表不应触发迁移
        conn.execute("CREATE TABLE fixed (id INT64 PRIMARY KEY) ENGINE = Columnar").unwrap();
        conn.execute("INSERT INTO fixed VALUES (1)").unwrap();

        let decisions = tick_decisions(conn.database_mut());
        assert_eq!(decisions.len(), 0);
    }
}