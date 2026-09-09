<!-- Parent: ../../AGENTS.md -->
<!-- Generated: 2026-07-22 | Updated: 2026-08-16 (tokio-only refactor) -->

# task

## Purpose

Task spawning and CPU offload: `spawn_task`, `cpu_block`, `block_on`, `JoinHandle`. Critical for flush-path crypto/snappy offload.

## Key Files

| File | Description |
|------|-------------|
| `mod.rs` | API surface + JoinHandle semantics; `cpu_block` global pool |
| `tokio.rs` | `tokio::spawn` / `spawn_blocking` style offload |

## Subdirectories

None.

## For AI Agents

### Working In This Directory

- Dropping `JoinHandle` does **not** cancel work (tokio detaches on drop).
- `cpu_block` is the shared offload path — binaries and CryptoBuf policy call into it; keep behavior stable.
- Avoid nested parallel encrypt inside already-offloaded work without measuring.
- `block_on` reuses a shared multi-thread Tokio runtime with Tokio's default
  system-derived worker count; `KCPTUN_WORKER_THREADS` does not control it.

### Testing Requirements

- knet tests; stress/e2e for offload correctness under load

### Common Patterns

```rust
knet::spawn_task(async move { ... });
let out = knet::cpu_block(|| heavy()).await;
```

## Dependencies

### Internal

- Parent `knet`

### External

- tokio (rt, macros, fs, process, io-util, time)

<!-- MANUAL: -->
