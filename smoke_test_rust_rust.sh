#!/bin/bash
# ═══════════════════════════════════════════════════════════════════════════
# kcptun-rs Rust↔Rust 冒烟测试（稳定性聚焦）
#
# 两组：
#   Group A: Rust(tokio)  ↔ Rust(tokio)
#   Group B: Rust(smol)   ↔ Rust(smol)
#
# 用法:
#   bash smoke_test_rust_rust.sh           # 全量
#   bash smoke_test_rust_rust.sh tokio     # 仅 tokio 组
#   bash smoke_test_rust_rust.sh smol      # 仅 smol 组
#
# 前置:
#   make release           # 构建 tokio release
#   make release-smol      # 构建 smol release
# ═══════════════════════════════════════════════════════════════════════════
set -eo pipefail
cd "$(dirname "$0")"

# ─── 全局变量 ───────────────────────────────────────────────────────────────
KEY="smoke-test-key"
PASS=0; FAIL=0; SKIP=0
PORT=$((30000 + $(date +%s | tail -c 4) * 7))

# 二进制路径
TOKIO_SERVER="./target/release/kcptun-server"
TOKIO_CLIENT="./target/release/kcptun-client"
SMOL_SERVER="./target/smol-release/release/kcptun-server"
SMOL_CLIENT="./target/smol-release/release/kcptun-client"

# 进程 PID（全局，cleanup 用）
ECHO_PID=""
SERVER_PID=""
CLIENT_PID=""

# 颜色 — 用 $'...' (ANSI-C quoting) 让转义序列在赋值时就被解释为真正的控制字符
RED=$'\033[0;31m'
GREEN=$'\033[0;32m'
YELLOW=$'\033[0;33m'
CYAN=$'\033[0;36m'
NC=$'\033[0m'

# ─── 辅助函数 ───────────────────────────────────────────────────────────────

cleanup() {
    kill $ECHO_PID $SERVER_PID $CLIENT_PID 2>/dev/null || true
    wait 2>/dev/null || true
}

# 启动 TCP echo server（多线程）
start_echo() {
    local port=$1
    python3 -u -c "
import socket, threading, sys
def echo(s, a):
    try:
        while True:
            d = s.recv(65536)
            if not d: break
            s.sendall(d)
    except Exception:
        pass
    s.close()
s = socket.socket()
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(('0.0.0.0', $port))
s.listen(256)
sys.stderr.write('echo on $port\n')
sys.stderr.flush()
while True:
    threading.Thread(target=echo, args=s.accept(), daemon=True).start()
" 2>/dev/null &
    ECHO_PID=$!
    sleep 1
    if ! kill -0 $ECHO_PID 2>/dev/null; then
        echo "  ${RED}echo server 启动失败 (port $port)${NC}"
        return 1
    fi
}

# 核心测试函数：启动 server+client 隧道，发送 echo 验证数据完整性
# 参数: name  server_bin  server_args  client_bin  client_args  payload_size  timeout  [extra_desc]
try_test() {
    local name="$1"
    local server_bin="$2" server_args="$3"
    local client_bin="$4" client_args="$5"
    local payload_size="${6:-256}"
    local timeout_secs="${7:-30}"
    local extra_desc="${8:-}"

    local E=$PORT;       local S=$((PORT+1));  local L=$((PORT+2))
    PORT=$((PORT+7))

    local label="$name"
    [ -n "$extra_desc" ] && label="$name ($extra_desc)"

    if ! start_echo $E; then
        FAIL=$((FAIL+1)); return
    fi

    $server_bin -l "0.0.0.0:$S" -t "127.0.0.1:$E" --key "$KEY" $server_args 2>/dev/null &
    SERVER_PID=$!
    sleep 2
    if ! kill -0 $SERVER_PID 2>/dev/null; then
        echo "  ${RED}❌ $label (server 启动后退出)${NC}"
        cleanup; FAIL=$((FAIL+1)); return
    fi

    $client_bin -l "127.0.0.1:$L" -r "127.0.0.1:$S" --key "$KEY" $client_args 2>/dev/null &
    CLIENT_PID=$!
    sleep 3
    if ! kill -0 $CLIENT_PID 2>/dev/null; then
        echo "  ${RED}❌ $label (client 启动后退出)${NC}"
        cleanup; FAIL=$((FAIL+1)); return
    fi

    local MSG="SMOKE_$(date +%s)_$$"
    python3 -c "
import socket, select, sys
s = socket.socket()
s.settimeout($timeout_secs)
try:
    s.connect(('127.0.0.1', $L))
    data = bytes([($payload_size + i) % 256 for i in range($payload_size)])
    tag = b'$MSG\n'
    s.sendall(tag + data)
    expected = len(tag) + len(data)
    recv_buf = b''
    while len(recv_buf) < expected:
        r, _, _ = select.select([s], [], [], $timeout_secs)
        if not r:
            print(f'TIMEOUT: got {len(recv_buf)}/{expected} bytes', file=sys.stderr)
            sys.exit(2)
        chunk = s.recv(65536)
        if not chunk:
            break
        recv_buf += chunk
    if len(recv_buf) >= expected:
        tag_recv = recv_buf[:len(tag)]
        data_recv = recv_buf[len(tag):]
        if tag_recv == tag and data_recv == data:
            sys.exit(0)
        else:
            for i in range(min(len(data), len(data_recv))):
                if data[i] != data_recv[i]:
                    print(f'MISMATCH at byte {i}: sent {data[i]:02x}, got {data_recv[i]:02x}', file=sys.stderr)
                    break
            sys.exit(1)
    else:
        print(f'SHORT: got {len(recv_buf)}/{expected} bytes', file=sys.stderr)
        sys.exit(3)
except Exception as e:
    print(f'ERROR: {e}', file=sys.stderr)
    sys.exit(4)
finally:
    try: s.close()
    except: pass
" && { echo "  ${GREEN}✅ $label${NC} (${payload_size}B)"; PASS=$((PASS+1)); } || {
        echo "  ${RED}❌ $label${NC} (${payload_size}B)"
        FAIL=$((FAIL+1));
    }

    cleanup
    sleep 1
}

# 多并发连接测试
# 参数: name  server_bin  server_args  client_bin  client_args  num_conns  payload_size  timeout
try_concurrency() {
    local name="$1"
    local server_bin="$2" server_args="$3"
    local client_bin="$4" client_args="$5"
    local num_conns="${6:-10}"
    local payload_size="${7:-4096}"
    local timeout_secs="${8:-60}"

    local E=$PORT;       local S=$((PORT+1));  local L=$((PORT+2))
    PORT=$((PORT+7))

    local label="$name (${num_conns}并发 × ${payload_size}B)"

    if ! start_echo $E; then
        FAIL=$((FAIL+1)); return
    fi

    $server_bin -l "0.0.0.0:$S" -t "127.0.0.1:$E" --key "$KEY" $server_args 2>/dev/null &
    SERVER_PID=$!
    sleep 2
    if ! kill -0 $SERVER_PID 2>/dev/null; then
        echo "  ${RED}❌ $label (server died)${NC}"
        cleanup; FAIL=$((FAIL+1)); return
    fi

    $client_bin -l "127.0.0.1:$L" -r "127.0.0.1:$S" --key "$KEY" $client_args 2>/dev/null &
    CLIENT_PID=$!
    sleep 3
    if ! kill -0 $CLIENT_PID 2>/dev/null; then
        echo "  ${RED}❌ $label (client died)${NC}"
        cleanup; FAIL=$((FAIL+1)); return
    fi

    python3 -c "
import socket, threading, sys, time
results = [False] * $num_conns
errors = [''] * $num_conns
def worker(idx):
    try:
        s = socket.socket()
        s.settimeout($timeout_secs)
        s.connect(('127.0.0.1', $L))
        data = bytes([(idx + i) % 256 for i in range($payload_size)])
        s.sendall(data)
        recv_buf = b''
        while len(recv_buf) < len(data):
            chunk = s.recv(65536)
            if not chunk: break
            recv_buf += chunk
        if recv_buf == data:
            results[idx] = True
        else:
            errors[idx] = f'len sent={len(data)} recv={len(recv_buf)}'
        s.close()
    except Exception as e:
        errors[idx] = str(e)

threads = []
for i in range($num_conns):
    t = threading.Thread(target=worker, args=(i,))
    threads.append(t)
    t.start()

for t in threads:
    t.join(timeout=$timeout_secs + 10)

ok = sum(1 for r in results if r)
failed = $num_conns - ok
if failed > 0:
    details = []
    for i in range($num_conns):
        if not results[i]:
            details.append(f'conn{i}: {errors[i]}')
    print(f'FAIL: {failed}/$num_conns failed: {\"; \".join(details[:5])}', file=sys.stderr)
    sys.exit(1)
else:
    sys.exit(0)
" && { echo "  ${GREEN}✅ $label${NC} — 全部 $num_conns 连接数据一致"; PASS=$((PASS+1)); } || {
        echo "  ${RED}❌ $label${NC}"
        FAIL=$((FAIL+1));
    }

    cleanup
    sleep 1
}

