#!/bin/bash
# Isolated A/B test for 3des/no-comp vs 3des/comp
# Single connection, 8MB data, 5 rounds, ABBA ordering
set -e
cd "$(dirname "$0")/.."
ROOT="$(pwd)"

SR="$ROOT/target/release/kcptun-server"
CL="$ROOT/target/release/kcptun-client"
KEY="bench-key"
ECHO_PORT=39001
SERVER_PORT=39002
CLIENT_PORT=39003

cleanup() {
    pkill -f "kcptun-server" 2>/dev/null || true
    pkill -f "kcptun-client" 2>/dev/null || true
    pkill -f "python3.*echo" 2>/dev/null || true
    # kill background echo server by job
    kill $(jobs -p) 2>/dev/null || true
    wait 2>/dev/null || true
}
trap cleanup EXIT

run_pair() {
    local label="$1"
    local nocomp="$2"
    local extra=""
    [ "$nocomp" = "1" ] && extra="--nocomp"

    cleanup
    sleep 0.5

    # Echo server (background, no job control output)
    python3 -u -c "
import socket,threading
def echo(s,a):
    try:
        while True:
            d=s.recv(65536)
            if not d: break
            s.sendall(d)
    except: pass
    s.close()
s=socket.socket();s.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1)
s.bind(('0.0.0.0',${ECHO_PORT}));s.listen(10)
while True: threading.Thread(target=echo,args=s.accept(),daemon=True).start()
" 2>/dev/null &
    sleep 0.3

    "$SR" -l "0.0.0.0:${SERVER_PORT}" -t "127.0.0.1:${ECHO_PORT}" \
        --key "$KEY" --crypt 3des $extra --mode fast --sndwnd 1024 --rcvwnd 1024 --smuxver 2 >/dev/null 2>&1 &
    sleep 0.8
    "$CL" -l "127.0.0.1:${CLIENT_PORT}" -r "127.0.0.1:${SERVER_PORT}" \
        --key "$KEY" --crypt 3des $extra --mode fast --sndwnd 1024 --rcvwnd 1024 --smuxver 2 >/dev/null 2>&1 &
    sleep 1

    # Wait for tunnel
    for i in $(seq 1 30); do
        python3 -c "import socket;s=socket.socket();s.settimeout(0.3)
try:
    s.connect(('127.0.0.1',${CLIENT_PORT}));s.close()
except: exit(1)" 2>/dev/null && break
        sleep 0.1
    done

    # Run throughput test (uses --data-mb and --chunk-kb flags)
    local out=$(python3 bench/throughput.py "$CLIENT_PORT" --data-mb 8 --chunk-kb 128 2>&1)
    local mbps=$(echo "$out" | grep -oE '[0-9]+\.[0-9]+ MB/s' | head -1)
    echo "  ${label}: ${mbps}"

    cleanup
    sleep 0.3
}

echo "=== 3des isolated A/B test (8MB, single conn, ABBA x5) ==="
echo ""
for round in 1 2 3 4 5; do
    echo "--- Round $round ---"
    run_pair "3des/no-comp" 1
    run_pair "3des/comp   " 0
    run_pair "3des/comp   " 0
    run_pair "3des/no-comp" 1
    echo ""
done
