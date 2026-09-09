#!/bin/bash
# kcptun performance benchmark: Go vs Rust-Tokio
#
# Measures throughput (MB/s) and latency (ms RTT) for each backend.
# Requires: release build of Rust-Tokio and Go kcptun binaries.
#
# Usage:
#   make bench                  — full comparison
#   bash bench/run_bench.sh     — same thing
#   BENCH_DATA_MB=50 bash bench/run_bench.sh  — larger data size
#   BENCH_WORKERS=4 bash bench/run_bench.sh  — run with four workers
#   BENCH_CONNECTIONS=64 BENCH_DATA_MB=4 bash bench/run_bench.sh \
#       — run 64 concurrent TCP streams (data size is per connection)
#   BENCH_ROUNDS=3 bash bench/run_bench.sh    — 3 interleaved rounds + median
#       summary (default 3; set 1 for a single pass). Rounds rotate the config
#       order so machine-state drift cannot systematically favor one backend.
#   BENCH_FORCE=1 bash bench/run_bench.sh     — skip the load guard
#
# Methodology notes:
# - A load guard aborts when 1-min load exceeds ~70% of core count: a busy
#   desktop produces ±25% garbage (measured 2026-09-03: load 13/8 cores while
#   Unity+Docker ran flipped orderings run-to-run).
# - Single 200 MB runs cannot resolve ~3 MB/s gaps on any shared machine;
#   compare the MEDIAN of rounds, never a single number.
#
# NOTE: Compatible with bash 3.2 (macOS default). No negative array indices.
# This runner owns and reaps short-lived backend processes; do not emit shell
# job-completion notifications for the expected SIGINT cleanup path.
set +m
set +b
cd "$(dirname "$0")/.."

KEY="bench-key"
DATA_MB="${BENCH_DATA_MB:-200}"
CHUNK_KB="${BENCH_KB:-128}"
LATENCY_ITERS="${BENCH_LATENCY_ITERS:-50}"
# `0` lets throughput.py derive a transfer timeout from the data size at a
# conservative 5 MB/s floor.  Set BENCH_TIMEOUT_SECONDS to override it.
TRANSFER_TIMEOUT_SECONDS="${BENCH_TIMEOUT_SECONDS:-0}"
CONNECTIONS="${BENCH_CONNECTIONS:-1}"
# Interleaved rounds per backend; results are summarized by median.
BENCH_ROUNDS="${BENCH_ROUNDS:-3}"
# Give both implementations the same top-level concurrency budget:
# Go receives GOMAXPROCS, Rust KcpListener receives KCPTUN_WORKER_THREADS.
# Rust's shared Tokio runtime keeps its system-derived default worker count.
# Set 0 to preserve each backend's own default instead.
BENCH_WORKERS="${BENCH_WORKERS:-4}"

GO_SERVER="${GO_SERVER:-./tests/kcptun-go/server}"
GO_CLIENT="${GO_CLIENT:-./tests/kcptun-go/client}"
RUST_TOKIO_SERVER="${RUST_TOKIO_SERVER:-./target/release/kcptun-server}"
RUST_TOKIO_CLIENT="${RUST_TOKIO_CLIENT:-./target/release/kcptun-client}"

# kcptun args — aligned on both sides for fair cross-impl comparison.
# Default Rust client sndwnd is 128 vs server 1024; leave them implicit and
# cross-impl paths are not comparable. Force the same windows/mode/smuxver.
#
# Traffic direction (measured by throughput.py):
#   loadgen ──TCP──► client(-l) ──KCP/UDP──► server(-l) ──TCP──► echo
# Labels are always "Client → Server" (who encrypts/sends the bulk stream).
CRYPT="${BENCH_CRYPT:-aes}"
MODE="${BENCH_MODE:-fast}"
SNDWND="${BENCH_SNDWND:-1024}"
RCVWND="${BENCH_RCVWND:-1024}"
SMUXVER="${BENCH_SMUXVER:-2}"
COMMON_ARGS="--crypt ${CRYPT} --nocomp --mode ${MODE} --sndwnd ${SNDWND} --rcvwnd ${RCVWND} --smuxver ${SMUXVER}"
SERVER_ARGS="$COMMON_ARGS"
CLIENT_ARGS="$COMMON_ARGS"

# Port management — increment per test to avoid TIME_WAIT conflicts
PORT_COUNTER=$((10000 + (RANDOM % 1000) * 3))

ECHO_PID=""
SERVER_PID=""
CLIENT_PID=""

