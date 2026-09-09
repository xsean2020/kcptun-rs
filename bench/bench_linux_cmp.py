#!/usr/bin/env python3
"""
kcptun-rs vs Go kcptun — comprehensive Linux benchmark.

Tests all 15 ciphers × compression on/off, measuring:
  - Throughput (MB/s, concurrent connections)
  - Latency P50/P90/P99/P999 (microseconds)

Prerequisites on the Linux machine:
  1. Pre-built binaries from build_linux.sh (Go + Rust in this directory)
  2. If Go binaries not pre-built: Go toolchain + --go-src /path/to/kcptun

Usage:
    python3 bench_linux_cmp.py                          # full matrix, defaults
    python3 bench_linux_cmp.py --quick                  # fewer ciphers, 1 run
    python3 bench_linux_cmp.py --rust-only              # Rust only
    python3 bench_linux_cmp.py --go-only                # Go only
    python3 bench_linux_cmp.py --latency-only           # skip throughput
    python3 bench_linux_cmp.py --throughput-only         # skip latency
    python3 bench_linux_cmp.py --conn 4 --size 1048576  # custom params
    python3 bench_linux_cmp.py --crypts aes,null,salsa20 # specific ciphers
"""
import argparse
import hashlib
import json
import os
import signal
import socket
import subprocess
import sys
import threading
import time

# ─── Configuration ────────────────────────────────────────────────────────

ALL_CRYPTS = [
    "null", "none", "xor",
    "aes-128", "aes-192", "aes",
    "sm4", "tea", "xtea",
    "salsa20", "blowfish", "twofish",
    "cast5", "3des", "aes-128-gcm",
]

# Ciphers that are known to be slower (used for quick mode to cover edge cases)
SLOW_CRYPTS = {"3des", "cast5", "blowfish", "twofish"}

QUICK_CRYPTS = ["null", "aes", "salsa20", "3des", "aes-128-gcm"]

KEY = "bench-key"
MODE = "fast"
SNDWND = "2048"
RCVWND = "2048"
SOCKBUF = "8388608"  # 8MB

_base_port = 30000

def next_ports():
    global _base_port
    ports = (_base_port, _base_port + 1, _base_port + 2)
    _base_port += 10
    return ports

def log(msg):
    print(f"[{time.strftime('%H:%M:%S')}] {msg}", flush=True)

# ─── Port / process management ─────────────────────────────────────────────

def _pids_on_port(port):
    pids = set()
    try:
        out = subprocess.check_output(
            ["lsof", f"-ti:{port}"], stderr=subprocess.DEVNULL, universal_newlines=True)
        pids.update(int(x) for x in out.split() if x.strip().isdigit())
    except (subprocess.CalledProcessError, FileNotFoundError, ValueError):
        pass
    return pids

def kill_ports(*ports):
    my_pid = os.getpid()
    for p in ports:
        for _ in range(10):
            pids = _pids_on_port(p)
            pids.discard(my_pid)
            if not pids:
                break
            for pid in pids:
                try:
                    os.kill(pid, signal.SIGKILL)
                except (ProcessLookupError, PermissionError):
                    pass
            time.sleep(0.15)

def can_bind(port, udp=False):
    sock_type = socket.SOCK_DGRAM if udp else socket.SOCK_STREAM
    s = socket.socket(socket.AF_INET, sock_type)
    try:
        s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        if hasattr(socket, "SO_REUSEPORT"):
            try:
                s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEPORT, 1)
            except OSError:
                pass
        s.bind(("0.0.0.0", port))
        return True
    except OSError:
        return False
    finally:
        s.close()

def allocate_ports():
    for _ in range(40):
        ports = next_ports()
        if all(can_bind(p, udp=(p == ports[1])) for p in ports):
            return ports
        kill_ports(*ports)
        time.sleep(0.1)
    return None

def wait_port_ready(port, timeout=8.0):
    deadline = time.perf_counter() + timeout
    while time.perf_counter() < deadline:
        try:
            s = socket.socket()
            s.settimeout(0.2)
            s.connect(("127.0.0.1", port))
            s.close()
            return True
        except OSError:
            time.sleep(0.1)
    return False

# ─── Echo server ───────────────────────────────────────────────────────────

def start_echo_server(port):
    srv = socket.socket()
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("0.0.0.0", port))
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
                        if not d:
                            break
                        c.sendall(d)
                except:
                    pass
                finally:
                    c.close()
            threading.Thread(target=echo, args=(conn,), daemon=True).start()

    threading.Thread(target=handle, daemon=True).start()
    return srv

