//! SMUX logical stream implementation.
//!
//! ## Lock model (R4)
//!
//! - [`RecvInner`]: stream state, zero-copy recv queue, read waker, local_closed_at
//! - [`SendInner`]: send queue, write waker
//! - Lock order when both needed: **recv then send** (`close` / `clear_buffers`)
//! - Take wakers under the lock, **wake after release** (no re-entrant deadlock)
//! - Flow-control / half-close flags stay on atomics for hot queries

use std::collections::VecDeque;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use log::debug;
use parking_lot::Mutex;

/// Default buffer size for pooled BytesMut chunks (matching typical SMUX frame payload).
const POOL_BUF_SIZE: usize = 16384;
/// Maximum pooled buffers per stream to avoid unbounded growth.
const POOL_MAX_BUFFERS: usize = 8;

/// A lightweight buffer pool for SMUX stream data chunks.
///
/// Reduces `Bytes::copy_from_slice` allocations on the hot write/push_data path
/// by reusing `BytesMut` capacity. When the pool is empty, falls back to
/// `Bytes::copy_from_slice` (same behavior as before).
struct BytesPool {
    buffers: Vec<BytesMut>,
}

impl BytesPool {
    fn new() -> Self {
        BytesPool {
            buffers: Vec::with_capacity(POOL_MAX_BUFFERS),
        }
    }

    /// Acquire a buffer from the pool, or allocate a fresh one.
    #[inline]
    fn acquire(&mut self) -> BytesMut {
        self.buffers
            .pop()
            .unwrap_or_else(|| BytesMut::with_capacity(POOL_BUF_SIZE))
    }

    /// Return a buffer to the pool for reuse (cleared, capacity retained).
    #[inline]
    fn release(&mut self, mut buf: BytesMut) {
        if self.buffers.len() < POOL_MAX_BUFFERS {
            buf.clear();
            self.buffers.push(buf);
        }
    }
}

/// Error returned by stream operations.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StreamError {
    /// Stream has been closed.
    #[error("stream closed")]
    Closed,
    /// Stream has been reset.
    #[error("stream reset")]
    Reset,
    /// Buffer overflow.
    #[error("buffer overflow")]
    BufferOverflow,
    /// Not enough data available.
    #[error("not enough data (would block)")]
    WouldBlock,
}

/// The state of a SMUX stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamState {
    /// Initial state.
    Init,
    /// Ready for data transfer.
    Ready,
    /// Remote side has closed.
    FinReceived,
    /// Local side has closed.
    FinSent,
    /// Fully closed.
    Closed,
    /// Reset.
    Reset,
}

/// Grace period after `remote_closed` during which `read` keeps returning
/// `WouldBlock` instead of EOF, so a data tail that arrives after the peer's
/// FIN (its pipe drained faster than its flush emitted the tail) is not dropped
/// by a prematurely-completing pipe.
const EOF_GRACE_MS: u64 = 300;
const EOF_GRACE: std::time::Duration = std::time::Duration::from_millis(EOF_GRACE_MS);

/// Receive half + state + read waker (single mutex).
struct RecvInner {
    state: StreamState,
    /// Zero-copy receive queue (formerly `recv_buf_bytes`). Legacy contiguous
    /// `BytesMut` recv buffer was removed in R4.
    recv: VecDeque<Bytes>,
    read_waker: Option<Waker>,
    local_closed_at: Option<Instant>,
    /// When the remote FIN was processed — used for the EOF grace period
    /// (the FIN may precede the data tail when the peer's pipe drains faster
    /// than its flush; without the grace, `read` EOFs and the caller's pipe
    /// completes, dropping the tail).
    remote_closed_at: Option<Instant>,
    /// Buffer pool for inbound data chunks (reduces Bytes::copy_from_slice alloc).
    pool: BytesPool,
}

/// Send half + write waker (separate mutex so push/read does not block drain).
struct SendInner {
    send: VecDeque<Bytes>,
    write_waker: Option<Waker>,
    /// Buffer pool for outbound data chunks (reduces Bytes::copy_from_slice alloc).
    pool: BytesPool,
}

/// A single logical stream within a SMUX session.
///
/// Each stream has a unique ID within its session and supports
/// bidirectional data transfer.
pub struct Stream {
    /// Stream ID (unique within the session).
    id: u32,
    /// Maximum receive buffer size (flow-control window).
    max_recv_buf: usize,
    /// Receive path + state (R4).
    recv: Mutex<RecvInner>,
    /// Send path (R4; independent of recv to avoid read/write lock coupling).
    send: Mutex<SendInner>,
    /// Number of bytes waiting to be sent (avoids locking send on every query).
    send_buf_bytes: AtomicUsize,
    /// Number of bytes available to read (avoids locking recv on every query).
    recv_buf_bytes_avail: AtomicUsize,
    /// Number of bytes read by the consumer.
    bytes_read: AtomicU32,
    /// Number of bytes written on the wire (after drain).
    bytes_written: AtomicU32,
    /// Whether the stream has been opened (SYN sent).
    opened: AtomicBool,
    /// Whether the remote has closed.
    remote_closed: AtomicBool,
    /// Whether the local has closed.
    local_closed: AtomicBool,
    /// Whether a FIN frame has been sent for this stream.
    fin_sent: AtomicBool,

