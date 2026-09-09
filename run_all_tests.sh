#!/usr/bin/env bash
#
# run_all_tests.sh — kcptun-rs 全量测试脚本
#
# 一次跑完：构建 → 质量门(unit/integration/clippy) → 独立 crate 冒烟 →
#           压力测试 → 性能 bench → Go 互操作 e2e
#
# 输出纪律（项目 CLAUDE.md §7）：只报失败 + 结果表，不倾倒全量输出。
# 每阶段完整日志写入 target/test-logs/<run-timestamp>/ 备查。
#
# 用法:
#   ./run_all_tests.sh                 # 全量（tokio 后端；bench+e2e 需要 Go）
#   ./run_all_tests.sh --quick         # 快速：跳过 bench/e2e/ignored 重测试
#   ./run_all_tests.sh --with-smol     # 额外构建+测试 smol 后端（慢）
#   ./run_all_tests.sh --skip-bench    # 跳过性能 bench
#   ./run_all_tests.sh --skip-e2e      # 跳过 Go e2e
#   ./run_all_tests.sh --keep-logs     # 保留日志（默认保留到 target/test-logs/）
#
set -uo pipefail   # 不用 -e：收集每阶段结果，最后汇总退出码

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$ROOT"

QUICK=0; WITH_SMOL=0; SKIP_E2E=0; SKIP_BENCH=0
usage() {
    sed -n '2,16p' "$0" | sed 's/^# \{0,1\}//'
    exit 0
}
for a in "$@"; do
    case "$a" in
        --quick)      QUICK=1 ;;
        --with-smol)  WITH_SMOL=1 ;;
        --skip-e2e)   SKIP_E2E=1 ;;
        --skip-bench) SKIP_BENCH=1 ;;
        -h|--help)    usage ;;
        *) echo "unknown option: $a"; usage ;;
    esac
done

# ── 日志目录 ───────────────────────────────────────────────────────────────
LOGDIR="target/test-logs/$(date +%Y%m%d-%H%M%S)"
mkdir -p "$LOGDIR"
NCPU=$(getconf _NPROCESSORS_ONLN 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || echo 4)

# ── 阶段执行器 ──────────────────────────────────────────────────────────────
# run_phase NAME SHOWTAIL_ON_OK CMD...   （SHOWTAIL=1：成功时也打印日志尾部摘要）
# 返回 0/1；不中断脚本。
RESULTS=()
run_phase() {
    local name="$1" showtail="$2"; shift 2
    local log="$LOGDIR/$name.log" rc t0 t1 dur
    t0=$(date +%s)
    echo "==> [$name] $*"
    "$@" >"$log" 2>&1
    rc=$?; t1=$(date +%s); dur=$((t1-t0))
    if [ "$rc" -eq 0 ]; then
        RESULTS+=("PASS $name")
        echo "  [PASS] $name (${dur}s)"
        if [ "$showtail" = 1 ]; then
            echo "  --- summary (tail) ---"
            tail -12 "$log" | sed 's/^/    /'
        fi
    else
        RESULTS+=("FAIL $name")
        echo "  [FAIL] $name (${dur}s) — log: $log"
        # 只报失败行
        grep -E "test result: FAILED|FAILED|failures:|^error(\[|:)|panicked|assertion .* failed|aborted" "$log" 2>/dev/null | head -15 | sed 's/^/      /'
    fi
    return "$rc"
}
skip_phase() { RESULTS+=("SKIP $1"); echo "  [SKIP] $1"; }

echo "══════════════════════════════════════════════════════════════════"
echo " kcptun-rs 全量测试   $(date '+%Y-%m-%d %H:%M:%S')"
echo " platform: $(uname -s)/$(uname -m)  cores=$NCPU  quick=$QUICK  smol=$WITH_SMOL"
echo "══════════════════════════════════════════════════════════════════"

