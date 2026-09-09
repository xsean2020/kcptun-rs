<!-- Generated: 2026-09-01 | Regenerated from source analysis (ignore-history rule): 9 crates, tokio-only -->

# kcptun-rs

## Purpose

Rust port of [xtaci/kcptun](https://github.com/xtaci/kcptun) — KCP-based TCP stream accelerator with SMUX multiplexing, Reed-Solomon FEC, Snappy compression, and wire-compatible ciphers. **Vibe Coding** experiment targeting **full Go kcptun / kcp-go v5 wire compatibility**, not production software. Single async backend: **tokio** (via the `knet` facade).

Protocol stack (bottom → top):

```
UDP/raw TCP → BlockCrypt/AEAD (+ optional QPP) → (+ optional FEC) → KCP ARQ → Snappy (session-level) → SMUX Session → SMUX Stream → TCP
```

## Workspace (9 crates)

| Crate | Purpose | Notes |
|-------|---------|-------|
| `qpp-rs` | Quantum Permutation Pad stream obfuscation | no internal deps |
| `knet-rs` (lib `knet`) | tokio I/O facade: `TcpStream`, `UdpSocket`, `DatagramSocket` (Linux mmsg), tcpraw (raw IP), `spawn_task`, `cpu_block`, `sleep_ms`, `block_on` | mmsg/tcpraw compile on Linux only (macOS stubs). `raw_udp` has a Windows fallback using `std::net::UdpSocket`. |
| `kpprof-rs` | Go-compatible pprof HTTP server + `ProfilingAllocator` | default binary feature `pprof`. **pprof-rs (CPU/heap profiling) is gated to `cfg(unix)`** — on Windows, the HTTP server runs but CPU profile returns 501 and heap/allocs return empty. `ProfilingAllocator` works cross-platform (raw address capture); only symbolization + Go pprof protobuf encoding are Unix-only. |
| `kcrypt-rs` | 13 BlockCrypt + AES-128-GCM + wire packing (`CryptoBuf`/`encrypt_batch`) | no crypto lives in kcp-rs. AES-CFB picks its backend at construction: hardware AES (AES-NI/ARMv8) → `aes` crate; otherwise Go-style T-table soft AES (`crypt/aes_soft.rs`) — CFB can't amortize fixslice batching (3.6× slower per block). Soft tables are NOT constant-time, same posture as Go's `crypto/aes` fallback. |
| `kcp-rs` | KCP ARQ state machine, FEC, SNMP; feature `async` adds `KcpStream`/`KcpListener`/`PacketTransport` | deps: knet-rs (optional) |
| `smux-rs` | SMUX v1/v2; `SmuxConn` Builder, `Session`, `Arc<Stream>` | deps: knet-rs |
| `kcptun-common` | Production stack: `KcptunConfig`, `CryptoTransport`, `kcp_transport`, `KcptunSession`, `KcptunListener`, Snappy pipes, ratelimit, snmp_log | deps: kcp-rs + kcrypt-rs + smux-rs |
| `kcptun-client` | Client binary (`app.rs` parses CLI inside `async_main`) | default = `["tokio","qpp","pprof"]` |
| `kcptun-server` | Server binary (CLI parsed in `main`) + stress tests | same defaults |

Dependency graph:

```
kcptun-client ──┐
                ├─► kcptun-common ─► kcp-rs ─► knet-rs(optional "async")
kcptun-server ──┤        ├─► kcrypt-rs
                ├─► kcrypt-rs
                ├─► smux-rs ─► knet-rs
                ├─► qpp-rs
                └─► kpprof-rs ─► knet-rs
```

Feature cascade: binary `tokio` ⇒ `kcptun-common/tokio` ⇒ `kcp-rs/async` + `smux-rs/tokio` + `knet-rs`. Keep all three in sync when editing binary Cargo.toml.

## Key Files

| File | Description |
|------|-------------|
| `Cargo.toml` | Workspace root; release: `opt-level=3`, LTO, `panic=abort`, strip; `profiling` profile |
| `Makefile` | `build/release/test/stress/e2e/clippy/fmt/gate/bench/profile*/release-armv7/release-arm64/check-deps` |
| `test_e2e.sh` | Go↔Rust interop matrix (needs Go bins in `tests/kcptun-go/`) |
| `publish_crates.sh` | crates.io release entrypoint; dry-run default, `--execute` uploads |
| `bench_rust_vs_go.py` / `bench_results.json` | Throughput harness + latest numbers |
| `docs/kcp-rs-optimization-2026-09-01.md` | Evidence-based kcp-rs optimization plan + dead-code audit record |
| `bugs/` | Bug reports & postmortems only |
| `.cargo/config.toml` | aarch64 `aes_armv8` rustflags |

## Commands

```bash
make build / release          # debug / release (LTO, stripped)
make test                     # unit tests
make stress                   # needs release build first
make e2e                      # Go↔Rust interop — REQUIRES user confirmation first
make clippy                   # -D warnings (kcptun-server/tests currently carries pre-existing lint debt)
make fmt
make bench / profile / profile-rust-go / profiling-bins
make release-armv7 / release-arm64
./publish_crates.sh [--execute]
```

## Architecture Summary

### Wire formats (hard constraint: stay Go-compatible)

| Layer | Layout |
|-------|--------|
| CFB crypto | `[nonce 16B][CRC32 4B][payload]` — fixed IV `GO_CFB_IV` |
| AES-GCM | `[nonce 12B][ciphertext+tag 16B]` |
| `null` cipher | **no** crypto header (unlike `none`) |
| KCP segment | 24B LE: `conv\|cmd\|frg\|wnd\|ts\|sn\|una\|len` |
| FEC header | 6B: `seqid(4) + type(2)`; `0x00f1` data / `0x00f2` parity |
| SMUX frame | 8B: `ver\|cmd\|length(2 LE)\|stream_id(4 LE)` |
| Key derive | PBKDF2-HMAC-SHA1, salt `b"kcp-go"`, 32-byte key |

### kcp-rs internals (2026-09 state)

- `kcp.rs` — KCP state machine (Go kcp-go v5 port): `send`/`input_no_flush`/`flush_with_current`; `acklist: SmallVec` ; `pending_flush` batches one flush per datagram burst; cached monotonic clock (`current_ms`).
- `conn.rs` (module) — async `KcpStream`: single `parking_lot::Mutex<KCP>`, `RawPacketQueue` double-buffer, inline send bypassing the flush loop, absolute-deadline flush loop (idle connections park with zero timers), `process_inbound_batch` (FEC decode outside the KCP lock, one lock per burst, read prefetch). Split engine/facade (no behavior change): `conn/raw_queue.rs` (wire-packet FIFO + read prefetch), `conn/endpoint.rs` (`SharedIoState` + input/flush loops — the user-space `struct sock`), `conn/halves.rs` (AsyncRead/Write + split halves); `conn.rs` keeps the `KcpStream` facade + builder. All `kcp_rs::conn::*` paths unchanged.
- `sharded.rs` — `KcpListener` with three topologies: **direct single worker** (`worker_count==1`, any platform — the worker recvmmsg-drains the socket itself), **direct reuseport group** (Linux fresh bind, N>1 — one SO_REUSEPORT socket per worker, kernel 4-tuple hash = affinity, no reader/channel), **reader pipeline** (shared socket + N>1: RX thread recvmmsg → tokio-aware `async_channel` per worker, FNV affinity). All workers are OS threads with own current-thread runtimes running decrypt→KCP→encrypt via `feed_raw_batch` (`background_input(false)`); idle park shares the driver epoll with flush timers; `close()` wakes direct workers via `knet::CancellationToken` race. `PeerQueue` pop side is legacy (nothing pushes in production).
- `fec.rs` — RS FEC encoder/decoder + `AutoTune`; decoder uses `MAX_SHARD_SETS=3` tombstone eviction.
- `snmp.rs` — Go-CSV-compatible counters, gated by `SNMP_ENABLED` (zero cost when off). Rust-only extras: `write_inline_sends`, `write_flush_sends`, `read_fallback_timeout`.
- `transport.rs` — `PacketTransport` trait (`#[async_trait]`); impls: `knet::DatagramSocket`, `kcptun_common::CryptoTransport`, `PeerTransport` (per-peer server stream).
- `listener.rs` — `KcpTcpListener`: a TCP **transport factory** (each accepted raw-TCP conn → one connected `KcpStream` via `TcpRaw` `PacketTransport`), NOT a demux listener — the real demux/accept listener is `sharded.rs`'s `KcpListener`. Linux-only library surface, not used by the binaries (one test).

### Binary session path

Both binaries use the shared `kcptun_common` stack; binaries do not run their own KCP/FEC/Snappy/SMUX loops.

```
client: TCP accept → KcptunSession::connect → CryptoTransport(+QPP) → KcpStream → SnappyPipe → SMUX → pipe to local TCP
server: shared UDP → KcptunListener demux (workers) / raw TCP per-peer → KcptunSession::serve_transport → SMUX → forward to target
```

Snappy is **session-level**, on by default (`--nocomp` disables). `--key`, `--crypt`, `--mode`, `--nocomp` must match client and server.

## For AI Agents

### Rules

- Wire compatibility with Go kcptun / kcp-go v5 is the hard constraint; do not change any layout in the table above without an e2e plan.
- New bug write-ups go under `bugs/` only. After structural/public-API changes, update the nearest AGENTS.md or state "no AGENTS sync needed".
- Do not create `AGENTS.md` under `tests/` or `kcptun-server/tests/`.
- Business code uses `knet::*` only — never raw tokio in new code paths.
- kcp-rs is a published crate (see `publish_crates.sh`): removing/renaming `pub` items is a breaking change — bump the version or add a deprecation shim.

### Testing

- `cargo test --workspace` / `make test`; kcp-rs integration tests need `--features async` (or `--all-features`).
- Clippy gate: `make clippy` (`-D warnings`). Known pre-existing debt: `kcptun-server/tests/stress_test.rs` (21 lints) and `test_fast_retransmit_fires_on_duplicate_acks` (fails on current branch, user WIP).
- **E2E requires explicit user confirmation.** Never start `test_e2e.sh` / `make e2e` proactively; recommend it and ask.
- `make stress` after flush/lock/session changes.

### Common Patterns

- Global allocator: `mimalloc` in both binaries.
- Crypto selection: `kcrypt_rs::select_block_crypt` / `select_aead_crypt`; packet packing via `kcrypt_rs::wire::CryptoBuf` + `encrypt_batch`.
- SNMP logging: `kcptun_common::snmp_log::snmp_logger` writes the Go CSV plus a Rust-only sidecar `<path>.rustobs` (`timestamp,WriteInlineSends,WriteFlushSends`).

## Dependencies

External deps are fetched from crates.io per platform (not vendored). Notable: `bytes`, `parking_lot`, `crossbeam-channel`, `reed-solomon-erasure`, `aes`/`aes-gcm`, `snap`, `clap`, `mimalloc`, optional `pprof`.

<!-- MANUAL: Any manually added notes below this line are preserved on regeneration -->

<!-- gitnexus:start -->
# GitNexus — Code Intelligence

This project is indexed by GitNexus as **kcptun-rs** (6212 symbols, 13722 relationships, 300 execution flows). Use the GitNexus MCP tools to understand code, assess impact, and navigate safely.

> Index stale? Run `node .gitnexus/run.cjs analyze` from the project root — it auto-selects an available runner. No `.gitnexus/run.cjs` yet? `npx gitnexus analyze` (npm 11 crash → `npm i -g gitnexus`; #1939).

## Always Do

- **MUST run impact analysis before editing any symbol.** Before modifying a function, class, or method, run `impact({target: "symbolName", direction: "upstream"})` and report the blast radius (direct callers, affected processes, risk level) to the user.
- **MUST run `detect_changes()` before committing** to verify your changes only affect expected symbols and execution flows. For regression review, compare against the default branch: `detect_changes({scope: "compare", base_ref: "master"})`.
- **MUST warn the user** if impact analysis returns HIGH or CRITICAL risk before proceeding with edits.
- When exploring unfamiliar code, use `query({query: "concept"})` to find execution flows instead of grepping. It returns process-grouped results ranked by relevance.
- When you need full context on a specific symbol — callers, callees, which execution flows it participates in — use `context({name: "symbolName"})`.

## Never Do

- NEVER edit a function, class, or method without first running `impact` on it.
- NEVER ignore HIGH or CRITICAL risk warnings from impact analysis.
- NEVER rename symbols with find-and-replace — use `rename` which understands the call graph.
- NEVER commit changes without running `detect_changes()` to check affected scope.

## Resources

| Resource | Use for |
|----------|---------|
| `gitnexus://repo/kcptun-rs/context` | Codebase overview, check index freshness |
| `gitnexus://repo/kcptun-rs/clusters` | All functional areas |
| `gitnexus://repo/kcptun-rs/processes` | All execution flows |
| `gitnexus://repo/kcptun-rs/process/{name}` | Step-by-step execution trace |

## CLI

| Task | Read this skill file |
|------|---------------------|
| Understand architecture / "How does X work?" | `.claude/skills/gitnexus/gitnexus-exploring/SKILL.md` |
| Blast radius / "What breaks if I change X?" | `.claude/skills/gitnexus/gitnexus-impact-analysis/SKILL.md` |
| Trace bugs / "Why is X failing?" | `.claude/skills/gitnexus/gitnexus-debugging/SKILL.md` |
| Rename / extract / split / refactor | `.claude/skills/gitnexus/gitnexus-refactoring/SKILL.md` |
| Tools, resources, schema reference | `.claude/skills/gitnexus/gitnexus-guide/SKILL.md` |
| Index, status, clean, wiki CLI commands | `.claude/skills/gitnexus/gitnexus-cli/SKILL.md` |

<!-- gitnexus:end -->
