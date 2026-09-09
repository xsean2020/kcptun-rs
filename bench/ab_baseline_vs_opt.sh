#!/bin/bash
# Direct A/B: baseline (original select!) vs optimized (poll_fn) for 3des/no-comp and sm4/no-comp
# Single connection, 8MB, ABBA x5
set -e
cd "$(dirname "$0")/.."
ROOT="$(pwd)"

SR_BASE="$ROOT/target/release/kcptun-server-baseline"
CL_BASE="$ROOT/target/release/kcptun-client-baseline"
SR_OPT="$ROOT/target/release/kcptun-server"
CL_OPT="$ROOT/target/release/kcptun-client"
KEY="bench-key"
ECHO_PORT=39001
SERVER_PORT=39002
CLIENT_PORT=39003

cleanup() { pkill -f "kcptun" 2>/dev/null || true; kill $(jobs -p) 2>/dev/null || true; wait 2>/dev/null || true; }
trap cleanup EXIT

run_test() {
    local label="$1" crypt="$2" sr="$3" cl="$4"
    cleanup; sleep 0.5
    python3 -u -c "import socket,threading
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
while True: threading.Thread(target=echo,args=s.accept(),daemon=True).start()" 2>/dev/null &
    sleep 0.3
    "$sr" -l "0.0.0.0:${SERVER_PORT}" -t "127.0.0.1:${ECHO_PORT}" --key "$KEY" --crypt "$crypt" --nocomp --mode fast --sndwnd 1024 --rcvwnd 1024 --smuxver 2 >/dev/null 2>&1 &
    sleep 0.8
    "$cl" -l "127.0.0.1:${CLIENT_PORT}" -r "127.0.0.1:${SERVER_PORT}" --key "$KEY" --crypt "$crypt" --nocomp --mode fast --sndwnd 1024 --rcvwnd 1024 --smuxver 2 >/dev/null 2>&1 &
    sleep 1
    for i in $(seq 1 30); do python3 -c "import socket;s=socket.socket();s.settimeout(0.3)
try:
 s.connect(('127.0.0.1',${CLIENT_PORT}));s.close()
except: exit(1)" 2>/dev/null && break; sleep 0.1; done
    local mbps=$(python3 bench/throughput.py "$CLIENT_PORT" --data-mb 8 --chunk-kb 128 2>&1 | grep -oE '[0-9]+\.[0-9]+ MB/s' | head -1)
    printf "  %-30s %s\n" "$label" "$mbps"
    cleanup; sleep 0.3
}

for crypt in 3des sm4; do
    echo "=== ${crypt}/no-comp: baseline vs optimized (ABBA x5, 8MB, single conn) ==="
    for round in 1 2 3 4 5; do
        echo "--- Round $round ---"
        run_test "baseline (select!)"  "$crypt" "$SR_BASE" "$CL_BASE"
        run_test "optimized (poll_fn)" "$crypt" "$SR_OPT"  "$CL_OPT"
        run_test "optimized (poll_fn)" "$crypt" "$SR_OPT"  "$CL_OPT"
        run_test "baseline (select!)"  "$crypt" "$SR_BASE" "$CL_BASE"
    done
    echo ""
done
