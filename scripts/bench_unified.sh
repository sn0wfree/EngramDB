#!/usr/bin/env bash
# scripts/bench_unified.sh — B3 统一 bench：E3 场景表 U1–U8 一键重跑（WSL 固定机口径）
#
# 用法（WSL 内，任意目录均可，脚本自行 cd 到仓库根）：
#   bash scripts/bench_unified.sh            # 全量 U1–U8（会整体重写报告，推荐）
#   bash scripts/bench_unified.sh u4 u8      # 只跑指定场景（同一次运行内去重到 bench）
# 输出：
#   target/bench_unified.log              —— 原始日志（所有 cargo/bench 输出重定向到此，防会话卡顿）
#   target/bench_unified_stdout.log       —— 脚本自身进度（后台跑时重定向）
#   scripts/benchmarks/unified-report.md  —— 同构 markdown 报告（强制元数据头；每次运行整体重写）
#
# 场景 → bench 映射（E3「材料推断，收敛不新发明」）：
#   U1/U3/U7 → v0_13_acceptance_bench（A-1 / A-2 / A-3，附带 A-1b~d、B-4、A-4）
#   U2       → write_bench（bench_batch_sql / bench_prepared）
#   U4/U5/U6 → m1_acceptance_bench（M1-1/2/3）
#   U8       → wal_group_commit_bench（group=0/16/64/128）
set -uo pipefail
export PATH="$HOME/.cargo/bin:$PATH"

cd "$(dirname "${BASH_SOURCE[0]}")/.." || exit 1

LOG="target/bench_unified.log"
REPORT="scripts/benchmarks/unified-report.md"

# ---------- 场景清单（固定，来源 docs/评审证据包-E1-E5.md E3） ----------
ALL_SCENES=(u1 u2 u3 u4 u5 u6 u7 u8)
declare -A SCEN_TITLE=(
  [u1]="事务逐行写"
  [u2]="批量/Prepared 写"
  [u3]="索引点查"
  [u4]="低选择性扫描"
  [u5]="ORDER BY"
  [u6]="GROUP BY"
  [u7]="COUNT(*)"
  [u8]="WAL 组提交吞吐"
)
declare -A SCEN_BENCH=(
  [u1]=v0_13_acceptance_bench
  [u2]=write_bench
  [u3]=v0_13_acceptance_bench
  [u4]=m1_acceptance_bench
  [u5]=m1_acceptance_bench
  [u6]=m1_acceptance_bench
  [u7]=v0_13_acceptance_bench
  [u8]=wal_group_commit_bench
)
declare -A SCEN_PARAMS=(
  [u1]="1000 tx × 1 row；ITERS=5 取中位数（bench 内 A-1）；对照 SQLite"
  [u2]="bench 固定：单条 1k 行 / batch_sql 10k+100k(batch=100)+100k(batch=1000) / prepared 10k+100k(batch=100) / compact 50k"
  [u3]="1M 行表 × 1000 随机等值；ITERS=5；查询 seed=42（SimpleRng）；bench 内 A-2"
  [u4]="10万行装载 seed=42；WHERE val>990 ≈ 1%；ITERS=5；bench 内 M1-1"
  [u5]="10万行整数列 ORDER BY k；ITERS=5；bench 内 M1-2"
  [u6]="10万行单列整数 GROUP BY k（1000 组）；ITERS=5；bench 内 M1-3"
  [u7]="1M 行 COUNT(*) 无 WHERE；ITERS=5；bench 内 A-3"
  [u8]="每轮 5000 autocommit INSERT × 10 样本取 P50；group=0/16/64/128（顺序写 id，无随机种子）"
)
declare -A BENCH_COVERS=(
  [v0_13_acceptance_bench]="U1 · U3 · U7"
  [write_bench]="U2"
  [m1_acceptance_bench]="U4 · U5 · U6"
  [wal_group_commit_bench]="U8"
)
# 结果摘录 grep 模式（只摘行，不搬全量日志进报告）
declare -A BENCH_PAT=(
  [v0_13_acceptance_bench]='^(={10,}|  EngramDB v0\.13|  轮次|--- |  EngramDB setup done|  SQLite setup done|  中位数|  FAIL|.*FAILED|.*panicked)'
  [write_bench]='^(={6,}|【测试|  耗时:|  速度:|查询\+compact|  返回行数)'
  [m1_acceptance_bench]='^(={6,}|  M1-[123])'
  [wal_group_commit_bench]='(median=|=== 总结|基线 \(true|group=16 default|group=64:|group=128:|KPI|⚠️|❌|ℹ️)'
)

