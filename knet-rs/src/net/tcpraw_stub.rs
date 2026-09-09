//! TCP raw socket stub for non-Linux platforms.
//!
//! Returns `Unsupported` errors for all operations — tcpraw requires
//! Linux raw sockets and TCP_REPAIR.

use std::io;
use std::net::SocketAddr;

/// Stub: tcpraw transport requires Linux.
pub struct TcpRawConn;

impl TcpRawConn {
    fn unsupported<T>() -> io::Result<T> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "tcpraw transport requires Linux (raw sockets + TCP_REPAIR)",
        ))
    }

    #[allow(unused)]
    pub fn send_to(&self, _buf: &[u8], _target: &SocketAddr) -> io::Result<usize> {
        Self::unsupported()
    }
    #[allow(unused)]
    pub fn send(&self, _buf: &[u8]) -> io::Result<usize> {
        Self::unsupported()
    }
    #[allow(unused)]
    pub fn try_recv_from(&self, _buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        Self::unsupported()
    }
    #[allow(unused)]
    pub fn try_recv(&self, _buf: &mut [u8]) -> io::Result<usize> {
        Self::unsupported()
    }
    #[allow(unused)]
    pub async fn recv_from(&self, _buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        Self::unsupported()
    }
    #[allow(unused)]
    pub async fn recv(&self, _buf: &mut [u8]) -> io::Result<usize> {
        Self::unsupported()
    }
    #[allow(unused)]
    pub async fn send_batch_to<B: AsRef<[u8]>>(
        &self,
        _bufs: &[B],
        _target: &SocketAddr,
    ) -> io::Result<()> {
        Self::unsupported()
    }
    pub fn try_recv_batch_from(
        &self,
        _packet_bufs: &mut [Vec<u8>],
        _out: &mut Vec<(Vec<u8>, SocketAddr)>,
    ) -> io::Result<usize> {
        Self::unsupported()
    }
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        Self::unsupported()
    }
    pub fn set_dscp(&self, _dscp: u32) -> io::Result<()> {
        Self::unsupported()
    }
}

/// Stub: tcpraw transport requires Linux.
pub struct TcpRawListener;

impl TcpRawListener {
    pub fn bind(_addr: &SocketAddr) -> io::Result<Self> {
        TcpRawConn::unsupported()
    }
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        TcpRawConn::unsupported()
    }
    pub fn set_dscp(&self, _dscp: u32) -> io::Result<()> {
        TcpRawConn::unsupported()
    }
    pub async fn accept(&self) -> io::Result<(TcpRawConn, SocketAddr)> {
        TcpRawConn::unsupported()
    }
}

/// Stub: tcpraw transport requires Linux.
pub fn dial(_remote_addr: &SocketAddr) -> io::Result<TcpRawConn> {
    TcpRawConn::unsupported()
}

/// Stub: tcpraw transport requires Linux.
pub fn listen(_addr: &SocketAddr) -> io::Result<TcpRawListener> {
    TcpRawListener::bind(_addr)
}
