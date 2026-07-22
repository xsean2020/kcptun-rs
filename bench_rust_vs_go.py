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
_base_port = 20000
def next_ports(n=3):
    global _base_port
    ports = (_base_port, _base_port + 1, _base_port + 2)
    _base_port += 10
    return ports

def log(msg):
    print(f"[{time.strftime('%H:%M:%S')}] {msg}", flush=True)

def _pids_on_port(port):
    """Return PIDs holding TCP-LISTEN or UDP on `port` (macOS/Linux lsof)."""
    pids = set()
    # TCP listeners (echo / kcptun client)
    try:
        out = subprocess.check_output(
            ['lsof', f'-tiTCP:{port}', '-sTCP:LISTEN'],
            stderr=subprocess.DEVNULL, text=True)
        pids.update(int(x) for x in out.split() if x.strip().isdigit())
    except (subprocess.CalledProcessError, FileNotFoundError, ValueError):
        pass
    # UDP (kcptun server KCP/UDP bind)
    try:
        out = subprocess.check_output(
            ['lsof', f'-tiUDP:{port}'],
            stderr=subprocess.DEVNULL, text=True)
        pids.update(int(x) for x in out.split() if x.strip().isdigit())
    except (subprocess.CalledProcessError, FileNotFoundError, ValueError):
        pass
    # Fallback: any protocol on that port number
    try:
        out = subprocess.check_output(
            ['lsof', f'-ti:{port}'],
            stderr=subprocess.DEVNULL, text=True)
        pids.update(int(x) for x in out.split() if x.strip().isdigit())
    except (subprocess.CalledProcessError, FileNotFoundError, ValueError):
        pass
    return pids

def port_in_use(port):
    """True if any process holds TCP-LISTEN or UDP on `port` (lsof-based)."""
    return bool(_pids_on_port(port))

def _set_reuse(sock):
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    # macOS: SO_REUSEPORT helps when SO_REUSEADDR alone cannot reclaim a port
    if hasattr(socket, 'SO_REUSEPORT'):
        try:
            sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEPORT, 1)
        except OSError:
            pass

def can_bind(port, udp=False):
    """True if we can actually bind `port` right now (catches TIME_WAIT etc.)."""
    sock_type = socket.SOCK_DGRAM if udp else socket.SOCK_STREAM
    s = socket.socket(socket.AF_INET, sock_type)
    try:
        _set_reuse(s)
        s.bind(('0.0.0.0', port))
        return True
    except OSError:
        return False
    finally:
        try:
            s.close()
        except OSError:
            pass

def kill_ports(*ports):
    """Force-kill holders of the given ports and wait until they are free."""
    my_pid = os.getpid()
    for p in ports:
        for attempt in range(10):
            pids = _pids_on_port(p)
            # Never kill ourselves (echo server is in this process)
            pids.discard(my_pid)
            if not pids:
                break
            for pid in pids:
                try:
                    os.kill(pid, 9)
                except ProcessLookupError:
                    pass
                except PermissionError:
                    log(f"  warn: no permission to kill pid {pid} on port {p}")
            time.sleep(0.15)
        if port_in_use(p):
            # Still held by another process (or ourselves) — lsof-only check
            remaining = _pids_on_port(p) - {my_pid}
            if remaining:
                log(f"  warn: port {p} still occupied by pids {remaining}")

def wait_port_free(port, timeout=5.0, udp=False):
    """Poll until port is bindable. Returns True if free, False on timeout."""
    deadline = time.perf_counter() + timeout
    while time.perf_counter() < deadline:
        if can_bind(port, udp=udp):
            return True
        time.sleep(0.1)
    return False

def wait_port_ready(port, timeout=5.0):
    """Poll until a TCP connect to 127.0.0.1:port succeeds (listener ready)."""
    deadline = time.perf_counter() + timeout
    while time.perf_counter() < deadline:
        try:
            s = socket.socket()
            s.settimeout(0.2)
            s.connect(('127.0.0.1', port))
            s.close()
            return True
        except OSError:
            time.sleep(0.1)
    return False

def ensure_ports_free(echo_port, srv_port, cli_port, timeout=5.0):
    """Kill holders and wait until the bench port triple is bindable.

    echo/cli are TCP; srv is UDP (KCP). Returns True if all three can bind.
    """
    kill_ports(echo_port, srv_port, cli_port)
    checks = (
        (echo_port, False),
        (srv_port, True),
        (cli_port, False),
    )
    ok = True
    for p, is_udp in checks:
        if not wait_port_free(p, timeout=timeout, udp=is_udp):
            log(f"  ERROR: port {p} ({'udp' if is_udp else 'tcp'}) still not bindable")
            ok = False
    return ok

