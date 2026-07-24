# CPU profiling runbook (kcptun-rs)

Reproducible CPU sampling for the data plane on **macOS arm64** (primary) and Linux.

## Prerequisites

```bash
# Go toolchain (for pprof analysis UI)
# Install from https://go.dev/dl/ or: brew install go

# Build profiling binaries with symbols + pprof feature
RUSTFLAGS="-C force-frame-pointers=yes" cargo build --profile profiling --features pprof -p kcptun-server -p kcptun-client
```

## One-shot capture (Go pprof)

```bash
# Rust server CPU profile → Go pprof protobuf
bash bench/profile_rust_go_pprof.sh server 20
# or: make profile

# Rust client CPU profile
bash bench/profile_rust_go_pprof.sh client 15

# Override cipher / data size
CRYPT=3des BENCH_DATA_MB=100 bash bench/profile_rust_go_pprof.sh server 30
```

Artifacts land in `bench/profiles/` (gitignored except README / HOTSPOTS / .gitkeep).

## Scenario matrix

| ID | Crypt | Flags | Load | Purpose |
|----|-------|-------|------|---------|
| L1 | `null` | `--nocomp --mode fast` | bulk `BENCH_DATA_MB` (default 50) | Data plane, copies, scheduling, UDP |
| L2 | `aes` | same | bulk | Encrypt batch + CFB |
| L3 | `3des` | same | bulk | Heavy cipher dominance |
| L4 | (stress defaults) | stress test | `test_multithread_10_connections` | Locks / multi-conn |

## Viewing profiles

```bash
# Interactive web UI (flame graph, top, source listing)
go tool pprof -http=127.0.0.1:0 bench/profiles/rust-server-aes-*.pb

# Top functions
go tool pprof -top bench/profiles/rust-server-aes-*.pb

# Source-level annotation
go tool pprof -list=encrypt_batch bench/profiles/rust-server-aes-*.pb
```

## Manual pprof capture

```bash
# Start server with pprof HTTP endpoint
./target/profiling/kcptun-server -l :29900 -t 127.0.0.1:8080 --key k --crypt aes --nocomp --pprof 127.0.0.1:6060

# Capture CPU profile
curl -o cpu.pb 'http://127.0.0.1:6060/debug/pprof/profile?seconds=20'
go tool pprof -http=:0 cpu.pb

# Heap allocation profile
curl -o heap.pb 'http://127.0.0.1:6060/debug/pprof/heap'
go tool pprof -http=:0 heap.pb

# Thread dump (goroutine equivalent)
curl 'http://127.0.0.1:6060/debug/pprof/goroutine?debug=2'

# Deadlock check (requires --features pprof-deadlock)
curl 'http://127.0.0.1:6060/debug/pprof/deadlock'

# Browse all endpoints
open 'http://127.0.0.1:6060/debug/pprof/'
```

> Note: profiles show **Rust demangled names** inside Go pprof UI. This is not a Go binary profile of the Go kcptun process — use `bash bench/profile_go_pprof.sh` for pure Go.

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
BENCH_DATA_MB=50 bash bench/profile_rust_go_pprof.sh server 20
```

Update `bench/profiles/HOTSPOTS.md` with before/after notes.

## macOS / project gotchas

- Build with `--features pprof` to enable the pprof HTTP server; without it, `--pprof` is a no-op.
- Use `--features pprof-deadlock` to enable deadlock detection (adds overhead from `parking_lot::deadlock_detection`).
- `null` cipher has **no** crypto header; `none` has header without encryption.
- Compression is **on by default**; profiling scripts use `--nocomp` for raw data-plane profiles.
- Release profile strips symbols; profiling profile keeps them (`strip = false, debug = 2`).

## Related docs

- `PERF_OPTIMIZATION_PLAN.md` — residual R-items and KPI gates
- `.claude/skills/flamegraph-perf/SKILL.md` — agent skill for the full profiling loop
- `bench/profiles/HOTSPOTS.md` — last recorded ranking (after first real matrix)
