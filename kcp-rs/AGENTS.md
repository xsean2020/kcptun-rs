<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-07-22 | Updated: 2026-08-16 (tokio-only refactor) -->

# kcp-rs

## Purpose

KCP ARQ (Automatic Repeat-reQuest) reliable UDP protocol state machine — port of Go `github.com/xtaci/kcp-go/v5`. Ordered, reliable delivery over UDP with congestion control, Reed-Solomon FEC, and atomic SNMP counters. **All crypto lives in `kcrypt-rs`** — kcp-rs has **no** dependency on it and **no** crypto re-exports; depend on `kcrypt-rs` directly for `BlockCrypt` / `CryptEngine` / `CryptoBuf` / wire packing.

Async surface (optional): `KcpStream` is a tokio-TCP-shaped `AsyncRead`/`AsyncWrite` over UDP with optional FEC. **No encryption inside KcpStream** — crypto is an external `PacketTransport` (see `kcptun-common::CryptoTransport`).

## Key Files

| File | Description |
|------|-------------|
| `Cargo.toml` | Deps: `bytes`, `parking_lot`, `crossbeam`, `reed-solomon-erasure`, `crc32fast`, `thiserror`; optional `knet-rs`. **No** `kcrypt-rs` dependency |
| `src/lib.rs` | Crate root; large intentional `#![allow(clippy::…)]` list — do not "fix" |
| `src/kcp.rs` | Core `KCP` state machine: windows, RTO, flush, input, NoDelay |
| `src/segment.rs` | 24-byte LE wire header, `Command` enum, `SegmentPool` (SegQueue) |
| `src/fec.rs` | `FecEncoder` / `FecDecoder` / `fec_expand_packets` / `fec_kcp_from_recovered`; header types `0x00f1` / `0x00f2` / `0x00f3` |
| ~~`src/crypto_buf.rs`~~ | **Removed** (B2) — moved to `kcrypt-rs::wire`; see `../kcrypt-rs/AGENTS.md` |
| `src/conn.rs` | (feature `async`) `KcpStream` + `SharedIoState` + builder + background loops + poll impls + split halves; exposes monotonic `last_activity_ms` for upper-layer session expiry; **TcpStream-aligned surface** (`set_nodelay(bool)`, `shutdown(Shutdown)`, read/write timeouts, `split`/`into_split`, `readable`/`writable`, `peek`, `take_error`, builder `.connect_timeout`) + KCP tuning prefixed `set_kcp_*`; shared builder setters use one internal macro (re-exported for listener builders). TX path uses `flush_tx_batch` (non-blocking `sendmmsg` fast path → async `send_batch` fallback on `WouldBlock`, pipeline §15–§16); `feed_batch` (external worker path, `background_input=false`) does inline sync drain-and-flush via `drain_and_flush_tx` before falling back to `flush_notify` (pipeline §14) |
| `src/transport.rs` | (feature `async`) `PacketTransport` trait + impls (`knet::DatagramSocket`, per-peer `PeerTransport`) + `PeerQueue` (listener demux) + `TransportWrapper`; `MAX_DATAGRAM`/`MAX_RETAINED_PEER_BUFFERS` consts |
| `src/listener.rs` | (feature `async`) `KcpListener` (shared-UDP demux) + `KcpTcpListener` (raw TCP) + builders + `spawn_listener_reader`; **TcpListener-aligned surface** (`accept_timeout`, `try_accept` [KcpListener only], `take_error`, builders are `IntoFuture` so `bind(addr).await` works); depends on `conn` + `transport` |
| `src/sharded.rs` | (feature `async`) `ShardedKcpListener` + `ShardedKcpListenerBuilder` — sharded worker pipeline architecture: RX reader → bounded channel → N worker tasks with session affinity (`fast_hash_peer % worker_count`); workers build `KcpStream` with `background_input(false)` and feed inbound via `feed_batch` (§10.2 Mode B: decrypt + KCP + encrypt all on the same worker); public entry points `bind_listener(addr)` / `from_socket_listener(sock)` |
| `src/config.rs` | **Always-on** `KcpConfig` / `KcpMode`; `KCP::apply` / `set_mode` (B1) |
| `src/snmp.rs` | Global `DEFAULT_SNMP` atomic counters; `snmp_enable` / `snmp_add` / `snmp_store` |
| `README.md` | User-facing usage guide: sync + async API, wire format, config, testing |
| `test.sh` | Standalone test runner: sync (default) + `async` |
| `tests/data_correctness.rs` | Sync reliability + FEC data-correctness over in-memory flaky channel |
| `tests/kcpconn_integrity.rs` | Async `KcpStream` integrity over localhost UDP |
| `tests/kcpconn_listener.rs` | Server listen / client connect: accept echo, multi-peer demux, serve-after-close |

## Features

