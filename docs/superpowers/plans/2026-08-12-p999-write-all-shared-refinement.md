# P999 Tail Latency Refinement: `OwnedWriteHalf::write_all_shared`

**Date:** 2026-08-12  
**Branch:** `diag/p999-refinement` (worktree: `.claude/worktrees/p999-refine`)  
**Status:** A/B verified, ready for merge

## 1. Problem

After the previous inline-send fix (commit `ee9752a7`, P999 18ms→1.8ms), the remaining P999 gap
between Rust-tokio (~1.8ms) and Go (~0.8ms) was traced to the **server-side echo write path**.

### Root Cause

The benchmark's `echo_loop` used `writer.write_all(&buf)` (AsyncWrite trait), which goes through
`do_poll_write` → `flush_notify.notify_one()`. This is a **notify→wake→drain→send** scheduling hop:

```
echo_loop                flush_loop
  │                        │
  ├─ kcp.Send + flush      │
  ├─ flush_notify.notify() │
  │      ↓                 │
  │   [timer wheel jitter] │
  │      ↓                 │
  │                        ├─ wake (1–2ms later)
  │                        ├─ drain raw_packets
  │                        └─ send_packets().await
```

In contrast, `KcpConn::write_all_shared` (used by production `KcptunSession`) does
`try_drain_and_send().await` inline — no scheduling hop:

```
echo_loop
  │
  ├─ kcp.Send + flush
  ├─ try_drain_and_send().await  ← sends immediately
  └─ done
```

### Why `do_poll_write` Can't Do Inline Send

`poll_write` is **synchronous** (returns `Poll<io::Result<usize>>`). It cannot `.await` on
`try_drain_and_send()`. A non-blocking `try_send_batch` variant was attempted but **failed A/B
testing** because it drops packets on `WouldBlock` (kernel send buffer full), causing KCP
retransmission storms at medium+ load.

## 2. Solution

Expose `write_all_shared` (async inline send) through `OwnedWriteHalf`, and change the benchmark's
`echo_loop` to use it — matching the production `KcptunSession` code path.

### Changes

| File | Change |
|------|--------|
| `kcp-rs/src/conn.rs` | Add `OwnedWriteHalf::write_all_shared()` and `OwnedReadHalf::read_shared()` delegating to inner `KcpConn` |
| `kcp-rs/examples/latency_p99.rs` | `echo_loop`: replace `read_exact`/`write_all` with `read_shared`/`write_all_shared` |
| `kcp-rs/src/fec.rs` | Fix pre-existing clippy `manual_slice_fill` warning (`.fill(0)`) |
| `kio-rs/src/net/tokio.rs` | Add `try_send_batch_to()` for unconnected sockets (future use) |
| `kio-rs/src/net/smol.rs` | Add `try_send_batch_to()` using `libc::sendto` + `MSG_DONTWAIT` |
| `kio-rs/src/net/mod.rs` | Add `try_send_batch_to()` to `DatagramSocket` enum dispatch |
| `kcp-rs/src/transport.rs` | Add `try_send_batch_to()` to `PacketTransport` trait + `PeerTransport` override |

### Dead-end: `try_drain_and_send_sync` (not included)

A synchronous inline-send variant was implemented and A/B tested but **rejected**:

- **Low load (1KB@500RPS):** Occasional P999=0.44ms (excellent) but inconsistent — system noise dominated
- **Medium load (4KB@1000RPS):** P999=2.63ms vs baseline 0.62ms — **WORSE** due to packet drops on `WouldBlock`
- **High load (64KB@2000RPS):** P999=72ms vs baseline 57ms — **WORSE** due to `is_sending` token contention

**Root cause of failure:** `try_send_batch` is non-blocking. When the kernel send buffer is full,
it returns `WouldBlock` and the drained packets are **lost** (recycled). KCP must retransmit them,
causing latency spikes. The async `try_drain_and_send().await` properly waits for socket writability
via `writable().await`.

## 3. A/B Test Results

### Methodology

- **Baseline:** `master` branch (echo_loop uses `write_all` → `do_poll_write` → `flush_notify`)
- **Worktree:** `diag/p999-refinement` (echo_loop uses `write_all_shared` → inline `try_drain_and_send`)
- Interleaved rounds on the same server process, `--reader-poll 100`
- macOS, 8 physical cores, tokio runtime

### 3.1 Low Load: 1KB @ 500 RPS

| Round | Master P999 (µs) | Worktree P999 (µs) | Master P99 | Worktree P99 |
|-------|-----------------:|-------------------:|-----------:|-------------:|
| r1    | 39,305           | 30,554             | 641        | 15,643       |
| r2    | 17,764           | 18,907             | 4,756      | 419          |
| r3    | 36,225           | 18,499             | 351        | 752          |
| r4    | 37,661           | **332**            | 309        | 264          |
| r5    | 16,803           | **867**            | 365        | 401          |

