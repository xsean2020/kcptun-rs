# kcptun-rs

English | [中文](README.zh.md)

## Disclaimer

> ⚠️ **This project is a Vibe Coding porting test, for educational purposes only.**
>
> This project is a **Vibe Coding** experiment — using AI-assisted programming to port an existing codebase. The core focus is exploring and validating the Vibe Coding workflow itself, not producing a production-grade software port. This is **not** production software. No guarantees are made regarding correctness, stability, or security.
>
> **It is strictly prohibited for any illegal use**, including but not limited to circumventing censorship, illegal data transmission, network attacks, etc. Any illegal activities by users are not related to this project or its author. Users bear all legal responsibilities.
>
> See [DISCLAIMER.md](DISCLAIMER.md) for the full disclaimer.

## About

**kcptun-rs** is a **Vibe Coding** porting test — an experiment in AI-assisted programming, using [kcptun](https://github.com/xtaci/kcptun) (a KCP-based TCP stream accelerator by [xtaci](https://github.com/xtaci)) as the reference target.

> kcptun is a stable & secure tunnel that transfers TCP over KCP with SMUX multiplexing,
> Forward Error Correction (FEC), and optional encryption via kcp-go.

The goal of this project is **not** to produce a production-grade kcptun replacement. Instead, it serves as a real-world test case for the **Vibe Coding** workflow — can AI-assisted programming produce a working, wire-compatible port of a complex networked system? The codebase, bug fixes, benchmarks, and test results documented here are the artifacts of that experiment.

## Features

- ✅ **Go kcptun compatible** — interoperable with the original Go implementation
- ✅ **Snappy compression** (session-level, matching Go kcptun architecture)
- ✅ **Multiple encryption backends**: AES-128, AES-192, AES-256, AES-128-GCM, SM4, XOR, TEA, XTEA, Salsa20, Blowfish, Twofish, CAST5, 3DES, or none
- ✅ **KCP protocol modes**: `normal`, `fast`, `fast2`, `fast3`
- ✅ **SMUX multiplexing** (v1/v2) — multiple TCP streams over a single KCP connection
- ✅ **FEC** (Reed-Solomon forward error correction)
- ✅ **QPP** (Quantum Permutation Pad) — optional post-quantum obfuscation layer (per-stream)
- ✅ **Multi-port** client dialer and server listener
- ✅ **Auto-expire** session scavenging
- ✅ **SNMP** statistics logging
- ✅ **JSON config** file support

## Quick Start

### Build

```bash
cargo build --release
```

Binaries are placed at:
- `target/release/kcptun-client`
- `target/release/kcptun-server`

### Run

**Server** (listen on UDP :29900, forward to local HTTP service):

```bash
./target/release/kcptun-server -t "127.0.0.1:8080" -l ":29900" --key "my-secret"
```

**Client** (listen locally on :12948, tunnel to remote server):

```bash
./target/release/kcptun-client -r "server-ip:29900" -l ":12948" --key "my-secret"
```

Now point your application at `127.0.0.1:12948`. TCP data is encrypted,
compressed, and accelerated over KCP to the remote server.

### With a config file

```bash
kcptun-server -c config.json
kcptun-client -c config.json
```

Example `config.json`:

```json
{
    "localaddr": ":12948",
    "remoteaddr": "vps:29900",
    "key": "my-secret",
    "crypt": "aes-128",
    "mode": "fast2",
    "conn": 2,
    "mtu": 1350,
    "sndwnd": 1024,
    "rcvwnd": 1024,
    "datashard": 10,
    "parityshard": 3,
    "nocomp": false,
    "smuxver": 2,
    "keepalive": 10
}
```

## CLI Options

### kcptun-client

| Flag | Default | Description |
|------|---------|-------------|
| `-l` / `--localaddr` | `:12948` | Local listening address |
| `-r` / `--remoteaddr` | (required) | KCP server address, e.g. `"IP:29900"` or `"IP:min-max"` for multi-port |
| `--key` | `it's a secrect` | Pre-shared secret |
| `--crypt` | `aes` | Encryption: `null`, `none`, `xor`, `aes`, `aes-128`, `aes-192`, `aes-128-gcm`, `sm4`, `tea`, `xtea`, `salsa20`, `blowfish`, `twofish`, `cast5`, `3des` |
| `--mode` | `fast` | KCP mode: `normal`, `fast`, `fast2`, `fast3` |
| `--conn` | `1` | Number of UDP connections |
| `--mtu` | `1350` | Maximum transmission unit |
| `--sndwnd` | `1024` | Send window (packets) |
| `--rcvwnd` | `1024` | Receive window (packets) |
| `--datashard` | `0` | FEC data shards |
| `--parityshard` | `0` | FEC parity shards |
| `--nocomp` | `false` | Disable Snappy compression |
| `--smuxver` | `2` | SMUX protocol version (1 or 2) |
| `--keepalive` | `10` | Keepalive interval (seconds) |
| `--autoexpire` | `0` | Auto-expire connections (seconds, 0=off) |
| `--QPP` | `false` | Enable Quantum Permutation Pad |
| `--QPPCount` | `61` | Pads for QPP (should be prime) |
| `-c` | — | Path to JSON config file |

### kcptun-server

| Flag | Default | Description |
|------|---------|-------------|
| `-l` / `--listen` | `:29900` | KCP listen address |
| `-t` / `--target` | (required) | TCP target address |
| `--key` | `it's a secrect` | Pre-shared secret |
| `--crypt` | `aes` | Encryption (same as client) |
| `--mode` | `fast` | KCP mode |
| `--nocomp` | `false` | Disable Snappy compression |

Other flags mirror the client options (mtu, sndwnd, rcvwnd, datashard, etc.).

> **Important:** `--key`, `--crypt`, `--mode`, and `--nocomp` **must match**
> between client and server. Compression is disabled by default only when
> `--nocomp` is set, matching the Go behavior.

## Architecture

### Workspace Structure

This is a Cargo workspace with 6 crates:

```
kcptun-rs/
├── kcp-rs/          — KCP reliable UDP transport protocol (lib)
├── kcrypt-rs/       — Shared block/AEAD cipher library (lib, extracted from kcp-rs)
│   └── src/crypt/   — One file per cipher: none, xor, aes_cfb, sm4, tea, xtea,
│                       salsa20, blowfish, twofish, cast5_crypt, triple_des, aes_gcm
├── smux-rs/         — SMUX stream multiplexer (lib)
├── qpp-rs/          — Quantum Permutation Pad encryption (lib)
├── kcptun-client/   — Client binary
└── kcptun-server/   — Server binary
```

The crypto ciphers live in `kcrypt-rs` and are re-exported by `kcp-rs` for backward
compatibility. New code should depend on `kcrypt-rs` directly.

### Protocol Stack

The protocol stack, from bottom to top:

```
┌─────────────────────────────────────────┐
│          TCP / UNIX Socket              │
├─────────────────────────────────────────┤
│           SMUX Stream (mux)             │
├─────────────────────────────────────────┤
│         SMUX Session (mux)              │
├─────────────────────────────────────────┤
│   Snappy Compression (session-level)    │  ← matching Go kcptun
├─────────────────────────────────────────┤
│   BlockCrypt / FEC / KCP (kcp-go)       │
├─────────────────────────────────────────┤
│            UDP / TCPraw                 │
└─────────────────────────────────────────┘
```

## Go Compatibility

This Rust port is designed to be **fully wire-compatible** with the original Go kcptun.
Key compatibility points:

| Feature | Status |
|---------|--------|
| KCP segment wire format (kcp-go v5) | ✅ |
| Crypto header (nonce 16B + CRC32 4B) | ✅ |
| Snappy compression (session-level) | ✅ |
| SMUX frame format (v1/v2) | ✅ |
| Key derivation (PBKDF2-HMAC-SHA1) | ✅ |
| FEC header format | ✅ |
| Multi-port addressing | ✅ |
| QPP obfuscation (stream-level) | ✅ |
| SM4 block cipher (GB/T 32907) | ✅ (tjfoc/gmsm S-box + CK fix) |
| CAST5 (CAST-128) | ✅ (Full RFC 2144 implementation, Go-compatible) |

### Bug Fix: CRC32C Mismatch in Snappy Framing

**Root cause:** The Snappy framing specification requires **CRC32C (Castagnoli polynomial
0x1EDC6F41)** for chunk checksums, but the original implementation used
`crc32fast::hash()` which computes **CRC32/IEEE (polynomial 0x04C11DB7)**.

This caused Go kcptun servers to silently reject compressed data from the Rust
client — Go's `snappy.NewReader` validates CRC32C and discards chunks with
mismatched checksums. Rust↔Rust communication was unaffected because the custom
`SnappyStreamDecoder` skipped CRC validation entirely.

**Fix:** Replaced the hand-written `compress_stream_data` (which manually wrote
Snappy framing chunks with IEEE CRC) with a persistent
`snap::write::FrameEncoder<Vec<u8>>` from the `snap` crate. The `snap` crate
correctly implements CRC32C (Castagnoli), matching Go's `golang/snappy`.

**Symptoms:**
- `Go server + Rust client (compression)` — client sends SYN, server accepts
  SMUX, but no data flows; client retransmits forever
- `Rust server + Go client (compression)` — same failure pattern
- All `--nocomp` tests passed (no CRC involved)

**Verified:** All 8 cross-product combinations now pass:

| Client | Server | nocomp | compress |
|--------|--------|--------|----------|
| Go     | Go     | ✅     | ✅       |
| Go     | Rust   | ✅     | ✅       |
| Rust   | Rust   | ✅     | ✅       |
| Rust   | Go     | ✅     | ✅       |

### Compression Architecture

Like Go kcptun, compression is applied at the **SMUX session level**, not per-stream:

```
Go:       TCP ↔ SMUX stream ↔ SMUX session ↔ SnappyComp ↔ KCP ↔ UDP
Rust-rs:  TCP ↔ SMUX stream ↔ SMUX session ↔ SnappyComp ↔ KCP ↔ UDP
```

This means compression wraps the entire multiplexed session as one continuous
Snappy framing stream, compatible with Go's `snappy.NewBufferedWriter` /
`snappy.NewReader` (`github.com/golang/snappy`).

Verified: Snappy framed output from Rust's `snap` crate is **byte-identical**
to Go's `snappy` package for the same input.

## Compression

Snappy compression is **enabled by default** (`--nocomp` = `false`), matching Go
kcptun's default behavior.

- Works at the **SMUX session level** — wraps and compresses all multiplexed
  stream data as one Snappy framing stream
- Compatible with Go's `github.com/golang/snappy` (NewBufferedWriter / NewReader)
- Batch-compresses pending SMUX Data frames before sending via KCP;
  decompresses on receipt before dispatching to SMUX
- Disable with `--nocomp` on both client and server

## Logging

Log output is controlled by the standard `RUST_LOG` environment variable.
By default, only `info` and above is shown — clean and production-ready.

### Log Levels

| Level | When to use | Example output |
|-------|-------------|-----------------|
| `error` | System failures (always shown) | `connection refused`, `decrypt failed` |
| `warn` | Runtime warnings (always shown) | `KCP send error`, `UDP send_to error` |
| `info` | Key operations (**default**) | `listening on :29900`, `stream 5 opened` |
| `debug` | Debugging (`RUST_LOG=debug`) | `SMUX UPD frames`, `flush backpressure`, `conv extraction` |
| `trace` | Per-packet tracing (`RUST_LOG=trace`) | `send_frame details`, `feed_data per-packet`, `SMUX hex dump` |

### Usage

```bash
# Default — only info/warn/error (clean output)
./target/release/kcptun-server -l :29900 -t 127.0.0.1:8080
./target/release/kcptun-client -l :12948 -r server:29900

# Debug — show debug-level logs
RUST_LOG=debug ./target/release/kcptun-server -l :29900 -t 127.0.0.1:8080

# Trace — show everything (very verbose, includes per-packet details)
RUST_LOG=trace ./target/release/kcptun-client -l :12948 -r server:29900

# Per-module control — different levels for different crates
RUST_LOG=kcptun_client=debug,kcp_rs=warn ./target/release/kcptun-client

# Server file logging (also respects RUST_LOG)
./target/release/kcptun-server -l :29900 -t 127.0.0.1:8080 --log /var/log/kcptun.log
```

### Performance Notes

- The flush loop only calls `flush()` explicitly when there is new data to send,
  avoiding unnecessary ACK/probe packet generation every 10ms (matching Go's
  `update()` scheduling).
- `send_frame` flushes KCP immediately after `kcp.send()` (matching Go's
  `WriteBuffers` with `writeDelay=false`), eliminating 10ms flush-loop latency.
- `poll_write` reads `wait_send()` directly from the KCP state machine for
  real-time backpressure, and uses `tokio::sync::Notify` for immediate wakeup
  when the window drains (flush loop calls `notify_waiters()` after each cycle
  and after ACK processing).
- High-frequency per-packet logs (`send_frame`, `feed_data`, `SMUX hex dump`)
  are at `trace` level — they won't impact performance unless explicitly enabled.

## Performance Benchmark: Rust vs Go

### Performance profiling (flamegraphs)

CPU sampling for the data plane on **macOS arm64** uses **samply** → Speedscope:

```bash
cargo install samply --locked
make release
bash bench/profile_flamegraph.sh all    # or: make profile
```

- Runbook: [`bench/PROFILE_RUNBOOK.md`](bench/PROFILE_RUNBOOK.md)
- Hotspot notes: [`bench/profiles/HOTSPOTS.md`](bench/profiles/HOTSPOTS.md)
- Agent skill: [`.claude/skills/flamegraph-perf/SKILL.md`](.claude/skills/flamegraph-perf/SKILL.md)

On Apple Silicon, release builds enable RustCrypto **`aes_armv8`** (see `.cargo/config.toml`) so AES-CFB uses hardware AES instead of the soft fixslice path.

See also [`PERF_OPTIMIZATION_PLAN.md`](PERF_OPTIMIZATION_PLAN.md) for residual optimization items and KPI gates.

**Go-compatible pprof (optional feature — off by default for smaller binaries):**

`--pprof` is gated behind the Cargo feature `pprof` (pulls in protobuf/backtrace and
adds ~0.5–0.7 MB). Default `make release` / `make release-armv7` builds do **not**
include it. Enable when you need HTTP CPU profiles:

```bash
# Build with pprof support (+ frame pointers help go tool pprof)
RUSTFLAGS="-C force-frame-pointers=yes" \
  cargo build --release -p kcptun-server -p kcptun-client --features pprof
# Or with the profiling profile (debug info, no LTO/strip):
RUSTFLAGS="-C force-frame-pointers=yes" \
  cargo build --profile profiling -p kcptun-server -p kcptun-client --features pprof

# Run with HTTP endpoint, then sample:
./target/release/kcptun-server ... --pprof 127.0.0.1:6060
bash bench/profile_rust_go_pprof.sh server 20
go tool pprof -http=127.0.0.1:0 bench/profiles/rust-server-aes-*.pb
```

Rust samples are encoded as Google pprof protobuf so the **Go toolchain** UI shows demangled Rust function names (not raw `0x` addresses).


Two benchmark scripts measure different aspects of performance:

- **`bench/run_bench.sh`** — bulk throughput (200 MB, single connection, AES-128-CFB,
  `--nocomp`, `mode=fast`, `sndwnd/rcvwnd=1024`, `smuxver=2`). Measures sustained
  transfer rate and RTT latency.
- **`bench_rust_vs_go.py`** — comprehensive cipher × compression matrix (10 concurrent
  connections × 1 MB per connection, `--mode fast --sndwnd 2048 --rcvwnd 2048`).
  Uses concurrent send+receive with 256 KB warmup. Results saved to `bench_results.json`.

### Test machine

| Item | Value |
|------|-------|
| Host | Mac mini (Macmini9,1) |
| Chip | Apple M1 (8 cores: 4P + 4E) |
| Memory | 8 GB |
| OS | macOS 26.3.1 (Build 25D771280a), Darwin 25.3.0 arm64 |
| Rust | rustc / cargo 1.92.0-nightly (2025-10-13) |
| Go | go1.25.5 darwin/arm64 |
| Go kcptun | `SELFBUILD` (`tests/kcptun-go/{client,server}`) |
| Rust kcptun-rs | `0.1.0` @ `99803fe` (release LTO+strip; default features **without** `pprof`) |
| AES on this host | RustCrypto `aes_armv8` enabled via `.cargo/config.toml` |
| Date | 2026-07-21 |

> Numbers below were collected on this machine only. Cross-host comparison is not
> meaningful without matching CPU/AES/memory/OS conditions.

### 3-Way Bulk Throughput (200 MB, AES-128-CFB, `--nocomp`)

Command: `BENCH_DATA_MB=200 BENCH_LATENCY_ITERS=50 bash bench/run_bench.sh`

| Path | Throughput (MB/s) | Latency (ms median RTT) | vs Go→Go |
|------|------------------:|------------------------:|---------:|
| Go → Go | 51.15 | 0.31 | 1.00× |
| **Rust-Tokio → Rust-Tokio** | **85.60** | **0.12** | **1.67×** |
| **Rust-Smol → Rust-Smol** | **108.06** | **0.13** | **2.11×** |
| Go → Rust-Tokio | 76.48 | 0.11 | 1.50× |
| Rust-Tokio → Go | 30.28 | 0.15 | 0.59× |

> Same-stack Rust paths are clearly faster than Go→Go on this M1 host. Cross-stack
> paths are limited by the slower peer (Rust→Go is the weak direction here).

### Comprehensive Cipher × Compression Matrix (10 conn × 1 MB)

Command: `python3 bench_rust_vs_go.py` (defaults: 10 connections, 1 MB each).

Tokio `null/no-comp` failed once at server start in this run (marked —); remaining
cells completed with 0 failed connections.

#### Without compression (`--nocomp`)

| Cipher | Tokio MB/s | Smol MB/s | Go MB/s | T/Go | S/Go |
|--------|-----------:|----------:|--------:|-----:|-----:|
| null | — | 37.6 | 40.8 | — | 0.92× |
| none | 47.7 | 35.3 | 36.0 | 1.33× | 0.98× |
| xor | 31.1 | 30.8 | 34.2 | 0.91× | 0.90× |
| aes-128 | 35.2 | 38.1 | 30.9 | 1.14× | 1.23× |
| aes-128-gcm | 37.8 | 40.7 | 33.6 | 1.12× | 1.21× |
| salsa20 | 33.7 | 27.8 | 33.5 | 1.00× | 0.83× |
| blowfish | 32.8 | 34.1 | 27.4 | 1.20× | 1.24× |
| twofish | 31.2 | 34.6 | 20.6 | 1.51× | 1.68× |
| cast5 | 29.7 | 31.1 | 30.3 | 0.98× | 1.03× |
| 3des | 13.7 | 14.0 | 12.2 | 1.12× | 1.15× |
| tea | 30.3 | 35.0 | 40.1 | 0.76× | 0.87× |
| xtea | 18.7 | 16.7 | 17.4 | 1.07× | 0.96× |
| **sm4** | **17.4** | **9.4** | **3.9** | **4.41×** | **2.40×** |

#### With compression (Snappy)

| Cipher | Tokio MB/s | Smol MB/s | Go MB/s | T/Go | S/Go |
|--------|-----------:|----------:|--------:|-----:|-----:|
| null | 32.9 | 36.2 | 35.8 | 0.92× | 1.01× |
| none | 34.7 | 34.5 | 30.9 | 1.12× | 1.12× |
| xor | 39.8 | 38.2 | 39.3 | 1.01× | 0.97× |
| aes-128 | 38.2 | 43.1 | 39.6 | 0.96× | 1.09× |
| aes-128-gcm | 34.0 | 43.6 | 37.5 | 0.91× | 1.16× |
| salsa20 | 34.7 | 37.9 | 27.3 | 1.27× | 1.39× |
| blowfish | 32.7 | 40.2 | 28.2 | 1.16× | 1.42× |
| twofish | 30.8 | 17.1 | 23.5 | 1.31× | 0.73× |
| cast5 | 32.3 | 28.7 | 32.1 | 1.00× | 0.89× |
| 3des | 15.7 | 13.6 | 12.0 | 1.31× | 1.13× |
| tea | 33.6 | 36.9 | 29.8 | 1.13× | 1.24× |
| xtea | 10.5 | 14.7 | 16.5 | 0.63× | 0.89× |
| **sm4** | **17.2** | **16.5** | **3.6** | **4.84×** | **4.64×** |

> On this M1 host, multi-conn matrix numbers are closer across stacks than the bulk
> single-stream path (window/scheduling noise is higher at 10×1 MB). **sm4** remains
> Rust’s largest relative win vs Go. Raw JSON: `bench_results.json`.

### Stress tests (data integrity)

Command: `cargo test --release -p kcptun-server --test stress_test -- --nocapture --test-threads=1`

**Result (2026-07-21, same machine): 8 passed, 0 failed in 51.97s**

| Test | Connections | Payload | Result |
|------|-------------|---------|--------|
| `test_single_connection_mixed_sizes` | 1 | 1B…64KB | ✅ byte-for-byte |
| `test_single_connection_1mb` | 1 | 1 MB | ✅ |
| `test_multithread_10_connections` | 10 | 256B | ✅ |
| `test_multithread_50_connections` | 50 | 255B | ✅ |
| `test_multithread_100_connections` | 100 | 1B + 4KB | ✅ |
| `test_multithread_large_data` | 100 | 64KB + 128KB | ✅ |
| `test_page_refresh_simulation` | 80 (3 waves) | 512B…128KB | ✅ |
| `test_snappy_compressible_data` | 1 | compressible patterns | ✅ |

### Optimization History


| Version                        | Throughput | Latency (avg) | vs Go   |
|--------------------------------|------------|---------------|---------|
| Before optimization            | 5.4 MB/s   | 0.210 s       | 1.41× slower |
| + Real-time `wait_send()`      | 6.2 MB/s   | 0.142 s       | 1.18× slower |
| + Immediate flush on send      | 7.0 MB/s   | 0.111 s       | 1.19× slower |
| + `Notify` + ACK notification  | 7.1 MB/s   | 0.114 s       | 1.15× slower |
| + BufferPool + `register_read_waker` | —   | —             | Eliminates ~60K allocs/s |
| + `block_in_place` + `spawn_blocking` batch encrypt | — | — | Reactor freed during CPU work |
| + Cipher key schedule fix (blowfish/twofish/3des/aes) | 0.0→3.0 MB/s (blowfish) | — | **100x** |

**Net improvement:** throughput +31% (5.4 → 7.1 MB/s for null cipher), latency
−46% (0.210 → 0.114 s). Blowfish 100x, Twofish 11x improvement from key schedule
bug fix + pre-computed lookup tables.

### Cipher Key Schedule Bug Fix

**Root cause:** `blowfish`, `twofish`, `triple_des`, and `aes_cfb` all called
`new_from_slice(&self.key)` inside the per-block encryption function, re-running
the full key schedule for every block. In CFB-8 mode (blowfish/3des), a 1350-byte
packet triggered 1350 key schedules. In CFB-16 mode (twofish/aes), ~85 key schedules.

**Fix:** Store the cipher instance in the struct, created once in the constructor.
For Twofish, additionally replaced the RustCrypto crate (v0.7.1, computes
`sbox()` + `gf_mult()` per block) with a custom implementation pre-computing
`s [4][256]u32` lookup tables (matching Go's approach).

| Cipher    | Before  | After   | Improvement |
|-----------|---------|---------|-------------|
| blowfish  | 0.0 MB/s | 3.0 MB/s | 100x       |
| twofish   | 0.4 MB/s | 4.5 MB/s | 11x        |
| 3des      | 2.5 MB/s | 3.3 MB/s | 32%        |
| aes-128   | 3.4 MB/s | 3.1 MB/s | ~same      |

### Zero-copy optimizations

| Path                          | Before                     | After                          |
|-------------------------------|----------------------------|--------------------------------|
| Encryption: per-packet alloc  | `vec![]` + `rand`          | Reusable `BytesMut` + counter  |
| Encryption: return to tokio   | `Vec` deep copy            | `Bytes` reference-counted       |
| SMUX: Frame payload           | `copy_from_slice`          | `split_to + slice` (zero-copy) |
| SMUX: `push_data`             | `extend_from_slice`        | `VecDeque<Bytes>` append       |
| Decrypt: return payload       | `.to_vec()`                | `Bytes` reference-counted       |
| KCP `recv` (single segment)   | `extend_from_slice`        | `split_to + freeze` (zero-copy) |
| Server session lookup         | Global `Mutex<HashMap>`    | `DashMap` (shard locks)         |

## Streaming & Connection Lifecycle

### Half-Close & FIN Handling

The SMUX layer implements proper half-close semantics matching Go kcptun:

1. **`poll_shutdown`** marks the stream as `local_closed` — the flush loop
   continues draining pending send data before sending a FIN frame.
2. **Flush loop** detects `local_closed && pending_send == 0 && !fin_sent`,
   encodes a FIN (cmd=1) SMUX frame, and sends it through KCP.
3. **`fin_sent` flag** prevents duplicate FIN frames from being sent on
   subsequent flush cycles.
4. **Remote FIN** marks the stream `remote_closed` and sets state to
   `FinReceived`, which causes `poll_read` to return EOF (0 bytes).
5. **`close()`** is only called when both sides have closed — it clears
   buffers and sets state to `Closed`.

### KCP Lock Contention Optimization

The flush loop is split into 4 phases to minimize KCP mutex hold time:

| Phase | Work | KCP Lock |
|-------|------|----------|
| 1 | Drain SMUX send buffers, collect FIN-pending streams | Not held |
| 2 | Encode SMUX PSH + FIN frames | Not held |
| 3 | Snappy compress (if enabled) | Not held |
| 4 | `kcp.send()` + `kcp.update()` + `kcp.flush()` | Held briefly |

This allows the UDP recv loop to feed data into KCP while the flush loop
prepares the next batch of frames, eliminating lock contention stalls
under high concurrency.

### KCP Protocol Fixes

Several critical bugs in the KCP implementation were fixed to enable
high-concurrency tunnel operation:

| Fix | Problem | Solution |
|-----|---------|----------|
| `nocwnd` flag | `nc=1` (no congestion control) was set but never checked — both code branches were identical, always limiting `cwnd` to `self.cwnd` (32) | Added `nocwnd` field; when true, bypass `self.cwnd` limit and use `min(snd_wnd, rmte_wnd)` directly |
| `snd_buf` cleanup | ACKed segments were never removed from `snd_buf` — the `retain()` call was in `flush()` but segments accumulated, blocking new segments from `snd_queue` | Added front-of-buffer cleanup in `flush()` matching Go's `k.snd_buf = k.snd_buf[1:]` |
| `rmte_wnd` init | Initial `rmte_wnd` was `KCP_DEFAULT_WND` (32), unnecessarily limiting the send window before the first remote window advertisement arrived | `set_rcv_wnd()` now also updates `rmte_wnd` to assume symmetric window sizes |
| UDP buffer size | macOS default UDP buffer (~42KB) was too small for 50+ concurrent connections | Both client and server now set 4MB send/receive buffers via `socket2` |
| `flush()` time init | `flush()` called from `send_frame()` before any `update()` left `self.current = 0`, causing incorrect `resendts` calculations | `flush()` now initializes `self.current` if it's still 0 |
| ACK generation | Receiver never queued ACKs for received Push segments — `self.acks` was never populated, so the sender never learned its data arrived, causing infinite retransmission and deadlock | `input()` now pushes `seg.sn` to `self.acks` for every received Push segment (including duplicates), matching Go kcp-go behavior |
| Retransmission flood | `flush_output()` retransmitted ALL segments in the window on every flush cycle (every 10ms), flooding the UDP socket and causing ACK loss → deadlock under high concurrency | Added `xmit` check: only transmit on first transmission (`xmit==0`), RTO expiry (`resendts <= current`), or fast retransmit (`fastack >= IKCP_FASTACK_LIMIT`) |
| `KCP_MAX_FRAG` overflow | When multiple SMUX streams had data, the flush loop concatenated all frames into one `kcp.send()` call. With 3+ streams × 60KB, total exceeded 128 × MSS ≈ 169KB, triggering `TooManyFragments` error and silently dropping data | Both client and server now split `send_data` into chunks of at most `(KCP_MAX_FRAG - 1) × MSS` bytes before calling `kcp.send()` |

### Resource Leak Fixes (FD exhaustion)

**Symptom:** `accept error: Too many open files (os error 24)` under high
connection churn — the process exhausts its file descriptor limit.

**Root cause:** SMUX streams were never removed from the session after
connections closed. The `handle_client` / `handle_stream` functions relied
on `poll_shutdown` being called by `copy_bidirectional`, but when the pipe
timed out or the remote side vanished without sending FIN, `poll_shutdown`
was never invoked. Streams accumulated indefinitely in the session's
`streams` HashMap, holding TCP sockets, send/recv buffers, and tokio task
resources.

**Fixes applied (client + server):**

| Fix | Problem | Solution |
|-----|---------|----------|
| Explicit stream close | After `pipe()` returns (normal, error, or timeout), `poll_shutdown` may not have been called | `handle_client` / `handle_stream` now explicitly calls `mark_local_closed()` + `mark_fin_sent()` after the pipe completes |
| Stream cleanup in flush loop | Closed streams were never removed from the SMUX session | Flush loop Phase 1a now removes streams where `is_local_closed() && is_remote_closed() && is_fin_sent()` |
| Pipe idle timeout | When `close_wait=0` (default), `copy_bidirectional` could hang forever if neither side closed | Default 5-minute idle timeout applied to all pipes; can be overridden with `--closewait` |
| `handled_streams` cleanup (server) | Server's `handled_streams` set tracked dispatched stream IDs but never removed entries | Phase 1a cleanup now also removes IDs from `handled_streams` when streams are removed from the session |

## Builds & Tests

### Build

```bash
# Release build (optimized, LTO, stripped) — pprof OFF by default
cargo build --release
# or
make release

# Debug build
cargo build
# or
make build

# Optional: include --pprof HTTP CPU profiler (larger binary)
cargo build --release -p kcptun-client -p kcptun-server --features pprof
```

**Cargo features** (`kcptun-client` / `kcptun-server`):

| Feature | Default | Notes |
|---------|---------|-------|
| `tokio` | yes | High-concurrency async runtime |
| `smol` | no | Lightweight runtime (`--no-default-features --features smol`) |
| `pprof` | no | `--pprof ADDR` HTTP CPU profiles for `go tool pprof`; omit for slim ARM builds |

Release profile optimizations (`Cargo.toml`):
- `opt-level = 3` — full optimization
- `lto = true` — link-time optimization across crates
- `codegen-units = 1` — better optimization at cost of compile time
- `panic = "abort"` — smaller binary, no unwind tables
- `strip = true` — strip debug symbols from binary

### Cross-Compilation

The Makefile provides cross-compilation targets for ARM platforms (e.g. Raspberry Pi,
OpenWrt routers, AWS Graviton). It auto-detects a glibc or musl C toolchain on `PATH`
so the same targets work on both Linux and macOS.

```bash
# List all supported build targets (shows the detected triple + compiler)
make targets

# Install Rust cross-compilation toolchains (glibc + musl, one-time)
make install-cross
# Then install a C cross-compiler (Makefile picks whichever is present):
#   macOS:  brew install filosottile/musl-cross/musl-cross
#   Debian: sudo apt install gcc-arm-linux-gnueabihf gcc-aarch64-linux-gnu

# ARMv7 (Raspberry Pi 2/3, OpenWrt, embedded Linux)
make release-armv7
# Binaries at: target/<triple>/release/{kcptun-client,kcptun-server}
# e.g. target/armv7-unknown-linux-musleabihf/release/ on macOS (musl-cross)
# Typical size (smol, no pprof, stripped, static musl): ~1.3M client / ~1.5M server

# ARM64 (Raspberry Pi 4/5, AWS Graviton, Apple Silicon Linux VM)
make release-arm64
# Binaries at: target/<triple>/release/{kcptun-client,kcptun-server}
# e.g. target/aarch64-unknown-linux-musl/release/ on macOS (musl-cross)
```

ARM cross builds use the **smol** runtime and leave **`pprof` disabled** so OpenWrt /
embedded images stay small. Do not pass `--features pprof` unless you need on-device
CPU profiling.

| Target | Detected triples | Typical Hardware |
|--------|------------------|------------------|
| `release-armv7` | `armv7-unknown-linux-gnueabihf` (glibc) or `…-musleabihf` (musl) | Raspberry Pi 2/3, OpenWrt, most ARM SBCs |
| `release-arm64` | `aarch64-unknown-linux-gnu` (glibc) or `…-musl` (musl) | Raspberry Pi 4/5, AWS Graviton, ARM servers |

Force a specific toolchain with make variables if needed:

```bash
make release-armv7 ARMV7_TARGET=armv7-unknown-linux-musleabihf \
                   ARMV7_CC=arm-linux-musleabihf-gcc \
                   ARMV7_CXX=arm-linux-musleabihf-g++ \
                   ARMV7_AR=arm-linux-musleabihf-ar \
                   ARMV7_LINKER=arm-linux-musleabihf-gcc
```

### Run Tests

```bash
# All tests
cargo test --all
# or
make test

# Multi-threaded stress tests (data integrity + concurrency)
make stress
# or
cargo test --release --package kcptun-server --test stress_test -- --nocapture --test-threads=1

# Specific concurrency level
cargo test --release --package kcptun-server --test stress_test -- test_multithread_100_connections -- --nocapture

# Snappy Go-Rust interop test
cargo test test_snappy_go_rust_interop -- --nocapture

# Go compatibility test (requires Go installed)
cd /tmp/kcptun && go test ./std/ -run TestCompStreamRoundTrip -v

# Comprehensive Go↔Rust e2e interop test (requires Go kcptun binaries)
bash test_e2e.sh

# Clippy (warnings = errors)
make clippy
```

### Stress Test Coverage

The stress tests verify **data integrity**, not just connection success:

| Test | Connections | Payload Sizes | Verification |
|------|-------------|---------------|--------------|
| `test_single_connection_mixed_sizes` | 1 | 1B, 10B, 100B, 1KB, 10KB, 64KB | Byte-for-byte echo check |
| `test_multithread_10_connections` | 10 | 256B each | Byte-for-byte echo check |
| `test_multithread_50_connections` | 50 | 255B each | Byte-for-byte echo check |
| `test_multithread_100_connections` | 100 | 1B + 4KB per connection | Byte-for-byte verification of both sizes |
| `test_multithread_large_data` | 100 | 64KB + 128KB per connection | Byte-for-byte verification of both sizes |

Each payload uses a deterministic pattern (`conn_id + offset ^ 0xA5`) so that
any data corruption, stream mixing, truncation, or loss is immediately detected
by comparing the echoed response against the original.

### Go↔Rust E2E Interop Test Results

The `test_e2e.sh` script tests Go↔Rust compatibility across all encryption
algorithms, KCP modes, SMUX versions, compression settings, and FEC parameters.

**Summary: 68 passed, 0 failed, 0 skipped**

#### Encryption Algorithm Compatibility (Go(version 20260101)↔Rust, `--nocomp`)

| Cipher | Go→Rust | Rust→Go | Notes |
|--------|---------|---------|-------|
| `null` | ✅ | ✅ | No encryption, no crypto header (fixed: null mode no longer strips header) |
| `none` | ✅ | ✅ | No encryption, with crypto header |
| `xor` | ✅ | ✅ | SimpleXOR with PBKDF2 key expansion |
| `aes-128` | ✅ | ✅ | AES-128-CFB |
| `aes-192` | ✅ | ✅ | AES-192-CFB |
| `aes` (aes-256) | ✅ | ✅ | AES-256-CFB (default) |
| `sm4` | ✅ | ✅ | Manual impl with tjfoc/gmsm S-box + CK fix |
| `tea` | ✅ | ✅ | Fixed: copy_from_slice panic + 8 rounds (Go rounds/2) |
| `xtea` | ✅ | ✅ | XTEA, ported from Go source |
| `salsa20` | ✅ | ✅ | Salsa20 with Go-compatible state matrix |
| `blowfish` | ✅ | ✅ | Blowfish-CFB |
| `twofish` | ✅ | ✅ | Twofish-CFB |
| `cast5` | ✅ | ✅ | Full RFC 2144 CAST-128 implementation (ported from Go cast5) |
| `3des` | ✅ | ✅ | TripleDES-CFB |
| `aes-128-gcm` | ✅ | ✅ | AEAD (fixed: FEC header offset + correct buffer length in conv extraction) |

#### KCP Mode Compatibility (Go↔Rust)

| Mode | Go→Rust | Rust→Go |
|------|---------|---------|
| `normal` | ✅ | ✅ |
| `fast` | ✅ | ✅ |
| `fast2` | ✅ | ✅ |
| `fast3` | ✅ | ✅ |

#### SMUX Version Compatibility (Go↔Rust)

| Version | Go→Rust | Rust→Go | Notes |
|---------|---------|---------|-------|
| SMUX v1 | ✅ | ✅ | Fixed: PSH frames now include .with_ver(smuxver) for v1 compatibility |
| SMUX v2 | ✅ | ✅ | Default, fully compatible |

#### Compression + Encryption (Go↔Rust)

| Cipher | Go→Rust + compress | Rust→Go + compress |
|--------|---------------------|---------------------|
| `aes-128` | ✅ | ✅ |
| `aes` | ✅ | ✅ |
| `sm4` | ✅ | ✅ | Fixed (CK constant + S-box) |
| `tea` | ✅ | ✅ | Fixed (copy_from_slice + rounds) |
| `blowfish` | ✅ | ✅ |
| `twofish` | ✅ | ✅ |
| `3des` | ✅ | ✅ |

#### FEC Compatibility (Go↔Rust)

| FEC | Go→Rust | Rust→Go |
|-----|---------|---------|
| 10/3 | ✅ | ✅ |
| 4/2 | ✅ | ✅ |

## License

MIT — see [LICENSE](LICENSE) for details.

This is a Rust port of [kcptun](https://github.com/xtaci/kcptun) by [xtaci](https://github.com/xtaci).