def allocate_ports(max_tries=40):
    """Pick a free (echo, srv, cli) triple; skip/kill occupied ranges."""
    for _ in range(max_tries):
        ports = next_ports()
        echo_port, srv_port, cli_port = ports
        occupied = [p for p in ports if port_in_use(p) or not can_bind(p, udp=(p == srv_port))]
        if occupied:
            # Best-effort free; if still unusable, advance the pool
            if not ensure_ports_free(echo_port, srv_port, cli_port, timeout=1.5):
                continue
        # Final bindability check (lsof can miss TIME_WAIT)
        if (can_bind(echo_port, udp=False)
                and can_bind(srv_port, udp=True)
                and can_bind(cli_port, udp=False)):
            return ports
    return None

def start_echo_server(port):
    srv = socket.socket()
    _set_reuse(srv)
    try:
        srv.bind(('0.0.0.0', port))
    except OSError as e:
        srv.close()
        raise OSError(e.errno, f"echo bind 0.0.0.0:{port} failed: {e.strerror}") from e
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
    # Allocate a (echo, srv, cli) triple guaranteed bindable right now
    allocated = allocate_ports()
    if allocated is None:
        log(f"  [{label}] cannot allocate free ports after 40 tries; skip")
        return None
    echo_port, srv_port, cli_port = allocated

    try:
        echo = start_echo_server(echo_port)
    except OSError as e:
        log(f"  [{label}] {e}")
        ensure_ports_free(echo_port, srv_port, cli_port, timeout=2.0)
        return None
    if not wait_port_ready(echo_port, timeout=3.0):
        log(f"  [{label}] echo server not ready on {echo_port}")
        echo.close()
        ensure_ports_free(echo_port, srv_port, cli_port, timeout=2.0)
        return None

    srv_args = build_args(server_bin, is_go, 'server', echo_port, srv_port, cli_port,
                          crypt, nocomp, conn)
    log(f"  [{label}] starting server (udp :{srv_port})...")
    srv = subprocess.Popen(srv_args, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    # Wait for process to stay up; UDP bind has no TCP connect probe
    for _ in range(20):
        if srv.poll() is not None:
            break
        time.sleep(0.1)
    if srv.poll() is not None:
        out = srv.stdout.read().decode(errors='replace') if srv.stdout else ""
        log(f"  [{label}] server died:\n{out[:300]}")
        echo.close()
        ensure_ports_free(echo_port, srv_port, cli_port, timeout=2.0)
        return None

    cli_args = build_args(client_bin, is_go, 'client', echo_port, srv_port, cli_port,
                          crypt, nocomp, conn)
    log(f"  [{label}] starting client (tcp :{cli_port})...")
    cli = subprocess.Popen(cli_args, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    # Poll TCP listener readiness instead of fixed sleep (cold-start first test)
    if not wait_port_ready(cli_port, timeout=8.0):
        out = ""
        if cli.poll() is not None and cli.stdout:
            out = cli.stdout.read().decode(errors='replace')
        log(f"  [{label}] client listener not ready on {cli_port}"
            + (f":\n{out[:300]}" if out else " (process still running)"))
        cli.terminate()
        srv.terminate()
        try: cli.wait(timeout=3)
        except: cli.kill()
        try: srv.wait(timeout=3)
        except: srv.kill()
        echo.close()
        ensure_ports_free(echo_port, srv_port, cli_port, timeout=3.0)
        return None
    if cli.poll() is not None:
        out = cli.stdout.read().decode(errors='replace') if cli.stdout else ""
        log(f"  [{label}] client died:\n{out[:300]}")
        srv.terminate()
        echo.close()
        ensure_ports_free(echo_port, srv_port, cli_port, timeout=2.0)
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
    if not warmup_results.get(-1, {}).get('ok'):
        err = warmup_results.get(-1, {}).get('error', 'no result')
        log(f"  [{label}] warmup failed ({err}); continuing to timed run")

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
    # Always free ports after each run so the next test is clean
    ensure_ports_free(echo_port, srv_port, cli_port, timeout=3.0)

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