# 连接抖动测试 — 快速建连/断连，验证 SMUX 流清理和内存稳定性
# 参数: name  server_bin  server_args  client_bin  client_args  num_cycles  payload_size
try_churn() {
    local name="$1"
    local server_bin="$2" server_args="$3"
    local client_bin="$4" client_args="$5"
    local num_cycles="${6:-50}"
    local payload_size="${7:-1024}"

    local E=$PORT;       local S=$((PORT+1));  local L=$((PORT+2))
    PORT=$((PORT+7))

    local label="$name (抖动 ${num_cycles}次 × ${payload_size}B)"

    if ! start_echo $E; then
        FAIL=$((FAIL+1)); return
    fi

    $server_bin -l "0.0.0.0:$S" -t "127.0.0.1:$E" --key "$KEY" $server_args 2>/dev/null &
    SERVER_PID=$!
    sleep 2
    if ! kill -0 $SERVER_PID 2>/dev/null; then
        echo "  ${RED}❌ $label (server died)${NC}"
        cleanup; FAIL=$((FAIL+1)); return
    fi

    $client_bin -l "127.0.0.1:$L" -r "127.0.0.1:$S" --key "$KEY" $client_args 2>/dev/null &
    CLIENT_PID=$!
    sleep 3
    if ! kill -0 $CLIENT_PID 2>/dev/null; then
        echo "  ${RED}❌ $label (client died)${NC}"
        cleanup; FAIL=$((FAIL+1)); return
    fi

    python3 -c "
import socket, sys, time
ok = 0
fail = 0
for i in range($num_cycles):
    try:
        s = socket.socket()
        s.settimeout(10)
        s.connect(('127.0.0.1', $L))
        data = bytes([(i + j) % 256 for j in range($payload_size)])
        s.sendall(data)
        recv_buf = b''
        while len(recv_buf) < len(data):
            chunk = s.recv(65536)
            if not chunk: break
            recv_buf += chunk
        if recv_buf == data:
            ok += 1
        else:
            fail += 1
        s.close()
        time.sleep(0.05)
    except Exception:
        fail += 1
if fail == 0:
    sys.exit(0)
else:
    print(f'churn FAIL: {fail}/$num_cycles ({ok} ok)', file=sys.stderr)
    sys.exit(1)
" && { echo "  ${GREEN}✅ $label${NC} — ${num_cycles} 次建断连全部成功"; PASS=$((PASS+1)); } || {
        echo "  ${RED}❌ $label${NC}"
        FAIL=$((FAIL+1));
    }

    cleanup
    sleep 1
}

# 长连接保活测试
# 参数: name  server_bin  server_args  client_bin  client_args  duration_secs  interval_secs
try_keepalive() {
    local name="$1"
    local server_bin="$2" server_args="$3"
    local client_bin="$4" client_args="$5"
    local duration="${6:-30}"
    local interval="${7:-5}"

    local E=$PORT;       local S=$((PORT+1));  local L=$((PORT+2))
    PORT=$((PORT+7))

    local label="$name (保活 ${duration}s, 间隔 ${interval}s)"

    if ! start_echo $E; then
        FAIL=$((FAIL+1)); return
    fi

    $server_bin -l "0.0.0.0:$S" -t "127.0.0.1:$E" --key "$KEY" $server_args 2>/dev/null &
    SERVER_PID=$!
    sleep 2
    if ! kill -0 $SERVER_PID 2>/dev/null; then
        echo "  ${RED}❌ $label (server died)${NC}"
        cleanup; FAIL=$((FAIL+1)); return
    fi

    $client_bin -l "127.0.0.1:$L" -r "127.0.0.1:$S" --key "$KEY" $client_args 2>/dev/null &
    CLIENT_PID=$!
    sleep 3
    if ! kill -0 $CLIENT_PID 2>/dev/null; then
        echo "  ${RED}❌ $label (client died)${NC}"
        cleanup; FAIL=$((FAIL+1)); return
    fi

    python3 -c "
import socket, sys, time
s = socket.socket()
s.settimeout(10)
s.connect(('127.0.0.1', $L))
rounds = int($duration / $interval)
ok = 0
for r in range(rounds):
    try:
        data = bytes([(r + i) % 256 for i in range(256)])
        s.sendall(data)
        recv_buf = b''
        while len(recv_buf) < len(data):
            chunk = s.recv(65536)
            if not chunk: break
            recv_buf += chunk
        if recv_buf == data:
            ok += 1
        time.sleep($interval)
    except Exception as e:
        print(f'round {r} error: {e}', file=sys.stderr)
        sys.exit(1)
s.close()
if ok == rounds:
    sys.exit(0)
else:
    print(f'keepalive: {ok}/{rounds} rounds ok', file=sys.stderr)
    sys.exit(1)
" && { echo "  ${GREEN}✅ $label${NC} — ${duration}s 内 ${ok:-$duration} 轮保活成功"; PASS=$((PASS+1)); } || {
        echo "  ${RED}❌ $label${NC}"
        FAIL=$((FAIL+1));
    }

    cleanup
    sleep 1
}

# ─── 多线程数据完整性测试 ──────────────────────────────────────────────────
#
# 核心差异点（vs try_test / try_concurrency）：
#
#   1. 多线程:        N 个线程同时通过同一隧道发送数据（单 KCP 通道，多 SMUX 流）
#   2. 多轮多尺寸:    每个线程在同一个连接上发送多个不同大小的 payload
#                      (1B → 4KB → 64KB → 128KB)，覆盖从单段到超长分片
#   3. 防流混淆模式:  payload[i] = ((conn_id * 31 + round * 17 + i) % 256) ^ 0xA5
#                      — 每个 (连接, 轮次, 字节位置) 产生唯一值
#                      — 若 stream A 的数据泄漏到 stream B 的响应，模式不匹配 → 立即检出
#   4. 逐字节校验:    每个 payload 的 echo 响应与原始数据逐字节比对
#                      — 首个不匹配字节位置 + hex dump 输出
#   5. 全失败收集:    不因第一个失败就退出，收集所有失败详情后统一报告
#
# 参数: name  server_bin  server_args  client_bin  client_args  num_threads  timeout
try_multithread_integrity() {
    local name="$1"
    local server_bin="$2" server_args="$3"
    local client_bin="$4" client_args="$5"
    local num_threads="${6:-20}"
    local timeout_secs="${7:-120}"

    local E=$PORT;       local S=$((PORT+1));  local L=$((PORT+2))
    PORT=$((PORT+7))

    local label="$name (${num_threads}线程 × 4尺寸, 逐字节校验)"

    if ! start_echo $E; then
        FAIL=$((FAIL+1)); return
    fi

    $server_bin -l "0.0.0.0:$S" -t "127.0.0.1:$E" --key "$KEY" $server_args 2>/dev/null &
    SERVER_PID=$!
    sleep 2
    if ! kill -0 $SERVER_PID 2>/dev/null; then
        echo "  ${RED}❌ $label (server died)${NC}"
        cleanup; FAIL=$((FAIL+1)); return
    fi

    $client_bin -l "127.0.0.1:$L" -r "127.0.0.1:$S" --key "$KEY" $client_args 2>/dev/null &
    CLIENT_PID=$!
    sleep 3
    if ! kill -0 $CLIENT_PID 2>/dev/null; then
        echo "  ${RED}❌ $label (client died)${NC}"
        cleanup; FAIL=$((FAIL+1)); return
    fi

    # 多线程数据完整性测试
    # 每个线程在同一个 TCP 连接上依次发送 4 个不同大小的 payload
    # 数据模式: byte[i] = ((conn_id * 31 + round * 17 + i) % 256) ^ 0xA5
    # — conn_id 和 round 都参与，确保跨连接、跨轮次的数据模式唯一
    # — 若 stream A 的数据泄漏进 stream B 的 echo，模式不匹配 → 检出流混淆
    python3 -c "
import socket, threading, sys, time, json

# 每个线程发送的 payload 尺寸序列
SIZES = [
    (1, '1B'),
    (4096, '4KB'),
    (65536, '64KB'),
    (131072, '128KB'),
]

def make_payload(conn_id, round_idx, size):
    \"\"\"生成确定性 payload，每个 (conn, round, position) 唯一\"\"\"
    return bytes([((conn_id * 31 + round_idx * 17 + i) % 256) ^ 0xA5 for i in range(size)])

def send_and_verify(sock, data, timeout):
    \"\"\"发送 data，接收 echo，逐字节校验。返回 (ok, error_msg)\"\"\"
    try:
        sock.settimeout(timeout)
        sock.sendall(data)
        recv_buf = b''
        while len(recv_buf) < len(data):
            chunk = sock.recv(65536)
            if not chunk:
                break
            recv_buf += chunk
        if len(recv_buf) != len(data):
            return (False, f'len mismatch: sent={len(data)} recv={len(recv_buf)}')
        # 逐字节比对
        for i in range(len(data)):
            if data[i] != recv_buf[i]:
                end = min(i + 16, len(data), len(recv_buf))
                return (False,
                    f'byte {i} mismatch: sent={data[i]:02x} got={recv_buf[i]:02x} '
                    f'expected={data[i:end].hex()} got={recv_buf[i:end].hex()}')
        return (True, '')
    except Exception as e:
        return (False, str(e))

def worker(conn_id, results):
    \"\"\"每个线程：建连 → 发 4 个 payload → 逐个校验 → 收集结果\"\"\"
    errors = []
    try:
        s = socket.socket()
        s.settimeout($timeout_secs)
        s.connect(('127.0.0.1', $L))
    except Exception as e:
        results[conn_id] = {'status': 'connect_fail', 'error': str(e), 'details': errors}
        return

    for round_idx, (size, label) in enumerate(SIZES):
        data = make_payload(conn_id, round_idx, size)
        ok, err = send_and_verify(s, data, $timeout_secs)
        if not ok:
            errors.append({'round': round_idx, 'label': label, 'size': size, 'error': err})
        # 短暂间隔，让 KCP flush 有机会跑
        time.sleep(0.1)

    try:
        s.close()
    except:
        pass

    if errors:
        results[conn_id] = {'status': 'fail', 'error': f'{len(errors)}/{len(SIZES)} payloads failed', 'details': errors}
    else:
        results[conn_id] = {'status': 'ok', 'error': '', 'details': []}

# 启动线程
results = {}
threads = []
for i in range($num_threads):
    t = threading.Thread(target=worker, args=(i, results))
    threads.append(t)
    t.start()

# 等待全部完成
for t in threads:
    t.join(timeout=$timeout_secs * len(SIZES) + 30)

# 汇总
ok_count = sum(1 for v in results.values() if v['status'] == 'ok')
fail_count = $num_threads - ok_count

if fail_count == 0:
    print(f'ALL OK: {$num_threads} threads × {len(SIZES)} payloads each, all verified byte-for-byte', file=sys.stderr)
    sys.exit(0)
else:
    print(f'FAIL: {fail_count}/{$num_threads} threads failed', file=sys.stderr)
    for conn_id in sorted(results.keys()):
        r = results[conn_id]
        if r['status'] != 'ok':
            print(f'  conn{conn_id}: {r[\"status\"]} — {r[\"error\"]}', file=sys.stderr)
            for d in r['details'][:3]:
                print(f'    round {d[\"round\"]} ({d[\"label\"]}, {d[\"size\"]}B): {d[\"error\"]}', file=sys.stderr)
    sys.exit(1)
" && { echo "  ${GREEN}✅ $label${NC} — ${num_threads}线程 × 4尺寸全部逐字节校验通过"; PASS=$((PASS+1)); } || {
        echo "  ${RED}❌ $label${NC}"
        FAIL=$((FAIL+1));
    }

    cleanup
    sleep 1
}

