#!/bin/bash
export PATH="/home/ll/.cargo/bin:$PATH"
cd /mnt/d/Github/EngramDB
cargo test --test phase0_semantic_gate -- --nocapture
echo "EXIT:$?"