cleanup() {
    stop_process "$CLIENT_PID"
    stop_process "$SERVER_PID"
    stop_process "$ECHO_PID"
    CLIENT_PID=""
    SERVER_PID=""
    ECHO_PID=""
    # Give processes time to fully exit + release ports
    sleep 0.5
}

# Reap only a process started by this runner.  A runtime shutdown bug must not
# turn one bad backend into a permanently stuck benchmark or a leaked server.
stop_process() {
    local pid=$1
    local tries=30
    [ -z "$pid" ] && return
    # kcptun has a Ctrl-C path that closes its listeners and runtime cleanly.
    # Unlike SIGTERM this also leaves a normal, observable shutdown route for
    # all backends before the bounded SIGKILL fallback below.
    kill -INT "$pid" 2>/dev/null || true
    while [ "$tries" -gt 0 ]; do
        if ! kill -0 "$pid" 2>/dev/null || ps -o stat= -p "$pid" 2>/dev/null | grep -q 'Z'; then
            # Reap inside a subshell so bash's "Terminated: 15" job-status
            # report (bash 3.2 prints it even with job control off) lands in
            # the subshell's stderr, which is discarded below.
            ( wait "$pid" ) 2>/dev/null
            return
        fi
        sleep 0.1
        tries=$((tries - 1))
    done
    kill -KILL "$pid" 2>/dev/null || true
    ( wait "$pid" ) 2>/dev/null
}

next_ports() {
    ECHO_PORT=$PORT_COUNTER
    SERVER_PORT=$((PORT_COUNTER + 1))
    CLIENT_PORT=$((PORT_COUNTER + 2))
    PORT_COUNTER=$((PORT_COUNTER + 3))
}

# Invoke a backend with the shared comparison budget.
launch_backend() {
    local bin=$1
    shift
    if [ "$BENCH_WORKERS" = "0" ]; then
        exec "$bin" "$@"
    fi
    if [ "$bin" = "$GO_SERVER" ] || [ "$bin" = "$GO_CLIENT" ]; then
        exec env GOMAXPROCS="$BENCH_WORKERS" "$bin" "$@"
    fi
    exec env KCPTUN_WORKER_THREADS="$BENCH_WORKERS" "$bin" "$@"
}

# A listening TCP socket only proves that the client process started.  It does
# not prove the KCP/SMUX path has completed its first handshake.  Exercise the
# full tunnel before warming up so connection setup is never charged to the
# throughput measurement.
wait_for_tunnel_echo() {
    local port=$1
    local tries=100
    while [ "$tries" -gt 0 ]; do
        if python3 - "$port" <<'PY' 2>/dev/null
import socket
import sys

payload = b"bench-ready"
sock = socket.socket()
sock.settimeout(0.5)
try:
    sock.connect(("127.0.0.1", int(sys.argv[1])))
    sock.sendall(payload)
    received = bytearray()
    while len(received) < len(payload):
        data = sock.recv(len(payload) - len(received))
        if not data:
            raise OSError("tunnel closed before echo")
        received.extend(data)
    if bytes(received) != payload:
        raise OSError("unexpected tunnel echo")
except OSError:
    sys.exit(1)
finally:
    sock.close()
PY
        then
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
s.settimeout(1.0)
while True:
    try:
        conn,a=s.accept()
    except socket.timeout:
        continue
    threading.Thread(target=echo,args=(conn,a),daemon=True).start()
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
    launch_backend "$bin" -l "0.0.0.0:$SERVER_PORT" -t "127.0.0.1:$ECHO_PORT" \
        --key "$KEY" $SERVER_ARGS 2>/dev/null &
    SERVER_PID=$!
    # kcptun server listens on UDP (not TCP), so wait_for_port can't probe it.
    # Poll for process liveness + give it time to bind the UDP socket.
    # 1s is enough on loopback; if the process dies in that window we catch it.
    sleep 1
    if ! kill -0 "$SERVER_PID" 2>/dev/null; then
        echo "  ❌ kcptun server failed on port $SERVER_PORT"
        return 1
    fi
}

start_client() {
    local bin=$1
    launch_backend "$bin" -l "127.0.0.1:$CLIENT_PORT" -r "127.0.0.1:$SERVER_PORT" \
        --key "$KEY" $CLIENT_ARGS 2>/dev/null &
    CLIENT_PID=$!
    # Readiness means an actual TCP → KCP → SMUX → TCP echo, not merely that
    # the client bound its local listener (up to ~10s for a cold handshake).
    if ! wait_for_tunnel_echo "$CLIENT_PORT"; then
        if ! kill -0 "$CLIENT_PID" 2>/dev/null; then
            echo "  ❌ kcptun client exited early on port $CLIENT_PORT"
        else
            echo "  ❌ kcptun tunnel did not become ready on port $CLIENT_PORT"
        fi
        return 1
    fi
}