# ─── Binary discovery ──────────────────────────────────────────────────────

def find_rust_binaries(script_dir):
    """Find Rust kcptun binaries relative to this script.

    Returns a dict: {"tokio": {client,server}}
    Keys are only present if binaries are found.
    """
    result = {}
    for label, subdirs in [("tokio", ["tokio", "x86_64", "aarch64"])]:
        for subdir in subdirs:
            client = os.path.join(script_dir, subdir, "kcptun-client")
            server = os.path.join(script_dir, subdir, "kcptun-server")
            if os.path.isfile(client) and os.access(client, os.X_OK) \
               and os.path.isfile(server) and os.access(server, os.X_OK):
                result[label] = {"client": client, "server": server}
                break
    return result

def find_go_binaries(script_dir):
    """Find pre-built Go kcptun binaries (from build_linux.sh)."""
    for subdir in ["go", "go/amd64", "go/arm64", "."]:
        client = os.path.join(script_dir, subdir, "kcptun-client")
        server = os.path.join(script_dir, subdir, "kcptun-server")
        if os.path.isfile(client) and os.access(client, os.X_OK) \
           and os.path.isfile(server) and os.access(server, os.X_OK):
            return {"client": client, "server": server}
    return None

def build_go_binaries(go_src):
    """Build Go kcptun client and server from source."""
    if not os.path.isdir(go_src):
        log(f"Go source not found at {go_src}")
        return None

    go_bin = os.environ.get("GO_BIN", "go")
    out_dir = os.path.join(go_src, "bin")
    os.makedirs(out_dir, exist_ok=True)
    client = os.path.join(out_dir, "kcptun-client")
    server = os.path.join(out_dir, "kcptun-server")

    need_build = (not os.path.isfile(client)) or (not os.path.isfile(server))
    if need_build:
        log(f"Building Go kcptun from {go_src}...")
        # Go kcptun repo has ./client/ and ./server/ subdirectories
        build_dirs = [
            (server, "./server"),
            (client, "./client"),
        ]
        ok = True
        for out_path, src_dir in build_dirs:
            try:
                subprocess.run(
                    [go_bin, "build", "-o", out_path, src_dir],
                    cwd=go_src, check=True, timeout=120,
                    stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            except (subprocess.CalledProcessError, FileNotFoundError) as e:
                stderr = e.stderr.decode() if hasattr(e, 'stderr') and e.stderr else str(e)
                log(f"  Failed to build {src_dir}: {stderr}")
                ok = False
                break
        if not ok:
            log("Failed to build Go binaries.")
            return None
        log("Go binaries built successfully.")
    else:
        log("Go binaries already built, skipping compilation.")

    return {"client": client, "server": server}

# ─── kcptun process management ─────────────────────────────────────────────

def build_args(binary, is_go, role, echo_port, srv_port, cli_port,
               crypt, nocomp, conn):
    common = [
        "--key", KEY,
        "--crypt", crypt,
        "--mode", MODE,
        "--datashard", "0",
        "--parityshard", "0",
        "--sndwnd", SNDWND,
        "--rcvwnd", RCVWND,
        "--sockbuf", SOCKBUF,
    ]
    if nocomp:
        common.append("--nocomp")

    client_common = common + ["--conn", str(conn)]

    if is_go:
        listen = "--localaddr" if role == "client" else "--listen"
        target = "--remoteaddr" if role == "client" else "--target"
        args = common if role == "server" else client_common
    else:
        listen = "-l"
        target = "-r" if role == "client" else "-t"
        args = common if role == "server" else client_common

    if role == "server":
        return [binary, listen, f"0.0.0.0:{srv_port}",
                target, f"127.0.0.1:{echo_port}"] + args
    else:
        return [binary, listen, f"127.0.0.1:{cli_port}",
                target, f"127.0.0.1:{srv_port}"] + args

def start_tunnel(server_bin, client_bin, is_go, echo_port, srv_port, cli_port,
                 crypt, nocomp, conn, label):
    """Start server+client. Returns (srv_proc, cli_proc) or None."""
    kill_ports(echo_port, srv_port, cli_port)

    srv_args = build_args(server_bin, is_go, "server",
                          echo_port, srv_port, cli_port, crypt, nocomp, conn)
    srv = subprocess.Popen(srv_args, stdout=subprocess.DEVNULL,
                           stderr=subprocess.DEVNULL)
    for _ in range(30):
        if srv.poll() is not None:
            log(f"  [{label}] server died")
            return None
        time.sleep(0.1)

    cli_args = build_args(client_bin, is_go, "client",
                          echo_port, srv_port, cli_port, crypt, nocomp, conn)
    cli = subprocess.Popen(cli_args, stdout=subprocess.DEVNULL,
                           stderr=subprocess.DEVNULL)
    if not wait_port_ready(cli_port, timeout=10):
        log(f"  [{label}] client not ready")
        cli.kill()
        srv.kill()
        return None
    if cli.poll() is not None:
        log(f"  [{label}] client died")
        srv.kill()
        return None

    return srv, cli

def stop_tunnel(srv, cli):
    for p in [cli, srv]:
        if p:
            p.terminate()
    for p in [cli, srv]:
        if p:
            try:
                p.wait(timeout=3)
            except subprocess.TimeoutExpired:
                p.kill()

# ─── Throughput benchmark ──────────────────────────────────────────────────

def run_one_throughput(conn_id, local_port, payload_size, timeout, results):
    payload = os.urandom(payload_size)
    expected_md5 = hashlib.md5(payload).hexdigest()

    try:
        s = socket.socket()
        s.settimeout(timeout)
        s.connect(("127.0.0.1", local_port))
        s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)

        received = bytearray()

        def receiver():
            try:
                while len(received) < payload_size:
                    data = s.recv(65536)
                    if not data:
                        break
                    received.extend(data)
            except Exception:
                pass

        rx = threading.Thread(target=receiver, daemon=True)
        rx.start()

        t0 = time.perf_counter()
        sent = 0
        while sent < len(payload):
            n = s.send(payload[sent:])
            if n == 0:
                raise ConnectionError("send returned 0")
            sent += n
        rx.join(timeout=timeout)
        elapsed = time.perf_counter() - t0
        s.close()

        if len(received) != len(payload):
            results[conn_id] = {"ok": False, "error": f"short: {len(received)}/{len(payload)}"}
            return
        if hashlib.md5(bytes(received)).hexdigest() != expected_md5:
            results[conn_id] = {"ok": False, "error": "md5 mismatch"}
            return
        results[conn_id] = {"ok": True, "elapsed": elapsed}
    except socket.timeout:
        results[conn_id] = {"ok": False, "error": "timeout"}
    except Exception as e:
        results[conn_id] = {"ok": False, "error": str(e)}

