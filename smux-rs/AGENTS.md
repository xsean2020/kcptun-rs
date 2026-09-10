<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-07-22 | Updated: 2026-08-16 (tokio-only refactor) -->

# smux-rs

## Purpose

SMUX stream multiplexer over a single async transport (typically KCP+Snappy). Rust port of Go `xtaci/smux` used by kcptun. Supports v1/v2 framing, keepalive pings, and many logical streams per session.

## Key Files

| File | Description |
|------|-------------|
| `Cargo.toml` | Deps `knet-rs` (tokio), `bytes`, `log`, `parking_lot` |
| `src/lib.rs` | Public re-exports: `Session`, `Stream`, `Frame`, `Config`, `SmuxConn`, `SmuxConnBuilder`, … |
| `src/frame.rs` | 8B header codec; `Cmd`, `Frame`, `FrameCodec`; `FRAME_HEADER_SIZE=8`, `MAX_FRAME_SIZE` |
| `src/session.rs` | `Session` multiplexer, `Config` / `DEFAULT_CONFIG`, stream open/accept, keepalive, SYN queue |
| `src/stream.rs` | Logical `Stream`: `AsyncRead`/`AsyncWrite` + optional `set_flush_notify`; R4 locks |
| `src/conn.rs` | `SmuxConn` + `SmuxConnBuilder` (`connect`/`serve` → `.build().await`); `open_stream`/`accept` → `Arc<Stream>`; deprecated legacy `client`/`server` wrappers |
| `src/io.rs` | Thin `SmuxIo` (flush notify only; **no** KCP `with_backpressure` — removed) |

## Subdirectories

None (flat `src/`).

## API notes (Tasks 5–6)

- **Preferred:** `SmuxConn::connect(transport)` / `SmuxConn::serve(transport)` → chain `.version` / `.keepalive` / `.config` → `.build().await`.
- `open_stream()` / `accept()` return `Arc<Stream>` which implements `AsyncRead`+`AsyncWrite` directly (no required `SmuxIo` wrapper).
- Stream writes wake the driver via **`flush_notify`** set by SmuxConn; do not reintroduce KCP-specific `with_backpressure`.
- Deprecated `client`/`server` remain as thin sync wrappers for older call sites.
- Production kcptun binaries still drive **`Session` low-level** with custom flush loops; SmuxConn is library-ready for TCP or `KcpStream`-as-transport.

## For AI Agents

### Working In This Directory

- Frame layout: `ver(1)|cmd(1)|length(2 LE)|stream_id(4 LE)` + payload.
- Session owns stream map and read loop; streams are half-close aware.
- Keepalive via periodic ping frames — do not break idle timeout semantics expected by binaries.
- **Receive flow control is session-level and cooperative.** `process_data`
  charges `token_bucket` for every buffered payload byte; the flush cycle
  (`prepare_outbound_into` → `reclaim_tokens`) returns what the application
  has read, and `remove_stream` / `reap_stale_streams` recycle what will
  never be read. A transport read loop **must** check
  `Session::has_receive_capacity()` before pulling more data (both
  `SmuxConn::run` and `kcptun-common`'s `read_loop` do) — with `version = 1`
  there is no per-stream window at all, so this bucket is the only bound on
  how much unread data a fast peer can make us buffer.
- `accept_stream` returns `Ok(None)` for an id that already exists: a
  duplicate or replayed SYN must not evict a live stream.
- UPD (cmd 4) is v2-only; `check_upd` is a no-op for `version = 1`.
- A received frame whose `ver` differs from `config.version` closes the
  session (Go's `ErrInvalidProtocol`). `Frame::new` still defaults to
  `SMUX_VER` (2), so build frames with `.with_ver(session.version())`.
- Compression is **not** in this crate; binaries wrap transport with Snappy before/after SMUX.
- **Do not restore `with_backpressure`.** KCP backpressure belongs on the transport / KcpStream layer, not SmuxIo.
- **R4 lock model (`Stream`):** `recv: Mutex<RecvInner>` (state + recv queue + read_waker + local_closed_at) and `send: Mutex<SendInner>` (send queue + write_waker). If both locks are needed: **recv then send**. Take wakers under lock, **wake after release**. No legacy contiguous `recv_buf`; only `VecDeque<Bytes>`. Peer window / half-close flags stay atomic.

### Testing Requirements

- `cargo test -p smux-rs`
- Interop: `bash test_e2e.sh` with smuxver matrix after frame/session changes
- Stress: `make stress` exercises many concurrent streams

### Common Patterns

```rust
// High-level Builder (recommended for standalone use; aligns with KcpStream):
use smux_rs::{SmuxConn, Config, DEFAULT_CONFIG};
let conn = SmuxConn::connect(tcp)
    .version(2)
    .keepalive(10)
    .build()
    .await?;
// or server: SmuxConn::serve(tcp).config(cfg).build().await?
let stream = conn.open_stream()?; // Arc<Stream> with flush_notify set

// Compatibility (thin wrappers over connect/serve):
// SmuxConn::client(cfg, tcp)? / SmuxConn::server(cfg, tcp)?

// Low-level (for kcptun production / custom transport):
use smux_rs::{Config, Session, DEFAULT_CONFIG};
let session = Session::new_client(&Config { version: 2, ..DEFAULT_CONFIG.clone() })?;
// stream.set_flush_notify(flush); SmuxIo::new(stream) only if sharing Arc without notify
```

## Dependencies

### Internal

- `knet-rs` — AsyncRead/AsyncWrite, tokio runtime

### External

- `bytes`, `log`, `parking_lot`

<!-- MANUAL: -->