| Feature | Effect |
|---------|--------|
| *(default)* | Sync KCP only — no async deps |
| `async` | `KcpStream` + `PacketTransport` via `knet-rs` (tokio) |

## Async API sketch (`async`)

```rust
// Raw UDP, optional FEC; crypto is NOT here
let conn = KcpStream::connect("1.2.3.4:29900")
    .mtu(1400)
    .fec(10, 3)
    .mode(KcpMode::Fast3)
    .connect_timeout(Duration::from_secs(3)) // first WINS/ACK response, else TimedOut
    .build()
    .await?;

// Or plug a custom PacketTransport (e.g. CryptoTransport from kcptun-common)
let conn = KcpStream::with_transport(transport, cfg).await?;
```

- **TcpStream-aligned surface** (learn-cost ≈ `tokio::net::TcpStream`):
  `set_nodelay(bool)`/`nodelay()`, `set_read_timeout`/`set_write_timeout` (+ getters),
  `shutdown(std::net::Shutdown)` half-close, `peek()`, `take_error()`,
  `split()`/`into_split()` (owned halves close the connection on last-half drop),
  `readable()`/`writable()`. KCP-specific tuning is prefixed **`set_kcp_*`**
  (`set_kcp_nodelay(n,i,r,nc)`, `set_kcp_window_size`, `set_kcp_mtu`,
  `set_kcp_stream_mode`, `set_kcp_acknodelay`) so the plain `set_*` names stay free.
- `poll_shutdown`/`poll_close` = **write-half close** (tokio semantics), NOT full
  close — the production stack calls `KcpStream::close()` explicitly. KCP has **no wire
  FIN**, so peer-aware half-close lives at the SMUX/session layer.
- `connect_timeout` forces a `WASK` probe and waits for any conv-valid inbound
  (`WINS`/ACK); it proves reachability + conv match, **not** connection establishment
  (KCP has no handshake). Requires the background input loop.
- `PacketTransport`: datagram send/receive plus optional batch operations;
  `recv_vec` / `try_recv_vec` let queue-backed transports transfer reusable
  owned packet storage without an extra copy.
- FEC: `.fec(datashard, parityshard)` on builder; encode on flush, decode on input.
- `KcpListener`: `bind` → `accept() -> (KcpStream, SocketAddr)`. One bound UDP socket; demux by source addr
  via per-peer queue-backed `PeerTransport`. Reconnect = fresh client session (KCP SN continuity blocks
  same-stream reuse after a server-side close).
- Production `kcptun-client` / `kcptun-server` use this `KcpStream` through the
  shared `kcptun_common::KcptunSession` stack. Server UDP uses the single-reader
  `KcpListener` demultiplexer before constructing per-peer connections.

## For AI Agents

### Working In This Directory

