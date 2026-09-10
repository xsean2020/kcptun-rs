//! High-level SMUX connection wrapper for standalone usage.
//!
//! `SmuxConn` wraps a [`Session`] with background read/write/keepalive tasks,
//! so users can just `open_stream()` / `accept()` and use the returned
//! [`Stream`] with standard async I/O — no manual flush loop needed.
//!
//! ## Quick start (Builder, aligned with KcpStream)
//!
//! Pass your transport — `SmuxConn` takes ownership and drives it in a
//! background task. Chain config, then `.build().await`.
//!
//! ```ignore
//! use smux_rs::{SmuxConn, Config};
//! use knet::{AsyncReadExt, AsyncWriteExt};
//!
//! // Client
//! let tcp = knet::TcpStream::connect("127.0.0.1:8080").await?;
//! let conn = SmuxConn::connect(tcp)
//!     .version(2)
//!     .keepalive(10)
//!     .build()
//!     .await?;
//!
//! let stream = conn.open_stream()?; // Arc<Stream>, flush notify set
//! // Stream implements AsyncRead + AsyncWrite when you own &mut Stream;
//! // wrap Arc with SmuxIo if needed.
//!
//! // Server
//! let (tcp, _) = listener.accept().await?;
//! let conn = SmuxConn::serve(tcp).version(2).build().await?;
//! loop {
//!     let stream = conn.accept().await?;
//!     knet::spawn_task(async move { /* handle */ });
//! }
//!
//! // Full Config at once
//! let conn = SmuxConn::connect(tcp)
//!     .config(Config { version: 2, ..DEFAULT_CONFIG.clone() })
//!     .build()
//!     .await?;
//! ```
//!
//! ## Compatibility: `client` / `server`
//!
//! [`client`](SmuxConn::client) / [`server`](SmuxConn::server) remain as thin
//! sync wrappers around the same builder path (`connect`/`serve` + config).
//!
//! ## Advanced: manual driver control
//!
//! If you need to manage the driver task yourself (e.g., custom scheduling,
//! or a transport that is not `Send`), use the lower-level constructors:
//!
//! ```ignore
//! let conn = SmuxConn::new(DEFAULT_CONFIG.clone(), true)?; // no transport yet
//! let driver = conn.clone();
//! knet::spawn_task(async move { let _ = driver.run(&mut tcp).await; });
//! ```
//!
//! You can also split the transport and use [`spawn`](Self::spawn) for
//! concurrent read/write (lower latency) when the runtime supports it.

use std::sync::Arc;
use std::time::Duration;

use bytes::BytesMut;

use crate::frame::Frame;
use crate::session::{Config, Session, SessionError, DEFAULT_CONFIG};
use crate::stream::Stream;

/// How often the `run()` loop polls for keepalive / reap when idle (ms).
const RUN_POLL_MS: u64 = 10;
/// Keepalive / reap check interval (in poll cycles, ~1s at 10ms each).
const HEALTH_INTERVAL: u32 = 100;
/// Stale stream linger before forced reap.
const REAP_LINGER: Duration = Duration::from_secs(30);

/// A high-level SMUX connection that manages transport I/O internally.
///
/// Wraps a [`Session`] with background read/write/keepalive tasks. Users call
/// [`open_stream`](Self::open_stream) (client) or
/// [`accept`](Self::accept) (server) to get an [`Arc`]`<`[`Stream`]`>` with
/// flush notify set. Stream implements `AsyncRead`/`AsyncWrite` on `&mut Stream`;
/// wrap with [`crate::SmuxIo`] for the shared `Arc` handle — no manual
/// `process_data` / `prepare_outbound_into` loop required.
///
/// Prefer [`connect`](Self::connect) / [`serve`](Self::serve) builders (aligned
/// with `KcpStream`). Low-level drivers remain available:
///
/// - **[`run`](Self::run)**: single-task, takes `&mut T: AsyncRead + AsyncWrite`.
/// - **[`spawn`](Self::spawn)**: two-task, separate read + write halves.
///
/// Both handle keepalive, timeout, and zombie stream reaping automatically.
#[derive(Clone)]
pub struct SmuxConn {
    session: Arc<Session>,
    flush_notify: Arc<knet::Notify>,
    /// Set to true once a driver task has been started (via run/spawn or
    /// the connect/serve builders / client/server wrappers). Prevents
    /// accidentally starting a second driver on the same connection.
    driven: Arc<std::sync::atomic::AtomicBool>,
}

