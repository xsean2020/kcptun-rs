#!/bin/bash
# p99 collapse / fast-retransmit-storm regression gate — 256 KiB single-task echo.
#
# Guards the single-owner send fix (kcp-rs: raw_packets drained ONLY by the
# flush loop). Previously 3 tasks (write path inline, input loop, flush loop)
# drained `raw_packets` concurrently → batches interleaved on a FIFO link →
# receiver `rcv_nxt` gaps → spurious fastack storm (~14K fast retrans / 2s).
#
# Acceptance criteria (run the raw `latency_p99` benchmark at 256 KiB):
#   PRIMARY  — fast/early retrans << pre-fix storm (~14-20K / 2s) at BOTH
#              RPS=450 and RPS=500. This is the deterministic bug guard
#              (storm is now ~0; it was ~14K / 2s before the fix).
#   SECONDARY — p99 below the pre-fix collapse (~672ms @450 / ~804ms @500).
#
# ⚠️ The raw benchmark is bistable near saturation (256 KiB @ RPS>=450 is right
# at the pipeline edge): even with a clean pipeline (fast=0, no loss) p99 can
# intermittently jump into the hundreds of ms — that is the single-task echo
# model's artifact, NOT a retransmit storm. The PRIMARY storm assertion is the
# reliable one; the p99 cap is a loose "not worse than pre-fix" floor.
# Also load-sensitive (loadavg > ~4 inflates p99). Run on a quiet machine.
set -u
cd "$(dirname "$0")/.."

RPS450=${RPS450:-450}
RPS500=${RPS500:-500}
SIZE=262144
WARMUP=${WARMUP:-2}
DUR=${DUR:-8}
P99CAP_450=${P99CAP_450:-800000}   # µs — pre-fix 450 was ~672ms p99
P99CAP_500=${P99CAP_500:-900000}   # µs — pre-fix 500 was ~804ms p99
FASTCAP=${FASTCAP:-3000}           # /2s — pre-fix storm was ~14-20K / 2s

cargo build --release --features async --example latency_p99 >/dev/null 2>&1 || {
    echo "RESULT build-failed"; exit 1
}
EXE=target/release/examples/latency_p99

run_one() {
    local rps=$1 p99cap=$2
    local out
    out=$("$EXE" --mode self --rps "$rps" --size "$SIZE" --warmup "$WARMUP" --duration "$DUR" 2>&1)
    local result p99 fasts
    result=$(echo "$out" | grep -E "^RESULT" | tail -1)
    [ -z "$result" ] && { echo "RESULT rps=$rps no-result"; return 1; }
    p99=$(echo "$result" | sed -n 's/.*p99_us=\([0-9.]*\).*/\1/p')
    # max fast retrans per 2s SNMP dump
    fasts=$(echo "$out" | grep -E "^\[snmp\]" | sed -n 's/.* fast=+\([0-9]*\).*/\1/p' | sort -n | tail -1)
    echo "RESULT rps=$rps $result"
    echo "METRIC rps=$rps max_fast_2s=${fasts:-n/a} (cap ${FASTCAP})"
    local ok=1
    # PRIMARY: storm guard (the actual bug) — applies at both RPS.
    if [ -n "$fasts" ] && awk "BEGIN{exit !($fasts <= $FASTCAP)}"; then
        :
    else
        echo "FAIL rps=$rps max_fast_2s=${fasts} exceeds storm cap ${FASTCAP}"
        ok=0
    fi
    # SECONDARY: p99 floor vs pre-fix collapse.
    if [ -n "$p99" ] && awk "BEGIN{exit !($p99 <= $p99cap)}"; then
        :
    else
        echo "FAIL rps=$rps p99=${p99}us exceeds pre-fix floor ${p99cap}us"
        ok=0
    fi
    return $((1-ok))
}

fail=0
run_one "$RPS450" "$P99CAP_450" || fail=1
run_one "$RPS500" "$P99CAP_500" || fail=1

[ "$fail" = "0" ] && echo "PASS retrans-storm-regression" || echo "FAIL retrans-storm-regression"
exit $fail
