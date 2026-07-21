---
name: flamegraph-perf
description: Profile kcptun-rs with flamegraphs (samply), rank hotspots, evidence-gated optimize, verify. Use when user asks for flamegraph, 火焰图, performance bottleneck, CPU profiling of kcptun client/server, or AES soft-path investigation.
---

# Flamegraph-driven performance loop (kcptun-rs)

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
cargo install samply --locked
# Optional: rustfilt for demangling nm output
cargo install rustfilt --locked
```

**macOS critical:** samply cannot use system `/bin/bash` or system Python as the **root** process. This repo uses locally built `bench/kcptun_prof_wl`.

## Scenario matrix

| ID | Command | Purpose |
|----|---------|---------|
| L1 | `bash bench/profile_flamegraph.sh l1` | null/nocomp bulk |
| L2 | `bash bench/profile_flamegraph.sh l2` | aes bulk |
| L3 | `bash bench/profile_flamegraph.sh l3` | 3des bulk |
| L4 | `bash bench/profile_flamegraph.sh l4` | stress 10-conn |
| All | `make profile` or `bash bench/profile_flamegraph.sh all` | full matrix |

Env:

- `BENCH_DATA_MB` (default 50)
- `SKIP_PROFILE_REBUILD=1` — skip unstripped/non-LTO rebuild (faster if bins already built for profiling)

Default rebuild flags (for symbolication):

```text
CARGO_PROFILE_RELEASE_STRIP=false
CARGO_PROFILE_RELEASE_DEBUG=true
CARGO_PROFILE_RELEASE_LTO=false
```

## Preferred: Go pprof format from Rust

```bash
RUSTFLAGS="-C force-frame-pointers=yes" cargo build --profile profiling -p kcptun-server -p kcptun-client
bash bench/profile_rust_go_pprof.sh server 20   # or make profile-rust-go
go tool pprof -http=127.0.0.1:0 bench/profiles/rust-server-aes-*.pb
go tool pprof -top bench/profiles/rust-server-aes-*.pb
```

Produces Google protobuf with **demangled Rust function names** (e.g. `AesCfbCrypt::encrypt`, `encrypt_batch`), not `0x` addresses.

## Closed loop

1. Capture: `bash bench/profile_flamegraph.sh l1` (or `all`)
2. Open: `samply load bench/profiles/<artifact>.json.gz`
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

If Speedscope shows only `0x…` offsets: rebuild with strip/LTO off, then map with:

```bash
nm -n target/release/kcptun-server | rustfilt | less
# address = 0x100000000 + offset_from_profile
```

## Decision tree

1. **Cipher soft path on aarch64** → ensure `.cargo/config.toml` has `--cfg aes_armv8` (see `make vendor` recipe).
2. **Cipher inner loop (L2/L3)** → algorithm micro-opts; verify not residual `dyn`.
3. **Copy / Bytes churn (L1)** → ownership pipeline.
4. **Lock / mutex (L1/L4)** → shorten critical sections; never hold KCP lock across encrypt/snappy.
5. **Syscall / send** → batch send; Linux sendmmsg only if justified.
6. **No actionable ≥~5% leaf** → stop coding; document in HOTSPOTS.md.

Hard rules: wire compatibility; no congestion cheats; one class per change; shared `encrypt_batch`.

## Gotchas

- `--pprof` is a placeholder HTTP banner, not real pprof
- `null` has no crypto header; `none` has header without encryption
- Compression default ON; profile script uses `--nocomp`
- Default release `strip=true` + LTO hides symbols
- `make vendor` rewrites `.cargo/config.toml` — must keep `aes_armv8` flags (Makefile does)

## Last run summary (2026-07-21)

- Git: `49f2aac` (armv8 AES) after tooling `d482d31` / analysis `a79d74a`
- Host: macOS arm64, samply 0.13.1
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
