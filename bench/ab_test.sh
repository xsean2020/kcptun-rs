#!/bin/bash
# A/B test harness for P99/P999 optimization verification
# Usage: bash bench/ab_test.sh <label> <connections> <rps> <duration> <runs>
# Outputs: bench/results_<label>.txt

set -e

LABEL="${1:-baseline}"
CONNS="${2:-8}"
RPS="${3:-5000}"
DURATION="${4:-30}"
RUNS="${5:-5}"
SIZE="${6:-1024}"
WARMUP="${7:-5}"

OUTFILE="bench/results_${LABEL}.txt"
echo "# A/B test: $LABEL conns=$CONNS rps=$RPS dur=$DURATION runs=$RUNS size=$SIZE warmup=$WARMUP" > "$OUTFILE"
echo "# timestamp: $(date -u +%Y-%m-%dT%H:%M:%SZ)" >> "$OUTFILE"
echo "# ---" >> "$OUTFILE"

for i in $(seq 1 "$RUNS"); do
    echo "Run $i/$RUNS..." >&2
    target/release/examples/multi_conn_latency \
        --connections "$CONNS" \
        --rps "$RPS" \
        --warmup "$WARMUP" \
        --duration "$DURATION" \
        --size "$SIZE" 2>&1 | grep "^RESULT" >> "$OUTFILE" || true
done

echo "# ---" >> "$OUTFILE"
echo "# Done: $RUNS runs" >> "$OUTFILE"
echo "Results saved to $OUTFILE" >&2
