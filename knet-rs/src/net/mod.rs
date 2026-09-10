//! Network socket wrappers.
//!
//! All sockets (TCP + UDP) are created via `socket2` for uniform buffer
//! tuning, SO_REUSEADDR, and non-blocking mode. The raw fd is then handed
//! to tokio's async wrapper.

use std::io;
use std::net::SocketAddr;

/// UDP recv/send buffer size (4 MB).
const SOCK_BUF: usize = 4 * 1024 * 1024;

/// Create a tuned, non-blocking `std::net::UdpSocket` via socket2.
///
/// - 4 MB recv/send buffer sizes
/// - SO_REUSEADDR
/// - non-blocking mode
///
/// If `remote_addr` is provided, the socket is `connect()`ed (client mode).
#[cfg(not(target_os = "windows"))]
pub(crate) fn raw_udp(
    bind_addr: SocketAddr,
    remote_addr: Option<SocketAddr>,
) -> io::Result<std::net::UdpSocket> {
    let domain = if bind_addr.is_ipv4() {
        socket2::Domain::IPV4
    } else {
        socket2::Domain::IPV6
    };
    let socket = socket2::Socket::new(domain, socket2::Type::DGRAM, None)?;

    let _ = socket.set_recv_buffer_size(SOCK_BUF);
    let _ = socket.set_send_buffer_size(SOCK_BUF);
    let _ = socket.set_reuse_address(true);

    socket.bind(&bind_addr.into())?;
    if let Some(remote) = remote_addr {
        socket.connect(&remote.into())?;
    }
    socket.set_nonblocking(true)?;
    Ok(socket.into())
}

/// Windows fallback: `socket2` is not used for UDP on Windows; use the
/// standard library's `UdpSocket` directly. Buffer tuning is skipped because
/// the standard API doesn't expose per-socket buffer sizing.
#[cfg(target_os = "windows")]
pub(crate) fn raw_udp(
    bind_addr: SocketAddr,
    remote_addr: Option<SocketAddr>,
) -> io::Result<std::net::UdpSocket> {
    let std_sock = std::net::UdpSocket::bind(bind_addr)?;
    if let Some(remote) = remote_addr {
        std_sock.connect(remote)?;
    }
    std_sock.set_nonblocking(true)?;
    Ok(std_sock)
}

/// SO_REUSEPORT variant of [`raw_udp`] (Linux): binds a fresh socket that
/// joins the kernel's reuseport group for `bind_addr`. Every group member
/// receives the flows whose 4-tuple hash maps to it, so N sockets give N-way
/// receive fan-out with stable per-flow affinity and no user-space
/// demultiplexer. All members must be created with this constructor before
/// the group's first bind.
#[cfg(target_os = "linux")]
pub(crate) fn raw_udp_reuseport(bind_addr: SocketAddr) -> io::Result<std::net::UdpSocket> {
    let domain = if bind_addr.is_ipv4() {
        socket2::Domain::IPV4
    } else {
        socket2::Domain::IPV6
    };
    let socket = socket2::Socket::new(domain, socket2::Type::DGRAM, None)?;

    let _ = socket.set_recv_buffer_size(SOCK_BUF);
    let _ = socket.set_send_buffer_size(SOCK_BUF);
    let _ = socket.set_reuse_address(true);
    socket.set_reuse_port(true)?;

    socket.bind(&bind_addr.into())?;
    socket.set_nonblocking(true)?;
    Ok(socket.into())
}

/// Create a tuned, non-blocking `std::net::TcpListener` via socket2.
pub(crate) fn raw_tcp_listener(addr: SocketAddr) -> io::Result<std::net::TcpListener> {
    let domain = if addr.is_ipv4() {
        socket2::Domain::IPV4
    } else {
        socket2::Domain::IPV6
    };
    let socket = socket2::Socket::new(domain, socket2::Type::STREAM, Some(socket2::Protocol::TCP))?;

    let _ = socket.set_recv_buffer_size(SOCK_BUF);
    let _ = socket.set_send_buffer_size(SOCK_BUF);
    socket.set_reuse_address(true)?;

    socket.bind(&addr.into())?;
    socket.listen(1024)?;
    socket.set_nonblocking(true)?;
    Ok(socket.into())
}

// ─── Backend selection ────────────────────────────────────────────────────────
#[cfg(target_os = "linux")]
mod mmsg;

mod tokio;

#[cfg(unix)]
pub use self::tokio::UnixListener;
pub use self::tokio::{TcpListener, TcpStream, UdpSocket};

// ─── TCP raw transport (Linux only) ──────────────────────────────────────────
#[cfg(target_os = "linux")]
mod tcpraw;

#[cfg(not(target_os = "linux"))]
mod tcpraw_stub;

#[cfg(target_os = "linux")]
pub use self::tcpraw::{dial as tcpraw_dial, listen as tcpraw_listen, TcpRawConn, TcpRawListener};

#[cfg(not(target_os = "linux"))]
pub use self::tcpraw_stub::{
    dial as tcpraw_dial, listen as tcpraw_listen, TcpRawConn, TcpRawListener,
};

// ─── DatagramSocket ──────────────────────────────────────────────────────────

/// Unified datagram socket over UDP or TCP raw transport.
///
/// Match-based dispatch — no trait/generic overhead, and the transport set
/// is closed (UDP + optionally TCP raw on Linux).
pub enum DatagramSocket {
    /// Standard UDP socket.
    Udp(UdpSocket),
    /// TCP raw socket (Linux only; compile-error on other platforms when
    /// constructed, but the variant exists for code coherence).
    TcpRaw(TcpRawConn),
}