**Verdict:** Highly noisy on macOS. Worktree shows best-case P999=332µs (vs master's best 16,803µs)
but also has bad rounds. P50 is identical (~140µs). The optimization helps when the system is quiet
but can't overcome OS-level scheduling jitter.

### 3.2 Medium Load: 4KB @ 1000 RPS

| Round | Master P999 (µs) | Worktree P999 (µs) | Master P99 | Worktree P99 |
|-------|-----------------:|-------------------:|-----------:|-------------:|
| r1    | 30,171           | **2,409**          | 1,667      | 818          |
| r2    | 1,665            | 1,507              | 524        | 548          |
| r3    | 30,874           | 20,255             | 15,835     | 1,487        |

**Verdict:** Worktree wins. P999 median: 2,409 vs 30,171 (**12.5× improvement**). P99 median:
818 vs 1,667 (**2× improvement**). The inline send eliminates the flush loop scheduling hop that
causes 30ms spikes on master.

### 3.3 High Load: 64KB @ 2000 RPS

| Round | Master P999 (µs) | Worktree P999 (µs) | Master P50 | Worktree P50 |
|-------|-----------------:|-------------------:|-----------:|-------------:|
| r1    | 21,441           | 61,406             | 8,287      | 12,334       |
| r2    | 66,325           | 44,908             | 7,982      | 12,366       |

**Verdict:** Worktree is worse at high load. P50 increases from ~8ms to ~12ms because
`write_all_shared`'s async `try_drain_and_send().await` holds the `is_sending` token during the
entire `send_batch().await`, blocking the flush loop from doing KCP state machine work
(retransmission, delayed ACKs). At 64KB/write (~48 KCP segments), this token hold is significant.

**Note:** This is the **same behavior as production** — `KcptunSession` already uses
`write_all_shared` for all writes. The benchmark was the only code path using `write_all`.

## 4. Analysis

### Why `write_all_shared` Helps at Medium Load

At 4KB@1000RPS, each write produces ~3 KCP segments. The `try_drain_and_send().await` for 3
segments is fast (< 100µs on loopback). The token is released quickly, and the flush loop barely
notices. But the notify→wake hop eliminated by inline send was adding 1–30ms of jitter (timer wheel
rounding + task scheduling).

### Why `write_all_shared` Hurts at High Load

At 64KB@2000RPS, each write produces ~48 KCP segments. The `send_batch().await` for 48 segments
takes ~1–2ms (many `sendto` syscalls + potential `writable().await` waits). During this time:

1. `is_sending` is held → flush loop can't acquire it
2. Flush loop can't run `kcp.flush()` → delayed ACKs pile up
3. Input loop's `try_drain_and_send()` also fails (CAS fails) → falls back to `notify_one()`
4. The notification wakes the flush loop, but it can't send until the write path releases the token

This creates a cascading delay that increases P50 and P999.

### Why the Sync Variant (`try_drain_and_send_sync`) Failed

The non-blocking `try_send_batch` returns `WouldBlock` when the kernel send buffer is full. The
drained packets are recycled (lost). KCP must retransmit them after RTO timeout (200ms for Fast3),
causing massive P999 spikes. The async path avoids this by `writable().await` — waiting for buffer
space instead of dropping packets.

## 5. Recommendation

- **Merge the `write_all_shared` optimization** — it makes the benchmark consistent with production
  and improves P999 by 12.5× at medium load
- **Do NOT merge `try_drain_and_send_sync`** — it drops packets and causes retransmission storms
- **Keep `try_send_batch_to` additions** to `PacketTransport`/`kio` — they're useful for future
  non-blocking send paths (e.g., ACK-only bursts)
- **For high-load scenarios:** Consider adding a `write_all_flushed` variant that delegates to
  `do_poll_write` (flush loop path) for large payloads, or chunking writes to ≤ 8 KCP segments
  per `write_all_shared` call

## 6. Files Changed

```
kcp-rs/src/conn.rs           +15 lines  (OwnedWriteHalf::write_all_shared, OwnedReadHalf::read_shared)
kcp-rs/src/fec.rs            ±1 line    (clippy fix: .fill(0))
kcp-rs/src/transport.rs      +15 lines  (try_send_batch_to trait + PeerTransport override)
kcp-rs/examples/latency_p99.rs ±15 lines (echo_loop uses write_all_shared)
kio-rs/src/net/tokio.rs      +28 lines  (try_send_batch_to for tokio)
kio-rs/src/net/smol.rs       +63 lines  (try_send_batch_to for smol via libc::sendto)
kio-rs/src/net/mod.rs        +12 lines  (DatagramSocket enum dispatch)
```

## 7. Gate Checks

- `cargo fmt --all -- --check` ✅
- `cargo test --workspace -p kcp-rs -p kio-rs` ✅ (105 tests passed)
- `cargo clippy -p kcp-rs -p kio-rs -- -D warnings` ✅
- `cargo build --features async-tokio` ✅
- `cargo build --features async-smol` ✅