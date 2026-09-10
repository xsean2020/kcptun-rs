//! SMUX session — the core multiplexer that manages multiple streams over a
//! single transport connection.
//!
//! A `Session` wraps a transport `io::Read + io::Write` and provides:
//! - Opening and accepting streams
//! - Multiplexing data frames across streams
//! - Keepalive (ping/pong)
//! - Graceful shutdown

use log::debug;
use std::collections::{HashMap, VecDeque};
use std::io::{self};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;

use bytes::{Bytes, BytesMut};

use crate::frame::{Cmd, Frame, FrameCodec};
use crate::stream::{Stream, StreamState};

/// Highest stream id an `open_stream` may hand out.
///
/// This is an id-space limit, not a concurrency limit: ids are never reused,
/// so the counter only stops when it runs out of room (Go signals `ErrGoAway`
/// when `nextStreamID` wraps past 2^32).
const MAX_STREAMS: u32 = u32::MAX - 1;
/// SMUX session configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// SMUX protocol version (1 or 2).
    pub version: u8,
    /// Maximum overall receive buffer for the session (bytes).
    pub max_receive_buffer: usize,
    /// Maximum per-stream receive buffer (bytes).
    pub max_stream_buffer: usize,
    /// Maximum frame size (bytes).
    pub max_frame_size: usize,
    /// Keepalive interval in seconds.
    pub keepalive_interval: u64,
    /// Keepalive timeout in seconds (0 = disabled).
    pub keepalive_timeout: u64,
}

/// Default SMUX configuration.
pub static DEFAULT_CONFIG: Config = Config {
    version: 1,
    max_receive_buffer: 4 * 1024 * 1024,
    max_stream_buffer: 256 * 1024,
    max_frame_size: 16 * 1024,
    keepalive_interval: 10,
    keepalive_timeout: 30,
};

impl Config {
    /// Verify that the configuration is valid.
    pub fn verify(&self) -> Result<(), SessionError> {
        if self.version != 1 && self.version != 2 {
            return Err(SessionError::InvalidConfig(format!(
                "unsupported smux version: {}",
                self.version
            )));
        }
        if self.keepalive_timeout != 0 {
            if self.keepalive_interval == 0 {
                return Err(SessionError::InvalidConfig(
                    "keep-alive interval must be positive".into(),
                ));
            }
            if self.keepalive_timeout < self.keepalive_interval {
                return Err(SessionError::InvalidConfig(
                    "keep-alive timeout must not be shorter than interval".into(),
                ));
            }
        }
        if self.max_frame_size == 0 {
            return Err(SessionError::InvalidConfig(
                "max frame size must be positive".into(),
            ));
        }
        if self.max_frame_size > u16::MAX as usize {
            return Err(SessionError::InvalidConfig(
                "max frame size must not exceed 65535".into(),
            ));
        }
        if self.max_receive_buffer == 0 {
            return Err(SessionError::InvalidConfig(
                "max receive buffer must be positive".into(),
            ));
        }
        if self.max_receive_buffer > i32::MAX as usize {
            return Err(SessionError::InvalidConfig(
                "max receive buffer must not exceed 2147483647".into(),
            ));
        }
        if self.max_stream_buffer == 0 {
            return Err(SessionError::InvalidConfig(
                "max stream buffer must be positive".into(),
            ));
        }
        if self.max_stream_buffer > self.max_receive_buffer {
            return Err(SessionError::InvalidConfig(
                "max stream buffer must not exceed max receive buffer".into(),
            ));
        }
        if self.max_stream_buffer > i32::MAX as usize {
            return Err(SessionError::InvalidConfig(
                "max stream buffer must not exceed 2147483647".into(),
            ));
        }
        Ok(())
    }
}

/// Errors from the SMUX session.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// I/O error from the underlying transport.
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    /// Invalid configuration.
    #[error("invalid config: {0}")]
    InvalidConfig(String),
    /// Session is closed.
    #[error("session closed")]
    SessionClosed,
    /// Maximum number of streams reached.
    #[error("too many streams")]
    TooManyStreams,
    /// Stream not found.
    #[error("stream {0} not found")]
    StreamNotFound(u32),
    /// Invalid frame received.
    #[error("invalid frame: {0}")]
    InvalidFrame(String),
}

/// A pending UPD frame to be sent to the peer.
///
/// Retained for API compatibility; UPD frames are now emitted directly
/// into the outbound buffer by [`Session::emit_upd_frames`], so this
/// struct is no longer produced by any Session method.
#[derive(Debug, Clone)]
pub struct UpdFrame {
    pub stream_id: u32,
    pub consumed: u32,
    pub window: u32,
}

/// The SMUX session — multiplexes streams over a single transport.
pub struct Session {
    /// Session configuration.
    config: Config,
    /// Whether the session is closed.
    closed: Arc<AtomicBool>,
    /// All active streams, keyed by stream ID.
    streams: Arc<Mutex<HashMap<u32, Arc<Stream>>>>,
    /// Next stream ID to assign (for client: odd, server: even).
    next_stream_id: AtomicU32,
    /// Frame codec for encoding/decoding frames.
    codec: Arc<Mutex<FrameCodec>>,
    /// Keepalive interval.
    keepalive_interval: Duration,
    /// Time of last keepalive.
    last_keepalive_ms: AtomicU64,
    /// Time of last inbound activity (any frame).
    last_activity_ms: AtomicU64,
    /// Maximum streams allowed.
    max_streams: u32,
    /// Token bucket for receive flow control (bytes remaining).
    token_bucket: AtomicI32,
    /// Pending SYN frames to send (queued by SmuxConn::open_stream).
    /// Drained by prepare_outbound_into() at the start of each flush cycle.
    pending_syns: Arc<Mutex<Vec<u32>>>,
    /// Accepted stream IDs waiting for SmuxConn::accept() to pick up.
    /// Only populated when `accept_enabled` is true (SmuxConn server mode).
    accepted_streams: Arc<Mutex<VecDeque<u32>>>,
    /// Notify for waking SmuxConn::accept() when a new stream arrives.
    accept_notify: knet::Notify,
    /// Only push to accepted_streams when true. kcptun never sets this,
    /// so the queue stays empty and there's zero overhead.
    accept_enabled: AtomicBool,
}