# 多轮单连接数据完整性测试 — 同一连接上连续发送多轮不同大小数据
# 参数: name  server_bin  server_args  client_bin  client_args  num_rounds  timeout
try_multiround_integrity() {
    local name="$1"
    local server_bin="$2" server_args="$3"
    local client_bin="$4" client_args="$5"
    local num_rounds="${6:-20}"
    local timeout_secs="${7:-60}"

    local E=$PORT;       local S=$((PORT+1));  local L=$((PORT+2))
    PORT=$((PORT+7))

    local label="$name (${num_rounds}轮 × 混合尺寸, 逐字节校验)"

    if ! start_echo $E; then
        FAIL=$((FAIL+1)); return
    fi

    $server_bin -l "0.0.0.0:$S" -t "127.0.0.1:$E" --key "$KEY" $server_args 2>/dev/null &
    SERVER_PID=$!
    sleep 2
    if ! kill -0 $SERVER_PID 2>/dev/null; then
        echo "  ${RED}❌ $label (server died)${NC}"
        cleanup; FAIL=$((FAIL+1)); return
    fi

    $client_bin -l "127.0.0.1:$L" -r "127.0.0.1:$S" --key "$KEY" $client_args 2>/dev/null &
    CLIENT_PID=$!
    sleep 3
    if ! kill -0 $CLIENT_PID 2>/dev/null; then
        echo "  ${RED}❌ $label (client died)${NC}"
        cleanup; FAIL=$((FAIL+1)); return
    fi

    # 单连接多轮：每轮发送不同大小的 payload，逐字节校验
    # 模式: byte[i] = ((round * 17 + i) % 256) ^ 0xA5
    python3 -c "
import socket, sys, time

SIZES = [1, 64, 256, 1024, 4096, 16384, 65536, 131072]

def make_payload(round_idx, size):
    return bytes([((round_idx * 17 + i) % 256) ^ 0xA5 for i in range(size)])

s = socket.socket()
s.settimeout($timeout_secs)
try:
    s.connect(('127.0.0.1', $L))
except Exception as e:
    print(f'connect fail: {e}', file=sys.stderr)
    sys.exit(1)

errors = []
for r in range($num_rounds):
    size = SIZES[r % len(SIZES)]
    data = make_payload(r, size)
    try:
        s.sendall(data)
        recv_buf = b''
        while len(recv_buf) < len(data):
            chunk = s.recv(65536)
            if not chunk: break
            recv_buf += chunk
        if len(recv_buf) != len(data):
            errors.append(f'round {r} ({size}B): len mismatch sent={len(data)} recv={len(recv_buf)}')
        else:
            for i in range(len(data)):
                if data[i] != recv_buf[i]:
                    errors.append(f'round {r} ({size}B): byte {i} mismatch sent={data[i]:02x} got={recv_buf[i]:02x}')
                    break
    except Exception as e:
        errors.append(f'round {r} ({size}B): {e}')
    time.sleep(0.05)

try: s.close()
except: pass

if errors:
    print(f'FAIL: {len(errors)}/{num_rounds} rounds failed', file=sys.stderr)
    for e in errors[:5]:
        print(f'  {e}', file=sys.stderr)
    sys.exit(1)
else:
    print(f'ALL OK: {$num_rounds} rounds, all verified byte-for-byte', file=sys.stderr)
    sys.exit(0)
" && { echo "  ${GREEN}✅ $label${NC} — ${num_rounds}轮混合尺寸逐字节校验通过"; PASS=$((PASS+1)); } || {
        echo "  ${RED}❌ $label${NC}"
        FAIL=$((FAIL+1));
    }

    cleanup
    sleep 1
}

# ─── QPP (Quantum Permutation Pad) 测试 ─────────────────────────────────────
# 验证 Go 兼容的 --QPP --QPPCount 在 SMUX stream 层不破坏数据完整性
# 参数: name  server_bin  server_args  client_bin  client_args  payload_size  timeout
try_qpp() {
    local name="$1"
    local server_bin="$2" server_args="$3"
    local client_bin="$4" client_args="$5"
    local payload_size="${6:-4096}"
    local timeout_secs="${7:-30}"

    local E=$PORT;       local S=$((PORT+1));  local L=$((PORT+2))
    PORT=$((PORT+7))

    local label="$name (QPP on, ${payload_size}B)"

    if ! start_echo $E; then
        FAIL=$((FAIL+1)); return
    fi

    $server_bin -l "0.0.0.0:$S" -t "127.0.0.1:$E" --key "$KEY" --QPP --QPPCount 61 $server_args 2>/dev/null &
    SERVER_PID=$!
    sleep 2
    if ! kill -0 $SERVER_PID 2>/dev/null; then
        echo "  ${RED}❌ $label (server died)${NC}"
        cleanup; FAIL=$((FAIL+1)); return
    fi

    $client_bin -l "127.0.0.1:$L" -r "127.0.0.1:$S" --key "$KEY" --QPP --QPPCount 61 $client_args 2>/dev/null &
    CLIENT_PID=$!
    sleep 3
    if ! kill -0 $CLIENT_PID 2>/dev/null; then
        echo "  ${RED}❌ $label (client died)${NC}"
        cleanup; FAIL=$((FAIL+1)); return
    fi

    local MSG="QPP_$(date +%s)_$$"
    python3 -c "
import socket, select, sys
s = socket.socket()
s.settimeout($timeout_secs)
try:
    s.connect(('127.0.0.1', $L))
    data = bytes([($payload_size + i) % 256 for i in range($payload_size)])
    tag = b'$MSG\n'
    s.sendall(tag + data)
    expected = len(tag) + len(data)
    recv_buf = b''
    while len(recv_buf) < expected:
        r, _, _ = select.select([s], [], [], $timeout_secs)
        if not r:
            print(f'TIMEOUT: got {len(recv_buf)}/{expected}', file=sys.stderr)
            sys.exit(2)
        chunk = s.recv(65536)
        if not chunk: break
        recv_buf += chunk
    if len(recv_buf) >= expected:
        tag_recv = recv_buf[:len(tag)]
        data_recv = recv_buf[len(tag):]
        if tag_recv == tag and data_recv == data:
            sys.exit(0)
        else:
            for i in range(min(len(data), len(data_recv))):
                if data[i] != data_recv[i]:
                    print(f'MISMATCH at byte {i}: sent {data[i]:02x}, got {data_recv[i]:02x}', file=sys.stderr)
                    break
            sys.exit(1)
    else:
        print(f'SHORT: got {len(recv_buf)}/{expected}', file=sys.stderr)
        sys.exit(3)
except Exception as e:
    print(f'ERROR: {e}', file=sys.stderr)
    sys.exit(4)
finally:
    try: s.close()
    except: pass
" && { echo "  ${GREEN}✅ $label${NC}"; PASS=$((PASS+1)); } || {
        echo "  ${RED}❌ $label${NC}"
        FAIL=$((FAIL+1));
    }

    cleanup
    sleep 1
}