# ── 前置检查 ────────────────────────────────────────────────────────────────
GO_OK=0; command -v go >/dev/null 2>&1 && GO_OK=1
GO_BINS_OK=0; [ -x tests/kcptun-go/server ] && [ -x tests/kcptun-go/client ] && GO_BINS_OK=1
echo "  Go tool: $GO_OK | Go kcptun bins: $GO_BINS_OK | logs: $LOGDIR"
[ "$GO_BINS_OK" = 0 ] && [ "$SKIP_E2E" = 0 ] && echo "  [warn] Go kcptun binaries missing → e2e/bench(Go) 将跳过"

# ══ 阶段 1: 构建 ═══════════════════════════════════════════════════════════
run_phase build-debug  0 cargo build --workspace -j "$NCPU"
run_phase build-release 0 cargo build --release -j "$NCPU"
if [ "$WITH_SMOL" = 1 ]; then
    run_phase build-smol-release 0 make release-smol
fi

# ══ 阶段 2: 质量门（fmt + 全量 unit/integration + clippy）═════════════════
run_phase fmt 0 cargo fmt --all -- --check
if [ "$QUICK" = 1 ]; then
    run_phase unit-workspace 0 cargo test --workspace --tests -- --test-threads=2
else
    # --tests: unit+integration（含 --include-ignored 重测试），不含 doctest。
    # doctest 单独跑（--doc 不带 include-ignored）——否则 rustdoc 会强制编译
    # 项目里那些"意图不完整"的 ignore doctest（smux-rs 7 个等），必然失败。
    run_phase unit-workspace 0 cargo test --workspace --tests -- --test-threads=2 --include-ignored
fi
run_phase doctests 0 cargo test --workspace --doc
run_phase clippy 0 cargo clippy --workspace -- -D warnings

# ══ 阶段 3: 独立 crate 冒烟（feature 门控 / 专项）═════════════════════════
run_phase kcp-async 0 cargo test -p kcp-rs --features async \
    --test kcpconn_integrity --test kcpconn_listener -- --test-threads=2
run_phase ratelimit-smoke 0 cargo test -p kcptun-common ratelimit -- --nocapture
run_phase snappy-interop 0 cargo test test_snappy_go_rust_interop -- --nocapture
# tcpraw/KcpTcpListener 测试：仅 Linux+root（macOS 上跳过）
if [ "$(uname -s)" = "Linux" ] && [ "$(id -u)" = 0 ]; then
    run_phase kcp-tcpconn-root 0 cargo test -p kcp-rs --features async \
        --test tcpconn_tcp -- --test-threads=2
else
    skip_phase kcp-tcpconn-root
fi

# ══ 阶段 4: 压力测试（release，串行）═════════════════════════════════════
run_phase stress 0 cargo test --release --package kcptun-server --test stress_test \
    -- --nocapture --test-threads=1

# ══ 阶段 5: 性能 bench ════════════════════════════════════════════════════
if [ "$QUICK" = 1 ] || [ "$SKIP_BENCH" = 1 ]; then
    skip_phase bench-quick
else
    run_phase bench-quick 1 python3 bench_rust_vs_go.py --quick --rust-only \
        --conn 4 --size 65536 --runs 3
fi

# ══ 阶段 6: Go ↔ Rust e2e 互操作 ═══════════════════════════════════════════
if [ "$QUICK" = 1 ] || [ "$SKIP_E2E" = 1 ] || [ "$GO_BINS_OK" != 1 ]; then
    skip_phase e2e-go
else
    run_phase e2e-go 1 bash test_e2e.sh
fi

# ══ 汇总 ═══════════════════════════════════════════════════════════════════
echo ""
echo "══════════════════════════════════════════════════════════════════"
echo "  最终结果"
echo "══════════════════════════════════════════════════════════════════"
printf "  %-22s %s\n" "阶段" "结果"
overall=0
for r in "${RESULTS[@]}"; do
    st="${r%% *}"; name="${r#* }"
    printf "  %-22s %s\n" "$name" "$st"
    [ "$st" = "FAIL" ] && overall=1
done
echo "──────────────────────────────────────────────────────────────────"
echo "  完整日志: $LOGDIR"
if [ "$overall" -eq 0 ]; then
    echo "  ✅ 全部通过 (ALL PASSED)"
else
    echo "  ❌ 存在失败 (SOME FAILED) — 见上方 [FAIL] 行对应日志"
fi
exit "$overall"
