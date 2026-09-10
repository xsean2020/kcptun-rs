//! tokio backend: UdpSocket, TcpListener, TcpStream.

use super::{raw_tcp_listener, raw_udp, SOCK_BUF};
use std::io;
use std::net::SocketAddr;
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
use std::time::Duration;

/// Explicit TCP connect timeout. Prevents minute-level OS default timeouts
/// from tying up sessions when a target is unreachable.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

// ─── UdpSocket ────────────────────────────────────────────────────────────────

pub struct UdpSocket {
    inner: tokio::net::UdpSocket,
}

impl UdpSocket {
    /// Create a connected UDP socket (for client use).
    #[inline(always)]
    pub fn connect(bind_addr: SocketAddr, remote_addr: SocketAddr) -> io::Result<Self> {
        let std_sock = raw_udp(bind_addr, Some(remote_addr))?;
        let tokio_sock = tokio::net::UdpSocket::from_std(std_sock)?;
        Ok(Self { inner: tokio_sock })
    }

    /// Create a bound UDP socket (for server use).
    #[inline(always)]
    pub fn bind(bind_addr: SocketAddr) -> io::Result<Self> {
        let std_sock = raw_udp(bind_addr, None)?;
        let tokio_sock = tokio::net::UdpSocket::from_std(std_sock)?;
        Ok(Self { inner: tokio_sock })
    }

    /// Bind a fresh UDP socket that joins the SO_REUSEPORT group on
    /// `bind_addr` (Linux only). Binding N sockets created this way on the
    /// same address makes the kernel hash each flow's 4-tuple to exactly one
    /// member — per-flow receive affinity without a user-space
    /// demultiplexer.
    #[cfg(target_os = "linux")]
    #[inline(always)]
    pub fn bind_reuseport(bind_addr: SocketAddr) -> io::Result<Self> {
        let std_sock = super::raw_udp_reuseport(bind_addr)?;
        let tokio_sock = tokio::net::UdpSocket::from_std(std_sock)?;
        Ok(Self { inner: tokio_sock })
    }

    /// Wrap a pre-configured `std::net::UdpSocket`. The socket must be non-blocking.
    #[inline(always)]
    pub fn from_std(std_sock: std::net::UdpSocket) -> io::Result<Self> {
        let tokio_sock = tokio::net::UdpSocket::from_std(std_sock)?;
        Ok(Self { inner: tokio_sock })
    }

