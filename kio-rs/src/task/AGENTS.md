<!-- Parent: ../../AGENTS.md -->
<!-- Generated: 2026-07-20 | Updated: 2026-07-20 -->

# task

## Purpose

Task spawning and blocking offload: `spawn_task`, `block_on`, `cpu_block`, `JoinHandle`. Critical for moving Snappy/crypto off the async reactor.

## Key Files

| File | Description |
|------|-------------|
| `mod.rs` | Public API surface |
| `tokio.rs` | `tokio::spawn` / `spawn_blocking` path |
| `smol.rs` | smol executor + **persistent** blocking thread pool for `cpu_block` |

## For AI Agents

### Working In This Directory

- **smol `cpu_block` must stay a long-lived pool** — do not regress to `smol::unblock` (thread churn under 10–100 ms flush cadence).
- Pool sizing: ~CPU count clamped [2, 8] (see current smol.rs).
- `spawn_task` return type is `JoinHandle` — keep abort/detach semantics consistent across backends where possible.

### Testing Requirements

`make test-both`; stress tests exercise `cpu_block` heavily.

## Dependencies

### External
tokio **or** smol + async-executor + crossbeam-channel (pool impl)

<!-- MANUAL: -->