    // ── V2 flow control ──
    /// Incremental bytes consumed by reader (triggers UPD at threshold).
    incr: AtomicU32,
    /// Accumulated UPD consumed value (total bytes consumed from stream).
    upd_consumed: AtomicU32,
    /// Pending UPD notification to session.
    pending_upd: AtomicBool,
    /// Peer's reported consumed byte count (from UPD).
    peer_consumed: AtomicU32,
    /// Peer's advertised receive window size (from UPD).
    /// Initialized to 256 KiB matching Go `initialPeerWindow`.
    peer_window: AtomicU32,

    // ── Async notification ──
    /// Wakes up a reader blocked in `read_async()`.
    ch_reader_wakeup: knet::Notify,
    /// Wakes up a writer blocked in `poll_write()` (v2 peer window).
    ch_write_wakeup: knet::Notify,
    /// Optional flush-loop wake (SmuxConn / kcptun set this so writes flush ASAP).
    /// Low-level Session users may leave this `None`.
    flush_notify: Mutex<Option<Arc<knet::Notify>>>,
    /// Weak self-reference so a grace-expiry wakeup task can re-wake a reader.
    self_ref: Mutex<Option<std::sync::Weak<Stream>>>,
}

impl Stream {
    fn new_inner(id: u32, max_recv_buf: usize) -> Self {
        Stream {
            id,
            max_recv_buf,
            recv: Mutex::new(RecvInner {
                state: StreamState::Init,
                recv: VecDeque::new(),
                read_waker: None,
                local_closed_at: None,
                remote_closed_at: None,
                pool: BytesPool::new(),
            }),
            send: Mutex::new(SendInner {
                send: VecDeque::new(),
                write_waker: None,
                pool: BytesPool::new(),
            }),
            send_buf_bytes: AtomicUsize::new(0),
            recv_buf_bytes_avail: AtomicUsize::new(0),
            bytes_read: AtomicU32::new(0),
            bytes_written: AtomicU32::new(0),
            opened: AtomicBool::new(false),
            remote_closed: AtomicBool::new(false),
            local_closed: AtomicBool::new(false),
            fin_sent: AtomicBool::new(false),
            incr: AtomicU32::new(0),
            upd_consumed: AtomicU32::new(0),
            pending_upd: AtomicBool::new(false),
            peer_consumed: AtomicU32::new(0),
            peer_window: AtomicU32::new(262144), // Go initialPeerWindow
            ch_reader_wakeup: knet::Notify::new(),
            ch_write_wakeup: knet::Notify::new(),
            flush_notify: Mutex::new(None),
            self_ref: Mutex::new(None),
        }
    }

    /// Attach a weak self-reference (set by the Session after Arc creation) so a
    /// grace-expiry wakeup task can re-wake a reader blocked after `remote_closed`.
    #[inline]
    pub fn set_self_ref(&self, w: std::sync::Weak<Stream>) {
        *self.self_ref.lock() = Some(w);
    }

    /// Attach a flush-loop notifier so `poll_write` / shutdown wake the driver.
    #[inline]
    pub fn set_flush_notify(&self, n: Arc<knet::Notify>) {
        *self.flush_notify.lock() = Some(n);
    }

    /// Wake the flush loop if one was registered via [`set_flush_notify`].
    #[inline]
    fn notify_flush(&self) {
        if let Some(ref n) = *self.flush_notify.lock() {
            n.notify_one();
        }
    }

    /// Create a new stream with the given ID.
    pub fn new(id: u32) -> Self {
        Self::new_inner(id, 4 * 1024 * 1024)
    }

    /// Create a new stream with custom buffer capacity.
    ///
    /// `recv_capacity` is the **max** receive window, not a pre-allocation size.
    /// Buffers grow on demand so short-lived proxy streams do not reserve multi-MB
    /// RSS up front (see bugs/BUGREPORT_PROXY_MEMORY_GROWTH.md).
    pub fn with_buffer(id: u32, recv_capacity: usize) -> Self {
        Self::new_inner(id, recv_capacity)
    }

    /// Diagnostic: reserved capacity hint for the receive path.
    ///
    /// After R4 there is no pre-sized contiguous `BytesMut`; this returns `0`
    /// while the queue is empty (lazy growth), matching the historical
    /// "must not preallocate streambuf" test intent.
    #[inline]
    pub fn recv_buf_capacity(&self) -> usize {
        // No contiguous buffer is pre-allocated; queue storage is empty until data.
        0
    }

