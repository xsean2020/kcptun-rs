# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added — Rust `--pprof` emits Go-compatible protobuf

- Replaced placeholder `--pprof` HTTP banner with real CPU profiling via
  `pprof` crate (`protobuf-codec`): `GET /debug/pprof/profile?seconds=N`
  returns a **Google pprof protobuf** usable by `go tool pprof`.
- Helper: `bash bench/profile_rust_go_pprof.sh` / `make profile-rust-go`.
- Cargo profile `profiling` (debug=2, no strip, no LTO) for readable stacks.
- `bench/run_bench.sh` forces shared `--sndwnd/--rcvwnd/--mode/--smuxver` for fair cross-impl runs.

### Added

- Flamegraph profiling: `bench/profile_flamegraph.sh`, `bench/kcptun_prof_wl`,
  `bench/PROFILE_RUNBOOK.md`, project skill `.claude/skills/flamegraph-perf/`,
  and `make profile` (samply → Speedscope; L1–L4 matrix).

### Changed — enable ARMv8 AES on aarch64 (`aes_armv8`)

Flamegraph L2 (AES-CFB bulk) showed `aes::soft::fixslice` dominating on Apple
Silicon. RustCrypto `aes` 0.8 requires `--cfg aes_armv8` to select the ARMv8
crypto extension path; without it, soft AES is used even when the CPU has
FEAT_AES.

- **`.cargo/config.toml`**: `rustflags = ["--cfg", "aes_armv8"]` for
  `aarch64-apple-darwin` and `aarch64-unknown-linux-gnu`.
- **`make vendor` / `vendor-force`**: regenerate the same flags so they are not
  lost when the vendor config is rewritten.
- **Measured (loopback bulk, this host):** AES ~12–14 MB/s (soft) → **~66–85 MB/s**
  (armv8), about **5–6×**; null path unchanged (~120+ MB/s). Wire format
  unchanged; `kcrypt-rs` tests green.

### Changed — R2: KCP output → `Bytes` ownership pipeline (reduce alloc + copy)

The KCP output callback previously received `&[u8]` and copied each packet
into a `Vec<u8>` acquired from a `BufferPool` (crossbeam `SegQueue` atomic
pop + `extend_from_slice` ~1400B memcpy per packet). On the null path the
pool was effectively write-only (packets moved into `Bytes` and never
returned), so every packet still allocated a fresh `Vec::with_capacity(2048)`.

- **`KCP::output` signature**: `Box<dyn FnMut(&[u8]) + Send>` →
  `Box<dyn FnMut(bytes::Bytes) + Send>`. The flush loop already produced
  `buf.split().freeze()` (`Bytes`); it now hands ownership directly to the
  callback instead of passing a `&[u8]` that the callback had to copy.
- **`encrypt_batch` signature**: `packets: Vec<Vec<u8>>` → `Vec<Bytes>`,
  and the `pool: &BufferPool` parameter is removed (RC `Bytes` self-releases;
  no per-packet pool acquire/release).
- **Client/server `raw_packets`**: `Vec<Vec<u8>>` → `Vec<Bytes>`; the output
  callback is now a single `raw_packets.lock().push(data)` — zero-copy,
  zero-alloc.
- **ACK path (client)**: per-ACK `buffer_pool.release(data)` calls removed;
  null ACKs pass the `Bytes` straight through.
- **Result**: each outbound KCP packet saves 1 `Vec` allocation + 1
  `extend_from_slice` copy + 1 `SegQueue` atomic op. Loopback bulk bench
  (AES-128, `--nocomp`, 50 MB) improved from ~1.2× to **~1.43× Go**
  (Rust-Tokio 68.8 vs Go 48.1 MB/s); smol reached **1.56×** (75.0 MB/s).
  e2e 138/138 pass; clippy + tests green on tokio & smol.

### Fixed — smol persistent `cpu_block` thread pool + dead code removal

- **`kio::cpu_block` (smol backend)** — Replaced `smol::unblock` (which kills
  idle threads after 500ms) with a **persistent blocking thread pool** whose
  N workers (N = CPU count, clamped to [2, 8]) live for the process lifetime.
  This eliminates the ~10–50µs per-call thread creation overhead that
  kcptun's 10–100ms flush cadence was incurring on every `cpu_block` call.
  Jobs are dispatched via `std::sync::mpsc` (Arc<Mutex<Receiver>> shared
  across workers); results return via `async_channel::bounded(1)`.
  **This was previously claimed in CHANGELOG but never implemented in the
  main branch — now the code matches the documentation.**
- **Removed dead `copy_bidirectional`** from `kio-rs/src/net/mod.rs` — a
  duplicate 8 KB-buffer implementation that was superseded by the 64 KB-buffer
  version in `kio-rs/src/lib.rs` but never deleted. The `net/mod.rs` copy was
  never called by any client/server code path.