# Abort when the machine is too busy for meaningful numbers (desktop reality:
# a single background app can halve every result). Override with BENCH_FORCE=1.
check_load() {
    [ "${BENCH_FORCE:-0}" = "1" ] && return 0
    local ncpu load1 over
    ncpu=$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4)
    if [ -r /proc/loadavg ]; then
        load1=$(cut -d' ' -f1 /proc/loadavg)
    else
        # macOS `sysctl vm.loadavg` prints "{ 7.04 11.07 9.72 }" (braces,
        # variable spacing) — strip braces, then take the first number.
        load1=$(sysctl -n vm.loadavg 2>/dev/null | tr -d '{}' | awk '{print $1}')
    fi
    [ -z "$load1" ] && return 0
    over=$(awk -v l="$load1" -v n="$ncpu" 'BEGIN { print (l > n * 0.7) ? 1 : 0 }')
    if [ "$over" = "1" ]; then
        echo "❌ 1-min load ${load1} on ${ncpu} cores — results would be noise."
        echo "   Quit heavy apps first, or re-run with BENCH_FORCE=1."
        exit 1
    fi
}

# median_of NUM... — median of a numeric list ("n/a" when empty).
median_of() {
    printf '%s\n' "$@" | sort -n | awk '
        { a[NR] = $1 }
        END {
            if (NR == 0) { print "n/a"; exit }
            if (NR % 2) printf "%.2f", a[(NR + 1) / 2]
            else printf "%.2f", (a[NR / 2] + a[NR / 2 + 1]) / 2
        }'
}

# run_one LABEL CLIENT_BIN SERVER_BIN
# One measurement pass; prints the live python output and sets TP_VAL/LAT_VAL.
# Uses bash 3.2-compatible globals (no associative arrays).
TP_VAL=""
LAT_VAL=""
run_one() {
    local label=$1 client_bin=$2 server_bin=$3
    local result_file
    result_file=$(mktemp /tmp/kcptun_bench.XXXXXX)

    echo "━━━ $label ━━━"

    if [ ! -x "$client_bin" ]; then
        echo "  ⏭️  Skipped (binary not found: $client_bin)"
        echo ""
        return 1
    fi
    if [ ! -x "$server_bin" ]; then
        echo "  ⏭️  Skipped (binary not found: $server_bin)"
        echo ""
        return 1
    fi

    cleanup
    next_ports

    start_echo       || { echo ""; cleanup; return 1; }
    start_server "$server_bin" || { echo ""; cleanup; return 1; }
    start_client "$client_bin" || { echo ""; cleanup; return 1; }

    # Progress goes to stderr (live); results are captured for parsing.
    python3 bench/throughput.py "$CLIENT_PORT" \
        --data-mb "$DATA_MB" --chunk-kb "$CHUNK_KB" \
        --latency-iterations "$LATENCY_ITERS" \
        --connections "$CONNECTIONS" \
        --timeout-seconds "$TRANSFER_TIMEOUT_SECONDS" > "$result_file" \
        || echo "  ❌ Benchmark failed"
    cat "$result_file"

    TP_VAL=$(grep 'MB/s' "$result_file" | awk -F': ' '/Throughput/ {print $2}' | awk '{print $1}' | head -1)
    LAT_VAL=$(grep 'median RTT' "$result_file" | awk -F': ' '/Latency/ {print $2}' | awk '{print $1}' | head -1)
    rm -f "$result_file"

    echo ""
    cleanup 2>/dev/null
    [ -n "$TP_VAL" ] || return 1
    return 0
}

# ═══════════════════════════════════════════════════════════════════════
# Header
# ═══════════════════════════════════════════════════════════════════════
echo "╔══════════════════════════════════════════════════════════════════╗"
echo "║          kcptun Performance Benchmark                            ║"
echo "║          Go vs Rust-Tokio                                        ║"
echo "╠══════════════════════════════════════════════════════════════════╣"
echo "║  Data:       ${DATA_MB} MB                                          ║"
echo "║  Chunk:      ${CHUNK_KB} KB                                         ║"
echo "║  Connections:${CONNECTIONS} (data size is per connection)                         ║"
echo "║  Crypto:     ${CRYPT}  mode=${MODE}  smuxver=${SMUXVER}                          ║"
echo "║  Windows:    sndwnd=${SNDWND} rcvwnd=${RCVWND}  Compression: OFF (--nocomp)     ║"
echo "║  Workers:    ${BENCH_WORKERS} (0=backend defaults)  Rounds: ${BENCH_ROUNDS} (median)      ║"
echo "║  Transfer timeout: ${TRANSFER_TIMEOUT_SECONDS}s (0=auto)                         ║"
echo "╚══════════════════════════════════════════════════════════════════╝"
echo ""