    /// Get the stream ID.
    #[inline]
    pub fn id(&self) -> u32 {
        self.id
    }

    /// Check if the stream is closed.
    #[inline]
    pub fn is_closed(&self) -> bool {
        matches!(
            self.recv.lock().state,
            StreamState::Closed | StreamState::Reset
        )
    }

    /// Check if the stream is ready for data transfer.
    #[inline]
    pub fn is_ready(&self) -> bool {
        self.recv.lock().state == StreamState::Ready
    }

    /// Get the current state.
    #[inline]
    pub fn state(&self) -> StreamState {
        self.recv.lock().state
    }

    /// Set the stream state.
    #[inline]
    pub fn set_state(&self, new_state: StreamState) {
        self.recv.lock().state = new_state;
    }

    /// Mark the stream as opened (SYN sent).
    #[inline]
    pub fn mark_opened(&self) {
        self.opened.store(true, Ordering::Release);
    }

    /// Wake up any reader blocked in `read_async()` or async `read()`.
    /// Also wakes any poll_read-based reader that registered a waker.
    #[inline]
    pub fn wakeup_reader(&self) {
        let waker = self.recv.lock().read_waker.take();
        self.ch_reader_wakeup.notify_one();
        if let Some(w) = waker {
            w.wake();
        }
    }

    /// Register a waker for poll_read-based async readers.
    #[inline]
    pub fn register_read_waker(&self, waker: Waker) {
        self.recv.lock().read_waker = Some(waker);
    }

    /// Signal that a FIN event has been received (remote closed).
    #[inline]
    pub fn fin_event(&self) {
        let waker = self.recv.lock().read_waker.take();
        self.ch_reader_wakeup.notify_one();
        if let Some(w) = waker {
            w.wake();
        }
    }

    /// Wake up any writer blocked in `poll_write()` (v2 peer window).
    #[inline]
    pub fn wakeup_writer(&self) {
        let waker = self.send.lock().write_waker.take();
        self.ch_write_wakeup.notify_one();
        if let Some(w) = waker {
            w.wake();
        }
    }

    /// Register a waker for poll_write-based async writers.
    #[inline]
    pub fn register_write_waker(&self, waker: Waker) {
        self.send.lock().write_waker = Some(waker);
    }

    /// Push incoming data into the receive buffer.
    ///
    /// Uses the internal [`BytesPool`] to avoid `Bytes::copy_from_slice`
    /// allocation when a pooled buffer is available. The buffer is copied
    /// into a pooled `BytesMut`, then split and frozen; the remaining capacity
    /// is returned to the pool for reuse.
    pub fn push_data(&self, data: &[u8]) -> Result<(), StreamError> {
        if data.is_empty() {
            return Ok(());
        }
        let bytes = {
            let mut inner = self.recv.lock();
            let mut buf = inner.pool.acquire();
            buf.extend_from_slice(data);
            let n = buf.len();
            let bytes = buf.split_to(n).freeze();
            // Return the remaining BytesMut (with preserved capacity) to pool
            inner.pool.release(buf);
            bytes
        };
        self.push_data_bytes(bytes)
    }

    /// Push incoming data as a `Bytes` (zero-copy append).
    pub fn push_data_bytes(&self, data: Bytes) -> Result<(), StreamError> {
        if data.is_empty() {
            return Ok(());
        }
        let n = data.len();
        {
            let mut inner = self.recv.lock();
            inner.recv.push_back(data);
        }
        self.recv_buf_bytes_avail.fetch_add(n, Ordering::Relaxed);
        self.wakeup_reader();
        Ok(())
    }

