#!/usr/bin/env python3
"""
Rust vs Go kcptun — comprehensive performance benchmark.

Tests all cipher methods × compression on/off, comparing:
  - Throughput (MB/s)
  - Latency (avg per-connection)
  - Total wall-clock time

Implementations:
  Go          tests/kcptun-go/{client,server}
  Rust-tokio  target/release/{kcptun-client,kcptun-server}
  Rust-smol   target/smol-release/release/{kcptun-client,kcptun-server}

Usage:
  python3 bench_rust_vs_go.py [--conn N] [--size S] [--timeout T] [--quick]
  python3 bench_rust_vs_go.py --rust-only   # only Rust-tokio
  python3 bench_rust_vs_go.py --smol-only   # only Rust-smol
  python3 bench_rust_vs_go.py --go-only     # only Go
"""
import socket
import threading
import time
import sys
import os
import subprocess
import argparse
import hashlib
import json

REPO = os.path.dirname(os.path.abspath(__file__))

# Port pool — incremented per test to avoid TIME_WAIT conflicts
_base_port = 40000
def next_ports(n=3):
    global _base_port
    ports = (_base_port, _base_port + 1, _base_port + 2)
    _base_port += 10
    return ports

def log(msg):
    print(f"[{time.strftime('%H:%M:%S')}] {msg}", flush=True)

def start_echo_server(port):
    srv = socket.socket()
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(('0.0.0.0', port))
    srv.listen(256)
    def handle():
        while True:
            try:
                conn, _ = srv.accept()
            except:
                break
            def echo(c):
                try:
                    while True:
                        d = c.recv(65536)
                        if not d: break
                        c.sendall(d)
                except:
                    pass
                finally:
                    c.close()
            threading.Thread(target=echo, args=(conn,), daemon=True).start()
    threading.Thread(target=handle, daemon=True).start()
    return srv

def kill_ports(*ports):
    for p in ports:
        os.system(f"lsof -ti:{p} 2>/dev/null | xargs kill -9 2>/dev/null")

def build_args(binary_path, is_go, role, echo_port, srv_port, cli_port,
               crypt, nocomp, conn, key='bench-key'):
    """Build CLI args for either Go or Rust kcptun binary."""
    common = [
        '--key', key,
        '--crypt', crypt,
        '--mode', 'fast',
        '--datashard', '0',
        '--parityshard', '0',
        '--sndwnd', '2048',
        '--rcvwnd', '2048',
        '--sockbuf', str(4*1024*1024),
    ]
    if nocomp:
        common.append('--nocomp')

    # --conn is client-only
    client_common = common + ['--conn', str(conn)]

    if is_go:
        listen_flag = '--localaddr' if role == 'client' else '--listen'
        target_flag = '--remoteaddr' if role == 'client' else '--target'
        args = common if role == 'server' else client_common
        if role == 'server':
            return [binary_path, listen_flag, f'0.0.0.0:{srv_port}',
                    target_flag, f'127.0.0.1:{echo_port}'] + args
        else:
            return [binary_path, listen_flag, f'127.0.0.1:{cli_port}',
                    target_flag, f'127.0.0.1:{srv_port}'] + args
    else:
        listen_flag = '-l'
        target_flag = '-r' if role == 'client' else '-t'
        args = common if role == 'server' else client_common
        if role == 'server':
            return [binary_path, listen_flag, f'0.0.0.0:{srv_port}',
                    target_flag, f'127.0.0.1:{echo_port}'] + args
        else:
            return [binary_path, listen_flag, f'127.0.0.1:{cli_port}',
                    target_flag, f'127.0.0.1:{srv_port}'] + args

def run_one_connection(conn_id, local_port, payload_size, timeout, results):
    """Send payload_size bytes, receive echo concurrently, verify MD5.

    Uses a receiver thread to drain echo data while the main thread keeps
    sending — full-duplex pipeline matching bench/throughput.py. Without
    this, the TCP receive buffer fills up and sendall() blocks on larger
    payloads.
    """
    payload = os.urandom(payload_size)
    expected_md5 = hashlib.md5(payload).hexdigest()
    try:
        s = socket.socket()
        s.settimeout(timeout)
        s.connect(('127.0.0.1', local_port))
        s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)

        received = bytearray()
        rx_error = [None]

        def receiver():
            try:
                while len(received) < payload_size:
                    data = s.recv(65536)
                    if not data:
                        break
                    received.extend(data)
            except Exception as e:
                rx_error[0] = e

        rx_thread = threading.Thread(target=receiver, daemon=True)
        rx_thread.start()

        # Start timing AFTER connect — measures pure data transfer time
        t0 = time.perf_counter()
        sent = 0
        while sent < len(payload):
            n = s.send(payload[sent:])
            if n == 0:
                raise ConnectionError("send returned 0")
            sent += n
        # Wait for receiver to drain all echo data
        rx_thread.join(timeout=timeout)
        elapsed = time.perf_counter() - t0
        s.close()

        if rx_error[0] is not None:
            results[conn_id] = {'ok': False, 'error': str(rx_error[0]), 'elapsed': elapsed}
            return
        if len(received) != len(payload):
            results[conn_id] = {'ok': False, 'error': f'short: {len(received)}/{len(payload)}', 'elapsed': elapsed}
            return
        if hashlib.md5(bytes(received)).hexdigest() != expected_md5:
            results[conn_id] = {'ok': False, 'error': 'md5 mismatch', 'elapsed': elapsed}
            return
        results[conn_id] = {'ok': True, 'elapsed': elapsed}
    except socket.timeout:
        results[conn_id] = {'ok': False, 'error': 'timeout', 'elapsed': timeout}
    except Exception as e:
        results[conn_id] = {'ok': False, 'error': str(e), 'elapsed': 0}

