//! Datagram transport layer for the async `KcpStream`.
//!
//! [`PacketTransport`] is the pluggable packet-delivery abstraction under
//! [`crate::KcpStream`]; [`PeerTransport`] is the per-peer transport handed to
//! streams accepted by the shared-socket server demultiplexer in
//! [`crate::KcpListener`] (inbound datagrams are fed by the worker via
//! `feed_raw_batch`, not through the legacy [`PeerQueue`]).

use std::collections::VecDeque;
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use bytes::Bytes;
use parking_lot::Mutex;

/// Max UDP datagram size for the input-loop recv buffers and peer queues.
pub(crate) const MAX_DATAGRAM: usize = 2048;
/// Bound retained per-peer receive storage after a transient queue burst.
pub(crate) const MAX_RETAINED_PEER_BUFFERS: usize = 64;

// ─── PacketTransport ──────────────────────────────────────────────────────────

/// Pluggable datagram layer under [`crate::KcpStream`].
///
/// Implementations: [`knet::DatagramSocket`] (plain UDP / TcpRaw) and
/// `kcptun_common::CryptoTransport` (encrypt/decrypt wrapper).
///
/// Uses `#[async_trait]` so async methods are object-safe without hand-written
/// future return types.  Each implementation saves ~15 lines of
/// `Box::pin(async move { ... })` boilerplate.
#[async_trait::async_trait]
pub trait PacketTransport: Send + Sync {
    /// Read one datagram into `buf`. Returns bytes written.
    async fn recv(&self, buf: &mut [u8]) -> io::Result<usize>;

    /// Non-blocking read; `WouldBlock` when nothing ready.
    fn try_recv(&self, buf: &mut [u8]) -> io::Result<usize>;

    /// Decrypt a single raw datagram **in place**, without going through the
    /// internal queue. Returns the plaintext length, or 0 on bad packets.
    ///
    /// The default implementation is identity (no crypto). `CryptoTransport`
    /// overrides this to call its decrypt path directly, avoiding the
    /// push/pop queue round-trip when the worker processes packets serially.
    fn decrypt_packet_in_place(&self, buf: &mut [u8], n: usize) -> usize {
        let _ = buf;
        n
    }

    /// Read one datagram into reusable owned storage.
    ///
    /// The default delegates to [`recv`](Self::recv). Queue-backed transports
    /// may override this to transfer packet ownership without another copy.
    async fn recv_vec(&self, buf: &mut Vec<u8>) -> io::Result<usize> {
        let n = self.recv(buf.as_mut_slice()).await?;
        buf.truncate(n);
        Ok(n)
    }

    /// Non-blocking counterpart to [`recv_vec`](Self::recv_vec).
    fn try_recv_vec(&self, buf: &mut Vec<u8>) -> io::Result<usize> {
        let n = self.try_recv(buf.as_mut_slice())?;
        buf.truncate(n);
        Ok(n)
    }

    /// Batch-send on a connected socket.
    async fn send_batch(&self, packets: &[Bytes]) -> io::Result<()>;

    /// Batch-send to an explicit peer (unconnected socket).
    async fn send_batch_to(&self, packets: &[Bytes], target: SocketAddr) -> io::Result<()>;

    /// High-priority send (ACK path). Default = [`send_batch`](Self::send_batch).
    /// Crypto wrappers use a separate buffer here to avoid lock contention.
    async fn send_urgent(&self, packets: &[Bytes]) -> io::Result<()> {
        self.send_batch(packets).await
    }

    /// High-priority send_to (ACK path, unconnected). Default = send_batch_to.
    async fn send_urgent_to(&self, packets: &[Bytes], target: SocketAddr) -> io::Result<()> {
        self.send_batch_to(packets, target).await
    }

    /// Non-blocking batch send (connected socket). Returns the number of
    /// datagrams handed to the kernel, stopping at the first `WouldBlock`
    /// (socket send buffer full); the caller must re-queue `packets[sent..]`
    /// for a later send (e.g. via the flush loop).
    ///
    /// Default: unavailable → `Err(WouldBlock)`, so callers fall back to the
    /// async flush-loop path (existing behavior). `knet::DatagramSocket`
    /// overrides this with a real non-blocking send.
    fn try_send_batch(&self, _packets: &[Bytes]) -> io::Result<usize> {
        Err(io::Error::from(io::ErrorKind::WouldBlock))
    }

    /// Non-blocking batch send to an explicit peer (unconnected socket).
    /// Returns the number of datagrams handed to the kernel, stopping at the
    /// first `WouldBlock` (socket send buffer full); the caller must re-queue
    /// `packets[sent..]` for a later send.
    ///
    /// Default: unavailable → `Err(WouldBlock)`, so callers fall back to the
    /// async flush-loop path (existing behavior). `knet::DatagramSocket`
    /// overrides this with a real non-blocking send.
    fn try_send_batch_to(&self, _packets: &[Bytes], _target: SocketAddr) -> io::Result<usize> {
        Err(io::Error::from(io::ErrorKind::WouldBlock))
    }

