//! KCP connection — wraps a KCP state machine + UDP socket for both client and
//! server side communication.
//!
//! This module provides the `UDPSession` which combines a `KCP` instance with
//! a raw UDP socket, feeding incoming data into the KCP state machine and
//! flushing outgoing data to the wire.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use parking_lot::RwLock;

use crate::kcp::{KcpError, KCP};

/// Shared KCP session state between read/write halves.
pub struct SessionInner {
    /// KCP state machine.
    pub kcp: RwLock<KCP>,
    /// Whether the session has been closed.
    pub closed: AtomicBool,
    /// Remote address.
    pub remote_addr: SocketAddr,
    /// Whether ACK nodelay is enabled.
    pub ack_nodelay: AtomicBool,
}

/// A KCP over UDP session.
///
/// `UDPSession` wraps a raw UDP socket with a KCP state machine. It handles:
/// - Reading incoming datagrams and feeding them to KCP
/// - Flushing KCP output to the wire
/// - Periodic updates for retransmission timing
pub struct UDPSession {
    inner: Arc<SessionInner>,
}

impl UDPSession {
    /// Create a new UDP session wrapping an existing KCP state machine.
    pub fn new(kcp: KCP, remote_addr: SocketAddr) -> Self {
        let inner = Arc::new(SessionInner {
            kcp: RwLock::new(kcp),
            closed: AtomicBool::new(false),
            remote_addr,
            ack_nodelay: AtomicBool::new(false),
        });

        UDPSession { inner }
    }

    /// Set stream mode on or off.
    #[inline]
    pub fn set_stream_mode(&self, enable: bool) {
        self.inner.kcp.write().set_stream_mode(enable);
    }

    /// Set ACK nodelay.
    #[inline]
    pub fn set_ack_nodelay(&self, enable: bool) {
        self.inner.ack_nodelay.store(enable, Ordering::Relaxed);
    }

    /// Set nodelay parameters.
    #[inline]
    pub fn set_nodelay(&self, nodelay: u32, interval: u32, resend: u32, nc: u32) {
        self.inner
            .kcp
            .write()
            .set_nodelay(nodelay, interval, resend, nc);
    }

    /// Set window sizes.
    #[inline]
    pub fn set_window_size(&self, snd_wnd: u32, rcv_wnd: u32) {
        let mut kcp = self.inner.kcp.write();
        kcp.set_snd_wnd(snd_wnd);
        kcp.set_rcv_wnd(rcv_wnd);
    }

    /// Set MTU.
    #[inline]
    pub fn set_mtu(&self, mtu: u32) {
        self.inner.kcp.write().set_mtu(mtu);
    }

    /// Set rate limit.
    #[inline]
    pub fn set_rate_limit(&self, _bytes_per_sec: u32) {
        // Rate limiting is not yet implemented at the KCP level.
        // This capability exists in the Go session layer (postProcess rate limiter).
    }

    /// Send data through the KCP connection.
    pub fn send(&self, data: &[u8]) -> Result<(), KcpError> {
        self.inner.kcp.write().send(data)
    }

    /// Receive data from the KCP connection.
    pub fn recv(&self) -> Result<bytes::BytesMut, KcpError> {
        self.inner.kcp.write().recv()
    }

    /// Check if the session is closed.
    #[inline]
    pub fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::Acquire)
    }

    /// Close the session.
    pub fn close(&self) {
        self.inner.closed.store(true, Ordering::Release);
    }

    /// Get the remote address.
    #[inline]
    pub fn remote_addr(&self) -> SocketAddr {
        self.inner.remote_addr
    }

    /// Perform a KCP update tick.
    pub fn update(&self, current_ms: u32) {
        self.inner.kcp.write().update(current_ms);
    }

    /// Force a KCP flush.
    pub fn flush(&self) {
        self.inner.kcp.write().flush();
    }

    /// Feed incoming raw data into the KCP state machine.
    pub fn input(&self, data: &[u8]) -> Result<usize, KcpError> {
        let ack_nodelay = self.inner.ack_nodelay.load(Ordering::Relaxed);
        self.inner.kcp.write().input(data, ack_nodelay)
    }

    /// Get a reference to the underlying KCP state machine (read lock).
    #[inline]
    pub fn kcp(&self) -> parking_lot::RwLockWriteGuard<'_, KCP> {
        self.inner.kcp.write()
    }

    /// Get a read lock on the KCP.
    #[inline]
    pub fn kcp_read(&self) -> parking_lot::RwLockReadGuard<'_, KCP> {
        self.inner.kcp.read()
    }
}

impl Drop for UDPSession {
    fn drop(&mut self) {
        self.close();
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_create_and_close() {
        let kcp = KCP::new(1, 0, Box::new(|_| {}));
        let addr: SocketAddr = "127.0.0.1:12345".parse().unwrap();
        let session = UDPSession::new(kcp, addr);
        assert!(!session.is_closed());
        assert_eq!(session.remote_addr(), addr);
        session.close();
        assert!(session.is_closed());
    }

    #[test]
    fn session_configure() {
        let kcp = KCP::new(1, 0, Box::new(|_| {}));
        let addr: SocketAddr = "127.0.0.1:12345".parse().unwrap();
        let session = UDPSession::new(kcp, addr);

        session.set_stream_mode(true);
        session.set_mtu(1400);
        session.set_nodelay(1, 10, 2, 1);
        session.set_window_size(256, 256);

        assert_eq!(session.kcp_read().mtu(), 1400);
    }
}
