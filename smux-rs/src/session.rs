//! SMUX session — the core multiplexer that manages multiple streams over a
//! single transport connection.
//!
//! A `Session` wraps a transport `io::Read + io::Write` and provides:
//! - Opening and accepting streams
//! - Multiplexing data frames across streams
//! - Keepalive (ping/pong)
//! - Graceful shutdown

use log::debug;
use std::collections::HashMap;
use std::io::{self};
use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::frame::{Cmd, FrameCodec};
use crate::stream::{Stream, StreamState};

const MAX_STREAMS: u32 = 65536;
/// Channel capacity for pending UPD frames.
const UPD_CHANNEL_CAPACITY: usize = 1024;

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
        if self.max_receive_buffer < 1024 {
            return Err(SessionError::InvalidConfig(
                "max receive buffer too small".into(),
            ));
        }
        if self.max_stream_buffer < 1024 {
            return Err(SessionError::InvalidConfig(
                "max stream buffer too small".into(),
            ));
        }
        if self.max_frame_size < 256 {
            return Err(SessionError::InvalidConfig(
                "max frame size too small".into(),
            ));
        }
        Ok(())
    }
}

/// Errors from the SMUX session.
#[derive(Debug)]
pub enum SessionError {
    /// I/O error from the underlying transport.
    Io(io::Error),
    /// Invalid configuration.
    InvalidConfig(String),
    /// Session is closed.
    SessionClosed,
    /// Maximum number of streams reached.
    TooManyStreams,
    /// Stream not found.
    StreamNotFound(u32),
    /// Invalid frame received.
    InvalidFrame(String),
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionError::Io(e) => write!(f, "io error: {}", e),
            SessionError::InvalidConfig(msg) => write!(f, "invalid config: {}", msg),
            SessionError::SessionClosed => write!(f, "session closed"),
            SessionError::TooManyStreams => write!(f, "too many streams"),
            SessionError::StreamNotFound(id) => write!(f, "stream {} not found", id),
            SessionError::InvalidFrame(msg) => write!(f, "invalid frame: {}", msg),
        }
    }
}

impl std::error::Error for SessionError {}

impl From<io::Error> for SessionError {
    fn from(e: io::Error) -> Self {
        SessionError::Io(e)
    }
}

/// A pending UPD frame to be sent to the peer.
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
    closed: Arc<Mutex<bool>>,
    /// All active streams, keyed by stream ID.
    streams: Arc<Mutex<HashMap<u32, Arc<Stream>>>>,
    /// Next stream ID to assign (for client: odd, server: even).
    next_stream_id: AtomicU32,
    /// Frame codec for encoding/decoding frames.
    codec: Arc<Mutex<FrameCodec>>,
    /// Keepalive interval.
    keepalive_interval: Duration,
    /// Time of last keepalive.
    last_keepalive: Arc<Mutex<Instant>>,
    /// Maximum streams allowed.
    max_streams: u32,
    /// Token bucket for receive flow control (bytes remaining).
    token_bucket: AtomicI32,
    /// Channel sender for pending UPD frames to be sent by the flush loop.
    upd_tx: kio::Sender<UpdFrame>,
    /// Channel receiver for pending UPD frames.
    upd_rx: kio::Receiver<UpdFrame>,
}

impl Session {
    /// Returns the configured SMUX protocol version (1 or 2).
    /// Go smux validates: hdr.Version() != config.Version → reject.
    pub fn version(&self) -> u8 {
        self.config.version
    }

    /// Create a new client-side SMUX session.
    ///
    /// A client session initiates stream creation and uses odd-numbered stream IDs.
    pub fn new_client(config: &Config) -> Result<Self, SessionError> {
        config.verify()?;
        let (upd_tx, upd_rx) = kio::bounded(UPD_CHANNEL_CAPACITY);
        Ok(Session {
            config: config.clone(),
            closed: Arc::new(Mutex::new(false)),
            streams: Arc::new(Mutex::new(HashMap::new())),
            next_stream_id: AtomicU32::new(1),
            codec: Arc::new(Mutex::new(FrameCodec::new(config.max_receive_buffer))),
            keepalive_interval: Duration::from_secs(config.keepalive_interval),
            last_keepalive: Arc::new(Mutex::new(Instant::now())),
            max_streams: MAX_STREAMS,
            token_bucket: AtomicI32::new(config.max_receive_buffer as i32),
            upd_tx,
            upd_rx,
        })
    }

    /// Create a new server-side SMUX session.
    ///
    /// A server session accepts stream creation and uses even-numbered stream IDs.
    pub fn new_server(config: &Config) -> Result<Self, SessionError> {
        config.verify()?;
        let (upd_tx, upd_rx) = kio::bounded(UPD_CHANNEL_CAPACITY);
        Ok(Session {
            config: config.clone(),
            closed: Arc::new(Mutex::new(false)),
            streams: Arc::new(Mutex::new(HashMap::new())),
            next_stream_id: AtomicU32::new(0),
            codec: Arc::new(Mutex::new(FrameCodec::new(config.max_receive_buffer))),
            keepalive_interval: Duration::from_secs(config.keepalive_interval),
            last_keepalive: Arc::new(Mutex::new(Instant::now())),
            max_streams: MAX_STREAMS,
            token_bucket: AtomicI32::new(config.max_receive_buffer as i32),
            upd_tx,
            upd_rx,
        })
    }