    /// Read data from the receive buffer.
    ///
    /// Returns the number of bytes read and whether a UPD update should be
    /// sent (for V2 flow control).
    pub fn read(&self, buf: &mut [u8]) -> Result<(usize, bool), StreamError> {
        let mut offset = 0;
        {
            let mut inner = self.recv.lock();
            while offset < buf.len() && !inner.recv.is_empty() {
                let front = inner.recv.front_mut().unwrap();
                let to_copy = front.len().min(buf.len() - offset);
                buf[offset..offset + to_copy].copy_from_slice(&front[..to_copy]);
                offset += to_copy;
                if to_copy == front.len() {
                    inner.recv.pop_front();
                } else {
                    *front = front.slice(to_copy..);
                }
            }
        }

        if offset == 0 {
            if self.remote_closed.load(Ordering::Acquire) {
                // ── EOF grace for the FIN-vs-data-tail race (BUGREPORT fix) ──
                //
                // WHY: A sender can emit its FIN frame *before* its data tail is
                // fully transmitted. This happens when the sender's pipe drains
                // its stream faster than the sender's own flush can emit the
                // remaining data (KCP congestion-window slow start means the
                // tail can land 100–300 ms after the FIN). In the M1-A lib
                // KcpStream path, the reader adds a `KCP → read_buf → reader →
                // process_data` hop, which widens this window.
                //
                // CONSEQUENCE WITHOUT THE GRACE: the moment this reader sees
                // `remote_closed && empty` it returned `Err(Closed)`, the
                // caller's `knet::pipe` completed, and the still-in-transit data
                // tail was silently dropped (observed ~1/5 runs under
                // `aes + FEC 10/3` for 200 KB transfers).
                //
                // FIX: for a grace period after the FIN (`EOF_GRACE_MS`) this
                // read returns `WouldBlock` (Pending) instead of EOF, so the
                // caller keeps polling and late-arriving data is delivered. We
                // schedule a one-shot wakeup after the grace so a reader that is
                // blocked (no new data) re-polls exactly once more and then gets
                // the real EOF — it does not wait forever.
                let closed_at = self.recv.lock().remote_closed_at;
                if let Some(t) = closed_at {
                    if t.elapsed() < EOF_GRACE {
                        if let Some(w) = self.self_ref.lock().clone() {
                            knet::spawn_task(async move {
                                knet::sleep_ms(EOF_GRACE_MS).await;
                                if let Some(s) = w.upgrade() {
                                    s.wakeup_reader();
                                }
                            });
                        }
                        return Err(StreamError::WouldBlock);
                    }
                }
                return Err(StreamError::Closed);
            }
            return Err(StreamError::WouldBlock);
        }

        self.recv_buf_bytes_avail
            .fetch_sub(offset, Ordering::Relaxed);
        let was_empty = self.bytes_read.fetch_add(offset as u32, Ordering::Relaxed) == 0;
        let incr_val = self.incr.fetch_add(offset as u32, Ordering::Relaxed) + offset as u32;
        let need_upd = if incr_val >= (self.max_recv_buf as u32 / 2) || was_empty {
            self.incr.store(0, Ordering::Relaxed);
            let consumed = self.bytes_read.load(Ordering::Relaxed);
            self.upd_consumed.store(consumed, Ordering::Relaxed);
            self.pending_upd.store(true, Ordering::Release);
            true
        } else {
            false
        };
        Ok((offset, need_upd))
    }

