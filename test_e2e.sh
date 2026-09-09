#!/bin/bash
# kcptun end-to-end interoperability test suite.
# Tests Go↔Rust compatibility for all encryption algorithms,
# KCP modes, SMUX versions, and compression settings.
set -eo pipefail
cd "$(dirname "$0")"

KEY="test-key"
GO_SERVER=./tests/kcptun-go/server
GO_CLIENT=./tests/kcptun-go/client
RUST_SERVER=./target/release/kcptun-server
RUST_CLIENT=./target/release/kcptun-client
PASS=0; FAIL=0; SKIP=0

# Dynamic port counter — each test uses 3 ports (echo, server, local)
PORT=$((20000 + $(date +%s | tail -c 5) * 3))

cleanup() {
    kill $ECHO_PID $SERVER_PID $CLIENT_PID 2>/dev/null || true
    wait 2>/dev/null || true
}

start_echo() {
    local port=$1
    python3 -u -c "
import socket, threading, sys
def echo(s,a):
    while True:
        d=s.recv(4096)
        if not d: break
        s.sendall(d)
    s.close()
s=socket.socket()
s.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1)
s.bind(('0.0.0.0',$port)); s.listen(10)
sys.stderr.write('echo on $port\n')
sys.stderr.flush()
while True: threading.Thread(target=echo,args=s.accept()).start()
" &
    ECHO_PID=$!
    sleep 1
    if ! kill -0 $ECHO_PID 2>/dev/null; then
        echo "  ❌ echo server failed on port $port"
        return 1
    fi
}

try_test() {
    local name="$1"
    local server_bin="$2" server_args="$3"
    local client_bin="$4" client_args="$5"
    local extra_desc="${6:-}"

    E=$PORT;       S=$((PORT+1));  L=$((PORT+2))
    PORT=$((PORT+3))

    local label="$name"
    [ -n "$extra_desc" ] && label="$name ($extra_desc)"
    echo "=== Test: $label ==="

    start_echo $E || { FAIL=$((FAIL+1)); return; }

    $server_bin -l "0.0.0.0:$S" -t "127.0.0.1:$E" --key "$KEY" $server_args 2>/dev/null &
    SERVER_PID=$!; sleep 2
    if ! kill -0 $SERVER_PID 2>/dev/null; then
        echo "  ❌ $label (server died)"
        cleanup; FAIL=$((FAIL+1)); return
    fi

    $client_bin -l "127.0.0.1:$L" -r "127.0.0.1:$S" --key "$KEY" $client_args 2>/dev/null &
    CLIENT_PID=$!; sleep 3
    if ! kill -0 $CLIENT_PID 2>/dev/null; then
        echo "  ❌ $label (client died)"
        cleanup; FAIL=$((FAIL+1)); return
    fi

    # Test echo
    MSG="ECHO_$(date +%s)_$RANDOM"
    python3 -c "
import socket, select, sys
s=socket.socket(); s.settimeout(10)
try:
    s.connect(('127.0.0.1',$L))
    s.sendall(b'$MSG\n')
    r,_,_ = select.select([s],[],[],10)
    if r:
        d = s.recv(1024).decode().strip()
        sys.exit(0 if '$MSG' in d else 1)
    else:
        sys.exit(2)
except Exception: sys.exit(3)
" && { echo "  ✅ $label"; PASS=$((PASS+1)); } || { echo "  ❌ $label"; FAIL=$((FAIL+1)); }

    cleanup
    sleep 1
}

# Skip a test (e.g., known incompatibility)
skip_test() {
    local name="$1" reason="$2"
    echo "=== Test: $name ==="
    echo "  ⏭️  $name (skipped: $reason)"
    SKIP=$((SKIP+1))
}

echo "Starting e2e test suite (port base: $PORT)"
echo "  Rust: $([ -x "$RUST_SERVER" ] && echo '✓' || echo '✗ (run: make release)')"
echo ""

# ═══════════════════════════════════════════════════════════════════════
# Section 1: Baseline cross-product (8 tests)
# ═══════════════════════════════════════════════════════════════════════
echo "━━━ Section 1: Baseline cross-product ━━━"

try_test "Go→Go nocomp"    "$GO_SERVER"   "--crypt aes --nocomp" "$GO_CLIENT"   "--crypt aes --nocomp"
try_test "Go→Go compress"  "$GO_SERVER"   "--crypt aes"          "$GO_CLIENT"   "--crypt aes"
try_test "Go→Rust nocomp"  "$RUST_SERVER" "--crypt aes --nocomp" "$GO_CLIENT"   "--crypt aes --nocomp"
try_test "Go→Rust compress" "$RUST_SERVER" "--crypt aes"          "$GO_CLIENT"   "--crypt aes"
try_test "Rust→Rust nocomp" "$RUST_SERVER" "--crypt aes --nocomp" "$RUST_CLIENT" "--crypt aes --nocomp"
try_test "Rust→Rust compress" "$RUST_SERVER" "--crypt aes"        "$RUST_CLIENT" "--crypt aes"
try_test "Rust→Go nocomp"   "$GO_SERVER"   "--crypt aes --nocomp" "$RUST_CLIENT" "--crypt aes --nocomp"
try_test "Rust→Go compress" "$GO_SERVER"   "--crypt aes"          "$RUST_CLIENT" "--crypt aes"

# ═══════════════════════════════════════════════════════════════════════
# Section 2: Encryption algorithm compatibility (Go↔Rust, nocomp)
# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "━━━ Section 2: Encryption algorithms (Go↔Rust, --nocomp) ━━━"

# All Go-compatible ciphers (including cast5 with full CAST5 implementation)
CRYPTS="null none xor aes-128 aes-192 aes sm4 tea xtea salsa20 blowfish twofish cast5 3des aes-128-gcm"

