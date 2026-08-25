# Plan Mode Checklist

## Current Status
- [x] Create high-priority features implementation plan document
- [x] Analyze codebase for implementation details
- [x] Implement ALTER TABLE parser support
- [x] Implement VIEW support (AST, Parser, Planner, Executor, Database)
- [x] Implement CHECK constraints
- [x] Implement scalar subqueries
- [x] Implement recursive CTE
- [x] Run tests to verify implementations (1213 tests passed)
- [x] Fix any test failures
- [x] Improve edge cases of high-priority features
- [x] Run cargo bench to verify performance
- [x] Update verification checklist in plan document

## Completed Features

### ALTER TABLE Parser (v0.22.0)
- Added parser support for ALTER TABLE operations
- Supported operations: ADD COLUMN, DROP COLUMN, RENAME COLUMN, RENAME TABLE
- Edge case handling: column name validation, primary key protection, index protection
- File: `src/sql/parser.rs`, `src/executor/operators/alter_table.rs`

### VIEW Support (v0.22.0)
- Added AST nodes: `CreateViewStmt`, `DropViewStmt`
- Added parser support for CREATE VIEW and DROP VIEW
- Added planner support: `plan_create_view`, `plan_drop_view`
- Added executor support for CREATE VIEW and DROP VIEW
- Added `ViewDef` struct to storage module
- Added `create_view`, `drop_view`, `get_view`, `view_names` methods to Database
- Files: `src/sql/ast.rs`, `src/sql/parser.rs`, `src/sql/planner.rs`, `src/executor/physical_plan.rs`, `src/executor/executor.rs`, `src/storage/mod.rs`

### CHECK Constraints (v0.22.0)
- Added `check_expr` field to `ColumnDef` in `src/common/types.rs` and `src/sql/ast.rs`
- Added parser support for CHECK constraints in CREATE TABLE and ALTER TABLE ADD COLUMN
- Added `validate_check_constraints` function to validate CHECK constraints during INSERT
- Added `eval_check_expr` function to evaluate CHECK expressions
- Supported expressions: `>`, `<`, `>=`, `<=`, `=`, `!=`, `<>`, `IS NULL`, `IS NOT NULL`, `NOT NULL`
- Supported compound expressions: AND, OR, NOT
- Supported: BETWEEN ... AND ..., IN (...)
- Type coercion: Int32/Int64, Float32/Float64 cross-comparison
- Files: `src/common/types.rs`, `src/sql/ast.rs`, `src/sql/parser.rs`, `src/sql/planner.rs`, `src/executor/operators/insert.rs`

### Scalar Subqueries (v0.22.0)
- Added `eval_scalar_subquery`, `eval_exists_subquery`, `eval_in_subquery` functions
- Updated `eval_vectorized` to accept optional Database reference via `eval_vectorized_with_db`
- Added `eval_function_with_db` and `eval_case_vectorized_with_db` functions
- Supported subquery types: scalar subquery, EXISTS, IN subquery
- Edge case handling: NULL three-value logic, empty result sets
- Files: `src/executor/expression.rs`

### Recursive CTE (v0.22.0)
- Added `recursive` field to `Cte` struct in AST
- Updated parser to handle WITH RECURSIVE syntax
- Added `is_recursive_cte`, `references_cte`, `table_ref_references_cte` functions
- Updated `inline_ctes` to handle recursive CTEs differently
- Added `plan_recursive_cte`, `plan_select_with_cte_result` functions in planner
- Added `RecursiveCte` PhysicalPlan node
- Added executor support for iterative execution of recursive CTEs
- Edge case handling: infinite loop detection, max iterations, column count validation
- Files: `src/sql/ast.rs`, `src/sql/parser.rs`, `src/sql/planner.rs`, `src/executor/physical_plan.rs`, `src/executor/executor.rs`

## All High-Priority Features Completed!

### Summary
All high-priority features (P1) have been implemented:
1. ALTER TABLE Parser ✅
2. VIEW Support ✅
3. CHECK Constraints ✅
4. Scalar Subqueries ✅
5. Recursive CTE ✅

### Performance Results
- 1213 tests passed, 0 failed
- All benchmarks completed successfully
- Performance highlights:
  - Batch import: 24.36M rows/s
  - Vector filter: 2.86B rows/s
  - vs SQLite batch: 76.9x faster

### Remaining Work (Future Versions)

#### Medium Priority
- [ ] Nested VIEW support (VIEW referencing VIEW)
- [ ] Correlated subqueries
- [ ] ALTER COLUMN (change type/nullability)
- [ ] CHECK constraints with cross column references

#### Low Priority
- [ ] DATE/TIME types
- [ ] ARRAY type
- [ ] JSONB type
- [ ] UUID type
- [ ] ENUM/SET types

#### Documentation
- [ ] Update README.md feature list
- [ ] Update CHANGELOG.md
- [ ] Add v0.22.0 release notes

#### Performance Optimization
- [ ] View query optimization (view merging)
- [ ] Recursive CTE optimization (incremental computation)
- [ ] CHECK constraint optimization (lazy validation)