# ─── 全双工双向同时传输测试 ────────────────────────────────────────────────
# 两个线程在同一连接上：一个持续发送，另一个持续接收 echo
# 验证双向同时传输不死锁、不数据错乱
# 参数: name  server_bin  server_args  client_bin  client_args  total_size  timeout
try_full_duplex() {
    local name="$1"
    local server_bin="$2" server_args="$3"
    local client_bin="$4" client_args="$5"
    local total_size="${6:-1048576}"
    local timeout_secs="${7:-60}"

    local E=$PORT;       local S=$((PORT+1));  local L=$((PORT+2))
    PORT=$((PORT+7))

    local label="$name (全双工 ${total_size}B)"

    if ! start_echo $E; then
        FAIL=$((FAIL+1)); return
    fi

    $server_bin -l "0.0.0.0:$S" -t "127.0.0.1:$E" --key "$KEY" $server_args 2>/dev/null &
    SERVER_PID=$!
    sleep 2
    if ! kill -0 $SERVER_PID 2>/dev/null; then
        echo "  ${RED}❌ $label (server died)${NC}"
        cleanup; FAIL=$((FAIL+1)); return
    fi

    $client_bin -l "127.0.0.1:$L" -r "127.0.0.1:$S" --key "$KEY" $client_args 2>/dev/null &
    CLIENT_PID=$!
    sleep 3
    if ! kill -0 $CLIENT_PID 2>/dev/null; then
        echo "  ${RED}❌ $label (client died)${NC}"
        cleanup; FAIL=$((FAIL+1)); return
    fi

    # 全双工：sender 线程持续发送，receiver 线程持续接收
    # 数据模式: byte[i] = (i % 256) ^ 0xA5
    python3 -c "
import socket, threading, sys, time

total_size = $total_size
chunk_size = 65536
sent_total = [0]
recv_total = [0]
error = [None]

def make_chunk(offset, size):
    return bytes([((offset + i) % 256) ^ 0xA5 for i in range(size)])

def sender(sock):
    \"\"\"持续发送直到 total_size 字节发完\"\"\"
    try:
        offset = 0
        while offset < total_size:
            sz = min(chunk_size, total_size - offset)
            data = make_chunk(offset, sz)
            sock.sendall(data)
            offset += sz
            sent_total[0] = offset
            time.sleep(0.001)  # 微小间隔，让接收侧有机会跑
    except Exception as e:
        error[0] = f'sender: {e}'

def receiver(sock):
    \"\"\"持续接收并逐块校验\"\"\"
    try:
        offset = 0
        while offset < total_size:
            chunk = sock.recv(65536)
            if not chunk:
                break
            expected = make_chunk(offset, len(chunk))
            if chunk != expected:
                for i in range(len(chunk)):
                    if chunk[i] != expected[i]:
                        error[0] = f'recv mismatch at offset {offset+i}: expected {expected[i]:02x} got {chunk[i]:02x}'
                        return
                error[0] = f'recv mismatch at offset {offset}'
                return
            offset += len(chunk)
            recv_total[0] = offset
    except Exception as e:
        error[0] = f'receiver: {e}'

s = socket.socket()
s.settimeout($timeout_secs)
s.connect(('127.0.0.1', $L))

t_send = threading.Thread(target=sender, args=(s,))
t_recv = threading.Thread(target=receiver, args=(s,))
t_send.start()
t_recv.start()
t_send.join(timeout=$timeout_secs + 10)
t_recv.join(timeout=$timeout_secs + 10)

try: s.close()
except: pass

if error[0]:
    print(f'FAIL: {error[0]} (sent={sent_total[0]}/{total_size}, recv={recv_total[0]}/{total_size})', file=sys.stderr)
    sys.exit(1)
elif sent_total[0] == total_size and recv_total[0] == total_size:
    sys.exit(0)
else:
    print(f'SHORT: sent={sent_total[0]}/{total_size}, recv={recv_total[0]}/{total_size}', file=sys.stderr)
    sys.exit(1)
" && { echo "  ${GREEN}✅ $label${NC} — 双向同时传输 ${total_size}B 逐字节校验通过"; PASS=$((PASS+1)); } || {
        echo "  ${RED}❌ $label${NC}"
        FAIL=$((FAIL+1));
    }

    cleanup
    sleep 1
}

# ─── 内存增长监控测试 ──────────────────────────────────────────────────────
# 在连接抖动前后监控 client 进程 RSS，验证 SMUX 流清理不导致内存泄漏
# 参数: name  server_bin  server_args  client_bin  client_args  num_cycles  payload_size  rss_threshold_mb
try_memory_monitor() {
    local name="$1"
    local server_bin="$2" server_args="$3"
    local client_bin="$4" client_args="$5"
    local num_cycles="${6:-200}"
    local payload_size="${7:-2048}"
    local rss_threshold="${8:-100}"

    local E=$PORT;       local S=$((PORT+1));  local L=$((PORT+2))
    PORT=$((PORT+7))

    local label="$name (抖动${num_cycles}次, RSS阈值${rss_threshold}MB)"

    if ! start_echo $E; then
        FAIL=$((FAIL+1)); return
    fi

    $server_bin -l "0.0.0.0:$S" -t "127.0.0.1:$E" --key "$KEY" $server_args 2>/dev/null &
    SERVER_PID=$!
    sleep 2
    if ! kill -0 $SERVER_PID 2>/dev/null; then
        echo "  ${RED}❌ $label (server died)${NC}"
        cleanup; FAIL=$((FAIL+1)); return
    fi

    $client_bin -l "127.0.0.1:$L" -r "127.0.0.1:$S" --key "$KEY" $client_args 2>/dev/null &
    CLIENT_PID=$!
    sleep 3
    if ! kill -0 $CLIENT_PID 2>/dev/null; then
        echo "  ${RED}❌ $label (client died)${NC}"
        cleanup; FAIL=$((FAIL+1)); return
    fi

    # 记录初始 RSS (KB)
    local rss_before
    rss_before=$(ps -o rss= -p $CLIENT_PID 2>/dev/null | tr -d ' ' || echo 0)
    rss_before=$((rss_before / 1024))  # 转 MB

    # 执行抖动
    python3 -c "
import socket, sys, time
ok = 0; fail = 0
for i in range($num_cycles):
    try:
        s = socket.socket()
        s.settimeout(10)
        s.connect(('127.0.0.1', $L))
        data = bytes([(i + j) % 256 for j in range($payload_size)])
        s.sendall(data)
        recv_buf = b''
        while len(recv_buf) < len(data):
            chunk = s.recv(65536)
            if not chunk: break
            recv_buf += chunk
        if recv_buf == data:
            ok += 1
        else:
            fail += 1
        s.close()
        time.sleep(0.02)
    except Exception:
        fail += 1
print(f'churn: {ok}/$num_cycles ok, {fail} fail', file=sys.stderr)
sys.exit(0 if fail == 0 else 1)
" 2>/dev/null

    # 记录结束 RSS (KB)
    sleep 2  # 等 GC/allocator 归还
    local rss_after
    rss_after=$(ps -o rss= -p $CLIENT_PID 2>/dev/null | tr -d ' ' || echo 0)
    rss_after=$((rss_after / 1024))  # 转 MB

    local rss_diff=$((rss_after - rss_before))

    if [ "$rss_diff" -gt "$rss_threshold" ]; then
        echo "  ${RED}❌ $label${NC} — RSS: ${rss_before}MB → ${rss_after}MB (增长 ${rss_diff}MB > 阈值 ${rss_threshold}MB)"
        FAIL=$((FAIL+1))
    else
        echo "  ${GREEN}✅ $label${NC} — RSS: ${rss_before}MB → ${rss_after}MB (增长 ${rss_diff}MB ≤ 阈值 ${rss_threshold}MB)"
        PASS=$((PASS+1))
    fi

    cleanup
    sleep 1
}

# ─── Wave 波次并发测试 ─────────────────────────────────────────────────────
# 模拟浏览器加载：3 波连接（HTML→CSS/JS→图片），每波不同大小
# 参数: name  server_bin  server_args  client_bin  client_args
try_wave_concurrency() {
    local name="$1"
    local server_bin="$2" server_args="$3"
    local client_bin="$4" client_args="$5"

    local E=$PORT;       local S=$((PORT+1));  local L=$((PORT+2))
    PORT=$((PORT+7))

    local label="$name (3波 80连接)"

    if ! start_echo $E; then
        FAIL=$((FAIL+1)); return
    fi

    $server_bin -l "0.0.0.0:$S" -t "127.0.0.1:$E" --key "$KEY" $server_args 2>/dev/null &
    SERVER_PID=$!
    sleep 2
    if ! kill -0 $SERVER_PID 2>/dev/null; then
        echo "  ${RED}❌ $label (server died)${NC}"
        cleanup; FAIL=$((FAIL+1)); return
    fi

    $client_bin -l "127.0.0.1:$L" -r "127.0.0.1:$S" --key "$KEY" $client_args 2>/dev/null &
    CLIENT_PID=$!
    sleep 3
    if ! kill -0 $CLIENT_PID 2>/dev/null; then
        echo "  ${RED}❌ $label (client died)${NC}"
        cleanup; FAIL=$((FAIL+1)); return
    fi

    # Wave 1: 10 连接 × 8KB (HTML/CSS)
    # Wave 2: 20 连接 × 32KB (JS bundles)
    # Wave 3: 50 连接 × 混合 4KB~128KB (images, API)
    python3 -c "
import socket, threading, sys, time

results = [False] * 80
errors = [''] * 80

def make_payload(conn_id, size):
    return bytes([((conn_id * 31 + i) % 256) ^ 0xA5 for i in range(size)])

def worker(idx, size, label):
    try:
        s = socket.socket()
        s.settimeout(120)
        s.connect(('127.0.0.1', $L))
        data = make_payload(idx, size)
        s.sendall(data)
        recv_buf = b''
        while len(recv_buf) < len(data):
            chunk = s.recv(65536)
            if not chunk: break
            recv_buf += chunk
        if recv_buf == data:
            results[idx] = True
        else:
            errors[idx] = f'{label}: len sent={len(data)} recv={len(recv_buf)}'
        s.close()
    except Exception as e:
        errors[idx] = f'{label}: {e}'

threads = []

# Wave 1: 10 × 8KB
for i in range(10):
    t = threading.Thread(target=worker, args=(i, 8192, '8KB'))
    threads.append(t); t.start()
time.sleep(0.2)

# Wave 2: 20 × 32KB
for i in range(10, 30):
    t = threading.Thread(target=worker, args=(i, 32768, '32KB'))
    threads.append(t); t.start()
time.sleep(0.3)

# Wave 3: 50 × mixed sizes
for i in range(30, 80):
    size = [4096, 16384, 65536, 131072, 512][i % 5]
    t = threading.Thread(target=worker, args=(i, size, f'{size}B'))
    threads.append(t); t.start()

for t in threads:
    t.join(timeout=180)

ok = sum(1 for r in results if r)
failed = 80 - ok
if failed > 0:
    details = [f'conn{i}: {errors[i]}' for i in range(80) if not results[i]][:5]
    print(f'FAIL: {failed}/80 failed: {\"; \".join(details)}', file=sys.stderr)
    sys.exit(1)
else:
    sys.exit(0)
" && { echo "  ${GREEN}✅ $label${NC} — 3 波 80 连接逐字节校验通过"; PASS=$((PASS+1)); } || {
        echo "  ${RED}❌ $label${NC}"
        FAIL=$((FAIL+1));
    }

    cleanup
    sleep 1
}