    #[inline(always)]
    pub async fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.recv(buf).await
    }

    #[inline(always)]
    pub async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        self.inner.recv_from(buf).await
    }

    #[inline(always)]
    pub async fn send(&self, buf: &[u8]) -> io::Result<usize> {
        self.inner.send(buf).await
    }

    #[inline(always)]
    pub async fn send_to(&self, buf: &[u8], target: SocketAddr) -> io::Result<usize> {
        self.inner.send_to(buf, target).await
    }

    /// Send all `bufs` on a connected socket without interleaving other work.
    ///
    /// Linux: `sendmmsg` batches (P1.2b). Other OS: `try_send` + `writable` (P1.2a).
    pub async fn send_batch<B: AsRef<[u8]>>(&self, bufs: &[B]) -> io::Result<()> {
        if bufs.is_empty() {
            return Ok(());
        }
        #[cfg(target_os = "linux")]
        {
            let mut offset = 0;
            while offset < bufs.len() {
                match super::mmsg::sendmmsg_connected(self.inner.as_raw_fd(), &bufs[offset..]) {
                    Ok(n) if n > 0 => offset += n,
                    Ok(_) => self.inner.writable().await?,
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                        self.inner.writable().await?;
                    }
                    Err(e) => return Err(e),
                }
            }
            Ok(())
        }
        #[cfg(not(target_os = "linux"))]
        {
            for buf in bufs {
                let mut remaining = buf.as_ref();
                while !remaining.is_empty() {
                    match self.inner.try_send(remaining) {
                        Ok(n) => remaining = &remaining[n..],
                        Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                            self.inner.writable().await?;
                        }
                        Err(e) => return Err(e),
                    }
                }
            }
            Ok(())
        }
    }

    /// Send all `bufs` to `target` (server / unconnected path).
    pub async fn send_batch_to<B: AsRef<[u8]>>(
        &self,
        bufs: &[B],
        target: SocketAddr,
    ) -> io::Result<()> {
        if bufs.is_empty() {
            return Ok(());
        }
        #[cfg(target_os = "linux")]
        {
            let mut offset = 0;
            while offset < bufs.len() {
                match super::mmsg::sendmmsg_to(self.inner.as_raw_fd(), &bufs[offset..], &target) {
                    Ok(n) if n > 0 => offset += n,
                    Ok(_) => self.inner.writable().await?,
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                        self.inner.writable().await?;
                    }
                    Err(e) => return Err(e),
                }
            }
            Ok(())
        }
        #[cfg(not(target_os = "linux"))]
        {
            for buf in bufs {
                let mut remaining = buf.as_ref();
                while !remaining.is_empty() {
                    match self.inner.try_send_to(remaining, target) {
                        Ok(n) => remaining = &remaining[n..],
                        Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                            self.inner.writable().await?;
                        }
                        Err(e) => return Err(e),
                    }
                }
            }
            Ok(())
        }
    }

    #[inline(always)]
    pub fn try_recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.try_recv(buf)
    }

    /// Non-blocking send for connected sockets (tokio native).
    #[inline(always)]
    pub fn try_send(&self, buf: &[u8]) -> io::Result<usize> {
        self.inner.try_send(buf)
    }

    /// Try to send all `bufs` without blocking. Returns the number of packets
    /// sent (stops on `WouldBlock` / partial).
    ///
    /// Linux: `sendmmsg` (one syscall for the batch). Others: per-packet
    /// `try_send`.
    pub fn try_send_batch<B: AsRef<[u8]>>(&self, bufs: &[B]) -> io::Result<usize> {
        if bufs.is_empty() {
            return Ok(0);
        }
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            match super::mmsg::sendmmsg_connected(self.inner.as_raw_fd(), bufs) {
                Ok(n) => Ok(n),
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => Ok(0),
                Err(e) => Err(e),
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let mut sent = 0;
            for b in bufs {
                let b = b.as_ref();
                match self.inner.try_send(b) {
                    Ok(n) if n == b.len() => sent += 1,
                    Ok(_) => return Ok(sent), // partial datagram (shouldn't happen)
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(sent),
                    Err(e) => return Err(e),
                }
            }
            Ok(sent)
        }
    }

    /// Try to send all `bufs` to an explicit peer (unconnected socket) without
    /// blocking. Returns the number of packets actually sent (stops on `WouldBlock`).
    ///
    /// Linux: `sendmmsg` with explicit `target` address. Others: per-packet
    /// `try_send_to`.
    pub fn try_send_batch_to<B: AsRef<[u8]>>(
        &self,
        bufs: &[B],
        target: SocketAddr,
    ) -> io::Result<usize> {
        if bufs.is_empty() {
            return Ok(0);
        }
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            match super::mmsg::sendmmsg_to(self.inner.as_raw_fd(), bufs, &target) {
                Ok(n) => Ok(n),
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => Ok(0),
                Err(e) => Err(e),
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            let mut sent = 0;
            for b in bufs {
                let b = b.as_ref();
                match self.inner.try_send_to(b, target) {
                    Ok(n) if n == b.len() => sent += 1,
                    Ok(_) => return Ok(sent), // partial datagram (shouldn't happen)
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(sent),
                    Err(e) => return Err(e),
                }
            }
            Ok(sent)
        }
    }

    #[inline(always)]
    pub fn try_recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        self.inner.try_recv_from(buf)
    }

    /// Drain ready datagrams without awaiting between packets (P1.3).
    ///
    /// Returns number of packets written into `out` (each is `(payload, peer)`).
    /// Stops on WouldBlock or when `out` is full.
    pub fn try_recv_batch_from(
        &self,
        packet_bufs: &mut [Vec<u8>],
        out: &mut Vec<(Vec<u8>, SocketAddr)>,
    ) -> io::Result<usize> {
        out.clear();
        #[cfg(target_os = "linux")]
        {
            match super::mmsg::recvmmsg_from(self.inner.as_raw_fd(), packet_bufs) {
                Ok(msgs) => {
                    for (i, (n, addr)) in msgs.into_iter().enumerate() {
                        if let Some(peer) = addr {
                            let mut v = std::mem::take(&mut packet_bufs[i]);
                            v.truncate(n);
                            // Replace slot with a fresh empty buffer for next batch.
                            packet_bufs[i] = Vec::with_capacity(v.capacity().max(2048));
                            out.push((v, peer));
                        }
                    }
                    Ok(out.len())
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => Ok(0),
                Err(e) => Err(e),
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            for slot in packet_bufs.iter_mut() {
                if slot.capacity() < 2048 {
                    slot.reserve(2048);
                }
                slot.resize(slot.capacity(), 0);
                match self.inner.try_recv_from(slot) {
                    Ok((n, peer)) => {
                        let payload = slot[..n].to_vec();
                        slot.clear();
                        out.push((payload, peer));
                    }
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) => return Err(e),
                }
            }
            Ok(out.len())
        }
    }

    /// Allocation-free batch receive on an unconnected socket.
    ///
    /// Fills `packet_bufs[..n]` with payloads **in place** (slots stay owned by
    /// the caller, no per-slot replacement `Vec`) and writes the source
    /// addresses into `peers` (cleared first; capacity reused). Returns the
    /// number of datagrams received; `Ok(0)` on `WouldBlock`.
    ///
    /// Linux: one `recvmmsg` syscall. Elsewhere: sequential `try_recv_from`
    /// filling slots in place (no `to_vec()` copy).
    pub fn try_recv_batch_from_into(
        &self,
        packet_bufs: &mut [Vec<u8>],
        peers: &mut Vec<SocketAddr>,
    ) -> io::Result<usize> {
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            return super::mmsg::recvmmsg_from_into(self.inner.as_raw_fd(), packet_bufs, peers);
        }
        #[cfg(not(target_os = "linux"))]
        {
            peers.clear();
            let mut n = 0;
            for slot in packet_bufs.iter_mut() {
                if slot.capacity() < 2048 {
                    slot.reserve(2048);
                }
                slot.resize(slot.capacity(), 0);
                match self.inner.try_recv_from(slot) {
                    Ok((len, peer)) => {
                        slot.truncate(len);
                        peers.push(peer);
                        n += 1;
                    }
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(e) => return Err(e),
                }
            }
            Ok(n)
        }
    }

    #[inline(always)]
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }

    /// Set the TTL (hop limit). Delegates to the underlying tokio UdpSocket.
    pub fn set_ttl(&self, ttl: u32) -> io::Result<()> {
        self.inner.set_ttl(ttl)
    }

    /// Get the TTL (hop limit). Delegates to the underlying tokio UdpSocket.
    pub fn ttl(&self) -> io::Result<u32> {
        self.inner.ttl()
    }

    /// Non-blocking batch receive on a connected socket (no peer addresses).
    ///
    /// Linux/macOS: one `recvmmsg` syscall fills the pool. Others: one
    /// `try_recv` per call into `pool[0]`. Returns datagrams received.
    pub fn try_recv_batch(&self, pool: &mut [Vec<u8>]) -> io::Result<usize> {
        if pool.is_empty() {
            return Ok(0);
        }
        #[cfg(target_os = "linux")]
        {
            match super::mmsg::recvmmsg_connected(self.inner.as_raw_fd(), pool) {
                Ok(n) => Ok(n),
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => Ok(0),
                Err(e) => Err(e),
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            if pool[0].capacity() < 2048 {
                pool[0].reserve(2048);
            }
            // resize to capacity so try_recv gets a non-zero-length buffer.
            pool[0].resize(pool[0].capacity(), 0);
            match self.inner.try_recv(&mut pool[0]) {
                Ok(n) if n > 0 => {
                    pool[0].truncate(n);
                    Ok(1)
                }
                Ok(_) => Ok(0),
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => Ok(0),
                Err(e) => Err(e),
            }
        }
    }
}

