#!/bin/bash
# tunnel_p99.sh — Professional P99/P999 tunnel latency test orchestrator.
#
# Tests the FULL kcptun tunnel stack (crypto + KCP + SMUX + Snappy) under
# controlled network impairment, payload size sweep, and RPS sweep.
#
# Methodology: open-model fixed-rate sends (Coordinated Omission safe),
# warmup exclusion, global percentile computation, no batch averaging.
#
# Usage:
#   bash bench/tunnel_p99.sh                    # default: TC-01 (10k RPS, 1KB, 0% loss, tokio↔tokio)
#   bash bench/tunnel_p99.sh TC-02              # single test case
#   bash bench/tunnel_p99.sh all                # run all test cases
#   RPS=5000 SIZE=128 LOSS=5 bash bench/tunnel_p99.sh TC-03  # override params
#
# Requires:
#   - Built binaries: target/release/kcptun-client, target/release/kcptun-server
#   - Go kcptun binaries: tests/kcptun-go/client, tests/kcptun-go/server
#   - tc (iproute2) for network impairment injection
#   - hdrhistogram-rs (cargo install hdrhistogram) for Rust-side histogram,
#     OR the script falls back to sort-based nearest-rank percentiles.
#
# Env overrides:
#   RPS        (default 10000)
#   WARMUP     (default 30)
#   DURATION   (default 180)
#   SIZE       (default 1024)
#   LOSS       (default 0)
#   JITTER     (default 0, ms)
#   CRYPT      (default aes)
#   MODE       (default fast3)
#   SMUXVER    (default 2)
#   CONN       (default 1)
#   RUNTIME    (optional override: tokio|smol|goroutine; otherwise use case runtime)
#   CONCURRENCY (default 32, bounded open-model probe concurrency)
#   SERVER_SHARDS (default 1; Linux SO_REUSEPORT shards for multi-P tests)
#   PROBE      (python, default; or rust for the low-noise std-thread probe)
#   POST_CASE_COOLDOWN (default 15; seconds for TIME_WAIT drain between cases)
#
set -euo pipefail
cd "$(dirname "$0")/.."
REPO=$(pwd)

# ── Defaults ──────────────────────────────────────────────────────────
RPS=${RPS:-10000}
WARMUP=${WARMUP:-30}
DURATION=${DURATION:-180}
SIZE=${SIZE:-1024}
LOSS=${LOSS:-0}
JITTER=${JITTER:-0}
CRYPT=${CRYPT:-aes}
MODE=${MODE:-fast3}
SMUXVER=${SMUXVER:-2}
CONN=${CONN:-1}
RUNTIME_OVERRIDE=${RUNTIME:-}
CONCURRENCY=${CONCURRENCY:-32}
SERVER_SHARDS=${SERVER_SHARDS:-1}
PROBE=${PROBE:-python}
POST_CASE_COOLDOWN=${POST_CASE_COOLDOWN:-15}
CONV=0x00C0_FFEE

# ── Binary paths ──────────────────────────────────────────────────────
RUST_TOKIO_SERVER="$REPO/target/release/kcptun-server"
RUST_TOKIO_CLIENT="$REPO/target/release/kcptun-client"
RUST_SMOL_SERVER="$REPO/target/smol-release/release/kcptun-server"
RUST_SMOL_CLIENT="$REPO/target/smol-release/release/kcptun-client"
RUST_GOROUTINE_SERVER="$REPO/target/goroutine-release/release/kcptun-server"
RUST_GOROUTINE_CLIENT="$REPO/target/goroutine-release/release/kcptun-client"
RUST_TUNNEL_PROBE="$REPO/target/release/examples/tunnel_probe"
GO_SERVER="$REPO/tests/kcptun-go/server"
GO_CLIENT="$REPO/tests/kcptun-go/client"

