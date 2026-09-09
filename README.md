<div align="center">

# kcptun-rs ⚡

**Rust port of kcptun — up to 5.38× faster than Go, fully wire-compatible**

[![Build Status](https://img.shields.io/badge/build-passing-brightgreen)](#)
[![Tests](https://img.shields.io/badge/tests-334%20passed-brightgreen)](#)
[![E2E](https://img.shields.io/badge/e2e-68%20passed-brightgreen)](#)
[![License](https://img.shields.io/badge/license-MIT-blue)](#)
[![Rust](https://img.shields.io/badge/rust-1.92+-orange)](#)
[![Go Compatible](https://img.shields.io/badge/Go%20compat-v5-success)](#)

English | [中文](README.zh.md)

</div>

---

> <details>
> <summary><b>Disclaimer</b> — This project is a Vibe Coding porting test, for educational purposes only.</summary>
>
> This project is a **Vibe Coding** experiment — using AI-assisted programming to port an existing codebase. The core focus is exploring and validating the Vibe Coding workflow itself, not producing a production-grade software port. This is **not** production software. No guarantees are made regarding correctness, stability, or security.
>
> **It is strictly prohibited for any illegal use**, including but not limited to circumventing censorship, illegal data transmission, network attacks, etc. Any illegal activities by users are not related to this project or its author. Users bear all legal responsibilities.
>
> See [DISCLAIMER.md](DISCLAIMER.md) for the full disclaimer.
> </details>

---

## 🔥 Performance at a Glance

kcptun-rs **outperforms Go kcptun across nearly every cipher and compression setting**, while maintaining **full wire compatibility** — meaning you can replace one side of a Go kcptun tunnel with the Rust binary and get an instant speed boost.

| Cipher | vs Go (macOS M1) | vs Go (Linux VPS) |
|--------|:---------------:|:-----------------:|
| **SM4** (nocomp) | **4.76× faster** 🏆 | **1.87× faster** |
| **SM4** (comp) | **5.32× faster** 🏆 | **3.17× faster** 🏆 |
| **XOR** (nocomp) | **2.54× faster** | **1.60× faster** |
| **CAST5** (nocomp) | **1.98× faster** | **1.04× faster** |
| **Twofish** (comp) | **1.69× faster** | **1.63× faster** |
| **AES-128** (nocomp) | **1.59× faster** | **1.14× faster** |
| **AES-128-GCM** (comp) | **1.33× faster** | **1.25× faster** |
| **AES-128** (comp) | **1.18× faster** | **1.83× faster** |
| **AES-128** bulk | **1.67× faster** | **1.17× faster** |

*macOS M1: 10 conn × 1 MB. Linux VPS: 1 vCPU AMD EPYC-Rome, 4 conn × 1 MB. Full matrix below.*

---

## 📖 What is kcptun-rs?

**kcptun** is a stable & secure TCP-over-UDP tunnel that uses [KCP](https://github.com/skywind3000/kcp) (a fast ARQ protocol) to accelerate TCP streams over high-latency or lossy networks. It features SMUX multiplexing, Reed-Solomon FEC, Snappy compression, and optional encryption — all wrapped in a single binary.

**kcptun-rs** is a complete Rust reimplementation that is:

- ✅ **Wire-compatible** with Go kcptun (kcp-go v5) — Rust ↔ Go, Go → Rust, Rust → Rust all work
- ⚡ **Faster than Go** on most cipher/mode combinations (up to **5.38×** on macOS, **1.11–1.59×** on Linux VPS)
- 🧩 **13 encryption backends** + AES-128-GCM: AES, SM4, Salsa20, Blowfish, Twofish, CAST5, 3DES, TEA, XTEA, XOR, and more
- ⚡ **Single async runtime**: tokio (multi-threaded, high-concurrency)
- 🎯 **Production-ready features**: FEC, SMUX v1/v2, QPP obfuscation, SNMP stats, rate limiting, pprof profiling
- 🔄 **Cross-platform**: macOS, Linux, ARMv7 (Raspberry Pi), ARM64 (Graviton)

---

## ✨ Features

| Category | Details |
|----------|---------|
| **Compatibility** | Full wire-compatible with Go kcptun (kcp-go v5) — all ciphers, modes, SMUX versions, FEC, Snappy |
| **Encryption** | 14 backends: `null`, `none`, `xor`, `aes-128`, `aes-192`, `aes`(256), `aes-128-gcm`, `sm4`, `tea`, `xtea`, `salsa20`, `blowfish`, `twofish`, `cast5`, `3des` |
| **KCP Modes** | `normal`, `fast`, `fast2`, `fast3` |
| **SMUX** | v1 & v2 multiplexing — multiple TCP streams over a single KCP connection |
| **FEC** | Reed-Solomon forward error correction (Go-compatible 10/3 default) |
| **Compression** | Session-level Snappy compression, Go byte-identical, on by default |
| **QPP** | Quantum Permutation Pad — optional post-quantum stream obfuscation |
| **Runtime** | tokio (multi-threaded) |
| **Profiling** | Optional `--pprof` emits Go-compatible protobuf → `go tool pprof` |
| **Rate Limiting** | Per-connection token bucket pacing (`--ratelimit`) |
| **SNMP Stats** | Go-compatible SNMP fields with zero-cost opt-in collection |
| **Cross-Compile** | ARMv7 (Raspberry Pi), ARM64 (Graviton), Linux musl — all from macOS |
| **Logging** | Structured log levels (RUST_LOG), optional file logging |

---

## 🚀 Quick Start

```bash
# Build (optimized release with LTO)
cargo build --release
# Binary: target/release/kcptun-server, target/release/kcptun-client

# Start server (listens on UDP :29900, forwards to local HTTP :8080)
./target/release/kcptun-server -t "127.0.0.1:8080" -l ":29900" --key "my-secret"

# Start client (listens on :12948, tunnels to remote server)
./target/release/kcptun-client -r "server-ip:29900" -l ":12948" --key "my-secret"
```

Now point your application at `127.0.0.1:12948` — all TCP data is encrypted, compressed, and accelerated over KCP to the remote server.

### With config file

```bash
kcptun-server -c config.json
kcptun-client -c config.json
```

```json
{
    "localaddr": ":12948",
    "remoteaddr": "vps:29900",
    "key": "my-secret",
    "crypt": "aes-128",
    "mode": "fast2",
    "conn": 2,
    "sndwnd": 1024,
    "rcvwnd": 1024,
    "datashard": 10,
    "parityshard": 3,
    "nocomp": false,
    "smuxver": 2,
    "keepalive": 10
}
```

> ⚠️ **`--key`, `--crypt`, `--mode`, and `--nocomp` must match between server and client.** Compression is enabled by default.

---

## 📊 Performance Deep Dive

### Bulk Throughput (macOS M1, 200 MB, AES-128-CFB, no compression)

Path labels are **Client → Server** (bulk data leaves the client toward the server; from `bench/run_bench.sh`).

| Path (Client → Server) | Throughput | Latency | vs Go→Go |
|------|:---------:|:-------:|:--------:|
| **Go → Go** | 51.15 MB/s | 0.31 ms | 1.00× |
| **Rust-Tokio → Rust-Tokio** | **85.60 MB/s** 🏆 | **0.12 ms** | **1.67×** |
| Rust-Tokio → Go | 76.48 MB/s | 0.11 ms | 1.50× |
| Go → Rust-Tokio | 30.28 MB/s | 0.15 ms | 0.59× |

> Rust-Tokio is clearly faster than Go→Go on this M1 host.

### Full Cipher × Compression Matrix

Tests: 10 concurrent connections, 1 MB each, all 30+ runs per cell passed (0 failures).

**Without compression** (`--nocomp`):

| Cipher | Rust-Tokio | Go | R/Go |
|--------|:----------:|:--:|:----:|
| null | 38.4 | 35.5 | 1.08× |
| none | 29.4 | 39.2 | 0.75× |
| xor | 41.6 | 16.4 | **2.54×** |
| aes-128 | 43.4 | 27.2 | **1.59×** |
| aes-128-gcm | 36.6 | 41.5 | 0.88× |
| salsa20 | 35.8 | 32.3 | **1.11×** |
| blowfish | 31.5 | 28.6 | **1.10×** |
| twofish | 35.1 | 23.2 | **1.51×** |
| cast5 | 33.3 | 16.9 | **1.98×** |
| 3des | 14.5 | 11.8 | **1.23×** |
| tea | 38.2 | 31.7 | **1.20×** |
| xtea | 24.7 | 18.6 | **1.33×** |
| **sm4** | **16.7** | **3.5** | **4.76×** 🏆 |

**With compression** (Snappy):

| Cipher | Rust-Tokio | Go | R/Go |
|--------|:----------:|:--:|:----:|
| aes-128-gcm | 36.4 | 27.4 | **1.33×** |
| salsa20 | 29.0 | 20.1 | **1.44×** |
| **sm4** | **18.7** | **3.5** | **5.32×** 🏆 |
| twofish | 34.4 | 20.4 | **1.69×** |
| cast5 | 36.5 | 26.4 | **1.38×** |
| blowfish | 34.4 | 25.7 | **1.33×** |
| aes-128 | 31.3 | 26.5 | **1.18×** |

> **SM4** is the standout: Rust outperforms Go by **4.6–5.4×** because the Go implementation uses a pure software S-box while Rust benefits from compiler auto-vectorization and pre-computed lookup tables.

### Linux VPS Benchmark (1 vCPU, AMD EPYC-Rome)

Results from a Linux VPS (CentOS 8, 1 vCPU / 2 threads, AMD EPYC-Rome @ 2.8 GHz, 2 GB RAM) — the kind of low-end cloud instance where kcptun is commonly deployed. Kernel UDP buffers (`net.core.rmem_max=4MB`) and CFS wakeup granularity (1 ms) were tuned before testing (see [Latency Tuning Guide](#-latency-tuning-guide-linux) below).

**Bulk Throughput (100 MB, AES, fast mode, from `bench/run_bench.sh`):**

| Path (Client → Server) | Throughput | Latency | vs Go→Go |
|------|:---------:|:-------:|:--------:|
| **Go → Go** | 51.27 MB/s | 0.27 ms | 1.00× |
| **Rust-Tokio → Rust-Tokio** | **59.91 MB/s** 🏆 | **0.20 ms** | **1.17×** |
| Rust-Tokio → Go | 57.94 MB/s | 0.23 ms | 1.13× |
| Go → Rust-Tokio | 64.36 MB/s | 0.20 ms | 1.26× |

**Full Cipher × Compression Matrix (4 conn × 1 MB, from `bench/bench_linux_cmp.py`):**

Without compression (`--nocomp`):

| Cipher | Rust-Tokio | Go | R/Go | Winner |
|--------|:----------:|:--:|:----:|:------:|
| null | **66.2** | 49.8 | **1.33×** | Rust |
| none | **56.6** | 41.7 | **1.36×** | Rust |
| xor | **58.4** | 36.5 | **1.60×** | Rust |
| aes-128 | **45.5** | 39.9 | **1.14×** | Rust |
| aes-192 | **50.1** | 43.9 | **1.14×** | Rust |
| aes | **50.3** | 35.2 | **1.43×** | Rust |
| sm4 | **24.3** | 13.0 | **1.87×** | Rust |
| tea | **36.5** | 27.7 | **1.32×** | Rust |
| xtea | **23.6** | 18.7 | **1.26×** | Rust |
| salsa20 | 22.6 | **36.3** | 0.62× | Go |
| blowfish | **33.4** | 20.2 | **1.65×** | Rust |
| twofish | **34.3** | 22.4 | **1.53×** | Rust |
| cast5 | **24.9** | 23.9 | **1.04×** | Rust |
| 3des | **8.4** | 8.1 | **1.04×** | Rust |
| aes-128-gcm | **61.7** | 48.2 | **1.28×** | Rust |

With compression (Snappy):

| Cipher | Rust-Tokio | Go | R/Go | Winner |
|--------|:----------:|:--:|:----:|:------:|
| null | 50.6 | **52.3** | 0.97× | Go |
| none | **59.7** | 48.8 | **1.22×** | Rust |
| xor | **54.7** | 46.8 | **1.17×** | Rust |
| aes-128 | **60.2** | 32.9 | **1.83×** | Rust 🏆 |
| aes-192 | **48.6** | 33.4 | **1.46×** | Rust |
| aes | **50.3** | 37.8 | **1.33×** | Rust |
| sm4 | **23.8** | 7.5 | **3.17×** | Rust 🏆 |
| tea | **40.7** | 29.8 | **1.37×** | Rust |
| xtea | **13.7** | 11.0 | **1.25×** | Rust |
| salsa20 | 25.4 | **27.5** | 0.92× | Go |
| blowfish | **29.4** | 21.9 | **1.34×** | Rust |
| twofish | **33.2** | 20.4 | **1.63×** | Rust |
| cast5 | **31.3** | 23.2 | **1.35×** | Rust |
| 3des | **12.1** | 7.9 | **1.53×** | Rust |
| aes-128-gcm | **48.4** | 38.6 | **1.25×** | Rust |

> **Linux VPS result:** Rust-Tokio wins **28 out of 30** cipher×compression combinations on this 1-vCPU VPS. The only exceptions are `salsa20` (Go's Salsa20 implementation is highly optimized) and `null`+comp (where Go's Snappy edges ahead on trivially compressible data with no crypto overhead). Standout ratios: **SM4+comp 3.17×**, **AES-128+comp 1.83×**, **XOR 1.60×**, **Blowfish 1.65×**, **Twofish 1.53×**. The key prerequisite is tuning kernel UDP buffers (`net.core.rmem_max=4MB`) — with default 208 KB buffers, both backends lose ~80% throughput to silent packet drops.

### Stress Tests (Data Integrity)

All 8 stress tests pass — verifying **byte-for-byte accuracy** under concurrent load:

| Test | Connections | Payload | Result |
|------|:-----------:|:-------:|:------:|
| Single conn, mixed sizes | 1 | 1B…64KB | ✅ |
| Multi-thread 10 conn | 10 | 256B each | ✅ |
| Multi-thread 50 conn | 50 | 255B each | ✅ |
| Multi-thread 100 conn | 100 | 1B + 4KB | ✅ |
| Large data (100 conn) | 100 | 64KB + 128KB | ✅ |
| Page refresh simulation | 80 (3 waves) | 512B…128KB | ✅ |
| Compressible data | 1 | patterns | ✅ |

### Client I/O Modes

The `latency_p99` example supports three client I/O modes to measure the effect of different KCP scheduling architectures on round-trip latency:

```bash
# 1. Start an echo server in a separate process (avoids CPU contention)
cargo run -p kcp-rs --features async --example latency_p99 -- --mode server --port 39001

# 2a. Normal mode (per-connection tokio tasks) — the production default
cargo run -p kcp-rs --features async --example latency_p99 -- \
    --mode peer --addr 127.0.0.1:39001 --rps 200 --warmup 3 --duration 10

# 2b. WorkerPool direct_rx (1 worker, no reader thread)
cargo run -p kcp-rs --features async --example latency_p99 -- \
    --mode peer --addr 127.0.0.1:39001 --wp-client --wp-workers 1 --rps 200 --warmup 3 --duration 10

# 2c. WorkerPool channel (2 workers, dedicated reader + channel)
cargo run -p kcp-rs --features async --example latency_p99 -- \
    --mode peer --addr 127.0.0.1:39001 --wp-client --wp-workers 2 --rps 200 --warmup 3 --duration 10
```

**Architecture comparison:**

| | Normal (per-conn) | WP direct\_rx (1w) | WP channel (2w) |
|---|---|---|---|
| **RX path** | `input_loop` tokio task → `kcp.input()` | worker thread `try_recv_from` → `kcp.input()` | reader thread `recv_from().await` → channel → worker `kcp.input()` |
| **TX path** | `write_all` → inline `try_send_batch_to` | same inline send; flush/retransmit in worker | same inline send; flush/retransmit in worker |
| **Cross-thread hops** | 0 (same runtime) | 1 (worker → client `read_notify`) | 2 (reader → channel → worker, worker → client) |
| **Best for** | **All client use cases** | Experimental: low-RPS P999 tuning | Experimental: multi-conn server demux |

**Measured (200 RPS, 1KB, external server, macOS M1):**

| Mode | P50 | P99 | P999 | Max |
|------|----:|----:|-----:|----:|
| **Normal** | **604 μs** | **1.1 ms** | 16.6 ms | 26.3 ms |
| WP direct\_rx (1w) | 1.5 ms | 3.8 ms | 33.4 ms | 42.1 ms |
| WP channel (2w) | 1.4 ms | 3.7 ms | 15.1 ms | 23.8 ms |

**Measured (128 concurrent, 1KB, external server):**

| Mode | RPS | P50 | P99 | P999 |
|------|----:|----:|----:|-----:|
| **Normal** | **49,766** | 2.4 ms | 4.4 ms | 22.6 ms |
| WP direct\_rx (1w) | 37,764 | 3.3 ms | 4.7 ms | 9.1 ms |
| WP channel (2w) | 2,465 | 3.7 ms | 5.3 ms | 5.6 ms |

> **Conclusion:** Normal mode (per-connection tokio tasks) is the correct choice for clients. WorkerPool modes add cross-thread wake overhead that hurts P50 and throughput. WorkerPool's value is on the **server side** — shared UDP socket demux for many concurrent connections (not benchmarked here via `latency_p99`).

---

## 🔗 Go Compatibility

kcptun-rs is **fully wire-compatible** with Go kcptun (kcp-go v5). All 68 end-to-end interop tests pass across every combination:

| Feature | Status | Notes |
|---------|:------:|-------|
| KCP segment format | ✅ | 24-byte LE header, same as kcp-go v5 |
| Crypto header (CFB) | ✅ | `[nonce 16B][CRC32 4B][payload]` |
| AES-GCM | ✅ | `[nonce 12B][ciphertext+tag 16B]` |
| Snappy (session-level) | ✅ | Byte-identical to Go's `github.com/golang/snappy` |
| SMUX v1 & v2 | ✅ | Full frame format compatibility |
| FEC (10/3, 4/2) | ✅ | Reed-Solomon, same header format |
| Key derivation | ✅ | PBKDF2-HMAC-SHA1, salt `b"kcp-go"` |
| QPP obfuscation | ✅ | Stream-level, same permutation algorithm |
| All 15 ciphers | ✅ | Bidirectional (Go→Rust, Rust→Go) |
| All 4 KCP modes | ✅ | normal, fast, fast2, fast3 |
| SM4 (Chinese national standard) | ✅ | tjfoc/gmsm S-box + CK fix |
| CAST5 (RFC 2144) | ✅ | Full implementation, ported from Go |

### E2E Test Results

```
Encryption:  15/15 ciphers passed (Go→Rust + Rust→Go)
KCP Modes:   4/4 passed
SMUX:        2/2 versions passed
Compression: 8/8 cipher×compression combos passed
FEC:         2/2 configurations passed
Total:       68 passed, 0 failed, 0 skipped 🎉
```

---

## 🏗️ Architecture

### Protocol Stack

```
┌──────────────────────────────────┐
│         TCP / UNIX Socket        │
├──────────────────────────────────┤
│        SMUX Stream (mux)         │
├──────────────────────────────────┤
│       SMUX Session (mux)         │
├──────────────────────────────────┤
│  Snappy Compression (session)    │  ← byte-identical to Go
├──────────────────────────────────┤
│  BlockCrypt / FEC / KCP (ARQ)    │
├──────────────────────────────────┤
│           UDP / TCPraw           │
└──────────────────────────────────┘
```

### Workspace (9 crates)

```
kcptun-rs/
├── kcp-rs/          — KCP ARQ protocol state machine
├── kcrypt-rs/       — 13 block ciphers + AES-128-GCM
├── smux-rs/         — SMUX stream multiplexer (v1/v2)
├── qpp-rs/          — Quantum Permutation Pad obfuscation
├── knet-rs/          — Async I/O abstraction (tokio)
├── kpprof-rs/       — Go-compatible pprof HTTP server
├── kcptun-common/   — Shared client/server helpers
├── kcptun-client/   — Client binary
└── kcptun-server/   — Server binary + stress tests
```

### Runtime Design

- **tokio** (sole runtime) — multi-threaded, high-concurrency, production-scaled
- Business code uses `knet::*` abstractions only — never raw tokio APIs

### Flush Loop Optimization

The flush loop is split into **4 phases** to minimize KCP mutex hold time:

| Phase | Work | KCP Lock |
|:-----:|------|:---------:|
| 1 | Drain SMUX send buffers, collect FIN-pending streams | ❌ Not held |
| 2 | Encode SMUX frames | ❌ Not held |
| 3 | Snappy compress (if enabled) | ❌ Not held |
| 4 | `kcp.send()` + `kcp.update()` + `kcp.flush()` | ✅ Held briefly |

This allows the UDP recv loop to feed data into KCP while the flush loop prepares the next batch of frames — eliminating lock contention under high concurrency.

---

## 🔧 Build & Run

### Makefile Targets

```bash
make build          # Debug build (tokio)
make release        # Release build (LTO, stripped, panic=abort)
make test           # All unit tests
make stress         # Data integrity stress tests (needs release first)
make e2e            # Go↔Rust interop (needs Go kcptun binaries)
make clippy         # Lint (warnings = errors)
make fmt            # Format all Rust code
make profile        # Flamegraph profiling (samply → Speedscope)
```

### Cross-Compilation

```bash
make install-cross     # Install cross toolchains (one-time)
make release-armv7     # Raspberry Pi 2/3, OpenWrt (~1.3M binary)
make release-arm64     # Raspberry Pi 4/5, AWS Graviton
make linux             # x86_64 Linux musl (from macOS)
make linux-aarch64     # ARM64 Linux musl (from macOS)
```

ARM cross builds use the **tokio** runtime with `pprof` disabled for minimal binary size.

### Linux Binaries from macOS

Build fully static musl binaries directly from macOS — ideal for CI or deployment testing:

```bash
make linux              # x86_64 musl (tokio)
make linux-aarch64      # ARM64 musl (tokio)
make linux-full         # x86_64 musl + QPP
```

### OS-Level UDP Buffer Tuning (macOS)

On macOS, the default UDP socket buffer sizes are small (typically 256 KB for send, 256 KB for receive). Under high-throughput KCP workloads — especially with large windows (`sndwnd`/`rcvwnd` ≥ 512) or high concurrency — the kernel UDP receive buffer can overflow, causing **silent packet drops** that inflate P99/P999 tail latency and trigger KCP retransmit storms.

Increasing the kernel socket buffer limits eliminates this bottleneck. This is the single most effective OS-level tuning for P99/P999 latency on macOS:

```bash
# Raise max socket buffer to 8 MB (default ~256 KB)
sudo sysctl -w kern.ipc.maxsockbuf=8388608

# Raise UDP receive buffer to 4 MB (default ~256 KB)
sudo sysctl -w net.inet.udp.recvspace=4194304
```

> **Effect on P99/P999 (measured):** With these settings applied, the raw KCP layer's max sustainable throughput on loopback improved from **2975 → 3802 req/s** (tokio, +28%) and **2411 → 2921 req/s** (Go, +21%), with P99 latency dropping from 14.2 ms → 10.5 ms (tokio) and 20.3 ms → 15.9 ms (Go). See [bench/LATENCY_P99_REPORT.md](bench/LATENCY_P99_REPORT.md) for full numbers.

To make the change persistent across reboots, add to `/etc/sysctl.conf`:

```
kern.ipc.maxsockbuf=8388608
net.inet.udp.recvspace=4194304
```

> **Linux equivalent:** `net.core.rmem_max`, `net.core.rmem_default`, `net.core.wmem_max`, `net.core.wmem_default` — set to `4194304` or higher. Some distributions also require `net.core.netdev_max_backlog`.

### Runtime Environment Variables

Runtime tuning knobs are read from the environment when the relevant component starts:

| Variable | Default | Scope | Description |
|---|---|---|---|
| `KCP_BUSY_YIELDS` | `0` (disabled) | kcp-rs `KcpStream::read` | Spin-bounded busy-poll: how many `yield_now` spins `read()` performs before parking on the wakeup `Notify`. Compensates tokio's notify→wake→schedule→poll hop (~0.2–2 ms per wakeup) — Go gets equivalent wakeup speed for free from its runtime's µs-level goroutine scheduling. |
| `KCPTUN_WORKER_THREADS` | available parallelism (clamped to 1–16) | `KcpListener` shards | Default listener shard count. A positive value overrides auto-detection; an explicit Builder `worker_count(n)` overrides the environment. This variable does not size the shared Tokio runtime. |

**`KCP_BUSY_YIELDS` rules of thumb** (measured on arm64 macOS, 500 RPS × 26 KB echo; see [bench/LATENCY_P99_REPORT.md](bench/LATENCY_P99_REPORT.md)):

- **Latency-sensitive request initiator with few connections**: `512` can reduce wakeup tail latency at the cost of CPU.
- **Listener / high-concurrency server**: keep `0`. Spinning readers compete with connection and flush tasks and can sharply inflate P99.
- **Throughput-bound / closed-loop workloads**: keep `0`. Spinning measurably regressed closed-loop req/s. Accordingly, `bench/run_p99.sh` enables `512` only for the standalone Rust→Go request initiator (combination 3); Rust↔Rust self mode, the Rust server, and closed-loop runs explicitly use the event-driven setting.

The library default stays `0`, so production deployments remain event-driven. `KcpListener` creates one dedicated current-thread runtime per shard. The shared multi-thread Tokio runtime uses Tokio's system-derived default worker count.

---

## 🎛️ Latency Tuning Guide (Linux)

Practical tuning findings from the P99 optimization work (2026-09). All numbers
come from controlled same-machine A/B runs on a 1-vCPU Linux VM
([docs/kcp-rs-optimization-2026-09-01.md](docs/kcp-rs-optimization-2026-09-01.md) §6–§9).

### 1. Let the listener pick its topology (no action needed)

`KcpListener` (used by `kcptun-server`) selects its receive pipeline at bind time:

| Topology | When | What you get |
|:---------|:-----|:-------------|
| **Direct single worker** | `worker_count == 1` (any platform) | The worker drains the UDP socket itself — no reader thread, no queue hop. Best default on 1–2 vCPU hosts. |
| **Direct SO_REUSEPORT group** | Linux, fresh bind, N workers | One socket per worker; the kernel's 4-tuple hash keeps each session pinned to one worker (session affinity without user-space routing). Best for multi-core hosts. |
| **Reader pipeline** | Shared/external socket + N workers | Dedicated RX thread fans out to worker queues (also the fallback on macOS/Windows, where SO_REUSEPORT does not distribute UDP flows). |

Set the shard count with `KCPTUN_WORKER_THREADS` (library) — on Linux a value > 1
on a fresh bind automatically uses the reuseport group.

### 2. `--shards N` on kcptun-server (Linux)

Each shard is an independent SO_REUSEPORT socket processed by its own thread,
so there is no shared-fd send contention.

- Default (`--shards 0`): one shard per logical CPU on **Linux**; a single
  shard elsewhere (macOS does not distribute UDP via SO_REUSEPORT).
- Rule of thumb: **shards ≈ vCPUs**. On a 1–2 vCPU box keep 1 shard — every
  extra worker only adds cross-core wakeups on the same core (the N=2 reuseport
  path is verified correct, but on a single vCPU it is strictly slower).

### 3. Kernel CFS wakeup granularity — the single biggest P99 knob on Linux

The default `kernel.sched_wakeup_granularity_ns` (15 ms on many distros,
including CentOS 7 / kernel 3.10) lets a woken task wait up to 15 ms before it
may preempt the current one. Every hop in the wake chain (socket event →
runtime → KCP task → flush task) can absorb that gate, producing rare
**8–12 ms P999 tail clusters** on busy cores. Setting it to 1 ms erases the
cluster: in the same 3-run validation Rust's p99 dropped from 394 µs to
**112 µs** (p50/p90 93/91 → 19/78 µs), **beating Go** at every percentile
(Go is immune because goroutines reschedule on an already-running P in
user space).

```bash
# transient
sudo sysctl -w kernel.sched_wakeup_granularity_ns=1000000

# persistent (recommended for latency-sensitive hosts)
echo 'kernel.sched_wakeup_granularity_ns=1000000' | sudo tee /etc/sysctl.d/99-kcptun-low-latency.conf
sudo sysctl --system
```

Version notes: the knob lives at the `/proc/sys/kernel/` path up to kernel
5.15, under `/sys/kernel/debug/sched/` (debugfs) on 5.16–6.5, and is **removed
entirely on ≥ 6.6** (EEVDF scheduler — the gate no longer exists there; just
skip it). It is a host-wide setting owned by ops, so the binaries never touch
it themselves.

### 4. OS-level UDP buffers (Linux)

```bash
sudo sysctl -w net.core.rmem_max=4194304
sudo sysctl -w net.core.wmem_max=4194304
```

The binaries already request 4 MB per socket (`knet`); these sysctls just
raise the kernel ceiling so the requests are honored.

### 5. Quick checklist

| Scenario | Setting |
|:---------|:--------|
| 1–2 vCPU host (VPS, container) | 1 shard (`--shards 1` or default worker count 1) + `wakeup_granularity=1ms` |
| Multi-core host, many concurrent sessions | default `--shards` (= CPUs, reuseport group on Linux) + `wakeup_granularity=1ms` |
| Lossy network | keep default; FEC (`--datashard/--parityshard`) trades bandwidth for tail latency |
| Throughput benchmarking | restore `wakeup_granularity` to default (1 ms slightly raises fair-share switching) |

---

## 🔬 Optimization Journey

The project evolved from **5.4 MB/s** to over **108 MB/s** through evidence-driven profiling:

| Milestone | Throughput | vs Go |
|:----------|:----------:|:-----:|
| Initial port | 5.4 MB/s | 0.71× |
| + Event-driven flush scheduling | 7.1 MB/s | 0.87× |
| + Zero-copy KCP output pipeline | 68.8 MB/s | 1.43× |
| + ARMv8 AES hardware acceleration | ~85 MB/s | 1.67× |
| + Tokio persistent blocking pool | +108% | 2.1× |
| + SMUX v2 write window control | performance unblocked | — |
| + Snappy offload & threshold tuning | — | — |
| + sendmmsg/recvmmsg batch I/O | — | — |
| + Cipher enum static dispatch | vtable eliminated | — |
| + macOS UDP buffer tuning (sysctl) | P99 −26%, throughput +28% | — |
| + Tokio-aware worker queue (flush timers share the driver's epoll) | open-model p99 −17%, p999 −72~85% | — |
| + Direct-worker topologies (single-worker takeover + Linux SO_REUSEPORT group) | closed-loop throughput +4.1~6.6%, p99 −6~8% more | — |
| → **Final (tokio)** | **85.6 MB/s** | **1.67×** |

### Notable Bug Fixes Found Along the Way

| Bug | Impact | Fix |
|:----|:-------|:----|
| Blowfish key schedule per block | 0.0 MB/s (100× improvement) | Store cipher instance |
| Twofish key schedule per block | 0.4 → 4.5 MB/s (11×) | Custom pre-computed tables |
| CRC32C vs CRC32/IEEE in Snappy | Data silently dropped by Go | Switch to `snap::FrameEncoder` |
| KCP ACK never populated | Infinite retransmission → deadlock | Queue ACKs for every received Push |
| `snd_buf` never cleaned | Window stuck at 32 packets | Front-of-buffer cleanup in flush() |
| Twofish 256-bit key S-box | Wrong ciphertext vs Go | Added 5th sbox layer |

### p99 Latency Collapse Investigation (256KB @ High Concurrency)

Symptom: the raw `kcp-rs` KcpStream (no tunnel layers) collapsed on large
payloads under sustained load — 256KB round-trip at **RPS=300 went from ~4ms
to p50=3.2s**, while Go with the *identical* 512/512 window + Fast3 config
stayed at **19ms**. Single-request latency was already fast (4.3ms); the
pipeline stalled only when requests overlapped.

**Root cause:** every received KCP segment triggered a full
`flush_with_current()` (kcp.input → `parse_una>0` → iterate the whole
`snd_buf` ~500 segments for retransmit checks). At 50k+ pkt/s that is ~30M
`snd_buf` iterations/s, inflating per-segment cost to ~680µs and driving a
fast/early retransmit storm (~20K/2s).

**Effective fixes (kept):**

| Fix | Where | Result (256KB@RPS=300) |
|:----|:------|:----------------------|
| Batch input flush: `input_no_flush()` + `flush_if_pending()` (one deferred flush per recv burst, single lock) | kcp.rs, conn.rs | **3183ms → 2.1ms**, 100% ok (was 87%), retrans storm → 0 |
| Flush loop trusts `kcp.flush()` return (clamp 1..10ms) instead of forcing 1ms | conn.rs | flush-loop churn ~5-10× lower |
| P3: nodelay window-probe interval 500→50ms (`IKCP_PROBE_INIT_NODELAY`) | kcp.rs | collapse-edge recovery 947ms → 47ms at RPS=250 |
| Same batch-flush applied to the **legacy binary sessions** (`input_no_flush` + one `flush_if_pending` per datagram FEC group) | kcptun-client, kcptun-server | 256KB@RPS=300: p50 12.4→10.5ms (legacy tunnel doesn't collapse — 1024-window + FEC + SMUX buffering keep it below the cliff) |
| Gate fast/early retransmit on `new_segs_count > 0` — only retransmit when the window can carry new data; a fastack on an in-flight segment under a full window is usually a DELAYED ACK, not loss | kcp.rs | RPS=300 p99 69ms→3.9ms; RPS≤450 clean (~2.3ms) |
| `write_notify` → `notify_one()` (permit-storing) — the old `notify_waiters` lost wakes that landed before the waiter registered, forcing 10ms fallbacks under load | conn.rs | RPS=475 clean 2.3ms (was 539ms collapse); RPS=500 p50 500ms+→~100ms |

**Tunnel comparison (why the raw lib's extreme-load queueing isn't a lib defect):**
the same `kcp_rs::KcpStream` sustains **256KB@RPS=500 at ~11ms, 100% ok** when used
the way the product uses it (the default shared-session tunnel, `copy_bidirectional`
two-task per conn) — vs Go tunnel 30.5ms. The raw benchmark's residual RPS=500
deep-queue is the single-task serial-echo worst case at 131 MB/s on one
connection; the tunnel's SMUX/TCP layers decouple read from write. macOS exposes
no public `sendmmsg`/`recvmmsg` (libSystem has no symbols), so batch I/O there
would require raw syscalls (not implemented).

Rust now sustains 300 RPS of 256KB at ~2.1ms — **~9× faster than Go's 19ms**.
Wire format unchanged; verified by Go↔Rust interop (both directions 500/500).

**Ineffective approaches tested and reverted** (recorded so they aren't
re-attempted):

| Approach | Outcome |
|:---------|:--------|
| Asymmetric window (rcv_wnd=2048) | No change — **disproved wnd=0 deadlock as the primary cause** (Go uses 512/512 too) |
| Suppress retransmit when `rmt_wnd==0` | No change — `rmt_wnd` stays >0 during the collapse |
| Disable fast/early retransmit entirely | 4× worse — retransmits were recovering real packet loss |
| ackOnly input flush (skip snd_buf scan) | 5× worse — the data-recovery half of flush is load-bearing |
| Drain-first recv loops (+ `yield_now`) | deadlock — the reactor wait was the implicit yield |
| Listener-reader batch drain | 3.7× worse |

---

## 🧪 Testing Rigor

| Test Type | Count | What It Verifies |
|:----------|:-----:|:-----------------|
| Unit tests | 334 | Individual crate correctness |
| E2E interop | 68 | Go↔Rust bidirectional compatibility |
| Stress tests | 8 | Byte-for-byte data integrity at scale |
| Clippy | `-D warnings` | Zero warnings enforced |
| Fmt | `cargo fmt --check` | Consistent formatting |

---

## 📋 CLI Options

### kcptun-client

| Flag | Default | Description |
|:-----|:--------|:------------|
| `-l` / `--localaddr` | `:12948` | Local listening address |
| `-r` / `--remoteaddr` | (required) | KCP server address |
| `--key` | `it's a secrect` | Pre-shared secret |
| `--crypt` | `aes` | Encryption algorithm |
| `--mode` | `fast` | KCP mode |
| `--conn` | `1` | Number of UDP connections |
| `--mtu` | `1350` | Maximum transmission unit |
| `--sndwnd` | `1024` | Send window |
| `--rcvwnd` | `1024` | Receive window |
| `--datashard` | `0` | FEC data shards |
| `--parityshard` | `0` | FEC parity shards |
| `--ratelimit` | `0` | Rate limit (bytes/sec) |
| `--nocomp` | `false` | Disable Snappy compression |
| `--smuxver` | `2` | SMUX version (1 or 2) |
| `--keepalive` | `10` | Keepalive interval (sec) |
| `--QPP` | `false` | Enable QPP obfuscation |

### kcptun-server

| Flag | Default | Description |
|:-----|:--------|:------------|
| `-l` / `--listen` | `:29900` | KCP listen address |
| `-t` / `--target` | (required) | TCP target address |
| `--shards` | `0` (auto: per-CPU on Linux, 1 elsewhere) | SO_REUSEPORT shard sockets — each shard is its own socket + worker thread (see the Latency Tuning Guide below) |
| `--key` | `it's a secrect` | Pre-shared secret |
| `--crypt` | `aes` | Encryption (same as client) |
| `--mode` | `fast` | KCP mode |

---

## 💡 Why Rust?

- **Memory safety** — no dangling pointers, no buffer overflows, no use-after-free
- **Zero-cost abstractions** — enum dispatch eliminates vtable overhead on hot crypto paths
- **True parallelism** — `std::thread::scope` for parallel batch encryption, no GIL
- **Compile-time guarantees** — the borrow checker catches data races before they happen
- **ARM ecosystem** — Rust is a first-class citizen on aarch64, with hardware AES available (`aes_armv8`)
- **Small binaries** — stripped release binary is ~2 MB, far smaller than Go's statically linked blobs
- **Cross-compilation** — build ARM Linux binaries from macOS with a single `make` command

---

## 📝 License

MIT — see [LICENSE](LICENSE) for details.

This is a Rust port of [kcptun](https://github.com/xtaci/kcptun) by [xtaci](https://github.com/xtaci).  
Source: [github.com/xsean2020/kcptun-rs](https://github.com/xsean2020/kcptun-rs)

---

<div align="center">

**If you find this project useful or impressive, please ⭐ star it on GitHub!**

*Built with Rust, driven by curiosity, validated by benchmarks.*

</div>
