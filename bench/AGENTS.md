<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-07-22 | Updated: 2026-07-22 -->

# bench

## Purpose

Throughput and CPU-profile tooling for Rust vs Go kcptun. Shell/Python runners, samply flamegraph matrix (L1–L4), Go-compatible pprof export, symbolication helpers, and captured artifacts under `profiles/`.

## Key Files

| File | Description |
|------|-------------|
| `run_bench.sh` | Bench orchestration helper |
| `throughput.py` | Throughput measurement utility |
| `profile_flamegraph.sh` | L1–L4 samply capture (`make profile`) |
| `profile_rust_go_pprof.sh` | Rust CPU → Go pprof protobuf |
| `profile_go_pprof.sh` | Go side pprof helper |
| `symbolicate_profile.py` | Post-process flamegraph symbols / named stacks |
| `kcptun_prof_wl.rs` / `kcptun_prof_wl` | Local workload binary for macOS samply (not system bash/python as root) |
| `PROFILE_RUNBOOK.md` | How to run and interpret profiles |
| `profiles/` | Artifacts: `HOTSPOTS.md`, `*.json.gz`, `*.pb`, README |

## Subdirectories

| Directory | Purpose |
|-----------|---------|
| `profiles/` | Flamegraph/pprof outputs and hotspot notes |

## For AI Agents

### Working In This Directory

- Before speculative perf edits: skill `.claude/skills/flamegraph-perf/` + `PROFILE_RUNBOOK.md`.
- Prefer evidence from `profiles/HOTSPOTS.md` + re-bench over guesswork.
- macOS: do not use system `/bin/bash` as samply root process — use `kcptun_prof_wl`.
- One optimization class per change; keep wire compatibility; shared `encrypt_batch` paths.
- Root also has `bench_rust_vs_go.py` / `bench_results.json` for 3-way throughput.

### Testing Requirements

- Not unit tests; validate by re-running profile/bench scripts after perf changes
- `make bench`, `make profile`, `make profile-rust-go`

### Common Patterns

- Env: `BENCH_DATA_MB`, `SKIP_PROFILE_REBUILD=1`
- Profiling profile: `cargo build --profile profiling -p kcptun-server -p kcptun-client`

## Dependencies

### Internal

- Built `kcptun-client` / `kcptun-server` release or profiling bins

### External

- `samply`, optional `rustfilt`, Go toolchain for pprof UI

<!-- MANUAL: -->
