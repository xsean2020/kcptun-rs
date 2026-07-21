#!/bin/bash
# kcptun-rs flamegraph / samply capture for L1–L4 loads.
#
# Usage:
#   bash bench/profile_flamegraph.sh           # all scenarios
#   bash bench/profile_flamegraph.sh l1|l2|l3|l4|all
#   BENCH_DATA_MB=100 bash bench/profile_flamegraph.sh l2
#   PROFILE_SIDE=server bash bench/profile_flamegraph.sh l2   # sample only server (default: both via wl)
#
# Produces human-readable stacks:
#   1. Builds with --profile profiling (debug=2, strip=false, lto=false)
#   2. RUSTFLAGS frame-pointers for better stacks
#   3. samply --unstable-presymbolicate
#   4. post-process with bench/symbolicate_profile.py → *.named.json.gz
#
# Open: samply load bench/profiles/L2-aes-....named.json.gz
#
# NOTE: Compatible with bash 3.2 (macOS default).
set -e
cd "$(dirname "$0")/.."
ROOT="$(pwd)"

KEY="${KEY:-bench-key}"
DATA_MB="${BENCH_DATA_MB:-50}"
LATENCY_ITERS="${BENCH_LATENCY_ITERS:-10}"
OUT_DIR="${OUT_DIR:-bench/profiles}"
# Prefer profiling profile binaries (full symbols)
PROF_SERVER="${PROF_SERVER:-$ROOT/target/profiling/kcptun-server}"
PROF_CLIENT="${PROF_CLIENT:-$ROOT/target/profiling/kcptun-client}"
RUST_SERVER="${RUST_SERVER:-$PROF_SERVER}"
RUST_CLIENT="${RUST_CLIENT:-$PROF_CLIENT}"
WL_BIN="${WL_BIN:-$ROOT/bench/kcptun_prof_wl}"
WL_SRC="$ROOT/bench/kcptun_prof_wl.rs"
SCENARIO="${1:-all}"
SYMBOLICATE="${SYMBOLICATE:-1}"

mkdir -p "$OUT_DIR"

if command -v samply >/dev/null 2>&1; then
    SAMPLER=samply
elif command -v flamegraph >/dev/null 2>&1; then
    SAMPLER=flamegraph
else
    echo "No samply/flamegraph found. Install: cargo install samply --locked"
    exit 1
fi

ensure_bins() {
    if [ "${SKIP_PROFILE_REBUILD:-0}" != "1" ]; then
        echo "Building profiling profile binaries (debug info, no strip, no LTO, frame pointers)..."
        # Frame pointers help stack walkers; aes_armv8 comes from .cargo/config.toml on aarch64.
        # Keep aes_armv8 (from .cargo/config.toml) — bare RUSTFLAGS can replace target rustflags.
        extra="-C force-frame-pointers=yes"
        case "$(uname -m)" in arm64|aarch64) extra="--cfg aes_armv8 ${extra}" ;; esac
        export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }${extra}"
        cargo build --profile profiling -p kcptun-server -p kcptun-client
    fi
    if [ ! -x "$RUST_SERVER" ] || [ ! -x "$RUST_CLIENT" ]; then
        echo "Missing profiling binaries:"
        echo "  server: $RUST_SERVER"
        echo "  client: $RUST_CLIENT"
        echo "Run: cargo build --profile profiling -p kcptun-server -p kcptun-client"
        echo "Or:  SKIP_PROFILE_REBUILD=0 bash bench/profile_flamegraph.sh l1"
        exit 1
    fi
    # Sanity: should have many named text symbols
    nsyms=$(nm -n "$RUST_SERVER" 2>/dev/null | awk '$2 ~ /^[tT]$/ {c++} END {print c+0}')
    echo "  server text symbols (nm): $nsyms"
    if [ "${nsyms:-0}" -lt 50 ]; then
        echo "  ⚠️  few text symbols — stacks may stay as 0xOFFSET; check strip/LTO settings"
    fi
}

ensure_workload() {
    if [ ! -x "$WL_BIN" ] || [ "$WL_SRC" -nt "$WL_BIN" ]; then
        echo "Building profile workload helper: $WL_BIN"
        # Frame pointers on helper too (minor); keep optimized
        rustc -C opt-level=3 -C force-frame-pointers=yes "$WL_SRC" -o "$WL_BIN"
    fi
}

timestamp() {
    date +%Y%m%d-%H%M%S
}

post_symbolicate() {
    local raw=$1
    [ "$SYMBOLICATE" = "1" ] || return 0
    [ -f "$raw" ] || return 0
    if [ ! -f "$ROOT/bench/symbolicate_profile.py" ]; then
        echo "  ⚠️  symbolicate_profile.py missing; leave raw profile"
        return 0
    fi
    python3 "$ROOT/bench/symbolicate_profile.py" "$raw" \
        --bin "$RUST_SERVER" \
        --bin "$RUST_CLIENT" \
        || echo "  ⚠️  symbolication failed (raw profile still at $raw)"
}