# ── Network impairment setup ─────────────────────────────────────────
setup_netem() {
    local iface=$1 loss=$2 jitter=$3
    sudo tc qdisc del dev "$iface" root netem 2>/dev/null || true
    if [ "$loss" != "0" ] || [ "$jitter" != "0" ]; then
        local args="netem"
        [ "$loss" != "0" ] && args="$args loss ${loss}%"
        [ "$jitter" != "0" ] && args="$args delay 0ms ${jitter}ms distribution normal"
        sudo tc qdisc add dev "$iface" root "$args"
        echo "  [netem] iface=$iface loss=${loss}% jitter=${jitter}ms"
    else
        echo "  [netem] no impairment (clean link)"
    fi
}

cleanup_netem() {
    sudo tc qdisc del dev lo root netem 2>/dev/null || true
}

# ── Tunnel start/stop ─────────────────────────────────────────────────
start_tunnel() {
    local side=$1 bin=$1 port=$2 target_port=$3
    local extra_args=()

    if [ "$side" = "server" ]; then
        "$bin" -l "0.0.0.0:$port" -t "127.0.0.1:$target_port" \
            --key "tunnel-p99-key" --crypt "$CRYPT" --mode "$MODE" \
            --smuxver "$SMUXVER" \
            --sndwnd 1024 --rcvwnd 1024 --nocomp 2>/dev/null &
    else
        "$bin" -l "127.0.0.1:$port" -r "127.0.0.1:$target_port" \
            --key "tunnel-p99-key" --crypt "$CRYPT" --mode "$MODE" \
            --smuxver "$SMUXVER" --conn "$CONN" \
            --sndwnd 1024 --rcvwnd 1024 --nocomp 2>/dev/null &
    fi
    echo $!
}