# ═══════════════════════════════════════════════════════════════════════
# Load guard
# ═══════════════════════════════════════════════════════════════════════
check_load

# ═══════════════════════════════════════════════════════════════════════
# Build check
# ═══════════════════════════════════════════════════════════════════════
echo "Checking binaries..."
echo "  Go server:           $([ -x "$GO_SERVER" ] && echo '✓' || echo '✗ (will skip)')"
echo "  Go client:           $([ -x "$GO_CLIENT" ] && echo '✓' || echo '✗ (will skip)')"
echo "  Rust-Tokio srv:      $([ -x "$RUST_TOKIO_SERVER" ] && echo '✓' || echo '✗ (run: make release)')"
echo "  Rust-Tokio cli:      $([ -x "$RUST_TOKIO_CLIENT" ] && echo '✓' || echo '✗ (run: make release)')"
echo ""

# ═══════════════════════════════════════════════════════════════════════
# Config table.  Label convention: "Client → Server" (bulk traffic leaves
# the client toward the server).
# ═══════════════════════════════════════════════════════════════════════
LABELS=(
    "Go → Go"
    "Rust-Tokio → Rust-Tokio"
    "Rust-Tokio → Go"
    "Go → Rust-Tokio"
)
CLIENTS=( "$GO_CLIENT" "$RUST_TOKIO_CLIENT" "$RUST_TOKIO_CLIENT" "$GO_CLIENT" )
SERVERS=( "$GO_SERVER" "$RUST_TOKIO_SERVER" "$GO_SERVER" "$RUST_TOKIO_SERVER" )
NC=${#LABELS[@]}

# Per-config result accumulator strings (bash 3.2: no assoc arrays).
TP_ALL=( "" "" "" "" )
LAT_ALL=( "" "" "" "" )

# Rounds rotate the config order so machine-state drift (thermal, background
# daemons, page-cache) cannot systematically favor one backend.
r=0
while [ "$r" -lt "$BENCH_ROUNDS" ]; do
    if [ "$BENCH_ROUNDS" -gt 1 ]; then
        echo "════════ ROUND $((r + 1)) / $BENCH_ROUNDS ════════"
        echo ""
    fi
    j=0
    while [ "$j" -lt "$NC" ]; do
        i=$(( (r + j) % NC ))
        if run_one "${LABELS[$i]}" "${CLIENTS[$i]}" "${SERVERS[$i]}"; then
            eval "TP_${i}=\"\${TP_${i}} \$TP_VAL\""
            eval "LAT_${i}=\"\${LAT_${i}} \$LAT_VAL\""
        else
            eval "FAILS_$i=\$((\${FAILS_$i:-0} + 1))"
        fi
        j=$((j + 1))
    done
    r=$((r + 1))
done

# ═══════════════════════════════════════════════════════════════════════
# Summary (median across rounds)
# ═══════════════════════════════════════════════════════════════════════
if [ "$BENCH_ROUNDS" -gt 1 ]; then
    echo "════════ SUMMARY (median of $BENCH_ROUNDS interleaved rounds) ════════"
    echo ""
    printf "%-28s %12s %12s %8s\n" "Path (Client → Server)" "Throughput" "Latency" "Fails"
    i=0
    while [ "$i" -lt "$NC" ]; do
        eval "tps=\${TP_${i}}"
        eval "lats=\${LAT_${i}}"
        eval "f=\${FAILS_$i:-0}"
        if [ -z "${tps# }" ]; then
            printf "%-28s %12s %12s %8s\n" "${LABELS[$i]}" "n/a" "n/a" "$f"
        else
            # shellcheck disable=SC2086
            printf "%-28s %12s %12s %8s\n" "${LABELS[$i]}" \
                "$(median_of $tps) MB/s" "$(median_of $lats) ms" "$f"
            echo "    rounds tp: $tps | lat: $lats"
        fi
        i=$((i + 1))
    done
    echo ""
fi

echo "Benchmark complete."