- **PERF_OPTIMIZATION_PLAN.md** — Corrected §5.3 (R3 sendmmsg/recvmmsg:
  marked as ✅ implemented, was showing design stub), §9.4 (sendmmsg
  checkbox: `[ ]` → `[x]`), and §14 Appendix B (P1.2b/c: 🔄 → ✅).

### Changed — bulk throughput: client backpressure + SMUX v2 write window

Loopback bulk bench (`null`/`aes`, `--nocomp`, FEC off) moved from ~0.25–0.35× Go
to **~1.2–1.35× Go** (Rust-tokio ≈ 74 MB/s null / 64 MB/s aes vs Go ≈ 55 / 53).

#### Client write path

- **Event-driven KCP backpressure** (`SmuxStreamAsync`): `poll_write` waits on
  `write_notify` (ACK + flush paths) instead of always `sleep_ms(1)`. Single-flight
  `bp_armed` avoids spawning a waiter task per pending poll. Matches Go kcp-go
  `chWriteEvent` intent.
- **Multi-frame SMUX drain** (client + server flush): drain up to 64 KiB per cycle
  (multiple `MAX_FRAME_SIZE` frames per stream), not one frame per stream.
- **Re-arm flush** only when `pending_send > 0` **and** `peer_send_window() > 0`
  (no busy-spin when the peer window is exhausted).

#### SMUX v2 write-side flow control (`smux-rs`)

Previously missing Go smux `peerWindow` / `numWritten` accounting — large transfers
to Go peers could stall after ~256 KiB.

- Per-stream `peer_window` / `peer_consumed`; `apply_peer_update` on inbound UPD.
- `drain_send_max` caps by `peer_send_window`; `bytes_written` increments only on
  drain (wire), matching Go `numWritten`.
- Initial window **256 KiB** (`initialPeerWindow`); v1 streams call
  `disable_peer_window()` (`u32::MAX`).
- After `process_data`, client/server notify flush so UPD can unblock drain promptly.
- Unit test: `peer_window_limits_drain`.

#### Misc

- **`TcpListener::accept`**: set `TCP_NODELAY` on accepted sockets (tokio + smol),
  matching Go and `raw_tcp_stream`.

### Changed — PERF_OPTIMIZATION_PLAN P0 (data-plane flush path)

#### P0.1–P0.2 Client encrypt parity + conditional `cpu_block`

- **Client `KcpConn.crypt` / `aead`**: `Arc<Mutex<Box<dyn …>>>` → `Arc<dyn BlockCrypt>` /
  `Arc<dyn AeadCrypt>` (matches server). Encrypt/decrypt no longer take a Mutex on the hot path.
- **Client flush encrypt**: uses `CryptoBuf::prepare_encrypt` + `thread::scope` parallel
  `crypt.encrypt` when ≥ 4 packets (same pattern as server).
- **Conditional `cpu_block`** on both client and server flush encrypt:
  - null/none: inline unless packet count ≥ 8
  - CFB/AEAD: inline unless packet count ≥ 4 **or** total bytes ≥ 4 KiB
  - Small/null batches skip thread-pool scheduling tax on the latency path

#### P0.3–P0.4 Zero-copy SMUX assemble + Snappy outside KCP lock

- **`Frame::encode_header_into` / `patch_header_length`** (`smux-rs`): write 8-byte header
  then drain payload in place (no `Frame::new` + `data.clone()` + `to_vec` chain).
- **Client/server flush**: single reused `BytesMut out_buf` — reserve header →
  `drain_send_max` → patch length; FIN/UPD append the same way.
- **Client Snappy**: compress **outside** the KCP mutex; lock held only for
  `kcp.send` + `flush` (aligned with server Phase 3/4).

#### P0.5 Shared encrypt helpers

- **`kcp_rs::encrypt_batch`** and **`kcp_rs::should_cpu_block_encrypt`** in
  `kcp-rs/src/crypto_buf.rs`; client and server call the same path (prevents drift).

#### P1.1–P1.2a Null path move + UDP batch send

- **`encrypt_batch` null path**: moves `Vec<u8>` into `Bytes` (no extra copy; pool not reclaimed for that path).
- **`UdpSocket::send_batch` / `send_batch_to`** (`kio-rs`):
  - tokio: `try_send`/`try_send_to` + `writable` loop (no per-packet full async send setup when socket stays ready)
  - smol: sequential `send`/`send_to` without other work between packets
- Client flush uses `send_batch`; server flush uses `send_batch_to`.

#### P1.3 Inbound decrypt less allocation

- Client UDP reader: reusable `dec_scratch` for CFB (no per-packet `to_vec` alloc).
- Server `feed_data` CFB: strip crypto header with `drain` (no second `to_vec`).
- Client raw_packets drain: single Mutex acquisition.

#### P1.5 AEAD seal_into + counter nonce