wait_for_port() {
    local port=$1 tries=50
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

# A listening TCP socket is not sufficient: the local client can accept while
# its KCP/SMUX path is still creating the first UDP session. Starting an
# open-model run at that point parks every worker on the first read and turns
# a startup race into an apparent scheduler regression.
wait_for_tunnel_echo() {
    local port=$1 tries=80
    while [ $tries -gt 0 ]; do
        if python3 -c "import socket,sys
s=socket.socket(); s.settimeout(0.25)
try:
    s.connect(('127.0.0.1',$port)); s.sendall(b'kcptun-ready')
    out=b''
    while len(out) < 12:
        chunk=s.recv(12-len(out))
        if not chunk: raise OSError('eof')
        out += chunk
    s.close()
    sys.exit(0 if out == b'kcptun-ready' else 1)
except Exception:
    s.close(); sys.exit(1)
" 2>/dev/null; then
            return 0
        fi
        sleep 0.1
        tries=$((tries - 1))
    done
    return 1
}

# ── Go kcptun tunnel P99 measurement ──────────────────────────
run_go_tunnel_p99() {
    local label=$1 go_client=$2 go_server=$3 client_port=$4 server_port=$5
    local rps=$6 size=$7 warmup=$8 duration=$9

    # Start echo server on the real TCP target
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
s.bind(('0.0.0.0',$server_port)); s.listen(128)
while True: threading.Thread(target=echo,args=s.accept(),daemon=True).start()
" >/dev/null 2>&1 &
    local ECHO_PID=$!
    sleep 0.3

    # Start Go kcptun server
    "$go_server" -l "0.0.0.0:$server_port" -t "127.0.0.1:$server_port" \
        --key "tunnel-p99-key" --crypt "$CRYPT" --mode "$MODE" \
        --smuxver "$SMUXVER" \
        --sndwnd 1024 --rcvwnd 1024 --nocomp 2>/dev/null &
    local SRV_PID=$!
    sleep 0.5

    # Start Go kcptun client
    "$go_client" -l "127.0.0.1:$client_port" -r "127.0.0.1:$server_port" \
        --key "tunnel-p99-key" --crypt "$CRYPT" --mode "$MODE" \
        --smuxver "$SMUXVER" --conn "$CONN" \
        --sndwnd 1024 --rcvwnd 1024 --nocomp 2>/dev/null &
    local CLI_PID=$!

    if ! wait_for_port "$client_port"; then
        echo "  ❌ Go tunnel client not ready on port $client_port"
        kill "$CLI_PID" "$SRV_PID" "$ECHO_PID" 2>/dev/null || true
        wait 2>/dev/null || true
        return 1
    fi

    # Run latency probe via Python (same open-model approach)
    python3 -u -c "
import socket, time, sys

port = $client_port
rps = $rps
size = $size
warmup = $warmup
duration = $duration

payload = b'X' * size
interval = 1.0 / rps

latencies = []
ok = 0
sends = 0

# Warmup
warmup_end = time.monotonic() + warmup
next_send = time.monotonic()
while time.monotonic() < warmup_end:
    s = socket.socket()
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.settimeout(5)
    s.connect(('127.0.0.1', port))
    s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    t0 = time.monotonic()
    s.sendall(payload)
    try:
        data = b''
        while len(data) < size:
            chunk = s.recv(65536)
            if not chunk: break
            data += chunk
        if len(data) == size:
            pass  # excluded from warmup
    except Exception:
        pass
    s.close()
    next_send += interval
    if next_send > time.monotonic():
        time.sleep(next_send - time.monotonic())

# Measurement
measure_end = warmup_end + duration
next_send = warmup_end
while time.monotonic() < measure_end:
    s = socket.socket()
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.settimeout(5)
    s.connect(('127.0.0.1', port))
    s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    t0 = time.monotonic()
    s.sendall(payload)
    try:
        data = b''
        while len(data) < size:
            chunk = s.recv(65536)
            if not chunk: break
            data += chunk
        if len(data) == size:
            lat = (time.monotonic() - t0) * 1e6
            latencies.append(lat)
            ok += 1
    except Exception:
        pass
    s.close()
    sends += 1
    next_send += interval
    if next_send > time.monotonic():
        time.sleep(next_send - time.monotonic())

latencies.sort()
n = len(latencies)
if n == 0:
    print('RESULT label=$label samples=0 ok=0 size=$size rps=$rps p50_us=0 p90_us=0 p99_us=0 p999_us=0 avg_us=0 min_us=0 max_us=0')
    sys.exit(0)

p = lambda q: latencies[min(int(n * q), n - 1)]
avg = sum(latencies) / n

print(f'RESULT label=$label samples={n} ok={ok} size={size} rps={rps} \
p50_us={p(0.50):.1f} p90_us={p(0.90):.1f} p99_us={p(0.99):.1f} \
p999_us={p(0.999):.1f} avg_us={avg:.1f} min_us={latencies[0]:.1f} \
max_us={latencies[-1]:.1f}')
" 2>/dev/null

    local result=$?

    kill "$CLI_PID" "$SRV_PID" "$ECHO_PID" 2>/dev/null || true
    wait 2>/dev/null || true
    cleanup_netem

    return $result
}

# ── Go comparison test case ──────────────────────────────────
run_go_comparison() {
    local tc_id=$1 label=$2 rps=$3 size=$4 loss=$5 jitter=$6

    echo ""
    echo "══════════════════════════════════════════════════════════════"
    echo "  Go kcptun Comparison: $tc_id — $label"
    echo "  RPS=$rps SIZE=${size}B LOSS=${loss}% JITTER=${jitter}ms CRYPT=$CRYPT MODE=$MODE"
    echo "══════════════════════════════════════════════════════════════"

    setup_netem lo "$loss" "$jitter"

    local result
    result=$(run_go_tunnel_p99 "$tc_id" "$GO_CLIENT" "$GO_SERVER" \
        12949 29901 "$rps" "$size" "$WARMUP" "$DURATION")

    echo "$result"

    cleanup_netem
}
run_tunnel_p99_legacy() {
    local label=$1 client_bin=$2 server_bin=$3 client_port=$4 server_port=$5
    local rps=$6 size=$7 warmup=$8 duration=$9

    # Start echo server on the real TCP target
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
s.bind(('0.0.0.0',$server_port)); s.listen(128)
while True: threading.Thread(target=echo,args=s.accept(),daemon=True).start()
" >/dev/null 2>&1 &
    local ECHO_PID=$!
    sleep 0.3

    # Start kcptun server (KCP → TCP echo)
    "$server_bin" -l "0.0.0.0:$server_port" -t "127.0.0.1:$server_port" \
        --key "tunnel-p99-key" --crypt "$CRYPT" --mode "$MODE" \
        --smuxver "$SMUXVER" \
        --sndwnd 1024 --rcvwnd 1024 --nocomp 2>/dev/null &
    local SRV_PID=$!
    sleep 0.5

    # Start kcptun client (TCP → KCP → UDP → KCP → TCP)
    "$client_bin" -l "127.0.0.1:$client_port" -r "127.0.0.1:$server_port" \
        --key "tunnel-p99-key" --crypt "$CRYPT" --mode "$MODE" \
        --smuxver "$SMUXVER" --conn "$CONN" \
        --sndwnd 1024 --rcvwnd 1024 --nocomp 2>/dev/null &
    local CLI_PID=$!

    # Wait for client TCP listener
    if ! wait_for_tunnel_echo "$client_port"; then
        echo "  ❌ tunnel did not complete a ready echo on port $client_port"
        kill "$CLI_PID" "$SRV_PID" "$ECHO_PID" 2>/dev/null || true
        wait 2>/dev/null || true
        return 1
    fi

    # Run the latency probe via Python (open model, fixed rate)
    python3 -u -c "
import socket, time, sys, threading, statistics

port = $client_port
rps = $rps
size = $size
warmup = $warmup
duration = $duration

payload = b'X' * size
interval = 1.0 / rps

latencies = []
ok = 0
sends = 0

# Warmup
warmup_end = time.monotonic() + warmup
next_send = time.monotonic()
while time.monotonic() < warmup_end:
    s = socket.socket()
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.settimeout(5)
    s.connect(('127.0.0.1', port))
    s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    t0 = time.monotonic()
    s.sendall(payload)
    try:
        data = b''
        while len(data) < size:
            chunk = s.recv(65536)
            if not chunk: break
            data += chunk
        if len(data) == size:
            lat = (time.monotonic() - t0) * 1e6
            # excluded from warmup
    except Exception:
        pass
    s.close()
    next_send += interval
    if next_send > time.monotonic():
        time.sleep(next_send - time.monotonic())

# Measurement
measure_end = warmup_end + duration
next_send = warmup_end
while time.monotonic() < measure_end:
    s = socket.socket()
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.settimeout(5)
    s.connect(('127.0.0.1', port))
    s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    t0 = time.monotonic()
    s.sendall(payload)
    try:
        data = b''
        while len(data) < size:
            chunk = s.recv(65536)
            if not chunk: break
            data += chunk
        if len(data) == size:
            lat = (time.monotonic() - t0) * 1e6
            latencies.append(lat)
            ok += 1
    except Exception:
        pass
    s.close()
    sends += 1
    next_send += interval
    if next_send > time.monotonic():
        time.sleep(next_send - time.monotonic())

# Compute percentiles (nearest-rank, no averaging)
latencies.sort()
n = len(latencies)
if n == 0:
    print('RESULT label=$label samples=0 ok=0 size=$size rps=$rps p50_us=0 p90_us=0 p99_us=0 p999_us=0 avg_us=0 min_us=0 max_us=0')
    sys.exit(0)

p = lambda q: latencies[min(int(n * q), n - 1)]
avg = sum(latencies) / n

print(f'RESULT label=$label samples={n} ok={ok} size={size} rps={rps} \
p50_us={p(0.50):.1f} p90_us={p(0.90):.1f} p99_us={p(0.99):.1f} \
p999_us={p(0.999):.1f} avg_us={avg:.1f} min_us={latencies[0]:.1f} \
max_us={latencies[-1]:.1f}')
" 2>/dev/null

    local result=$?

    # Cleanup
    kill "$CLI_PID" "$SRV_PID" "$ECHO_PID" 2>/dev/null || true
    wait 2>/dev/null || true
    cleanup_netem

    return $result
}

# Bounded-concurrency open-model probe. This is intentionally separate from
# the historical serial probe above: the Python driver creates a connection per
# request, while the Rust driver keeps one TCP/SMUX stream per worker. Neither
# may serialize requests in the driver, or scheduler contention is hidden.
run_tunnel_p99() {
    local label=$1 client_bin=$2 server_bin=$3 client_port=$4 server_port=$5
    local rps=$6 size=$7 warmup=$8 duration=$9

    python3 -u "$REPO/bench/echo_server.py" "$server_port" >/dev/null 2>&1 &
    local echo_pid=$!
    "$server_bin" -l "0.0.0.0:$server_port" -t "127.0.0.1:$server_port" \
        --key "tunnel-p99-key" --crypt "$CRYPT" --mode "$MODE" \
        --smuxver "$SMUXVER" --shards "$SERVER_SHARDS" \
        --sndwnd 1024 --rcvwnd 1024 --nocomp 2>/dev/null &
    local server_pid=$!
    "$client_bin" -l "127.0.0.1:$client_port" -r "127.0.0.1:$server_port" \
        --key "tunnel-p99-key" --crypt "$CRYPT" --mode "$MODE" \
        --smuxver "$SMUXVER" --conn "$CONN" \
        --sndwnd 1024 --rcvwnd 1024 --nocomp 2>/dev/null &
    local client_pid=$!

    local result=0
    if ! wait_for_tunnel_echo "$client_port"; then
        echo "  ❌ tunnel did not complete a ready echo on port $client_port"
        result=1
    elif [ "$PROBE" = "rust" ]; then
        if [ ! -x "$RUST_TUNNEL_PROBE" ]; then
            echo "  ❌ Rust probe missing: build with cargo build -p kcptun-common --example tunnel_probe --release"
            result=1
        elif PROBE_LABEL="$label" "$RUST_TUNNEL_PROBE" \
            "$client_port" "$rps" "$size" "$warmup" "$duration" "$CONCURRENCY"; then
            :
        else
            result=$?
        fi
    elif [ "$PROBE" = "python" ]; then
        if PROBE_LABEL="$label" python3 -u "$REPO/bench/probe_tunnel.py" \
            "$client_port" "$rps" "$size" "$warmup" "$duration" "$CONCURRENCY"; then
            :
        else
            result=$?
        fi
    else
        echo "  ❌ unknown PROBE=$PROBE (expected python or rust)"
        result=1
    fi
    kill "$client_pid" "$server_pid" "$echo_pid" 2>/dev/null || true
    wait "$client_pid" "$server_pid" "$echo_pid" 2>/dev/null || true
    cleanup_netem
    return "$result"
}

# ── Test case runner ──────────────────────────────────────────────────
run_test_case() {
    local tc_id=$1 label=$2 rps=$3 size=$4 loss=$5 jitter=$6 runtime=$7

    echo ""
    echo "══════════════════════════════════════════════════════════════"
    echo "  $tc_id — $label"
    echo "  RPS=$rps SIZE=${size}B LOSS=${loss}% JITTER=${jitter}ms RUNTIME=$runtime CRYPT=$CRYPT MODE=$MODE"
    echo "══════════════════════════════════════════════════════════════"

    if [ "$runtime" = "go" ]; then
        if [ ! -x "$GO_CLIENT" ] || [ ! -x "$GO_SERVER" ]; then
            echo "  ⚠️  Go kcptun binaries not found, skipping"
            return 1
        fi
        run_go_comparison "$tc_id" "$label" "$rps" "$size" "$loss" "$jitter"
    else
        local client_bin server_bin
        case "$runtime" in
            tokio) client_bin="$RUST_TOKIO_CLIENT"; server_bin="$RUST_TOKIO_SERVER" ;;
            smol) client_bin="$RUST_SMOL_CLIENT"; server_bin="$RUST_SMOL_SERVER" ;;
            goroutine) client_bin="$RUST_GOROUTINE_CLIENT"; server_bin="$RUST_GOROUTINE_SERVER" ;;
            *) echo "  ❌ unknown runtime: $runtime"; return 1 ;;
        esac
        if [ ! -x "$client_bin" ] || [ ! -x "$server_bin" ]; then
            echo "  ❌ $runtime binaries not found; build its release target first"
            return 1
        fi
        setup_netem lo "$loss" "$jitter"
        run_tunnel_p99 "$tc_id" "$client_bin" "$server_bin" \
            12948 29900 "$rps" "$size" "$WARMUP" "$DURATION"
        local result=$?
        cleanup_netem
        return $result
    fi
}

