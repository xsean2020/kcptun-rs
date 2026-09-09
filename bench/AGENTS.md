<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-07-22 | Updated: 2026-08-27 (game/bulk P99 profiles) -->

# bench

## Purpose

Throughput and CPU-profile tooling for Rust vs Go kcptun. Go-compatible pprof profiling (CPU, heap, goroutine/deadlock), Go pprof export, and captured artifacts under `profiles/`.

## Key Files

| File | Description |
|------|-------------|
| `run_bench.sh` | Bench orchestration; labels are **Client → Server** (bulk stream direction) |
| `throughput.py` | Throughput measurement (loadgen → client listen port → server → echo) |
| `run_p99.sh` | Raw KCP P99/P999 cross-test: 4 two-process open-model combinations plus 2 closed-loop baselines; `PROFILE=game` (512B, default) and `PROFILE=bulk` (26624B) |
| `tunnel_p99.sh` | **Full tunnel-stack** P99/P999 orchestrator for tokio + Go, using a bounded open-model probe; `CONN` and `SERVER_SHARDS` exercise multi-worker KCP paths |
| `../kcp-rs/examples/tunnel_latency.rs` | Rust example for tunnel-stack latency (crypto + KCP + SMUX + Snappy) with open-model fixed-rate measurement |
| `probe_tunnel.py` | Bounded-concurrency asyncio open-model probe; RTT starts when its request task actually runs (not at `create_task`), while `queue_p999_us`/`queue_max_us` separately expose probe scheduling delay |
| `../kcptun-common/examples/tunnel_probe.rs` | Preferred low-noise standard-library persistent-connection probe (`PROBE=rust`); synchronous handoff makes its worker count the exact in-flight cap and reports handoff delay and failed requests separately |
| `TUNNEL_TEST_MATRIX.md` | Test matrix documentation with TC-01 through TC-14 + Go comparison cases |
| `REPORT_TEMPLATE.md` | P99/P999 tunnel test report template with diagnostic rules |
| `profile_rust_go_pprof.sh` | Rust CPU → Go pprof protobuf (`make profile`) |
| `profile_go_pprof.sh` | Go side pprof helper |
| `PROFILE_RUNBOOK.md` | How to run and interpret profiles |
| `profiles/` | Artifacts: `HOTSPOTS.md`, `*.pb`, README |

## Subdirectories

| Directory | Purpose |
|-----------|---------|
| `profiles/` | pprof outputs and hotspot notes |

## For AI Agents

### Working In This Directory

- Before speculative perf edits: skill `.claude/skills/flamegraph-perf/` + `PROFILE_RUNBOOK.md`.
- Prefer evidence from `profiles/HOTSPOTS.md` + re-bench over guesswork.
- One optimization class per change; keep wire compatibility; shared `encrypt_batch` paths.
- Root also has `bench_rust_vs_go.py` / `bench_results.json` for 3-way throughput.
- `run_bench.sh` path labels are always **Client → Server** (not server→client). `run_bench <label> <client_bin> <server_bin>`.
- `run_p99.sh` uses one Rust listener shard by default because each raw-KCP case has one session. Override `RUST_SHARDS` for shard-scaling experiments; `WORKERS` controls Go's `GOMAXPROCS` and the Rust probe's top-level runtime separately.
- `run_p99.sh` anchors warm-up and measurement to one fixed send schedule, so an unshed run offers exactly `RPS * DURATION` measurement slots. Keep `inflight_end` honest on both Rust and Go harnesses, and keep the displayed baseline combo labels aligned with the actual two-process topology.

### Testing Requirements

- Not unit tests; validate by re-running profile/bench scripts after perf changes
- `make bench`, `make profile`, `make profile-rust-go`

### Common Patterns

- Env: `BENCH_DATA_MB`, `BENCH_CONNECTIONS`, `BENCH_FILTER`, `BENCH_WORKERS`, `SKIP_PROFILE_REBUILD=1`
- `BENCH_CONNECTIONS` runs the same `BENCH_DATA_MB` payload on each concurrent
  TCP stream and reports aggregate throughput; use a small per-connection size
  when exercising high fan-out (for example, `BENCH_CONNECTIONS=64 BENCH_DATA_MB=4`).
- `run_bench.sh` applies a positive `BENCH_WORKERS` concurrency budget as
  `GOMAXPROCS` for Go and `KCPTUN_WORKER_THREADS` for the Rust `KcpListener`
  shard count. The shared Tokio runtime keeps its system-derived default.
  Use `0` only when intentionally comparing backend defaults.
- `run_bench.sh` runs `BENCH_ROUNDS` (default 3) **interleaved rounds** per
  backend with rotated config order, then reports a MEDIAN summary table —
  compare medians, never a single 200 MB pass (single runs swing ±25% on a
  shared desktop; ordering drift otherwise biases whichever backend runs
  first/last). A preflight load guard aborts when 1-min load exceeds ~70%
  of core count; override with `BENCH_FORCE=1`. `BENCH_FILTER` (exact label)
  still works per round.
- `run_bench.sh` accepts `GO_{CLIENT,SERVER}` and
  `RUST_TOKIO_{CLIENT,SERVER}` binary overrides for commit-to-commit
  comparisons without replacing workspace artifacts.
- For a long connect-per-request run, also run `probe_tunnel.py` directly
  against `echo_server.py`. macOS/Python scheduler pauses are part of the
  direct baseline; do not attribute their P999 alone to the tunnel runtime.
- For an acceptance comparison use `PROBE=rust` after building
  `cargo build -p kcptun-common --example tunnel_probe --release`; it avoids
  coupling the load generator to an async executor and avoids ephemeral-port
  exhaustion by reusing one TCP/SMUX stream per worker. Treat nonzero `failed`
  samples as a failed run, never as an acceptable latency percentile. Also
  report `offered`, `dropped`, and `driver_late`: a low percentile is not a
  throughput result if the probe was capacity-limited or its OS thread paused.
- `tunnel_p99.sh` waits for an end-to-end readiness echo, not merely the
  client listen socket; retain this check when adding new load drivers.
- `POST_CASE_COOLDOWN=0` is suitable for a single isolated case; retain the
  default 15 seconds for a multi-case macOS run so TIME_WAIT sockets drain.
- Profiling profile: `make profiling-bins` (bakes in `force-frame-pointers=yes`)
- pprof HTTP endpoints: `--pprof` (bool flag, fixed `:6060`, requires `--features pprof`)
- Deadlock detection: `--features pprof-deadlock` (adds overhead)

## Dependencies

### Internal

- Built `kcptun-client` / `kcptun-server` release or profiling bins

### External

- Go toolchain for pprof UI (`go tool pprof`)

<!-- MANUAL -->
