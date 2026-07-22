<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-07-22 | Updated: 2026-07-22 -->

# smux-rs

## Purpose

SMUX stream multiplexer over a single async transport (typically KCP+Snappy). Rust port of Go `xtaci/smux` used by kcptun. Supports v1/v2 framing, keepalive pings, and many logical streams per session.

## Key Files

| File | Description |
|------|-------------|
| `Cargo.toml` | Features `tokio` (default) / `smol` via `kio-rs`; deps `bytes`, `log`, `parking_lot` |
| `build.rs` | Runtime feature glue if present |
| `src/lib.rs` | Public re-exports: `Session`, `Stream`, `Frame`, `Config`, … |
| `src/frame.rs` | 8B header codec; `Cmd`, `Frame`, `FrameCodec`; `FRAME_HEADER_SIZE=8`, `MAX_FRAME_SIZE` |
| `src/session.rs` | `Session` multiplexer, `Config` / `DEFAULT_CONFIG`, stream open/accept, keepalive |
| `src/stream.rs` | Logical `Stream` (AsyncRead/AsyncWrite via session), window / state |

## Subdirectories

None (flat `src/`).

## For AI Agents

### Working In This Directory

- Frame layout: `ver(1)|cmd(1)|length(2 LE)|stream_id(4 LE)` + payload.
- Features must match the binary: `tokio` XOR `smol` through `kio-rs`.
- Session owns stream map and read loop; streams are half-close aware.
- Keepalive via periodic ping frames — do not break idle timeout semantics expected by binaries.
- Compression is **not** in this crate; binaries wrap transport with Snappy before/after SMUX.

### Testing Requirements

- Crate/unit tests if present
- Interop: `bash test_e2e.sh` with smuxver matrix after frame/session changes
- Stress: `make stress` exercises many concurrent streams

### Common Patterns

```rust
use smux_rs::{Config, Session, DEFAULT_CONFIG};
// Session over async transport from kio
```

## Dependencies

### Internal

- `kio-rs` — AsyncRead/AsyncWrite, runtime features

### External

- `bytes`, `log`, `parking_lot`

<!-- MANUAL: -->