// ─── TcpListener ──────────────────────────────────────────────────────────────

pub struct TcpListener {
    inner: tokio::net::TcpListener,
}

impl TcpListener {
    #[inline(always)]
    pub async fn bind(addr: SocketAddr) -> io::Result<Self> {
        let std_listener = raw_tcp_listener(addr)?;
        let l = tokio::net::TcpListener::from_std(std_listener)?;
        Ok(Self { inner: l })
    }

    #[inline(always)]
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }

    #[inline(always)]
    pub async fn accept(&self) -> io::Result<(TcpStream, SocketAddr)> {
        let (s, a) = self.inner.accept().await?;
        // Match Go net.TCPConn defaults: disable Nagle.
        let _ = s.set_nodelay(true);
        Ok((
            TcpStream {
                inner: TcpStreamInner::Tcp(s),
            },
            a,
        ))
    }

    /// Non-blocking accept of one pending connection; `WouldBlock` when none.
    ///
    /// Uses `poll_accept` with a no-op waker (tokio 1.52 lacks `try_accept`).
    #[inline(always)]
    pub fn try_accept(&self) -> io::Result<(TcpStream, SocketAddr)> {
        use std::task::{Context, Poll, Waker};
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        match self.inner.poll_accept(&mut cx) {
            Poll::Ready(Ok((s, a))) => {
                let _ = s.set_nodelay(true);
                Ok((
                    TcpStream {
                        inner: TcpStreamInner::Tcp(s),
                    },
                    a,
                ))
            }
            Poll::Ready(Err(e)) => Err(e),
            Poll::Pending => Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "no pending connection",
            )),
        }
    }
}

