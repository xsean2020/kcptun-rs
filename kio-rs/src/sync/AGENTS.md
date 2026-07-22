<!-- Parent: ../../AGENTS.md -->
<!-- Generated: 2026-07-22 | Updated: 2026-07-22 -->

# sync

## Purpose

Runtime-agnostic sync primitives used for backpressure and mutual exclusion: `Notify` (backend-specific) and `Mutex` from `async_lock`.

## Key Files

| File | Description |
|------|-------------|
| `mod.rs` | Re-exports `async_lock::Mutex`; selects tokio/smol `Notify` |
| `tokio.rs` | Tokio-backed `Notify` |
| `smol.rs` | Smol-backed `Notify` |

## Subdirectories

None.

## For AI Agents

### Working In This Directory

- Prefer `kio::sync::Notify` over `tokio::sync::Notify` in shared code.
- `Mutex` is always `async_lock` — works on both backends.

### Testing Requirements

- Crate-level kio tests / usage from smux & binaries

### Common Patterns

- Wake flush/read loops on buffer space or new data

## Dependencies

### Internal

- Parent `kio`

### External

- `async_lock`; tokio or smol feature crates

<!-- MANUAL: -->