def bench_throughput(server_bin, client_bin, is_go, conn, size, timeout,
                     crypt, nocomp, label):
    allocated = allocate_ports()
    if not allocated:
        log(f"  [{label}] cannot allocate ports")
        return None
    echo_port, srv_port, cli_port = allocated

    echo = start_echo_server(echo_port)
    if not wait_port_ready(echo_port, timeout=3):
        log(f"  [{label}] echo not ready")
        echo.close()
        return None

    tunnel = start_tunnel(server_bin, client_bin, is_go,
                           echo_port, srv_port, cli_port,
                           crypt, nocomp, conn, label)
    if not tunnel:
        echo.close()
        return None
    srv, cli = tunnel

    try:
        # Warmup
        warmup_res = {}
        t = threading.Thread(target=run_one_throughput,
                             args=(-1, cli_port, min(size, 262144), timeout, warmup_res),
                             daemon=True)
        t.start()
        t.join(timeout=timeout + 10)

        # Timed run
        results = {}
        threads = []
        t_start = time.perf_counter()
        for i in range(conn):
            t = threading.Thread(target=run_one_throughput,
                                 args=(i, cli_port, size, timeout, results),
                                 daemon=True)
            t.start()
            threads.append(t)
        for t in threads:
            t.join(timeout=timeout + 10)
        wall = time.perf_counter() - t_start

        ok_count = sum(1 for v in results.values() if v.get("ok"))
        if ok_count == 0:
            log(f"  [{label}] all connections failed")
            return None

        total_bytes = ok_count * size
        throughput_mbs = (total_bytes / (1024 * 1024)) / wall
        elapsed_list = [v["elapsed"] for v in results.values() if v.get("ok")]
        avg_elapsed = sum(elapsed_list) / len(elapsed_list)

        return {
            "throughput_mbs": throughput_mbs,
            "total_mb": total_bytes / (1024 * 1024),
            "wall_s": wall,
            "ok": ok_count,
            "fail": conn - ok_count,
            "avg_conn_s": avg_elapsed,
        }
    finally:
        stop_tunnel(srv, cli)
        echo.close()
        kill_ports(echo_port, srv_port, cli_port)
        time.sleep(0.5)