// ─── UnixListener ────────────────────────────────────────────────────────────

/// Unix-domain listener used when the Go-compatible client `localaddr` is a
/// filesystem path rather than a TCP host/port.
#[cfg(unix)]
pub struct UnixListener {
    inner: tokio::net::UnixListener,
    path: std::path::PathBuf,
}

#[cfg(unix)]
impl UnixListener {
    pub async fn bind(path: impl AsRef<std::path::Path>) -> io::Result<Self> {
        let path = path.as_ref().to_owned();
        let inner = tokio::net::UnixListener::bind(&path)?;
        Ok(Self { inner, path })
    }

    pub async fn accept(&self) -> io::Result<(TcpStream, String)> {
        let (stream, addr) = self.inner.accept().await?;
        let peer = addr
            .as_pathname()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "unix".to_string());
        Ok((
            TcpStream {
                inner: TcpStreamInner::Unix(stream),
            },
            peer,
        ))
    }

    pub fn try_accept(&self) -> io::Result<(TcpStream, String)> {
        use std::task::{Context, Poll, Waker};
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        match self.inner.poll_accept(&mut cx) {
            Poll::Ready(Ok((stream, addr))) => {
                let peer = addr
                    .as_pathname()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "unix".to_string());
                Ok((
                    TcpStream {
                        inner: TcpStreamInner::Unix(stream),
                    },
                    peer,
                ))
            }
            Poll::Ready(Err(error)) => Err(error),
            Poll::Pending => Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "no pending connection",
            )),
        }
    }
}

#[cfg(unix)]
impl Drop for UnixListener {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_file(&self.path) {
            if error.kind() != io::ErrorKind::NotFound {
                log::warn!(
                    "failed to remove unix listener {}: {}",
                    self.path.display(),
                    error
                );
            }
        }
    }
}

// ─── TcpStream ────────────────────────────────────────────────────────────────

enum TcpStreamInner {
    Tcp(tokio::net::TcpStream),
    #[cfg(unix)]
    Unix(tokio::net::UnixStream),
}

pub struct TcpStream {
    inner: TcpStreamInner,
}