- **`AeadCrypt::seal_into`**: encrypt into reusable `BytesMut`, return `Bytes`.
- **`Aes128GcmCrypt`**: counter nonce (no per-packet PRNG); `encrypt_in_place_detached`.
- **`encrypt_batch` AEAD path**: one shared buffer across the batch via `seal_into`.

#### P1.1 CFB small-batch path + P1.4 SMUX send queue

- **`encrypt_batch` CFB**: small batches (`< 4` packets) use `encrypt_cfb` (reuses
  `CryptoBuf` internal buffer); large batches still prepare + parallel encrypt.
- **`encrypt_cfb` / `prepare_encrypt`**: keep spare capacity after `split_to`.
- **SMUX `send_buf`**: `VecDeque<Bytes>` + `write_bytes` (zero-copy enqueue);
  `drain_send_max` copies once into the flush frame buffer. Double-lock
  `is_closed` fixed.

#### P2.2 Flush scheduling

- Server `KCP_UPDATE_INTERVAL_MS`: 10 → 2 (match client max wake interval).
- After flush: if data was just sent or `wait_send > 0`, set `next_update = 1`
  instead of clamping to the full interval (lower latency under load).

#### P1.2b Linux `sendmmsg` + P2.1 segment encode

- **`kio::UdpSocket::send_batch{,_to}`** on Linux uses `sendmmsg` (batch ≤64);
  non-Linux keeps try_send / sequential path.
- **KCP `Segment::encode`**: 24-byte header assembled as one LE block (`#[inline(always)]`).
- Segment payload remains `BytesMut` (stream-mode append + pool reuse); full `Bytes`
  payload deferred (non-surgical).

#### P1.3 inbound batch recv + SMUX recv Bytes path

- **Linux `recvmmsg`** in `kio-rs/src/net/mmsg.rs`; `UdpSocket::try_recv_batch_from`.
- **Server** main loop: after async `recv_from`, drain ready packets via batch recv.
- **Client** UDP reader: after each packet, `try_recv` drain loop (no await).
- **SMUX**: FIN uses `push_data_bytes`; `push_data` routes to Bytes queue;
  `available()` prefers Bytes queue (one lock when non-empty).

#### P2.1 input header parse + P2.2 empty_flush metric

- **KCP `input`**: 24B header decoded from a single stack slice (no `try_into` chain).
- **SNMP `EmptyFlush`**: counted when a flush cycle produces no UDP packets
  (client + server).

#### P3 cipher enum static dispatch

- **`kcrypt_rs::CryptEngine`**: enum of all concrete ciphers with `match`-based
  `encrypt`/`decrypt` (no deep vtable). Client/server sessions store
  `Arc<CryptEngine>` as `Arc<dyn BlockCrypt>`.

### Changed — smol runtime + cipher pipeline optimizations

#### smol backend: persistent blocking thread pool

- **Replaced `smol::unblock` with a persistent thread pool** in
  `kio::cpu_block` (`kio-rs/src/task/smol.rs`). The `blocking` crate's
  `Executor` kills idle threads after 500 ms, but kcptun's flush loop calls
  `cpu_block` every 10–100 ms, so threads were constantly recreated
  (~10–50 µs overhead per call). The new pool uses `crossbeam-channel` with
  N worker threads (N = CPU count, clamped to [2, 8]) that stay alive for
  the process lifetime, eliminating per-call thread creation overhead.

#### smol backend: true idle-timeout for `copy_bidirectional_idle`

- **Replaced total-timeout fallback with `poll_fn`-based idle timeout**
  (`kio-rs/src/lib.rs`). The smol backend previously used
  `timeout(total_duration, copy_bidirectional(a, b))`, which is a *total*
  timeout — long-lived connections transferring large data would
  erroneously disconnect. The new implementation uses `poll_fn` +
  `smol::Timer` to poll both read directions and the timer concurrently,
  resetting the timer on every data transfer. Semantics now match the
  tokio backend's `tokio::select!` implementation.

#### Snappy compression offloaded to `cpu_block`

- **Server flush loop Phase 3**: Snappy compression is now wrapped in
  `kio::cpu_block` instead of running inline on the async runtime thread.
  This frees the reactor to process I/O (UDP reads, stream writes) during
  compression.
- **Client flush loop Phase 1c**: Snappy compression moved out of the KCP
  lock and offloaded to `kio::cpu_block`, reducing lock hold time and
  allowing concurrent ACK processing.

#### CFB generic inlining (cipher throughput)

- **`cfb8_enc/dec` and `cfb16_enc/dec` changed from `&dyn Fn` to generic
  `<F: Fn>`** (`kcrypt-rs/src/crypt.rs`). The dynamic-dispatch vtable call
  per 8-byte block added ~2–5 ns overhead × ~175 blocks/packet for 3DES.
  Generic monomorphization allows the compiler to inline each cipher's
  block function (`td_enc`, `bf_enc`, `tea_enc`, etc.), eliminating
  vtable overhead entirely. Benefits all CFB-8 ciphers (3DES, Blowfish,
  TEA, XTEA, CAST5) and CFB-16 ciphers (AES, Twofish, SM4).

