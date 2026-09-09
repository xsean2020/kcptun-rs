<!-- Parent: ../../AGENTS.md -->
<!-- Generated: 2026-07-22 | Updated: 2026-08-16 (tokio-only refactor) -->

# time

## Purpose

Sleep and timeout primitives: `sleep`, `sleep_ms`, `timeout`, `Elapsed`.

## Key Files

| File | Description |
|------|-------------|
| `mod.rs` | `sleep_ms`, `Elapsed`; re-exports `sleep`/`timeout` from tokio |

## Subdirectories

None.

## For AI Agents

### Working In This Directory

- Prefer `knet::sleep_ms` / `knet::timeout` in shared code.
- `Elapsed` is the timeout error type.

### Testing Requirements

- knet tests; binary idle/closewait paths use these timers

### Common Patterns

```rust
knet::sleep_ms(2).await;
knet::timeout(dur, fut).await
```

## Dependencies

### Internal

- Parent `knet`

### External

- tokio time

<!-- MANUAL: -->