# ── Main ──────────────────────────────────────────────────────────────
trap cleanup_netem EXIT

# Determine which test cases to run
if [ "${1:-}" = "all" ]; then
    # Run the full matrix including Go comparison
    CASES=(
        "TC-01|Small-packet baseline (128B, 1 conn, 0% loss, tokio)|10000|128|0|0|tokio"
        "TC-01-GO|Small-packet baseline Go kcptun (128B, 1 conn, 0% loss)|10000|128|0|0|go"
        "TC-02|Large-packet baseline (1400B, 1 conn, 0% loss, tokio)|10000|1400|0|0|tokio"
        "TC-02-GO|Large-packet baseline Go kcptun (1400B, 1 conn, 0% loss)|10000|1400|0|0|go"
        "TC-03|Multi-stream baseline (1KB, 10 streams, 0% loss, tokio)|5000|1024|0|0|tokio"
        "TC-03-GO|Multi-stream baseline Go kcptun (1KB, 10 streams, 0% loss)|5000|1024|0|0|go"
        "TC-04|5% loss stress (1KB, 1 conn, 5% loss, tokio)|5000|1024|5|0|tokio"
        "TC-04-GO|5% loss stress Go kcptun (1KB, 1 conn, 5% loss)|5000|1024|5|0|go"
        "TC-05|10% loss extreme (1KB, 1 conn, 10% loss, tokio)|5000|1024|10|0|tokio"
        "TC-05-GO|10% loss extreme Go kcptun (1KB, 1 conn, 10% loss)|5000|1024|10|0|go"
        "TC-06|Burst + 10% loss (1400B, 100 conn, 10% loss, tokio)|5000|1400|10|0|tokio"
        "TC-06-GO|Burst + 10% loss Go kcptun (1400B, 100 conn, 10% loss)|5000|1400|10|0|go"
        "TC-07|Small-packet smol baseline (128B, 1 conn, 0% loss)|10000|128|0|0|smol"
        "TC-08|Large-packet smol baseline (1400B, 1 conn, 0% loss)|10000|1400|0|0|smol"
        "TC-09|5% loss stress smol (1KB, 1 conn, 5% loss)|5000|1024|5|0|smol"
        "TC-10|10% loss extreme smol (1KB, 1 conn, 10% loss)|5000|1024|10|0|smol"
        "TC-11|Small-packet goroutine baseline (128B, 1 conn, 0% loss)|10000|128|0|0|goroutine"
        "TC-12|Large-packet goroutine baseline (1400B, 1 conn, 0% loss)|10000|1400|0|0|goroutine"
        "TC-13|5% loss stress goroutine (1KB, 1 conn, 5% loss)|5000|1024|5|0|goroutine"
        "TC-14|10% loss extreme goroutine (1KB, 1 conn, 10% loss)|5000|1024|10|0|goroutine"
    )