impl TcpStream {
    /// Connect to `addr` (IP:port, hostname:port, or Unix path on Unix).
    ///
    /// DNS resolution and non-blocking TCP connect are handled by tokio's
    /// internal resolver (`spawn_blocking`), **not** our `cpu_block`
    /// crypto/snappy pool. This prevents slow DNS or unreachable targets
    /// from starving crypto work.
    ///
    /// Numeric `SocketAddr` inputs skip DNS entirely.
    #[inline(always)]
    pub async fn connect(addr: impl AsRef<str>) -> io::Result<Self> {
        let addr_str = addr.as_ref().to_owned();

        // Numeric IP:port — skip DNS, connect directly.
        if let Ok(socket_addr) = addr_str.parse::<SocketAddr>() {
            let stream =
                tokio::time::timeout(CONNECT_TIMEOUT, tokio::net::TcpStream::connect(socket_addr))
                    .await
                    .map_err(|_| {
                        io::Error::new(
                            io::ErrorKind::TimedOut,
                            format!("connect timeout: {addr_str}"),
                        )
                    })??;
            tune_socket(&stream);
            return Ok(Self {
                inner: TcpStreamInner::Tcp(stream),
            });
        }

        // Hostname:port — tokio resolves DNS via spawn_blocking (separate
        // from our crypto pool), then non-blocking connect with timeout.
        match tokio::time::timeout(
            CONNECT_TIMEOUT,
            tokio::net::TcpStream::connect(addr_str.as_str()),
        )
        .await
        {
            Ok(Ok(stream)) => {
                tune_socket(&stream);
                Ok(Self {
                    inner: TcpStreamInner::Tcp(stream),
                })
            }
            Ok(Err(_)) | Err(_) => {
                // Fall back to Unix-domain socket.
                #[cfg(unix)]
                {
                    match tokio::net::UnixStream::connect(&addr_str).await {
                        Ok(stream) => Ok(Self {
                            inner: TcpStreamInner::Unix(stream),
                        }),
                        Err(unix_err) => Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            format!("could not connect to {addr_str} as TCP or Unix: {unix_err}"),
                        )),
                    }
                }
                #[cfg(not(unix))]
                Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("invalid TCP address: {addr_str}"),
                ))
            }
        }
    }
}

/// Set socket buffer sizes and TCP_NODELAY on a connected tokio TcpStream.
/// Uses `socket2::SockRef` to access the underlying socket fd.
fn tune_socket(stream: &tokio::net::TcpStream) {
    let sock_ref = socket2::SockRef::from(stream);
    let _ = sock_ref.set_recv_buffer_size(SOCK_BUF);
    let _ = sock_ref.set_send_buffer_size(SOCK_BUF);
    let _ = sock_ref.set_nodelay(true);
}

impl crate::AsyncRead for TcpStream {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut crate::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        match &mut self.get_mut().inner {
            TcpStreamInner::Tcp(stream) => std::pin::Pin::new(stream).poll_read(cx, buf),
            #[cfg(unix)]
            TcpStreamInner::Unix(stream) => std::pin::Pin::new(stream).poll_read(cx, buf),
        }
    }
}

impl crate::AsyncWrite for TcpStream {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        match &mut self.get_mut().inner {
            TcpStreamInner::Tcp(stream) => std::pin::Pin::new(stream).poll_write(cx, buf),
            #[cfg(unix)]
            TcpStreamInner::Unix(stream) => std::pin::Pin::new(stream).poll_write(cx, buf),
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        match &mut self.get_mut().inner {
            TcpStreamInner::Tcp(stream) => std::pin::Pin::new(stream).poll_flush(cx),
            #[cfg(unix)]
            TcpStreamInner::Unix(stream) => std::pin::Pin::new(stream).poll_flush(cx),
        }
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        match &mut self.get_mut().inner {
            TcpStreamInner::Tcp(stream) => std::pin::Pin::new(stream).poll_shutdown(cx),
            #[cfg(unix)]
            TcpStreamInner::Unix(stream) => std::pin::Pin::new(stream).poll_shutdown(cx),
        }
    }
}
