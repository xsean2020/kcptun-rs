#!/bin/bash
# =============================================================================
# run_linux_bench.sh — One-click benchmark on Linux
#
# Tests: Go vs Rust-tokio vs Rust-smol (3-way comparison)
#
# Usage:
#   bash run_linux_bench.sh                  # full matrix (15 ciphers × 2 comp)
#   bash run_linux_bench.sh --quick          # quick mode (5 ciphers)
#   bash run_linux_bench.sh --throughput-only  # skip latency
#   bash run_linux_bench.sh --latency-only     # skip throughput
#   bash run_linux_bench.sh --conn 8 --size 2097152
#
# Prerequisites:
#   - Pre-built binaries from build_linux.sh (Go + Rust-tokio + Rust-smol)
#   - If Go not pre-built: set GO_SRC env var + Go toolchain
# =============================================================================
set -eo pipefail
cd "$(dirname "$0")"

echo "╔══════════════════════════════════════════════════════════════╗"
echo "║     kcptun-rs (tokio+smol) vs Go kcptun — Linux Benchmark    ║"
echo "╚══════════════════════════════════════════════════════════════╝"
echo ""

# Check Go binaries (pre-built or from source)
GO_SRC="${GO_SRC:-}"
HAS_PREBUILT_GO=false
if [ -x "./go/kcptun-client" ] && [ -x "./go/kcptun-server" ]; then
    HAS_PREBUILT_GO=true
    echo "✓ Go binaries (pre-built): ./go/"
elif [ -n "$GO_SRC" ] && [ -d "$GO_SRC" ]; then
    echo "✓ Go source: $GO_SRC (will build from source)"
    if ! command -v go >/dev/null 2>&1; then
        echo "ERROR: Go toolchain not found. Install: https://go.dev/dl/"
        exit 1
    fi
    echo "✓ Go: $(go version)"
else
    echo "⚠ Go binaries not pre-built, and GO_SRC not set."
    echo "  Either run build_linux.sh to pre-build, or:"
    echo "    GO_SRC=/path/to/kcptun bash run_linux_bench.sh"
fi

# Check Rust binaries
for variant in tokio smol; do
    if [ -x "./$variant/kcptun-client" ] && [ -x "./$variant/kcptun-server" ]; then
        echo "✓ Rust-$variant binaries: ./$variant/"
        file "./$variant/kcptun-client" | head -1
    else
        echo "⚠ Rust-$variant binaries not found"
    fi
done
echo ""

# Build Go binaries only if not pre-built and GO_SRC is set
if [ "$HAS_PREBUILT_GO" = false ] && [ -n "$GO_SRC" ] && [ -d "$GO_SRC" ]; then
    echo "Building Go kcptun binaries from source..."
    mkdir -p "$GO_SRC/bin"
    (cd "$GO_SRC" && go build -o bin/kcptun-server ./server && go build -o bin/kcptun-client ./client)
    echo "✓ Go binaries built"
elif [ "$HAS_PREBUILT_GO" = true ]; then
    echo "✓ Skipping Go build (pre-built binaries found)"
fi
echo ""

# Run benchmark
echo "Starting benchmark..."
echo ""

EXTRA_ARGS=""
if [ -n "$GO_SRC" ] && [ "$HAS_PREBUILT_GO" = false ]; then
    EXTRA_ARGS="--go-src $GO_SRC"
fi

python3 bench_linux_cmp.py "$@" $EXTRA_ARGS