elif [ -n "${1:-}" ] && [ "$1" != "all" ]; then
    case "$1" in
        TC-01) CASES=("TC-01|Small-packet baseline (128B, 1 conn, 0% loss, tokio)|10000|128|0|0|tokio") ;;
        TC-01-GO) CASES=("TC-01-GO|Small-packet baseline Go kcptun (128B, 1 conn, 0% loss)|10000|128|0|0|go") ;;
        TC-02) CASES=("TC-02|Large-packet baseline (1400B, 1 conn, 0% loss, tokio)|10000|1400|0|0|tokio") ;;
        TC-02-GO) CASES=("TC-02-GO|Large-packet baseline Go kcptun (1400B, 1 conn, 0% loss)|10000|1400|0|0|go") ;;
        TC-03) CASES=("TC-03|Multi-stream baseline (1KB, 10 streams, 0% loss, tokio)|5000|1024|0|0|tokio") ;;
        TC-03-GO) CASES=("TC-03-GO|Multi-stream baseline Go kcptun (1KB, 10 streams, 0% loss)|5000|1024|0|0|go") ;;
        TC-04) CASES=("TC-04|5% loss stress (1KB, 1 conn, 5% loss, tokio)|5000|1024|5|0|tokio") ;;
        TC-04-GO) CASES=("TC-04-GO|5% loss stress Go kcptun (1KB, 1 conn, 5% loss)|5000|1024|5|0|go") ;;
        TC-05) CASES=("TC-05|10% loss extreme (1KB, 1 conn, 10% loss, tokio)|5000|1024|10|0|tokio") ;;
        TC-05-GO) CASES=("TC-05-GO|10% loss extreme Go kcptun (1KB, 1 conn, 10% loss)|5000|1024|10|0|go") ;;
        TC-06) CASES=("TC-06|Burst + 10% loss (1400B, 100 conn, 10% loss, tokio)|5000|1400|10|0|tokio") ;;
        TC-06-GO) CASES=("TC-06-GO|Burst + 10% loss Go kcptun (1400B, 100 conn, 10% loss)|5000|1400|10|0|go") ;;
        TC-07) CASES=("TC-07|Small-packet smol baseline (128B, 1 conn, 0% loss)|10000|128|0|0|smol") ;;
        TC-08) CASES=("TC-08|Large-packet smol baseline (1400B, 1 conn, 0% loss)|10000|1400|0|0|smol") ;;
        TC-09) CASES=("TC-09|5% loss stress smol (1KB, 1 conn, 5% loss)|5000|1024|5|0|smol") ;;
        TC-10) CASES=("TC-10|10% loss extreme smol (1KB, 1 conn, 10% loss)|5000|1024|10|0|smol") ;;
        TC-11) CASES=("TC-11|Small-packet goroutine baseline (128B, 1 conn, 0% loss)|10000|128|0|0|goroutine") ;;
        TC-12) CASES=("TC-12|Large-packet goroutine baseline (1400B, 1 conn, 0% loss)|10000|1400|0|0|goroutine") ;;
        TC-13) CASES=("TC-13|5% loss stress goroutine (1KB, 1 conn, 5% loss)|5000|1024|5|0|goroutine") ;;
        TC-14) CASES=("TC-14|10% loss extreme goroutine (1KB, 1 conn, 10% loss)|5000|1024|10|0|goroutine") ;;
        *) echo "Unknown test case: $1"; exit 1 ;;
    esac
