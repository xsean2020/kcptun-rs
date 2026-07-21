//! SMUX logical stream implementation.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

use bytes::{Buf, Bytes, BytesMut};
use parking_lot::Mutex;

/// Error returned by stream operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamError {
    /// Stream has been closed.
    Closed,
    /// Stream has been reset.
    Reset,
    /// Buffer overflow.
    BufferOverflow,
    /// Not enough data available.
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

/// A single logical stream within a SMUX session.
///
/// Each stream has a unique ID within its session and supports
/// bidirectional data transfer.
pub struct Stream {
    /// Stream ID (unique within the session).
    id: u32,
    /// Current state.
    state: Arc<Mutex<StreamState>>,
    /// Receive buffer — legacy contiguous buffer (used by push_data).
    recv_buf: Arc<Mutex<BytesMut>>,
    /// Receive buffer — zero-copy chunks (used by push_data_bytes).
    /// Each `Bytes` is a reference-counted view into the codec buffer,
    /// avoiding a copy on push. Consumed by `read()`.
    recv_buf_bytes: Arc<Mutex<VecDeque<Bytes>>>,
    /// Maximum receive buffer size.
    max_recv_buf: usize,
    /// Send buffer — zero-copy chunks waiting to be drained by flush.
    /// `write` / `write_bytes` push; `drain_send_max` copies into the caller's
    /// frame assembly buffer once (P1.4).
    send_buf: Arc<Mutex<VecDeque<Bytes>>>,
    /// Number of bytes read by the consumer.
    bytes_read: Arc<AtomicU32>,
    /// Number of bytes written by the consumer.
    bytes_written: Arc<AtomicU32>,
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
    /// Fires on both data arrival (`wakeup_reader`) and FIN (`fin_event`).
    ch_reader_wakeup: kio::Notify,
    /// Stored waker for poll_read-based async wrappers.
    /// Set by `register_read_waker`, woken by `wakeup_reader`.
    read_waker: Mutex<Option<std::task::Waker>>,
}

impl Stream {
    /// Create a new stream with the given ID.
    pub fn new(id: u32) -> Self {
        Stream {
            id,
            state: Arc::new(Mutex::new(StreamState::Init)),
            recv_buf: Arc::new(Mutex::new(BytesMut::with_capacity(65536))),
            recv_buf_bytes: Arc::new(Mutex::new(VecDeque::new())),
            max_recv_buf: 4 * 1024 * 1024, // 4MB default
            send_buf: Arc::new(Mutex::new(VecDeque::new())),
            bytes_read: Arc::new(AtomicU32::new(0)),
            bytes_written: Arc::new(AtomicU32::new(0)),
            opened: AtomicBool::new(false),
            remote_closed: AtomicBool::new(false),
            local_closed: AtomicBool::new(false),
            fin_sent: AtomicBool::new(false),
            incr: AtomicU32::new(0),
            upd_consumed: AtomicU32::new(0),
            pending_upd: AtomicBool::new(false),
            peer_consumed: AtomicU32::new(0),
            peer_window: AtomicU32::new(262144), // Go initialPeerWindow
            ch_reader_wakeup: kio::Notify::new(),
            read_waker: Mutex::new(None),
        }
    }

