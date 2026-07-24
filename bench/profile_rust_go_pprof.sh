#!/bin/bash
# Capture Rust kcptun CPU profile as **Google pprof protobuf**, then analyze
# with the official Go toolchain (`go tool pprof`).
#
# This is what you want when you said: Rust profile → Go profile format → go tools.
#
# Prerequisites:
#   cargo build --profile profiling --features pprof -p kcptun-server -p kcptun-client
#   go on PATH
#
# Usage:
#   bash bench/profile_rust_go_pprof.sh              # profile Rust server, 20s
#   bash bench/profile_rust_go_pprof.sh client 15    # profile Rust client
#   CRYPT=null bash bench/profile_rust_go_pprof.sh
#   BENCH_DATA_MB=100 bash bench/profile_rust_go_pprof.sh server 30
#
# Then:
#   go tool pprof -http=127.0.0.1:0 bench/profiles/rust-server-aes-*.pb
#   go tool pprof -top bench/profiles/rust-server-aes-*.pb
#   go tool pprof -list=encrypt_batch bench/profiles/rust-server-aes-*.pb
set -e
cd "$(dirname "$0")/.."
ROOT="$(pwd)"

SIDE="${1:-server}"          # server | client
SECONDS_N="${2:-20}"

# Support memory-focused mode: bash ... mem [side] [seconds]
# In mem mode we skip the long CPU sampling and focus on heap/allocs snapshots.
MEM_ONLY=0
if [ "$1" = "mem" ]; then
  MEM_ONLY=1
  SIDE="${2:-server}"
  SECONDS_N="${3:-10}"
fi
KEY="${KEY:-bench-key}"
DATA_MB="${BENCH_DATA_MB:-50}"
CRYPT="${CRYPT:-aes}"
MODE="${MODE:-fast}"
SNDWND="${SNDWND:-1024}"
RCVWND="${RCVWND:-1024}"
SMUXVER="${SMUXVER:-2}"
OUT_DIR="${OUT_DIR:-bench/profiles}"
PPROF_PORT="${PPROF_PORT:-16060}"
SERVER="${RUST_SERVER:-$ROOT/target/profiling/kcptun-server}"
CLIENT="${RUST_CLIENT:-$ROOT/target/profiling/kcptun-client}"
# fallback to release if profiling missing
[ -x "$SERVER" ] || SERVER="$ROOT/target/release/kcptun-server"
[ -x "$CLIENT" ] || CLIENT="$ROOT/target/release/kcptun-client"
COMMON="--crypt $CRYPT --nocomp --mode $MODE --sndwnd $SNDWND --rcvwnd $RCVWND --smuxver $SMUXVER"

mkdir -p "$OUT_DIR"

if [ ! -x "$SERVER" ] || [ ! -x "$CLIENT" ]; then
    echo "Missing binaries. Build first:"
    echo "  RUSTFLAGS='-C force-frame-pointers=yes' cargo build --profile profiling --features pprof -p kcptun-server -p kcptun-client"
    exit 1
fi
if ! command -v go >/dev/null 2>&1; then
    echo "go not found — install Go to use go tool pprof (profile file will still be saved)"
fi
if ! command -v curl >/dev/null 2>&1; then
    echo "curl required"
    exit 1
fi

PORT_BASE=$((33000 + (RANDOM % 400) * 3))
ECHO_PORT=$PORT_BASE
SERVER_PORT=$((PORT_BASE + 1))
CLIENT_PORT=$((PORT_BASE + 2))
ECHO_PID=""; SERVER_PID=""; CLIENT_PID=""

cleanup() {
    [ -n "$CLIENT_PID" ] && kill "$CLIENT_PID" 2>/dev/null || true
    [ -n "$SERVER_PID" ] && kill "$SERVER_PID" 2>/dev/null || true
    [ -n "$ECHO_PID" ] && kill "$ECHO_PID" 2>/dev/null || true
    wait 2>/dev/null || true
}
trap cleanup EXIT

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
" &
ECHO_PID=$!
sleep 0.3

TS=$(date +%Y%m%d-%H%M%S)
OUT_PB="$OUT_DIR/rust-${SIDE}-${CRYPT}-${TS}.pb"
PPROF_ADDR="127.0.0.1:${PPROF_PORT}"

