#!/bin/bash
# smux-rs A/B performance test — fast version
# Usage: bash bench/smux_ab_test.sh <label>
set -e
cd "$(dirname "$0")/.."

BASELINE="${BASELINE:-target/smux_bench_baseline}"
OPTIMIZED="${OPTIMIZED:-target/release/examples/smux_bench}"
LABEL="${1:-opt}"

OUTFILE="bench/results_smux_${LABEL}.txt"

echo "# smux-rs A/B test: $LABEL" > "$OUTFILE"
echo "# timestamp: $(date -u +%Y-%m-%dT%H:%M:%SZ)" >> "$OUTFILE"
echo "# ---" >> "$OUTFILE"

# Fast matrix: 2 runs per config, 2s duration
CONFIGS="1_65536 4_65536 16_4096 32_1024"
LATENCY_SIZES="1024 4096"

for cfg in $CONFIGS; do
    streams=$(echo $cfg | cut -d_ -f1)
    size=$(echo $cfg | cut -d_ -f2)
    echo "  [throughput] streams=$streams size=$size" >&2
    for run in 1 2; do
        r=$("$BASELINE" --streams "$streams" --size "$size" --duration 2 2>/dev/null | grep "^RESULT" || true)
        echo "baseline $r" >> "$OUTFILE"
        r=$("$OPTIMIZED" --streams "$streams" --size "$size" --duration 2 2>/dev/null | grep "^RESULT" || true)
        echo "optimized $r" >> "$OUTFILE"
    done
done

for size in $LATENCY_SIZES; do
    echo "  [latency] size=$size" >&2
    for run in 1 2; do
        r=$("$BASELINE" --latency --size "$size" --duration 2 2>/dev/null | grep "^RESULT" || true)
        echo "baseline $r" >> "$OUTFILE"
        r=$("$OPTIMIZED" --latency --size "$size" --duration 2 2>/dev/null | grep "^RESULT" || true)
        echo "optimized $r" >> "$OUTFILE"
    done
done

echo "# Done" >> "$OUTFILE"
echo "Results:" >&2
cat "$OUTFILE" | grep RESULT >&2
