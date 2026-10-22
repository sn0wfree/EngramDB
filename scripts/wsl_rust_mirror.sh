#!/bin/bash
set -euo pipefail
export RUSTUP_DIST_SERVER=https://mirrors.ustc.edu.cn/rust-static
export RUSTUP_UPDATE_ROOT=https://mirrors.ustc.edu.cn/rust-static/rustup
export PATH="/home/ll/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
echo "PATH=$PATH"
rustup --version
rustup default stable
rustc --version
cargo --version
echo "RUST_OK"
