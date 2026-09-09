#!/usr/bin/env python3
"""Throughput + latency benchmark for kcptun.

Connects to a local kcptun-client TCP port, sends DATA_SIZE bytes in CHUNK_SIZE
chunks, receives the echo concurrently, and measures:
  - Throughput (MB/s, unidirectional)
  - Round-trip latency (ms for 1KB packet)

Usage:
    python3 throughput.py <port> --data-mb 4 --connections 64
"""
import socket
import sys
import time
import threading
import argparse
from concurrent.futures import ThreadPoolExecutor


def transfer_timeout_seconds(data_mb: int, configured: float) -> float:
    """Return a bounded transfer timeout suitable for the selected payload.

    A fixed ten-second timeout makes an otherwise healthy 800MB transfer look
    like a runtime regression at anything below 80 MB/s.  The automatic value
    permits a conservative 5 MB/s while keeping a genuinely stalled benchmark
    bounded.  A positive command-line value takes precedence.
    """
    if configured > 0:
        return configured
    return min(180.0, max(10.0, data_mb / 5.0))


def _bench_one_connection(port, total, chunk, timeout_seconds, start_gate):
    """Run one full-duplex stream and return (ok, elapsed, error)."""
    sock = socket.socket()
    sock.settimeout(timeout_seconds)
    try:
        sock.connect(("127.0.0.1", port))
        sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    except Exception as exc:
        try:
            start_gate.abort()
        except Exception:
            pass
        sock.close()
        return False, 0.0, exc

    received = [0]
    error = [None]

    def receiver():
        try:
            while received[0] < total:
                try:
                    data = sock.recv(65536)
                except socket.timeout:
                    continue
                if not data:
                    break
                received[0] += len(data)
        except Exception as exc:
            error[0] = exc

    rx_thread = threading.Thread(target=receiver, daemon=True)
    rx_thread.start()

    try:
        start_gate.wait()
    except threading.BrokenBarrierError as exc:
        sock.close()
        rx_thread.join(timeout=1)
        return False, 0.0, exc

    start = time.perf_counter()
    sent = 0
    try:
        while sent < total:
            n = min(len(chunk), total - sent)
            sock.sendall(chunk[:n])
            sent += n
    except Exception as exc:
        error[0] = exc

    rx_thread.join(timeout=timeout_seconds)
    elapsed = time.perf_counter() - start
    sock.close()

    if error[0] is not None or received[0] < total:
        return False, elapsed, error[0] or RuntimeError(
            f"only received {received[0]}/{total} bytes"
        )
    return True, elapsed, None


def bench_throughput(
    port: int, data_mb: int, chunk_kb: int, timeout_seconds: float, connections: int = 1
) -> float:
    """Send data_mb MB per connection and return aggregate throughput in MB/s.

    Uses a receiver thread to drain the echo stream while the main thread
    keeps sending. Without this, the TCP receive buffer fills up and
    sendall() blocks — a classic TCP loopback deadlock.
    """
    chunk = bytes(range(256)) * (chunk_kb * 4)  # chunk_kb KB of patterned data
    total = data_mb * 1024 * 1024

    if connections == 1:
        ok, elapsed, error = _bench_one_connection(
            port, total, chunk, timeout_seconds, threading.Barrier(1)
        )
        if not ok:
            print(f"  WARNING: transfer failed: {error}", file=sys.stderr)
            return 0.0
        return (total / (1024 * 1024)) / elapsed

    # Every connection gets the same payload.  The reported rate aggregates
    # all connections, while the timer starts after all sockets are ready.
    start_gate = threading.Barrier(connections, timeout=timeout_seconds)
    with ThreadPoolExecutor(max_workers=connections) as pool:
        results = list(
            pool.map(
                lambda _: _bench_one_connection(
                    port, total, chunk, timeout_seconds, start_gate
                ),
                range(connections),
            )
        )

    failed = [result for result in results if not result[0]]
    if failed:
        print(
            f"  WARNING: {len(failed)}/{connections} connections failed; "
            f"first error: {failed[0][2]}",
            file=sys.stderr,
        )
        return 0.0
    elapsed = max(result[1] for result in results)
    return (total * connections / (1024 * 1024)) / elapsed


def bench_latency(port: int, iterations: int) -> float:
    """Measure RTT for small packets (1KB). Returns median latency in ms."""
    latencies = []
    sock = socket.socket()
    sock.settimeout(5)
    sock.connect(("127.0.0.1", port))
    sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)

    payload = b"X" * 1024

    for _ in range(iterations):
        try:
            start = time.perf_counter()
            sock.sendall(payload)
            data = b""
            while len(data) < len(payload):
                chunk = sock.recv(1024)
                if not chunk:
                    break
                data += chunk
            elapsed_ms = (time.perf_counter() - start) * 1000
            latencies.append(elapsed_ms)
        except socket.timeout:
            continue  # skip this iteration on timeout

    sock.close()
    if not latencies:
        return 0.0
    latencies.sort()
    return latencies[len(latencies) // 2]


def main():
    parser = argparse.ArgumentParser(description="kcptun throughput benchmark")
    parser.add_argument("port", type=int, help="kcptun-client local TCP port")
    parser.add_argument(
        "--data-mb", type=int, default=200, help="Data size in MB (default: 200)"
    )
    parser.add_argument(
        "--chunk-kb", type=int, default=128, help="Chunk size in KB (default: 128)"
    )
    parser.add_argument(
        "--latency-iterations",
        type=int,
        default=50,
        help="Latency iterations (default: 50)",
    )
    parser.add_argument(
        "--timeout-seconds",
        type=float,
        default=0,
        help="Per-transfer timeout; 0 derives one from data size (default: 0)",
    )
    parser.add_argument(
        "--connections",
        type=int,
        default=1,
        help="Concurrent TCP connections (default: 1)",
    )
    args = parser.parse_args()

    if args.connections < 1:
        parser.error("--connections must be at least 1")

    timeout_seconds = transfer_timeout_seconds(args.data_mb, args.timeout_seconds)

    # Warmup
    print(
        f"  Warming up ({min(args.data_mb, 2)}MB/connection, "
        f"{args.connections} connections, {args.chunk_kb}KB chunks)...",
        file=sys.stderr,
    )
    bench_throughput(
        args.port,
        min(args.data_mb, 2),
        args.chunk_kb,
        transfer_timeout_seconds(min(args.data_mb, 2), args.timeout_seconds),
        args.connections,
    )

    # Throughput
    print(
        f"  Throughput: sending {args.data_mb}MB/connection "
        f"({args.connections} connections)...",
        file=sys.stderr,
    )
    tp = bench_throughput(
        args.port, args.data_mb, args.chunk_kb, timeout_seconds, args.connections
    )
    print(f"  Throughput: {tp:.2f} MB/s")

    if tp == 0.0:
        return 1

    # Latency
    print(f"  Latency: {args.latency_iterations} iterations...", file=sys.stderr)
    lat = bench_latency(args.port, args.latency_iterations)
    print(f"  Latency:   {lat:.2f} ms (median RTT, 1KB packet)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