    /// Create a new stream with custom buffer capacity.
    pub fn with_buffer(id: u32, recv_capacity: usize) -> Self {
        Stream {
            id,
            state: Arc::new(Mutex::new(StreamState::Init)),
            recv_buf: Arc::new(Mutex::new(BytesMut::with_capacity(recv_capacity))),
            recv_buf_bytes: Arc::new(Mutex::new(VecDeque::new())),
            max_recv_buf: recv_capacity,
            send_buf: Arc::new(Mutex::new(VecDeque::new())),
            bytes_read: Arc::new(AtomicU32::new(0)),
            bytes_written: Arc::new(AtomicU32::new(0)),
            opened: AtomicBool::new(false),
            remote_closed: AtomicBool::new(false),
            local_closed: AtomicBool::new(false),
            fin_sent: AtomicBool::new(false),
            incr: AtomicU32::new(0),
            upd_consumed: AtomicU32::new(0),
            pending_upd: AtomicBool::new(false),
            peer_consumed: AtomicU32::new(0),
            peer_window: AtomicU32::new(262144), // Go initialPeerWindow
            ch_reader_wakeup: kio::Notify::new(),
            read_waker: Mutex::new(None),
        }
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
            *self.state.lock(),
            StreamState::Closed | StreamState::Reset
        )
    }

    /// Check if the stream is ready for data transfer.
    #[inline]
    pub fn is_ready(&self) -> bool {
        *self.state.lock() == StreamState::Ready
    }

    /// Get the current state.
    #[inline]
    pub fn state(&self) -> StreamState {
        *self.state.lock()
    }

    /// Set the stream state.
    #[inline]
    pub fn set_state(&self, new_state: StreamState) {
        *self.state.lock() = new_state;
    }

    /// Mark the stream as opened (SYN sent).
    #[inline]
    pub fn mark_opened(&self) {
        self.opened.store(true, Ordering::Release);
    }

    /// Check if the stream has been opened.
    #[inline]
    pub fn is_opened(&self) -> bool {
        self.opened.load(Ordering::Acquire)
    }

    /// Wake up any reader blocked in `read_async()` or async `read()`.
    /// Also wakes any poll_read-based reader that registered a waker.
    #[inline]
    pub fn wakeup_reader(&self) {
        self.ch_reader_wakeup.notify_one();
        if let Some(w) = self.read_waker.lock().take() {
            w.wake();
        }
    }

    /// Register a waker for poll_read-based async readers.
    /// The waker will be called by `wakeup_reader()` when data arrives
    /// or by `fin_event()` when the remote side closes.
    #[inline]
    pub fn register_read_waker(&self, waker: std::task::Waker) {
        *self.read_waker.lock() = Some(waker);
    }

    /// Signal that a FIN event has been received (remote closed).
    /// This wakes any reader blocked in `read_async()` and any
    /// poll_read-based reader that registered a waker.
    #[inline]
    pub fn fin_event(&self) {
        self.ch_reader_wakeup.notify_one();
        if let Some(w) = self.read_waker.lock().take() {
            w.wake();
        }
    }

    /// Push incoming data into the receive buffer.
    ///
    /// Routes through the zero-copy `Bytes` queue (same as `push_data_bytes`)
    /// so the hot path does not use the legacy contiguous `recv_buf` (P1.4).
    pub fn push_data(&self, data: &[u8]) -> Result<(), StreamError> {
        if data.is_empty() {
            return Ok(());
        }
        self.push_data_bytes(Bytes::copy_from_slice(data))
    }

    /// Push incoming data as a `Bytes` (zero-copy append).
    ///
    /// The `Bytes` is stored as-is in the receive buffer without copying.
    /// When `read()` is called, data is copied from the stored `Bytes`
    /// chunks into the caller's buffer (which is unavoidable because the
    /// caller provides `&mut [u8]`).
    pub fn push_data_bytes(&self, data: bytes::Bytes) -> Result<(), StreamError> {
        if data.is_empty() {
            return Ok(());
        }
        let mut recv = self.recv_buf_bytes.lock();
        recv.push_back(data);
        // Wake up any waiting reader
        self.wakeup_reader();
        Ok(())
    }

    /// Read data from the receive buffer.
    ///
    /// Returns the number of bytes read and whether a UPD update should be
    /// sent (for V2 flow control).
    pub fn read(&self, buf: &mut [u8]) -> Result<(usize, bool), StreamError> {
        // ── First: drain zero-copy Bytes chunks ──
        {
            let mut recv_bytes = self.recv_buf_bytes.lock();
            if !recv_bytes.is_empty() {
                let mut offset = 0;
                while offset < buf.len() && !recv_bytes.is_empty() {
                    let front = recv_bytes.front_mut().unwrap();
                    let to_copy = front.len().min(buf.len() - offset);
                    buf[offset..offset + to_copy].copy_from_slice(&front[..to_copy]);
                    offset += to_copy;
                    if to_copy == front.len() {
                        recv_bytes.pop_front();
                    } else {
                        *front = front.slice(to_copy..);
                    }
                }
                if offset > 0 {
                    let was_empty =
                        self.bytes_read.fetch_add(offset as u32, Ordering::Relaxed) == 0;
                    let incr_val =
                        self.incr.fetch_add(offset as u32, Ordering::Relaxed) + offset as u32;
                    let need_upd = if incr_val >= (self.max_recv_buf as u32 / 2) || was_empty {
                        self.incr.store(0, Ordering::Relaxed);
                        let consumed = self.bytes_read.load(Ordering::Relaxed);
                        self.upd_consumed.store(consumed, Ordering::Relaxed);
                        self.pending_upd.store(true, Ordering::Release);
                        true
                    } else {
                        false
                    };
                    return Ok((offset, need_upd));
                }
            }
        }

        // ── Fallback: legacy contiguous buffer ──
        let mut recv = self.recv_buf.lock();
        if recv.is_empty() {
            if self.remote_closed.load(Ordering::Acquire) {
                return Err(StreamError::Closed);
            }
            return Err(StreamError::WouldBlock);
        }

        let to_read = buf.len().min(recv.len());
        buf[..to_read].copy_from_slice(&recv[..to_read]);
        recv.advance(to_read);

        // V2 flow control: track incremental reads for UPD generation (matching Go)
        let was_empty = self.bytes_read.fetch_add(to_read as u32, Ordering::Relaxed) == 0;
        let incr_val = self.incr.fetch_add(to_read as u32, Ordering::Relaxed) + to_read as u32;
        let need_upd = if incr_val >= (self.max_recv_buf as u32 / 2) || was_empty {
            self.incr.store(0, Ordering::Relaxed);
            let consumed = self.bytes_read.load(Ordering::Relaxed);
            self.upd_consumed.store(consumed, Ordering::Relaxed);
            self.pending_upd.store(true, Ordering::Release);
            true
        } else {
            false
        };

        Ok((to_read, need_upd))
    }

    /// Async read — waits for data or FIN, like Go's tryRead.
    ///
    /// Waits until data is available in the receive buffer or the remote
    /// side has sent a FIN/closed. Returns the number of bytes read and
    /// whether a UPD update should be sent.
    pub async fn read_async(&self, buf: &mut [u8]) -> Result<(usize, bool), StreamError> {
        loop {
            match self.read(buf) {
                Ok(result) => return Ok(result),
                Err(StreamError::WouldBlock) => {
                    if self.remote_closed.load(Ordering::Acquire) {
                        return Err(StreamError::Closed);
                    }
                    // Wait for data arrival or FIN — both wake ch_reader_wakeup.
                    self.ch_reader_wakeup.notified().await;
                    // Loop back and try reading again. If remote closed with
                    // pending data, read() returns Ok; if closed with no data,
                    // read() returns Closed.
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Write data to the send buffer (copies into an owned `Bytes` chunk).
    pub fn write(&self, data: &[u8]) -> Result<usize, StreamError> {
        if data.is_empty() {
            return Ok(0);
        }
        self.write_bytes(Bytes::copy_from_slice(data))
    }

    /// Write an owned `Bytes` chunk with no extra copy (P1.4).
    pub fn write_bytes(&self, data: Bytes) -> Result<usize, StreamError> {
        if self.local_closed.load(Ordering::Acquire) {
            return Err(StreamError::Closed);
        }
        if data.is_empty() {
            return Ok(0);
        }
        let n = data.len();
        self.send_buf.lock().push_back(data);
        // Note: bytes_written tracks *on-wire* bytes (incremented in drain_send_max),
        // matching Go smux `numWritten` which advances only when frames are transmitted.
        Ok(n)
    }

    /// Drain pending send data into the provided buffer, up to `max_bytes`.
    ///
    /// Copies from queued `Bytes` chunks into `buf` (single copy on the flush path).
    pub fn drain_send_max(&self, buf: &mut BytesMut, max_bytes: usize) -> usize {
        // v2 write-side flow control: never emit more than the peer window allows.
        let peer_win = self.peer_send_window() as usize;
        let max_bytes = max_bytes.min(peer_win);
        if max_bytes == 0 {
            return 0;
        }
        let mut send = self.send_buf.lock();
        if send.is_empty() {
            return 0;
        }
        let mut drained = 0usize;
        while drained < max_bytes && !send.is_empty() {
            let front = send.front_mut().unwrap();
            let take = front.len().min(max_bytes - drained);
            buf.extend_from_slice(&front[..take]);
            drained += take;
            if take == front.len() {
                send.pop_front();
            } else {
                *front = front.split_off(take);
            }
        }
        if drained > 0 {
            // Count transmitted bytes for peer-window inflight (Go numWritten).
            self.bytes_written
                .fetch_add(drained as u32, Ordering::Relaxed);
        }
        drained
    }

    /// Drain all pending send data.
    #[inline]
    pub fn drain_send(&self, buf: &mut BytesMut) -> usize {
        self.drain_send_max(buf, usize::MAX)
    }

    /// Get the number of bytes available to read.
    #[inline]
    pub fn available(&self) -> usize {
        let n = self
            .recv_buf_bytes
            .lock()
            .iter()
            .map(|b| b.len())
            .sum::<usize>();
        if n > 0 {
            return n;
        }
        // Legacy contiguous buffer (rare after push_data routes to Bytes).
        self.recv_buf.lock().len()
    }

    /// Get the number of bytes waiting to be sent.
    #[inline]
    pub fn pending_send(&self) -> usize {
        self.send_buf.lock().iter().map(|b| b.len()).sum()
    }

    /// Mark the remote side as closed.
    #[inline]
    pub fn mark_remote_closed(&self) {
        self.remote_closed.store(true, Ordering::Release);
        // Wake up any waiting reader and signal FIN event
        self.fin_event();
    }

    /// Mark the local side as closed.
    #[inline]
    pub fn mark_local_closed(&self) {
        self.local_closed.store(true, Ordering::Release);
    }

    /// Check if the local side has been closed.
    #[inline]
    pub fn is_local_closed(&self) -> bool {
        self.local_closed.load(Ordering::Acquire)
    }

    /// Check if the remote side has been closed.
    #[inline]
    pub fn is_remote_closed(&self) -> bool {
        self.remote_closed.load(Ordering::Acquire)
    }

    /// Mark that a FIN frame has been sent for this stream.
    #[inline]
    pub fn mark_fin_sent(&self) {
        self.fin_sent.store(true, Ordering::Release);
    }

    /// Checks if a FIN frame has already been sent.
    #[inline]
    pub fn is_fin_sent(&self) -> bool {
        self.fin_sent.load(Ordering::Acquire)
    }

    /// Apply a peer UPD frame (consumed + window) — matching Go `stream.update`.
    pub fn apply_peer_update(&self, consumed: u32, window: u32) {
        self.peer_consumed.store(consumed, Ordering::Release);
        self.peer_window.store(window, Ordering::Release);
    }

    /// Disable write-side peer window (SMUX v1 has no UPD / no per-stream window).
    pub fn disable_peer_window(&self) {
        self.peer_consumed.store(0, Ordering::Release);
        self.peer_window.store(u32::MAX, Ordering::Release);
    }

    /// Remaining send window toward the peer (v2 flow control).
    ///
    /// `peer_window - (bytes_written - peer_consumed)` with modular u32 math,
    /// matching Go smux `writeV2`. Returns `u32::MAX` when the window is
    /// effectively unlimited / not yet constrained.
    pub fn peer_send_window(&self) -> u32 {
        let window = self.peer_window.load(Ordering::Acquire);
        // v1 / unlimited: peer_window set to u32::MAX via disable_peer_window().
        if window == u32::MAX {
            return u32::MAX;
        }
        let written = self.bytes_written.load(Ordering::Acquire);
        let consumed = self.peer_consumed.load(Ordering::Acquire);
        let inflight = written.wrapping_sub(consumed);
        // Treat as signed like Go's int32(inflight)
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

    /// Close the stream fully.
    pub fn close(&self) {
        self.local_closed.store(true, Ordering::Release);
        self.remote_closed.store(true, Ordering::Release);
        *self.state.lock() = StreamState::Closed;
        self.recv_buf.lock().clear();
        self.recv_buf_bytes.lock().clear();
        self.send_buf.lock().clear();
        // Wake up any waiting readers
        self.fin_event();
    }

    /// Get the number of bytes read in total.
    #[inline]
    pub fn bytes_read_total(&self) -> u32 {
        self.bytes_read.load(Ordering::Relaxed)
    }

    /// Get the number of bytes written in total.
    #[inline]
    pub fn bytes_written_total(&self) -> u32 {
        self.bytes_written.load(Ordering::Relaxed)
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_create_and_close() {
        let stream = Stream::new(1);
        assert_eq!(stream.id(), 1);
        assert!(!stream.is_closed());
        stream.close();
        assert!(stream.is_closed());
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
        assert_eq!(stream.bytes_read_total(), 5);
        stream.write(b"world").unwrap();
        // bytes_written tracks on-wire bytes (Go numWritten) — only after drain.
        assert_eq!(stream.bytes_written_total(), 0);
        assert_eq!(stream.pending_send(), 5);
        let mut out = bytes::BytesMut::new();
        assert_eq!(stream.drain_send_max(&mut out, 64), 5);
        assert_eq!(stream.bytes_written_total(), 5);
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
        // issues in tests (smol global executor doesn't shut down cleanly).
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(5));
            s.push_data(b"hello async").unwrap();
        });
        kio::block_on(async {
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
        kio::block_on(async {
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
        kio::block_on(async {
            let mut buf = [0u8; 32];
            let (n, _) = stream.read_async(&mut buf).await.unwrap();
            assert_eq!(n, 9);
            assert_eq!(&buf[..9], b"last data");
        });
    }
}
