<!-- Generated: 2026-07-20 | Updated: 2026-07-20 -->

# kcptun-rs

## Purpose

Rust port of [xtaci/kcptun](https://github.com/xtaci/kcptun) — a KCP-based TCP stream accelerator with SMUX multiplexing, Reed-Solomon FEC, Snappy compression, and 13 wire-compatible ciphers. This is a **Vibe Coding** experiment targeting **full Go kcptun / kcp-go v5 wire compatibility**, not production software. Dual async backends: **tokio** (default, high-concurrency) and **smol** (lightweight / ARM).

Protocol stack (bottom → top):

```
UDP → BlockCrypt/AEAD (+ optional FEC) → KCP ARQ → Snappy (session-level) → SMUX Session → SMUX Stream (+ optional QPP) → TCP
```

## Key Files

| File | Description |
|------|-------------|
| `Cargo.toml` | Workspace of 7 crates; release profile: `opt-level=3`, LTO, `panic=abort`, strip |
| `Makefile` | Build/test/clippy/bench for tokio & smol; ARMv7/ARM64 cross-compile; vendor |
| `CLAUDE.md` | AI behavioral rules + project gotchas (authoritative for agent work) |
| `README.md` / `README.zh.md` | User-facing docs (EN/ZH) |
| `CHANGELOG.md` | Keep-a-Changelog history of perf/interop work |
| `test_e2e.sh` | Go↔Rust (tokio & smol) interop matrix (crypt, mode, smuxver, nocomp, FEC) |
| `bench_rust_vs_go.py` | Throughput comparison harness |
| `bench_results.json` | Latest 3-way bench numbers (Go / Rust-tokio / Rust-smol) |
| `PERF_OPTIMIZATION_PLAN.md` | **最终性能方案定稿**：已完成 P0/P1、KPI、剩余 R1–R10、验收纪律 |
| `bench/profile_flamegraph.sh` | samply/flamegraph capture for L1–L4 loads |
| `bench/profile_rust_go_pprof.sh` | Rust CPU profile as Go pprof protobuf (`go tool pprof`) |
| `.claude/skills/flamegraph-perf/` | Agent skill: profile → optimize → verify |
| `BUGREPORT.md` | Known issues / investigation notes |
| `DISCLAIMER.md` | Legal / non-production disclaimer |
| `.cargo/config.toml` | Vendored crates-io (`vendor/`) + aarch64 `aes_armv8` rustflags |

## Subdirectories

| Directory | Purpose |
|-----------|---------|
| `kcp-rs/` | KCP ARQ state machine, FEC, CryptoBuf, SNMP (see `kcp-rs/AGENTS.md`) |
| `kcrypt-rs/` | 13 BlockCrypt + AES-128-GCM (see `kcrypt-rs/AGENTS.md`) |
| `smux-rs/` | SMUX v1/v2 multiplexer over async transport (see `smux-rs/AGENTS.md`) |
| `qpp-rs/` | Quantum Permutation Pad stream obfuscation (see `qpp-rs/AGENTS.md`) |
| `kio-rs/` | Runtime-agnostic async I/O (tokio \| smol) (see `kio-rs/AGENTS.md`) |
| `kcptun-client/` | Client binary (see `kcptun-client/AGENTS.md`) |
| `kcptun-server/` | Server binary + stress tests (see `kcptun-server/AGENTS.md`) |
| `bench/` | Shell/Python bench runners (see `bench/AGENTS.md`) |
| `tests/` | Go kcptun reference binaries for e2e (see `tests/AGENTS.md`) |
| `vendor/` | Vendored third-party crates (do not edit by hand; `make vendor`) |

## Commands

```bash
# Build
make build              # debug, tokio (default)
make build-smol         # debug, smol → target/smol
make release            # release, tokio
make release-smol       # release, smol → target/smol-release

# Test
make test / test-smol / test-both
make stress             # needs release build first
make e2e                # Go↔Rust interop (tokio + smol); needs Go binaries in tests/kcptun-go/
bash test_e2e.sh        # same, without auto-build

# Lint / format
make clippy / clippy-smol / clippy-both   # -D warnings
make fmt

# Bench (3-way: Go vs Rust-tokio vs Rust-smol)
make bench

# Cross (ARM defaults to smol)
make release-armv7 / release-arm64
make install-cross      # rustup targets

# Vendor refresh
make vendor
```

Runtime-feature packages (`RT_PKGS`): `kcptun-client`, `kcptun-server`, `kio-rs`, `smux-rs`.  
Runtime-agnostic: `kcp-rs`, `kcrypt-rs`, `qpp-rs`.

## Architecture Summary

### Crate dependency graph

```
kcptun-client ──┐
                ├──► kcp-rs ──► kcrypt-rs
kcptun-server ──┤
                ├──► smux-rs ──► kio-rs  (feature: tokio | smol)
                ├──► qpp-rs
                └──► kio-rs
```

### Wire formats (must stay Go-compatible)

| Layer | Layout |
|-------|--------|
| CFB crypto | `[nonce 16B][CRC32 4B][payload]` — fixed IV `GO_CFB_IV` |
| AES-GCM | `[nonce 12B][ciphertext+tag 16B]` |
| `null` cipher | **no** crypto header (unlike `none`) |
| KCP segment | 24B LE: `conv\|cmd\|frg\|wnd\|ts\|sn\|una\|len` |
| FEC header | 6B: `seqid(4) + type(2)`; types `0x00f1` data / `0x00f2` parity |
| SMUX frame | 8B: `ver\|cmd\|length(2 LE)\|stream_id(4 LE)` |
| Key derive | PBKDF2-HMAC-SHA1, salt `b"kcp-go"`, 32-byte key |

### Dual runtime model

- Feature flags: `default = ["tokio"]` or `--no-default-features --features smol`
- `tokio` and `smol` are **mutually exclusive** (`compile_error!` if both)
- Native default: tokio; ARM Makefile targets default to smol
- Business code uses `kio::*` only — never raw tokio/smol in new code paths

### Binary hot path (flush loop)

Both client (`KcpConn`) and server (`KcpServerSession`) split flush into **4 phases** to minimize KCP mutex hold time:

1–3. Outside lock: snappy (via `kio::cpu_block`), crypto prepare, optional parallel encrypt (`std::thread::scope` when ≥4 packets)
4. Hold lock briefly: `kcp.send() / update() / flush()`

Pipe buffers: **64 KB** (matching Go; not tokio's 8 KB default).

### Performance notes (as of Unreleased)

- Client + server: `Arc<dyn BlockCrypt/AeadCrypt>`; shared `encrypt_batch` / `should_cpu_block_encrypt`
- P0/P1 主体 + write backpressure + SMUX v2 peer_window 已落地；bulk 吞吐约 **1.2–1.35× Go**
- CFB monomorphized; AEAD `seal_into`; UDP `send_batch`; SMUX `VecDeque<Bytes>` send
- smol: persistent blocking pool; Global allocator: `mimalloc`
- **定稿方案与剩余项（R1 CryptEngine 等）:** `PERF_OPTIMIZATION_PLAN.md`

## For AI Agents

### Auto-orientation (see also `CLAUDE.md`)

`CLAUDE.md` instructs agents to **read this tree before full-repo scans**. Session start → root `AGENTS.md`; crate work → that crate's `AGENTS.md`. Update the nearest AGENTS.md when structure/API ownership changes.

### Working In This Directory

1. **Read `CLAUDE.md` gotchas before touching protocol code.** Wire compatibility > Rust idioms.
2. **Surgical changes only** — do not "clean up" kcp-rs control flow or clippy allows.
3. Prefer `kcrypt-rs` over re-exports from `kcp-rs` for new crypto deps.
4. All async I/O goes through `kio-rs`. New runtime-specific code belongs under `kio-rs/src/{net,sync,task,time}/`.
5. Client and server share patterns (`derive_key`, `apply_mode`, Snappy framing, QPPPort) but live as separate large `main.rs` files (~2k–2.5k LOC each) — keep them in sync when fixing shared bugs.
6. After protocol/crypto changes, run interop: `bash test_e2e.sh` and/or stress tests.
7. Never commit `tests/kcptun-go/{client,server}` binaries (gitignored); rebuild from Go source at `/Users/sean/Documents/kcptun` if needed.
8. Vendor is source of truth for deps offline; after adding crates run `make vendor`.

### Testing Requirements

| Change area | Minimum verification |
|-------------|----------------------|
| Any crate API | `cargo test --workspace` |
| Runtime paths | `make test-both` or `clippy-both` |
| Crypto / wire / KCP / SMUX | `bash test_e2e.sh` (all crypt modes if crypto) |
| Concurrency / integrity | `make stress` (release) |
| Perf claims | `make bench` + compare `bench_results.json` |
| Lint | `cargo clippy --workspace -- -D warnings` |

### Common Patterns

- **Modes** (`apply_mode`): `normal` / `fast` / `fast2` / `fast3` → nodelay/interval/resend/nc tuples matching Go
- **Snappy**: session-level framing with **CRC32C (Castagnoli)**, not IEEE `crc32fast` alone
- **Multi-port**: `IP:min-max` range parsing on client remote / server listen
- **Config**: clap CLI + optional JSON (`-c`); fields mirror Go flag names
- **SNMP**: periodic JSON/text stats logger task

### Critical Gotchas

- `--key`, `--crypt`, `--mode`, `--nocomp` **must match** client/server
- Compression **on by default** (`--nocomp=false`)
- `null` ≠ `none` (header presence)
- TEA: **8 rounds** (Go rounds/2); SM4: tjfoc/gmsm S-box + CK fix
- KCP: ACK every received Push; `snd_buf` front-pop on ACK in `flush()`
- Default cwnd cap 32 without `nocwnd` / large `sndwnd`
- Client KCP update interval **2 ms**; server **10 ms** (constants in each main)

## Dependencies

### Internal
All workspace crates as above.

### External (selected)
- `reed-solomon-erasure`, `bytes`, `parking_lot`, `crossbeam`, `snap`, `clap`, `serde`, `mimalloc`, `dashmap` (server)
- Crypto: `aes`, `aes-gcm`, `twofish`, `blowfish`, `des`, `pbkdf2`, `hmac`, `sha1`/`sha2`
- Runtime: `tokio` **or** `smol` + `async-io` + `async-executor` + `futures-lite`

## Line-count map (approx, 2026-07-20)

| Crate / binary | ~LOC |
|----------------|------|
| kcptun-server `main.rs` | 2589 |
| kcptun-client `main.rs` | 2040 |
| kcp-rs (total) | ~3400 |
| kcrypt-rs (total) | ~2400 |
| smux-rs (total) | ~1550 |
| kio-rs (total) | ~1300 |
| qpp-rs | ~500 |
| **Workspace Rust total** | **~14k** |

<!-- MANUAL: Any manually added notes below this line are preserved on regeneration -->
