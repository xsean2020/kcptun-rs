<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-07-22 | Updated: 2026-07-23 -->

# bench

## Purpose

Throughput and CPU-profile tooling for Rust vs Go kcptun. Go-compatible pprof profiling (CPU, heap, goroutine/deadlock), Go pprof export, and captured artifacts under `profiles/`.

## Key Files

| File | Description |
|------|-------------|
| `run_bench.sh` | Bench orchestration helper |
| `throughput.py` | Throughput measurement utility |
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

### Testing Requirements

- Not unit tests; validate by re-running profile/bench scripts after perf changes
- `make bench`, `make profile`, `make profile-rust-go`

### Common Patterns

- Env: `BENCH_DATA_MB`, `SKIP_PROFILE_REBUILD=1`
- Profiling profile: `cargo build --profile profiling --features pprof -p kcptun-server -p kcptun-client`
- pprof HTTP endpoints: `--pprof 127.0.0.1:6060` (requires `--features pprof`)
- Deadlock detection: `--features pprof-deadlock` (adds overhead)

## Dependencies

### Internal

- Built `kcptun-client` / `kcptun-server` release or profiling bins

### External

- Go toolchain for pprof UI (`go tool pprof`)

<!-- MANUAL: -->