case "$SIDE" in
    server)
        "$SERVER" -l "0.0.0.0:$SERVER_PORT" -t "127.0.0.1:$ECHO_PORT" \
            --key "$KEY" $COMMON --pprof "$PPROF_ADDR" >/tmp/rust-go-pprof-s.log 2>&1 &
        SERVER_PID=$!
        sleep 0.8
        "$CLIENT" -l "127.0.0.1:$CLIENT_PORT" -r "127.0.0.1:$SERVER_PORT" \
            --key "$KEY" $COMMON >/tmp/rust-go-pprof-c.log 2>&1 &
        CLIENT_PID=$!
        ;;
    client)
        "$SERVER" -l "0.0.0.0:$SERVER_PORT" -t "127.0.0.1:$ECHO_PORT" \
            --key "$KEY" $COMMON >/tmp/rust-go-pprof-s.log 2>&1 &
        SERVER_PID=$!
        sleep 0.5
        "$CLIENT" -l "127.0.0.1:$CLIENT_PORT" -r "127.0.0.1:$SERVER_PORT" \
            --key "$KEY" $COMMON --pprof "$PPROF_ADDR" >/tmp/rust-go-pprof-c.log 2>&1 &
        CLIENT_PID=$!
        ;;
    *)
        echo "Usage: bash bench/profile_rust_go_pprof.sh [server|client] [seconds]"
        exit 1
        ;;
esac

tries=50
while [ $tries -gt 0 ]; do
    if python3 -c "import socket,sys; s=socket.socket(); s.settimeout(0.2)
try:
  s.connect(('127.0.0.1',$CLIENT_PORT)); s.close()
except: sys.exit(1)
" 2>/dev/null; then break; fi
    sleep 0.1
    tries=$((tries - 1))
done
if [ $tries -le 0 ]; then
    echo "tunnel not ready"; tail -30 /tmp/rust-go-pprof-s.log /tmp/rust-go-pprof-c.log; exit 1
fi

# URL for CPU profile (Go pprof protobuf)
CPU_URL="http://${PPROF_ADDR}/debug/pprof/profile?seconds=${SECONDS_N}"
HEAP_URL="http://${PPROF_ADDR}/debug/pprof/heap"
ALLOCS_URL="http://${PPROF_ADDR}/debug/pprof/allocs"

echo "╔══════════════════════════════════════════════════════════════════╗"
echo "║  Rust → Go pprof protobuf   side=$SIDE  crypt=$CRYPT  ${SECONDS_N}s  ║"
echo "║  CPU:    $CPU_URL"
echo "║  HEAP:   $HEAP_URL"
echo "║  ALLOCS: $ALLOCS_URL"
echo "╚══════════════════════════════════════════════════════════════════╝"

# Drive load while sampling
(
    # keep traffic for ~SECONDS_N
    python3 bench/throughput.py "$CLIENT_PORT" \
        --data-mb "$DATA_MB" --chunk-kb 128 --latency-iterations 5 \
        || true
) &
LOAD_PID=$!

# Capture CPU profile (required)
if ! curl -fsS -o "$OUT_PB" "$CPU_URL"; then
    echo "curl pprof (cpu) failed"; tail -40 /tmp/rust-go-pprof-s.log /tmp/rust-go-pprof-c.log; exit 1
fi

# Capture heap + allocs (Go pprof memory profiles)
# These are instantaneous snapshots; capture after load settles a bit.
HEAP_PB="$OUT_DIR/rust-${SIDE}-${CRYPT}-${TS}-heap.pb"
ALLOCS_PB="$OUT_DIR/rust-${SIDE}-${CRYPT}-${TS}-allocs.pb"

curl -fsS -o "$HEAP_PB" "$HEAP_URL" || echo "  (heap capture failed or empty)"
curl -fsS -o "$ALLOCS_PB" "$ALLOCS_URL" || echo "  (allocs capture failed or empty)"

wait $LOAD_PID 2>/dev/null || true

echo "  artifact(cpu)=$OUT_PB ($(wc -c < "$OUT_PB" 2>/dev/null | tr -d ' ') bytes)"
[ -s "$HEAP_PB" ] && echo "  artifact(heap)=$HEAP_PB ($(wc -c < "$HEAP_PB" 2>/dev/null | tr -d ' ') bytes)"
[ -s "$ALLOCS_PB" ] && echo "  artifact(allocs)=$ALLOCS_PB ($(wc -c < "$ALLOCS_PB" 2>/dev/null | tr -d ' ') bytes)"
echo "  git=$(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
echo ""
if command -v go >/dev/null 2>&1; then
    echo "=== go tool pprof -top (function names) ==="
    go tool pprof -top "$OUT_PB" 2>&1 | head -35
    echo ""
    echo "Interactive flame graph UI:"
    echo "  go tool pprof -http=127.0.0.1:0 $OUT_PB"
    echo "  # then open Flame Graph in the browser"
else
    echo "Saved protobuf only. Install Go and run:"
    echo "  go tool pprof -http=127.0.0.1:0 $OUT_PB"
fi
