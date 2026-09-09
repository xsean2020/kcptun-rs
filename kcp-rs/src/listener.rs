//! KCP-over-raw-TCP listener ([`KcpTcpListener`]) — a TCP transport factory.
//!
//! In the system-networking sense a "listener" is a *demux + accept* point:
//! packets arrive on one socket and are demultiplexed into per-peer sessions
//! ([`crate::KcpListener`] for UDP, `knet::TcpListener` for TCP). This type is
//! **not** that. Here raw TCP is merely the *transport* carrying KCP: each
//! accepted TCP stream is wrapped in a `TcpRaw` [`PacketTransport`] and handed
//! to a fresh connected [`KcpStream`]. One accepted TCP connection == one
//! KCP session; there is no demultiplexing step at all.
//!
//! It therefore exists to plug the TCP transport into the same
//! `KcpStream::with_transport` construction path a UDP or crypto-wrapped
//! transport would use — a factory, not a demux listener. For the real
//! UDP listener (worker pipeline / SO_REUSEPORT topologies), see
//! [`crate::KcpListener`]; for plain TCP forwarding use `knet` directly.
//!
//! Linux only (`knet::TcpRawListener`); non-Linux bind returns
//! `io::Unsupported`.
//!
//! Known limitation: [`KcpTcpListener::close`] only flips the internal flag;
//! it does not abort an `accept()` already blocked in the kernel (the
//! underlying `TcpRawListener::accept` runs a blocking `accept(2)` inside
//! `cpu_block`). A blocked `accept()` returns once the listener is dropped
//! and the fd closes.

use std::io;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::config::KcpConfig;
use crate::conn::KcpStream;
use crate::conn::resolve_one;
use crate::transport::PacketTransport;

/// TCP-transport factory for connected [`KcpStream`] sessions: each accepted
/// raw-TCP connection becomes its own [`KcpStream`]. See the [module
/// documentation](self) for why this is a transport factory rather than a
/// demux listener. Linux only.
pub struct KcpTcpListener {
    listener: knet::TcpRawListener,
    config: KcpConfig,
    closed: AtomicBool,
}

impl Drop for KcpTcpListener {
    fn drop(&mut self) {
        self.close();
    }
}

impl KcpTcpListener {
    pub fn bind(addr: impl ToSocketAddrs) -> KcpTcpListenerBuilder {
        match resolve_one(addr) {
            Ok(a) => KcpTcpListenerBuilder {
                addr: Some(a),
                config: KcpConfig::default(),
                resolve_err: None,
            },
            Err(e) => KcpTcpListenerBuilder {
                addr: None,
                config: KcpConfig::default(),
                resolve_err: Some(e),
            },
        }
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Accept the next client connection: one [`KcpStream`] per accepted TCP
    /// connection. Returns `ConnectionAborted` once closed.
    pub async fn accept(&self) -> io::Result<(KcpStream, SocketAddr)> {
        if self.closed.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "KcpTcpListener closed",
            ));
        }
        let (conn, peer) = match self.listener.accept().await {
            Ok(v) => v,
            Err(e) => return Err(io::Error::new(e.kind(), format!("accept failed: {e}"))),
        };
        let socket: Arc<dyn PacketTransport> = Arc::new(knet::DatagramSocket::TcpRaw(conn));
        let kcp = KcpStream::with_transport(socket, peer)
            .connected(true)
            .config(self.config.clone())
            .build()
            .await?;
        Ok((kcp, peer))
    }

    /// Stop accepting new connections. Existing accepted [`KcpStream`]s are
    /// unaffected (they hold their own raw-fd Arc). See the [module
    /// documentation](self) for the blocked-`accept()` limitation.
    pub fn close(&self) {
        if self
            .closed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {}
    }

    /// Accept the next client connection within `timeout`, or fail with
    /// [`io::ErrorKind::TimedOut`].
    pub async fn accept_timeout(&self, timeout: Duration) -> io::Result<(KcpStream, SocketAddr)> {
        knet::timeout(timeout, self.accept())
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "accept timed out"))?
    }
}

/// Builder for [`KcpTcpListener`].
pub struct KcpTcpListenerBuilder {
    addr: Option<SocketAddr>,
    config: KcpConfig,
    resolve_err: Option<io::Error>,
}

impl KcpTcpListenerBuilder {
    pub fn config(mut self, cfg: KcpConfig) -> Self {
        self.config = cfg;
        self
    }

    /// Bind the raw-TCP listener and return it.
    pub fn build(self) -> io::Result<KcpTcpListener> {
        if let Some(e) = self.resolve_err {
            return Err(e);
        }
        let addr = self.addr.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "KcpTcpListener: bind address required",
            )
        })?;
        let listener = knet::tcpraw_listen(&addr)?;
        Ok(KcpTcpListener {
            listener,
            config: self.config,
            closed: AtomicBool::new(false),
        })
    }
}

/// `KcpTcpListener::bind(addr).await` — awaitable without an explicit `.build()`.
impl std::future::IntoFuture for KcpTcpListenerBuilder {
    type Output = io::Result<KcpTcpListener>;
    type IntoFuture = std::pin::Pin<Box<dyn std::future::Future<Output = Self::Output> + Send>>;

    fn into_future(self) -> Self::IntoFuture {
        // `build` is sync (bind + tcpraw_listen); wrap it so `.await` works.
        Box::pin(async move { self.build() })
    }
}