    /// Async read — waits for data or FIN, like Go's tryRead.
    pub async fn read_async(&self, buf: &mut [u8]) -> Result<(usize, bool), StreamError> {
        loop {
            match self.read(buf) {
                Ok(v) => return Ok(v),
                Err(StreamError::WouldBlock) => {
                    if self.remote_closed.load(Ordering::Acquire) {
                        // Re-check for data that raced with FIN.
                        match self.read(buf) {
                            Ok(v) => return Ok(v),
                            Err(StreamError::Closed) => return Err(StreamError::Closed),
                            Err(StreamError::WouldBlock) => {
                                // read() returns WouldBlock — not Closed — while
                                // inside the EOF grace period (the grace wakeup
                                // is scheduled by read() itself via
                                // spawn_task). Translating that WouldBlock to
                                // Closed here would truncate a late-arriving
                                // data tail, exactly the bug the grace period
                                // exists to prevent.
                                //
                                // Wait for the wakeup, but also bound the wait
                                // at the grace duration + a small margin. In a
                                // multi-thread runtime the spawn_task wakeup
                                // fires and we re-poll immediately when data
                                // arrives. In a current-thread runtime (tests),
                                // the spawned task is not polled until the
                                // block_on future yields, so the timeout is
                                // the only way to re-poll and reach the real
                                // EOF after the grace expires.
                                let _ = knet::timeout(
                                    std::time::Duration::from_millis(EOF_GRACE_MS + 50),
                                    self.ch_reader_wakeup.notified(),
                                )
                                .await;
                            }
                            Err(e) => return Err(e),
                        }
                    } else {
                        self.ch_reader_wakeup.notified().await;
                    }
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Write data into the send buffer (copies into owned `Bytes` via pool).
    ///
    /// Uses the internal [`BytesPool`] to avoid `Bytes::copy_from_slice`
    /// allocation when a pooled buffer is available.
    pub fn write(&self, data: &[u8]) -> Result<usize, StreamError> {
        if data.is_empty() {
            return Ok(0);
        }
        let bytes = {
            let mut inner = self.send.lock();
            let mut buf = inner.pool.acquire();
            buf.extend_from_slice(data);
            let n = buf.len();
            let bytes = buf.split_to(n).freeze();
            inner.pool.release(buf);
            bytes
        };
        self.write_bytes(bytes)
    }

    /// Write a `Bytes` chunk into the send buffer (zero-copy enqueue).
    pub fn write_bytes(&self, data: Bytes) -> Result<usize, StreamError> {
        if self.local_closed.load(Ordering::Acquire) {
            return Err(StreamError::Closed);
        }
        let n = data.len();
        if n == 0 {
            return Ok(0);
        }
        {
            let mut inner = self.send.lock();
            inner.send.push_back(data);
        }
        self.send_buf_bytes.fetch_add(n, Ordering::Relaxed);
        // Note: bytes_written tracks *on-wire* bytes (incremented in drain_send_max).
        Ok(n)
    }

    /// Drain up to `max_bytes` from the send buffer into `buf`, respecting peer window.
    pub fn drain_send_max(&self, buf: &mut BytesMut, max_bytes: usize) -> usize {
        if max_bytes == 0 {
            return 0;
        }
        let peer_win = self.peer_send_window() as usize;
        if peer_win == 0 {
            return 0;
        }
        let limit = max_bytes.min(peer_win);
        let mut drained = 0;
        {
            let mut inner = self.send.lock();
            while drained < limit && !inner.send.is_empty() {
                let front = inner.send.front_mut().unwrap();
                let take = front.len().min(limit - drained);
                buf.extend_from_slice(&front[..take]);
                drained += take;
                if take == front.len() {
                    inner.send.pop_front();
                } else {
                    *front = front.slice(take..);
                }
            }
        }
        if drained > 0 {
            self.send_buf_bytes.fetch_sub(drained, Ordering::Relaxed);
            self.bytes_written
                .fetch_add(drained as u32, Ordering::Relaxed);
        }
        drained
    }

    /// Drain the entire send buffer (subject to peer window).
    pub fn drain_send(&self, buf: &mut BytesMut) -> usize {
        self.drain_send_max(buf, usize::MAX)
    }

    /// Bytes available to read.
    #[inline]
    pub fn available(&self) -> usize {
        self.recv_buf_bytes_avail.load(Ordering::Relaxed)
    }

    /// Bytes pending in the send buffer.
    #[inline]
    pub fn pending_send(&self) -> usize {
        self.send_buf_bytes.load(Ordering::Relaxed)
    }

    /// Mark remote side closed (FIN received).
    pub fn mark_remote_closed(&self) {
        self.recv.lock().remote_closed_at = Some(Instant::now());
        self.remote_closed.store(true, Ordering::Release);
        self.fin_event();
    }

    /// Mark local side closed.
    pub fn mark_local_closed(&self) {
        self.local_closed.store(true, Ordering::Release);
        let mut inner = self.recv.lock();
        if inner.local_closed_at.is_none() {
            inner.local_closed_at = Some(Instant::now());
        }
    }

    /// Whether local side is closed.
    #[inline]
    pub fn is_local_closed(&self) -> bool {
        self.local_closed.load(Ordering::Acquire)
    }

    /// Whether remote side is closed.
    #[inline]
    pub fn is_remote_closed(&self) -> bool {
        self.remote_closed.load(Ordering::Acquire)
    }

    /// Mark that a FIN frame has been sent.
    #[inline]
    pub fn mark_fin_sent(&self) {
        self.fin_sent.store(true, Ordering::Release);
    }

    /// Whether a FIN frame has been sent.
    #[inline]
    pub fn is_fin_sent(&self) -> bool {
        self.fin_sent.load(Ordering::Acquire)
    }

    /// Time since `mark_local_closed` / `force_local_closed_at`, if local is closed.
    pub fn local_closed_elapsed(&self) -> Option<Duration> {
        self.recv.lock().local_closed_at.map(|t| t.elapsed())
    }

    /// Force local-closed timestamp (tests / aged reap).
    pub fn force_local_closed_at(&self, at: Instant) {
        self.local_closed.store(true, Ordering::Release);
        self.recv.lock().local_closed_at = Some(at);
    }

    /// Drop send/recv buffers without forging `remote_closed` or `fin_sent`.
    ///
    /// Lock order: recv then send (L1).
    pub fn clear_buffers(&self) {
        {
            let mut r = self.recv.lock();
            r.recv.clear();
        }
        {
            let mut s = self.send.lock();
            s.send.clear();
        }
        self.recv_buf_bytes_avail.store(0, Ordering::Relaxed);
        self.send_buf_bytes.store(0, Ordering::Relaxed);
    }

    /// Apply a peer UPD frame (consumed + window) — matching Go `stream.update`.
    pub fn apply_peer_update(&self, consumed: u32, window: u32) {
        let old_effective = self.peer_send_window();
        self.peer_consumed.store(consumed, Ordering::Release);
        self.peer_window.store(window, Ordering::Release);
        if old_effective == 0 && self.peer_send_window() > 0 {
            self.wakeup_writer();
        }
    }

    /// Disable write-side peer window (SMUX v1 has no UPD / no per-stream window).
    pub fn disable_peer_window(&self) {
        self.peer_consumed.store(0, Ordering::Release);
        self.peer_window.store(u32::MAX, Ordering::Release);
    }

    /// Remaining send window toward the peer (v2 flow control).
    pub fn peer_send_window(&self) -> u32 {
        let window = self.peer_window.load(Ordering::Acquire);
        if window == u32::MAX {
            return u32::MAX;
        }
        let written = self.bytes_written.load(Ordering::Acquire);
        let consumed = self.peer_consumed.load(Ordering::Acquire);
        let inflight = written.wrapping_sub(consumed);
        let inflight_i = inflight as i32;
        if inflight_i < 0 {
            return 0;
        }
        let win = (window as i32).saturating_sub(inflight_i);
        if win <= 0 {
            0
        } else {
            win as u32
        }
    }

    /// Get pending UPD state and reset the flag.
    pub fn take_upd(&self) -> Option<(u32, u32)> {
        if self.pending_upd.swap(false, Ordering::Acquire) {
            Some((
                self.upd_consumed.load(Ordering::Relaxed),
                self.max_recv_buf as u32,
            ))
        } else {
            None
        }
    }

    /// Close the stream fully (both sides + clear buffers).
    ///
    /// Lock order: recv then send. Wakers fired after locks are dropped.
    pub fn close(&self) {
        self.local_closed.store(true, Ordering::Release);
        self.remote_closed.store(true, Ordering::Release);
        let read_waker;
        let write_waker;
        {
            let mut r = self.recv.lock();
            if r.local_closed_at.is_none() {
                r.local_closed_at = Some(Instant::now());
            }
            r.state = StreamState::Closed;
            r.recv.clear();
            read_waker = r.read_waker.take();
        }
        {
            let mut s = self.send.lock();
            s.send.clear();
            write_waker = s.write_waker.take();
        }
        self.recv_buf_bytes_avail.store(0, Ordering::Relaxed);
        self.send_buf_bytes.store(0, Ordering::Relaxed);
        self.ch_reader_wakeup.notify_one();
        self.ch_write_wakeup.notify_one();
        if let Some(w) = read_waker {
            w.wake();
        }
        if let Some(w) = write_waker {
            w.wake();
        }
    }

}
/// Shared read logic for `AsyncRead` impls on `Stream` (and Arc wrappers).
///
/// tokio (via `ReadBuf`) reduces to this after extracting the raw byte slice. Returns:
/// - `Ok(0)` → EOF (stream closed or peer sent FIN)
/// - `Ok(n)` → read `n` bytes
/// - `Err(_)` → connection reset
/// - `Pending` → would block, waker registered
pub(crate) fn poll_read_into(
    stream: &Stream,
    waker: &std::task::Waker,
    buf: &mut [u8],
) -> Poll<io::Result<usize>> {
    match stream.read(buf) {
        Ok((0, _)) => Poll::Ready(Ok(0)),
        Ok((n, _)) => Poll::Ready(Ok(n)),
        Err(StreamError::WouldBlock) => {
            stream.register_read_waker(waker.clone());
            // Re-check after registering (lost-wakeup race).
            match stream.read(buf) {
                Ok((0, _)) => Poll::Ready(Ok(0)),
                Ok((n, _)) => Poll::Ready(Ok(n)),
                Err(StreamError::WouldBlock) => Poll::Pending,
                Err(StreamError::Closed) => Poll::Ready(Ok(0)),
                Err(e) => Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::ConnectionReset,
                    format!("SMUX read error: {:?}", e),
                ))),
            }
        }
        Err(StreamError::Closed) => Poll::Ready(Ok(0)),
        Err(e) => Poll::Ready(Err(io::Error::new(
            io::ErrorKind::ConnectionReset,
            format!("SMUX read error: {:?}", e),
        ))),
    }
}

// ─── knet::AsyncRead / AsyncWrite impls (standalone async support) ─────────

impl knet::AsyncRead for Stream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut knet::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let space = buf.initialize_unfilled();
        match poll_read_into(&self, cx.waker(), space) {
            Poll::Ready(Ok(0)) => Poll::Ready(Ok(())),
            Poll::Ready(Ok(n)) => {
                buf.advance(n);
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl knet::AsyncWrite for Stream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.local_closed.load(Ordering::Acquire) {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "SMUX stream closed",
            )));
        }
        // v2 write-side flow control: block when peer window is full.
        let peer_win = self.peer_send_window();
        if peer_win == 0 {
            self.register_write_waker(cx.waker().clone());
            // Re-check after registering waker (lost-wakeup race).
            if self.peer_send_window() == 0 {
                return Poll::Pending;
            }
        }
        match self.write(buf) {
            Ok(n) => {
                self.notify_flush();
                Poll::Ready(Ok(n))
            }
            Err(StreamError::Closed) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "SMUX stream closed",
            ))),
            Err(_) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "SMUX write error",
            ))),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // Writes are buffered; flush is a no-op (the external flush loop
        // drains send_buf through the Session). Standalone users should
        // ensure Session::prepare_outbound_into is called to drain.
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        debug!(
            "Stream::poll_shutdown: marking stream {} local_closed",
            self.id
        );
        self.mark_local_closed();
        self.notify_flush();
        Poll::Ready(Ok(()))
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn stream_create_and_close() {
        let stream = Stream::new(1);
        assert_eq!(stream.id(), 1);
        assert!(!stream.is_closed());
        stream.close();
        assert!(stream.is_closed());
    }

    #[test]
    fn clear_buffers_drops_send_and_recv_without_forging_remote_closed() {
        let stream = Stream::with_buffer(1, 2 * 1024 * 1024);
        stream.write(b"pending").unwrap();
        stream.push_data(b"inbound").unwrap();
        assert!(stream.pending_send() > 0);
        assert!(stream.available() > 0);

        stream.mark_local_closed();
        stream.clear_buffers();

        assert_eq!(stream.pending_send(), 0);
        assert_eq!(stream.available(), 0);
        assert!(stream.is_local_closed());
        assert!(
            !stream.is_remote_closed(),
            "clear_buffers must not forge remote_closed"
        );
        assert!(!stream.is_fin_sent());
    }

    #[test]
    fn with_buffer_does_not_preallocate_full_streambuf_capacity() {
        // Historical leak amplifier: BytesMut::with_capacity(streambuf) reserved ~2MB
        // per stream even when empty. Capacity must stay small until data arrives.
        let stream = Stream::with_buffer(9, 2 * 1024 * 1024);
        let cap = stream.recv_buf_capacity();
        assert!(
            cap < 64 * 1024,
            "recv_buf capacity should be lazy, got {}",
            cap
        );
    }

    #[test]
    fn mark_local_closed_stamps_elapsed() {
        let stream = Stream::new(1);
        assert!(stream.local_closed_elapsed().is_none());
        stream.mark_local_closed();
        let e = stream
            .local_closed_elapsed()
            .expect("elapsed after local close");
        assert!(e < std::time::Duration::from_secs(2));
    }

    #[test]
    fn force_local_closed_at_allows_aged_reap_tests() {
        let stream = Stream::new(1);
        let past = std::time::Instant::now() - std::time::Duration::from_secs(60);
        stream.force_local_closed_at(past);
        assert!(stream.is_local_closed());
        let e = stream.local_closed_elapsed().unwrap();
        assert!(e >= std::time::Duration::from_secs(59));
    }

    #[test]
    fn stream_push_and_read() {
        let stream = Stream::new(1);
        stream.push_data(b"hello").unwrap();
        assert_eq!(stream.available(), 5);

        let mut buf = [0u8; 16];
        let n = stream.read(&mut buf).unwrap();
        assert_eq!(n, (5, true)); // first read always sets need_upd (matching Go)
        assert_eq!(&buf[..5], b"hello");
    }

    #[test]
    fn stream_write_and_drain() {
        let stream = Stream::new(1);
        stream.write(b"data").unwrap();
        assert_eq!(stream.pending_send(), 4);

        let mut drain_buf = BytesMut::new();
        let n = stream.drain_send(&mut drain_buf);
        assert_eq!(n, 4);
        assert_eq!(&drain_buf[..], b"data");
    }

    #[test]
    fn stream_write_bytes_and_drain() {
        let s = Stream::new(7);
        s.write_bytes(Bytes::from_static(b"hello ")).unwrap();
        s.write_bytes(Bytes::from_static(b"world")).unwrap();
        assert_eq!(s.pending_send(), 11);
        let mut out = BytesMut::new();
        let n = s.drain_send_max(&mut out, 5);
        assert_eq!(n, 5);
        assert_eq!(&out[..], b"hello");
        assert_eq!(s.pending_send(), 6);
        out.clear();
        let n = s.drain_send(&mut out);
        assert_eq!(n, 6);
        assert_eq!(&out[..], b" world");
        assert_eq!(s.pending_send(), 0);
    }

    #[test]
    fn stream_state_transitions() {
        let stream = Stream::new(1);
        assert_eq!(stream.state(), StreamState::Init);
        stream.set_state(StreamState::Ready);
        assert_eq!(stream.state(), StreamState::Ready);
        assert!(stream.is_ready());
        stream.set_state(StreamState::Closed);
        assert_eq!(stream.state(), StreamState::Closed);
        assert!(stream.is_closed());
    }

    #[test]
    fn stream_multiple_writes() {
        let stream = Stream::new(1);
        stream.write(b"a").unwrap();
        stream.write(b"b").unwrap();
        stream.write(b"c").unwrap();
        assert_eq!(stream.pending_send(), 3);
    }

    #[test]
    fn stream_read_would_block() {
        let stream = Stream::new(1);
        let mut buf = [0u8; 4];
        let result = stream.read(&mut buf);
        assert_eq!(result, Err(StreamError::WouldBlock));
    }

    #[test]
    fn stream_tracking() {
        let stream = Stream::new(1);
        stream.push_data(b"hello").unwrap();
        let mut buf = [0u8; 5];
        stream.read(&mut buf).unwrap();
        assert_eq!(stream.bytes_read.load(Ordering::Relaxed), 5);
        stream.write(b"world").unwrap();
        // bytes_written tracks on-wire bytes (Go numWritten) — only after drain.
        assert_eq!(stream.bytes_written.load(Ordering::Relaxed), 0);
        assert_eq!(stream.pending_send(), 5);
        let mut out = bytes::BytesMut::new();
        assert_eq!(stream.drain_send_max(&mut out, 64), 5);
        assert_eq!(stream.bytes_written.load(Ordering::Relaxed), 5);
        assert_eq!(&out[..], b"world");
    }

    #[test]
    fn peer_window_limits_drain() {
        let stream = Stream::new(1);
        let big = vec![b'x'; 300 * 1024];
        stream.write(&big).unwrap();
        let mut out = bytes::BytesMut::new();
        let n1 = stream.drain_send_max(&mut out, usize::MAX);
        assert_eq!(n1, 262144, "first drain capped at initialPeerWindow");
        assert_eq!(stream.drain_send_max(&mut out, usize::MAX), 0);
        stream.apply_peer_update(100 * 1024, 2 * 1024 * 1024);
        let n2 = stream.drain_send_max(&mut out, usize::MAX);
        assert!(n2 > 0);
        assert_eq!(n1 + n2 + stream.pending_send(), big.len());
    }

    #[test]
    fn stream_read_async_returns_data() {
        let stream = Arc::new(Stream::new(1));
        let s = stream.clone();
        // Use std::thread instead of spawn_task to avoid executor lifecycle
        // issues in tests.
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(5));
            s.push_data(b"hello async").unwrap();
        });
        knet::block_on(async {
            let mut buf = [0u8; 32];
            let (n, _) = stream.read_async(&mut buf).await.unwrap();
            assert_eq!(n, 11);
            assert_eq!(&buf[..11], b"hello async");
        });
    }

    #[test]
    fn stream_read_async_returns_closed_on_fin() {
        let stream = Arc::new(Stream::new(1));
        let s = stream.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(5));
            s.mark_remote_closed();
        });
        knet::block_on(async {
            let mut buf = [0u8; 32];
            let result = stream.read_async(&mut buf).await;
            assert_eq!(result, Err(StreamError::Closed));
        });
    }

    #[test]
    fn stream_read_async_fin_with_data() {
        let stream = Arc::new(Stream::new(1));
        let s = stream.clone();
        std::thread::spawn(move || {
            s.push_data(b"last data").unwrap();
            std::thread::sleep(std::time::Duration::from_millis(5));
            s.mark_remote_closed();
        });
        knet::block_on(async {
            let mut buf = [0u8; 32];
            let (n, _) = stream.read_async(&mut buf).await.unwrap();
            assert_eq!(n, 9);
            assert_eq!(&buf[..9], b"last data");
        });
    }

    #[test]
    fn stream_poll_read_returns_data_via_trait() {
        let mut stream = Stream::new(1);
        stream.push_data(b"hello trait").unwrap();
        knet::block_on(async {
            let mut buf = [0u8; 32];
            // &mut Stream implements AsyncRead via the blanket impl
            // for T: AsyncRead + Unpin.
            let n = knet::AsyncReadExt::read(&mut stream, &mut buf).await.unwrap();
            assert_eq!(n, 11);
            assert_eq!(&buf[..11], b"hello trait");
        });
    }

    #[test]
    fn stream_poll_write_and_then_poll_read_shutdown() {
        knet::block_on(async {
            use knet::AsyncWriteExt;
            let mut stream = Stream::new(1);

            // Write some data via AsyncWrite trait
            let n = knet::AsyncWriteExt::write(&mut stream, b"hello world").await.unwrap();
            assert_eq!(n, 11);

            // Verify via the sync method
            assert_eq!(stream.pending_send(), 11);

            // Shutdown (half-close local side).
            // tokio's AsyncWriteExt provides shutdown().
            stream.shutdown().await.unwrap();
            assert!(stream.is_local_closed());
        });
    }
    #[test]
    fn set_flush_notify_wakes_on_async_write() {
        knet::block_on(async {
            
            let notify = Arc::new(knet::Notify::new());
            let mut stream = Stream::new(1);
            stream.set_flush_notify(notify.clone());
            let waiter = knet::spawn_task(async move {
                notify.notified().await;
            });
            knet::sleep_ms(1).await;
            let n = knet::AsyncWriteExt::write(&mut stream, b"ping").await.unwrap();
            assert_eq!(n, 4);
            let _ = knet::timeout(std::time::Duration::from_millis(200), waiter).await;
        });
    }
}