impl Session {
    /// Returns the configured SMUX protocol version (1 or 2).
    /// Go smux validates: hdr.Version() != config.Version → reject.
    pub fn version(&self) -> u8 {
        self.config.version
    }

    /// Create a new SMUX session.
    ///
    /// `is_client` controls the starting stream ID: client uses odd IDs
    /// (starting at 1), server uses even IDs (starting at 0).
    fn new(config: &Config, is_client: bool) -> Result<Self, SessionError> {
        config.verify()?;
        let next_id = if is_client { 1 } else { 0 };
        Ok(Session {
            config: config.clone(),
            closed: Arc::new(AtomicBool::new(false)),
            streams: Arc::new(Mutex::new(HashMap::new())),
            next_stream_id: AtomicU32::new(next_id),
            // One frame is at most 8 + 65535 bytes, and `decode` hands out
            // `Bytes` views into this block — a slice retained by a slow
            // stream pins the whole allocation. Sizing it at
            // `max_receive_buffer` (4 MiB by default) meant every session
            // reserved 4 MiB up front and any unread payload kept it alive.
            codec: Arc::new(Mutex::new(FrameCodec::new(
                (crate::frame::FRAME_HEADER_SIZE + u16::MAX as usize)
                    .min(config.max_receive_buffer),
            ))),
            keepalive_interval: Duration::from_secs(config.keepalive_interval),
            last_keepalive_ms: AtomicU64::new(knet::mono_ms()),
            last_activity_ms: AtomicU64::new(knet::mono_ms()),
            max_streams: MAX_STREAMS,
            token_bucket: AtomicI32::new(config.max_receive_buffer as i32),
            pending_syns: Arc::new(Mutex::new(Vec::new())),
            accepted_streams: Arc::new(Mutex::new(VecDeque::new())),
            accept_notify: knet::Notify::new(),
            accept_enabled: AtomicBool::new(false),
        })
    }

    /// Create a new client-side SMUX session.
    ///
    /// A client session initiates stream creation and uses odd-numbered stream IDs.
    #[inline]
    pub fn new_client(config: &Config) -> Result<Self, SessionError> {
        Self::new(config, true)
    }

    /// Create a new server-side SMUX session.
    ///
    /// A server session accepts stream creation and uses even-numbered stream IDs.
    #[inline]
    pub fn new_server(config: &Config) -> Result<Self, SessionError> {
        Self::new(config, false)
    }

    /// Check if the session is closed.
    #[inline]
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    /// Get a reference to the streams map.
    #[inline]
    pub fn streams(&self) -> Arc<Mutex<HashMap<u32, Arc<Stream>>>> {
        self.streams.clone()
    }

    /// Get the session configuration.
    #[inline]
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Get the current token bucket value (remaining receive window in bytes).
    #[inline]
    pub fn token_bucket_value(&self) -> i32 {
        self.token_bucket.load(Ordering::Relaxed)
    }

    /// Whether the session still has receive-window budget.
    ///
    /// A transport read loop must consult this before pulling more data:
    /// with `version = 1` there is no per-stream window at all, so this
    /// bucket is the only thing bounding how much unread data a fast peer
    /// can make us buffer. Go's `recvLoop` stops reading the transport while
    /// its bucket is non-positive; not implementing that meant
    /// `max_receive_buffer` was pure bookkeeping and memory grew without
    /// limit whenever the application read slower than the peer wrote.
    #[inline]
    pub fn has_receive_capacity(&self) -> bool {
        self.token_bucket.load(Ordering::Relaxed) > 0
    }

    /// Charge the receive window for a frame that was buffered for a stream.
    #[inline]
    fn consume_tokens(&self, n: usize) {
        self.token_bucket
            .fetch_sub(n.min(i32::MAX as usize) as i32, Ordering::Relaxed);
    }

    /// Return tokens to the token bucket (bytes consumed by the reader).
    /// This replenishes the flow control window after data has been read.
    pub fn return_tokens(&self, n: usize) {
        self.token_bucket
            .fetch_add(n.min(i32::MAX as usize) as i32, Ordering::Relaxed);
    }

    /// Reclaim the receive window for bytes the application has consumed.
    ///
    /// Go returns tokens from inside `Stream.Read`; a `Stream` here has no
    /// back-reference to its session, so the flush cycle sweeps the streams
    /// instead. Returns the number of bytes reclaimed.
    pub fn reclaim_tokens(&self) -> usize {
        let reclaimed: usize = {
            let streams = self.streams.lock();
            streams.values().map(|s| s.take_return_tokens()).sum()
        };
        if reclaimed > 0 {
            self.return_tokens(reclaimed);
        }
        reclaimed
    }

    /// Queue a SYN frame to be sent by the next prepare_outbound_into() call.
    ///
    /// Used by SmuxConn::open_stream() so that SYN frames are automatically
    /// included in the outbound flush. kcptun sends SYN manually, so it never
    /// calls this — pending_syns stays empty.
    pub fn queue_syn(&self, stream_id: u32) {
        self.pending_syns.lock().push(stream_id);
    }

    /// Pop the next accepted stream ID (for SmuxConn::accept()).
    ///
    /// Returns None when no new streams have been accepted since the last call.
    pub fn pop_accepted_stream(&self) -> Option<u32> {
        self.accepted_streams.lock().pop_front()
    }

    /// Get the accept notification handle (for SmuxConn::accept()).
    pub fn accept_notify(&self) -> &knet::Notify {
        &self.accept_notify
    }

    /// Enable accept queue (SmuxConn server mode).
    /// When enabled, process_data() will push accepted stream IDs and notify.
    pub fn enable_accept(&self) {
        self.accept_enabled.store(true, Ordering::Release);
    }

    /// Open a new stream on this session (client side).
    ///
    /// Returns the new stream.
    pub fn open_stream(&self) -> Result<Arc<Stream>, SessionError> {
        if self.is_closed() {
            return Err(SessionError::SessionClosed);
        }

        // Go only refuses to open once the id space itself is exhausted
        // (`ErrGoAway` when `nextStreamID` wraps). Capping at 65536 turned
        // the 32768th stream *ever opened* on a session into a hard failure,
        // which the client handles by closing the whole KCP session and
        // every stream still running on it — reachable in hours on a busy
        // short-connection proxy.
        let id = self.next_stream_id.fetch_add(2, Ordering::SeqCst);
        if id > self.max_streams {
            return Err(SessionError::TooManyStreams);
        }

        let stream = Arc::new(Stream::with_buffer(id, self.config.max_stream_buffer));
        stream.set_self_ref(Arc::downgrade(&stream));
        stream.set_state(StreamState::Ready);
        stream.mark_opened();
        // SMUX v1 has no UPD / per-stream send window.
        if self.config.version == 1 {
            stream.disable_peer_window();
        }

        self.streams.lock().insert(id, stream.clone());
        Ok(stream)
    }

