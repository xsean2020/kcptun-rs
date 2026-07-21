# Flamegraph / CPU profiling runbook (kcptun-rs)

Reproducible CPU sampling for the data plane on **macOS arm64** (primary) and Linux.

## Prerequisites

```bash
cargo install samply --locked   # primary sampler → Speedscope JSON
# Script rebuilds unstripped release binaries by default for better stacks:
#   CARGO_PROFILE_RELEASE_STRIP=false CARGO_PROFILE_RELEASE_DEBUG=true
```

Fallback (less reliable on macOS): `cargo install flamegraph --locked` (often needs `dtrace` privileges).

### macOS samply constraints (critical)

- samply **cannot** wrap system `/bin/bash` or system Python as the root process.
- This repo uses a **locally built** helper `bench/kcptun_prof_wl` (`rustc -O bench/kcptun_prof_wl.rs`) as the root; it spawns release server/client + Python echo + concurrent bulk.
- Default release `strip = true` yields anonymous `0x…` frames; the script rebuilds with strip off unless `SKIP_PROFILE_REBUILD=1`.
- Even with debug info, LTO can leave many frames poorly named — cross-check with throughput ratios and `nm`/`atos`.

## One-shot capture

```bash
bash bench/profile_flamegraph.sh all     # L1–L4
# or
make profile

# Single scenario
bash bench/profile_flamegraph.sh l1
BENCH_DATA_MB=100 bash bench/profile_flamegraph.sh l2
SKIP_PROFILE_REBUILD=1 bash bench/profile_flamegraph.sh l3   # reuse existing unstripped bins
```

Artifacts land in `bench/profiles/` (gitignored except README / HOTSPOTS / .gitkeep).

## Scenario matrix

| ID | Crypt | Flags | Load | Purpose |
|----|-------|-------|------|---------|
| L1 | `null` | `--nocomp --mode fast` | bulk `BENCH_DATA_MB` (default 50) | Data plane, copies, scheduling, UDP |
| L2 | `aes` | same | bulk | Encrypt batch + CFB |
| L3 | `3des` | same | bulk | Heavy cipher dominance |
| L4 | (stress defaults) | stress test | `test_multithread_10_connections` | Locks / multi-conn |

samply samples the **full process tree** under `kcptun_prof_wl` (server + client).

## Viewing profiles

```bash
samply load bench/profiles/L1-null-nocomp-*.json.gz
# or upload the .json.gz to https://www.speedscope.app
```

## Symbol map (this stack)

| Frame pattern | Layer |
|---------------|--------|
| `encrypt_batch` / `should_cpu_block_encrypt` | Crypto batch (shared client/server) |
| `CryptEngine` / cipher `encrypt` / CFB | Block crypt |
| `KCP::flush` / `KCP::input` / `KCP::send` | ARQ |
| `encode_header_into` / SMUX flush / `write_bytes` | Multiplexer |
| snappy compress / decompress | Session compression (off with `--nocomp`) |
| `send_batch` / UDP send/recv | Network I/O |
| `cpu_block` / tokio runtime / `park` | Scheduling / offload |



## Go-compatible pprof (Rust → `go tool pprof`)

Rust binaries emit **Google pprof protobuf** on `--pprof <addr>`:

```bash
# 1) build with symbols (recommended)
RUSTFLAGS="-C force-frame-pointers=yes" cargo build --profile profiling -p kcptun-server -p kcptun-client

# 2) one-shot capture under load → .pb file
bash bench/profile_rust_go_pprof.sh server 20
# or: make profile-rust-go

# 3) analyze with Go toolchain (function names, flame graph UI)
go tool pprof -http=127.0.0.1:0 bench/profiles/rust-server-aes-*.pb
go tool pprof -top bench/profiles/rust-server-aes-*.pb
go tool pprof -list=AesCfbCrypt bench/profiles/rust-server-aes-*.pb
```

Manual:

```bash
./target/profiling/kcptun-server -l :29900 -t 127.0.0.1:8080 --key k --crypt aes --nocomp --pprof 127.0.0.1:6060
curl -o cpu.pb 'http://127.0.0.1:6060/debug/pprof/profile?seconds=20'
go tool pprof -http=:0 cpu.pb
```

> Note: profiles show **Rust demangled names** inside Go pprof UI. This is not a Go binary profile of the Go kcptun process — use `bash bench/profile_go_pprof.sh` for pure Go.


## Optimization decision tree

1. **Cipher inner loop dominates (L2/L3)** → algorithm / monomorphization micro-opts; verify not residual `dyn` dispatch.
2. **Copy / Bytes / Vec churn (L1)** → ownership pipeline; avoid `to_vec`; null move paths.
3. **Lock / mutex (L1/L4)** → shorten critical sections; never hold KCP lock across encrypt/snappy.
4. **Syscall / send (L1)** → batch send paths; Linux `sendmmsg` only if justified.
5. **Scheduler / cpu_block (L1/L2)** → retune thresholds only with evidence.
6. **No actionable hotspot ≥ ~5%** → stop coding; document in `HOTSPOTS.md`.

Hard constraints: Go wire compatibility; no congestion-window cheats; one optimization class per change; prefer shared `kcp_rs::encrypt_batch` to avoid client/server drift.

## After a code change — verify

```bash
cargo test --workspace
cargo clippy --workspace -- -D warnings
# if data-plane / concurrency:
make stress
# if crypto / protocol:
bash test_e2e.sh
# re-measure:
BENCH_DATA_MB=50 bash bench/run_bench.sh
BENCH_DATA_MB=50 bash bench/profile_flamegraph.sh l1   # or affected ID
```

Update `bench/profiles/HOTSPOTS.md` with before/after notes.

## macOS / project gotchas

- No Linux `perf` by default — use **samply**, not `cargo flamegraph` alone.
- Prefer **wrap-command** mode (`samply record --save-only -o out -- cmd`) over `-p PID` (attach may need `samply setup` codesign).
- Binary flag `--pprof` is a **placeholder** HTTP banner, not a real pprof endpoint.
- `null` cipher has **no** crypto header; `none` has header without encryption.
- Compression is **on by default**; this script uses `--nocomp` for raw data-plane profiles.
- Release profile strips symbols; samply usually still attributes Rust frames, but stripped frames may look anonymous — build with debug info if needed for deep analysis (`CARGO_PROFILE_RELEASE_DEBUG=1` one-off).

## Related docs

- `PERF_OPTIMIZATION_PLAN.md` — residual R-items and KPI gates
- `.claude/skills/flamegraph-perf/SKILL.md` — agent skill for the full loop
- `bench/profiles/HOTSPOTS.md` — last recorded ranking (after first real matrix)