/// Builder for [`SmuxConn`]. Chain config, then [`.build().await`](Self::build).
///
/// Created via [`SmuxConn::connect`] (client) or [`SmuxConn::serve`] (server).
pub struct SmuxConnBuilder<T> {
    transport: T,
    is_client: bool,
    config: Config,
}

impl SmuxConn {
    /// Client-mode builder over an owned transport (`AsyncRead + AsyncWrite`).
    ///
    /// ```ignore
    /// let conn = SmuxConn::connect(tcp).version(2).keepalive(10).build().await?;
    /// ```
    pub fn connect<T>(transport: T) -> SmuxConnBuilder<T>
    where
        T: knet::AsyncRead + knet::AsyncWrite + Send + Unpin + 'static,
    {
        SmuxConnBuilder {
            transport,
            is_client: true,
            config: DEFAULT_CONFIG.clone(),
        }
    }

    /// Server-mode builder over an owned (accepted) transport.
    ///
    /// Enables the accept queue so [`accept`](Self::accept) works after build.
    ///
    /// ```ignore
    /// let conn = SmuxConn::serve(tcp).version(2).build().await?;
    /// ```
    pub fn serve<T>(transport: T) -> SmuxConnBuilder<T>
    where
        T: knet::AsyncRead + knet::AsyncWrite + Send + Unpin + 'static,
    {
        SmuxConnBuilder {
            transport,
            is_client: false,
            config: DEFAULT_CONFIG.clone(),
        }
    }

    /// Create a new SMUX connection (no transport yet).
    ///
    /// `is_client = true` → client session (odd stream IDs, can open streams).
    /// `is_client = false` → server session (even stream IDs, can accept streams).
    ///
    /// After creating, call [`run`](Self::run) or
    /// [`spawn`](Self::spawn) to drive the connection.
    ///
    /// Prefer [`connect`](Self::connect) / [`serve`](Self::serve) for standalone use.
    pub fn new(config: Config, is_client: bool) -> Result<Self, SessionError> {
        let session = Arc::new(if is_client {
            Session::new_client(&config)?
        } else {
            Session::new_server(&config)?
        });
        // Server mode: enable the accept queue so process_data() notifies us.
        if !is_client {
            session.enable_accept();
        }
        let flush_notify = Arc::new(knet::Notify::new());
        Ok(Self {
            session,
            flush_notify,
            driven: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        })
    }

    /// Compatibility wrapper: client-mode SMUX that owns and drives the transport.
    ///
    /// Equivalent to `SmuxConn::connect(transport).config(config)` + start driver
    /// (sync). Prefer [`connect`](Self::connect)`.build().await` for new code.
    #[deprecated(note = "use SmuxConn::connect(transport).config(config).build().await")]
    pub fn client<T>(config: Config, transport: T) -> Result<Self, SessionError>
    where
        T: knet::AsyncRead + knet::AsyncWrite + Send + Unpin + 'static,
    {
        SmuxConnBuilder {
            transport,
            is_client: true,
            config,
        }
        .build_sync()
    }

    /// Compatibility wrapper: server-mode SMUX that owns and drives the transport.
    ///
    /// Equivalent to `SmuxConn::serve(transport).config(config)` + start driver
    /// (sync). Prefer [`serve`](Self::serve)`.build().await` for new code.
    #[deprecated(note = "use SmuxConn::serve(transport).config(config).build().await")]
    pub fn server<T>(config: Config, transport: T) -> Result<Self, SessionError>
    where
        T: knet::AsyncRead + knet::AsyncWrite + Send + Unpin + 'static,
    {
        SmuxConnBuilder {
            transport,
            is_client: false,
            config,
        }
        .build_sync()
    }

