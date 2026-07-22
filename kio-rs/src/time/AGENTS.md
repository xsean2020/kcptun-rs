<!-- Parent: ../../AGENTS.md -->
<!-- Generated: 2026-07-22 | Updated: 2026-07-22 -->

# time

## Purpose

Sleep and timeout primitives unified across runtimes: `sleep`, `sleep_ms`, `timeout`, `Elapsed`.

## Key Files

| File | Description |
|------|-------------|
| `mod.rs` | `sleep_ms`, `Elapsed`; re-exports backend `sleep`/`timeout` |
| `tokio.rs` | `tokio::time` wrappers |
| `smol.rs` | `smol::Timer` wrappers |

## Subdirectories

None.

## For AI Agents

### Working In This Directory

- Prefer `kio::sleep_ms` / `kio::timeout` in shared code.
- `Elapsed` is the unified timeout error type.

### Testing Requirements

- kio tests; binary idle/closewait paths use these timers

### Common Patterns

```rust
kio::sleep_ms(2).await;
kio::timeout(dur, fut).await
```

## Dependencies

### Internal

- Parent `kio`

### External

- tokio time or smol Timer

<!-- MANUAL: -->
