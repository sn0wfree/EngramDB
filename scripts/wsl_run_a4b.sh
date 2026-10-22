#!/bin/bash
export PATH="/home/ll/.cargo/bin:$PATH"
cd /mnt/d/Github/EngramDB
cargo test --lib test_execute_insert_and_countstar -- --nocapture 2>&1 | tail -25
echo "---"
cargo test --lib test_txn_batch -- --nocapture 2>&1 | tail -30