    /// Open a new stream (client side).
    ///
    /// Returns an [`Arc`]`<`[`Stream`]`>` with flush notify attached so async
    /// writes wake the driver. A SYN frame is automatically queued and sent
    /// by the flush loop — no manual frame encoding needed.
    ///
    /// For `AsyncRead`/`AsyncWrite` over the shared handle, wrap with
    /// [`crate::SmuxIo::new`]`(stream, flush)` or use the stream's own
    /// `&mut Stream` impl when you own the value.
    pub fn open_stream(&self) -> Result<Arc<Stream>, SessionError> {
        let stream = self.session.open_stream()?;
        self.session.queue_syn(stream.id());
        stream.set_flush_notify(self.flush_notify.clone());
        Ok(stream)
    }

    /// Accept the next incoming stream (server side).
    ///
    /// Waits until a SYN frame arrives (via `process_data` in the driver
    /// loop) and a new stream is created. Returns an [`Arc`]`<`[`Stream`]`>`
    /// with flush notify attached.
    pub async fn accept(&self) -> Result<Arc<Stream>, SessionError> {
        loop {
            if self.session.is_closed() {
                return Err(SessionError::SessionClosed);
            }
            if let Some(id) = self.session.pop_accepted_stream() {
                let streams = self.session.streams();
                if let Some(stream) = streams.lock().get(&id).cloned() {
                    stream.set_flush_notify(self.flush_notify.clone());
                    return Ok(stream);
                }
                // Stream was already reaped — try again.
                continue;
            }
            self.session.accept_notify().notified().await;
        }
    }

    /// Drive the connection with a single transport (simplest mode).
    ///
    /// Runs a read / flush / keepalive / reap loop. Returns when the
    /// transport closes (EOF or error) or keepalive times out.
    ///
    /// **Spawn this as a background task** so you can use streams concurrently:
    ///
    /// ```ignore
    /// let conn = Arc::new(SmuxConn::new(Config::default(), true)?);
    /// let driver = conn.clone();
    /// knet::spawn_task(async move {
    ///     let _ = driver.run(&mut tcp).await;
    /// });
    /// // Use conn.open_stream() here…
    /// ```
    ///
    /// Uses a 10 ms read timeout for responsiveness — sufficient for
    /// standalone use. For high-throughput scenarios, use
    /// [`spawn`](Self::spawn) with split read/write halves.
    pub async fn run<T>(&self, transport: &mut T) -> Result<(), SessionError>
    where
        T: knet::AsyncRead + knet::AsyncWrite + Unpin,
    {
        use knet::{AsyncReadExt, AsyncWriteExt};

        let mut read_buf = vec![0u8; 65536];
        let mut write_buf = BytesMut::with_capacity(65536);
        let mut health: u32 = 0;

        loop {
            if self.session.is_closed() {
                break;
            }

            // ── Read from transport (with short timeout) ──
            // Skip the read while the receive window is exhausted, as Go's
            // recvLoop does: the peer then hits transport backpressure
            // instead of growing our buffers without bound.
            if self.session.has_receive_capacity() {
                match knet::timeout(
                    Duration::from_millis(RUN_POLL_MS),
                    transport.read(&mut read_buf),
                )
                .await
                {
                    Ok(Ok(0)) => break, // EOF
                    Ok(Ok(n)) => {
                        if let Err(e) = self.session.process_data(&read_buf[..n]) {
                            // Fall through to the shutdown path below so
                            // streams get their close wakeups; returning here
                            // left the session marked open and any
                            // `accept()` waiting forever.
                            log::warn!("SMUX: process_data failed: {e}");
                            break;
                        }
                    }
                    Ok(Err(_)) => break,
                    Err(_) => {} // timeout — fall through to flush
                }
            } else {
                knet::sleep_ms(RUN_POLL_MS).await;
            }

            // ── Flush outbound (SYN + PSH + FIN + UPD) ──
            write_buf.clear();
            let ver = self.session.version();
            let mut fin_ids = self
                .session
                .prepare_outbound_into(&mut write_buf, 65536, ver);

            // Reap zombie streams and append their FINs.
            // Reaped streams are removed from the map by reap_stale_streams.
            // We still encode their FINs here and will mark after successful write
            // for any that prepare also returned (mark is a no-op for already-removed).
            // Collect all ids we encoded in this batch for marking on success.
            if health == 0 {
                let need_fin = self.session.reap_stale_streams(REAP_LINGER);
                for id in &need_fin {
                    Frame::encode_header_into(&mut write_buf, ver, crate::frame::Cmd::Fin, *id, 0);
                }
                // Extend so that on success we attempt to mark everything we put on the wire.
                // For reaped entries, the subsequent mark_fins_sent will no-op (already removed),
                // which is consistent with the reaper contract.
                fin_ids.extend(need_fin);
            }

            if !write_buf.is_empty() {
                if transport.write_all(&write_buf).await.is_err() {
                    break;
                }
                self.session.mark_fins_sent(&fin_ids);
            }

            // ── Keepalive / timeout (throttled) ──
            if health == 0 {
                health = HEALTH_INTERVAL;
                if self.session.is_keepalive_timeout() {
                    self.session.close();
                    break;
                }
                if self.session.check_keepalive() {
                    write_buf.clear();
                    self.session.keepalive_frame().encode(&mut write_buf);
                    let _ = transport.write_all(&write_buf).await;
                    self.session.mark_keepalive_sent();
                }
            }
            health -= 1;
        }

        self.session.close();
        Ok(())
    }

