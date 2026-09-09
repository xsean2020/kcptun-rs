#!/usr/bin/env bash
# KIO performance A/B runner.
#
# Usage:
#   bash bench/run_kio_perf.sh [--rounds 8] [--duration 10]
#
# Runs kio benchmarks with the current (candidate) implementation.
# For A/B comparison: run once on baseline, once on candidate, then diff.
#
# Uses independent target dir to avoid clobbering user's build state.

set -euo pipefail

ROUNDS=8
DURATION=10
RUNTIME="tokio"  # or smol
TARGET_DIR="${CARGO_TARGET_DIR:-target/kio-perf}"
EXAMPLES_DIR="$(dirname "$0")/../knet-rs/examples"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --rounds) ROUNDS="$2"; shift 2 ;;
    --duration) DURATION="$2"; shift 2 ;;
    --runtime) RUNTIME="$2"; shift 2 ;;
    --target-dir) TARGET_DIR="$2"; shift 2 ;;
    *) echo "Unknown arg: $1"; exit 1 ;;
  esac
done

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

OUTDIR="bench/results/kio-perf-$(date +%Y%m%d-%H%M%S)"
mkdir -p "$OUTDIR"

echo "=== KIO Perf Runner ==="
echo "Runtime: $RUNTIME"
echo "Rounds:  $ROUNDS"
echo "Duration: ${DURATION}s per scenario"
echo "Target dir: $TARGET_DIR"
echo "Output dir: $OUTDIR"
echo

# Build release
if [[ "$RUNTIME" == "smol" ]]; then
  CARGO_TARGET_DIR="$TARGET_DIR" cargo build --release --no-default-features --features smol --examples -p knet-rs 2>&1 | tail -3
else
  CARGO_TARGET_DIR="$TARGET_DIR" cargo build --release --examples -p knet-rs 2>&1 | tail -3
fi

BIDI_BIN="$TARGET_DIR/release/examples/kio_bidi_bench"
CPU_BIN="$TARGET_DIR/release/examples/kio_cpu_block_bench"
STORM_BIN="$TARGET_DIR/release/examples/kio_connect_storm"

for round in $(seq 1 "$ROUNDS"); do
  echo "--- Round $round/$ROUNDS ---"
  
  echo "  [K2] bulk throughput..."
  "$BIDI_BIN" --scenario K2 --duration "$DURATION" > "$OUTDIR/k2_round_${round}.json" 2>&1

  echo "  [K3] backpressure..."
  "$BIDI_BIN" --scenario K3 --duration "$DURATION" > "$OUTDIR/k3_round_${round}.json" 2>&1

  echo "  [K4] cpu_block..."
  "$CPU_BIN" --jobs 500 --concurrency 4 > "$OUTDIR/k4_round_${round}.json" 2>&1

  echo "  [K5] connect storm..."
  "$STORM_BIN" --connects 50 --crypto-jobs 200 > "$OUTDIR/k5_round_${round}.json" 2>&1
done

echo
echo "=== Done. Results in $OUTDIR ==="
echo "Compare with: diff -ru <baseline_dir> $OUTDIR"