# ─── Latency benchmark ─────────────────────────────────────────────────────

def bench_latency(server_bin, client_bin, is_go, crypt, nocomp,
                 rps, size, duration, warmup_s, label):
    allocated = allocate_ports()
    if not allocated:
        log(f"  [{label}] cannot allocate ports")
        return None
    echo_port, srv_port, cli_port = allocated

    echo = start_echo_server(echo_port)
    if not wait_port_ready(echo_port, timeout=3):
        echo.close()
        return None

    tunnel = start_tunnel(server_bin, client_bin, is_go,
                           echo_port, srv_port, cli_port,
                           crypt, nocomp, 1, label)
    if not tunnel:
        echo.close()
        return None
    srv, cli = tunnel

    try:
        interval = 1.0 / rps
        payload = bytes(range(256)) * (size // 256)
        if len(payload) < size:
            payload += bytes(size - len(payload))

        s = socket.socket()
        s.settimeout(10)
        s.connect(("127.0.0.1", cli_port))
        s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)

        latencies = []
        rx_buf = bytearray()
        in_flight = []
        next_send = time.monotonic()
        warmup_end = next_send + warmup_s
        measure_end = warmup_end + duration

        while time.monotonic() < measure_end:
            now = time.monotonic()
            if now >= next_send:
                s.sendall(payload)
                in_flight.append(time.monotonic())
                next_send += interval
                if next_send < now:
                    next_send = now + interval

            s.settimeout(max(0, min(0.002, next_send - time.monotonic())))
            try:
                data = s.recv(65536)
                if data:
                    rx_buf.extend(data)
                    while len(rx_buf) >= size and in_flight:
                        rx_buf[:] = rx_buf[size:]
                        t0 = in_flight.pop(0)
                        if time.monotonic() >= warmup_end:
                            latencies.append((time.monotonic() - t0) * 1e6)
            except (socket.timeout, BlockingIOError, OSError) as e:
                # On Linux, settimeout(~0) puts socket in non-blocking mode,
                # so recv() raises BlockingIOError (EAGAIN) instead of
                # socket.timeout.  Both are harmless — just retry next loop.
                if isinstance(e, OSError) and e.errno not in (11, 35):
                    raise

        s.close()

        if not latencies:
            log(f"  [{label}] no latency samples")
            return None

        latencies.sort()
        n = len(latencies)

        def pct(q):
            return latencies[min(int(n * q), n - 1)]

        return {
            "p50_us": pct(0.50),
            "p90_us": pct(0.90),
            "p99_us": pct(0.99),
            "p999_us": pct(0.999),
            "avg_us": sum(latencies) / n,
            "min_us": latencies[0],
            "max_us": latencies[-1],
            "n": n,
        }
    finally:
        stop_tunnel(srv, cli)
        echo.close()
        kill_ports(echo_port, srv_port, cli_port)
        time.sleep(0.5)

# ─── Report formatting ─────────────────────────────────────────────────────

def fmt_throughput_table(results, crypts, impls):
    lines = []
    header = f"{'Cipher':<16} {'Comp':>5}"
    for impl in impls:
        header += f" {impl:>12} {impl:>8}"
    header += f" {'winner':>8}"
    lines.append(header)
    lines.append("-" * len(header))

    for crypt in crypts:
        for nocomp, comp_label in [(True, "off"), (False, "on")]:
            row = f"{crypt:<16} {comp_label:>5}"
            best = 0.0
            best_impl = ""
            for impl in impls:
                key = (crypt, nocomp)
                r = results.get(impl, {}).get(key)
                if r and r.get("throughput_mbs", 0) > 0:
                    mb = r["throughput_mbs"]
                    row += f" {mb:>10.1f}MB/s {r['ok']:>4}/{r['fail']:>3}"
                    if mb > best:
                        best = mb
                        best_impl = impl
                else:
                    row += f" {'N/A':>12} {'':>8}"
            row += f" {best_impl:>8}"
            lines.append(row)
    lines.append("")
    return "\n".join(lines)