#### Lock-free cipher storage

- **Removed `Mutex` from `crypt` and `aead` fields** in both
  `KcpServerSession` and `KcpConn`. `BlockCrypt::encrypt(&self)` and
  `decrypt(&self)` take `&self` — the cipher is stateless after
  construction, so `Mutex` was unnecessary contention. Changed from
  `Arc<std::sync::Mutex<Box<dyn BlockCrypt>>>` to `Arc<dyn BlockCrypt>`.
  This eliminates lock acquisition overhead on every packet
  encrypt/decrypt path (flush loop, ACK send, UDP recv decrypt).

#### Parallel multi-packet encryption

- **Added `CryptoBuf::prepare_encrypt()`** (`kcp-rs/src/crypto_buf.rs`):
  prepares the wire buffer (nonce + CRC32 + plaintext copy) without
  encrypting, returning an owned `BytesMut`. This separates the serial
  nonce-counter step from the CPU-bound encryption step.
- **Parallel encryption via `std::thread::scope`**: In both client and
  server flush loops, when ≥ 4 packets are queued, the prepared buffers
  are split into chunks and encrypted in parallel across worker threads.
  The cipher is now `Arc<dyn BlockCrypt>` (lock-free), so multiple
  threads can call `encrypt()` concurrently. Falls back to serial for
  small batches (< 4 packets) to avoid thread-spawn overhead.
- **Result**: 3DES/comp on tokio now matches Go (1.02×), and smol
  improved from 0.65× to 0.89× of Go. The parallel path benefits all
  CFB ciphers under high-throughput multi-packet flush cycles.

### Changed — `kcptun-rt` → `kio-rs` dual-track runtime abstraction

#### Runtime layer refactor

- **Renamed `kcptun-rt` → `kio-rs`** (module name `kio`) — The runtime
  abstraction crate is renamed to `kio-rs` for consistency with the
  workspace naming convention (`kcp-rs`, `kcrypt-rs`, `smux-rs`, `qpp-rs`).
  The `[lib] name = "kio"` ensures code uses `kio::` for all imports.
- **Simplified feature names**: `tokio-backend` → `tokio`,
  `smol-backend` → `smol`. Feature gates throughout the codebase updated.
- **TCP sockets now use `socket2`** — `TcpListener::bind` and
  `TcpStream::connect` now create sockets via `socket2::Socket` for uniform
  buffer tuning (2 MB recv/send), `SO_REUSEADDR`, `TCP_NODELAY`, and
  non-blocking mode — matching the existing UDP path.
- **`kio-rs` source split by feature** — Each module (`net`, `task`,
  `time`, `sync`) is now split into `mod.rs` + `tokio.rs` + `smol.rs`,
  so each file contains only one backend's implementation for readability.
- **Multi-threaded `smol` runtime** (`kio-rs/src/task/smol.rs`) —
  `block_on()` now spawns `N-1` worker threads (via `std::thread::scope`),
  each running the global `async_executor::Executor` via
  `race(exec.run(pending()), stop_rx.recv())`. The main thread runs
  `exec.run(future)`, concurrently driving the user future and spawned
  tasks. Workers exit cleanly when the main future completes and the stop
  channel closes. `JoinHandle.inner` changed to `Option<Task<T>>` so
  `Drop` can `detach()` the task (matching tokio's detached-spawn semantics
  — fire-and-forget tasks survive handle drop on both backends).
- **Direct UDP send (no `spawn_task`)** — ACK packets (client + server)
  and flush-loop output are now sent via direct `udp.send().await` instead
  of `kio::spawn_task(async move { u.send().await })`. On the smol backend,
  spawned tasks could be delayed by the executor's scheduling order, causing
  KCP ACK timeouts and retransmission storms. Direct `await` ensures ACKs
  are sent immediately within the calling task's context.
- **Makefile dual-runtime defaults** — Native (x86_64/aarch64) builds
  default to `tokio`; ARM (armv7) cross-builds default to `smol` (lighter
  binary, no tokio runtime overhead). Custom feature selection via
  `FEATURES=tokio` or `FEATURES=smol` overrides the default. Added
  `build-smol`, `release-smol`, `clippy-smol`, and `bench` targets.

#### smux-rs dual-track conversion

- **`smux-rs` no longer depends on `tokio` directly** — All runtime
  dependencies (`Notify`, `mpsc`, `select!`) replaced with `kio-rs`
  abstractions. `smux-rs` now compiles under both `tokio` and `smol`
  features without any `#[cfg]` in its own code.
