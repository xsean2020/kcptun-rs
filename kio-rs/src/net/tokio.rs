//! tokio backend: UdpSocket, TcpListener, TcpStream.

use super::{raw_tcp_listener, raw_tcp_stream, raw_udp};
use std::io;
use std::net::SocketAddr;
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;

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
                let refs: Vec<&[u8]> = bufs[offset..].iter().map(|b| b.as_ref()).collect();
                match super::mmsg::sendmmsg_connected(self.inner.as_raw_fd(), &refs) {
                    Ok(n) if n > 0 => offset += n,
                    Ok(_) => self.inner.writable().await?,
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                        self.inner.writable().await?;
                    }
                    Err(e) => return Err(e),
                }
            }
            return Ok(());
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
                let refs: Vec<&[u8]> = bufs[offset..].iter().map(|b| b.as_ref()).collect();
                match super::mmsg::sendmmsg_to(self.inner.as_raw_fd(), &refs, &target) {
                    Ok(n) if n > 0 => offset += n,
                    Ok(_) => self.inner.writable().await?,
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                        self.inner.writable().await?;
                    }
                    Err(e) => return Err(e),
                }
            }
            return Ok(());
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
                    return Ok(out.len());
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(0),
                Err(e) => return Err(e),
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

    #[inline(always)]
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
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
    pub async fn accept(&self) -> io::Result<(TcpStream, SocketAddr)> {
        let (s, a) = self.inner.accept().await?;
        // Match Go net.TCPConn defaults and raw_tcp_stream: disable Nagle.
        let _ = s.set_nodelay(true);
        Ok((TcpStream { inner: s }, a))
    }
}

// ─── TcpStream ────────────────────────────────────────────────────────────────

pub struct TcpStream {
    inner: tokio::net::TcpStream,
}

impl TcpStream {
    #[inline(always)]
    pub async fn connect(addr: impl AsRef<str>) -> io::Result<Self> {
        let remote: SocketAddr = addr
            .as_ref()
            .parse()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        let std_stream = raw_tcp_stream(remote)?;
        let s = tokio::net::TcpStream::from_std(std_stream)?;
        Ok(Self { inner: s })
    }
}

impl crate::AsyncRead for TcpStream {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut crate::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
    }
}

impl crate::AsyncWrite for TcpStream {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}