# ---------- 参数解析 ----------
declare -a REQUESTED=()
if [[ $# -eq 0 ]]; then
  REQUESTED=("${ALL_SCENES[@]}")
else
  for a in "$@"; do
    a="$(printf '%s' "$a" | tr '[:upper:]' '[:lower:]')"
    if [[ -z "${SCEN_BENCH[$a]:-}" ]]; then
      echo "未知场景: $a（合法：u1..u8）" >&2
      exit 2
    fi
    REQUESTED+=("$a")
  done
fi
INVOCATION="bash scripts/bench_unified.sh${*:+ }${*:-}"

# 按 bench 去重（U1/U3/U7 同一次 v0_13 运行覆盖）
declare -a BENCHES=()
for s in "${REQUESTED[@]}"; do
  b="${SCEN_BENCH[$s]}"
  dup=0
  for x in "${BENCHES[@]:-}"; do [[ "$x" == "$b" ]] && dup=1; done
  [[ $dup -eq 0 ]] && BENCHES+=("$b")
done

# ---------- 元数据（报告头强制字段） ----------
GIT_SHA="$(git rev-parse --short HEAD)"
GIT_BRANCH="$(git branch --show-current)"
GIT_DIRTY="$(git -c core.autocrlf=true status --porcelain | wc -l | xargs)"
CPU_MODEL="$(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2- | xargs)"
CPU_CORES="$(nproc)"
KERNEL="$(uname -r)"
OS_NAME="$(. /etc/os-release && echo "$PRETTY_NAME")"
RUSTC_V="$(rustc -V)"
RUN_START="$(date -u '+%Y-%m-%d %H:%M:%S UTC')"

mkdir -p target
{
  echo "=========================================================="
  echo "bench_unified run: $RUN_START"
  echo "invocation: $INVOCATION"
  echo "host: $CPU_MODEL / $KERNEL"
  echo "=========================================================="
} >>"$LOG"

# ---------- 阶段 1：编译（与场景耗时分离；--no-run 全量 bench 目标 = 编译门禁） ----------
echo ">>> [compile] cargo bench --no-run（profile=bench/Release，全部 bench 目标）"
t0=$(date +%s)
cargo bench --no-run >>"$LOG" 2>&1
COMPILE_RC=$?
COMPILE_S=$(( $(date +%s) - t0 ))
echo "<<< [compile] rc=$COMPILE_RC ${COMPILE_S}s"
if [[ $COMPILE_RC -ne 0 ]]; then
  echo "编译失败，详见 $LOG 尾部；中止（不生成报告）" >&2
  exit 1
fi

# ---------- 阶段 2：逐 bench 运行（输出重定向到 $LOG） ----------
declare -A BENCH_DUR=()
declare -A BENCH_RC=()
for b in "${BENCHES[@]}"; do
  echo ">>> [$b] start $(date -u '+%H:%M:%S')"
  echo "###BENCH_START $b" >>"$LOG"
  t0=$(date +%s)
  cargo bench --bench "$b" >>"$LOG" 2>&1
  rc=$?
  dur=$(( $(date +%s) - t0 ))
  echo "###BENCH_END $b rc=$rc dur=${dur}s" >>"$LOG"
  BENCH_RC[$b]=$rc
  BENCH_DUR[$b]=$dur
  echo "<<< [$b] rc=$rc dur=${dur}s"
done

RUN_END="$(date -u '+%Y-%m-%d %H:%M:%S UTC')"

# ---------- 阶段 3：生成报告 ----------
extract() {
  local b="$1"
  sed -n "/^###BENCH_START $b\$/,/^###BENCH_END $b/p" "$LOG" \
    | grep -E "${BENCH_PAT[$b]}" || true
}

{
  echo "# EngramDB 统一 Bench 报告（B3 · U1–U8）"
  echo
  echo "> 由 \`scripts/bench_unified.sh\` 一键生成，每次运行整体重写；数字真源 = **WSL2 固定机**，勿手改。原始日志 \`target/bench_unified.log\`。"
  echo
  echo "## 元数据头（强制字段）"
  echo
  echo "| 字段 | 值 |"
  echo "|---|---|"
  echo "| git SHA | \`$GIT_SHA\`（branch \`$GIT_BRANCH\`，工作区改动 $GIT_DIRTY 处） |"
  echo "| CPU | $CPU_MODEL × ${CPU_CORES} vCPU |"
  echo "| 内核/OS | \`$KERNEL\` / $OS_NAME（WSL2） |"
  echo "| cargo profile | bench（继承 release，opt-level 3）· $RUSTC_V |"
  echo "| 命令行 | \`$INVOCATION\` |"
  echo "| 种子 | 固定 seed=42（v0_13 A-2 查询、m1 装载 SimpleRng）；write_bench / WAL 组提交为顺序写，无随机种子 |"
  echo "| 行数 | 按场景固定，见下表 |"
  echo "| 迭代 | 按场景固定（bench 内 ITERS 取中位数/P50），见下表 |"
  echo "| 冷/热 | **冷启动**：每场景独立进程，/tmp 下 DB 由 bench 每次自清理重建；场景内循环为热缓存；编译耗时单独计入阶段 1（${COMPILE_S}s），不计入场景耗时 |"
  echo "| 运行时间 | $RUN_START → $RUN_END |"
  echo "| 编译 | \`cargo bench --no-run\` rc=$COMPILE_RC，${COMPILE_S}s |"
  echo
  echo "## 场景覆盖（E3 U1–U8）"
  echo
  echo "| # | 场景 | bench 入口 | 固定参数（行数 · 迭代 · 种子） | 本次状态 |"
  echo "|---|---|---|---|---|"
  for s in "${ALL_SCENES[@]}"; do
    b="${SCEN_BENCH[$s]}"
    id="$(printf '%s' "$s" | tr '[:lower:]' '[:upper:]')"
    params="${SCEN_PARAMS[$s]}"
    if [[ -z "${BENCH_RC[$b]:-}" ]]; then
      status="⏭ 本次未选（重跑不带参数即全量）"
    elif [[ "${BENCH_RC[$b]}" -ne 0 ]]; then
      status="❌ 失败 rc=${BENCH_RC[$b]}"
    else
      status="✅ 已跑（${BENCH_DUR[$b]}s）"
    fi
    echo "| $id | ${SCEN_TITLE[$s]} | \`$b\` | $params | $status |"
  done
  echo
  echo "## 结果摘录（按 bench 运行，节选自原始日志）"
  for b in "${BENCHES[@]}"; do
    echo
    echo "### \`$b\` — 覆盖 ${BENCH_COVERS[$b]} · rc=${BENCH_RC[$b]} · 耗时 ${BENCH_DUR[$b]}s"
    echo
    echo '```text'
    extract "$b"
    echo '```'
  done
  echo
  echo "## 缺失与偏差说明"
  echo
  echo "- E3 草案建议 U2 batch=512；\`write_bench\` 固定 batch=100/1000，未改他人 bench，按实际口径记录。"
  echo "- U1/U3/U7 共用 \`v0_13_acceptance_bench\` 一次运行，附带 A-1b/c/d（单事务批量，可视为 U2 参考）、B-4（ORDER BY 100K）、A-4a/b（WHERE 1%/10% 1M）——超出 U1–U8，一并摘录。"
  echo "- U4/U5/U6 共用 \`m1_acceptance_bench\` 一次运行（10 万行，E3 建议口径）。"
  echo "- 本报告数字为 **WSL2 release（profile=bench）**；与历史 Windows/debug 或 CI 数字不可比。"
  echo "- 场景级筛选用法：\`bash scripts/bench_unified.sh u4 u8\`——报告只含本次选中场景，全量口径请不带参数重跑。"
} >"$REPORT"

# ---------- 收尾：stdout 只留摘要 ----------
echo "=== 摘要（报告已写 $REPORT）==="
for s in "${REQUESTED[@]}"; do
  b="${SCEN_BENCH[$s]}"
  id="$(printf '%s' "$s" | tr '[:lower:]' '[:upper:]')"
  echo "$id ${SCEN_TITLE[$s]} → $b rc=${BENCH_RC[$b]} ${BENCH_DUR[$b]}s"
done
FAIL=0
for b in "${BENCHES[@]}"; do [[ "${BENCH_RC[$b]}" -ne 0 ]] && FAIL=1; done
exit $FAIL
