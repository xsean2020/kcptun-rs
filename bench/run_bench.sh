#!/bin/bash
# kcptun performance benchmark: Go vs Rust-Tokio vs Rust-Smol
#
# Measures throughput (MB/s) and latency (ms RTT) for each backend.
# Requires: release builds of Rust-Tokio and Rust-Smol, and Go kcptun binaries.
#
# Usage:
#   make bench                  — full comparison
#   bash bench/run_bench.sh     — same thing
#   BENCH_DATA_MB=50 bash bench/run_bench.sh  — larger data size
#
# NOTE: Compatible with bash 3.2 (macOS default). No negative array indices.
cd "$(dirname "$0")/.."

KEY="bench-key"
DATA_MB="${BENCH_DATA_MB:-200}"
CHUNK_KB="${BENCH_KB:-128}"
LATENCY_ITERS="${BENCH_LATENCY_ITERS:-50}"

GO_SERVER=./tests/kcptun-go/server
GO_CLIENT=./tests/kcptun-go/client
RUST_TOKIO_SERVER=./target/release/kcptun-server
RUST_TOKIO_CLIENT=./target/release/kcptun-client
RUST_SMOL_SERVER=./target/smol-release/release/kcptun-server
RUST_SMOL_CLIENT=./target/smol-release/release/kcptun-client

# kcptun args — aligned on both sides for fair cross-impl comparison.
# Default Rust client sndwnd is 128 vs server 1024; leave them implicit and
# the Rust→Go path is not comparable. Force the same windows/mode/smuxver.
CRYPT="${BENCH_CRYPT:-aes}"
MODE="${BENCH_MODE:-fast}"
SNDWND="${BENCH_SNDWND:-1024}"
RCVWND="${BENCH_RCVWND:-1024}"
SMUXVER="${BENCH_SMUXVER:-2}"
COMMON_ARGS="--crypt ${CRYPT} --nocomp --mode ${MODE} --sndwnd ${SNDWND} --rcvwnd ${RCVWND} --smuxver ${SMUXVER}"
SERVER_ARGS="$COMMON_ARGS"
CLIENT_ARGS="$COMMON_ARGS"

# Port management — increment per test to avoid TIME_WAIT conflicts
PORT_COUNTER=$((30000 + (RANDOM % 1000) * 3))

ECHO_PID=""
SERVER_PID=""
CLIENT_PID=""

cleanup() {
    [ -n "$CLIENT_PID" ] && kill "$CLIENT_PID" 2>/dev/null
    [ -n "$SERVER_PID" ] && kill "$SERVER_PID" 2>/dev/null
    [ -n "$ECHO_PID" ]   && kill "$ECHO_PID"   2>/dev/null
    # Reap the killed background jobs silently — without `wait`, bash prints
    # noisy "Terminated: 15 ..." messages when each job is reaped.
    wait 2>/dev/null
    CLIENT_PID=""
    SERVER_PID=""
    ECHO_PID=""
    # Give processes time to fully exit + release ports
    sleep 0.5
}
trap cleanup EXIT

next_ports() {
    ECHO_PORT=$PORT_COUNTER
    SERVER_PORT=$((PORT_COUNTER + 1))
    CLIENT_PORT=$((PORT_COUNTER + 2))
    PORT_COUNTER=$((PORT_COUNTER + 3))
}

# Wait until a TCP port accepts connections (max ~5s).
# Returns 0 on success, 1 on timeout.
wait_for_port() {
    local port=$1
    local tries=50
    while [ $tries -gt 0 ]; do
        if python3 -c "import socket,sys; s=socket.socket(); s.settimeout(0.2)
try:
    s.connect(('127.0.0.1',$port)); s.close()
except: sys.exit(1)
" 2>/dev/null; then
            return 0
        fi
        sleep 0.1
        tries=$((tries - 1))
    done
    return 1
}

start_echo() {
    python3 -u -c "
import socket, threading
def echo(s,a):
    try:
        while True:
            d=s.recv(65536)
            if not d: break
            s.sendall(d)
    except: pass
    s.close()
s=socket.socket(); s.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1)
s.bind(('0.0.0.0',$ECHO_PORT)); s.listen(10)
while True: threading.Thread(target=echo,args=s.accept(),daemon=True).start()
" 2>/dev/null &
    ECHO_PID=$!
    sleep 0.3
    if ! kill -0 "$ECHO_PID" 2>/dev/null; then
        echo "  ❌ echo server failed on port $ECHO_PORT"
        return 1
    fi
}

