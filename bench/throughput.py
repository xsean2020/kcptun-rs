#!/usr/bin/env python3
"""Throughput + latency benchmark for kcptun.

Connects to a local kcptun-client TCP port, sends DATA_SIZE bytes in CHUNK_SIZE
chunks, receives the echo concurrently, and measures:
  - Throughput (MB/s, unidirectional)
  - Round-trip latency (ms for 1KB packet)

Usage:
    python3 throughput.py <port> [data_size_mb] [chunk_size_kb]
"""
import socket
import sys
import time
import threading
import argparse


def bench_throughput(port: int, data_mb: int, chunk_kb: int) -> float:
    """Send data_mb MB, receive echo concurrently, return throughput in MB/s.

    Uses a receiver thread to drain the echo stream while the main thread
    keeps sending. Without this, the TCP receive buffer fills up and
    sendall() blocks — a classic TCP loopback deadlock.
    """
    chunk = bytes(range(256)) * (chunk_kb * 4)  # chunk_kb KB of patterned data
    total = data_mb * 1024 * 1024

    sock = socket.socket()
    sock.settimeout(60)
    sock.connect(("127.0.0.1", port))
    sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)

    # Receiver thread: drains echo data concurrently to prevent deadlock
    received = [0]
    error = [None]

    def receiver():
        try:
            while received[0] < total:
                data = sock.recv(65536)
                if not data:
                    break
                received[0] += len(data)
        except Exception as e:
            error[0] = e

    rx_thread = threading.Thread(target=receiver, daemon=True)
    rx_thread.start()

    start = time.perf_counter()

    # Send all data — receiver thread drains echo concurrently
    sent = 0
    while sent < total:
        n = min(len(chunk), total - sent)
        try:
            sock.sendall(chunk[:n])
        except socket.timeout:
            break
        sent += n

    # Wait for receiver to finish draining echo
    rx_thread.join(timeout=30)
    elapsed = time.perf_counter() - start
    sock.close()

    if error[0] is not None:
        print(f"  WARNING: receiver error: {error[0]}", file=sys.stderr)
        return 0.0
    if received[0] < total:
        print(
            f"  WARNING: only received {received[0]}/{total} bytes",
            file=sys.stderr,
        )
        return 0.0

    # Throughput = total data sent / time (unidirectional)
    return (total / (1024 * 1024)) / elapsed


def bench_latency(port: int, iterations: int) -> float:
    """Measure RTT for small packets (1KB). Returns median latency in ms."""
    latencies = []
    sock = socket.socket()
    sock.settimeout(10)
    sock.connect(("127.0.0.1", port))
    sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)

    payload = b"X" * 1024

    for _ in range(iterations):
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

    sock.close()
    latencies.sort()
    return latencies[len(latencies) // 2]


def main():
    parser = argparse.ArgumentParser(description="kcptun throughput benchmark")
    parser.add_argument("port", type=int, help="kcptun-client local TCP port")
    parser.add_argument(
        "--data-mb", type=int, default=200, help="Data size in MB (default: 20)"
    )
    parser.add_argument(
        "--chunk-kb", type=int, default=128, help="Chunk size in KB (default: 64)"
    )
    parser.add_argument(
        "--latency-iterations",
        type=int,
        default=50,
        help="Latency iterations (default: 50)",
    )
    args = parser.parse_args()

    # Warmup
    print(
        f"  Warming up ({min(args.data_mb, 2)}MB, {args.chunk_kb}KB chunks)...",
        file=sys.stderr,
    )
    bench_throughput(args.port, min(args.data_mb, 2), args.chunk_kb)

    # Throughput
    print(f"  Throughput: sending {args.data_mb}MB...", file=sys.stderr)
    tp = bench_throughput(args.port, args.data_mb, args.chunk_kb)
    print(f"  Throughput: {tp:.2f} MB/s")

    # Latency
    print(f"  Latency: {args.latency_iterations} iterations...", file=sys.stderr)
    lat = bench_latency(args.port, args.latency_iterations)
    print(f"  Latency:   {lat:.2f} ms (median RTT, 1KB packet)")


if __name__ == "__main__":
    main()
