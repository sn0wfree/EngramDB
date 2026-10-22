#!/bin/bash
export PATH="/home/ll/.cargo/bin:$PATH"
cd /mnt/d/Github/EngramDB
cargo test --test a5_concurrency -- --nocapture 2>&1 | tail -30
