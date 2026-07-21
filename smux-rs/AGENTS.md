<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-07-20 | Updated: 2026-07-20 -->

# smux-rs

## Purpose

SMUX stream multiplexer (port of Go `xtaci/smux`). Multiple logical streams over one underlying async transport (typically KCP+Snappy). Supports protocol v1 and v2 (window update frames). Runtime-agnostic via `kio-rs` features (`tokio` default / `smol`).

## Key Files

| File | Description |
|------|-------------|
| `Cargo.toml` | Features: `default=["tokio"]`, `tokio`, `smol`; depends on `kio-rs` with `default-features=false` |
| `src/lib.rs` | Re-exports `Frame`, `Session`, `Stream`, `Config` |
| `src/frame.rs` | 8-byte header codec; `Cmd::{Syn,Fin,Psh,Nop,Upd}` |
| `src/session.rs` | `Session` multiplexer, keepalive, stream open/accept, flow control |
| `src/stream.rs` | `Stream` logical channel (AsyncRead/AsyncWrite via kio) |

## Subdirectories

None.

## For AI Agents

### Working In This Directory

- Wire header: `ver(1)|cmd(1)|length(2 LE)|stream_id(4 LE)` = 8 bytes. Max payload `MAX_FRAME_SIZE = 60000`.
- Commands must match Go: SYN=0, FIN=1, PSH=2, NOP=3, UPD=4.
- Feature flags must stay aligned with `kio-rs` / binaries: enable exactly one of tokio/smol.
- Stream I/O uses `kio::AsyncRead` / `AsyncWrite` — different traits under each feature; do not import tokio/futures-lite directly in public APIs.
- Keepalive via periodic NOP frames; v2 uses UPD for window updates.

### Testing Requirements

- Crate tests + e2e with `--smuxver 1` and `2`
- Stress tests exercise many concurrent streams

### Common Patterns

- `Session` wraps an async transport; `open_stream` / `accept_stream`
- `Config` / `DEFAULT_CONFIG` for version, buffers, keepalive
- Binaries wrap `Stream` in `SmuxStreamAsync` / `SmuxStreamIo` + optional `QPPPort`

## Dependencies

### Internal
- `kio-rs` (async primitives)

### External
- `bytes`, `log`, `parking_lot`

<!-- MANUAL: -->