    /// Accept the next incoming stream (server side).
    ///
    /// Returns the accepted stream, or `None` if a stream with this id
    /// already exists — a duplicated or replayed SYN must not evict a live
    /// stream (Go ignores the frame in that case). The old `Arc<Stream>`
    /// stayed in the application's hands but was no longer in the session
    /// map, so it never received data again and its writes were never
    /// drained: a silently wedged connection until the peer's TCP timeout.
    pub fn accept_stream(&self, id: u32) -> Result<Option<Arc<Stream>>, SessionError> {
        if self.is_closed() {
            return Err(SessionError::SessionClosed);
        }

        let mut streams = self.streams.lock();
        if streams.contains_key(&id) {
            return Ok(None);
        }

        let stream = Arc::new(Stream::with_buffer(id, self.config.max_stream_buffer));
        stream.set_self_ref(Arc::downgrade(&stream));
        stream.set_state(StreamState::Ready);
        stream.mark_opened();
        // SMUX v1 has no UPD / per-stream send window.
        if self.config.version == 1 {
            stream.disable_peer_window();
        }

        streams.insert(id, stream.clone());
        Ok(Some(stream))
    }

    /// Process incoming data from the transport.
    ///
    /// This should be called whenever new data arrives on the underlying
    /// connection.
    pub fn process_data(&self, data: &[u8]) -> Result<Vec<(u32, bytes::Bytes)>, SessionError> {
        if self.is_closed() {
            return Err(SessionError::SessionClosed);
        }

        let mut codec = self.codec.lock();
        codec.feed(data);

        let mut results = Vec::new();

        while let Some(frame) = codec.decode() {
            // Any received frame confirms peer is alive.
            self.update_activity();
            if frame.ver != self.config.version {
                // Go rejects a version mismatch with ErrInvalidProtocol
                // rather than half-processing the frame.
                log::warn!(
                    "SMUX: frame version {} != session version {}, closing session",
                    frame.ver,
                    self.config.version
                );
                drop(codec);
                self.close();
                return Err(SessionError::InvalidFrame(format!(
                    "frame version {} != session version {}",
                    frame.ver, self.config.version
                )));
            }
            match frame.cmd {
                Cmd::Syn => {
                    // Incoming stream request (Go cmdSYN = 0)
                    debug!("SMUX: received SYN for stream {}", frame.stream_id);
                    if self.accept_stream(frame.stream_id)?.is_some()
                        && self.accept_enabled.load(Ordering::Acquire)
                    {
                        self.accepted_streams.lock().push_back(frame.stream_id);
                        self.accept_notify.notify_one();
                    }
                }
                Cmd::Fin => {
                    // Stream closed by remote (Go cmdFIN = 1) — may carry last data
                    debug!("SMUX: received FIN for stream {}", frame.stream_id);
                    if let Some(stream) = self.streams.lock().get(&frame.stream_id) {
                        if !frame.data.is_empty() {
                            self.consume_tokens(frame.data.len());
                            if let Err(e) = stream.push_data_bytes(frame.data.clone()) {
                                log::warn!(
                                    "push_data overflow FIN stream {}: {:?}",
                                    frame.stream_id,
                                    e
                                );
                            }
                        }
                        stream.mark_remote_closed();
                        stream.set_state(StreamState::FinReceived);
                    }
                }
                Cmd::Psh => {
                    // Data push (Go cmdPSH = 2)
                    if let Some(stream) = self.streams.lock().get(&frame.stream_id) {
                        // Use zero-copy push_data_bytes: the frame.data is a
                        // reference-counted Bytes slice from the codec buffer.
                        self.consume_tokens(frame.data.len());
                        if let Err(e) = stream.push_data_bytes(frame.data.clone()) {
                            log::warn!(
                                "push_data overflow DATA stream {}: {:?}",
                                frame.stream_id,
                                e
                            );
                        }
                        results.push((frame.stream_id, frame.data));
                    }
                }
                Cmd::Nop => {
                    // No operation / keepalive (Go cmdNOP = 3)
                    // Go smux sends NOP frames as keepalive probes.
                    // Nothing to do on receive — the frame itself confirms
                    // the connection is alive.
                }
                Cmd::Upd => {
                    // Window update (Go cmdUPD = 4, v2 only)
                    // Format: [consumed 4B LE][window 4B LE]
                    if frame.data.len() >= 8 {
                        let consumed =
                            u32::from_le_bytes(frame.data[0..4].try_into().unwrap_or([0; 4]));
                        let window =
                            u32::from_le_bytes(frame.data[4..8].try_into().unwrap_or([0; 4]));
                        // Apply per-stream peer window (write-side flow control).
                        {
                            let streams = self.streams.lock();
                            if let Some(stream) = streams.get(&frame.stream_id) {
                                stream.apply_peer_update(consumed, window);
                            }
                        }
                        // NOTE: a UPD advertises the *peer's* receive window;
                        // it says nothing about ours. Adding it to the local
                        // token bucket (as this used to) inflated our own
                        // receive window by 256 KiB on every window update.
                        debug!(
                            "SMUX: UPD stream {} consumed={} window={}",
                            frame.stream_id, consumed, window
                        );
                    }
                }
            }
        }

        Ok(results)
    }

    /// Emit pending UPD frames directly into the outbound buffer.
    ///
    /// Scans all streams for pending UPD flags and encodes each into `buf`
    /// immediately. Because `take_upd()` (which atomically clears the flag)
    /// and the frame encoding happen under the same `streams` lock, there is
    /// no window where a flag is cleared but the frame is lost — the old
    /// channel path could drop a frame on a full channel and relied on
    /// `rearm_upd()` to recover, which still left a race window.
    ///
    /// No-op for `version = 1`, which has no UPD frame.
    ///
    /// Callers that already hold the `streams` lock should use
    /// [`emit_upd_frames_locked`](Self::emit_upd_frames_locked) instead
    /// to avoid re-entrant locking.
    pub fn emit_upd_frames(&self, buf: &mut BytesMut, ver: u8) {
        if self.config.version < 2 {
            return;
        }
        let streams = self.streams.lock();
        self.emit_upd_frames_locked(&streams, buf, ver);
    }