- **Wire compatibility with kcp-go v5 is the primary constraint.** Control flow mirrors Go; crate-level clippy allows exist for that reason.
- `KCP::input()` must queue ACKs for **every** received Push segment.
- `snd_buf` cleanup: ACKed segments removed from the **front** in `flush()` (Go `k.snd_buf = k.snd_buf[1:]`).
- Constants (`IKCP_RTO_*`, `IKCP_PROBE_*`, `KCP_DEFAULT_WND=32`, cmds 81–84) must match Go.
- Crypto is entirely in `kcrypt-rs` — depend on it directly. kcp-rs has no crypto API / re-exports.
- `kcrypt_rs::wire::CryptoBuf` nonce is **not** the CFB IV (IV is fixed `GO_CFB_IV`); nonce is `[counter 8B][session_id 8B]`.
- SNMP collection is **opt-in** (`snmp_enable`) so hot paths stay free when unused.
- **Do not put crypto inside `KcpStream`.** Use `PacketTransport` wrappers (`CryptoTransport`).
- Snappy stays **outside** KcpStream (session-level over KCP user data in binaries / common).
- **Cancelable recv**: the input loop (conn.rs) and listener reader (listener.rs) race their socket `recv` against a `knet::CancellationToken` via `knet::race` instead of a 100ms poll tick, so `close()` cancels the recv immediately. There is **no** 100ms close-polling tick on the recv path — do not reintroduce one. Both the conn and the listener own a `cancel_token` cancelled in `close()`.
- **Non-blocking TX fast path**: all send paths (`try_drain_and_send`, flush loop fast/second drain, `feed_batch`) use `flush_tx_batch` which tries `try_send_batch` (non-blocking `sendmmsg` on Linux) first, falling back to async `send_batch` only on `WouldBlock` or `Ok(0)`. This eliminates the reactor scheduling hop per burst when the kernel send buffer has room (pipeline §15: worker only `try_push` to TX, not `await send`). The external-worker path (`feed_batch`) uses the sync variant `drain_and_flush_tx` (pipeline §14: `drain` + `flush`).
- **Sharded worker pipeline** (`sharded.rs`): `ShardedKcpListener` implements the three-stage pipeline (RX → bounded channel → Worker → TX). The reader task does `recvmmsg` (`try_recv_batch_from_into`) and `try_push`es `(peer, Vec<u8>)` to the correct worker's bounded `async_channel` — it never decrypts, runs KCP, or awaits a send. Each worker owns a `SessionMap` (`HashMap<SocketAddr, KcpStream>`) and processes inbound via `feed_batch` (Mode B: decrypt + KCP + encrypt on the same task). Session affinity: `fast_hash_peer(peer) % worker_count` routes to a fixed worker. Worker event loop: `drain_udp_batch` → `process_session` → `flush_ready_sessions`, with adaptive spin→park when idle. Build is async (`knet::spawn_task`) so the worker doesn't block on `KcpStream::build().await`.
- **Listener admission + lifecycle** (`sharded.rs`): a server session is created from a *single* inbound datagram, so `process_session` runs a pre-admission integrity gate — it builds the `PacketTransport` (crypto wrapper included) and calls `decrypt_packet_in_place` on the first burst **before** `KcpStream::build()`. A peer that cannot produce a valid CRC32 / AEAD tag gets no `KcpStream`, no flush loop and no accept-backlog entry (`WorkerPoolStats::unauthenticated_drops`). With no crypto configured the default `decrypt_packet_in_place` is the identity, so the gate is a no-op — the `max_sessions_per_worker` cap is the only bound in that configuration.
- `WorkerPoolLimits` defaults are **bounded, not unlimited**: `max_sessions_per_worker = 4096`, `idle_timeout = 600s`, `building_timeout = 10s`. KCP has no keepalive of its own, so a peer that sends one datagram and vanishes never trips `is_dead()`; `idle_timeout` is what frees its slot. `last_activity_ms` is stamped by `SharedIoState::mark_activity()` from **both** the input loop and the `feed_batch`/`feed_single` paths — server sessions run `background_input(false)` and have no input loop, so dropping the feed-path stamp makes the reaper kill live sessions.
- **Reaping closes.** `reaper_sweep` and `KcpListener::remove_peer` call `KcpStream::close()` on what they remove. The session map holds a non-owning clone (`detach_owner`), so a bare `remove` leaks the flush-loop task and leaves anything blocked on the stream parked forever.
- The lifecycle sweep runs as a `SWEEP_PARK_MS` timer task on each worker's own current-thread runtime, not off RX wakeups: a shard that goes completely silent has no wakeups, and the old wakeup-counted sweep stopped running exactly when reaping mattered most.
- `KCPTUN_WORKER_THREADS` controls the default sharded-listener worker count;
  an explicit Builder `worker_count(n)` takes precedence.

### Testing Requirements

- Standalone runner: `bash kcp-rs/test.sh` (sync default → `async`)
- Sync data-correctness: `cargo test -p kcp-rs --test data_correctness`
- Async integrity: `cargo test -p kcp-rs --features async --test kcpconn_integrity`
- Listener / connect: `cargo test -p kcp-rs --features async --test kcpconn_listener`
- In-module unit tests where present
- Async: `cargo test -p kcp-rs --features async`
- Interop: `bash test_e2e.sh` after segment/KCP/FEC changes
- Stress: `make stress` for flush/lock behavior under load

### Common Patterns

- Output callback: `Box<dyn FnMut(bytes::Bytes) + Send>` on `KCP`
- NoDelay modes applied by binaries via `nodelay/interval/resend/nc`
- FEC optional at **session / KcpStream layer** (`FecEncoder`/`FecDecoder`); no core-KCP FEC API
- Recovered FEC payload: `fec_kcp_from_recovered` (Go `r[2:sz]`); reconstruct present-flag is `true` = present
- Public API: `KCP`, `KcpConfig`/`KcpMode`, FEC + SNMP helpers. Crypto types (`BlockCrypt`, `CryptEngine`, `CryptoBuf`, `encrypt_batch`, wire helpers) live in `kcrypt-rs` — **not** re-exported here.
- With `async`: also `KcpStream`, `KcpConfig`, `KcpMode`, `PacketTransport`
  (including reusable owned receive buffers), `KcpListener`, `ShardedKcpListener`
- Wire packing / encrypt / offload heuristics (`CryptoBuf`, `encrypt_batch`, `decrypt_cfb_in_place`, `should_cpu_block_*`, `OffloadProfile`, …): see `../kcrypt-rs/AGENTS.md` — all live in `kcrypt_rs::wire`.

## Dependencies

### Internal

- None — crypto lives in `kcrypt-rs` (not a dependency of this crate)

### External

- `bytes`, `crossbeam`, `parking_lot`, `reed-solomon-erasure`, `crc32fast`
- optional `knet-rs` via feature `async`

<!-- MANUAL: -->