# ─── 连接半关闭 (FIN) 测试 ─────────────────────────────────────────────────
# 验证半关闭：send → shutdown(WRITE) → recv echo → close
# 参数: name  server_bin  server_args  client_bin  client_args  payload_size  timeout
try_half_close() {
    local name="$1"
    local server_bin="$2" server_args="$3"
    local client_bin="$4" client_args="$5"
    local payload_size="${6:-65536}"
    local timeout_secs="${7:-30}"

    local E=$PORT;       local S=$((PORT+1));  local L=$((PORT+2))
    PORT=$((PORT+7))

    local label="$name (半关闭 ${payload_size}B)"

    if ! start_echo $E; then
        FAIL=$((FAIL+1)); return
    fi

    $server_bin -l "0.0.0.0:$S" -t "127.0.0.1:$E" --key "$KEY" $server_args 2>/dev/null &
    SERVER_PID=$!
    sleep 2
    if ! kill -0 $SERVER_PID 2>/dev/null; then
        echo "  ${RED}❌ $label (server died)${NC}"
        cleanup; FAIL=$((FAIL+1)); return
    fi

    $client_bin -l "127.0.0.1:$L" -r "127.0.0.1:$S" --key "$KEY" $client_args 2>/dev/null &
    CLIENT_PID=$!
    sleep 3
    if ! kill -0 $CLIENT_PID 2>/dev/null; then
        echo "  ${RED}❌ $label (client died)${NC}"
        cleanup; FAIL=$((FAIL+1)); return
    fi

    python3 -c "
import socket, sys, time
s = socket.socket()
s.settimeout($timeout_secs)
try:
    s.connect(('127.0.0.1', $L))
    data = bytes([(i % 256) ^ 0xA5 for i in range($payload_size)])
    s.sendall(data)
    # 半关闭：通知 echo server 数据已发完
    s.shutdown(socket.SHUT_WR)
    # 接收全部 echo
    recv_buf = b''
    while len(recv_buf) < len(data):
        chunk = s.recv(65536)
        if not chunk: break
        recv_buf += chunk
    if recv_buf == data:
        sys.exit(0)
    else:
        print(f'MISMATCH: sent={len(data)} recv={len(recv_buf)}', file=sys.stderr)
        for i in range(min(len(data), len(recv_buf))):
            if data[i] != recv_buf[i]:
                print(f'  first diff at byte {i}: sent {data[i]:02x} got {recv_buf[i]:02x}', file=sys.stderr)
                break
        sys.exit(1)
except Exception as e:
    print(f'ERROR: {e}', file=sys.stderr)
    sys.exit(2)
finally:
    try: s.close()
    except: pass
" && { echo "  ${GREEN}✅ $label${NC}"; PASS=$((PASS+1)); } || {
        echo "  ${RED}❌ $label${NC}"
        FAIL=$((FAIL+1));
    }

    cleanup
    sleep 1
}

# ─── 不可压缩随机数据 + Snappy 压缩测试 ────────────────────────────────────
# 随机数据压缩后更大，验证 Snappy 缓冲估算不溢出
# 参数: name  server_bin  server_args  client_bin  client_args  payload_size  timeout
try_incompressible() {
    local name="$1"
    local server_bin="$2" server_args="$3"
    local client_bin="$4" client_args="$5"
    local payload_size="${6:-65536}"
    local timeout_secs="${7:-30}"

    local E=$PORT;       local S=$((PORT+1));  local L=$((PORT+2))
    PORT=$((PORT+7))

    local label="$name (随机数据+压缩 ${payload_size}B)"

    if ! start_echo $E; then
        FAIL=$((FAIL+1)); return
    fi

    $server_bin -l "0.0.0.0:$S" -t "127.0.0.1:$E" --key "$KEY" $server_args 2>/dev/null &
    SERVER_PID=$!
    sleep 2
    if ! kill -0 $SERVER_PID 2>/dev/null; then
        echo "  ${RED}❌ $label (server died)${NC}"
        cleanup; FAIL=$((FAIL+1)); return
    fi

    $client_bin -l "127.0.0.1:$L" -r "127.0.0.1:$S" --key "$KEY" $client_args 2>/dev/null &
    CLIENT_PID=$!
    sleep 3
    if ! kill -0 $CLIENT_PID 2>/dev/null; then
        echo "  ${RED}❌ $label (client died)${NC}"
        cleanup; FAIL=$((FAIL+1)); return
    fi

    # 用 os.urandom 生成不可压缩随机数据
    python3 -c "
import socket, select, sys, os
s = socket.socket()
s.settimeout($timeout_secs)
try:
    s.connect(('127.0.0.1', $L))
    data = os.urandom($payload_size)
    s.sendall(data)
    recv_buf = b''
    while len(recv_buf) < len(data):
        r, _, _ = select.select([s], [], [], $timeout_secs)
        if not r:
            print(f'TIMEOUT: got {len(recv_buf)}/{len(data)}', file=sys.stderr)
            sys.exit(2)
        chunk = s.recv(65536)
        if not chunk: break
        recv_buf += chunk
    if recv_buf == data:
        sys.exit(0)
    else:
        print(f'MISMATCH: sent={len(data)} recv={len(recv_buf)}', file=sys.stderr)
        for i in range(min(len(data), len(recv_buf))):
            if data[i] != recv_buf[i]:
                print(f'  first diff at byte {i}: sent {data[i]:02x} got {recv_buf[i]:02x}', file=sys.stderr)
                break
        sys.exit(1)
except Exception as e:
    print(f'ERROR: {e}', file=sys.stderr)
    sys.exit(3)
finally:
    try: s.close()
    except: pass
" && { echo "  ${GREEN}✅ $label${NC}"; PASS=$((PASS+1)); } || {
        echo "  ${RED}❌ $label${NC}"
        FAIL=$((FAIL+1));
    }

    cleanup
    sleep 1
}

# 跳过测试
skip_test() {
    local name="$1" reason="$2"
    echo "  ${YELLOW}⏭️  $name (跳过: $reason)${NC}"
    SKIP=$((SKIP+1))
}

# ─── 测试套件 ───────────────────────────────────────────────────────────────

