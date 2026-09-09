#!/bin/bash
# Closed-loop A/B test for P99/P999 — more stable than open-model
# Usage: bash bench/ab_test_cl.sh <label> <connections> <concurrency> <duration> <runs>
set -e

LABEL="${1:-baseline}"
CONNS="${2:-8}"
CONCURRENCY="${3:-32}"
DURATION="${4:-15}"
RUNS="${5:-10}"
SIZE="${6:-1024}"
WARMUP="${7:-5}"

OUTFILE="bench/results_cl_${LABEL}.txt"
echo "# Closed-loop A/B: $LABEL conns=$CONNS conc=$CONCURRENCY dur=$DURATION runs=$RUNS" > "$OUTFILE"
echo "# timestamp: $(date -u +%Y-%m-%dT%H:%M:%SZ)" >> "$OUTFILE"

for i in $(seq 1 "$RUNS"); do
    echo "Run $i/$RUNS..." >&2
    target/release/examples/multi_conn_latency \
        --connections "$CONNS" \
        --concurrency "$CONCURRENCY" \
        --warmup "$WARMUP" \
        --duration "$DURATION" \
        --size "$SIZE" 2>&1 | grep "^RESULT" >> "$OUTFILE" || true
done

echo "# Done: $RUNS runs" >> "$OUTFILE"
echo "Results saved to $OUTFILE" >&2
