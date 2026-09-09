#!/bin/bash
# A/B test matrix for P999 root cause diagnosis.
#
# Tests 10 configurations to isolate the source of tokio P999 tail latency.
# Each test runs 20s at 500 RPS with --diag (watchdog + histogram).
#
# Usage: bash bench/ab_test_p999.sh
# Output: RESULT + HISTOGRAM + WATCHDOG lines per test, plus a summary table.
set -euo pipefail
cd "$(dirname "$0")/.."
REPO=$(pwd)

EX_TOKIO="$REPO/target/release/examples/latency_p99_tokio"
EX_SMOL="$REPO/target/release/examples/latency_p99_smol"

echo "==> Building tokio variant (with diag instrumentation)..."
cargo build -q --release -p kcp-rs --features async --example latency_p99
cp "$REPO/target/release/examples/latency_p99" "$EX_TOKIO"

echo "==> Building smol variant..."
cargo build -q --release -p kcp-rs --features async --example latency_p99
cp "$REPO/target/release/examples/latency_p99" "$EX_SMOL"

WARMUP=3
DURATION=20
SIZE=26624
RPS=500
COMMON="--mode self --rps $RPS --warmup $WARMUP --duration $DURATION --size $SIZE --diag"

RESULTS_FILE="$REPO/bench/ab_test_results.txt"
> "$RESULTS_FILE"

run_test() {
    local label=$1; shift
    echo ""
    echo "================================================================"
    echo "=== $label ==="
    echo "================================================================"
    local output
    output=$("$@" 2>&1 || true)
    echo "$output" | grep -E "^RESULT|^HISTOGRAM|^WATCHDOG" || echo "(no output)"
    echo "$output" | grep -E "^RESULT|^HISTOGRAM" >> "$RESULTS_FILE"
    local wc_gap
    wc_gap=$(echo "$output" | grep -c "^WATCHDOG" || true)
    local max_gap
    max_gap=$(echo "$output" | grep "^WATCHDOG" | sed 's/.*SCHED_GAP=//;s/us//' | sort -n | tail -1 || echo 0)
    echo "  watchdog_stalls=$wc_gap max_sched_gap=${max_gap}us"
    echo "  watchdog_stalls=$wc_gap max_sched_gap=${max_gap}us" >> "$RESULTS_FILE"
    echo "---"
}

# ── A group: tokio variants ──────────────────────────────────────────────

run_test "A1: tokio multi-worker (baseline)" \
    "$EX_TOKIO" $COMMON

run_test "A2: tokio single-worker (--rt single)" \
    "$EX_TOKIO" $COMMON --rt single

run_test "A3: tokio multi 2 workers" \
    "$EX_TOKIO" $COMMON --workers 2

run_test "A4: tokio multi 4 workers" \
    "$EX_TOKIO" $COMMON --workers 4

run_test "A5: tokio multi + KCP_FORCE_INLINE_SEND=1" \
    env KCP_FORCE_INLINE_SEND=1 "$EX_TOKIO" $COMMON

run_test "A6: tokio multi + event-driven reader (--reader-poll 0)" \
    "$EX_TOKIO" $COMMON --reader-poll 0

run_test "A7: tokio multi + 1ms reader poll" \
    "$EX_TOKIO" $COMMON --reader-poll 1000

run_test "A8: tokio single + inline-send" \
    env KCP_FORCE_INLINE_SEND=1 "$EX_TOKIO" $COMMON --rt single

run_test "A9: tokio 2w + inline + event-driven (combined)" \
    env KCP_FORCE_INLINE_SEND=1 "$EX_TOKIO" $COMMON --workers 2 --reader-poll 0

# ── B group: smol baseline ───────────────────────────────────────────────

run_test "B1: smol (baseline)" \
    "$EX_SMOL" $COMMON

# ── Summary ──────────────────────────────────────────────────────────────
echo ""
echo "================================================================"
echo "=== SUMMARY ==="
echo "================================================================"
echo ""
cat "$RESULTS_FILE"
echo ""
echo "Full results saved to: $RESULTS_FILE"