else
    CASES=("TC-01|Small-packet baseline (128B, 1 conn, 0% loss, tokio)|10000|128|0|0|tokio")
fi

echo ""
echo "╔══════════════════════════════════════════════════════════════╗"
echo "║  kcptun-rs Tunnel P99/P999 Performance Test                   ║"
echo "║  $(date '+%Y-%m-%d %H:%M')                                    ║"
echo "║  Runtime override: ${RUNTIME_OVERRIDE:-case-defined}                         ║"
echo "║  Crypto: $CRYPT Mode: $MODE  SMUX: v$SMUXVER  Conn: $CONN        ║"
echo "║  Warmup: ${WARMUP}s  Duration: ${DURATION}s  Payload: ${SIZE}B  Loss: ${LOSS}%  Jitter: ${JITTER}ms ║"
echo "╚══════════════════════════════════════════════════════════════╝"
echo ""

# Run each test case
RESULTS=""
for case_def in "${CASES[@]}"; do
    IFS='|' read -r tc_id label rps size loss jitter runtime <<< "$case_def"

    # Override with env vars if set
    rps=${RPS:-$rps}
    size=${SIZE:-$size}
    loss=${LOSS:-$loss}
    jitter=${JITTER:-$jitter}
    runtime=${RUNTIME_OVERRIDE:-$runtime}

    if ! result=$(run_test_case "$tc_id" "$label" "$rps" "$size" "$loss" "$jitter" "$runtime"); then
        # Command substitution otherwise discards the probe's failure and its
        # error counters, hiding exactly the signal a tail-latency gate needs.
        echo "$result"
        echo "  ❌ $tc_id failed"
        RESULTS="${RESULTS}|${tc_id}|FAILED"
        continue
    fi

    echo "$result"
    RESULTS="${RESULTS}|${tc_id}|${result}"
    # The Python probe creates a connection per request; retain a TIME_WAIT
    # drain between cases on macOS. The persistent Rust probe can set this to 0.
    sleep "$POST_CASE_COOLDOWN"
