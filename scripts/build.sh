#!/bin/bash
# HybridDB 构建脚本

set -e

echo "=== HybridDB 构建脚本 ==="
echo ""

# 检查 Rust
if ! command -v cargo &> /dev/null; then
    echo "错误: 未找到 Rust 工具链"
    echo "请先安装 Rust: https://rustup.rs/"
    exit 1
fi

echo "Rust 版本:"
rustc --version
cargo --version
echo ""

# 构建模式
MODE=${1:-release}

echo "构建模式: $MODE"
echo ""

if [ "$MODE" = "release" ]; then
    echo "正在构建 Release 版本..."
    cargo build --release
    echo ""
    echo "✓ 构建完成!"
    echo "二进制文件: target/release/hybriddb"
else
    echo "正在构建 Debug 版本..."
    cargo build
    echo ""
    echo "✓ 构建完成!"
    echo "二进制文件: target/debug/hybriddb"
fi

echo ""
echo "=== 运行测试 ==="
cargo test

echo ""
echo "=== 完成 ==="
