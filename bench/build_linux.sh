#!/bin/bash
# =============================================================================
# build_linux.sh — Cross-compile Go + Rust (tokio + smol) kcptun for Linux
#
# Produces: Go binaries + Rust-tokio + Rust-smol (all static musl, release)
# Copy the output directory to a Linux machine and run run_linux_bench.sh.
#
# Usage:
#   bash bench/build_linux.sh                           # x86_64 (default)
#   bash bench/build_linux.sh --aarch64                 # aarch64 only
#   bash bench/build_linux.sh --both                    # x86_64 + aarch64
#   bash bench/build_linux.sh --go-src /path/to/kcptun  # Go source path
#   bash bench/build_linux.sh --rust-only              # skip Go
#   bash bench/build_linux.sh --go-only                 # skip Rust
# =============================================================================
set -eo pipefail
cd "$(dirname "$0")/.."

ARCH="x86_64"
GO_SRC="${KCPTUN_GO_SRC:-}"
BUILD_RUST=true
BUILD_GO=true

while [[ $# -gt 0 ]]; do
    case "$1" in
        --aarch64)  ARCH="aarch64";  shift ;;
        --both)     ARCH="both";     shift ;;
        --go-src)   GO_SRC="$2";     shift 2 ;;
        --rust-only) BUILD_GO=false;  shift ;;
        --go-only)  BUILD_RUST=false; shift ;;
        *) echo "unknown option: $1"; exit 1 ;;
    esac
done

RT_PKGS="-p kcptun-client -p kcptun-server -p knet-rs -p smux-rs -p kpprof-rs"
DIST_DIR="dist/linux"
NUM_JOBS="$(getconf _NPROCESSORS_ONLN 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || echo 4)"

# ─── Go cross-compile ──────────────────────────────────────────────────────

build_go() {
    local goos="linux"
    local goarch="$1"

    if [ "$BUILD_GO" = false ]; then
        echo "==> Skipping Go (—go-only)"
        return 0
    fi
    if ! command -v go >/dev/null 2>&1; then
        echo "WARNING: Go toolchain not found — skipping Go binaries"
        return 0
    fi
    if [ -z "$GO_SRC" ]; then
        echo "WARNING: Go source path not set — skipping Go binaries"
        echo "  Set KCPTUN_GO_SRC env var or --go-src to the Go kcptun source directory"
        return 0
    fi
    if [ ! -d "$GO_SRC" ] || [ ! -f "$GO_SRC/server/main.go" ]; then
        echo "WARNING: Go kcptun source not found at $GO_SRC — skipping Go binaries"
        return 0
    fi

    echo "==> Cross-compiling Go kcptun for ${goos}/${goarch}..."
    (
        cd "$GO_SRC"
        GOOS="$goos" GOARCH="$goarch" CGO_ENABLED=0 \
            go build -mod=vendor -trimpath -ldflags="-s -w" \
            -o "$OLDPWD/$DIST_DIR/go/kcptun-server" ./server
        GOOS="$goos" GOARCH="$goarch" CGO_ENABLED=0 \
            go build -mod=vendor -trimpath -ldflags="-s -w" \
            -o "$OLDPWD/$DIST_DIR/go/kcptun-client" ./client
    )
    echo "==> Go binaries → $DIST_DIR/go/"
    ls -lh "$DIST_DIR/go/kcptun-client" "$DIST_DIR/go/kcptun-server"
}

# ─── Rust cross-compile ────────────────────────────────────────────────────

