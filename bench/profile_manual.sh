#!/bin/bash
set -e
cd "$(dirname "$0")/.."
ROOT="$(pwd)"

SECONDS_N="${1:-20}"
CRYPT="${CRYPT:-null}"
DATA_MB="${BENCH_DATA_MB:-100}"
ECHO_PORT=38001
SERVER_PORT=38002
CLIENT_PORT=38003

cleanup() {
    [ -n "${CLIENT_PID:-}" ] && kill "$CLIENT_PID" 2>/dev/null || true
    [ -n "${SERVER_PID:-}" ] && kill "$SERVER_PID" 2>/dev/null || true
    [ -n "${ECHO_PID:-}" ] && kill "$ECHO_PID" 2>/dev/null || true
    wait 2>/dev/null || true
}
trap cleanup EXIT

# 1. Kill anything on 6060
pkill -f "pprof" 2>/dev/null || true
lsof -ti :6060 2>/dev/null | xargs kill 2>/dev/null || true
sleep 0.5

# 2. Start echo server
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
s.bind(('0.0.0.0',${ECHO_PORT})); s.listen(10)
while True: threading.Thread(target=echo,args=s.accept(),daemon=True).start()
" &
ECHO_PID=$!
sleep 0.3

# 3. Start Rust server with pprof
"$ROOT/target/profiling/kcptun-server" -l "0.0.0.0:${SERVER_PORT}" -t "127.0.0.1:${ECHO_PORT}" \
    --key bench-key --crypt "$CRYPT" --nocomp --mode fast --sndwnd 1024 --rcvwnd 1024 --smuxver 2 --pprof >/tmp/kio-prof-s.log 2>&1 &
SERVER_PID=$!
sleep 1

# 4. Verify pprof endpoint is Rust (check for Rust-specific patterns)
PPROF_CHECK=$(curl -s http://127.0.0.1:6060/debug/pprof/ 2>&1)
if echo "$PPROF_CHECK" | grep -qi "allocs\|profile\|heap"; then
    echo "✅ pprof endpoint OK (Rust server)"
else
    echo "❌ pprof endpoint check failed - port 6060 may be occupied by another process"
    echo "Response: $PPROF_CHECK"
    exit 1
fi

# 5. Start client
"$ROOT/target/profiling/kcptun-client" -l "127.0.0.1:${CLIENT_PORT}" -r "127.0.0.1:${SERVER_PORT}" \
    --key bench-key --crypt "$CRYPT" --nocomp --mode fast --sndwnd 1024 --rcvwnd 1024 --smuxver 2 >/tmp/kio-prof-c.log 2>&1 &
CLIENT_PID=$!
sleep 1

# 6. Generate load in background
python3 bench/throughput.py "$CLIENT_PORT" --data-mb "$DATA_MB" --chunk-kb 128 --latency-iterations 5 >/tmp/kio-prof-load.log 2>&1 &
LOAD_PID=$!

# 7. Capture CPU profile
TS=$(date +%Y%m%d-%H%M%S)
OUT_PB="bench/profiles/rust-server-${CRYPT}-${TS}.pb"
echo "=== Capturing ${SECONDS_N}s CPU profile (crypt=$CRYPT, data=${DATA_MB}MB) ==="
curl -fsS -o "$OUT_PB" "http://127.0.0.1:6060/debug/pprof/profile?seconds=${SECONDS_N}"
echo "  artifact=$OUT_PB ($(wc -c < "$OUT_PB" | tr -d ' ') bytes)"

# Capture heap + allocs
HEAP_PB="bench/profiles/rust-server-${CRYPT}-${TS}-heap.pb"
ALLOCS_PB="bench/profiles/rust-server-${CRYPT}-${TS}-allocs.pb"
curl -fsS -o "$HEAP_PB" "http://127.0.0.1:6060/debug/pprof/heap" || true
curl -fsS -o "$ALLOCS_PB" "http://127.0.0.1:6060/debug/pprof/allocs" || true
echo "  heap=$(wc -c < "$HEAP_PB" | tr -d ' ') bytes, allocs=$(wc -c < "$ALLOCS_PB" | tr -d ' ') bytes"

wait $LOAD_PID 2>/dev/null || true

echo ""
echo "=== go tool pprof -top (all) ==="
go tool pprof -top "$OUT_PB" 2>&1 | head -30
echo ""
echo "=== go tool pprof -top (ignore park) ==="
go tool pprof -top -ignore="Inner::park" "$OUT_PB" 2>&1 | head -40
echo ""
echo "Interactive: go tool pprof -http=127.0.0.1:0 $OUT_PB"
echo "git=$(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