impl DatagramSocket {
    pub async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        match self {
            Self::Udp(s) => s.recv_from(buf).await,
            Self::TcpRaw(s) => s.recv_from(buf).await,
        }
    }

    pub async fn send_to(&self, buf: &[u8], target: SocketAddr) -> io::Result<usize> {
        match self {
            Self::Udp(s) => s.send_to(buf, target).await,
            Self::TcpRaw(s) => s.send_to(buf, &target),
        }
    }

    pub async fn send(&self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Udp(s) => s.send(buf).await,
            Self::TcpRaw(s) => s.send(buf),
        }
    }

    pub async fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Udp(s) => s.recv(buf).await,
            Self::TcpRaw(s) => s.recv(buf).await,
        }
    }

    pub fn try_recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Udp(s) => s.try_recv(buf),
            Self::TcpRaw(s) => s.try_recv(buf),
        }
    }

    pub fn try_recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        match self {
            Self::Udp(s) => s.try_recv_from(buf),
            Self::TcpRaw(s) => s.try_recv_from(buf),
        }
    }

    /// Try to send all `bufs` on a connected socket without blocking.
    ///
    /// Returns the number of packets actually sent (stops on `WouldBlock`).
    /// Returns `0` for transports that can't send synchronously (TcpRaw), so
    /// the caller falls back to the async path.
    pub fn try_send_batch<B: AsRef<[u8]>>(&self, bufs: &[B]) -> io::Result<usize> {
        match self {
            Self::Udp(s) => s.try_send_batch(bufs),
            Self::TcpRaw(_) => Ok(0),
        }
    }

    /// Try to send all `bufs` to an explicit peer (unconnected socket) without blocking.
    ///
    /// Returns the number of packets actually sent (stops on `WouldBlock`).
    /// Returns `0` for transports that can't send synchronously (TcpRaw), so
    /// the caller falls back to the async path.
    pub fn try_send_batch_to<B: AsRef<[u8]>>(
        &self,
        bufs: &[B],
        target: SocketAddr,
    ) -> io::Result<usize> {
        match self {
            Self::Udp(s) => s.try_send_batch_to(bufs, target),
            Self::TcpRaw(_) => Ok(0),
        }
    }

    pub async fn send_batch_to<B: AsRef<[u8]>>(
        &self,
        bufs: &[B],
        target: SocketAddr,
    ) -> io::Result<()> {
        match self {
            Self::Udp(s) => s.send_batch_to(bufs, target).await,
            Self::TcpRaw(s) => s.send_batch_to(bufs, &target).await,
        }
    }

    /// Send all `bufs` on a connected socket.
    pub async fn send_batch<B: AsRef<[u8]>>(&self, bufs: &[B]) -> io::Result<()> {
        match self {
            Self::Udp(s) => s.send_batch(bufs).await,
            Self::TcpRaw(s) => {
                // TCP raw is point-to-point; send each buffer individually.
                for buf in bufs {
                    s.send(buf.as_ref())?;
                }
                Ok(())
            }
        }
    }

    pub fn try_recv_batch_from(
        &self,
        packet_bufs: &mut [Vec<u8>],
        out: &mut Vec<(Vec<u8>, SocketAddr)>,
    ) -> io::Result<usize> {
        match self {
            Self::Udp(s) => s.try_recv_batch_from(packet_bufs, out),
            Self::TcpRaw(s) => s.try_recv_batch_from(packet_bufs, out),
        }
    }

    /// Allocation-free batch receive on an unconnected socket: fills
    /// `packet_bufs[..n]` with payloads in place and writes source addresses
    /// into `peers` (capacity reused). `TcpRaw` returns `0`.
    pub fn try_recv_batch_from_into(
        &self,
        packet_bufs: &mut [Vec<u8>],
        peers: &mut Vec<SocketAddr>,
    ) -> io::Result<usize> {
        match self {
            Self::Udp(s) => s.try_recv_batch_from_into(packet_bufs, peers),
            Self::TcpRaw(_) => Ok(0),
        }
    }

    /// Non-blocking batch receive on a connected socket into the caller's
    /// buffer pool. Linux: `recvmmsg` (one syscall). Others: one `try_recv`
    /// into `pool[0]`. Returns datagrams received.
    pub fn try_recv_batch(&self, pool: &mut [Vec<u8>]) -> io::Result<usize> {
        match self {
            Self::Udp(s) => s.try_recv_batch(pool),
            Self::TcpRaw(_) => Ok(0),
        }
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        match self {
            Self::Udp(s) => s.local_addr(),
            Self::TcpRaw(s) => s.local_addr(),
        }
    }

    /// Set the TTL (hop limit) on the underlying socket.
    /// Only meaningful for UDP; TcpRaw returns `Unsupported`.
    pub fn set_ttl(&self, ttl: u32) -> io::Result<()> {
        match self {
            Self::Udp(s) => s.set_ttl(ttl),
            Self::TcpRaw(_) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "set_ttl not available on TcpRaw",
            )),
        }
    }

    /// Get the TTL (hop limit) from the underlying socket.
    /// Only meaningful for UDP; TcpRaw returns `Unsupported`.
    pub fn ttl(&self) -> io::Result<u32> {
        match self {
            Self::Udp(s) => s.ttl(),
            Self::TcpRaw(_) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "ttl not available on TcpRaw",
            )),
        }
    }
}