build_rust() {
    local target="$1"
    local runtime="$2"   # "tokio" or "smol"
    local features="$3"
    local target_dir="$4"
    local out_subdir="$5"

    if [ "$BUILD_RUST" = false ]; then
        echo "==> Skipping Rust (—go-only)"
        return 0
    fi

    local cc=""
    local prefix=""
    case "$target" in
        x86_64-unknown-linux-musl)
            cc="x86_64-linux-musl-gcc"; prefix="x86_64-linux-musl" ;;
        aarch64-unknown-linux-musl)
            cc="aarch64-linux-musl-gcc"; prefix="aarch64-linux-musl" ;;
    esac

    echo "==> Adding rustup target: $target"
    rustup target add "$target" 2>/dev/null || true

    if ! command -v "$cc" >/dev/null 2>&1; then
        echo "ERROR: C cross-compiler '$cc' not found."
        echo "  macOS: brew install filosottile/musl-cross/musl-cross"
        echo "  Linux: apt install musl-tools"
        return 1
    fi

    local upper_target
    upper_target=$(echo "$target" | tr '[:lower:]' '[:upper:]' | tr '-' '_')
    local linker_var="CARGO_TARGET_${upper_target}_LINKER"

    # tokio = default features (no --no-default-features)
    # smol  = --no-default-features --features "smol,qpp,pprof"
    local no_default=""
    if [ "$runtime" = "smol" ]; then
        no_default="--no-default-features"
    fi

    # Cross-compiling from ARM64 macOS → x86_64 Linux: LLVM cannot detect
    # the target CPU, so it defaults to the x86_64 baseline (SSE2 only).
    # Force x86-64-v3 to enable AVX2 + BMI2 + FMA — matching what a native
    # build would produce. This is critical for crypto throughput.
    local rustflags_extra=""
    case "$target" in
        x86_64-*)  rustflags_extra="-C target-cpu=x86-64-v3" ;;
    esac

    echo "==> Cross-compiling Rust ${runtime} for $target (release, ${rustflags_extra:-default cpu})..."

    RUSTFLAGS="${rustflags_extra}" \
    env "${linker_var}=${cc}" \
        "CC_${target}=${cc}" \
        "CXX_${target}=${cc}" \
        "AR_${target}=${prefix}-ar" \
        cargo build $RT_PKGS \
            $no_default --features "$features" \
            --release --target "$target" \
            --target-dir "$target_dir" \
            -j "$NUM_JOBS"

    local out_dir="$DIST_DIR/$out_subdir"
    mkdir -p "$out_dir"
    cp "$target_dir/$target/release/kcptun-client" "$out_dir/"
    cp "$target_dir/$target/release/kcptun-server" "$out_dir/"
    echo "==> Rust ${runtime} binaries → $out_dir/"
    ls -lh "$out_dir/kcptun-client" "$out_dir/kcptun-server"
}

# ─── Main ──────────────────────────────────────────────────────────────────

mkdir -p "$DIST_DIR/go"

# Determine target triple
TARGET=""
case "$ARCH" in
    x86_64)  TARGET="x86_64-unknown-linux-musl" ;;
    aarch64) TARGET="aarch64-unknown-linux-musl" ;;
    both)    TARGET="x86_64-unknown-linux-musl" ;;  # handled below
esac

# Build Rust tokio + smol
if [ "$BUILD_RUST" = true ]; then
    case "$ARCH" in
        x86_64)
            build_rust "$TARGET" "tokio" "qpp,pprof" "target/linux-tokio" "tokio"
            echo ""
            build_rust "$TARGET" "smol"  "smol,qpp,pprof" "target/linux-smol" "smol"
            ;;
        aarch64)
            build_rust "$TARGET" "tokio" "qpp,pprof" "target/linux-tokio" "tokio"
            echo ""
            build_rust "$TARGET" "smol"  "smol,qpp,pprof" "target/linux-smol" "smol"
            ;;
        both)
            build_rust "x86_64-unknown-linux-musl" "tokio" "qpp,pprof" "target/linux-tokio" "tokio"
            echo ""
            build_rust "x86_64-unknown-linux-musl" "smol"  "smol,qpp,pprof" "target/linux-smol" "smol"
            echo ""
            build_rust "aarch64-unknown-linux-musl" "tokio" "qpp,pprof" "target/linux-tokio-arm" "tokio-arm"
            echo ""
            build_rust "aarch64-unknown-linux-musl" "smol"  "smol,qpp,pprof" "target/linux-smol-arm" "smol-arm"
            ;;
    esac
fi

echo ""

# Build Go
build_go amd64
if [ "$ARCH" = "both" ]; then
    build_go arm64
fi

# Copy benchmark scripts
cp bench/bench_linux_cmp.py "$DIST_DIR/"
cp bench/run_linux_bench.sh "$DIST_DIR/"
cp bench/throughput.py "$DIST_DIR/" 2>/dev/null || true

echo ""
echo "==> Done. Copy $DIST_DIR/ to the Linux machine."
echo ""
echo "    Layout:"
echo "      $DIST_DIR/"
echo "        ├── bench_linux_cmp.py    — benchmark script"
echo "        ├── run_linux_bench.sh    — one-click runner"
echo "        ├── go/                   — Go Linux binaries"
echo "        ├── tokio/                — Rust tokio binaries"
echo "        └── smol/                 — Rust smol binaries"
echo ""
echo "    On Linux:  cd dist/linux && bash run_linux_bench.sh"
