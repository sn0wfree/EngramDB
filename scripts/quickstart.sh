#!/bin/bash
# 快速启动脚本 - 运行示例

set -e

cd "$(dirname "$0")/.."

echo "=== HybridDB 快速体验 ==="
echo ""

if ! command -v cargo &> /dev/null; then
    echo "错误: 未找到 Rust 工具链"
    echo "请先安装 Rust: https://rustup.rs/"
    exit 1
fi

echo "步骤 1/3: 编译..."
cargo build --release 2>&1 | tail -3
echo ""

echo "步骤 2/3: 运行基本示例..."
echo ""
cargo run --release --example basic
echo ""

echo "步骤 3/3: 运行测试..."
cargo test 2>&1 | tail -10
echo ""

echo "=== 完成 ==="
echo ""
echo "下一步:"
echo "  - 交互模式: cargo run --release -- mydb.hdb"
echo "  - 运行基准: cargo bench"
echo "  - 查看文档: cat docs/01-technical-design.md"