    /// Drive the connection with separate read/write halves (concurrent mode).
    ///
    /// Spawns two background tasks:
    /// - **Read task**: reads from `read` → `session.process_data()`
    /// - **Flush task**: `session.prepare_outbound_into()` → writes to `write`
    ///
    /// More efficient than [`run`](Self::run) because read and write
    /// happen concurrently. The flush task also wakes on
    /// `flush_notify` (set by stream write / `SmuxIo::poll_write`) for near-instant flush.
    ///
    /// Split a `knet::TcpStream` using your runtime's split function:
    ///
    /// ```ignore
    /// // tokio
    /// let (read, write) = tokio::io::split(tcp);
    /// conn.spawn(read, write);
    /// ```
    pub fn spawn<R, W>(&self, mut read: R, mut write: W)
    where
        R: knet::AsyncRead + Send + Unpin + 'static,
        W: knet::AsyncWrite + Send + Unpin + 'static,
    {
        use knet::{AsyncReadExt, AsyncWriteExt};

        // ── Read task ──
        let session = self.session.clone();
        knet::spawn_task(async move {
            let mut buf = vec![0u8; 65536];
            loop {
                if session.is_closed() {
                    break;
                }
                match read.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let _ = session.process_data(&buf[..n]);
                    }
                }
            }
            session.close();
        });

        // ── Flush + keepalive + reap task ──
        let session = self.session.clone();
        let flush_notify = self.flush_notify.clone();
        knet::spawn_task(async move {
            let mut buf = BytesMut::with_capacity(65536);
            let mut nop_buf = BytesMut::with_capacity(8);
            let mut health: u32 = 0;

            loop {
                if session.is_closed() {
                    break;
                }

                // Wait for notify (stream wrote data) or 10 ms timeout.
                let _ = knet::timeout(Duration::from_millis(RUN_POLL_MS), flush_notify.notified())
                    .await;

                buf.clear();
                let ver = session.version();
                let mut fin_ids = session.prepare_outbound_into(&mut buf, 65536, ver);

                // Keepalive / timeout / reap (throttled ~1s)
                if health == 0 {
                    health = HEALTH_INTERVAL;

                    if session.is_keepalive_timeout() {
                        session.close();
                        break;
                    }
                    if session.check_keepalive() {
                        nop_buf.clear();
                        session.keepalive_frame().encode(&mut nop_buf);
                        let _ = write.write_all(&nop_buf).await;
                        session.mark_keepalive_sent();
                    }

                    // Reap zombie streams and append FIN headers for them.
                    // Include their ids in the mark list so that on successful
                    // write we attempt to mark_fins_sent for everything we put
                    // on the wire this batch (consistent with "can't lose FIN").
                    let need_fin = session.reap_stale_streams(REAP_LINGER);
                    for id in &need_fin {
                        Frame::encode_header_into(&mut buf, ver, crate::frame::Cmd::Fin, *id, 0);
                    }
                    fin_ids.extend(need_fin);
                }
                health -= 1;

                if !buf.is_empty() {
                    // Only mark FINs as sent if the transport actually accepted the bytes.
                    // This preserves the "can't lose FIN" invariant.
                    if write.write_all(&buf).await.is_ok() {
                        session.mark_fins_sent(&fin_ids);
                    }
                }
            }
        });
    }

    /// Get the underlying session (for advanced use).
    pub fn session(&self) -> &Session {
        &self.session
    }

    /// Get the flush notify handle (for creating [`SmuxIo`] wrappers).
    pub fn flush_notify(&self) -> Arc<knet::Notify> {
        self.flush_notify.clone()
    }

    /// Close the connection and all streams.
    pub fn close(&self) {
        self.session.close();
    }
}