def run_bench(server_bin, client_bin, is_go, conn, size, timeout, label,
              crypt, nocomp, impl_name):
    """Run a single benchmark configuration."""
    echo_port, srv_port, cli_port = next_ports()
    kill_ports(echo_port, srv_port, cli_port)
    time.sleep(0.5)

    echo = start_echo_server(echo_port)
    time.sleep(0.3)

    srv_args = build_args(server_bin, is_go, 'server', echo_port, srv_port, cli_port,
                          crypt, nocomp, conn)
    log(f"  [{label}] starting server...")
    srv = subprocess.Popen(srv_args, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    time.sleep(1.5)
    if srv.poll() is not None:
        out = srv.stdout.read().decode(errors='replace') if srv.stdout else ""
        log(f"  [{label}] server died:\n{out[:200]}")
        echo.close()
        return None

    cli_args = build_args(client_bin, is_go, 'client', echo_port, srv_port, cli_port,
                          crypt, nocomp, conn)
    log(f"  [{label}] starting client...")
    cli = subprocess.Popen(cli_args, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    time.sleep(2.0)
    if cli.poll() is not None:
        out = cli.stdout.read().decode(errors='replace') if cli.stdout else ""
        log(f"  [{label}] client died:\n{out[:200]}")
        srv.terminate()
        echo.close()
        return None

    # Warmup — prime KCP congestion window, SMUX, and Snappy before timed measurement
    warmup_size = min(size, 262144)  # 256KB warmup
    warmup_results = {}
    warmup_thread = threading.Thread(
        target=run_one_connection,
        args=(-1, cli_port, warmup_size, timeout, warmup_results),
        daemon=True)
    warmup_thread.start()
    warmup_thread.join(timeout=timeout + 10)

    # Run test — start all connections simultaneously for accurate timing
    results = {}
    threads = []
    t_start = time.perf_counter()
    for i in range(conn):
        t = threading.Thread(target=run_one_connection,
                           args=(i, cli_port, size, timeout, results), daemon=True)
        threads.append(t)
    for t in threads:
        t.start()
    for t in threads:
        t.join(timeout=timeout + 30)
    t_total = time.perf_counter() - t_start

    cli.terminate(); srv.terminate()
    try: cli.wait(timeout=5)
    except: cli.kill()
    try: srv.wait(timeout=5)
    except: srv.kill()
    echo.close()
    time.sleep(0.5)

    ok = sum(1 for r in results.values() if r['ok'])
    fail = sum(1 for r in results.values() if not r['ok'])
    elapsed_list = [r['elapsed'] for r in results.values() if r['ok']]
    total_bytes = ok * size
    throughput = total_bytes / t_total if t_total > 0 else 0

    result = {
        'label': label, 'crypt': crypt, 'nocomp': nocomp, 'impl': impl_name,
        'ok': ok, 'fail': fail,
        'total_time': t_total,
        'throughput': throughput,
        'total_bytes': total_bytes,
    }
    if elapsed_list:
        result['latency_avg'] = sum(elapsed_list) / len(elapsed_list)
        result['latency_min'] = min(elapsed_list)
        result['latency_max'] = max(elapsed_list)
    return result

# ─── Test Matrix ──────────────────────────────────────────────────────────

CIPHERS = [
    'null', 'none', 'xor', 'aes-128', 'aes-128-gcm',
    'salsa20', 'blowfish', 'twofish', 'cast5',
    '3des', 'tea', 'xtea', 'sm4',
]

COMP_MODES = [
    ('nocomp', True),   # compression disabled
    ('comp',   False),  # compression enabled
]

def main():
    parser = argparse.ArgumentParser(description='Rust vs Go kcptun comprehensive benchmark')
    parser.add_argument('--conn', type=int, default=10, help='concurrent connections')
    parser.add_argument('--size', type=int, default=1048576, help='payload size in bytes (default: 1M=1048576)')
    parser.add_argument('--timeout', type=int, default=30, help='per-connection timeout (s)')
    parser.add_argument('--quick', action='store_true', help='quick mode: fewer ciphers')
    parser.add_argument('--rust-only', action='store_true', help='only Rust-tokio (skip Go and Rust-smol)')
    parser.add_argument('--smol-only', action='store_true', help='only Rust-smol (skip Go and Rust-tokio)')
    parser.add_argument('--go-only', action='store_true', help='only Go (skip Rust-tokio and Rust-smol)')
    args = parser.parse_args()

    rust_server = os.path.join(REPO, 'target/release/kcptun-server')
    rust_client = os.path.join(REPO, 'target/release/kcptun-client')
    smol_server = os.path.join(REPO, 'target/smol-release/release/kcptun-server')
    smol_client = os.path.join(REPO, 'target/smol-release/release/kcptun-client')
    go_server = os.path.join(REPO, 'tests/kcptun-go/server')
    go_client = os.path.join(REPO, 'tests/kcptun-go/client')

    test_rust = not args.go_only and not args.smol_only
    test_smol = not args.go_only and not args.rust_only
    test_go = not args.rust_only and not args.smol_only

    if test_rust:
        for p, n in [(rust_server, 'Rust-tokio server'), (rust_client, 'Rust-tokio client')]:
            if not os.path.exists(p):
                log(f"ERROR: {n} not found at {p}")
                test_rust = False
    if test_smol:
        for p, n in [(smol_server, 'Rust-smol server'), (smol_client, 'Rust-smol client')]:
            if not os.path.exists(p):
                log(f"ERROR: {n} not found at {p} (build with: make release-smol)")
                test_smol = False
    if test_go:
        for p, n in [(go_server, 'Go server'), (go_client, 'Go client')]:
            if not os.path.exists(p):
                log(f"ERROR: {n} not found at {p}")
                test_go = False

    if not test_rust and not test_smol and not test_go:
        log("No binaries found. Build first: cargo build --release && make release-smol")
        sys.exit(1)

    ciphers = ['null', 'aes-128', 'aes-128-gcm', 'salsa20', 'blowfish', 'sm4', '3des'] if args.quick else CIPHERS

    print()
    print("=" * 80)
    print(f"  kcptun Benchmark: {args.conn} conn × {args.size} bytes ({args.size/1024:.0f}K)")
    print(f"  Ciphers: {len(ciphers)} | Compression: on+off | Implementations: ", end="")
    impls = []
    if test_rust: impls.append("Rust-tokio")
    if test_smol: impls.append("Rust-smol")
    if test_go: impls.append("Go")
    print(" + ".join(impls))
    print("=" * 80)

    all_results = []
    n_impls = (1 if test_rust else 0) + (1 if test_smol else 0) + (1 if test_go else 0)
    total_tests = len(ciphers) * len(COMP_MODES) * n_impls
    test_num = 0

    for crypt in ciphers:
        for comp_name, nocomp in COMP_MODES:
            comp_label = "no-comp" if nocomp else "comp"
            config = f"{crypt}/{comp_label}"

            if test_rust:
                test_num += 1
                log(f"[{test_num}/{total_tests}] Rust-tokio {config}")
                r = run_bench(rust_server, rust_client, False, args.conn, args.size,
                             args.timeout, f"Rust-tokio {config}", crypt, nocomp, 'rust')
                if r:
                    all_results.append(r)
                    tp = r['throughput'] / 1024 / 1024
                    lat = r.get('latency_avg', 0)
                    ok = r['ok']
                    log(f"  → {ok} ok, {tp:.1f} MB/s, {lat:.3f}s avg")
                else:
                    log(f"  → FAILED")

            if test_smol:
                test_num += 1
                log(f"[{test_num}/{total_tests}] Rust-smol {config}")
                r = run_bench(smol_server, smol_client, False, args.conn, args.size,
                             args.timeout, f"Rust-smol {config}", crypt, nocomp, 'smol')
                if r:
                    all_results.append(r)
                    tp = r['throughput'] / 1024 / 1024
                    lat = r.get('latency_avg', 0)
                    ok = r['ok']
                    log(f"  → {ok} ok, {tp:.1f} MB/s, {lat:.3f}s avg")
                else:
                    log(f"  → FAILED")

            if test_go:
                test_num += 1
                log(f"[{test_num}/{total_tests}] Go {config}")
                r = run_bench(go_server, go_client, True, args.conn, args.size,
                             args.timeout, f"Go {config}", crypt, nocomp, 'go')
                if r:
                    all_results.append(r)
                    tp = r['throughput'] / 1024 / 1024
                    lat = r.get('latency_avg', 0)
                    ok = r['ok']
                    log(f"  → {ok} ok, {tp:.1f} MB/s, {lat:.3f}s avg")
                else:
                    log(f"  → FAILED")

    # ─── Summary Table ───
    print()
    print("=" * 100)
    print("  RESULTS SUMMARY")
    print("=" * 100)

    # Group by cipher + comp, show Rust vs Go side by side
    header = f"  {'Config':<28} {'Impl':>10} {'OK':>4} {'Fail':>5} {'Time':>7} {'Throughput':>11} {'Latency(avg)':>13}"
    print(header)
    print(f"  {'-'*28} {'-'*10} {'-'*4} {'-'*5} {'-'*7} {'-'*11} {'-'*13}")

    impl_display = {'rust': 'Rust-tokio', 'smol': 'Rust-smol', 'go': 'Go'}
    for r in all_results:
        tp = r['throughput'] / 1024 / 1024
        lat = r.get('latency_avg', 0)
        config = f"{r['crypt']}/{'no-comp' if r['nocomp'] else 'comp'}"
        impl = impl_display.get(r.get('impl', ''), r['label'])
        ok = r['ok']
        fail = r['fail']
        t = r['total_time']
        print(f"  {config:<28} {impl:>10} {ok:>4} {fail:>5} {t:>6.1f}s {tp:>8.1f} MB/s {lat:>12.3f}s")

    # ─── Speedup Comparison ───
    impls_present = set(r.get('impl', '') for r in all_results)
    if len(impls_present) >= 2:
        print()
        print("=" * 100)
        print("  SPEEDUP COMPARISON (MB/s)")
        print("=" * 100)

        has_rust = 'rust' in impls_present
        has_smol = 'smol' in impls_present
        has_go = 'go' in impls_present

        header = f"  {'Config':<24}"
        if has_rust: header += f" {'Tokio':>10}"
        if has_smol: header += f" {'Smol':>10}"
        if has_go:   header += f" {'Go':>10}"
        if has_rust and has_go: header += f" {'T/Go':>7}"
        if has_smol and has_go: header += f" {'S/Go':>7}"
        if has_rust and has_smol: header += f" {'T/S':>7}"
        print(header)
        sep = f"  {'-'*24}"
        if has_rust: sep += f" {'-'*10}"
        if has_smol: sep += f" {'-'*10}"
        if has_go:   sep += f" {'-'*10}"
        if has_rust and has_go: sep += f" {'-'*7}"
        if has_smol and has_go: sep += f" {'-'*7}"
        if has_rust and has_smol: sep += f" {'-'*7}"
        print(sep)

        def fmt_mb(r):
            if r and r['throughput'] > 0:
                return f"{r['throughput'] / 1024 / 1024:>8.1f}  "
            return f"{'—':>10}"

        def fmt_ratio(a, b):
            if a and b and a['throughput'] > 0 and b['throughput'] > 0:
                ratio = a['throughput'] / b['throughput']
                arrow = "↑" if ratio >= 1 else "↓"
                return f"{ratio:>5.2f}x{arrow}"
            return f"{'—':>7}"

        for crypt in ciphers:
            for comp_name, nocomp in COMP_MODES:
                comp_label = "no-comp" if nocomp else "comp"
                config = f"{crypt}/{comp_label}"
                rust_r = next((r for r in all_results if r['crypt'] == crypt and r['nocomp'] == nocomp and r.get('impl') == 'rust'), None)
                smol_r = next((r for r in all_results if r['crypt'] == crypt and r['nocomp'] == nocomp and r.get('impl') == 'smol'), None)
                go_r = next((r for r in all_results if r['crypt'] == crypt and r['nocomp'] == nocomp and r.get('impl') == 'go'), None)
                line = f"  {config:<24}"
                if has_rust: line += f" {fmt_mb(rust_r)}"
                if has_smol: line += f" {fmt_mb(smol_r)}"
                if has_go:   line += f" {fmt_mb(go_r)}"
                if has_rust and has_go: line += f" {fmt_ratio(rust_r, go_r)}"
                if has_smol and has_go: line += f" {fmt_ratio(smol_r, go_r)}"
                if has_rust and has_smol: line += f" {fmt_ratio(rust_r, smol_r)}"
                print(line)

    # Save results as JSON
    results_file = os.path.join(REPO, 'bench_results.json')
    with open(results_file, 'w') as f:
        json.dump(all_results, f, indent=2)
    print(f"\n  Results saved to: {results_file}")
    print("=" * 100)

if __name__ == '__main__':
    main()
