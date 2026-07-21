//! Network socket wrappers.
//!
//! All sockets (TCP + UDP) are created via `socket2` for uniform buffer
//! tuning, SO_REUSEADDR, and non-blocking mode. The raw fd is then handed
//! to the backend runtime's async wrapper (tokio or smol).
//!
//! Bidirectional copy (`copy_bidirectional` / `copy_bidirectional_idle`)
//! lives in [`crate::lib`] with custom 64 KB buffers — do not re-add it here.

use std::io;
use std::net::SocketAddr;

/// UDP recv/send buffer size (2 MB).
const SOCK_BUF: usize = 2 * 1024 * 1024;

/// Create a tuned, non-blocking `std::net::UdpSocket` via socket2.
///
/// Both backends share this function to ensure identical socket configuration:
/// - 2 MB recv/send buffer sizes
/// - SO_REUSEADDR
/// - non-blocking mode
///
/// If `remote_addr` is provided, the socket is `connect()`ed (client mode).
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

/// Create a tuned, non-blocking `std::net::TcpStream` via socket2.
pub(crate) fn raw_tcp_stream(remote_addr: SocketAddr) -> io::Result<std::net::TcpStream> {
    let domain = if remote_addr.is_ipv4() {
        socket2::Domain::IPV4
    } else {
        socket2::Domain::IPV6
    };
    let socket = socket2::Socket::new(domain, socket2::Type::STREAM, Some(socket2::Protocol::TCP))?;

    let _ = socket.set_recv_buffer_size(SOCK_BUF);
    let _ = socket.set_send_buffer_size(SOCK_BUF);
    let _ = socket.set_nodelay(true);

    socket.connect(&remote_addr.into())?;
    socket.set_nonblocking(true)?;
    Ok(socket.into())
}

// ─── Backend selection ────────────────────────────────────────────────────────
#[cfg(target_os = "linux")]
mod mmsg;

#[cfg(feature = "tokio")]
mod tokio;

#[cfg(feature = "smol")]
mod smol;

#[cfg(feature = "tokio")]
pub use self::tokio::{TcpListener, TcpStream, UdpSocket};

#[cfg(feature = "smol")]
pub use self::smol::{TcpListener, TcpStream, UdpSocket};
