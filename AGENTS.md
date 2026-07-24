<!-- Generated: 2026-07-22 | Updated: 2026-07-22 -->

# kcptun-rs

## Purpose

Rust port of [xtaci/kcptun](https://github.com/xtaci/kcptun) — a KCP-based TCP stream accelerator with SMUX multiplexing, Reed-Solomon FEC, Snappy compression, and 13 wire-compatible ciphers. **Vibe Coding** experiment targeting **full Go kcptun / kcp-go v5 wire compatibility**, not production software. Dual async backends: **tokio** (default, high-concurrency) and **smol** (lightweight / ARM).

Protocol stack (bottom → top):

```
UDP → BlockCrypt/AEAD (+ optional FEC) → KCP ARQ → Snappy (session-level) → SMUX Session → SMUX Stream (+ optional QPP) → TCP
```

## Key Files

| File | Description |
|------|-------------|
| `Cargo.toml` | Workspace of 8 crates; release: `opt-level=3`, LTO, `panic=abort`, strip; `profiling` profile for pprof |
| `Makefile` | Build/test/clippy/bench/e2e/profile for tokio & smol; ARMv7/ARM64 cross; vendor |
| `CLAUDE.md` | AI behavioral rules + project gotchas (authoritative for *how to work*) |
| `README.md` / `README.zh.md` | User-facing docs (EN/ZH) |
| `CHANGELOG.md` | Keep-a-Changelog history of perf/interop work |
| `test_e2e.sh` | Go↔Rust (tokio & smol) interop matrix (crypt, mode, smuxver, nocomp, FEC) |
| `bench_rust_vs_go.py` | Throughput comparison harness (Go / Rust-tokio / Rust-smol) |
| `bench_results.json` | Latest 3-way bench numbers |
| `PERF_OPTIMIZATION_PLAN.md` | Performance plan: completed P0/P1, remaining R1–R10, acceptance rules |
| `bench/profile_rust_go_pprof.sh` | Rust CPU profile as Go pprof protobuf (`go tool pprof`) |
| `.claude/skills/flamegraph-perf/` | Agent skill: Go pprof profile → optimize → verify |
| `.claude/skills/agents-md-orient/` | Agent skill: AGENTS.md-first orient + sync after structural changes |
| `.claude/skills/superpowers-sync/` | Agent skill: sync plan+spec to `docs/superpowers/` on task completion |
| `bugs/BUGREPORT.md` | Known issues (single-KCP deadlock / FIN history) |
| `bugs/BUGREPORT_PROXY_MEMORY_GROWTH.md` | **Open**: proxy SMUX stream leak → RSS growth |
| `DISCLAIMER.md` / `DISCLAIMER.zh.md` | Legal / non-production disclaimer |
| `.cargo/config.toml` | Vendored crates-io (`vendor/`) + aarch64 `aes_armv8` rustflags |

## Subdirectories

| Directory | Purpose |
|-----------|---------|
| `kcp-rs/` | KCP ARQ, FEC, CryptoBuf, SNMP (see `kcp-rs/AGENTS.md`) |
| `kcrypt-rs/` | 13 BlockCrypt + AES-128-GCM (see `kcrypt-rs/AGENTS.md`) |
| `smux-rs/` | SMUX v1/v2 multiplexer (see `smux-rs/AGENTS.md`) |
| `qpp-rs/` | Quantum Permutation Pad stream obfuscation (see `qpp-rs/AGENTS.md`) |
| `kio-rs/` | Runtime-agnostic async I/O tokio\|smol (see `kio-rs/AGENTS.md`) |
| `kpprof-rs/` | Go-compatible pprof HTTP server (CPU/heap/goroutine/deadlock) |
| `kcptun-client/` | Client binary (see `kcptun-client/AGENTS.md`) |
| `kcptun-server/` | Server binary + stress tests (see `kcptun-server/AGENTS.md`) |
| `bench/` | Bench/profile runners (see `bench/AGENTS.md`) |
| `bugs/` | Bug reports & postmortems (not under repo root) |
| `tests/` | Go kcptun reference binaries for e2e (`tests/kcptun-go/`) — **no AGENTS.md** |
| `vendor/` | Vendored third-party crates (`make vendor`; do not edit by hand) |
| `.cargo/` | Cargo config (see `.cargo/AGENTS.md`) |
| `.claude/skills/` | Project agent skills |

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
make e2e                # Go↔Rust interop (tokio + smol); needs Go bins in tests/kcptun-go/
bash test_e2e.sh        # same, without auto-build

# Lint / format
make clippy / clippy-smol / clippy-both   # -D warnings
make fmt

# Bench / profile
make bench
make profile / profile-rust-go / profiling-bins

# Cross (ARM defaults to smol)
make release-armv7 / release-arm64
make install-cross

# Vendor refresh
make vendor
```

Runtime-feature packages (`RT_PKGS`): `kcptun-client`, `kcptun-server`, `kio-rs`, `smux-rs`, `kpprof-rs`.  
Runtime-agnostic: `kcp-rs`, `kcrypt-rs`, `qpp-rs`.

## Architecture Summary

### Crate dependency graph

```
kcptun-client ──┐
                ├──► kcp-rs ──► kcrypt-rs
kcptun-server ──┤
                ├──► smux-rs ──► kio-rs  (feature: tokio | smol)
                ├──► qpp-rs
                ├──► kio-rs
                └──► kpprof-rs ──► kio-rs  (optional: feature pprof)
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
- `tokio` and `smol` are **mutually exclusive**
- Native default: tokio; ARM Makefile targets default to smol
- Business code uses `kio::*` only — never raw tokio/smol in new code paths

### Binary hot path (flush loop)

Both client (`KcpConn`) and server (`KcpServerSession`) split flush into **4 phases** to minimize KCP mutex hold time:

1–3. Outside lock: snappy (via `kio::cpu_block` when appropriate), crypto prepare, optional parallel encrypt  
4. Inside lock: `kcp.send()` / `update()` / `flush()` briefly

Snappy is **session-level** (not per-stream), matching Go. Compression **on by default** (`--nocomp` disables).

## For AI Agents

### Working In This Directory

- **Orient with hierarchical `AGENTS.md` before full-repo scans.** Skill: `.claude/skills/agents-md-orient/`.
- `CLAUDE.md` = how to work; this file = what lives where.
- Wire compatibility with Go kcptun/kcp-go v5 is the hard constraint.
- `--key`, `--crypt`, `--mode`, `--nocomp` must match client and server.
- New bug write-ups go under `bugs/` only.
- After structural/public-API changes: update nearest AGENTS or state "no AGENTS sync needed".
- **Do not create `AGENTS.md` under `tests/` or `kcptun-server/tests/`.**

### Testing Requirements

- `cargo test --workspace` / `make test`
- Interop: `bash test_e2e.sh` or `make e2e` after protocol/crypto/SMUX changes
- Stress: `make stress` after flush/lock/session changes
- Clippy: `make clippy` (`-D warnings`)

### Common Patterns

- Global allocator: `mimalloc` in both binaries
- Crypto selection: `kcrypt_rs::select_block_crypt` / `select_aead_crypt`
- Packet packing: `kcp_rs::CryptoBuf` + `encrypt_batch`
- Async I/O: `kio::{TcpStream, UdpSocket, spawn_task, cpu_block, sleep_ms}`

## Dependencies

### Internal

Workspace members only (see graph above).

### External

Vendored under `vendor/` via `.cargo/config.toml`. Notable: `bytes`, `parking_lot`, `crossbeam`, `reed-solomon-erasure`, `aes`/`aes-gcm`, `snap`, `clap`, `mimalloc`, optional `pprof`.

<!-- MANUAL: Any manually added notes below this line are preserved on regeneration -->