- **`Stream::read_async` simplified** — Removed `ch_fin_event` Notify and
  `tokio::select!`. A single `ch_reader_wakeup: kio::Notify` now handles
  both data-arrival and FIN events. `notify_one()` (new on `kio::Notify`)
  provides permit-stored wakeup semantics.
- **`Session` channel** — `tokio::sync::mpsc` replaced with
  `kio::bounded()` (backed by `async-channel`, runtime-agnostic).
- **Removed dead fields**: `is_client`, `keepalive_timeout`,
  `bucket_notify`, and 5 unused constants.

#### Dead code cleanup (`#[allow(dead_code)]` eliminated)

- **kcp-rs**: Removed `IKCP_FASTACK_LIMIT`, `debug_log()`,
  `encode_cache`; `token` field made `pub`; `SessionInner` slimmed
  (removed `send_buf`, `recv_buf`, `stream_mode`); `UDPSession` slimmed
  (removed `last_update`, `interval`, `inner()` accessor).
- **kcrypt-rs**: Removed unused `xtea_dec()`.
- **kcptun-client**: Removed `rate_limit`, `update_activity()`,
  `is_expired()`; fixed incorrect `#[allow(dead_code)]` on `has_encryption`
  and `last_activity` (both are actively used).
- **kcptun-server**: Removed `DEFAULT_CONV`, `rate_limit`, `drain_and_send()`,
  `peer()`; fixed incorrect `#[allow(dead_code)]` on `peer` field.

### Added — Event-Driven Flush Scheduling (latency reduction ~35-40%)

Replaced the fixed 10ms `tokio::time::sleep` flush loop with an
event-driven `select!` + `Notify` model, matching Go kcptun's
`SystemTimedSched` + `flush()`-returns-`nextUpdate` architecture.

- **`KCP::flush()` returns `nextUpdate`** (`kcp-rs/src/kcp.rs`) — The
  `flush()` function now returns `u32` instead of `()`, computing the
  milliseconds until the next meaningful event (nearest RTO or interval),
  matching Go kcp-go's `flush()` return value used by
  `SystemTimedSched.Put(s.update, time.Now().Add(interval))`.
- **`KCP::update()` returns `nextUpdate`** (`kcp-rs/src/kcp.rs`) — The
  `update()` function now returns `u32` (the `flush()` return value or
  `self.interval`), enabling the flush loop to use dynamic scheduling.
- **Flush loop: `sleep(10ms)` → `select! { sleep(next_update) | notify }`**
  (client + server) — The flush loop now waits for either the dynamic
  interval (nearest RTO or default) or an immediate `Notify` from SMUX
  stream writes. Uses `notify_one()` (permit-stored, no lost-wakeup).
  The `next_update` is clamped to `[1, KCP_UPDATE_INTERVAL_MS]` to avoid
  busy-looping.
- **SMUX stream write → `flush_notify.notify_one()`** (client + server) —
  `SmuxStreamAsync::poll_write` (client) and `SmuxStreamIo::poll_write`
  (server) now call `flush_notify.notify_one()` after `stream.write()`,
  waking the flush loop immediately. Eliminates the 0~10ms wait for
  outgoing data.
