#!/bin/bash
set -euo pipefail
export RUSTUP_DIST_SERVER=https://mirrors.ustc.edu.cn/rust-static
export RUSTUP_UPDATE_ROOT=https://mirrors.ustc.edu.cn/rust-static/rustup
export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
export PATH="/home/ll/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
# crates.io mirror (SJTU) in case index is slow
mkdir -p "$HOME/.cargo"
cat > "$HOME/.cargo/config.toml" <<'EOF'
[source.crates-io]
replace-with = "sjtu"

[source.sjtu]
registry = "sparse+https://mirrors.sjtug.sjtu.edu.cn/crates.io-index/"

[net]
retry = 3
EOF
cd /mnt/d/Github/EngramDB
echo "=== cargo test differential_sqlite ==="
cargo test --test differential_sqlite -- --nocapture 2>&1
echo "DIFF_OK"