record_wrap() {
    local out_file=$1
    shift
    if [ "$SAMPLER" = "samply" ]; then
        # --unstable-presymbolicate: sidecar + better names when DWARF available
        # --symbol-dir: point at binary dir for local lookup
        local symdir
        symdir=$(dirname "$RUST_SERVER")
        samply record --save-only --unstable-presymbolicate \
            --symbol-dir "$symdir" \
            -o "$out_file" -- "$@"
    else
        local svg="${out_file%.json.gz}.svg"
        flamegraph -o "$svg" -- "$@"
        out_file=$svg
    fi
    if [ -f "$out_file" ]; then
        echo "  artifact=$out_file ($(wc -c < "$out_file" | tr -d ' ') bytes)"
        post_symbolicate "$out_file"
        return 0
    fi
    echo "  ⚠️  artifact missing: $out_file"
    return 1
}

profile_bulk() {
    local scen_id=$1
    local crypt=$2
    local ts
    ts=$(timestamp)
    local out_file="$OUT_DIR/${scen_id}-${crypt}-nocomp-${ts}.json.gz"

    echo "━━━ ${scen_id} crypt=$crypt sampler=$SAMPLER data=${DATA_MB}MB ━━━"
    if record_wrap "$out_file" \
        "$WL_BIN" "$crypt" "$DATA_MB" "$RUST_SERVER" "$RUST_CLIENT" "$LATENCY_ITERS"; then
        echo "  git=$(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
        echo "  ✓ ${scen_id}/${crypt} done"
        if [ -f "${out_file%.json.gz}.named.json.gz" ]; then
            echo "  named=${out_file%.json.gz}.named.json.gz"
        elif [ -f "${out_file%.json.gz}.json.named.json.gz" ]; then
            : # unused
        else
            # symbolicate_profile default naming
            named=$(ls -t "$OUT_DIR"/${scen_id}-${crypt}-nocomp-${ts}*.named.json.gz 2>/dev/null | head -1 || true)
            [ -n "$named" ] && echo "  named=$named"
        fi
    else
        echo "  git=$(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
        echo "  ⚠️  ${scen_id}/${crypt} incomplete"
    fi
    echo ""
}

profile_l4_stress() {
    local ts
    ts=$(timestamp)
    local out_file="$OUT_DIR/L4-stress-${ts}.json.gz"
    echo "━━━ L4 stress (test_multithread_10_connections) sampler=$SAMPLER ━━━"

    if [ "$SAMPLER" = "samply" ]; then
        # Build stress test with profiling profile if possible
        cargo test --profile profiling -p kcptun-server --test stress_test --no-run >/dev/null 2>&1 \
            || cargo test --release -p kcptun-server --test stress_test --no-run >/dev/null
        local test_bin
        test_bin=$(ls -t target/profiling/deps/stress_test-* 2>/dev/null | head -1 || true)
        if [ -z "$test_bin" ]; then
            test_bin=$(ls -t target/release/deps/stress_test-* 2>/dev/null | head -1 || true)
        fi
        if [ -n "$test_bin" ] && [ -x "$test_bin" ]; then
            if ! record_wrap "$out_file" "$test_bin" test_multithread_10_connections --nocapture --test-threads=1; then
                echo "  ⚠️  L4 stress profile failed"
            fi
        else
            echo "  ⚠️  could not locate stress_test binary"
        fi
    else
        flamegraph -o "${out_file%.json.gz}.svg" -- \
            cargo test --release -p kcptun-server --test stress_test \
            test_multithread_10_connections -- --nocapture --test-threads=1 \
            || echo "  ⚠️  stress under flamegraph exited non-zero"
        out_file="${out_file%.json.gz}.svg"
        [ -f "$out_file" ] && echo "  artifact=$out_file"
    fi
    echo "  git=$(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
    echo "  ✓ L4 done"
    echo ""
}

run_scenario() {
    case "$1" in
        l1|L1) profile_bulk L1 null ;;
        l2|L2) profile_bulk L2 aes ;;
        l3|L3) profile_bulk L3 3des ;;
        l4|L4) profile_l4_stress ;;
        all)
            profile_bulk L1 null
            profile_bulk L2 aes
            profile_bulk L3 3des
            profile_l4_stress
            ;;
        *)
            echo "Usage: bash bench/profile_flamegraph.sh [l1|l2|l3|l4|all]"
            exit 1
            ;;
    esac
}

echo "╔══════════════════════════════════════════════════════════════════╗"
echo "║          kcptun-rs flamegraph capture                            ║"
echo "╠══════════════════════════════════════════════════════════════════╣"
echo "║  scenario:   $SCENARIO"
echo "║  data:       ${DATA_MB} MB"
echo "║  sampler:    $SAMPLER"
echo "║  server:     $RUST_SERVER"
echo "║  client:     $RUST_CLIENT"
echo "║  out:        $OUT_DIR"
echo "║  symbolicate:$SYMBOLICATE"
echo "╚══════════════════════════════════════════════════════════════════╝"
echo ""

ensure_bins
ensure_workload
run_scenario "$SCENARIO"
echo "Profiling complete."
echo "  Open named profile:  samply load bench/profiles/<file>.named.json.gz"
echo "  Or raw:              samply load bench/profiles/<file>.json.gz"
echo "  Speedscope:          https://www.speedscope.app  (drag the .named.json.gz)"
