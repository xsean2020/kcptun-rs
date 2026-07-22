//! smol backend: UdpSocket, TcpListener, TcpStream.

use super::{raw_tcp_listener, raw_tcp_stream, raw_udp};
use std::io;
use std::net::SocketAddr;

// ─── UdpSocket ────────────────────────────────────────────────────────────────

pub struct UdpSocket {
    inner: smol::net::UdpSocket,
}

impl UdpSocket {
    /// Create a connected UDP socket (for client use).
    #[inline(always)]
    pub fn connect(bind_addr: SocketAddr, remote_addr: SocketAddr) -> io::Result<Self> {
        let std_sock = raw_udp(bind_addr, Some(remote_addr))?;
        let async_sock = async_io::Async::new(std_sock)?;
        let smol_sock = smol::net::UdpSocket::from(async_sock);
        Ok(Self { inner: smol_sock })
    }

    /// Create a bound UDP socket (for server use).
    #[inline(always)]
    pub fn bind(bind_addr: SocketAddr) -> io::Result<Self> {
        let std_sock = raw_udp(bind_addr, None)?;
        let async_sock = async_io::Async::new(std_sock)?;
        let smol_sock = smol::net::UdpSocket::from(async_sock);
        Ok(Self { inner: smol_sock })
    }

    /// Wrap a pre-configured `std::net::UdpSocket`. The socket must be non-blocking.
    #[inline(always)]
    pub fn from_std(std_sock: std::net::UdpSocket) -> io::Result<Self> {
        let async_sock = async_io::Async::new(std_sock)?;
        let smol_sock = smol::net::UdpSocket::from(async_sock);
        Ok(Self { inner: smol_sock })
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

    /// Send all `bufs` without interleaving other work.
    ///
    /// Linux: `sendmmsg` (P1.2b). Other OS: sequential `send` (P1.2a).
    pub async fn send_batch<B: AsRef<[u8]>>(&self, bufs: &[B]) -> io::Result<()> {
        if bufs.is_empty() {
            return Ok(());
        }
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            let mut offset = 0;
            while offset < bufs.len() {
                let refs: Vec<&[u8]> = bufs[offset..].iter().map(|b| b.as_ref()).collect();
                match super::mmsg::sendmmsg_connected(self.inner.as_raw_fd(), &refs) {
                    Ok(n) if n > 0 => offset += n,
                    Ok(_) => {
                        // Wait until writable via a no-op send readiness: poll send of empty fails,
                        // so use async send of first remaining packet as readiness probe.
                        let _ = self.inner.send(bufs[offset].as_ref()).await?;
                        offset += 1;
                    }
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                        let _ = self.inner.send(bufs[offset].as_ref()).await?;
                        offset += 1;
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
                    let n = self.inner.send(remaining).await?;
                    remaining = &remaining[n..];
                }
            }
            Ok(())
        }
    }

    /// Send all `bufs` to `target`.
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
            use std::os::fd::AsRawFd;
            let mut offset = 0;
            while offset < bufs.len() {
                let refs: Vec<&[u8]> = bufs[offset..].iter().map(|b| b.as_ref()).collect();
                match super::mmsg::sendmmsg_to(self.inner.as_raw_fd(), &refs, &target) {
                    Ok(n) if n > 0 => offset += n,
                    Ok(_) => {
                        let _ = self.inner.send_to(bufs[offset].as_ref(), target).await?;
                        offset += 1;
                    }
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                        let _ = self.inner.send_to(bufs[offset].as_ref(), target).await?;
                        offset += 1;
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
                    let n = self.inner.send_to(remaining, target).await?;
                    remaining = &remaining[n..];
                }
            }
            Ok(())
        }
    }

    /// Non-blocking recv for connected sockets (smol: via get_ref + try).
    pub fn try_recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        // smol/async-net has no try_recv; use libc recv with MSG_DONTWAIT on Linux,
        // otherwise WouldBlock always to force async path.
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            let fd = self.inner.as_raw_fd();
            let n = unsafe {
                libc::recv(
                    fd,
                    buf.as_mut_ptr() as *mut _,
                    buf.len(),
                    libc::MSG_DONTWAIT,
                )
            };
            if n < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(n as usize)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = buf;
            Err(io::Error::from(io::ErrorKind::WouldBlock))
        }
    }

    pub fn try_recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            // use recvfrom with MSG_DONTWAIT
            let fd = self.inner.as_raw_fd();
            let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
            let mut len = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
            let n = unsafe {
                libc::recvfrom(
                    fd,
                    buf.as_mut_ptr() as *mut _,
                    buf.len(),
                    libc::MSG_DONTWAIT,
                    &mut storage as *mut _ as *mut libc::sockaddr,
                    &mut len,
                )
            };
            if n < 0 {
                return Err(io::Error::last_os_error());
            }
            // parse addr via mmsg helper pattern - inline minimal
            let peer = match storage.ss_family as i32 {
                x if x == libc::AF_INET as i32 => {
                    let sin = unsafe { &*(&storage as *const _ as *const libc::sockaddr_in) };
                    let ip = std::net::Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
                    std::net::SocketAddr::V4(std::net::SocketAddrV4::new(
                        ip,
                        u16::from_be(sin.sin_port),
                    ))
                }
                x if x == libc::AF_INET6 as i32 => {
                    let sin6 = unsafe { &*(&storage as *const _ as *const libc::sockaddr_in6) };
                    let ip = std::net::Ipv6Addr::from(sin6.sin6_addr.s6_addr);
                    std::net::SocketAddr::V6(std::net::SocketAddrV6::new(
                        ip,
                        u16::from_be(sin6.sin6_port),
                        sin6.sin6_flowinfo,
                        sin6.sin6_scope_id,
                    ))
                }
                _ => {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "unknown family"));
                }
            };
            Ok((n as usize, peer))
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = buf;
            Err(io::Error::from(io::ErrorKind::WouldBlock))
        }
    }

    /// Drain ready datagrams (Linux: recvmmsg; else sequential try_recv_from).
    pub fn try_recv_batch_from(
        &self,
        packet_bufs: &mut [Vec<u8>],
        out: &mut Vec<(Vec<u8>, SocketAddr)>,
    ) -> io::Result<usize> {
        out.clear();
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
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
                match self.try_recv_from(slot) {
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
    inner: smol::net::TcpListener,
}

impl TcpListener {
    #[inline(always)]
    pub async fn bind(addr: SocketAddr) -> io::Result<Self> {
        let std_listener = raw_tcp_listener(addr)?;
        let async_listener = async_io::Async::new(std_listener)?;
        let l = smol::net::TcpListener::from(async_listener);
        Ok(Self { inner: l })
    }

    #[inline(always)]
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
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
    inner: smol::net::TcpStream,
}

impl TcpStream {
    #[inline(always)]
    pub async fn connect(addr: impl AsRef<str>) -> io::Result<Self> {
        let remote: SocketAddr = addr
            .as_ref()
            .parse()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        let std_stream = raw_tcp_stream(remote)?;
        let async_stream = async_io::Async::new(std_stream)?;
        let s = smol::net::TcpStream::from(async_stream);
        Ok(Self { inner: s })
    }
}

impl crate::AsyncRead for TcpStream {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut [u8],
    ) -> std::task::Poll<io::Result<usize>> {
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

    fn poll_close(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_close(cx)
    }
}