// ─── Builder ──────────────────────────────────────────────────────────────────

impl<T> SmuxConnBuilder<T>
where
    T: knet::AsyncRead + knet::AsyncWrite + Send + Unpin + 'static,
{
    /// Replace the full SMUX [`Config`].
    pub fn config(mut self, cfg: Config) -> Self {
        self.config = cfg;
        self
    }

    /// SMUX protocol version (`1` or `2`).
    pub fn version(mut self, v: u8) -> Self {
        self.config.version = v;
        self
    }

    /// Keepalive interval in seconds (`Config::keepalive_interval`).
    pub fn keepalive(mut self, secs: u64) -> Self {
        self.config.keepalive_interval = secs;
        self
    }

    /// Keepalive timeout in seconds (`0` = disabled).
    pub fn keepalive_timeout(mut self, secs: u64) -> Self {
        self.config.keepalive_timeout = secs;
        self
    }

    /// Maximum frame payload size (bytes).
    pub fn max_frame_size(mut self, n: usize) -> Self {
        self.config.max_frame_size = n;
        self
    }

    /// Maximum overall receive buffer for the session (bytes).
    pub fn max_receive_buffer(mut self, n: usize) -> Self {
        self.config.max_receive_buffer = n;
        self
    }

    /// Maximum per-stream receive buffer (bytes).
    pub fn max_stream_buffer(mut self, n: usize) -> Self {
        self.config.max_stream_buffer = n;
        self
    }

    /// Construct the connection and start the background driver.
    ///
    /// Uses the single-task [`SmuxConn::run`] driver (no forced split) so any
    /// `AsyncRead + AsyncWrite` transport works. Callers that can split may
    /// still use [`SmuxConn::new`] + [`SmuxConn::spawn`] for concurrent I/O.
    pub async fn build(self) -> Result<SmuxConn, SessionError> {
        self.build_sync()
    }

    /// Sync path used by `build` and the legacy `client`/`server` wrappers.
    fn build_sync(self) -> Result<SmuxConn, SessionError> {
        let SmuxConnBuilder {
            mut transport,
            is_client,
            config,
        } = self;
        let conn = SmuxConn::new(config, is_client)?;
        if conn
            .driven
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_err()
        {
            return Err(SessionError::SessionClosed);
        }
        let driver = conn.clone();
        knet::spawn_task(async move {
            let _ = driver.run(&mut transport).await;
        });
        Ok(conn)
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{Cmd, FrameCodec};
    use crate::session::DEFAULT_CONFIG;
    use bytes::BytesMut;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll};

    /// Verify that SmuxConn is Clone and shares the same session.
    #[test]
    fn smux_conn_clone_shares_session() {
        let conn = SmuxConn::new(DEFAULT_CONFIG.clone(), true).unwrap();
        let conn2 = conn.clone();
        assert!(std::ptr::eq(
            conn.session() as *const Session,
            conn2.session() as *const Session,
        ));
    }

    /// Builder chain sets Config fields without needing a real network.
    #[test]
    fn connect_builder_sets_config_fields() {
        // Dummy transport never used — we only inspect builder.config before build.
        struct Dummy;
        impl knet::AsyncRead for Dummy {
            fn poll_read(
                self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
                _buf: &mut knet::ReadBuf<'_>,
            ) -> Poll<std::io::Result<()>> {
                Poll::Pending
            }
        }
        impl knet::AsyncWrite for Dummy {
            fn poll_write(
                self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
                _buf: &[u8],
            ) -> Poll<std::io::Result<usize>> {
                Poll::Ready(Ok(0))
            }
            fn poll_flush(
                self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
            ) -> Poll<std::io::Result<()>> {
                Poll::Ready(Ok(()))
            }
            fn poll_shutdown(
                self: Pin<&mut Self>,
                _cx: &mut Context<'_>,
            ) -> Poll<std::io::Result<()>> {
                Poll::Ready(Ok(()))
            }
        }

        let b = SmuxConn::connect(Dummy)
            .version(2)
            .keepalive(15)
            .keepalive_timeout(45)
            .max_frame_size(8 * 1024)
            .max_receive_buffer(2 * 1024 * 1024)
            .max_stream_buffer(128 * 1024);
        assert_eq!(b.config.version, 2);
        assert_eq!(b.config.keepalive_interval, 15);
        assert_eq!(b.config.keepalive_timeout, 45);
        assert_eq!(b.config.max_frame_size, 8 * 1024);
        assert_eq!(b.config.max_receive_buffer, 2 * 1024 * 1024);
        assert_eq!(b.config.max_stream_buffer, 128 * 1024);
        assert!(b.is_client);

        let b2 = SmuxConn::serve(Dummy).config(Config {
            version: 2,
            max_receive_buffer: 1024 * 1024,
            max_stream_buffer: 64 * 1024,
            max_frame_size: 4096,
            keepalive_interval: 5,
            keepalive_timeout: 20,
        });
        assert!(!b2.is_client);
        assert_eq!(b2.config.version, 2);
        assert_eq!(b2.config.keepalive_interval, 5);
        assert_eq!(b2.config.max_frame_size, 4096);
    }

    /// open_stream after builder-style new applies session version from config.
    #[test]
    fn new_with_version2_config() {
        let mut cfg = DEFAULT_CONFIG.clone();
        cfg.version = 2;
        let conn = SmuxConn::new(cfg, true).unwrap();
        assert_eq!(conn.session().version(), 2);
        assert_eq!(conn.session().config().version, 2);
    }

    /// Verify that open_stream queues a SYN that prepare_outbound drains.
    #[test]
    fn open_stream_then_prepare_outbound_has_syn() {
        let conn = SmuxConn::new(DEFAULT_CONFIG.clone(), true).unwrap();
        let stream = conn.open_stream().unwrap();
        let id = stream.id();

        let mut buf = BytesMut::new();
        let _ = conn
            .session()
            .prepare_outbound_into(&mut buf, 65536, conn.session().version());

        // Should contain a SYN frame for our stream
        let mut codec = FrameCodec::new(65536);
        codec.feed(&buf);
        let frame = codec.decode().unwrap();
        assert_eq!(frame.cmd, Cmd::Syn);
        assert_eq!(frame.stream_id, id);
    }

    /// Verify that server accept works with process_data.
    #[test]
    fn server_accept_after_syn() {
        let server = SmuxConn::new(DEFAULT_CONFIG.clone(), false).unwrap();

        // Simulate a SYN frame arriving
        let syn = Frame::new(Cmd::Syn, 0, bytes::Bytes::new()).with_ver(server.session().version());
        let mut buf = Vec::new();
        syn.encode(&mut buf);
        server.session().process_data(&buf).unwrap();

        // The stream should be in accepted_streams
        let accepted = server.session().pop_accepted_stream();
        assert_eq!(accepted, Some(0));
        assert!(server.session().pop_accepted_stream().is_none());
    }

    // ─── Mock transport for run()/spawn() FIN marking tests ──────────────────

    struct MockTransport {
        written: Arc<Mutex<BytesMut>>,
        fail_next_write: Arc<AtomicBool>,
    }

    impl knet::AsyncRead for MockTransport {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut knet::ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            // Never deliver data; causes the 10ms timeout in run() to fire and proceed to flush.
            Poll::Pending
        }
    }

    impl knet::AsyncWrite for MockTransport {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            let this = self.get_mut();
            if this.fail_next_write.swap(false, Ordering::SeqCst) {
                return Poll::Ready(Err(std::io::Error::other("mock write failure")));
            }
            let mut w = this.written.lock().unwrap();
            w.extend_from_slice(buf);
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// A write-only half for testing spawn(). Returns Ok(0) on read side is separate.
    struct MockWriteHalf {
        written: Arc<Mutex<BytesMut>>,
        fail_next_write: Arc<AtomicBool>,
    }

    impl knet::AsyncWrite for MockWriteHalf {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            let this = self.get_mut();
            if this.fail_next_write.swap(false, Ordering::SeqCst) {
                return Poll::Ready(Err(std::io::Error::other("mock write failure")));
            }
            let mut w = this.written.lock().unwrap();
            w.extend_from_slice(buf);
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// Reader that blocks forever (never returns) so the read task in spawn()
    /// keeps the session alive while we let the flush task reap and write FINs.
    struct BlockingReadHalf;

    impl knet::AsyncRead for BlockingReadHalf {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut knet::ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            Poll::Pending
        }
    }

    #[cfg(all(test, feature = "tokio"))]
    mod tokio_driver_tests {
        use super::*;

        /// run(): A locally-closed stream that is reaped should cause a FIN to be
        /// encoded and written. After a successful write, we consider FINs "sent".
        #[tokio::test]
        async fn run_reap_encodes_fin_and_marks_only_on_success() {
            let conn = SmuxConn::new(DEFAULT_CONFIG.clone(), true).unwrap();
            let smux_io = conn.open_stream().unwrap();
            let sid = smux_io.id();

            // Operate on the underlying Stream (via the session map) to simulate
            // a stream that has been locally closed long enough to be reaped.
            if let Some(s) = conn.session().streams().lock().get(&sid).cloned() {
                s.mark_local_closed();
                s.force_local_closed_at(
                    std::time::Instant::now() - std::time::Duration::from_secs(120),
                );
            }

            let written = Arc::new(Mutex::new(BytesMut::new()));
            let fail = Arc::new(AtomicBool::new(false));
            let mut transport = MockTransport {
                written: written.clone(),
                fail_next_write: fail.clone(),
            };

            let driver = conn.clone();
            let handle = knet::spawn_task(async move {
                let _ = driver.run(&mut transport).await;
            });

            // Allow the first read-timeout + flush iteration to happen.
            knet::sleep_ms(30).await;

            // Close to unblock the driver loop.
            conn.close();
            // Give the task a moment to exit.
            knet::sleep_ms(20).await;
            drop(handle);

            let data = written.lock().unwrap().clone();
            assert!(!data.is_empty(), "expected flush bytes to be written");

            let mut codec = FrameCodec::new(65536);
            codec.feed(&data);
            let mut saw_fin = false;
            while let Some(f) = codec.decode() {
                if f.cmd == Cmd::Fin && f.stream_id == sid {
                    saw_fin = true;
                }
            }
            assert!(
                saw_fin,
                "expected a FIN frame for the reaped stream id on the wire"
            );

            // The reaper should have removed it from the map.
            assert!(
                conn.session().streams().lock().get(&sid).is_none(),
                "reaped stream should be removed from the session map"
            );
        }

        /// run(): If the transport write fails for the batch containing the reaped FIN,
        /// we must NOT treat the FIN as sent (we break without marking).
        #[tokio::test]
        async fn run_does_not_consider_fin_sent_on_write_failure() {
            let conn = SmuxConn::new(DEFAULT_CONFIG.clone(), true).unwrap();
            let smux_io = conn.open_stream().unwrap();
            let sid = smux_io.id();

            // Operate on the underlying Stream (via the session map) to simulate
            // a stream that has been locally closed long enough to be reaped.
            if let Some(s) = conn.session().streams().lock().get(&sid).cloned() {
                s.mark_local_closed();
                s.force_local_closed_at(
                    std::time::Instant::now() - std::time::Duration::from_secs(120),
                );
            }

            let written = Arc::new(Mutex::new(BytesMut::new()));
            // Fail the very next write (the one that will carry the reaped FIN).
            let fail = Arc::new(AtomicBool::new(true));
            let mut transport = MockTransport {
                written: written.clone(),
                fail_next_write: fail.clone(),
            };

            let driver = conn.clone();
            let _handle = knet::spawn_task(async move {
                let _ = driver.run(&mut transport).await;
            });

            knet::sleep_ms(30).await;
            conn.close();
            knet::sleep_ms(20).await;

            // Because we failed before extending the buffer in the mock, the FIN should not be present.
            let data = written.lock().unwrap().clone();
            let mut codec = FrameCodec::new(65536);
            codec.feed(&data);
            while let Some(f) = codec.decode() {
                assert!(
                    !(f.cmd == Cmd::Fin && f.stream_id == sid),
                    "FIN should not appear in written bytes on write-failure path"
                );
            }

            // Stream was reaped (removed by the reaper before the failing write).
            assert!(conn.session().streams().lock().get(&sid).is_none());
        }

        /// spawn(): Similar to run — FIN from reap must be written, and only "accepted"
        /// (we only mark on success). We use a dedicated write half and an EOF read half.
        #[tokio::test]
        async fn spawn_reap_encodes_fin_and_marks_only_on_success() {
            let conn = SmuxConn::new(DEFAULT_CONFIG.clone(), true).unwrap();
            let smux_io = conn.open_stream().unwrap();
            let sid = smux_io.id();

            // Operate on the underlying Stream (via the session map) to simulate
            // a stream that has been locally closed long enough to be reaped.
            if let Some(s) = conn.session().streams().lock().get(&sid).cloned() {
                s.mark_local_closed();
                s.force_local_closed_at(
                    std::time::Instant::now() - std::time::Duration::from_secs(120),
                );
            }

            let written = Arc::new(Mutex::new(BytesMut::new()));
            let fail = Arc::new(AtomicBool::new(false));

            let write_half = MockWriteHalf {
                written: written.clone(),
                fail_next_write: fail.clone(),
            };
            let read_half = BlockingReadHalf;

            // spawn consumes the halves and drives read/write tasks.
            conn.spawn(read_half, write_half);

            // Let the flush task run at least one iteration.
            knet::sleep_ms(30).await;

            conn.close();
            knet::sleep_ms(20).await;

            let data = written.lock().unwrap().clone();
            assert!(
                !data.is_empty(),
                "expected flush bytes via spawn write half"
            );

            let mut codec = FrameCodec::new(65536);
            codec.feed(&data);
            let mut saw_fin = false;
            while let Some(f) = codec.decode() {
                if f.cmd == Cmd::Fin && f.stream_id == sid {
                    saw_fin = true;
                }
            }
            assert!(
                saw_fin,
                "expected a FIN frame for the reaped stream via spawn"
            );

            assert!(conn.session().streams().lock().get(&sid).is_none());
        }

        /// spawn(): write failure must not cause us to consider the FIN sent.
        #[tokio::test]
        async fn spawn_does_not_consider_fin_sent_on_write_failure() {
            let conn = SmuxConn::new(DEFAULT_CONFIG.clone(), true).unwrap();
            let smux_io = conn.open_stream().unwrap();
            let sid = smux_io.id();

            // Operate on the underlying Stream (via the session map) to simulate
            // a stream that has been locally closed long enough to be reaped.
            if let Some(s) = conn.session().streams().lock().get(&sid).cloned() {
                s.mark_local_closed();
                s.force_local_closed_at(
                    std::time::Instant::now() - std::time::Duration::from_secs(120),
                );
            }

            let written = Arc::new(Mutex::new(BytesMut::new()));
            let fail = Arc::new(AtomicBool::new(true)); // fail the reap flush write

            let write_half = MockWriteHalf {
                written: written.clone(),
                fail_next_write: fail.clone(),
            };
            let read_half = BlockingReadHalf;

            conn.spawn(read_half, write_half);

            knet::sleep_ms(30).await;
            conn.close();
            knet::sleep_ms(20).await;

            let data = written.lock().unwrap().clone();
            let mut codec = FrameCodec::new(65536);
            codec.feed(&data);
            while let Some(f) = codec.decode() {
                assert!(
                    !(f.cmd == Cmd::Fin && f.stream_id == sid),
                    "FIN should not be present after a failing write in spawn"
                );
            }

            assert!(conn.session().streams().lock().get(&sid).is_none());
        }
    }
}
