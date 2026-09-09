#!/bin/bash
# A/B test for P99 inline-send optimization.
#
# Builds both optimized (inline-send) and baseline (notify-only) versions of
# the latency_p99 example for both tokio and smol, then runs alternating
# A/B rounds.
#
# Parameters match the spec: RPS=500, SIZE=26624, DURATION=15, WARMUP=3
set -euo pipefail
cd "$(dirname "$0")/.."
REPO=$(pwd)

RPS=500
SIZE=26624
WARMUP=3
DURATION=15
ROUNDS=4  # 4 rounds each = 8 total runs per runtime

EX_TOKIO_OPT="$REPO/target/release/examples/latency_p99_tokio_opt"
EX_TOKIO_BASE="$REPO/target/release/examples/latency_p99_tokio_base"
EX_SMOL_OPT="$REPO/target/release/examples/latency_p99_smol_opt"
EX_SMOL_BASE="$REPO/target/release/examples/latency_p99_smol_base"

CONN="$REPO/kcp-rs/src/conn.rs"
CONN_BACKUP="$REPO/kcp-rs/src/conn.rs.ab_backup"

echo "==> Step 1: Build OPTIMIZED binaries (current code with inline-send)"
cargo build -q --release -p kcp-rs --features async --example latency_p99
cp "$REPO/target/release/examples/latency_p99" "$EX_TOKIO_OPT"
cargo build -q --release -p kcp-rs --features async --example latency_p99
cp "$REPO/target/release/examples/latency_p99" "$EX_SMOL_OPT"

echo "==> Step 2: Save current conn.rs, revert to baseline (notify-only)"
cp "$CONN" "$CONN_BACKUP"
git checkout -- "$CONN"

echo "==> Step 3: Build BASELINE binaries (notify-only, no inline-send)"
cargo build -q --release -p kcp-rs --features async --example latency_p99
cp "$REPO/target/release/examples/latency_p99" "$EX_TOKIO_BASE"
cargo build -q --release -p kcp-rs --features async --example latency_p99
cp "$REPO/target/release/examples/latency_p99" "$EX_SMOL_BASE"

echo "==> Step 4: Restore optimized conn.rs"
cp "$CONN_BACKUP" "$CONN"
rm "$CONN_BACKUP"

echo "==> Step 5: Verify diff restored"
git diff --stat -- "$CONN"

# ── A/B test runner ──
run_one() {
  local bin=$1
  local label=$2
  local r
  r=$("$bin" --mode self --rps "$RPS" --warmup "$WARMUP" --duration "$DURATION" --size "$SIZE" 2>/dev/null | grep '^RESULT')
  local p50 p99 p999
  p50=$(echo "$r" | sed -n 's/.*p50_us=\([0-9.]*\).*/\1/p')
  p99=$(echo "$r" | sed -n 's/.*p99_us=\([0-9.]*\).*/\1/p')
  p999=$(echo "$r" | sed -n 's/.*p999_us=\([0-9.]*\).*/\1/p')
  printf "| %-8s | %8s | %8s | %8s |\n" "$label" "$p50" "$p99" "$p999"
}

echo ""
echo "=========================================================="
echo "smol A/B test (RPS=$RPS SIZE=$SIZE DURATION=${DURATION}s WARMUP=${WARMUP}s)"
echo "=========================================================="
echo "| Round    |     P50  |     P99  |    P999  |"
echo "|----------|----------|----------|----------|"

for i in $(seq 1 $ROUNDS); do
  run_one "$EX_SMOL_BASE" "B#$i"
  run_one "$EX_SMOL_OPT"  "O#$i"
done

echo ""
echo "=========================================================="
echo "tokio A/B test (RPS=$RPS SIZE=$SIZE DURATION=${DURATION}s WARMUP=${WARMUP}s)"
echo "=========================================================="
echo "| Round    |     P50  |     P99  |    P999  |"
echo "|----------|----------|----------|----------|"

for i in $(seq 1 $ROUNDS); do
  run_one "$EX_TOKIO_BASE" "B#$i"
  run_one "$EX_TOKIO_OPT"  "O#$i"
done

echo ""
echo "B = baseline (notify-only), O = optimized (inline-send for smol, notify for tokio)"