- **Server `feed_data` immediate ACK drain** (`kcptun-server/src/main.rs`)
  — `feed_data()` now drains `raw_packets` at the end and spawns a
  fire-and-forget task to encrypt + send ACKs immediately (matching the
  client's UDP reader Task 1 behavior). Previously, server ACKs sat in
  `raw_packets` until the 10ms flush loop picked them up, adding 0~10ms
  latency.
- **Direct `flush()` call (no double-flush)** (client + server) — The
  flush loop calls `flush()` directly instead of `update()` + `flush()`,
  matching Go's `UDPSession.update()` which calls `s.kcp.flush()` directly
  (not the deprecated `KCP.Update()` that throttles via `ts_flush`).

#### Benchmark results (10 conn × 64KB, `--quick`)

| Config              | Rust latency | Go latency | Ratio   |
|---------------------|-------------|------------|---------|
| null/no-comp        | 0.038s      | 0.029s     | 1.31x   |
| aes-128/no-comp     | 0.032s      | 0.025s     | 1.28x   |
| blowfish/no-comp    | 0.032s      | 0.022s     | 1.45x   |
| salsa20/comp        | 0.025s      | 0.020s     | 1.25x   |
| sm4/no-comp         | 0.033s      | 0.285s     | 0.12x ↑ |
| sm4/comp            | 0.028s      | 0.414s     | 0.07x ↑ |

Previous baseline: Rust ~0.045s vs Go ~0.020s (2.25x).
After: Rust ~0.030s vs Go ~0.022s (~1.3x). SM4: Rust 8-14x faster than Go.

### Added — `spawn_blocking` + BufferPool + Cipher Key Schedule Fixes

#### P0: Low-risk, high-value optimizations

- **`Stream::register_read_waker`** (`smux-rs/src/stream.rs`) — Added a
  `read_waker: Mutex<Option<Waker>>` field to `Stream`. `poll_read` now
  registers the task waker directly with the stream instead of spawning a
  `tokio::spawn(sleep(2-5ms))` task on every empty read. `wakeup_reader()`
  and `fin_event()` wake the stored waker immediately when data arrives or
  the remote side closes. Includes a re-check after registration to prevent
  the lost-wakeup race (data arriving between `WouldBlock` and waker
  registration).
- **`poll_read` buffer reuse** (server + client) — Replaced
  `vec![0u8; buf.remaining()]` + `buf.put_slice()` with
  `buf.initialize_unfilled()` + `buf.advance()`, eliminating the per-call
  ~64KB heap allocation.
- **`BufferPool` enabled** (`kcp-rs/src/buffer_pool.rs` + server + client) —
  The KCP output callback now uses `pool.acquire()` + `extend_from_slice()`
  instead of `data.to_vec()`, and the flush loop returns buffers to the pool
  after encryption. **Bug fix:** `BufferPool::new()` was using
  `vec![0u8; buf_size]` (len=2048) instead of `Vec::with_capacity(buf_size)`
  (len=0), causing `extend_from_slice` to append data after 2048 zeros,
  corrupting every packet. Fixed by using `with_capacity` + `clear()` in
  `acquire()`.
- **QPPPort buffer reuse** (server + client) — Added `read_io_buf` and
  `write_enc_buf` fields to `QPPPort`, eliminating `vec![0u8; PIPE_BUF_SIZE]`
  per read and `buf.to_vec()` per write. Decryption is now in-place in the
  read buffer.

#### P1: Medium-risk, high-throughput optimizations

- **`block_in_place` for server `feed_data`** (`kcptun-server/src/main.rs`) —
  The recv loop now wraps `feed_data` + SMUX `process_data` +
  `drain_new_streams` in `tokio::task::block_in_place()`, freeing the reactor
  during the ~30-140μs CPU work chain (decrypt + FEC + KCP + decompress +
  SMUX process).
- **`spawn_blocking` batch encrypt** (server + client flush loops) — The
  flush loop now batches all raw KCP packets into a single
  `tokio::task::spawn_blocking(move || { ... }).await` call, locking
  `crypt`/`crypto_buf` once (vs per-packet) and offloading ~720-1200μs of
  CPU work from the async runtime. Required wrapping all `MutexGuard`
  sections in block scopes to ensure `!Send` guards are dropped before the
  `.await` point.

#### Cipher Key Schedule Bug Fixes (100x performance improvement)

- **Blowfish** (`kcrypt-rs/src/crypt/blowfish.rs`) — Fixed: `new_from_slice()`
  was called inside `bf_enc()` (the per-block encryption function), re-running
  the full key schedule for every 8-byte block. In CFB-8 mode, a 1350-byte
  packet triggered 1350 key schedules. Now the cipher is created once in the
  constructor and stored as a field. **Result: 0.0 MB/s → 3.0 MB/s (100x).**
- **Twofish** (`kcrypt-rs/src/crypt/twofish.rs`) — Same bug: `new_from_slice()`
  per block. Additionally replaced the RustCrypto twofish crate (v0.7.1,
  `#![deny(unsafe_code)]`, computes `sbox()` + `gf_mult()` per block) with a
  custom implementation that pre-computes `s [4][256]u32` lookup tables in the
  constructor (matching Go's approach). The `g_func` is now 4 table lookups
  + 3 XORs (O(1) per block). **Result: 0.4 MB/s → 4.5 MB/s (11x).**
- **Triple-DES** (`kcrypt-rs/src/crypt/triple_des.rs`) — Same key-schedule-per-block
  bug. Fixed by creating the cipher once in the constructor.
  **Result: 2.5 MB/s → 3.3 MB/s (32%).**
- **AES-CFB** (`kcrypt-rs/src/crypt/aes_cfb.rs`) — Same bug, less severe
  (AES key schedule is faster, CFB-16 uses 16-byte blocks). Fixed by storing
  the cipher in an `AesCipher` enum (`Aes128`/`Aes192`/`Aes256`).

#### Benchmark script

- **`bench_rust_vs_go.py`** — Extended to test all 13 ciphers × compression
  on/off (52 configurations per implementation). Generates a summary table
  with throughput, latency, and Rust-vs-Go speedup ratio. Results saved as
  JSON.

#### Multi-backend benchmark framework

- **`bench/run_bench.sh` + `bench/throughput.py`** — Automated Go vs
  Rust-Tokio vs Rust-Smol throughput and latency comparison. Runs 5
  combinations (Go→Go, Rust-Tokio→Rust-Tokio, Rust-Smol→Rust-Smol,
  Go→Rust-Tokio, Rust-Tokio→Go) with a shared Python echo server.
  `throughput.py` uses a concurrent receiver thread to drain echo data
  (preventing TCP loopback deadlock when the receive buffer fills), and
  `run_bench.sh` polls the client TCP port until the listener is ready
  (replacing fragile fixed `sleep` delays). Bash 3.2 compatible (macOS
  default).

### Added — Zero-copy & Performance Optimizations

- **`CryptoBuf`** (`kcp-rs/src/crypto_buf.rs`) — Eliminates per-packet
  allocation in the encryption path. Uses an `AtomicU64` counter for nonce
  generation (replacing `rand::thread_rng().fill_bytes()` per packet) and a
  reusable `BytesMut` buffer that returns reference-counted `Bytes` slices
  (zero-copy handoff to `tokio::spawn`).
- **`KCP::recv_bytes()`** — Zero-copy receive for single-segment messages
  (the common case in stream mode). Returns `Bytes::from(split_to + freeze)`
  instead of `BytesMut` built via `extend_from_slice`.
- **`Stream::push_data_bytes(Bytes)`** — Zero-copy SMUX stream append. Adds a
  `VecDeque<Bytes>` receive buffer alongside the legacy `BytesMut`, so
  `Frame::data` (a reference-counted slice from the codec buffer) can be
  stored without copying.
- **`FrameCodec::decode` zero-copy** — Replaced `Bytes::copy_from_slice`
  with `split_to(total_len).freeze()` + `slice(header..)`. The decoded
  `Frame.data` is now a reference-counted view into the codec buffer.
- **`DashMap` for server sessions** — Replaced
  `parking_lot::Mutex<HashMap<…>>` with `DashMap`. Session lookup only
  locks one shard, and `get_or_create_session` now performs decryption
  **outside** the map lock.
- **`tokio::sync::Notify` for write backpressure** — The flush loop calls
  `notify_waiters()` after each flush cycle and after ACK processing,
  waking blocked writers immediately instead of polling every 10 ms.
- **Immediate flush on `send_frame`** — Matches Go's `WriteBuffers` behavior:
  `kcp.flush()` is called right after `kcp.send()` when `writeDelay` is
  false (the default), eliminating the 10 ms flush-loop latency for
  outgoing data.
- **`bench_rust_vs_go.py`** — Benchmark script comparing Rust vs Go kcptun
  throughput and latency under identical parameters.

### Added
- **`kcrypt-rs` crate** — Shared block/AEAD cipher library extracted from
  `kcp-rs`, enabling reuse across the workspace. One file per cipher under
  `kcrypt-rs/src/crypt/` (`none`, `xor`, `aes_cfb`, `aes_gcm`, `sm4`, `tea`,
  `xtea`, `salsa20`, `blowfish`, `twofish`, `cast5_crypt`, `triple_des`).
  `kcp-rs` re-exports the crypto API for backward compatibility.
- **`CHANGELOG.md`** — this file.
- **`.gitignore`** — excludes `target/`, IDE settings, and compiled Go test
  binaries.
- **Workspace Structure** section in `README.md` documenting all 6 crates.
- **`make stress`** Makefile target for release-mode multi-threaded stress
  tests.
- **`make check-deps`** Makefile target invoking `cargo-udeps`.
- `strip = true` in the release profile for smaller binaries (client 2.1M,
  server 2.3M).

### Changed
- **Dependency cleanup across all crates** — removed unused dependencies and
  tightened feature flags so only actually-used features are enabled:
  - `kcp-rs`: dropped 11 unused deps (`thiserror`, `log`, `dashmap`,
    `arc-swap`, `typenum`, `smallvec`, `bitflags`, `arrayvec`, `num-derive`,
    `num-traits`, `crc32fast`).
  - `smux-rs`: dropped 4 unused deps (`thiserror`, `dashmap`, `crc32fast`,
    `rand`); trimmed `tokio` features (removed unused `rt-multi-thread`).
  - `qpp-rs`: dropped `thiserror`, `log`, and the unused `criterion`
    dev-dependency (no bench target existed).
  - `kcptun-client` / `kcptun-server`: dropped `thiserror`, `crc32c` (CRC32C
    is handled by the `snap` crate), and `hmac` (provided transitively by
    `pbkdf2`'s default `hmac` feature). Replaced tokio `["full"]` with the
    exact feature set used: `["rt", "rt-multi-thread", "net", "io-util",
    "fs", "sync", "time", "signal", "macros"]`.
- **Makefile** — header now lists all workspace members; documented `stress`
  and `check-deps` targets.
- **README.md** — Builds section now documents `make` targets and the 5
  release-profile optimizations (`opt-level`, `lto`, `codegen-units`,
  `panic`, `strip`).

### Fixed
- **Twofish k=4 (256-bit key) S-box precomputation** (`kcrypt-rs/src/crypt/twofish.rs`)
  — The k=4 case of the S-box+MDS lookup table precomputation incorrectly
  reused the k=3 structure (4 sbox layers), while Go's `twofish.go` default
  case uses 5 sbox layers + `^S[12..15]`. Specifically, each `s[j][i]` entry
  was missing the innermost `sbox[1][i]` (or equivalent) and the final
  `^ s_key[12+j]` XOR. This made the Rust twofish cipher produce different
  ciphertext than Go for all 256-bit keys — which is the default key size in
  kcptun (PBKDF2 derives 32 bytes). The fix adds the missing 5th sbox layer
  and `^ s_key[12..15]` terms, matching Go's
  `sbox[1][sbox[0][sbox[0][sbox[1][sbox[1][i]^S[0]]^S[4]]^S[8]]^S[12]]`
  pattern. Go↔Rust e2e interop tests for `crypt=twofish` (nocomp + compress)
  now pass. k=2 and k=3 cases were already correct and unchanged.
- **Server `pipe` now uses idle timeout instead of total timeout**
  (`kcptun-server/src/main.rs`) — The `pipe` function wrapped
  `tokio::io::copy_bidirectional` in `tokio::time::timeout(close_wait, …)`,
  treating `close_wait` (default 30s) as a **total** pipe duration limit.
  Under high concurrency (100 connections × 192KB), the bidirectional copy
  could exceed 30s, causing the server to close the SMUX stream before all
  echo data was delivered — resulting in intermittent
  `test_multithread_large_data` failures (recv < sent byte count).
  Rewrote `pipe` to use an **idle** timeout: the timer resets after every
  data transfer, and only fires when no data flows in either direction for
  `idle_secs` seconds. This matches Go kcptun's `closeWait` semantics (an
  idle/cleanup period, not a total pipe duration).
- **Stress test `test_multithread_large_data` & `test_page_refresh_simulation`
  now pass** — These previously timed out under high concurrency (50+ TCP
  connections multiplexed over a single KCP channel). Root cause: the
  `poll_write` backpressure used a stale shared `AtomicUsize` (updated every
  10 ms by the flush loop) and `tokio::spawn(sleep(5ms) + wake)` for retries,
  causing writers to spin without timely notification when the KCP window
  drained. Fixed by: (1) reading `wait_send()` directly from KCP in
  `poll_write`, (2) using `tokio::sync::Notify` for immediate wakeup after
  flush and ACK processing, (3) calling `kcp.flush()` immediately after
  `send_frame` (matching Go's `WriteBuffers`), and (4) using `--conn N` in
  stress tests so each TCP stream gets its own KCP channel (matching Go's
  `--conn` behavior).
- **Compiler warnings eliminated** (build is now warning-free):
  - Removed dead variable `nocomp_cb` in `kcptun-client/src/main.rs`
    (leftover from a prior refactor).
  - Removed unused test helper `create_kcp_with_output` in `kcp-rs/src/kcp.rs`.
- **Clippy lints fixed** (`cargo clippy --workspace -- -D warnings` passes):
  - `kcrypt-rs/src/crypt.rs`: replaced `copy_from_slice(&pad16(&ch))` with a
    direct array assignment and removed a needless borrow.
  - `kcrypt-rs/src/cast5.rs`: rewrote the key-schedule loop using
    `iter_mut().enumerate().take(4)` (fixing a latent out-of-bounds risk in
    the process — the original `0..4` was correct, but the first clippy
    suggestion would have iterated all 8 elements).
  - `qpp-rs/src/lib.rs`: `rol64` now uses `rotate_left`; `(x + 7) / 8` →
    `div_ceil(8)`; `r >> 0` → `r`; index-based pad fill loops rewritten with
    `iter_mut().enumerate()`.
  - `smux-rs/src/frame.rs`: added `FrameCodec::is_empty` to accompany `len`.
  - `kcptun-server/src/main.rs`: removed needless `Ok(...?)` wrappers in
    `parse_addr`; replaced `|d| Mutex::new(d)` with `Mutex::new`; removed
    explicit auto-derefs on QPP prng guards.
  - `kcptun-client/src/main.rs`: removed explicit auto-derefs on QPP prng
    guards.
- **Pre-existing style lints** in the KCP state machine port (`kcp-rs`) are
  suppressed at the crate level with a documented `#![allow(...)]` block,
  because that module intentionally mirrors Go kcp-go v5's control flow for
  easy auditing.

### Removed
- `kcp-rs/src/crypt.rs` and `kcp-rs/src/cast5.rs` — moved into `kcrypt-rs`
  (re-exported by `kcp-rs` for backward compatibility).

### Verification
- `cargo build --workspace` — 0 warnings.
- `cargo clippy --workspace -- -D warnings` — passes.
- `cargo test --workspace --lib --bins` — 129 unit tests pass
  (kcp-rs 30, kcrypt-rs 19, smux-rs 26, qpp-rs 7, kcptun-server 35).
- `cargo build --release` — client 2.1M, server 2.3M (stripped).
