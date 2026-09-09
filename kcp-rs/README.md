# kcp-rs

A high-performance Rust implementation of the **KCP** reliable UDP transport — a fast ARQ (Automatic Repeat-reQuest) protocol that gives you ordered, reliable, connection-oriented delivery over plain UDP.

Rust port of Go [`github.com/xtaci/kcp-go/v5`](https://github.com/xtaci/kcp-go). **Wire-compatible** with the Go implementation — KCP segments produced by this crate decode correctly on the Go side and vice versa.

## Table of Contents

- [Overview](#overview)
- [Feature Flags](#feature-flags)
- [Adding the Dependency](#adding-the-dependency)
- [Quick Start](#quick-start)
  - [Sync KCP state machine](#sync-kcp-state-machine)
  - [Async KcpStream](#async-kcpconn)
  - [Listen & connect (server / client)](#listen--connect-server--client)
- [API Reference](#api-reference)
  - [KCP (state machine)](#kcp-state-machine)
  - [KcpConfig / KcpMode](#kcpconfig--kcpmode)
  - [Reed-Solomon FEC](#reed-solomon-fec)
  - [SNMP counters](#snmp-counters)
  - [KcpStream (async)](#kcpconn-async)
  - [PacketTransport](#packettransport)
- [Configuration](#configuration)
- [Wire Protocol](#wire-protocol)
- [Reliability & Data Correctness](#reliability--data-correctness)
- [Go Compatibility](#go-compatibility)
- [Testing](#testing)
- [Architecture](#architecture)
- [Troubleshooting](#troubleshooting)

---

## Overview

KCP sits on top of UDP and provides:

- **Reliable delivery** — lost segments are retransmitted (RTO + fast retransmit + early retransmit).
- **Ordered delivery** — out-of-order segments are buffered and released in order.
- **Congestion control** — Reno-style window (RFC 5681) with rate halving (RFC 6937), disable with `nc=1`.
- **Stream or message mode** — `stream=true` gives a TCP-like byte stream; `stream=false` preserves message boundaries.
- **Reed-Solomon FEC** — optional `data + parity` forward error correction to hide loss.
- **Tunable latency/throughput** — the `Normal / Fast / Fast2 / Fast3` mode curves map to `(nodelay, interval, resend, nc)`.
- **Atomic SNMP counters** — Go-compatible statistics (`kcp-go` CSV layout).

Key design points:

- **Zero-copy** segment parsing via `bytes::BytesMut`; reference-counted `Bytes` receive path (`recv_bytes`).
- **Lock-free** segment pooling with `crossbeam::queue::SegQueue`.
- **Crypto-free core** — encryption lives in the sibling [`kcrypt-rs`](../kcrypt-rs) crate. `kcp-rs` has **no** crypto dependency; wrap the transport instead (see [`PacketTransport`](#packettransport)).

---

## Feature Flags

| Feature | Effect |
|---------|--------|
| *(default)* | Sync KCP state machine only — no async dependencies |
| `async` | `KcpStream` + `PacketTransport` on the tokio runtime (via `knet-rs`) |

---

## Adding the Dependency

```toml
[dependencies]
# Sync KCP only (no async runtime pulled in)
kcp-rs = { path = "../kcp-rs" }

# Or with the async KcpStream wrapper (tokio runtime)
kcp-rs = { path = "../kcp-rs", features = ["async"] }
```

If you use the async API you will usually also depend on [`knet-rs`](../knet-rs) for its `AsyncRead`/`AsyncWrite` traits, and on [`kcrypt-rs`](../kcrypt-rs) when you need encryption.

---

## Quick Start

### Sync KCP state machine

The core type is [`KCP`](#kcp-state-machine). It is a pure state machine: `send()` queues user bytes, the output callback hands you wire segments to put on the network, and `input()` consumes incoming segments.

```rust
use bytes::Bytes;
use kcp_rs::{KcpConfig, KcpMode, KCP};

// A send-side KCP instance. `output` is invoked with every segment that must
// go on the wire (e.g. write it to your UDP socket).
let mut sender = KCP::new(
    0xDEAD_BEEF,              // conversation ID — must match on both ends
    0,                        // token
    Box::new(|data: Bytes| {
        // send_to(data, peer_addr)
    }),
);

// Apply tuning (defaults are Fast3-ish: mtu=1350, sndwnd=128, rcvwnd=128,
// stream mode on, FEC off).
sender.apply(&KcpConfig::default());
// Or a custom config:
sender.apply(&KcpConfig {
    mtu: 1400,
    sndwnd: 512,
    rcvwnd: 512,
    mode: KcpMode::Fast3,
    ..KcpConfig::default()
});

// Queue user data (fragmented internally to the MSS).
sender.send(b"hello kcp").unwrap();
```

A minimal event loop drives both ends:

```rust
fn now_ms() -> u32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u32
}

loop {
    let now = now_ms();

    // Advance the sender (drives flush + retransmission timers).
    sender.update(now);
    sender.flush();           // force-flush any pending segments/ACKs

    // Feed incoming wire segments into the receiver.
    // (in a real app this is your UDP read loop)
    for seg in inbound_segments.drain(..) {
        receiver.input(&seg, true).unwrap();   // ack_no_delay = flush ACKs now
    }

    // Read reassembled bytes back out (Err(NoData) when nothing is ready).
    while let Ok(msg) = receiver.recv_bytes() {
        handle(msg);
    }

    std::thread::sleep(std::time::Duration::from_millis(5));
}
```

> `KCP::update()` returns the milliseconds until the next meaningful event; use it to pace your timer. `KCP::check(current)` gives the same for "when to call update again".

### Async KcpStream

With `features = ["async"]`, [`KcpStream`](#kcpconn-async) wraps a UDP socket + KCP + background input/flush loops behind a `knet::AsyncRead + AsyncWrite` stream — treat it like a reliable TCP connection.

```rust
use kcp_rs::{KcpStream, KcpMode};
use knet::{AsyncReadExt, AsyncWriteExt};

let conn = KcpStream::connect("127.0.0.1:29900")
    .mtu(1400)
    .fec(10, 3)                    // optional Reed-Solomon FEC 10 data + 3 parity
    .mode(KcpMode::Fast3)
    .build()
    .await?;

conn.write_all(b"hello over KCP").await?;
conn.flush().await?;

let mut buf = [0u8; 1024];
let n = conn.read(&mut buf).await?;
```

For two sockets on the same machine (or a custom transport), build each end explicitly:

```rust
use std::sync::Arc;
use kcp_rs::{KcpStream, KcpMode, PacketTransport};

let conn_a = KcpStream::with_transport(
    Arc::new(knet::DatagramSocket::Udp(sock_a)) as Arc<dyn PacketTransport>,
    addr_b,               // remote address
)
.connected(true)          // socket was created via UdpSocket::connect
.conv(0xC0FFEE)           // must match on both ends
.mode(KcpMode::Fast3)
.build()
.await?;
```

> **Encryption** is deliberately *not* inside `KcpStream`. To add crypto, implement [`PacketTransport`](#packettransport) around your encrypted datagrams — the workspace provides `kcptun_common::CryptoTransport` for this. **Snappy** compression also stays outside KCP (session level), matching Go.

### Listen & connect (server / client)

The server side uses [`KcpListener`](#kcpconn-async): bind one UDP socket, and `accept()` hands out a **per-peer** `KcpStream` — inbound datagrams are demultiplexed by source address. Client-side, `KcpStream::connect` dials the listener on a fresh ephemeral socket.

```rust
use kcp_rs::{KcpStream, KcpListener, KcpMode};
use knet::{AsyncReadExt, AsyncWriteExt};

// ── Server: bind, then serve each accepted peer ──
let listener = KcpListener::bind("0.0.0.0:29900")
    .conv(0xC0FFEE)                  // conv must match the clients
    .mode(KcpMode::Fast3)
    .build()
    .await?;

loop {
    let (mut conn, peer) = listener.accept().await?;
    knet::spawn_task(async move {
        // `conn` is a per-peer KcpStream (AsyncRead + AsyncWrite)
        let mut buf = [0u8; 4096];
        loop {
            match conn.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let _ = conn.write_all(&buf[..n]).await;
                }
            }
        }
    });
}

// ── Client: dial the listener ──
let conn = KcpStream::connect("127.0.0.1:29900")
    .conv(0xC0FFEE)
    .mode(KcpMode::Fast3)
    .build()
    .await?;
conn.write_all(b"hello").await?;
```

`accept()` returns `(KcpStream, SocketAddr)` — the per-peer connection and the client's address. Multiple clients hitting the same listener each get their own `KcpStream`; `KcpListener::close()` stops new accepts without disturbing already-accepted connections.

---

## API Reference

### KCP (state machine)

Core type: `kcp_rs::KCP`. Construct with `KCP::new(conv, token, output)`.

| Method | Description |
|--------|-------------|
| `send(data)` | Queue user bytes. Stream mode appends to the previous segment; otherwise fragments into `≤255` segments. `Err(TooManyFragments)` if oversized. |
| `recv()` / `recv_bytes()` | Return the next reassembled message as `BytesMut` / zero-copy `Bytes`. `Err(NoData)` when nothing is ready. |
| `peeksize()` | Size of the next complete message, without consuming. |
| `input(data, ack_no_delay)` | Feed one inbound wire segment (or several concatenated). Validates `conv`, parses ACK/PUSH/WASK/WINS, queues ACKs. |
| `update(current_ms)` | Advance timers; returns ms until the next meaningful event. Call on a timer. |
| `flush()` / `flush_with_current(current)` | Force-flush pending segments/ACKs/window probes to `output`. |
| `check(current_ms)` | `ms` until `update()` should next be called (Go `Check()`). |
| `set_mtu` / `set_mss` | Change MTU (and MSS). |
| `set_snd_wnd` / `set_rcv_wnd` | Change windows (0 = no change). |
| `set_nodelay(nodelay, interval, resend, nc)` | Raw knobs: `nodelay` (0/1), `interval` ms, fast-resend threshold, `nc` (no congestion control). |
| `set_stream_mode(bool)` | Stream vs message mode. |
| `set_mode(mode, nodelay, interval, resend, nc)` | Apply a [`KcpMode`](#kcpconfig--kcpmode) curve, or manual knobs. |
| `apply(&KcpConfig)` | Apply a full [`KcpConfig`](#kcpconfig--kcpmode) via the setters. |
| `wait_send()` | Segments currently queued or in-flight (backpressure hint). |
| `is_dead()` | `true` after `dead_link` (20) retransmits — connection is gone. |
| `reset()` | Reset all send/receive state (same `conv`/`token` kept). |
| `mtu` / `mss` / `snd_wnd` / `rcv_wnd` / `rmt_wnd` / `cwnd` / `rx_rto` / `rx_srtt` / `interval` / `conv` / `snd_nxt` / `snd_una` / `rcv_nxt` | Read accessors for diagnostics. |

Errors (`kcp_rs::KcpError`): `NoData`, `TooManyFragments`, `InvalidLength`, `ConvMismatch`, `UnknownCommand`, `InvalidSegment`, `BufferTooSmall`.

### KcpConfig / KcpMode

`kcp_rs::KcpConfig` is a plain value object (`Default`-constructible); tuning is applied with `KCP::apply` / `KCP::set_*`. `kcp_rs::KcpMode` selects a Go kcptun `--mode` curve:

| Mode | nodelay | interval | resend | nc | Typical use |
|------|---------|----------|--------|----|-------------|
| `Normal` | 0 | 40 | 2 | 1 | Conservative |
| `Fast` | 0 | 30 | 2 | 1 | Default-ish |
| `Fast2` | 1 | 20 | 2 | 1 | Lower latency |
| `Fast3` *(default)* | 1 | 10 | 2 | 1 | Lowest latency |
| `Manual` | — | — | — | — | Use `KcpConfig.nodelay/interval/resend/nc` |

```rust
let cfg = KcpConfig {
    mode: KcpMode::Fast2,     // or KcpMode::Manual + explicit nodelay knobs
    stream: true,
    ..KcpConfig::default()
};
kcp.apply(&cfg);
```

See the [Configuration](#configuration) table for every field and default.

### Reed-Solomon FEC

Optional forward error correction (`kcp_rs::fec`). Wire-compatible with Go kcp-go's FEC framing (`seq 4B + type 2B`; types `0x00f1` data / `0x00f2` parity).

```rust
use kcp_rs::fec::{FecDecoder, FecEncoder, fec_kcp_from_recovered};

// 10 data + 3 parity shards; `offset` reserves crypto header space (0 when
// crypto wraps the whole FEC frame).
let mut enc = FecEncoder::new(10, 3, 0).unwrap();
let mut dec = FecDecoder::new(10, 3).unwrap();

// ── sender: wrap each raw KCP segment ──
let (data_frame, parity_frames) = enc.wrap_kcp_packet(kcp_segment, 500);
// send data_frame, then any parity_frames (emitted when a group of 10 fills)

// ── receiver: feed every frame; the decoder returns recovered data packets ──
let recovered = dec.decode(&incoming_frame);
for r in recovered {
    if let Some(kcp) = fec_kcp_from_recovered(&r) {
        receiver_kcp.input(kcp, true).unwrap();
    }
}
```

Higher-level helpers:

- `fec_expand_packets(&mut encoder, &[Bytes], rto_ms)` — expand raw KCP segments into data (+ parity) frames for batching onto the wire.
- `fec_kcp_from_recovered(&[u8])` — strip the 2-byte SIZE field from a recovered shard (RS padding is trimmed here, matching Go's `r[2:sz]`).
- Constants: `FEC_HEADER_SIZE` (6), `FEC_TYPE_DATA`, `FEC_TYPE_PARITY`.

### SNMP counters

Go-compatible statistics (`kcp_rs::snmp`), matching the `kcp-go` CSV layout (`bytes_sent`, `bytes_received`, `in_segs`, `out_segs`, `retrans_segs`, `lost_segs`, `repeat_segs`, FEC counters, …).

```rust
use kcp_rs::{snmp_add, snmp_enable, snmp_store, DEFAULT_SNMP};

snmp_enable();                  // opt-in: enable hot-path counter updates
// ... run transfers ...
println!("{}", DEFAULT_SNMP);   // Display prints a kcp-go style CSV row
```

Collection is **opt-in** (`snmp_enable`) so the hot path stays free when you don't need stats.

### KcpStream (async)

`kcp_rs::KcpStream` (feature `async`) — a reliable stream over UDP with background input/flush loops. Implements `knet::AsyncRead + AsyncWrite`.

| Method / builder | Description |
|------------------|-------------|
| `connect(addr)` | Dial `addr`, binding an ephemeral local UDP socket. Returns a builder. |
| `with_transport(transport, remote)` | Build over an existing [`PacketTransport`](#packettransport) (e.g. `CryptoTransport`). |
| `.connect_timeout(Duration)` | Require a first peer response (probe `WINS` / ACK) within the timeout; `build` fails with `TimedOut` otherwise. |
| `.mtu(v)` / `.sndwnd(v)` / `.rcvwnd(v)` | Size knobs. |
| `.mode(KcpMode)` / `.nodelay(n, i, r, c)` | Latency curve / raw knobs. |
| `.stream(bool)` / `.acknodelay(bool)` / `.conv(v)` / `.token(v)` | Protocol knobs. |
| `.connected(bool)` | `true` when the transport socket is already `connect()`ed (uses `send_batch`); default `true` for `connect`, `false` for `with_transport`. |
| `.fec(data, parity)` | Enable Reed-Solomon FEC (both `> 0`). |
| `.config(KcpConfig)` | Apply a full config value. |
| `.build().await` | Construct and start the background loops. |
| `read` / `write_all` / `flush` | Standard async I/O (`knet::AsyncReadExt` / `AsyncWriteExt`). |
| `set_kcp_nodelay` / `set_kcp_window_size` / `set_kcp_mtu` / `set_kcp_stream_mode` / `set_kcp_acknodelay` | KCP-specific post-construction tuning (`set_kcp_*` prefix avoids collisions). |
| `set_nodelay(bool)` / `nodelay()` | TCP-style Nagle toggle (`true` → KCP fast path) + getter. |
| `set_read_timeout(Option<Duration>)` / `read_timeout()` | Read deadline (`TimedOut` after it elapses with no data). |
| `set_write_timeout(Option<Duration>)` / `write_timeout()` | Write deadline when blocked on a full send window. |
| `shutdown(std::net::Shutdown)` | Half-close: `Write` stops writes + flushes, `Read` surfaces EOF, `Both` = `close()`. |
| `remote_addr()` / `local_addr()` | Peer / local addresses. |
| `peek(&mut [u8])` | Non-blocking peek at buffered inbound (`WouldBlock` when empty). |
| `take_error()` | Last background-loop I/O error, clearing it. |
| `split()` / `into_split()` | Borrowing / owned read+write halves (tokio-style); connection closes on last owned-half drop. |
| `readable()` / `writable()` | Await data-available / window-open readiness. |
| `read_shared(&mut [u8])` / `write_all_shared(&[u8])` | Concurrent `&self` read/write (used by the session layer). |
| `close()` / `is_closed()` / `is_dead()` | Close / query state. |
| `snd_wnd()` / `rcv_wnd()` / `wait_send()` / `last_activity_ms()` | Backpressure & activity diagnostics. |

### KcpListener (async)

`kcp_rs::KcpListener` (feature `async`) — server-side listener that binds **one** UDP socket and demultiplexes inbound datagrams by source address into per-peer [`KcpStream`]s. The full example lives in [Listen & connect](#listen--connect-server--client).

| Method / builder | Description |
|------------------|-------------|
| `bind(addr)` | Bind a UDP socket to `addr`; returns a `KcpListenerBuilder` (awaitable directly via `IntoFuture`, or `.build().await`). |
| `.mtu(v)` / `.sndwnd(v)` / `.rcvwnd(v)` | Size knobs (propagated to accepted conns). |
| `.mode(KcpMode)` / `.nodelay(n, i, r, c)` | Latency curve / raw knobs. |
| `.stream(bool)` / `.acknodelay(bool)` / `.conv(v)` / `.token(v)` | Protocol knobs. |
| `.fec(data, parity)` | Enable Reed-Solomon FEC on accepted conns (both `> 0`). |
| `.config(KcpConfig)` | Apply a full config value. |
| `.build().await` | Bind the socket, spawn the demux reader, return the listener. |
| `accept()` | Await the next client — `io::Result<(KcpStream, SocketAddr)>` (per-peer conn + source addr). |
| `accept_timeout(t)` | Await the next client within `t`, else `io::ErrorKind::TimedOut`. |
| `try_accept()` | Non-blocking: `Ok(Some(conn))` if pending, `Ok(None)` otherwise. |
| `take_error()` | Surface + clear the last demux-reader transport error. |
| `local_addr()` | Local address of the listen socket. |
| `close()` | Stop accepting new clients (existing accepted conns are unaffected). |

`KcpListener` is **not** `Clone`; the demux reader lives inside it. Dropping the listener closes it.

### PacketTransport

The pluggable datagram layer under `KcpStream` — this is where crypto lives (never inside `KcpStream`).

```rust
pub trait PacketTransport: Send + Sync {
    fn recv<'a>(&'a self, buf: &'a mut [u8])
        -> Pin<Box<dyn Future<Output = io::Result<usize>> + Send + 'a>>;
    fn try_recv(&self, buf: &mut [u8]) -> io::Result<usize>;
    fn send_batch<'a>(&'a self, packets: &'a [Bytes])
        -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + 'a>>;
    fn send_batch_to<'a>(&'a self, packets: &'a [Bytes], target: SocketAddr)
        -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + 'a>>;
    fn send_urgent<'a>(&'a self, packets: &'a [Bytes])
        -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + 'a>> { self.send_batch(packets) }
    fn send_urgent_to<'a>(&'a self, packets: &'a [Bytes], target: SocketAddr)
        -> Pin<Box<dyn Future<Output = io::Result<()>> + Send + 'a>> { self.send_batch_to(packets, target) }
    fn local_addr(&self) -> io::Result<SocketAddr>;
}
```

Built-in implementations:

- `knet::DatagramSocket` — plain UDP (also `TcpRaw` on some platforms).
- `kcptun_common::CryptoTransport` — encrypts/decrypts (and offloads CPU-heavy crypto), then delegates to UDP.

`send_urgent*` is the ACK path — crypto wrappers use a separate buffer here to avoid lock contention with the data path.

---

## Configuration

`KcpConfig` fields and defaults:

| Field | Default | Meaning |
|-------|---------|---------|
| `mtu` | `1350` | Max transmit unit; MSS = `mtu - 24`. |
| `sndwnd` | `128` | Send window (segments in flight). |
| `rcvwnd` | `128` | Receive window. |
| `mode` | `KcpMode::Fast3` | Latency curve (see above). |
| `nodelay` | `1` | Used only when `mode = Manual`. |
| `interval` | `10` | Update interval ms (`Manual` mode). |
| `resend` | `2` | Fast-resend threshold (`Manual` mode). |
| `nc` | `1` | No congestion control (`Manual` mode). |
| `stream` | `true` | Stream (TCP-like) vs message mode. |
| `acknodelay` | `true` | Flush ACKs immediately on input. |
| `datashard` | `0` | FEC data shards; `0` disables FEC. |
| `parityshard` | `0` | FEC parity shards; must be `> 0` together with `datashard`. |
| `conv` | `0xDEAD_BEEF` | Conversation ID (kcp-go default). |
| `token` | `0` | Auth token. |

> Library default has **FEC off**. Go kcptun product defaults (e.g. FEC 10/3) belong to the CLI mapping in `kcptun-common`.

---

## Wire Protocol

KCP segment — **24-byte little-endian header** + payload:

```
 0      4       8       12      16      20      24
+------+-------+-------+-------+-------+-------+
| conv | cmd   | frg   | wnd   | ts    | sn    |
+------+-------+-------+-------+-------+-------+
| una  | len   | data...                       |
+------+-------+                               |
+-----------------------------------------------+
```

| Field | Bytes | Meaning |
|-------|-------|---------|
| `conv` | 0–4 | Conversation ID — mismatches are rejected. |
| `cmd` | 4 | `81`=PUSH, `82`=ACK, `83`=WASK (window probe), `84`=WINS (window ad). |
| `frg` | 5 | Fragment index (message mode); `0` = last/first. |
| `wnd` | 6–8 | Sender's free receive window (advertised to peer). |
| `ts` | 8–12 | Timestamp (ms) for RTT / fast retransmit. |
| `sn` | 12–16 | Sequence number. |
| `una` | 16–20 | Oldest unacknowledged SN — cumulative ACK. |
| `len` | 20–24 | Payload length. |

Go-compatible constants: `MTU` 1400, `KCP_OVERHEAD` 24, `KCP_DEFAULT_WND` 32, `KCP_MAX_FRAG` 255.

---

## Reliability & Data Correctness

KCP guarantees — under arbitrary packet loss, duplication, reordering and delay (each direction independently):

1. **No loss** — every accepted byte is delivered exactly once (duplicates are dropped by the receive window).
2. **In order** — the receiver buffer (`rcv_buf`) only releases segments to the read queue when they are contiguous from `rcv_nxt`.
3. **Byte-exact** — stream reassembly preserves the exact byte sequence written by the sender.

These properties are **verified by the crate's standalone tests** (see [Testing](#testing)), which run two KCP instances over an in-memory channel that drops 20% of packets, duplicates 5%, adds jitter and delay — and assert the received bytes are byte-for-byte identical (length + content + FNV-1a checksum). The FEC test additionally drops a data shard and recovers it byte-exactly from parity.

---

## Go Compatibility

| Aspect | Go kcp-go v5 | kcp-rs |
|--------|--------------|--------|
| Segment header | 24B LE `conv|cmd|frg|wnd|ts|sn|una|len` | ✅ Same |
| Commands | PUSH=81, ACK=82, WASK=83, WINS=84 | ✅ Same |
| Mode curves | `NoDelay(nodelay, interval, resend, nc)` | ✅ Same |
| RTO / backoff | `rx_minrto` 30 (nodelay) / 100, half-linear backoff | ✅ Same |
| Dead link | 20 retransmits → `0xFFFFFFFF` | ✅ Same |
| Congestion | RFC 5681 + rate halving | ✅ Same |
| Stream mode | append-to-last-segment, `frg=0` | ✅ Same |
| FEC framing | `[seq 4][type 2][size 2][kcp…]` | ✅ Same |
| SNMP counters | kcp-go CSV layout | ✅ Same |

> The core `KCP` control flow mirrors Go kcp-go v5 line-for-line, which is why crate-level clippy lints are suppressed — do not "fix" them.

---

## Testing

All tests are **self-contained** to this crate — no client/server binaries, no Go e2e harness, no network beyond `127.0.0.1` loopback.

### One command

```bash
bash kcp-rs/test.sh
```

This runs, in order:

1. **Sync data-correctness** — `cargo test -p kcp-rs` (default features, no async)
2. **Async integrity** — `cargo test -p kcp-rs --features async`

### Individual test targets

```bash
# Sync: in-memory flaky-channel reliability + FEC recovery (default features)
cargo test -p kcp-rs --test data_correctness

# Sync: run just the loss test
cargo test -p kcp-rs --test data_correctness reliable_delivery_with_20pct_loss

# Async: KcpStream integrity over real localhost UDP
cargo test -p kcp-rs --features async --test kcpconn_integrity

# Full crate suite
cargo test -p kcp-rs                    # sync
cargo test -p kcp-rs --features async   # + async
```

### What the tests verify

| Test file | Verifies |
|-----------|----------|
| `tests/data_correctness.rs` | Byte-exact delivery (length + content + FNV-1a checksum) over a clean link, a 20%-loss link, and a loss+dup+reorder+delay link; FEC recovers a dropped data shard byte-exactly. |
| `tests/kcpconn_integrity.rs` | Bidirectional byte-exact transfers through real `KcpStream` over localhost UDP, with and without FEC 10/3. |
| `tests/kcpconn_listener.rs` | Server **listen** / client **connect**: accept echo round-trip, multi-peer demux, listener serves a fresh client after a close. |

---

## Architecture

```
kcp-rs/
├── Cargo.toml          — deps: bytes, parking_lot, crossbeam, reed-solomon-erasure, crc32fast; optional knet-rs
├── AGENTS.md           — AI-orientation map for this crate
├── test.sh             — standalone test runner (sync + async)
├── tests/
│   ├── data_correctness.rs   — sync reliability + FEC data-correctness tests
│   ├── kcpconn_integrity.rs  — async KcpStream integrity over localhost UDP
│   └── kcpconn_listener.rs   — server listen / client connect (accept, demux, reconnect)
└── src/
    ├── lib.rs          — crate root + re-exports (large intentional clippy allow-list)
    ├── kcp.rs          — core KCP state machine (windows, RTO, flush, input, NoDelay)
    ├── config.rs       — always-on KcpConfig / KcpMode; KCP::apply / set_mode
    ├── segment.rs      — 24-byte LE wire header, Command enum, SegmentPool
    ├── fec.rs          — FecEncoder / FecDecoder / fec_expand_packets / fec_kcp_from_recovered
    ├── snmp.rs         — global DEFAULT_SNMP atomic counters; snmp_enable / snmp_add / snmp_store
    └── conn.rs         — (feature async) KcpStream + KcpStreamBuilder + KcpListener + PacketTransport
```

Data flow (async `KcpStream`):

```
Write path:  AsyncWrite::poll_write → write_buf → flush loop
             → KCP::send/update/flush → output callback → [FEC] → PacketTransport::send_batch
Read path:   PacketTransport::recv → [FEC decode] → KCP::input → recv_bytes → read_buf → AsyncRead
```

---

## Troubleshooting

### `send()` returns `Err(TooManyFragments)`

A single `KCP::send` call cannot exceed `255` MSS-sized fragments (`KCP_MAX_FRAG`). Chunk large writes into `≤ (KCP_MAX_FRAG − 1) × MSS` bytes per call (the async `KcpStream` does this internally).

### Nothing arrives when I `recv()`

KCP only releases data once it is contiguous from `rcv_nxt`. If a segment is missing, `recv()` stays empty until retransmission delivers it. Make sure your event loop calls `update()` on a timer (not just when data arrives) so RTO retransmission can fire, and pass `ack_no_delay = true` to `input()`.

### `KcpListener::accept()` never returns

The listener only learns about a client when it receives that client's **first datagram**. A `KcpStream` sends nothing until there is data to send (or a window probe fires), so `accept()` blocks until the client writes. A silent client that never sends will never be accepted.

### Congestion window caps throughput

The default congestion window starts small (slow-start) and `sndwnd`/`rcvwnd` cap it. For bulk transfers bump `sndwnd`/`rcvwnd`, or set `nc=1` (no congestion control) — e.g. `KcpMode::Fast3` already sets `nc=1`.

### Where is encryption?

Deliberately not in this crate. Crypto lives in `kcrypt-rs`; wrap your datagrams with a `PacketTransport` (e.g. `kcptun_common::CryptoTransport`) beneath `KcpStream`. See [`kcrypt-rs/AGENTS.md`](../kcrypt-rs/AGENTS.md) for its API surface.
