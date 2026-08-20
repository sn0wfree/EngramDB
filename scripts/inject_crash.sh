#!/bin/bash
# WAL 组提交崩溃注入测试
# 用法：./scripts/inject_crash.sh <scenario>
#   scenario in: mid-write | mid-commit | post-commit
#
# mid-write:    写入中途崩溃（数据可能未提交到 WAL）
# mid-commit:   写 COMMIT 后 fsync 前崩溃（可能留下悬挂 BEGIN）
# post-commit:  完整 commit 后崩溃（数据应全部 fsync）
#
# 每个场景跑完后调用 inject_crash_recover 验证数据完整性

set -e

# 定位 cargo（开发机 PATH 可能不含 ~/.cargo/bin）
if ! command -v cargo >/dev/null 2>&1; then
    if [ -x "$HOME/.cargo/bin/cargo" ]; then
        export PATH="$HOME/.cargo/bin:$PATH"
    fi
fi

# 关闭 sccache（本环境未安装但 ~/.cargo/config.toml 默认开启）
export CARGO_BUILD_RUSTC_WRAPPER=""
export RUSTC_WRAPPER=""

SCENARIO="${1:-post-commit}"
PATH_DB="/tmp/m1_crash_${SCENARIO}.hdb"
PATH_WAL="${PATH_DB}-wal"
N_TXNS=200
KILL_AT=100

rm -f "$PATH_DB" "$PATH_WAL"

echo "=== scenario: $SCENARIO ==="

# 启动后台进程
cargo run --release --example inject_crash_runner --quiet -- \
    --path "$PATH_DB" --scenario "$SCENARIO" --n-txns "$N_TXNS" --kill-at "$KILL_AT" \
    2>"${PATH_DB}.runner.log" &
RUNNER_PID=$!

# 给 1 秒让 runner 启动并写入 KILL_AT 笔
sleep 1

# 注入崩溃（如果还没到 kill_at，等更长）
if [ "$SCENARIO" != "post-commit" ]; then
    # runner 应该已经到 kill_at 后在 sleep
    kill -9 "$RUNNER_PID" 2>/dev/null || true
    wait "$RUNNER_PID" 2>/dev/null || true
else
    wait "$RUNNER_PID"
fi

echo "--- runner log ---"
cat "${PATH_DB}.runner.log" 2>/dev/null || true
echo "--- end log ---"

# 恢复
echo "=== recovery ==="
cargo run --release --example inject_crash_recover --quiet -- --path "$PATH_DB" 2>&1
RECOVER_EXIT=$?

rm -f "$PATH_DB" "$PATH_WAL" "${PATH_DB}.runner.log"

if [ "$RECOVER_EXIT" -eq 0 ]; then
    echo "✅ scenario $SCENARIO: recovery OK"
else
    echo "❌ scenario $SCENARIO: recovery FAILED (exit=$RECOVER_EXIT)"
    exit 1
fi