for crypt in $CRYPTS; do
    # Go client → Rust server
    try_test "Go→Rust crypt=$crypt" "$RUST_SERVER" "--crypt $crypt --nocomp" "$GO_CLIENT" "--crypt $crypt --nocomp"

    # Rust client → Go server
    try_test "Rust→Go crypt=$crypt" "$GO_SERVER" "--crypt $crypt --nocomp" "$RUST_CLIENT" "--crypt $crypt --nocomp"
done

# CAST5 is now fully implemented and Go-compatible

# ═══════════════════════════════════════════════════════════════════════
# Section 3: KCP mode compatibility (Go↔Rust)
# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "━━━ Section 3: KCP modes (Go↔Rust, crypt=aes, nocomp) ━━━"

MODES="normal fast fast2 fast3"

for mode in $MODES; do
    # Go client → Rust server
    try_test "Go→Rust mode=$mode" "$RUST_SERVER" "--crypt aes --mode $mode --nocomp" "$GO_CLIENT" "--crypt aes --mode $mode --nocomp"

    # Rust client → Go server
    try_test "Rust→Go mode=$mode" "$GO_SERVER" "--crypt aes --mode $mode --nocomp" "$RUST_CLIENT" "--crypt aes --mode $mode --nocomp"
done

# ═══════════════════════════════════════════════════════════════════════
# Section 4: SMUX version compatibility (Go↔Rust)
# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "━━━ Section 4: SMUX versions (Go↔Rust, crypt=aes, nocomp) ━━━"

# SMUX v1
try_test "Go→Rust smuxver=1" "$RUST_SERVER" "--crypt aes --smuxver 1 --nocomp" "$GO_CLIENT" "--crypt aes --smuxver 1 --nocomp"
try_test "Rust→Go smuxver=1" "$GO_SERVER"   "--crypt aes --smuxver 1 --nocomp" "$RUST_CLIENT" "--crypt aes --smuxver 1 --nocomp"

# SMUX v2 (already tested in baseline, but explicit)
try_test "Go→Rust smuxver=2" "$RUST_SERVER" "--crypt aes --smuxver 2 --nocomp" "$GO_CLIENT" "--crypt aes --smuxver 2 --nocomp"
try_test "Rust→Go smuxver=2" "$GO_SERVER"   "--crypt aes --smuxver 2 --nocomp" "$RUST_CLIENT" "--crypt aes --smuxver 2 --nocomp"

# ═══════════════════════════════════════════════════════════════════════
# Section 5: Encryption + compression (Go↔Rust)
# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "━━━ Section 5: Encryption + compression (Go↔Rust) ━━━"

# Test a subset of ciphers WITH compression to verify Snappy + crypto interop
COMP_CRYPTS="aes-128 aes sm4 tea blowfish twofish 3des"

for crypt in $COMP_CRYPTS; do
    try_test "Go→Rust crypt=$crypt +compress" "$RUST_SERVER" "--crypt $crypt" "$GO_CLIENT" "--crypt $crypt"
    try_test "Rust→Go crypt=$crypt +compress" "$GO_SERVER"   "--crypt $crypt" "$RUST_CLIENT" "--crypt $crypt"
done

# ═══════════════════════════════════════════════════════════════════════
# Section 6: FEC (Forward Error Correction) Go↔Rust
# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "━━━ Section 6: FEC (Go↔Rust, crypt=aes, nocomp) ━━━"

try_test "Go→Rust FEC 10/3"   "$RUST_SERVER" "--crypt aes --nocomp --datashard 10 --parityshard 3"  "$GO_CLIENT" "--crypt aes --nocomp --datashard 10 --parityshard 3"
try_test "Rust→Go FEC 10/3"   "$GO_SERVER"   "--crypt aes --nocomp --datashard 10 --parityshard 3"  "$RUST_CLIENT" "--crypt aes --nocomp --datashard 10 --parityshard 3"
try_test "Go→Rust FEC 4/2"   "$RUST_SERVER" "--crypt aes --nocomp --datashard 4 --parityshard 2"   "$GO_CLIENT" "--crypt aes --nocomp --datashard 4 --parityshard 2"
try_test "Rust→Go FEC 4/2"   "$GO_SERVER"   "--crypt aes --nocomp --datashard 4 --parityshard 2"   "$RUST_CLIENT" "--crypt aes --nocomp --datashard 4 --parityshard 2"

# ═══════════════════════════════════════════════════════════════════════
# tcpraw --tcp transport (Linux+root only)
# ═══════════════════════════════════════════════════════════════════════
if [ "$(uname -s)" = "Linux" ] && [ "$(id -u)" = "0" ]; then
    echo ""
    echo "═══════ tcpraw --tcp transport ═══════"
    # Use KCPTCP_TAKEOVER=repair to skip iptables requirement for e2e
    export KCPTCP_TAKEOVER=repair
    try_test "Rust→Go --tcp" "$RUST_SERVER" "--tcp --crypt none --nocomp" "$GO_CLIENT" "--crypt none --nocomp --tcp"
    try_test "Go→Rust --tcp" "$GO_SERVER" "--crypt none --nocomp --tcp" "$RUST_CLIENT" "--tcp --crypt none --nocomp"
    unset KCPTCP_TAKEOVER
else
    echo "  ⏭️  tcpraw --tcp tests skipped (needs Linux + root)"
fi

# ═══════════════════════════════════════════════════════════════════════
# Results
# ═══════════════════════════════════════════════════════════════════════
echo ""
echo "═══════════════════════════════════════"
echo "  Results: $PASS passed, $FAIL failed, $SKIP skipped"
echo "═══════════════════════════════════════"
exit $FAIL