    /// Same as [`emit_upd_frames`](Self::emit_upd_frames) but assumes the
    /// caller already holds the `streams` lock. Used by
    /// [`prepare_outbound_into_controlled`](Self::prepare_outbound_into_controlled)
    /// to merge all three passes (reclaim tokens, UPD emit, PSH drain) into
    /// a single `streams` lock acquisition.
    fn emit_upd_frames_locked(
        &self,
        streams: &parking_lot::MutexGuard<'_, HashMap<u32, Arc<Stream>>>,
        buf: &mut BytesMut,
        ver: u8,
    ) {
        if self.config.version < 2 {
            return;
        }
        for (&stream_id, stream) in streams.iter() {
            if let Some((consumed, window)) = stream.take_upd() {
                Frame::encode_header_into(buf, ver, Cmd::Upd, stream_id, 8);
                buf.extend_from_slice(&consumed.to_le_bytes());
                buf.extend_from_slice(&window.to_le_bytes());
                debug!(
                    "SMUX: emitted UPD frame stream={} consumed={} window={}",
                    stream_id, consumed, window
                );
            }
        }
    }

    /// Close the session and all streams.
    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.accept_notify.notify_waiters();
        let mut streams = self.streams.lock();
        for (_, stream) in streams.drain() {
            stream.close();
        }
    }

    /// Remove one stream from the session map and fully close it.
    ///
    /// Returns `true` if the id was present. Used for SYN-send failures and
    /// force-reap paths so map entries cannot leak.
    pub fn remove_stream(&self, id: u32) -> bool {
        let mut streams = self.streams.lock();
        if let Some(stream) = streams.remove(&id) {
            // Release this stream's share of the receive window before its
            // buffers go away (Go's `recycleTokens`); otherwise every stream
            // dropped with unread data shrinks the session window for good.
            let recycled = stream.recycle_tokens();
            stream.close();
            drop(streams);
            self.return_tokens(recycled);
            true
        } else {
            false
        }
    }

    /// Reap streams that are fully closed, or local-closed past `linger` without
    /// a peer FIN (zombie half-open streams under proxy short-connect load).
    ///
    /// Returns stream ids that still need a wire FIN before/while being removed
    /// (`!fin_sent`). Callers should encode FIN for those ids, then treat them as
    /// gone from the map (this method already `remove`s + `close`s).
    ///
    /// Fully closed (`local && remote && fin_sent`) streams are removed with an
    /// empty contribution to the returned vec.
    pub fn reap_stale_streams(&self, linger: Duration) -> Vec<u32> {
        let mut streams = self.streams.lock();
        let mut need_fin = Vec::new();
        let mut to_remove = Vec::new();

        for (&id, s) in streams.iter() {
            let local = s.is_local_closed();
            let remote = s.is_remote_closed();
            let fin = s.is_fin_sent();

            if local && remote && fin {
                to_remove.push((id, false));
                continue;
            }

            if local {
                if let Some(elapsed) = s.local_closed_elapsed() {
                    if elapsed >= linger {
                        // Timed out waiting for peer FIN — force remove.
                        to_remove.push((id, !fin));
                    }
                }
            }
        }

        let mut recycled = 0usize;
        for (id, wants_fin) in to_remove {
            if let Some(stream) = streams.remove(&id) {
                if wants_fin {
                    need_fin.push(id);
                }
                recycled += stream.recycle_tokens();
                stream.close();
            }
        }
        drop(streams);
        // Reaped streams give their receive-window share back (Go's
        // `recycleTokens`).
        self.return_tokens(recycled);

        need_fin
    }

    /// Get the number of active streams.
    #[inline]
    pub fn stream_count(&self) -> usize {
        self.streams.lock().len()
    }

    /// Perform keepalive check — returns true if a ping should be sent.
    ///
    /// `keepalive_interval == 0` means keepalives are disabled (`Config::verify`
    /// allows `interval == 0` together with `timeout == 0`); without this guard
    /// the elapsed-time comparison is always true and every idle write-loop
    /// wake emits a NOP — with kcptun-common's 10 ms idle cadence both peers
    /// then ping-pong NOPs continuously (observed via frame tracing).
    pub fn check_keepalive(&self) -> bool {
        if self.config.keepalive_interval == 0 {
            return false;
        }
        let last = self.last_keepalive_ms.load(Ordering::Relaxed);
        let elapsed_ms = knet::mono_ms().saturating_sub(last);
        elapsed_ms >= self.keepalive_interval.as_millis() as u64
    }

    /// Update last inbound activity timestamp.
    pub fn update_activity(&self) {
        self.last_activity_ms
            .store(knet::mono_ms(), Ordering::Relaxed);
    }

    /// Mark that a keepalive NOP was just sent (resets the interval).
    pub fn mark_keepalive_sent(&self) {
        self.last_keepalive_ms
            .store(knet::mono_ms(), Ordering::Relaxed);
    }

    /// Returns true if no inbound activity within keepalive_timeout.
    pub fn is_keepalive_timeout(&self) -> bool {
        if self.config.keepalive_timeout == 0 {
            return false;
        }
        let last = self.last_activity_ms.load(Ordering::Relaxed);
        let elapsed_ms = knet::mono_ms().saturating_sub(last);
        elapsed_ms >= self.config.keepalive_timeout.saturating_mul(1000)
    }

    /// Build a NOP keepalive frame (empty payload, stream id 0).
    pub fn keepalive_frame(&self) -> Frame {
        Frame::new(Cmd::Nop, 0, Bytes::new()).with_ver(self.config.version)
    }

    /// Prepare outbound SMUX frames by draining streams, encoding FINs for
    /// eligible closed streams, and appending any pending UPD frames.
    ///
    /// This is the unified outbound path for both client and server schedulers.
    /// It appends directly into the caller's `buf` (zero-copy from each stream's
    /// send buffer — one `extend_from_slice` per chunk on the flush path).
    ///
    /// - `max_bytes`: soft cap on total *payload* bytes to drain across streams.
    /// - `ver`: SMUX version (1 or 2) used for frame headers.
    ///
    /// Returns the stream IDs for which a FIN frame was encoded. The caller
    /// **must** call `mark_fin_sent(id)` on each of these **only after** the
    /// corresponding data has been successfully accepted by the transport
    /// (e.g., after `kcp.send` of the whole batch succeeds). This preserves the
    /// "can't lose FIN" invariant.
    ///
    /// The low-level `drain_send_max` / `emit_upd_frames` remain
    /// available for advanced integration; this method is the recommended
    /// single entry point for normal high-performance flush loops.
    pub fn prepare_outbound_into(&self, buf: &mut BytesMut, max_bytes: usize, ver: u8) -> Vec<u32> {
        self.prepare_outbound_into_controlled(buf, max_bytes, ver, true)
    }

    /// Prepare outbound frames, optionally deferring FIN emission.
    ///
    /// KCP-backed servers set `allow_fin=false` while previously drained KCP
    /// data is still unacknowledged, preventing FIN from overtaking echo tail
    /// data at Go clients.
    pub fn prepare_outbound_into_controlled(
        &self,
        buf: &mut BytesMut,
        max_bytes: usize,
        ver: u8,
        allow_fin: bool,
    ) -> Vec<u32> {
        let mut fin_streams = Vec::new();
        let mut drained_total = 0usize;

        // Drain pending SYN frames first (queued by SmuxConn::open_stream).
        // kcptun never queues SYNs, so this is a no-op for kcptun.
        {
            let mut syns = self.pending_syns.lock();
            if !syns.is_empty() {
                for id in syns.drain(..) {
                    Frame::encode_header_into(buf, ver, Cmd::Syn, id, 0);
                }
            }
        }

        // ── Single `streams` lock for all three passes ──
        //
        // The old code acquired `streams` three times per flush cycle:
        //   1. `reclaim_tokens()` — lock, scan, unlock
        //   2. `emit_upd_frames()` — lock, scan, unlock
        //   3. PSH drain + FIN collect — lock, scan (twice), unlock
        //
        // Each lock/unlock is a `parking_lot::Mutex` (futex on Linux,
        // lllval/ulock on macOS). Under high stream count (64+), the scan
        // itself dominates; but the three lock acquisitions add up:
        //   - 3× atomic CAS fences
        //   - 3× HashMap bucket iteration (cold cache lines)
        //   - 2× `pending_syns` nested lock inside the PSH/FIN loops
        //
        // Merged into one lock pass: reclaim tokens → emit UPD → drain PSH
        // → collect FIN, all without releasing the lock between them.
        {
            let streams = self.streams.lock();

            // Per-stream share of this cycle's budget.
            let share = if streams.is_empty() {
                max_bytes
            } else {
                (max_bytes / streams.len()).max(self.config.max_frame_size)
            };

            // Drain pending SYN frames queued during the stream iteration
            // (by open_stream). This replaces the per-stream `pending_syns`
            // lock checks that used to run inside the PSH/FIN loops.
            // We snapshot the SYN set once and check membership with a simple
            // `contains` — no nested lock per stream.
            let syn_snapshot: std::collections::HashSet<u32> = {
                let syns = self.pending_syns.lock();
                syns.iter().copied().collect()
            };
            if !syn_snapshot.is_empty() {
                let mut syns = self.pending_syns.lock();
                syns.clear();
                for id in &syn_snapshot {
                    Frame::encode_header_into(buf, ver, Cmd::Syn, *id, 0);
                }
            }
            // Stream IDs that had a SYN queued are now consumed; the PSH
            // loop below does not need to re-check `pending_syns` per stream.

            // Pass 1: reclaim tokens (give receive window back to session).
            let mut reclaimed: usize = 0;
            for s in streams.values() {
                reclaimed += s.take_return_tokens();
            }
            if reclaimed > 0 {
                self.return_tokens(reclaimed);
            }

            // Pass 2: emit UPD frames (v2 only, directly into buf).
            self.emit_upd_frames_locked(&streams, buf, ver);

            // Pass 3: drain PSH data from streams, respecting per-stream
            // peer window and the overall max_bytes cap.
            'outer: for (&id, s) in streams.iter() {
                let mut drained_this_stream = 0usize;
                loop {
                    if drained_total >= max_bytes {
                        break 'outer;
                    }
                    if drained_this_stream >= share {
                        break;
                    }
                    let header_pos = buf.len();
                    Frame::encode_header_into(buf, ver, Cmd::Psh, id, 0);
                    let n = s.drain_send_max(buf, self.config.max_frame_size);
                    if n == 0 {
                        buf.truncate(header_pos);
                        break;
                    }
                    Frame::patch_header_length(buf, header_pos, n as u16);
                    drained_total += n;
                    drained_this_stream += n;
                }
            }

            // Pass 4: collect FIN candidates in the same lock scope.
            if allow_fin {
                for (&id, s) in streams.iter() {
                    if s.is_local_closed() && s.pending_send() == 0 && !s.is_fin_sent() {
                        debug!("SMUX: prepare_outbound encoding FIN for stream {}", id);
                        Frame::encode_header_into(buf, ver, Cmd::Fin, id, 0);
                        fin_streams.push(id);
                    }
                }
            }
        }

        fin_streams
    }

    /// Mark the given stream IDs as having had their FIN frame sent.
    ///
    /// Call this **after** the transport has accepted the bytes containing
    /// the corresponding FIN frames (e.g., after a successful `kcp.send` of
    /// the batch that included them). This is required to preserve the
    /// "can't lose FIN" rule and to allow proper linger/reap behavior.
    ///
    /// Unknown IDs are ignored.
    pub fn mark_fins_sent(&self, ids: &[u32]) {
        if ids.is_empty() {
            return;
        }
        let streams = self.streams.lock();
        for &id in ids {
            if let Some(s) = streams.get(&id) {
                s.mark_fin_sent();
            }
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::Frame;
    use std::time::Instant;

    #[test]
    fn session_create_client() {
        let session = Session::new_client(&DEFAULT_CONFIG).unwrap();
        assert!(!session.is_closed());
        assert_eq!(session.stream_count(), 0);
    }

    #[test]
    fn session_create_server() {
        let session = Session::new_server(&DEFAULT_CONFIG).unwrap();
        assert!(!session.is_closed());
    }

    #[test]
    fn session_open_stream() {
        let session = Session::new_client(&DEFAULT_CONFIG).unwrap();
        let stream = session.open_stream().unwrap();
        assert_eq!(stream.id(), 1);
        assert_eq!(session.stream_count(), 1);
    }

    #[test]
    fn remove_stream_drops_map_entry_and_closes() {
        let session = Session::new_client(&DEFAULT_CONFIG).unwrap();
        let stream = session.open_stream().unwrap();
        let id = stream.id();
        assert!(session.remove_stream(id));
        assert_eq!(session.stream_count(), 0);
        assert!(stream.is_closed());
        assert!(!session.remove_stream(id));
    }

    #[test]
    fn reap_stale_streams_removes_fully_closed() {
        let session = Session::new_client(&DEFAULT_CONFIG).unwrap();
        let s = session.open_stream().unwrap();
        s.mark_local_closed();
        s.mark_remote_closed();
        s.mark_fin_sent();
        let need_fin = session.reap_stale_streams(Duration::from_secs(30));
        assert!(need_fin.is_empty());
        assert_eq!(session.stream_count(), 0);
    }

    #[test]
    fn reap_stale_streams_removes_local_closed_past_linger() {
        let session = Session::new_client(&DEFAULT_CONFIG).unwrap();
        let s = session.open_stream().unwrap();
        let id = s.id();
        // Local closed long ago, peer never FINed → zombie that must be reaped.
        s.force_local_closed_at(Instant::now() - Duration::from_secs(120));
        assert!(!s.is_remote_closed());
        let need_fin = session.reap_stale_streams(Duration::from_secs(30));
        assert_eq!(need_fin, vec![id], "stale stream still needs wire FIN");
        assert_eq!(session.stream_count(), 0);
    }

    #[test]
    fn reap_stale_streams_keeps_fresh_local_closed() {
        let session = Session::new_client(&DEFAULT_CONFIG).unwrap();
        let s = session.open_stream().unwrap();
        s.mark_local_closed();
        // Just closed — within linger, wait for remote FIN.
        let need_fin = session.reap_stale_streams(Duration::from_secs(30));
        assert!(need_fin.is_empty());
        assert_eq!(session.stream_count(), 1);
    }

    #[test]
    fn session_open_multiple_streams() {
        let session = Session::new_client(&DEFAULT_CONFIG).unwrap();
        let s1 = session.open_stream().unwrap();
        let s2 = session.open_stream().unwrap();
        assert_eq!(s1.id(), 1);
        assert_eq!(s2.id(), 3); // Client uses odd IDs, incrementing by 2
        assert_eq!(session.stream_count(), 2);
    }

    #[test]
    fn session_server_stream_ids() {
        let session = Session::new_server(&DEFAULT_CONFIG).unwrap();
        let s1 = session.accept_stream(0).unwrap().unwrap();
        let s2 = session.accept_stream(2).unwrap().unwrap();
        assert_eq!(s1.id(), 0);
        assert_eq!(s2.id(), 2);
    }

    #[test]
    fn session_close() {
        let session = Session::new_client(&DEFAULT_CONFIG).unwrap();
        session.open_stream().unwrap();
        session.close();
        assert!(session.is_closed());
        assert_eq!(session.stream_count(), 0);
    }

    #[test]
    fn session_process_data() {
        let session = Session::new_client(&DEFAULT_CONFIG).unwrap();
        // Create a data frame for stream 1
        let frame =
            Frame::new(Cmd::Psh, 1, bytes::Bytes::from("test data")).with_ver(session.version());
        let mut buf = Vec::new();
        frame.encode(&mut buf);

        // Process should succeed but stream 1 doesn't exist yet,
        // so data will be silently dropped
        let results = session.process_data(&buf).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn session_accept_stream() {
        let session = Session::new_server(&DEFAULT_CONFIG).unwrap();
        let stream = session.accept_stream(0).unwrap().unwrap();
        assert!(stream.is_ready());
        // A duplicate SYN must not evict the live stream.
        assert!(session.accept_stream(0).unwrap().is_none());
        assert!(std::sync::Arc::ptr_eq(
            &stream,
            session.streams().lock().get(&0).unwrap()
        ));
    }

    #[test]
    fn session_cannot_open_after_close() {
        let session = Session::new_client(&DEFAULT_CONFIG).unwrap();
        session.close();
        let result = session.open_stream();
        assert!(result.is_err());
    }

    #[test]
    fn config_validation() {
        let mut cfg = DEFAULT_CONFIG.clone();
        assert!(cfg.verify().is_ok());
        cfg.version = 3;
        assert!(cfg.verify().is_err());
        cfg.version = 2;
        cfg.max_receive_buffer = 0;
        assert!(cfg.verify().is_err());

        cfg.max_receive_buffer = DEFAULT_CONFIG.max_receive_buffer;
        cfg.max_frame_size = u16::MAX as usize + 1;
        assert!(cfg.verify().is_err());

        cfg.max_frame_size = DEFAULT_CONFIG.max_frame_size;
        cfg.max_stream_buffer = cfg.max_receive_buffer + 1;
        assert!(cfg.verify().is_err());

        cfg.max_stream_buffer = DEFAULT_CONFIG.max_stream_buffer;
        cfg.keepalive_interval = 31;
        cfg.keepalive_timeout = 30;
        assert!(cfg.verify().is_err());
    }

    #[test]
    fn session_keepalive() {
        let session = Session::new_client(&DEFAULT_CONFIG).unwrap();
        // Initially, keepalive should not be needed yet
        assert!(!session.check_keepalive());
    }

    #[test]
    fn session_return_tokens() {
        let session = Session::new_client(&DEFAULT_CONFIG).unwrap();
        let initial = session.token_bucket_value();
        session.return_tokens(1024);
        assert_eq!(session.token_bucket_value(), initial + 1024);
    }

    #[test]
    fn session_emit_upd_frames() {
        // v2 config: UPD frames are emitted directly into the outbound buffer.
        let v2_config = Config {
            version: 2,
            ..DEFAULT_CONFIG.clone()
        };
        let session = Session::new_client(&v2_config).unwrap();

        // No streams → no UPD frames.
        let mut buf = BytesMut::new();
        session.emit_upd_frames(&mut buf, 2);
        assert!(buf.is_empty());

        // Open a stream and simulate a pending UPD by reading data.
        let s = session.open_stream().unwrap();
        let id = s.id();
        // Push some data into the stream's receive buffer to trigger a UPD.
        s.push_data_bytes(Bytes::from_static(b"hello world"))
            .unwrap();
        // Simulate a read to arm the UPD flag.
        let mut read_buf = [0u8; 11];
        let _ = s.read(&mut read_buf);

        let mut buf = BytesMut::new();
        session.emit_upd_frames(&mut buf, 2);
        assert!(!buf.is_empty(), "UPD frame should have been emitted");

        // Decode and verify.
        let mut codec = FrameCodec::new(v2_config.max_receive_buffer);
        codec.feed(&buf);
        let frame = codec.decode().expect("a frame");
        assert_eq!(frame.cmd, Cmd::Upd);
        assert_eq!(frame.stream_id, id);
        assert_eq!(frame.data.len(), 8);

        // Second call should produce nothing — the flag was consumed.
        let mut buf2 = BytesMut::new();
        session.emit_upd_frames(&mut buf2, 2);
        assert!(buf2.is_empty(), "UPD flag should have been cleared");
    }

    #[test]
    fn session_emit_upd_frames_v1_noop() {
        // v1 sessions never emit UPD frames.
        let session = Session::new_client(&DEFAULT_CONFIG).unwrap();
        let s = session.open_stream().unwrap();
        s.push_data_bytes(Bytes::from_static(b"data")).unwrap();
        let mut read_buf = [0u8; 4];
        let _ = s.read(&mut read_buf);

        let mut buf = BytesMut::new();
        session.emit_upd_frames(&mut buf, 1);
        assert!(buf.is_empty(), "v1 must not emit UPD frames");
    }

    #[test]
    fn session_prepare_outbound_basic_psh_and_fin() {
        let session = Session::new_client(&DEFAULT_CONFIG).unwrap();
        let s = session.open_stream().unwrap();
        let id = s.id();

        // Write some data; it should be drained into a PSH frame.
        s.write_bytes(Bytes::from_static(b"hello")).unwrap();
        let mut buf = BytesMut::new();
        let fin_ids = session.prepare_outbound_into(&mut buf, 64 * 1024, 2);
        assert!(fin_ids.is_empty(), "no FIN yet");
        assert!(!buf.is_empty(), "should have produced frame bytes");

        // Decode the frame(s) we produced and ensure we see a PSH for our stream.
        let mut codec = FrameCodec::new(DEFAULT_CONFIG.max_receive_buffer);
        codec.feed(&buf);
        let mut saw_psh = false;
        while let Some(f) = codec.decode() {
            if f.cmd == Cmd::Psh && f.stream_id == id {
                saw_psh = true;
                assert_eq!(&f.data[..], b"hello");
            }
        }
        assert!(saw_psh, "expected a PSH frame for our stream");

        // Now mark the stream locally closed with no pending send.
        s.mark_local_closed();
        // Drain any residual (should be none) and request FIN.
        let mut buf2 = BytesMut::new();
        let fin_ids2 = session.prepare_outbound_into(&mut buf2, 64 * 1024, 2);
        assert_eq!(
            fin_ids2,
            vec![id],
            "should have encoded FIN for this stream"
        );
        assert!(!s.is_fin_sent(), "FIN not yet sent until mark_fins_sent");

        // Simulate transport acceptance: mark FINs sent.
        session.mark_fins_sent(&fin_ids2);
        assert!(s.is_fin_sent(), "FIN should now be marked as sent");
    }

    #[test]
    /// Regression: a stream created via `open_stream` + `queue_syn` can hold
    /// queued data BEFORE the session's next `prepare_outbound` runs. The SYN
    /// and the data must reach the wire in SYN-first order — a PSH that
    /// precedes its SYN is dropped by the peer (unknown stream), which leaves
    /// an accepted-but-empty stream and deadlocks the writer (observed as
    /// `fresh_stream_echoes_*` hangs on macOS).
    #[test]
    fn session_prepare_outbound_orders_syn_before_psh_of_same_stream() {
        let cfg = Config {
            version: 1,
            keepalive_interval: 0,
            keepalive_timeout: 0,
            ..DEFAULT_CONFIG.clone()
        };
        let session = Session::new_client(&cfg).unwrap();
        let stream = session.open_stream().unwrap();
        let sid = stream.id();
        session.queue_syn(sid);
        // Data queued while the SYN is still in pending_syns.
        stream.write(&[0xAA]).unwrap();

        let mut out = BytesMut::new();
        session.prepare_outbound_into_controlled(&mut out, 64 * 1024, 1, false);
        assert!(!out.is_empty());

        // Walk frames: SYN(sid) must appear before PSH(sid).
        let b = &out[..];
        let mut i = 0usize;
        let mut syn_pos = None;
        let mut psh_pos = None;
        while i + 8 <= b.len() {
            let cmd = b[i + 1];
            let len = u16::from_le_bytes([b[i + 2], b[i + 3]]) as usize;
            let fsid = u32::from_le_bytes(b[i + 4..i + 8].try_into().unwrap());
            if fsid == sid {
                if cmd == 0 && syn_pos.is_none() {
                    syn_pos = Some(i);
                }
                if cmd == 2 && psh_pos.is_none() {
                    psh_pos = Some(i);
                }
            }
            i += 8 + len;
        }
        let syn_pos = syn_pos.expect("SYN for the stream must be emitted");
        if let Some(p) = psh_pos {
            assert!(syn_pos < p, "SYN must precede PSH on the wire");
        }
        // The SYN must not linger in pending_syns after this prepare.
        assert!(session.pending_syns.lock().is_empty());
    }

    fn session_prepare_outbound_respects_max_bytes_and_peer_window() {
        let session = Session::new_client(&DEFAULT_CONFIG).unwrap();
        let s = session.open_stream().unwrap();

        // Large write, but cap drain to a small amount.
        let big = vec![b'x'; 100 * 1024];
        s.write_bytes(Bytes::from(big.clone())).unwrap();

        let mut buf = BytesMut::new();
        // Very small cap to force partial drain.
        let _ = session.prepare_outbound_into(&mut buf, 1024, 2);

        // We should have produced some bytes, but not the entire payload.
        // The peer window starts at 256KiB (initialPeerWindow), so max_bytes is the limiter.
        assert!(!buf.is_empty());
        assert!(buf.len() < big.len(), "should be capped by max_bytes");
        // Stream should still have pending data.
        assert!(s.pending_send() > 0);
    }

    #[test]
    fn session_prepare_outbound_respects_configured_frame_size() {
        let cfg = Config {
            max_frame_size: 1024,
            ..DEFAULT_CONFIG.clone()
        };
        let session = Session::new_client(&cfg).unwrap();
        let stream = session.open_stream().unwrap();
        stream.write_bytes(Bytes::from(vec![b'x'; 2500])).unwrap();

        let mut buf = BytesMut::new();
        let _ = session.prepare_outbound_into(&mut buf, 64 * 1024, 2);

        let mut codec = FrameCodec::new(cfg.max_receive_buffer);
        codec.feed(&buf);
        let mut lengths = Vec::new();
        while let Some(frame) = codec.decode() {
            if frame.cmd == Cmd::Psh {
                lengths.push(frame.data.len());
            }
        }
        assert_eq!(lengths, vec![1024, 1024, 452]);
    }

    #[test]
    fn session_prepare_outbound_includes_upd() {
        // UPD is a v2-only frame (Go v1 never emits one).
        let cfg = Config {
            version: 2,
            ..DEFAULT_CONFIG.clone()
        };
        let session = Session::new_client(&cfg).unwrap();
        let s = session.open_stream().unwrap();

        // Push some inbound data and read enough to trigger a pending UPD (v2).
        // read() sets need_upd when bytes_read crosses half max_recv_buf or on first read.
        s.push_data_bytes(Bytes::from_static(b"data")).unwrap();
        let mut tmp = [0u8; 8];
        let _ = s.read(&mut tmp);

        // Prepare outbound should include an UPD frame (cmd=4).
        let mut buf = BytesMut::new();
        let _ = session.prepare_outbound_into(&mut buf, 64 * 1024, 2);

        let mut codec = FrameCodec::new(DEFAULT_CONFIG.max_receive_buffer);
        codec.feed(&buf);
        let mut saw_upd = false;
        while let Some(f) = codec.decode() {
            if f.cmd == Cmd::Upd && f.stream_id == s.id() {
                saw_upd = true;
            }
        }
        assert!(saw_upd, "expected an UPD frame when reader advanced");
    }

    #[test]
    fn receive_window_is_charged_on_push_and_reclaimed_on_read() {
        let session = Session::new_client(&DEFAULT_CONFIG).unwrap();
        let s = session.open_stream().unwrap();
        let initial = session.token_bucket_value();

        let frame = Frame::new(Cmd::Psh, s.id(), Bytes::from_static(b"0123456789"))
            .with_ver(session.version());
        let mut buf = Vec::new();
        frame.encode(&mut buf);
        session.process_data(&buf).unwrap();
        assert_eq!(
            session.token_bucket_value(),
            initial - 10,
            "buffered data must charge the receive window"
        );

        // Nothing is reclaimed until the application actually reads.
        assert_eq!(session.reclaim_tokens(), 0);

        let mut tmp = [0u8; 4];
        assert_eq!(s.read(&mut tmp).unwrap().0, 4);
        assert_eq!(session.reclaim_tokens(), 4);
        assert_eq!(session.token_bucket_value(), initial - 6);
    }

    #[test]
    fn receive_window_is_exhaustible_and_recycled_with_the_stream() {
        let cfg = Config {
            max_receive_buffer: 32,
            max_stream_buffer: 32,
            ..DEFAULT_CONFIG.clone()
        };
        let session = Session::new_client(&cfg).unwrap();
        let s = session.open_stream().unwrap();
        assert!(session.has_receive_capacity());

        let frame = Frame::new(Cmd::Psh, s.id(), Bytes::from_static(&[7u8; 32]))
            .with_ver(session.version());
        let mut buf = Vec::new();
        frame.encode(&mut buf);
        session.process_data(&buf).unwrap();

        // The window is now spent, so a read loop must stop pulling from the
        // transport (Go's recvLoop blocks here). Without this the peer can
        // make us buffer without bound — `max_receive_buffer` was previously
        // never decremented at all.
        assert!(!session.has_receive_capacity());

        // Dropping the stream with its data unread must give the window
        // back, or enough closed streams would stall the session for good.
        assert!(session.remove_stream(s.id()));
        assert_eq!(session.token_bucket_value(), 32);
        assert!(session.has_receive_capacity());
    }

    #[test]
    fn outbound_drain_gives_every_stream_a_share() {
        let cfg = Config {
            version: 2,
            max_frame_size: 64,
            ..DEFAULT_CONFIG.clone()
        };
        let session = Session::new_client(&cfg).unwrap();
        // Two saturated streams; the scheduler used to drain the first one
        // in iteration order until the global cap was gone, leaving the
        // other with zero bytes for as long as the first stayed busy.
        let a = session.open_stream().unwrap();
        let b = session.open_stream().unwrap();
        for s in [&a, &b] {
            s.write(&[1u8; 4096]).unwrap();
        }

        let mut buf = BytesMut::new();
        let _ = session.prepare_outbound_into(&mut buf, 512, 2);

        let mut codec = FrameCodec::new(4096);
        codec.feed(&buf);
        let mut per_stream = std::collections::HashMap::new();
        while let Some(f) = codec.decode() {
            if f.cmd == Cmd::Psh {
                *per_stream.entry(f.stream_id).or_insert(0usize) += f.data.len();
            }
        }
        assert!(
            per_stream.get(&a.id()).copied().unwrap_or(0) > 0,
            "first stream got nothing: {per_stream:?}"
        );
        assert!(
            per_stream.get(&b.id()).copied().unwrap_or(0) > 0,
            "second stream starved: {per_stream:?}"
        );
    }

    #[test]
    fn version_mismatch_closes_the_session() {
        let session = Session::new_client(&DEFAULT_CONFIG).unwrap();
        assert_eq!(session.version(), 1);
        let frame = Frame::new(Cmd::Nop, 0, Bytes::new()).with_ver(2);
        let mut buf = Vec::new();
        frame.encode(&mut buf);
        assert!(session.process_data(&buf).is_err());
        assert!(session.is_closed());
    }
}