    /// Non-blocking batch receive into the caller's buffer pool. Returns the
    /// number of datagrams received. Default: one via [`try_recv`](Self::try_recv)
    /// into `pool[0]`.
    fn try_recv_batch(&self, pool: &mut [Vec<u8>]) -> io::Result<usize> {
        if pool.is_empty() {
            return Ok(0);
        }
        match self.try_recv(&mut pool[0]) {
            Ok(n) if n > 0 => {
                pool[0].truncate(n);
                Ok(1)
            }
            Ok(_) => Ok(0),
            Err(e) => Err(e),
        }
    }

    /// Whether [`try_recv_batch`](Self::try_recv_batch) can receive multiple
    /// datagrams per call (vs. the default single). The input loop uses this to
    /// switch to the batch drain (recvmmsg on Linux).
    fn supports_recv_batch(&self) -> bool {
        false
    }

    fn local_addr(&self) -> io::Result<SocketAddr>;

    /// Set the TTL (hop limit) on the underlying socket.
    /// Default: `Unsupported` (no raw socket to configure).
    fn set_ttl(&self, _ttl: u32) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "set_ttl not available on this transport",
        ))
    }

    /// Get the TTL (hop limit) from the underlying socket.
    /// Default: `Unsupported` (no raw socket to query).
    fn ttl(&self) -> io::Result<u32> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "ttl not available on this transport",
        ))
    }
}

#[async_trait::async_trait]
impl PacketTransport for knet::DatagramSocket {
    async fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        // Call inherent method (not trait) to avoid recursion.
        knet::DatagramSocket::recv(self, buf).await
    }

    fn try_recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        knet::DatagramSocket::try_recv(self, buf)
    }

    async fn send_batch(&self, packets: &[Bytes]) -> io::Result<()> {
        knet::DatagramSocket::send_batch(self, packets).await
    }

    async fn send_batch_to(&self, packets: &[Bytes], target: SocketAddr) -> io::Result<()> {
        knet::DatagramSocket::send_batch_to(self, packets, target).await
    }

    fn try_send_batch(&self, packets: &[Bytes]) -> io::Result<usize> {
        knet::DatagramSocket::try_send_batch(self, packets)
    }

    fn try_send_batch_to(&self, packets: &[Bytes], target: SocketAddr) -> io::Result<usize> {
        knet::DatagramSocket::try_send_batch_to(self, packets, target)
    }

    fn try_recv_batch(&self, pool: &mut [Vec<u8>]) -> io::Result<usize> {
        knet::DatagramSocket::try_recv_batch(self, pool)
    }

    fn supports_recv_batch(&self) -> bool {
        true
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        knet::DatagramSocket::local_addr(self)
    }

    fn set_ttl(&self, ttl: u32) -> io::Result<()> {
        knet::DatagramSocket::set_ttl(self, ttl)
    }

    fn ttl(&self) -> io::Result<u32> {
        knet::DatagramSocket::ttl(self)
    }
}

// ─── Per-peer queue + transport (KcpListener demux) ───────────────────────────

/// FIFO of inbound datagrams for a single peer.
///
/// **Historical note:** this queue once fed each accepted peer's input loop
/// (`PeerTransport::recv`). The sharded worker pipeline now feeds datagrams
/// directly via `feed_raw_batch`, so nothing pushes into the queue in
/// production; only the pop side remains as the trait's required receive
/// implementations and returns `WouldBlock` / blocks on a permanently empty
/// queue. Kept because `PeerTransport` must implement `PacketTransport`.
#[allow(dead_code)]
pub(crate) struct PeerQueue {
    buffers: Mutex<PeerBuffers>,
    notify: knet::Notify,
    closed: AtomicBool,
}

struct PeerBuffers {
    packets: VecDeque<Vec<u8>>,
    spare: Vec<Vec<u8>>,
}

#[allow(dead_code)]
impl PeerQueue {
    pub(crate) fn new() -> Self {
        // Keep only a tiny spare-vector index up front. Packet-sized buffers
        // are allocated lazily as traffic arrives, avoiding 128KiB of eager
        // storage for every idle peer; the recycle cap still bounds retained
        // memory after bursts.
        let spare = Vec::with_capacity(2);
        Self {
            buffers: Mutex::new(PeerBuffers {
                packets: VecDeque::new(),
                spare,
            }),
            notify: knet::Notify::new(),
            closed: AtomicBool::new(false),
        }
    }

    fn pop(&self) -> Option<Vec<u8>> {
        let mut buffers = self.buffers.lock();
        buffers.packets.pop_front()
    }

    /// Move a queued datagram into the consumer buffer and recycle the
    /// consumer's previous allocation back to the listener.
    fn pop_into(&self, buf: &mut Vec<u8>) -> Option<usize> {
        let mut buffers = self.buffers.lock();
        let mut pkt = buffers.packets.pop_front()?;
        std::mem::swap(buf, &mut pkt);
        let n = buf.len();
        if buffers.spare.len() < MAX_RETAINED_PEER_BUFFERS {
            pkt.resize(MAX_DATAGRAM, 0);
            buffers.spare.push(pkt);
        }
        Some(n)
    }

