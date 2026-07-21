#!/bin/bash
# Profile Go kcptun with the official Go toolchain (net/http/pprof + go tool pprof).
#
# Go path (compatible with go tool pprof / go flame UI):
#   - Go server/client:  --pprof  →  :6060/debug/pprof/*
#   - Capture:           curl .../debug/pprof/profile?seconds=N
#   - View:              go tool pprof -http=:0 cpu.pb.gz
#
# Rust cannot produce native Go pprof protobuf from samply without conversion.
# For Rust readable stacks: bash bench/profile_flamegraph.sh → *.named.json.gz
# Compare Go vs Rust by profiling the same bulk load on each stack separately.
#
# Usage:
#   bash bench/profile_go_pprof.sh              # Go server, aes bulk
#   bash bench/profile_go_pprof.sh client       # Go client
#   bash bench/profile_go_pprof.sh server 30    # 30s CPU sample
#   CRYPT=null bash bench/profile_go_pprof.sh
#
# Requires: go on PATH, tests/kcptun-go/{server,client}, python3
set -e
cd "$(dirname "$0")/.."

SIDE="${1:-server}"
SECONDS_N="${2:-20}"
KEY="${KEY:-bench-key}"
DATA_MB="${BENCH_DATA_MB:-50}"
CRYPT="${CRYPT:-aes}"
MODE="${MODE:-fast}"
SNDWND="${SNDWND:-1024}"
RCVWND="${RCVWND:-1024}"
SMUXVER="${SMUXVER:-2}"
OUT_DIR="${OUT_DIR:-bench/profiles}"
GO_SERVER="${GO_SERVER:-./tests/kcptun-go/server}"
GO_CLIENT="${GO_CLIENT:-./tests/kcptun-go/client}"
COMMON="--crypt $CRYPT --nocomp --mode $MODE --sndwnd $SNDWND --rcvwnd $RCVWND --smuxver $SMUXVER"

mkdir -p "$OUT_DIR"

if ! command -v go >/dev/null 2>&1; then
    echo "go not found on PATH"
    exit 1
fi
if [ ! -x "$GO_SERVER" ] || [ ! -x "$GO_CLIENT" ]; then
    echo "Missing Go binaries: $GO_SERVER / $GO_CLIENT"
    exit 1
fi

PORT_BASE=$((32000 + (RANDOM % 500) * 3))
ECHO_PORT=$PORT_BASE
SERVER_PORT=$((PORT_BASE + 1))
CLIENT_PORT=$((PORT_BASE + 2))
ECHO_PID=""
SERVER_PID=""
CLIENT_PID=""

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
OUT_PB="$OUT_DIR/go-${SIDE}-${CRYPT}-${TS}.pb.gz"

case "$SIDE" in
    server)
        "$GO_SERVER" -l "0.0.0.0:$SERVER_PORT" -t "127.0.0.1:$ECHO_PORT" \
            --key "$KEY" $COMMON --pprof >/tmp/go-prof-server.log 2>&1 &
        SERVER_PID=$!
        sleep 0.6
        "$GO_CLIENT" -l "127.0.0.1:$CLIENT_PORT" -r "127.0.0.1:$SERVER_PORT" \
            --key "$KEY" $COMMON >/tmp/go-prof-client.log 2>&1 &
        CLIENT_PID=$!
        ;;
    client)
        "$GO_SERVER" -l "0.0.0.0:$SERVER_PORT" -t "127.0.0.1:$ECHO_PORT" \
            --key "$KEY" $COMMON >/tmp/go-prof-server.log 2>&1 &
        SERVER_PID=$!
        sleep 0.5
        "$GO_CLIENT" -l "127.0.0.1:$CLIENT_PORT" -r "127.0.0.1:$SERVER_PORT" \
            --key "$KEY" $COMMON --pprof >/tmp/go-prof-client.log 2>&1 &
        CLIENT_PID=$!
        ;;
    *)
        echo "Usage: bash bench/profile_go_pprof.sh [server|client] [seconds]"
        exit 1
        ;;
esac

PPROF_URL="http://127.0.0.1:6060/debug/pprof/profile?seconds=${SECONDS_N}"

tries=50
while [ $tries -gt 0 ]; do
    if python3 -c "import socket,sys; s=socket.socket(); s.settimeout(0.2)
try:
  s.connect(('127.0.0.1',$CLIENT_PORT)); s.close()
except: sys.exit(1)
" 2>/dev/null; then
        break
    fi
    sleep 0.1
    tries=$((tries - 1))
done
if [ $tries -le 0 ]; then
    echo "client not ready; logs:"
    tail -20 /tmp/go-prof-server.log /tmp/go-prof-client.log 2>/dev/null || true
    exit 1
fi

echo "╔══════════════════════════════════════════════════════════════════╗"
echo "║  Go pprof  side=$SIDE  crypt=$CRYPT  ${SECONDS_N}s CPU           ║"
echo "║  $PPROF_URL"
echo "╚══════════════════════════════════════════════════════════════════╝"

(
    if curl -fsS -o "$OUT_PB" "$PPROF_URL"; then
        echo "  saved $OUT_PB via curl"
    else
        echo "  curl failed; trying go tool pprof"
        go tool pprof -proto -output "$OUT_PB" "$PPROF_URL" || true
    fi
) &
PROF_PID=$!

python3 bench/throughput.py "$CLIENT_PORT" \
    --data-mb "$DATA_MB" --chunk-kb 128 --latency-iterations 5 \
    || echo "  ⚠️  throughput finished early/failed"

wait $PROF_PID 2>/dev/null || true

if [ ! -s "$OUT_PB" ]; then
    echo "  ❌ no profile at $OUT_PB"
    tail -30 /tmp/go-prof-server.log /tmp/go-prof-client.log 2>/dev/null || true
    exit 1
fi

echo "  artifact=$OUT_PB ($(wc -c < "$OUT_PB" | tr -d ' ') bytes)"
echo "  git=$(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
echo ""
echo "View with Go toolchain (function names are Go symbols, not 0x addresses):"
echo "  go tool pprof -http=127.0.0.1:0 $OUT_PB"
echo "  go tool pprof -top $OUT_PB"
echo "  go tool pprof -list=Encrypt $OUT_PB"
echo ""
echo "SVG (needs graphviz): go tool pprof -svg -output ${OUT_PB%.pb.gz}.svg $OUT_PB"
echo ""
echo "Rust readable flamegraphs: bash bench/profile_flamegraph.sh l2"
echo "  → open *.named.json.gz with: samply load <file>  or speedscope.app"