run_suite() {
    local group_name="$1"
    local server_bin="$2"
    local client_bin="$3"
    local common_args="--sndwnd 256 --rcvwnd 256"

    echo ""
    echo "${CYAN}═══════════════════════════════════════════════════════"
    echo "  $group_name"
    echo "  Server: $server_bin"
    echo "  Client: $client_bin"
    echo "═══════════════════════════════════════════════════════${NC}"
    echo ""

    # ═══════════════════════════════════════════════════════════════════════
    # Section 1: 基础连通性 + 数据完整性（多 payload 大小）
    # ═══════════════════════════════════════════════════════════════════════
    echo "${CYAN}── Section 1: 基础连通性 + 数据完整性 ──${NC}"
    echo ""

    for size in 1 64 1024 8192 65536 524288; do
        local label
        case $size in
            1) label="1B" ;;
            64) label="64B" ;;
            1024) label="1KB" ;;
            8192) label="8KB" ;;
            65536) label="64KB" ;;
            524288) label="512KB" ;;
        esac
        try_test "基础-$label" "$server_bin" "--crypt null --nocomp --datashard 0 --parityshard 0 $common_args" \
                  "$client_bin" "--crypt null --nocomp --datashard 0 --parityshard 0 $common_args" \
                  "$size" 60
    done

    # ═══════════════════════════════════════════════════════════════════════
    # Section 2: 全加密算法矩阵（nocomp，256B 验证）
    # ═══════════════════════════════════════════════════════════════════════
    echo ""
    echo "${CYAN}── Section 2: 全加密算法 (--nocomp, 256B) ──${NC}"
    echo ""

    CRYPTS="null none xor aes-128 aes-192 aes sm4 tea xtea salsa20 blowfish twofish cast5 3des aes-128-gcm"

    for crypt in $CRYPTS; do
        try_test "crypt=$crypt" \
            "$server_bin" "--crypt $crypt --nocomp --datashard 0 --parityshard 0 $common_args" \
            "$client_bin" "--crypt $crypt --nocomp --datashard 0 --parityshard 0 $common_args" \
            256 30
    done

    # ═══════════════════════════════════════════════════════════════════════
    # Section 3: KCP 模式兼容性（crypt=aes, nocomp）
    # ═══════════════════════════════════════════════════════════════════════
    echo ""
    echo "${CYAN}── Section 3: KCP 模式 (crypt=aes, --nocomp) ──${NC}"
    echo ""

    MODES="normal fast fast2 fast3"

    for mode in $MODES; do
        try_test "mode=$mode" \
            "$server_bin" "--crypt aes --mode $mode --nocomp --datashard 0 --parityshard 0 $common_args" \
            "$client_bin" "--crypt aes --mode $mode --nocomp --datashard 0 --parityshard 0 $common_args" \
            4096 30
    done

    # ═══════════════════════════════════════════════════════════════════════
    # Section 4: SMUX 版本（crypt=aes, nocomp）
    # ═══════════════════════════════════════════════════════════════════════
    echo ""
    echo "${CYAN}── Section 4: SMUX 版本 (crypt=aes, --nocomp) ──${NC}"
    echo ""

    for smuxver in 1 2; do
        try_test "smuxver=$smuxver" \
            "$server_bin" "--crypt aes --smuxver $smuxver --nocomp --datashard 0 --parityshard 0 $common_args" \
            "$client_bin" "--crypt aes --smuxver $smuxver --nocomp --datashard 0 --parityshard 0 $common_args" \
            4096 30
    done

    # ═══════════════════════════════════════════════════════════════════════
    # Section 5: 压缩（Snappy on/off，多 cipher + 多大小）
    # ═══════════════════════════════════════════════════════════════════════
    echo ""
    echo "${CYAN}── Section 5: Snappy 压缩 ──${NC}"
    echo ""

    try_test "压缩开-aes-4KB" \
        "$server_bin" "--crypt aes --datashard 0 --parityshard 0 $common_args" \
        "$client_bin" "--crypt aes --datashard 0 --parityshard 0 $common_args" \
        4096 30

    try_test "压缩-全零-64KB" \
        "$server_bin" "--crypt aes --datashard 0 --parityshard 0 $common_args" \
        "$client_bin" "--crypt aes --datashard 0 --parityshard 0 $common_args" \
        65536 30 "全零payload"

    for crypt in null aes-128 sm4 blowfish 3des aes-128-gcm; do
        try_test "压缩+$crypt" \
            "$server_bin" "--crypt $crypt --datashard 0 --parityshard 0 $common_args" \
            "$client_bin" "--crypt $crypt --datashard 0 --parityshard 0 $common_args" \
            8192 30
    done

    # ═══════════════════════════════════════════════════════════════════════
    # Section 6: FEC 前向纠错
    # ═══════════════════════════════════════════════════════════════════════
    echo ""
    echo "${CYAN}── Section 6: FEC 前向纠错 (crypt=aes, --nocomp) ──${NC}"
    echo ""

    try_test "FEC-10/3" \
        "$server_bin" "--crypt aes --nocomp $common_args" \
        "$client_bin" "--crypt aes --nocomp $common_args" \
        4096 30

    try_test "FEC-4/2" \
        "$server_bin" "--crypt aes --nocomp --datashard 4 --parityshard 2 $common_args" \
        "$client_bin" "--crypt aes --nocomp --datashard 4 --parityshard 2 $common_args" \
        4096 30

    try_test "FEC-15/5" \
        "$server_bin" "--crypt aes --nocomp --datashard 15 --parityshard 5 $common_args" \
        "$client_bin" "--crypt aes --nocomp --datashard 15 --parityshard 5 $common_args" \
        4096 30

    # ═══════════════════════════════════════════════════════════════════════
    # Section 7: 窗口大小 + 多 conn
    # ═══════════════════════════════════════════════════════════════════════
    echo ""
    echo "${CYAN}── Section 7: 窗口大小 + 多 conn ──${NC}"
    echo ""

    try_test "小窗口-32/32" \
        "$server_bin" "--crypt aes --nocomp --datashard 0 --parityshard 0 --sndwnd 32 --rcvwnd 32" \
        "$client_bin" "--crypt aes --nocomp --datashard 0 --parityshard 0 --sndwnd 32 --rcvwnd 32" \
        4096 30

    try_test "大窗口-1024/1024" \
        "$server_bin" "--crypt aes --nocomp --datashard 0 --parityshard 0 --sndwnd 1024 --rcvwnd 1024" \
        "$client_bin" "--crypt aes --nocomp --datashard 0 --parityshard 0 --sndwnd 1024 --rcvwnd 1024" \
        65536 30

    try_test "conn=4" \
        "$server_bin" "--crypt aes --nocomp --datashard 0 --parityshard 0 $common_args" \
        "$client_bin" "--crypt aes --nocomp --datashard 0 --parityshard 0 --conn 4 $common_args" \
        4096 30

    # ═══════════════════════════════════════════════════════════════════════
    # Section 8: 并发稳定性（数据完整性聚焦）
    # ═══════════════════════════════════════════════════════════════════════
    echo ""
    echo "${CYAN}── Section 8: 并发稳定性 ──${NC}"
    echo ""

    try_concurrency "并发10×4KB" \
        "$server_bin" "--crypt null --nocomp --datashard 0 --parityshard 0 --sndwnd 256 --rcvwnd 256" \
        "$client_bin" "--crypt null --nocomp --datashard 0 --parityshard 0 --sndwnd 256 --rcvwnd 256" \
        10 4096 60

    try_concurrency "并发50×1KB" \
        "$server_bin" "--crypt aes --nocomp --datashard 0 --parityshard 0 --sndwnd 256 --rcvwnd 256" \
        "$client_bin" "--crypt aes --nocomp --datashard 0 --parityshard 0 --sndwnd 256 --rcvwnd 256" \
        50 1024 90

    try_concurrency "并发30×64KB" \
        "$server_bin" "--crypt null --nocomp --datashard 0 --parityshard 0 --sndwnd 512 --rcvwnd 512" \
        "$client_bin" "--crypt null --nocomp --datashard 0 --parityshard 0 --sndwnd 512 --rcvwnd 512" \
        30 65536 120

    try_concurrency "并发20×128KB" \
        "$server_bin" "--crypt null --nocomp --datashard 0 --parityshard 0 --sndwnd 512 --rcvwnd 512" \
        "$client_bin" "--crypt null --nocomp --datashard 0 --parityshard 0 --conn 1 --sndwnd 512 --rcvwnd 512" \
        20 131072 120

    # ═══════════════════════════════════════════════════════════════════════
    # Section 9: 连接抖动（SMUX 流清理 + 内存稳定性）
    # ═══════════════════════════════════════════════════════════════════════
    echo ""
    echo "${CYAN}── Section 9: 连接抖动 (SMUX 流清理) ──${NC}"
    echo ""

    try_churn "抖动50×1KB" \
        "$server_bin" "--crypt null --nocomp --datashard 0 --parityshard 0 --sndwnd 128 --rcvwnd 128" \
        "$client_bin" "--crypt null --nocomp --datashard 0 --parityshard 0 --sndwnd 128 --rcvwnd 128" \
        50 1024

    try_churn "抖动100×256B" \
        "$server_bin" "--crypt aes --nocomp --datashard 0 --parityshard 0 --sndwnd 128 --rcvwnd 128" \
        "$client_bin" "--crypt aes --nocomp --datashard 0 --parityshard 0 --sndwnd 128 --rcvwnd 128" \
        100 256

    try_churn "抖动100×2KB-压缩" \
        "$server_bin" "--crypt aes --datashard 0 --parityshard 0 --sndwnd 128 --rcvwnd 128" \
        "$client_bin" "--crypt aes --datashard 0 --parityshard 0 --sndwnd 128 --rcvwnd 128" \
        100 2048

    # ═══════════════════════════════════════════════════════════════════════
    # Section 10: 保活 + 空闲超时
    # ═══════════════════════════════════════════════════════════════════════
    echo ""
    echo "${CYAN}── Section 10: 保活 + 空闲超时 ──${NC}"
    echo ""

    try_keepalive "保活30s" \
        "$server_bin" "--crypt null --nocomp --datashard 0 --parityshard 0 --keepalive 10 --sndwnd 128 --rcvwnd 128" \
        "$client_bin" "--crypt null --nocomp --datashard 0 --parityshard 0 --keepalive 10 --sndwnd 128 --rcvwnd 128" \
        30 5

    try_keepalive "保活60s-aes-压缩" \
        "$server_bin" "--crypt aes --datashard 0 --parityshard 0 --keepalive 10 --sndwnd 256 --rcvwnd 256" \
        "$client_bin" "--crypt aes --datashard 0 --parityshard 0 --keepalive 10 --sndwnd 256 --rcvwnd 256" \
        60 10

    # ═══════════════════════════════════════════════════════════════════════
    # Section 11: 组合极限（加密+压缩+FEC+模式）
    # ═══════════════════════════════════════════════════════════════════════
    echo ""
    echo "${CYAN}── Section 11: 组合极限 ──${NC}"
    echo ""

    try_test "极限-aes-fast3-压缩-FEC" \
        "$server_bin" "--crypt aes --mode fast3 $common_args" \
        "$client_bin" "--crypt aes --mode fast3 $common_args" \
        16384 30

    try_test "极限-aes128-fast2-压缩-FEC4/2" \
        "$server_bin" "--crypt aes-128 --mode fast2 --datashard 4 --parityshard 2 $common_args" \
        "$client_bin" "--crypt aes-128 --mode fast2 --datashard 4 --parityshard 2 $common_args" \
        32768 30

    try_test "极限-sm4-normal-压缩-FEC10/3" \
        "$server_bin" "--crypt sm4 --mode normal $common_args" \
        "$client_bin" "--crypt sm4 --mode normal $common_args" \
        16384 30

    try_test "极限-salsa20-fast3-nocomp-大窗口" \
        "$server_bin" "--crypt salsa20 --mode fast3 --nocomp --datashard 0 --parityshard 0 --sndwnd 1024 --rcvwnd 1024" \
        "$client_bin" "--crypt salsa20 --mode fast3 --nocomp --datashard 0 --parityshard 0 --sndwnd 1024 --rcvwnd 1024" \
        131072 60

    # ═══════════════════════════════════════════════════════════════════════
    # Section 12: SMUX v1 + 压缩（旧版协议稳定性）
    # ═══════════════════════════════════════════════════════════════════════
    echo ""
    echo "${CYAN}── Section 12: SMUX v1 + 压缩 ──${NC}"
    echo ""

    try_test "v1+压缩-aes-16KB" \
        "$server_bin" "--crypt aes --smuxver 1 --datashard 0 --parityshard 0 $common_args" \
        "$client_bin" "--crypt aes --smuxver 1 --datashard 0 --parityshard 0 $common_args" \
        16384 30

    try_test "v1+nocomp-sm4-4KB" \
        "$server_bin" "--crypt sm4 --smuxver 1 --nocomp --datashard 0 --parityshard 0 $common_args" \
        "$client_bin" "--crypt sm4 --smuxver 1 --nocomp --datashard 0 --parityshard 0 $common_args" \
        4096 30

    # ═══════════════════════════════════════════════════════════════════════
    # Section 13: 连接抖动 + 并发混合（真实代理场景模拟）
    # ═══════════════════════════════════════════════════════════════════════
    echo ""
    echo "${CYAN}── Section 13: 代理场景模拟 ──${NC}"
    echo ""

    # 代理场景：多并发 + 大数据 + 单 KCP 通道
    try_concurrency "代理-50并发×32KB" \
        "$server_bin" "--crypt null --nocomp --datashard 0 --parityshard 0 --sndwnd 512 --rcvwnd 512" \
        "$client_bin" "--crypt null --nocomp --datashard 0 --parityshard 0 --conn 1 --sndwnd 512 --rcvwnd 512" \
        50 32768 120

    # 代理场景：抖动 + 加密 + 压缩（最接近生产）
    try_churn "代理-抖动80×4KB-aes-压缩" \
        "$server_bin" "--crypt aes --datashard 0 --parityshard 0 --sndwnd 256 --rcvwnd 256" \
        "$client_bin" "--crypt aes --datashard 0 --parityshard 0 --conn 1 --sndwnd 256 --rcvwnd 256" \
        80 4096

    # ═══════════════════════════════════════════════════════════════════════
    # Section 14: 多线程数据完整性（核心稳定性验证）
    # ═══════════════════════════════════════════════════════════════════════
    # 这是最重要的稳定性测试：
    #   - 多线程同时通过同一隧道（单 KCP 通道，多 SMUX 流）
    #   - 每线程发 4 个不同大小 payload (1B → 4KB → 64KB → 128KB)
    #   - 防流混淆数据模式：byte[i] = ((conn_id * 31 + round * 17 + i) % 256) ^ 0xA5
    #   - 逐字节校验，首个不匹配位置 + hex dump 输出
    #   — 如果 stream A 的数据泄漏到 stream B，模式不匹配 → 立即检出
    # ═══════════════════════════════════════════════════════════════════════
    echo ""
    echo "${CYAN}── Section 14: 多线程数据完整性（核心） ──${NC}"
    echo ""

    # 20 线程 × 4 尺寸 (1B/4KB/64KB/128KB)，null cipher
    try_multithread_integrity "多线程完整性-20线程-null" \
        "$server_bin" "--crypt null --nocomp --datashard 0 --parityshard 0 --sndwnd 512 --rcvwnd 512" \
        "$client_bin" "--crypt null --nocomp --datashard 0 --parityshard 0 --conn 1 --sndwnd 512 --rcvwnd 512" \
        20 120

    # 20 线程 × 4 尺寸，aes + 压缩
    try_multithread_integrity "多线程完整性-20线程-aes-压缩" \
        "$server_bin" "--crypt aes --datashard 0 --parityshard 0 --sndwnd 512 --rcvwnd 512" \
        "$client_bin" "--crypt aes --datashard 0 --parityshard 0 --conn 1 --sndwnd 512 --rcvwnd 512" \
        20 120

    # 50 线程 × 4 尺寸，null + nocomp（高并发压力）
    try_multithread_integrity "多线程完整性-50线程-null" \
        "$server_bin" "--crypt null --nocomp --datashard 0 --parityshard 0 --sndwnd 512 --rcvwnd 512" \
        "$client_bin" "--crypt null --nocomp --datashard 0 --parityshard 0 --conn 1 --sndwnd 512 --rcvwnd 512" \
        50 180

    # 20 线程 × 4 尺寸，aes + FEC 10/3（加密+FEC+压缩组合）
    try_multithread_integrity "多线程完整性-20线程-aes-FEC" \
        "$server_bin" "--crypt aes --sndwnd 512 --rcvwnd 512" \
        "$client_bin" "--crypt aes --conn 1 --sndwnd 512 --rcvwnd 512" \
        20 120

    # 10 线程 × 4 尺寸，sm4 + 压缩（国密算法压力）
    try_multithread_integrity "多线程完整性-10线程-sm4-压缩" \
        "$server_bin" "--crypt sm4 --datashard 0 --parityshard 0 --sndwnd 512 --rcvwnd 512" \
        "$client_bin" "--crypt sm4 --datashard 0 --parityshard 0 --conn 1 --sndwnd 512 --rcvwnd 512" \
        10 120

    # ═══════════════════════════════════════════════════════════════════════
    # Section 15: 单连接多轮数据完整性
    # ═══════════════════════════════════════════════════════════════════════
    # 同一连接上连续发送 20+ 轮不同大小的数据，逐字节校验
    # 覆盖：KCP 分片重组、窗口管理、flush 循环稳定性
    # ═══════════════════════════════════════════════════════════════════════
    echo ""
    echo "${CYAN}── Section 15: 单连接多轮数据完整性 ──${NC}"
    echo ""

    # 20 轮 × 混合尺寸 (1B~128KB)，null
    try_multiround_integrity "多轮完整性-20轮-null" \
        "$server_bin" "--crypt null --nocomp --datashard 0 --parityshard 0 --sndwnd 256 --rcvwnd 256" \
        "$client_bin" "--crypt null --nocomp --datashard 0 --parityshard 0 --sndwnd 256 --rcvwnd 256" \
        20 60

    # 20 轮 × 混合尺寸，aes + 压缩
    try_multiround_integrity "多轮完整性-20轮-aes-压缩" \
        "$server_bin" "--crypt aes --datashard 0 --parityshard 0 --sndwnd 256 --rcvwnd 256" \
        "$client_bin" "--crypt aes --datashard 0 --parityshard 0 --sndwnd 256 --rcvwnd 256" \
        20 60

    # 30 轮 × 混合尺寸，salsa20 + nocomp + 大窗口
    try_multiround_integrity "多轮完整性-30轮-salsa20-大窗口" \
        "$server_bin" "--crypt salsa20 --nocomp --datashard 0 --parityshard 0 --sndwnd 1024 --rcvwnd 1024" \
        "$client_bin" "--crypt salsa20 --nocomp --datashard 0 --parityshard 0 --sndwnd 1024 --rcvwnd 1024" \
        30 90

    # ═══════════════════════════════════════════════════════════════════════
    # Section 16: QPP (Quantum Permutation Pad) 数据完整性
    # ═══════════════════════════════════════════════════════════════════════
    # --QPP 在 SMUX stream 层加一层置换混淆，验证不破坏数据完整性
    # ═══════════════════════════════════════════════════════════════════════
    echo ""
    echo "${CYAN}── Section 16: QPP 量子置换混淆 ──${NC}"
    echo ""

    try_qpp "QPP-aes-4KB" \
        "$server_bin" "--crypt aes --nocomp --datashard 0 --parityshard 0 $common_args" \
        "$client_bin" "--crypt aes --nocomp --datashard 0 --parityshard 0 $common_args" \
        4096 30

    try_qpp "QPP-null-64KB" \
        "$server_bin" "--crypt null --nocomp --datashard 0 --parityshard 0 $common_args" \
        "$client_bin" "--crypt null --nocomp --datashard 0 --parityshard 0 $common_args" \
        65536 30

    try_qpp "QPP-aes-压缩-16KB" \
        "$server_bin" "--crypt aes --datashard 0 --parityshard 0 $common_args" \
        "$client_bin" "--crypt aes --datashard 0 --parityshard 0 $common_args" \
        16384 30

    # ═══════════════════════════════════════════════════════════════════════
    # Section 17: 大数据传输 (1MB+)
    # ═══════════════════════════════════════════════════════════════════════
    # 跨更多 KCP 段和 flush 周期，验证大窗口下的持续传输
    # ═══════════════════════════════════════════════════════════════════════
    echo ""
    echo "${CYAN}── Section 17: 大数据传输 (1MB+) ──${NC}"
    echo ""

    try_test "大数据-1MB-null" \
        "$server_bin" "--crypt null --nocomp --datashard 0 --parityshard 0 --sndwnd 512 --rcvwnd 512" \
        "$client_bin" "--crypt null --nocomp --datashard 0 --parityshard 0 --sndwnd 512 --rcvwnd 512" \
        1048576 90

    try_test "大数据-2MB-aes" \
        "$server_bin" "--crypt aes --nocomp --datashard 0 --parityshard 0 --sndwnd 512 --rcvwnd 512" \
        "$client_bin" "--crypt aes --nocomp --datashard 0 --parityshard 0 --sndwnd 512 --rcvwnd 512" \
        2097152 120

    try_test "大数据-1MB-aes-压缩-FEC" \
        "$server_bin" "--crypt aes --sndwnd 512 --rcvwnd 512" \
        "$client_bin" "--crypt aes --sndwnd 512 --rcvwnd 512" \
        1048576 90

    # ═══════════════════════════════════════════════════════════════════════
    # Section 18: 全双工双向同时传输
    # ═══════════════════════════════════════════════════════════════════════
    # 两线程在同一连接上同时发送和接收，验证 pipe 不死锁、不数据错乱
    # ═══════════════════════════════════════════════════════════════════════
    echo ""
    echo "${CYAN}── Section 18: 全双工双向同时传输 ──${NC}"
    echo ""

    try_full_duplex "全双工-null-1MB" \
        "$server_bin" "--crypt null --nocomp --datashard 0 --parityshard 0 --sndwnd 512 --rcvwnd 512" \
        "$client_bin" "--crypt null --nocomp --datashard 0 --parityshard 0 --sndwnd 512 --rcvwnd 512" \
        1048576 60

    try_full_duplex "全双工-aes-512KB" \
        "$server_bin" "--crypt aes --nocomp --datashard 0 --parityshard 0 --sndwnd 512 --rcvwnd 512" \
        "$client_bin" "--crypt aes --nocomp --datashard 0 --parityshard 0 --sndwnd 512 --rcvwnd 512" \
        524288 60

    # ═══════════════════════════════════════════════════════════════════════
    # Section 19: --nc 1 无拥塞控制 + 大窗口
    # ═══════════════════════════════════════════════════════════════════════
    # --nc 1 禁用拥塞控制，配合大窗口实现高速传输，生产常用配置
    # ═══════════════════════════════════════════════════════════════════════
    echo ""
    echo "${CYAN}── Section 19: --nc 1 无拥塞控制 ──${NC}"
    echo ""

    try_test "nc1-aes-128KB" \
        "$server_bin" "--crypt aes --nocomp --datashard 0 --parityshard 0 --nc 1 --sndwnd 1024 --rcvwnd 1024" \
        "$client_bin" "--crypt aes --nocomp --datashard 0 --parityshard 0 --nc 1 --sndwnd 1024 --rcvwnd 1024" \
        131072 60

    try_test "nc1-null-512KB" \
        "$server_bin" "--crypt null --nocomp --datashard 0 --parityshard 0 --nc 1 --sndwnd 1024 --rcvwnd 1024" \
        "$client_bin" "--crypt null --nocomp --datashard 0 --parityshard 0 --nc 1 --sndwnd 1024 --rcvwnd 1024" \
        524288 90

    try_test "nc1-aes-压缩-1MB" \
        "$server_bin" "--crypt aes --nc 1 --sndwnd 1024 --rcvwnd 1024" \
        "$client_bin" "--crypt aes --nc 1 --sndwnd 1024 --rcvwnd 1024" \
        1048576 90

    # ═══════════════════════════════════════════════════════════════════════
    # Section 20: 内存增长监控 (SMUX 流泄漏检测)
    # ═══════════════════════════════════════════════════════════════════════
    # 200 次建断连后监控 client RSS，验证 SMUX 流清理不泄漏
    # 对应 bugs/BUGREPORT_PROXY_MEMORY_GROWTH.md 修复验证
    # ═══════════════════════════════════════════════════════════════════════
    echo ""
    echo "${CYAN}── Section 20: 内存增长监控 ──${NC}"
    echo ""

    try_memory_monitor "内存-null-200次" \
        "$server_bin" "--crypt null --nocomp --datashard 0 --parityshard 0 --sndwnd 128 --rcvwnd 128" \
        "$client_bin" "--crypt null --nocomp --datashard 0 --parityshard 0 --conn 1 --sndwnd 128 --rcvwnd 128" \
        200 2048 50

    try_memory_monitor "内存-aes-压缩-200次" \
        "$server_bin" "--crypt aes --datashard 0 --parityshard 0 --sndwnd 128 --rcvwnd 128" \
        "$client_bin" "--crypt aes --datashard 0 --parityshard 0 --conn 1 --sndwnd 128 --rcvwnd 128" \
        200 2048 80

    # ═══════════════════════════════════════════════════════════════════════
    # Section 21: Wave 波次并发 (浏览器加载模拟)
    # ═══════════════════════════════════════════════════════════════════════
    # 3 波 80 连接：10×8KB + 20×32KB + 50×混合(4KB~128KB)
    # 模拟浏览器页面加载：HTML→CSS/JS→图片
    # ═══════════════════════════════════════════════════════════════════════
    echo ""
    echo "${CYAN}── Section 21: Wave 波次并发 (浏览器加载模拟) ──${NC}"
    echo ""

    try_wave_concurrency "Wave-null" \
        "$server_bin" "--crypt null --nocomp --datashard 0 --parityshard 0 --sndwnd 512 --rcvwnd 512" \
        "$client_bin" "--crypt null --nocomp --datashard 0 --parityshard 0 --conn 1 --sndwnd 512 --rcvwnd 512"

    try_wave_concurrency "Wave-aes-压缩" \
        "$server_bin" "--crypt aes --datashard 0 --parityshard 0 --sndwnd 512 --rcvwnd 512" \
        "$client_bin" "--crypt aes --datashard 0 --parityshard 0 --conn 1 --sndwnd 512 --rcvwnd 512"

    # ═══════════════════════════════════════════════════════════════════════
    # Section 22: 连接半关闭 (FIN) + 不可压缩随机数据
    # ═══════════════════════════════════════════════════════════════════════
    # 半关闭: send → shutdown(WRITE) → recv → close
    # 不可压缩: os.urandom + Snappy 压缩，验证缓冲估算不溢出
    # ═══════════════════════════════════════════════════════════════════════
    echo ""
    echo "${CYAN}── Section 22: 半关闭 + 不可压缩数据 ──${NC}"
    echo ""

    try_half_close "半关闭-null-64KB" \
        "$server_bin" "--crypt null --nocomp --datashard 0 --parityshard 0 --sndwnd 2048 --rcvwnd 2048" \
        "$client_bin" "--crypt null --nocomp --datashard 0 --parityshard 0 --sndwnd 2048 --rcvwnd 2048" \
        65536 30

    try_half_close "半关闭-aes-128KB" \
        "$server_bin" "--crypt aes --nocomp --datashard 0 --parityshard 0 --sndwnd 2048 --rcvwnd 2048" \
        "$client_bin" "--crypt aes --nocomp --datashard 0 --parityshard 0 --sndwnd 2048 --rcvwnd 2048" \
        131072 30

    try_incompressible "不可压缩-aes-压缩-64KB" \
        "$server_bin" "--crypt aes --datashard 0 --parityshard 0 $common_args" \
        "$client_bin" "--crypt aes --datashard 0 --parityshard 0 $common_args" \
        65536 30

    try_incompressible "不可压缩-salsa20-压缩-128KB" \
        "$server_bin" "--crypt salsa20 --datashard 0 --parityshard 0 $common_args" \
        "$client_bin" "--crypt salsa20 --datashard 0 --parityshard 0 $common_args" \
        131072 60
}

