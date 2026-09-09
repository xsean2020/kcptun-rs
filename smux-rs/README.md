# smux-rs

A stream multiplexer for Rust — multiplexes multiple logical streams over a single async transport connection.

Rust port of the Go [`xtaci/smux`](https://github.com/xtaci/smux) library. Wire-compatible with Go smux v1 and v2.

## Table of Contents

- [Overview](#overview)
- [Feature Flags](#feature-flags)
- [Quick Start](#quick-start)
- [API Reference](#api-reference)
  - [SmuxConn](#smuxconn)
  - [Stream](#stream)
  - [Session (Advanced)](#session-advanced)
- [Configuration](#configuration)
- [Protocol](#protocol)
- [Flow Control](#flow-control)
- [Keepalive](#keepalive)
- [Go Compatibility](#go-compatibility)
- [Architecture](#architecture)
- [Troubleshooting](#troubleshooting)

---

## Overview

SMUX (Stream Multiplexer) allows many independent logical streams to share a single underlying transport (e.g. a TCP or UDP connection). Each stream provides ordered, reliable data transfer with flow control.

Key features:

- **`SmuxConn`** — high-level wrapper that manages read/write/keepalive automatically. Prefer `connect`/`serve` Builder; `open_stream()` / `accept()` return `Arc<Stream>` implementing standard async I/O.
- **v1 / v2 protocol** — wire-compatible with Go smux
- **Flow control** — v2 uses per-stream window updates (UPD frames)
- **Half-close** — streams support independent local/remote close (like TCP)
- **Async I/O** — `Stream` (and thin `SmuxIo`) implement `kio::AsyncRead + AsyncWrite` (tokio or smol)
- **Zero-copy** — receive path uses `Bytes` reference-counted slices

---

## Feature Flags

| Feature | Default | Description |
|---------|---------|-------------|
| `tokio` | ✅ | Use tokio runtime (via `knet-rs`) |
| `smol`  | ❌ | Use smol runtime (via `knet-rs`) — mutually exclusive with `tokio` |

> `tokio` and `smol` are **mutually exclusive**. The `build.rs` script enforces this at compile time.

---

## Quick Start

### Client (simplest)

```rust
use smux_rs::{SmuxConn, Config};
use kio::{AsyncReadExt, AsyncWriteExt};

// 1. Connect to server (transport is owned by SmuxConn)
let tcp = kio::TcpStream::connect("127.0.0.1:8080").await?;

// 2. Create SMUX client — Builder starts the driver on build().await
let conn = SmuxConn::connect(tcp)
    .config(Config::default())
    .build()
    .await?;

// 3. Open a stream — Arc<Stream> is AsyncRead + AsyncWrite
let mut stream = conn.open_stream()?;
stream.write_all(b"hello").await?;

let mut buf = [0u8; 1024];
let n = stream.read(&mut buf).await?;
```

### Server (simplest)

```rust
use smux_rs::{SmuxConn, Config};
use kio::{AsyncReadExt, AsyncWriteExt};

// 1. Accept TCP connection
let listener = kio::TcpListener::bind("0.0.0.0:8080".parse().unwrap()).await?;
let (tcp, _) = listener.accept().await?;

// 2. Create SMUX server — Builder starts the driver on build().await
let conn = SmuxConn::serve(tcp)
    .config(Config::default())
    .build()
    .await?;

// 3. Accept streams
loop {
    let mut stream = conn.accept().await?;
    kio::spawn_task(async move {
        let mut buf = [0u8; 1024];
        loop {
            match stream.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => { let _ = stream.write_all(&buf[..n]).await; }
            }
        }
    });
}
```

> Compatibility: `SmuxConn::client(config, transport)?` / `SmuxConn::server(config, transport)?` remain as thin wrappers over `connect`/`serve`.

### High-performance mode (split read/write)

For lower write latency, use `spawn()` with split transport halves:

```rust
let conn = SmuxConn::new(Config::default(), true)?;

// Split transport (runtime-specific)
#[cfg(feature = "tokio")]
{
    use kio::AsyncReadExt;
    let (read, write) = tokio::io::split(tcp);
    conn.spawn(read, write);
}

#[cfg(feature = "smol")]
{
    let (read, write) = smol::io::split(tcp);
    conn.spawn(read, write);
}

// Use streams — writes flush near-instantly via flush_notify
let mut stream = conn.open_stream()?;
```

---

## API Reference

### SmuxConn

High-level connection wrapper. Manages read/flush/keepalive/reap automatically.

| Method | Description |
|--------|-------------|
| `connect(transport)` | **Recommended for clients.** Returns `SmuxConnBuilder`; chain options then `.build().await`. |
| `serve(transport)` | **Recommended for servers.** Same builder path, `is_client=false`. |
| `client(config, transport)` | Thin wrapper: `connect(transport).config(config).build()` (sync spawn path). |
| `server(config, transport)` | Thin wrapper over `serve`. |
| `new(config, is_client)` | Low-level: create connection (no transport yet). Call `run`/`spawn` yourself. |
| `open_stream()` | Open a new stream (client). Returns `Arc<Stream>`. SYN is auto-queued. |
| `accept()` | Accept next incoming stream (server). Async — waits for SYN. |
| `run(&mut transport)` | Low-level: drive with a single transport (10ms poll). |
| `spawn(read, write)` | Low-level: drive with split halves (concurrent, lower latency). |
| `session()` | Access underlying `Session` (advanced). |
| `close()` | Close connection and all streams. |

`SmuxConn` is `Clone` — all clones share the same `Session` via `Arc`.

**When to use `run()` vs `spawn()`:**

| | `run()` | `spawn()` |
|---|---------|-----------|
| Transport | Single `&mut T` | Split read + write halves |
| Read/Write | Sequential (10ms poll) | Concurrent |
| Write latency | Up to 10ms | Near-instant (notify-driven) |
| Complexity | Simplest | Requires splitting transport |

### Stream

Logical stream within a session. Implements `kio::AsyncRead + AsyncWrite`.

```rust
// Async read/write on Arc<Stream> (preferred)
let mut stream = conn.open_stream()?;
stream.write_all(b"data").await?;
let mut buf = [0u8; 4096];
let n = stream.read(&mut buf).await?;
stream.shutdown().await?; // half-close local side (tokio)
// stream.close().await?;  // half-close local side (smol)
```

Key `Stream` methods (on `Arc<Stream>` or via thin `SmuxIo`):

| Method | Description |
|--------|-------------|
| `id()` | Stream ID |
| `available()` | Bytes available to read |
| `pending_send()` | Bytes waiting to be flushed |
| `is_local_closed()` | Local side shut down |
| `is_remote_closed()` | Remote side sent FIN |
| `read(&mut buf)` | Sync read (non-blocking) |
| `read_async(&mut buf)` | Async read (waits for data or FIN) |
| `write(&data)` / `write_bytes(bytes)` | Write to send buffer |
| `mark_local_closed()` | Half-close local side |
| `close()` | Full close (both sides + clear buffers) |

### Session (Advanced)

For use cases that need manual control over the transport (e.g. KCP, custom protocols), use `Session` directly:

```rust
use smux_rs::{Config, Session, Frame, Cmd};
use bytes::BytesMut;

let session = Session::new_client(&Config::default())?;

// --- Inbound: feed transport bytes ---
session.process_data(&inbound_bytes)?;

// --- Outbound: drain frames to transport ---
let mut buf = BytesMut::with_capacity(64 * 1024);
let fin_ids = session.prepare_outbound_into(&mut buf, 64 * 1024, session.version());
// Write buf to transport, then:
session.mark_fins_sent(&fin_ids);

// --- Open stream (SYN must be sent manually) ---
let stream = session.open_stream()?;
let syn = Frame::new(Cmd::Syn, stream.id(), bytes::Bytes::new()).with_ver(session.version());
// Encode and send SYN via your transport...
```

> **When to use `Session` directly:** When the transport has its own reliability layer (like KCP), custom compression, or you need precise control over flush timing. Otherwise, prefer `SmuxConn`.

---

## Configuration

```rust
pub struct Config {
    pub version: u8,              // 1 or 2
    pub max_receive_buffer: usize, // Session-level (default 4 MB)
    pub max_stream_buffer: usize,  // Per-stream (default 256 KB)
    pub max_frame_size: usize,     // Max payload (default 16 KB)
    pub keepalive_interval: u64,   // Seconds (default 10)
    pub keepalive_timeout: u64,    // Seconds, 0=disabled (default 30)
}
```

Default: `Config { version: 1, max_receive_buffer: 4MB, max_stream_buffer: 256KB, ... }`

For v2 (with per-stream flow control):
```rust
let config = Config { version: 2, ..Config::default() };
```

---

## Protocol

### Frame format (8-byte header + payload)

```
+-------+-------+-------+-------+-------+-------+-------+-------+
| ver   | cmd   | length (LE)  | stream_id (LE)              |
+-------+-------+-------+-------+-------+-------+-------+-------+
| data ...                                                     |
+---------------------------------------------------------------+
```

### Commands

| Cmd | Value | Description |
|-----|-------|-------------|
| `SYN` | 0 | Stream open |
| `FIN` | 1 | Stream close / EOF (may carry last data) |
| `PSH` | 2 | Data push |
| `NOP` | 3 | Keepalive ping |
| `UPD` | 4 | Window update (v2 only, 8-byte payload) |

### v1 vs v2

| Feature | v1 | v2 |
|---------|----|----|
| Per-stream window | Unlimited | UPD frames with `consumed` + `window` |
| Write-side flow control | None | `peer_send_window()` limits drain |
| UPD command | Not used | `cmdUPD` with 8-byte payload |

---

## Flow Control

### v1

No per-stream window. `peer_window` set to `u32::MAX` (unlimited). Drain limited only by `max_bytes` parameter.

### v2

**Receive:** When the reader consumes data past half of `max_stream_buffer`, a UPD frame is queued. `SmuxConn` / flush loop sends it via `prepare_outbound_into()`.

**Send:** `drain_send_max()` caps at `peer_send_window()`:
```
peer_window - (bytes_written - peer_consumed)
```
Initial peer window: 256 KB (matching Go `initialPeerWindow`).

When `peer_send_window() == 0`, `AsyncWrite::poll_write` returns `Pending` and registers a waker. When a UPD arrives, the writer is woken.

---

## Keepalive

SMUX uses NOP frames (cmd=3, stream_id=0) as keepalive pings.

- `SmuxConn` checks keepalive every ~1 second
- Sends NOP when `keepalive_interval` elapses
- Closes session when no inbound frame for `keepalive_timeout` seconds
- Any received frame resets the timeout (not just NOP)

---

## Go Compatibility

| Aspect | Go smux | smux-rs |
|--------|---------|---------|
| Frame header | `ver(1)\|cmd(1)\|length(2 LE)\|sid(4 LE)` | ✅ Same |
| Commands | SYN=0, FIN=1, PSH=2, NOP=3, UPD=4 | ✅ Same |
| Stream IDs | Client=odd, Server=even | ✅ Same |
| Initial peer window | 256 KB | ✅ Same |
| UPD payload | `[consumed 4B LE][window 4B LE]` | ✅ Same |
| v1 no per-stream window | UPD disabled | ✅ Same |
| FIN can carry data | Yes | ✅ Same |
| `bytes_written` tracks on-wire | Yes (`numWritten`) | ✅ Same |

---

## Architecture

```
smux-rs/
├── Cargo.toml
├── build.rs           — enforces tokio/smol mutual exclusion
└── src/
    ├── lib.rs          — public re-exports
    ├── conn.rs         — SmuxConn + SmuxConnBuilder (connect/serve/build)
    ├── frame.rs        — 8B header codec; Cmd, Frame, FrameCodec
    ├── session.rs      — Session multiplexer, Config, flow control
    ├── stream.rs       — logical Stream (AsyncRead/AsyncWrite, flush_notify)
    └── io.rs           — thin SmuxIo (flush notify only; no KCP backpressure)
```

### Data flow (SmuxConn)

```
Inbound:
  transport → run()/spawn() read task → Session::process_data()
    → SYN: accept_stream() + notify accept()
    → FIN: push_data + mark_remote_closed
    → PSH: push_data_bytes() → Stream recv buffer
    → NOP: update_activity()
    → UPD: apply_peer_update()

Outbound:
  Stream::write() → send buffer → flush_notify
    → run()/spawn() flush task → prepare_outbound_into()
        → SYN: drain pending_syns
        → PSH: drain_send_max()
        → FIN: encode for local-closed streams
        → UPD: take_upd_frames()
    → transport.write()
    → mark_fins_sent()
```

### Zero-copy design

- **Receive:** `push_data_bytes(Bytes)` stores `Bytes` directly in `VecDeque<Bytes>` — no copy on push
- **Codec:** `FrameCodec::decode()` uses `split_to + slice` for zero-copy payload extraction
- **Send:** `drain_send_max()` copies into caller's `BytesMut` (single copy on flush path)

---

## Troubleshooting

### Streams not receiving data

`SmuxConn::run()` or `spawn()` must be driving the connection. Check that the background task is running and the transport is connected.

### `accept()` never returns

Ensure `SmuxConn::new(config, false)` is used for the server side (is_client=false). Server mode enables the accept queue — `process_data()` pushes accepted streams and notifies.

### Writes not reaching the peer

`stream.write()` only buffers data. The flush loop (`run()` or `spawn()`) must be running to drain the buffer and write to the transport.

### v2 writes blocked (peer window = 0)

The peer hasn't sent a UPD frame yet, or the peer's receive buffer is full. Ensure the reader on the other end is consuming data (triggering UPD generation).

### FIN frames not being sent

`SmuxConn` handles FIN encoding automatically. The stream must be marked `local_closed` (via `shutdown()` / `close()` on `Stream` / `SmuxIo`). The flush loop encodes FIN and marks it sent after transport accepts.

### `build.rs` panic: feature conflict

```
[CRITICAL ERROR] Feature conflict: `tokio` and `smol` are mutually exclusive!
```

Use `--no-default-features --features smol` to select a single runtime.
