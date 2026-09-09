//! Client socket creation: UDP and TCP raw.

use std::net::SocketAddr;
use std::sync::Arc;

use log::warn;

/// Create the datagram transport for a client connection, honoring `--tcp`.
///
/// TCP mode dials a Linux raw-TCP socket (tcpraw); UDP mode uses a plain UDP
/// socket. Mirrors Go kcptun's `dial()` which routes to `tcpraw.Dial` when
/// `config.TCP`. Both the initial dial and the reconnect path use this so a
/// `--tcp` session always re-dials TCP (never silently falling back to UDP).
pub(crate) fn create_client_socket(
    remote: SocketAddr,
    tcp: bool,
    sockbuf: u32,
    dscp: u32,
) -> anyhow::Result<Arc<knet::DatagramSocket>> {
    if tcp {
        #[cfg(not(target_os = "linux"))]
        anyhow::bail!("--tcp requires Linux (raw sockets + TCP_REPAIR)");
        #[cfg(target_os = "linux")]
        {
            let conn = knet::tcpraw_dial(&remote).map_err(|e| {
                anyhow::anyhow!(
                    "tcpraw dial to {}: {} (needs Linux + CAP_NET_RAW/ADMIN, server --tcp up)",
                    remote,
                    e
                )
            })?;
            if dscp > 0 {
                if let Err(e) = conn.set_dscp(dscp) {
                    log::warn!("SetDSCP({}) failed on tcpraw conn: {}", dscp, e);
                }
            }
            Ok(Arc::new(knet::DatagramSocket::TcpRaw(conn)))
        }
    } else {
        let socket = create_client_udp_socket(remote, sockbuf, dscp)?;
        Ok(Arc::new(knet::DatagramSocket::Udp(socket)))
    }
}

/// Create a connected UDP socket for a KCP client connection.
pub(crate) fn create_client_udp_socket(
    remote_addr: SocketAddr,
    sockbuf: u32,
    dscp: u32,
) -> std::io::Result<knet::UdpSocket> {
    let domain = if remote_addr.is_ipv4() {
        socket2::Domain::IPV4
    } else {
        socket2::Domain::IPV6
    };
    let socket = socket2::Socket::new(domain, socket2::Type::DGRAM, None)?;
    let buf_size = sockbuf as usize;
    let _ = socket.set_recv_buffer_size(buf_size);
    let _ = socket.set_send_buffer_size(buf_size);
    let _ = socket.set_reuse_address(true);
    if dscp > 0 {
        // Go: IPv4 → IP_TOS = dscp << 2; IPv6 → IPV6_TCLASS = dscp (no shift).
        // socket2::set_tos maps to IP_TOS (IPv4) or IPV6_TCLASS (IPv6) on Linux.
        let tos = if remote_addr.is_ipv4() {
            dscp << 2
        } else {
            dscp
        };
        if let Err(e) = socket.set_tos(tos) {
            warn!("set_tos (DSCP) failed for client socket: {}", e);
        }
    }
    socket.connect(&remote_addr.into())?;
    socket.set_nonblocking(true)?;
    knet::UdpSocket::from_std(socket.into())
}