# ─── 主流程 ─────────────────────────────────────────────────────────────────

# 选择测试组
GROUP="${1:-all}"

echo "${CYAN}"
echo "╔═══════════════════════════════════════════════════════╗"
echo "║       kcptun-rs Rust↔Rust 冒烟测试（稳定性聚焦）       ║"
echo "╚═══════════════════════════════════════════════════════╝"
echo "${NC}"

# 检查二进制
echo "二进制检查："
if [ "$GROUP" = "all" ] || [ "$GROUP" = "tokio" ]; then
    if [ -x "$TOKIO_SERVER" ] && [ -x "$TOKIO_CLIENT" ]; then
        echo "  ${GREEN}✓${NC} tokio: $TOKIO_SERVER + $TOKIO_CLIENT"
    else
        echo "  ${RED}✗${NC} tokio: 缺少二进制（运行: make release）"
        [ "$GROUP" = "tokio" ] && exit 1
        GROUP="smol"
    fi
fi
if [ "$GROUP" = "all" ] || [ "$GROUP" = "smol" ]; then
    if [ -x "$SMOL_SERVER" ] && [ -x "$SMOL_CLIENT" ]; then
        echo "  ${GREEN}✓${NC} smol:  $SMOL_SERVER + $SMOL_CLIENT"
    else
        echo "  ${RED}✗${NC} smol: 缺少二进制（运行: make release-smol）"
        [ "$GROUP" = "smol" ] && exit 1
        if [ "$GROUP" = "all" ]; then
            echo "  ${YELLOW}⚠ 跳过 smol 组${NC}"
            GROUP="tokio"
        fi
    fi
fi
echo ""

# 运行测试组
if [ "$GROUP" = "all" ] || [ "$GROUP" = "tokio" ]; then
    run_suite "Group A: Rust(tokio) ↔ Rust(tokio)" "$TOKIO_SERVER" "$TOKIO_CLIENT"
fi

if [ "$GROUP" = "all" ] || [ "$GROUP" = "smol" ]; then
    run_suite "Group B: Rust(smol) ↔ Rust(smol)" "$SMOL_SERVER" "$SMOL_CLIENT"
fi

# ─── 结果汇总 ─────────────────────────────────────────────────────────────
echo ""
echo "═══════════════════════════════════════════════════════"
if [ $FAIL -eq 0 ]; then
    echo "  ${GREEN}🎉 全部通过: $PASS passed, $FAIL failed, $SKIP skipped${NC}"
else
    echo "  ${RED}💥 有失败: $PASS passed, $FAIL failed, $SKIP skipped${NC}"
fi
echo "═══════════════════════════════════════════════════════"
echo ""

exit $FAIL