def fmt_latency_table(results, crypts, impls):
    lines = []
    header = f"{'Cipher':<16} {'Comp':>5} {'Metric':>6}"
    for impl in impls:
        header += f" {impl:>12}"
    lines.append(header)
    lines.append("-" * len(header))

    for crypt in crypts:
        for nocomp, comp_label in [(True, "off"), (False, "on")]:
            key = (crypt, nocomp)
            for metric, label in [("p50_us", "P50"), ("p99_us", "P99"), ("p999_us", "P999")]:
                row = f"{crypt:<16} {comp_label:>5} {label:>6}"
                for impl in impls:
                    r = results.get(impl, {}).get(key)
                    if r and metric in r:
                        row += f" {r[metric]/1000:>10.2f}ms"
                    else:
                        row += f" {'N/A':>12}"
                lines.append(row)
    lines.append("")
    return "\n".join(lines)

# ─── Main ──────────────────────────────────────────────────────────────────

def main():
    parser = argparse.ArgumentParser(description="kcptun-rs vs Go benchmark")
    parser.add_argument("--quick", action="store_true", help="fewer ciphers, fewer runs")
    parser.add_argument("--rust-only", action="store_true")
    parser.add_argument("--go-only", action="store_true")
    parser.add_argument("--latency-only", action="store_true")
    parser.add_argument("--throughput-only", action="store_true")
    parser.add_argument("--conn", type=int, default=4, help="concurrent connections")
    parser.add_argument("--size", type=int, default=1048576, help="payload bytes")
    parser.add_argument("--timeout", type=int, default=60, help="timeout seconds")
    parser.add_argument("--runs", type=int, default=1, help="repeats per config (median)")
    parser.add_argument("--crypts", type=str, default="", help="comma-separated cipher list")
    parser.add_argument("--go-src", type=str, default=None, help="Go kcptun source dir (only needed if Go binaries not pre-built)")
    parser.add_argument("--rps", type=int, default=200, help="requests/sec for latency test")
    parser.add_argument("--lat-duration", type=int, default=10, help="latency test duration (s)")
    parser.add_argument("--lat-size", type=int, default=4096, help="latency payload size (B)")
    args = parser.parse_args()

    crypts = args.crypts.split(",") if args.crypts else (QUICK_CRYPTS if args.quick else ALL_CRYPTS)
    impls = []          # list of impl labels: "Go", "Rust-tokio"
    all_bins = {}       # label -> {"client": path, "server": path}

    script_dir = os.path.dirname(os.path.abspath(__file__))

    if not args.go_only:
        rust_variants = find_rust_binaries(script_dir)
        for label in ["tokio"]:
            if label in rust_variants:
                impl_name = f"Rust-{label}"
                impls.append(impl_name)
                all_bins[impl_name] = rust_variants[label]
                log(f"{impl_name} binaries: {rust_variants[label]['client']}")
        if not rust_variants:
            log("Rust binaries not found — skipping Rust tests")

    if not args.rust_only:
        go_bins = find_go_binaries(script_dir)
        if go_bins:
            log(f"Go binaries (pre-built): {go_bins['client']}")
        elif args.go_src:
            go_bins = build_go_binaries(args.go_src)
            if go_bins:
                log(f"Go binaries (built from source): {go_bins['client']}")
        if go_bins:
            impls.append("Go")
            all_bins["Go"] = go_bins
        else:
            log("Go binaries not found. Use build_linux.sh to pre-build, or pass --go-src /path/to/kcptun")

    if not impls:
        log("ERROR: No implementations to test. Exiting.")
        sys.exit(1)

    log(f"Testing {len(crypts)} ciphers × 2 comp modes = {len(crypts)*2} configs")
    log(f"Implementations: {', '.join(impls)}")
    log(f"Throughput: conn={args.conn}, size={args.size//1024}KB, runs={args.runs}")
    log(f"Latency: rps={args.rps}, size={args.lat_size}B, duration={args.lat_duration}s")
    log("")

    # ─── Throughput ───
    tp_results = {impl: {} for impl in impls}
    if not args.latency_only:
        log("=" * 60)
        log("THROUGHPUT BENCHMARK")
        log("=" * 60)
        for crypt in crypts:
            for nocomp in [True, False]:
                comp_label = "nocomp" if nocomp else "comp"
                label_base = f"crypt={crypt} {comp_label}"
                for impl in impls:
                    bins = all_bins[impl]
                    is_go = impl == "Go"
                    medians = []
                    for run in range(args.runs):
                        label = f"{impl} {label_base} run={run+1}/{args.runs}"
                        r = bench_throughput(
                            bins["server"], bins["client"], is_go,
                            args.conn, args.size, args.timeout,
                            crypt, nocomp, label)
                        if r:
                            medians.append(r["throughput_mbs"])
                            log(f"  [{label}] {r['throughput_mbs']:.1f} MB/s "
                                f"({r['ok']} ok / {r['fail']} fail, {r['wall_s']:.1f}s)")
                        else:
                            log(f"  [{label}] FAILED")
                    if medians:
                        medians.sort()
                        median = medians[len(medians) // 2]
                        # Store the best result's details
                        tp_results[impl][(crypt, nocomp)] = {
                            "throughput_mbs": median,
                            "ok": args.conn,
                            "fail": 0,
                        }

        log("\n" + fmt_throughput_table(tp_results, crypts, impls))

    # ─── Latency ───
    lat_results = {impl: {} for impl in impls}
    if not args.throughput_only:
        log("=" * 60)
        log("LATENCY BENCHMARK (P50 / P99 / P999)")
        log("=" * 60)
        for crypt in crypts:
            for nocomp in [True, False]:
                comp_label = "nocomp" if nocomp else "comp"
                for impl in impls:
                    bins = all_bins[impl]
                    is_go = impl == "Go"
                    label = f"{impl} crypt={crypt} {comp_label}"
                    r = bench_latency(
                        bins["server"], bins["client"], is_go,
                        crypt, nocomp, args.rps, args.lat_size,
                        args.lat_duration, 3, label)
                    if r:
                        lat_results[impl][(crypt, nocomp)] = r
                        log(f"  [{label}] P50={r['p50_us']/1000:.2f}ms "
                            f"P99={r['p99_us']/1000:.2f}ms "
                            f"P999={r['p999_us']/1000:.2f}ms "
                            f"(n={r['n']})")
                    else:
                        log(f"  [{label}] FAILED")

        log("\n" + fmt_latency_table(lat_results, crypts, impls))

    # ─── Summary comparison ───
    rust_labels = [i for i in impls if i.startswith("Rust")]
    go_label = "Go" if "Go" in impls else None

    if len(impls) >= 2 and not args.latency_only:
        log("=" * 70)
        log("SUMMARY: Throughput comparison")
        log("=" * 70)
        for crypt in crypts:
            for nocomp in [True, False]:
                key = (crypt, nocomp)
                comp = "nocomp" if nocomp else "comp"
                vals = {}
                for impl in impls:
                    v = tp_results.get(impl, {}).get(key, {}).get("throughput_mbs", 0)
                    if v > 0:
                        vals[impl] = v
                if len(vals) >= 2:
                    best_impl = max(vals, key=vals.get)
                    parts = [f"{impl}={v:>7.1f}" for impl, v in vals.items()]
                    log(f"  {crypt:<16} {comp:>6}: {'  '.join(parts)}  winner={best_impl}")

    if len(impls) >= 2 and not args.throughput_only:
        log("\n" + "=" * 70)
        log("SUMMARY: P99 latency comparison (lower is better)")
        log("=" * 70)
        for crypt in crypts:
            for nocomp in [True, False]:
                key = (crypt, nocomp)
                comp = "nocomp" if nocomp else "comp"
                vals = {}
                for impl in impls:
                    r = lat_results.get(impl, {}).get(key, {})
                    v = r.get("p99_us", 0)
                    if v > 0:
                        vals[impl] = v
                if len(vals) >= 2:
                    best_impl = min(vals, key=vals.get)
                    parts = [f"{impl}={v/1000:>7.2f}ms" for impl, v in vals.items()]
                    log(f"  {crypt:<16} {comp:>6}: {'  '.join(parts)}  winner={best_impl}")

    # ─── JSON output ───
    json_out = {
        "throughput": {impl: {f"{k[0]}/{k[1]}": v for k, v in d.items()}
                       for impl, d in tp_results.items()},
        "latency": {impl: {f"{k[0]}/{k[1]}": v for k, v in d.items()}
                    for impl, d in lat_results.items()},
        "config": {
            "conn": args.conn, "size": args.size, "runs": args.runs,
            "rps": args.rps, "lat_duration": args.lat_duration,
            "lat_size": args.lat_size, "crypts": crypts,
        },
    }
    json_path = os.path.join(os.path.dirname(os.path.abspath(__file__)), "bench_results_linux.json")
    with open(json_path, "w") as f:
        json.dump(json_out, f, indent=2)
    log(f"\nJSON results saved to {json_path}")

if __name__ == "__main__":
    main()
