#!/bin/bash
export PATH="/home/ll/.cargo/bin:$PATH"
cd /mnt/d/Github/EngramDB
echo "=== phase0 ==="
cargo test --test phase0_semantic_gate -- --nocapture
echo "=== lib unit tests ==="
cargo test --lib 2>&1 | tail -30
echo "=== differential_sqlite ==="
cargo test --test differential_sqlite 2>&1 | tail -20
echo "DONE"