    /// Pop up to `pool.len()` queued datagrams under **one lock**, swapping each
    /// consumer buffer into the queue's spare pool (recycles capacity). Returns
    /// the number popped; `WouldBlock` when the queue is empty. Keeps queued
    /// order, so a peer's input loop drains a whole burst and batches its ACKs.
    fn pop_batch(&self, pool: &mut [Vec<u8>]) -> io::Result<usize> {
        let mut buffers = self.buffers.lock();
        let mut n = 0;
        while n < pool.len() {
            let mut pkt = match buffers.packets.pop_front() {
                Some(p) => p,
                None => break,
            };
            std::mem::swap(&mut pool[n], &mut pkt);
            if buffers.spare.len() < MAX_RETAINED_PEER_BUFFERS {
                pkt.resize(MAX_DATAGRAM, 0);
                buffers.spare.push(pkt);
            }
            n += 1;
        }
        if n == 0 {
            Err(io::Error::from(io::ErrorKind::WouldBlock))
        } else {
            Ok(n)
        }
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    pub(crate) fn mark_closed(&self) {
        self.closed.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }
}

/// `PacketTransport` for one accepted peer: reads inbound from its
/// [`PeerQueue`] and writes outbound on the shared listen socket addressed to
/// that peer.
///
/// Dropping the transport (i.e. dropping the accepted `KcpStream`) closes the
/// peer queue so the listener reaps it and can accept a fresh connection from
/// the same address.
pub(crate) struct PeerTransport {
    pub(crate) queue: Arc<PeerQueue>,
    pub(crate) socket: Arc<knet::DatagramSocket>,
    pub(crate) peer: SocketAddr,
}

#[async_trait::async_trait]
impl PacketTransport for PeerTransport {
    async fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            if let Some(pkt) = self.queue.pop() {
                let n = pkt.len().min(buf.len());
                buf[..n].copy_from_slice(&pkt[..n]);
                return Ok(n);
            }
            if self.queue.is_closed() {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionReset,
                    "KcpStream: peer session closed",
                ));
            }
            // Arm the notification, then re-check to close the wake race.
            let notified = self.queue.notify.notified();
            if let Some(pkt) = self.queue.pop() {
                let n = pkt.len().min(buf.len());
                buf[..n].copy_from_slice(&pkt[..n]);
                return Ok(n);
            }
            if self.queue.is_closed() {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionReset,
                    "KcpStream: peer session closed",
                ));
            }
            notified.await;
        }
    }

    fn try_recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        match self.queue.pop() {
            Some(pkt) => {
                let n = pkt.len().min(buf.len());
                buf[..n].copy_from_slice(&pkt[..n]);
                Ok(n)
            }
            None => Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "peer queue empty",
            )),
        }
    }

    async fn recv_vec(&self, buf: &mut Vec<u8>) -> io::Result<usize> {
        loop {
            if let Some(n) = self.queue.pop_into(buf) {
                return Ok(n);
            }
            if self.queue.is_closed() {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionReset,
                    "KcpStream: peer session closed",
                ));
            }
            let notified = self.queue.notify.notified();
            if let Some(n) = self.queue.pop_into(buf) {
                return Ok(n);
            }
            if self.queue.is_closed() {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionReset,
                    "KcpStream: peer session closed",
                ));
            }
            notified.await;
        }
    }

    fn try_recv_vec(&self, buf: &mut Vec<u8>) -> io::Result<usize> {
        self.queue
            .pop_into(buf)
            .ok_or_else(|| io::Error::new(io::ErrorKind::WouldBlock, "peer queue empty"))
    }

    fn try_recv_batch(&self, pool: &mut [Vec<u8>]) -> io::Result<usize> {
        self.queue.pop_batch(pool)
    }

    fn supports_recv_batch(&self) -> bool {
        true
    }

    async fn send_batch(&self, packets: &[Bytes]) -> io::Result<()> {
        self.socket.send_batch_to(packets, self.peer).await
    }

    async fn send_batch_to(&self, packets: &[Bytes], _target: SocketAddr) -> io::Result<()> {
        self.socket.send_batch_to(packets, self.peer).await
    }

    fn try_send_batch_to(&self, packets: &[Bytes], _target: SocketAddr) -> io::Result<usize> {
        self.socket.try_send_batch_to(packets, self.peer)
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }
}

impl Drop for PeerTransport {
    fn drop(&mut self) {
        self.queue.mark_closed();
    }
}

/// Optional per-accepted-peer transport wrapper applied by
/// [`crate::KcpListenerBuilder`] (e.g. adding encryption while retaining the
/// listener's single shared-socket reader).
pub(crate) type TransportWrapper =
    Arc<dyn Fn(Arc<dyn PacketTransport>, SocketAddr) -> Arc<dyn PacketTransport> + Send + Sync>;

#[cfg(test)]
mod tests {
    use super::*;

    /// Idle peers must not preallocate packet buffers.
    #[test]
    fn peer_queue_spares_are_lazy() {
        let q = PeerQueue::new();
        let buffers = q.buffers.lock();
        assert!(
            buffers.spare.is_empty(),
            "idle peers must not preallocate packet buffers"
        );
    }
}
