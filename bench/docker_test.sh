#!/bin/bash
# Linux verification script for kcptun-rs (run inside Docker)
# Verifies: build (tokio+smol), tests (incl. sendmmsg), clippy, fmt
set -eo pipefail
cd /workspace

export CARGO_TARGET_DIR=/workspace/target/linux-docker

echo "╔══════════════════════════════════════════════════════════╗"
echo "║  kcptun-rs Linux verification (rust:latest Docker)       ║"
echo "╚══════════════════════════════════════════════════════════╝"
echo ""
echo "=== OS info ==="
uname -a
cat /etc/os-release | head -3
echo ""
echo "=== Rust version ==="
rustc --version
echo ""

# Install missing components and system deps
rustup component add rustfmt clippy 2>/dev/null || true
true
export CC=gcc

# ─── 1. Format check ───
echo "=== [1/6] cargo fmt --check ==="
cargo fmt --all -- --check
echo "  ✅ fmt clean"
echo ""

# ─── 2. Build (tokio) ───
echo "=== [2/6] cargo build --workspace (tokio) ==="
cargo build --workspace 2>&1 | tail -3
echo "  ✅ tokio build"
echo ""

# ─── 3. Build (smol) ───
echo "=== [3/6] cargo build --features smol (smol) ==="
cargo build --no-default-features --features smol -p knet-rs -p kcptun-server -p kcptun-client -p kcptun-common -p smux-rs 2>&1 | tail -3
echo "  ✅ smol build"
echo ""

# ─── 4. Tests (tokio) — includes Linux sendmmsg/recvmmsg ───
echo "=== [4/6] cargo test --workspace (tokio, includes sendmmsg) ==="
cargo test --workspace 2>&1 | grep -E "test result|sendmmsg|recvmmsg|FAILED|error\[" | head -20
echo ""

# ─── 5. Tests (smol) ───
echo "=== [5/6] cargo test --features smol (smol, serial) ==="
cargo test --no-default-features --features smol -p knet-rs -- --test-threads=1 2>&1 | grep -E "test result|FAILED|error\[" | head -10
echo ""

# ─── 6. Clippy ───
echo "=== [6/6] cargo clippy -- -D warnings ==="
cargo clippy --workspace -- -D warnings 2>&1 | tail -3
echo "  ✅ clippy clean"
echo ""

echo "╔══════════════════════════════════════════════════════════╗"
echo "║  ✅ Linux verification PASSED                            ║"
echo "╚══════════════════════════════════════════════════════════╝"
echo ""
echo "=== sendmmsg/recvmmsg test details ==="
cargo test -p knet-rs -- --nocapture 2>&1 | grep -iE "sendmmsg|recvmmsg|mmsg" | head -10
