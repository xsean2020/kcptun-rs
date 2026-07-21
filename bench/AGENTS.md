<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-07-20 | Updated: 2026-07-20 -->

# bench

## Purpose

Throughput benchmarking harness comparing **Go kcptun**, **Rust-tokio**, and **Rust-smol**. Invoked via `make bench` (rebuilds both Rust backends first).

## Key Files

| File | Description |
|------|-------------|
| `run_bench.sh` | Orchestrates builds, process lifecycle, invokes Python thruput runner |
| `throughput.py` | Measures pipe throughput through client/server pair |

## Related (repo root)

| File | Description |
|------|-------------|
| `../bench_rust_vs_go.py` | Higher-level comparison script |
| `../bench_results.json` | Latest numeric results (committed artifact of experiments) |

## For AI Agents

### Working In This Directory

- Expect Go binaries under `tests/kcptun-go/` and Rust under `target/release` + `target/smol-release`.
- Keep 3-way comparison semantics when editing scripts.
- Do not treat `bench_results.json` as a source of truth for correctness — only performance snapshots.

### Testing Requirements

Manual: `make bench` (long-running, needs free ports).

## Dependencies

### External
Python 3, bash; optional Go binaries for full comparison.

<!-- MANUAL: -->