    /// Check if the session is closed.
    #[inline]
    pub fn is_closed(&self) -> bool {
        *self.closed.lock()
    }

    /// Get a reference to the streams map.
    #[inline]
    pub fn streams(&self) -> Arc<Mutex<HashMap<u32, Arc<Stream>>>> {
        self.streams.clone()
    }

    /// Get a reference to the frame codec.
    #[inline]
    pub fn codec(&self) -> Arc<Mutex<FrameCodec>> {
        self.codec.clone()
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

    /// Return tokens to the token bucket (bytes consumed by the reader).
    /// This replenishes the flow control window after data has been read.
    pub fn return_tokens(&self, n: usize) {
        self.token_bucket.fetch_add(n as i32, Ordering::Relaxed);
    }

    /// Take all pending UPD frames that need to be sent.
    /// The caller should encode and send these via the transport.
    pub fn take_upd_frames(&self) -> Vec<UpdFrame> {
        let mut frames = Vec::new();
        while let Ok(frame) = self.upd_rx.try_recv() {
            frames.push(frame);
        }
        frames
    }

    /// Open a new stream on this session (client side).
    ///
    /// Returns the new stream.
    pub fn open_stream(&self) -> Result<Arc<Stream>, SessionError> {
        if self.is_closed() {
            return Err(SessionError::SessionClosed);
        }

        let id = self.next_stream_id.fetch_add(2, Ordering::SeqCst);
        if id > self.max_streams {
            return Err(SessionError::TooManyStreams);
        }

        let stream = Arc::new(Stream::with_buffer(id, self.config.max_stream_buffer));
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
    /// Returns the accepted stream.
    pub fn accept_stream(&self, id: u32) -> Result<Arc<Stream>, SessionError> {
        if self.is_closed() {
            return Err(SessionError::SessionClosed);
        }

        let stream = Arc::new(Stream::with_buffer(id, self.config.max_stream_buffer));
        stream.set_state(StreamState::Ready);
        stream.mark_opened();
        // SMUX v1 has no UPD / per-stream send window.
        if self.config.version == 1 {
            stream.disable_peer_window();
        }

        self.streams.lock().insert(id, stream.clone());
        Ok(stream)
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
            match frame.cmd {
                Cmd::Syn => {
                    // Incoming stream request (Go cmdSYN = 0)
                    debug!("SMUX: received SYN for stream {}", frame.stream_id);
                    self.accept_stream(frame.stream_id)?;
                }
                Cmd::Fin => {
                    // Stream closed by remote (Go cmdFIN = 1) — may carry last data
                    debug!("SMUX: received FIN for stream {}", frame.stream_id);
                    if let Some(stream) = self.streams.lock().get(&frame.stream_id) {
                        if !frame.data.is_empty() {
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
                        // Session-level token bucket (receive side).
                        self.return_tokens(window as usize);
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

    /// Check streams for pending UPD notifications and enqueue UPD frames.
    ///
    /// Call this periodically — it scans all streams for pending UPD flags
    /// and queues UpdFrame messages on the channel for the flush loop to send.
    pub fn check_upd(&self) {
        let streams = self.streams.lock();
        for (&stream_id, stream) in streams.iter() {
            if let Some((consumed, window)) = stream.take_upd() {
                // Enqueue UPD frame for sending
                let _ = self.upd_tx.try_send(UpdFrame {
                    stream_id,
                    consumed,
                    window,
                });
                debug!(
                    "SMUX: enqueued UPD frame stream={} consumed={} window={}",
                    stream_id, consumed, window
                );
            }
        }
    }

    /// Close the session and all streams.
    pub fn close(&self) {
        *self.closed.lock() = true;
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
            stream.close();
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

        for (id, wants_fin) in to_remove {
            if let Some(stream) = streams.remove(&id) {
                if wants_fin {
                    need_fin.push(id);
                }
                stream.close();
            }
        }

        need_fin
    }

    /// Get the number of active streams.
    #[inline]
    pub fn stream_count(&self) -> usize {
        self.streams.lock().len()
    }

    /// Perform keepalive check — returns true if a ping should be sent.
    pub fn check_keepalive(&self) -> bool {
        let elapsed = self.last_keepalive.lock().elapsed();
        elapsed >= self.keepalive_interval
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::Frame;

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
        let s1 = session.accept_stream(0).unwrap();
        let s2 = session.accept_stream(2).unwrap();
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
        let frame = Frame::new(Cmd::Psh, 1, bytes::Bytes::from("test data"));
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
        let stream = session.accept_stream(0).unwrap();
        assert!(stream.is_ready());
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
    fn session_upd_frame_channel() {
        let session = Session::new_client(&DEFAULT_CONFIG).unwrap();
        // Initially no UPD frames
        assert!(session.take_upd_frames().is_empty());

        // Send a UPD frame through the channel
        session
            .upd_tx
            .try_send(UpdFrame {
                stream_id: 1,
                consumed: 100,
                window: 65536,
            })
            .unwrap();

        let frames = session.take_upd_frames();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].stream_id, 1);
        assert_eq!(frames[0].consumed, 100);
        assert_eq!(frames[0].window, 65536);

        // Should be empty again
        assert!(session.take_upd_frames().is_empty());
    }
}
