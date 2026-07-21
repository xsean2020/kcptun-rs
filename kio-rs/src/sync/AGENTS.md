<!-- Parent: ../../AGENTS.md -->
<!-- Generated: 2026-07-20 | Updated: 2026-07-20 -->

# sync

## Purpose

Minimal sync primitives shared across runtimes. Currently exposes `Notify` for wake/wait patterns used by flush loops and session lifecycle.

## Key Files

| File | Description |
|------|-------------|
| `mod.rs` | Re-exports |
| `tokio.rs` | Tokio `Notify` wrapper |
| `smol.rs` | Smol/`event-listener` style notify |

## For AI Agents

### Working In This Directory

Keep both backends' `Notify` API identical (`notify_one` / `notified().await` style as implemented).

### Testing Requirements

Unit tests in `kio-rs/src/tests.rs` if added; otherwise exercised via binaries.

## Dependencies

### External
tokio sync **or** event-listener / async-lock ecosystem under smol feature

<!-- MANUAL: -->