start_server() {
    local bin=$1
    "$bin" -l "0.0.0.0:$SERVER_PORT" -t "127.0.0.1:$ECHO_PORT" \
        --key "$KEY" $SERVER_ARGS 2>/dev/null &
    SERVER_PID=$!
    sleep 0.5
    if ! kill -0 "$SERVER_PID" 2>/dev/null; then
        echo "  ❌ kcptun server failed on port $SERVER_PORT"
        return 1
    fi
}

start_client() {
    local bin=$1
    "$bin" -l "127.0.0.1:$CLIENT_PORT" -r "127.0.0.1:$SERVER_PORT" \
        --key "$KEY" $CLIENT_ARGS 2>/dev/null &
    CLIENT_PID=$!
    # Poll for client TCP listener (up to ~5s)
    if ! wait_for_port "$CLIENT_PORT"; then
        if ! kill -0 "$CLIENT_PID" 2>/dev/null; then
            echo "  ❌ kcptun client exited early on port $CLIENT_PORT"
        else
            echo "  ❌ kcptun client listener not ready on port $CLIENT_PORT"
        fi
        return 1
    fi
}

run_bench() {
    local label=$1
    local server_bin=$2
    local client_bin=$3

    echo "━━━ $label ━━━"

    if [ ! -x "$server_bin" ]; then
        echo "  ⏭️  Skipped (binary not found: $server_bin)"
        echo ""
        return
    fi
    if [ ! -x "$client_bin" ]; then
        echo "  ⏭️  Skipped (binary not found: $client_bin)"
        echo ""
        return
    fi

    cleanup
    next_ports

    start_echo       || { echo ""; cleanup; return; }
    start_server "$server_bin" || { echo ""; cleanup; return; }
    start_client "$client_bin" || { echo ""; cleanup; return; }

    python3 bench/throughput.py "$CLIENT_PORT" \
        --data-mb "$DATA_MB" --chunk-kb "$CHUNK_KB" \
        --latency-iterations "$LATENCY_ITERS" || \
        echo "  ❌ Benchmark failed"

    echo ""
    cleanup
}

# ═══════════════════════════════════════════════════════════════════════
# Header
# ═══════════════════════════════════════════════════════════════════════
echo "╔══════════════════════════════════════════════════════════════════╗"
echo "║          kcptun Performance Benchmark                            ║"
echo "║          Go vs Rust-Tokio vs Rust-Smol                           ║"
echo "╠══════════════════════════════════════════════════════════════════╣"
echo "║  Data:       ${DATA_MB} MB                                          ║"
echo "║  Chunk:      ${CHUNK_KB} KB                                         ║"
echo "║  Crypto:     ${CRYPT}  mode=${MODE}  smuxver=${SMUXVER}                          ║"
echo "║  Windows:    sndwnd=${SNDWND} rcvwnd=${RCVWND}  Compression: OFF (--nocomp)     ║"
echo "╚══════════════════════════════════════════════════════════════════╝"
echo ""

# ═══════════════════════════════════════════════════════════════════════
# Build check
# ═══════════════════════════════════════════════════════════════════════
echo "Checking binaries..."
echo "  Go server:      $([ -x "$GO_SERVER" ] && echo '✓' || echo '✗ (will skip)')"
echo "  Go client:      $([ -x "$GO_CLIENT" ] && echo '✓' || echo '✗ (will skip)')"
echo "  Rust-Tokio srv: $([ -x "$RUST_TOKIO_SERVER" ] && echo '✓' || echo '✗ (run: make release)')"
echo "  Rust-Tokio cli: $([ -x "$RUST_TOKIO_CLIENT" ] && echo '✓' || echo '✗ (run: make release)')"
echo "  Rust-Smol srv:  $([ -x "$RUST_SMOL_SERVER" ] && echo '✓' || echo '✗ (run: make release-smol)')"
echo "  Rust-Smol cli:  $([ -x "$RUST_SMOL_CLIENT" ] && echo '✓' || echo '✗ (run: make release-smol)')"
echo ""

# ═══════════════════════════════════════════════════════════════════════
# Run benchmarks
# ═══════════════════════════════════════════════════════════════════════
run_bench "Go → Go"                    "$GO_SERVER"           "$GO_CLIENT"
run_bench "Rust-Tokio → Rust-Tokio"    "$RUST_TOKIO_SERVER"   "$RUST_TOKIO_CLIENT"
run_bench "Rust-Smol → Rust-Smol"      "$RUST_SMOL_SERVER"    "$RUST_SMOL_CLIENT"
run_bench "Go → Rust-Tokio"            "$GO_SERVER"           "$RUST_TOKIO_CLIENT"
run_bench "Rust-Tokio → Go"            "$RUST_TOKIO_SERVER"   "$GO_CLIENT"

echo "Benchmark complete."