done

# ── Render report ─────────────────────────────────────────────────────
echo ""
echo "══════════════════════════════════════════════════════════════"
echo "  TUNNEL P99/P999 RESULTS"
echo "══════════════════════════════════════════════════════════════"

# Parse and display results
echo ""
echo "| Test | RPS | Size | Loss | P50(ms) | P90(ms) | P99(ms) | P999(ms) | Max(ms) | Verdict |"
echo "|------|-----|------|------|---------|---------|---------|----------|---------|---------|"

echo "$RESULTS" | tr '|' '\n' | grep "^TC-" | while read -r tc_id; do
    # Find the matching result line
    result_line=$(echo "$RESULTS" | grep "RESULT label=$tc_id" | head -1)
    if [ -z "$result_line" ]; then
        continue
    fi

    p50=$(echo "$result_line" | sed -n 's/.*p50_us=\([0-9.]*\).*/\1/p')
    p90=$(echo "$result_line" | sed -n 's/.*p90_us=\([0-9.]*\).*/\1/p')
    p99=$(echo "$result_line" | sed -n 's/.*p99_us=\([0-9.]*\).*/\1/p')
    p999=$(echo "$result_line" | tr ' ' '\n' | sed -n 's/^p999_us=\([0-9.]*\)$/\1/p')
    avg=$(echo "$result_line" | sed -n 's/.*avg_us=\([0-9.]*\).*/\1/p')
    mn=$(echo "$result_line" | sed -n 's/.*min_us=\([0-9.]*\).*/\1/p')
    mx=$(echo "$result_line" | tr ' ' '\n' | sed -n 's/^max_us=\([0-9.]*\)$/\1/p')
    ok=$(echo "$result_line" | sed -n 's/.*ok=\([0-9]*\).*/\1/p')

    if [ -z "$p50" ] || [ -z "$p90" ] || [ -z "$p99" ] || [ -z "$p999" ] || [ -z "$mx" ]; then
        echo "| $tc_id | $rps | ${size}B | ${loss}% | — | — | — | — | — | ❌ no successful samples |"
        continue
    fi

    # Convert µs to ms
    p50_ms=$(awk "BEGIN { printf \"%.2f\", $p50 / 1000 }")
    p90_ms=$(awk "BEGIN { printf \"%.2f\", $p90 / 1000 }")
    p99_ms=$(awk "BEGIN { printf \"%.2f\", $p99 / 1000 }")
    p999_ms=$(awk "BEGIN { printf \"%.2f\", $p999 / 1000 }")
    mx_ms=$(awk "BEGIN { printf \"%.2f\", $mx / 1000 }")

    # Verdict: healthy if P99 ≤ 3×P50 and P999 ≤ 3×P99
    verdict="✅"
    if awk "BEGIN { exit !($p99 > 3 * $p50) }"; then verdict="⚠️ P99>3×P50"; fi
    if awk "BEGIN { exit !($p999 > 3 * $p99) }"; then verdict="🔴 P999>3×P99"; fi

    echo "| $tc_id | $rps | ${size}B | ${loss}% | ${p50_ms} | ${p90_ms} | ${p99_ms} | ${p999_ms} | ${mx_ms} | $verdict |"
done

echo ""
echo "Report generated: $(date)"
echo "Method: open-model fixed-rate, warmup=${WARMUP}s excluded, duration=${DURATION}s"
echo "Stack: TCP → KCP (crypto+SMUX+Snappy) → UDP → KCP → TCP → echo"
