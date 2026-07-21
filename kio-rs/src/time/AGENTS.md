<!-- Parent: ../../AGENTS.md -->
<!-- Generated: 2026-07-20 | Updated: 2026-07-20 -->

# time

## Purpose

Timers and timeouts: `sleep`, `sleep_ms`, `timeout`, `Elapsed` error type. Used by KCP update loops, keepalive, idle pipe timeout.

## Key Files

| File | Description |
|------|-------------|
| `mod.rs` | Re-exports |
| `tokio.rs` | `tokio::time` |
| `smol.rs` | `smol::Timer` / futures-lite timing |

## For AI Agents

### Working In This Directory

- `timeout` should map cleanly to `Elapsed` on both backends.
- Idle-timeout logic for pipes lives in `lib.rs` (`copy_bidirectional_idle`), not only here — coordinate changes.

### Testing Requirements

Covered by kio tests and e2e idle behavior.

## Dependencies

### External
tokio time **or** smol Timer

<!-- MANUAL: -->
