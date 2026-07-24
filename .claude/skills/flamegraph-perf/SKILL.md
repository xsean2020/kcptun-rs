---
name: flamegraph-perf
description: Profile kcptun-rs with Go pprof, rank hotspots, evidence-gated optimize, verify. Use when user asks for flamegraph, 火焰图, performance bottleneck, CPU profiling of kcptun client/server, or AES soft-path investigation.
---

# Go pprof performance loop (kcptun-rs)

## When to use

- Finding CPU bottlenecks on client/server data plane
- Before speculative perf refactors
- After large perf PRs to confirm hotspot movement
- Suspected soft-AES / wrong crypto backend on aarch64

## When not to use

- Pure correctness bugs (use systematic debugging)
- Protocol design without a load hypothesis

## Prerequisites

```bash
# Go toolchain for pprof analysis
# Install from https://go.dev/dl/ or: brew install go

# Build profiling binaries with symbols + pprof feature
RUSTFLAGS="-C force-frame-pointers=yes" cargo build --profile profiling --features pprof -p kcptun-server -p kcptun-client
```

## Scenario matrix

| ID | Command | Purpose |
|----|---------|---------|
| L1 | `CRYPT=null bash bench/profile_rust_go_pprof.sh server 20` | null/nocomp bulk |
| L2 | `CRYPT=aes bash bench/profile_rust_go_pprof.sh server 20` | aes bulk |
| L3 | `CRYPT=3des bash bench/profile_rust_go_pprof.sh server 20` | 3des bulk |
| L4 | `make stress` | stress 10-conn |
| All | `make profile` | default server profile |

Env:

- `BENCH_DATA_MB` (default 50)
- `SKIP_PROFILE_REBUILD=1` — skip rebuild (faster if bins already built for profiling)

## Capture and analyze

```bash
# One-shot capture under load → .pb file
bash bench/profile_rust_go_pprof.sh server 20   # or make profile

# Analyze with Go toolchain (function names, flame graph UI)
go tool pprof -http=127.0.0.1:0 bench/profiles/rust-server-aes-*.pb
go tool pprof -top bench/profiles/rust-server-aes-*.pb
go tool pprof -list=encrypt_batch bench/profiles/rust-server-aes-*.pb
```

Produces Google protobuf with **demangled Rust function names** (e.g. `AesCfbCrypt::encrypt`, `encrypt_batch`), not `0x` addresses.

### Manual pprof endpoints

```bash
# Start server with pprof
./target/profiling/kcptun-server -l :29900 -t 127.0.0.1:8080 --key k --crypt aes --nocomp --pprof 127.0.0.1:6060

# CPU profile
curl -o cpu.pb 'http://127.0.0.1:6060/debug/pprof/profile?seconds=20'
go tool pprof -http=:0 cpu.pb

# Heap profile
curl -o heap.pb 'http://127.0.0.1:6060/debug/pprof/heap'
go tool pprof -http=:0 heap.pb

# Thread dump / deadlock check (requires --features pprof-deadlock)
curl 'http://127.0.0.1:6060/debug/pprof/goroutine?debug=2'
curl 'http://127.0.0.1:6060/debug/pprof/deadlock'
```

## Closed loop

1. Capture: `bash bench/profile_rust_go_pprof.sh server 20`
2. Open: `go tool pprof -http=127.0.0.1:0 bench/profiles/rust-server-*.pb`
3. Update `bench/profiles/HOTSPOTS.md` with ranks + throughput
4. One surgical fix if hotspot actionable (map to `PERF_OPTIMIZATION_PLAN.md`)
5. `cargo test --workspace` + `cargo clippy --workspace -- -D warnings`
6. stress/e2e as needed (`make stress`, `bash test_e2e.sh`)
7. Re-bench + re-profile
8. CHANGELOG if measurable

## Symbol map

| Frame pattern | Layer |
|---------------|--------|
| `encrypt_batch` / `should_cpu_block_encrypt` | Crypto batch |
| `CryptEngine` / cipher `encrypt` / CFB | Block crypt |
| `aes::soft::fixslice::*` | **Wrong on Apple Silicon** — soft AES |
| `aes::armv8::*` / `aes::ni::*` | Hardware AES |
| `TripleDesCipher::encrypt_block` | 3DES |
| `KCP::flush` / `input` / `send` / `SegmentPool` | ARQ |
| `encode_header_into` / SMUX flush | Mux |
| snappy | Compression (off with `--nocomp`) |
| `send_batch` | UDP I/O |
| `async_main::{closure}` (collapsed) | Tokio entry; may hide inlined leaves |

## Decision tree

1. **Cipher soft path on aarch64** → ensure `.cargo/config.toml` has `--cfg aes_armv8` (see `make vendor` recipe).
2. **Cipher inner loop (L2/L3)** → algorithm micro-opts; verify not residual `dyn`.
3. **Copy / Bytes churn (L1)** → ownership pipeline.
4. **Lock / mutex (L1/L4)** → shorten critical sections; never hold KCP lock across encrypt/snappy.
5. **Syscall / send** → batch send; Linux sendmmsg only if justified.
6. **No actionable ≥~5% leaf** → stop coding; document in HOTSPOTS.md.

Hard rules: wire compatibility; no congestion cheats; one class per change; shared `encrypt_batch`.

## Gotchas

- Build with `--features pprof` to enable the pprof HTTP server; without it, `--pprof` is a no-op.
- Use `--features pprof-deadlock` for deadlock detection (adds overhead from `parking_lot::deadlock_detection`).
- `null` has no crypto header; `none` has header without encryption
- Compression default ON; profile script uses `--nocomp`
- Default release `strip=true` + LTO hides symbols; use `--profile profiling` for symbol info
- `make vendor` rewrites `.cargo/config.toml` — must keep `aes_armv8` flags (Makefile does)

## Last run summary (2026-07-21)

- Git: `49f2aac` (armv8 AES) after tooling `d482d31` / analysis `a79d74a`
- Host: macOS arm64
- L1 null bulk: ~77–128 MB/s (no single ≥5% leaf beyond async main)
- L2 aes **before** armv8: ~12–14 MB/s, soft fixslice in profile
- L2 aes **after** armv8: ~66–85 MB/s (~5–6×)
- L3 3des: ~12–14 MB/s; `TripleDesCipher::encrypt_block` visible
- L4 stress: test OK; low signal for locks
- Change landed: enable `aes_armv8` cfg for aarch64 targets

## Related docs

- `bench/PROFILE_RUNBOOK.md`
- `bench/profiles/HOTSPOTS.md`
- `PERF_OPTIMIZATION_PLAN.md`
- Design: `docs/superpowers/specs/2026-07-21-flamegraph-perf-design.md`
