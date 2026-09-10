//! Server socket creation: UDP socket with buffer size and DSCP.

use std::net::SocketAddr;

use anyhow::Result;
use log::warn;

/// Bind and tune a UDP fd without registering it with an async runtime.
///
/// Sharded server startup moves this raw fd into its dedicated local runtime
/// before calling `knet::UdpSocket::from_std`, preserving poller ownership.
pub(crate) fn create_udp_socket_std(
    addr: SocketAddr,
    sockbuf: u32,
    dscp: u32,
) -> Result<std::net::UdpSocket> {
    build_udp(addr, sockbuf, dscp, false)
}

/// SO_REUSEPORT variant of [`create_udp_socket_std`].
pub(crate) fn create_udp_socket_shard_std(
    addr: SocketAddr,
    sockbuf: u32,
    dscp: u32,
) -> Result<std::net::UdpSocket> {
    build_udp(addr, sockbuf, dscp, true)
}

fn build_udp(
    addr: SocketAddr,
    sockbuf: u32,
    dscp: u32,
    reuse_port: bool,
) -> Result<std::net::UdpSocket> {
    let socket = socket2::Socket::new(
        if addr.is_ipv4() {
            socket2::Domain::IPV4
        } else {
            socket2::Domain::IPV6
        },
        socket2::Type::DGRAM,
        None,
    )?;
    if let Err(e) = socket.set_recv_buffer_size(sockbuf as usize) {
        warn!("set_recv_buffer_size failed: {}", e);
    }
    if let Err(e) = socket.set_send_buffer_size(sockbuf as usize) {
        warn!("set_send_buffer_size failed: {}", e);
    }
    if reuse_port {
        // SO_REUSEPORT: allow N sockets to bind the same addr:port; the kernel
        // distributes inbound datagrams across them (Linux: by connection hash).
        // Not available on Windows — skip silently.
        #[cfg(unix)]
        if let Err(e) = socket.set_reuse_port(true) {
            warn!("set_reuse_port failed: {}", e);
        }
    }
    if dscp > 0 {
        // Go: IPv4 → IP_TOS = dscp << 2; IPv6 → IPV6_TCLASS = dscp (no shift).
        // socket2::set_tos maps to IP_TOS (IPv4) or IPV6_TCLASS (IPv6) on Linux.
        let tos = if addr.is_ipv4() { dscp << 2 } else { dscp };
        if let Err(e) = socket.set_tos(tos) {
            warn!("set_tos (DSCP) failed: {}", e);
        }
    }
    socket.bind(&addr.into())?;
    socket.set_nonblocking(true)?;
    Ok(socket.into())
}
