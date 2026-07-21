//! KCP wire-format segment definitions and zero-allocation pool.
//!
//! The segment header is 24 bytes on the wire:
//!
//! ```text
//! 0      4       8       12      16      20      24
//! +------+-------+-------+-------+-------+-------+
//! | conv | cmd   | frg   | wnd   | ts    | sn    |
//! +------+-------+-------+-------+-------+-------+
//! | una  | len   | data...                       |
//! +------+-------+                               |
//! |                                               |
//! +-----------------------------------------------+
//! ```
//!
//! All multibyte fields are **little-endian** on the wire.

use std::fmt;
use std::sync::atomic::{AtomicU32, Ordering};

use bytes::{BufMut, BytesMut};
use crossbeam::queue::SegQueue;

// ─── Wire constants ───────────────────────────────────────────────────────────

/// Maximum segment size (MTU-safe default).
/// Matches Go kcp-go `IKCP_MTU_DEF = 1400`.
pub const MTU: usize = 1400;

/// Overhead of the KCP header (24 bytes).
pub const KCP_OVERHEAD: usize = 24;

/// Minimum reliable window.
pub const KCP_MIN_WND: u32 = 32;

/// Default reliable window.
/// Matches Go kcp-go `IKCP_WND_SND = 32` / `IKCP_WND_RCV = 32`.
pub const KCP_DEFAULT_WND: u32 = 32;

/// Maximum reliable window.
pub const KCP_MAX_WND: u32 = 32768;

/// Maximum number of fragments per segment.
/// Matches Go kcp-go limit of 255 (uint8 max).
pub const KCP_MAX_FRAG: u32 = 255;

/// KCP command codes.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Push data.
    Push = 81, // 'Q' — command for IKCP_CMD_PUSH
    /// Acknowledgment.
    Ack = 82, // 'R' — command for IKCP_CMD_ACK
    /// Window probe.
    WAsk = 83, // 'S' — command for IKCP_CMD_WASK
    /// Window size advertisement.
    WIns = 84, // 'T' — command for IKCP_CMD_WINS
}

impl Command {
    #[inline]
    pub fn from_u8(v: u8) -> Option<Command> {
        match v {
            81 => Some(Command::Push),
            82 => Some(Command::Ack),
            83 => Some(Command::WAsk),
            84 => Some(Command::WIns),
            _ => None,
        }
    }
}

/// Flag: send window size advertisement (WIns) in next flush.
/// Set when a WAsk is received from the peer, or proactively when
/// our receive window opens up after being full.
/// Matches the original KCP C define `IKCP_ASK_TELL = 2`.
pub const KCP_ASK_TELL: u32 = 2;

/// Ask threshold for window probing.
pub const KCP_ASK_SEND: u32 = 1;

/// Data segment is ready to send.
pub const KCP_THRESHOLD_INIT: u32 = 2;

/// ─── Segment ─────────────────────────────────────────────────────────────────

/// A single KCP segment. Fields correspond directly to the wire header.
#[derive(Clone)]
pub struct Segment {
    /// Conversation ID.
    pub conv: u32,
    /// Command (PUSH, ACK, WASK, WINS).
    pub cmd: u8,
    /// Fragment count (for reassembly).
    pub frg: u8,
    /// Remote window size (advertised).
    pub wnd: u16,
    /// Timestamp.
    pub ts: u32,
    /// Sequence number.
    pub sn: u32,
    /// Unacknowledged sequence number.
    pub una: u32,
    /// Payload length.
    pub len: u32,
    /// Payload data.
    pub data: BytesMut,
    /// Number of retransmissions.
    pub resendts: u32,
    /// Retransmission timeout.
    pub rto: u32,
    /// Fast acknowledgment count.
    pub fastack: u32,
    /// Whether this segment has been acknowledged.
    pub acked: bool,
    /// Number of times this segment has been transmitted (0 = never sent).
    pub xmit: u32,
}

impl Segment {
    /// Create a new segment with the given capacity for payload data.
    #[inline]
    pub fn with_capacity(cap: usize) -> Self {
        Segment {
            conv: 0,
            cmd: 0,
            frg: 0,
            wnd: 0,
            ts: 0,
            sn: 0,
            una: 0,
            len: 0,
            data: BytesMut::with_capacity(cap),
            resendts: 0,
            rto: 0,
            fastack: 0,
            acked: false,
            xmit: 0,
        }
    }

    /// Reset all fields for re-use — avoids deallocation.
    #[inline]
    pub fn reset(&mut self) {
        self.conv = 0;
        self.cmd = 0;
        self.frg = 0;
        self.wnd = 0;
        self.ts = 0;
        self.sn = 0;
        self.una = 0;
        self.len = 0;
        self.data.clear();
        self.resendts = 0;
        self.rto = 0;
        self.fastack = 0;
        self.acked = false;
        self.xmit = 0;
    }

    /// Encode this segment into the provided `BufMut`.
    ///
    /// Returns the number of bytes written (header + payload).
    /// Header is written as a 24-byte little-endian block (P2.1 micro-opt).
    #[inline(always)]
    pub fn encode<B: BufMut>(&self, buf: &mut B) -> usize {
        let mut hdr = [0u8; KCP_OVERHEAD];
        hdr[0..4].copy_from_slice(&self.conv.to_le_bytes());
        hdr[4] = self.cmd;
        hdr[5] = self.frg;
        hdr[6..8].copy_from_slice(&self.wnd.to_le_bytes());
        hdr[8..12].copy_from_slice(&self.ts.to_le_bytes());
        hdr[12..16].copy_from_slice(&self.sn.to_le_bytes());
        hdr[16..20].copy_from_slice(&self.una.to_le_bytes());
        hdr[20..24].copy_from_slice(&self.len.to_le_bytes());
        buf.put_slice(&hdr);
        if self.len > 0 {
            buf.put_slice(&self.data[..self.len as usize]);
        }
        KCP_OVERHEAD + self.len as usize
    }

    /// Decode a segment header (and optionally payload) from the given bytes.
    ///
    /// Returns `None` if the input is too short to contain a header.
    pub fn decode(data: &[u8]) -> Option<(Segment, usize)> {
        if data.len() < KCP_OVERHEAD {
            return None;
        }
        let conv = u32::from_le_bytes(data[0..4].try_into().unwrap());
        let cmd = data[4];
        let frg = data[5];
        let wnd = u16::from_le_bytes(data[6..8].try_into().unwrap());
        let ts = u32::from_le_bytes(data[8..12].try_into().unwrap());
        let sn = u32::from_le_bytes(data[12..16].try_into().unwrap());
        let una = u32::from_le_bytes(data[16..20].try_into().unwrap());
        let len = u32::from_le_bytes(data[20..24].try_into().unwrap());

        let total_len = KCP_OVERHEAD + len as usize;
        if data.len() < total_len {
            return None;
        }

        let mut seg = Segment::with_capacity(len as usize);
        seg.conv = conv;
        seg.cmd = cmd;
        seg.frg = frg;
        seg.wnd = wnd;
        seg.ts = ts;
        seg.sn = sn;
        seg.una = una;
        seg.len = len;

        if len > 0 {
            seg.data.extend_from_slice(&data[KCP_OVERHEAD..total_len]);
        }

        Some((seg, total_len))
    }
}

impl fmt::Debug for Segment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Segment")
            .field("conv", &self.conv)
            .field("cmd", &self.cmd)
            .field("frg", &self.frg)
            .field("wnd", &self.wnd)
            .field("ts", &self.ts)
            .field("sn", &self.sn)
            .field("una", &self.una)
            .field("len", &self.len)
            .field("acked", &self.acked)
            .finish()
    }
}

// ─── Segment Pool ─────────────────────────────────────────────────────────────

/// A lock-free pool of [`Segment`] instances.
///
/// Go's `sync.Pool` is approximated here by `crossbeam::SegQueue` with an
/// upper bound. When the pool is empty, new segments are allocated directly —
/// the amortized cost is still near-zero under steady-state traffic because
/// segments are recycled as soon as they are acknowledged.
pub struct SegmentPool {
    inner: SegQueue<Segment>,
    max_capacity: usize,
    created: AtomicU32,
}

impl SegmentPool {
    /// Create a new pool with the given maximum capacity.
    #[inline]
    pub fn new(max_capacity: usize) -> Self {
        SegmentPool {
            inner: SegQueue::new(),
            max_capacity,
            created: AtomicU32::new(0),
        }
    }

    /// Acquire a segment from the pool, or allocate a fresh one.
    #[inline]
    pub fn acquire(&self) -> Segment {
        self.inner.pop().unwrap_or_else(|| {
            self.created.fetch_add(1, Ordering::Relaxed);
            Segment::with_capacity(MTU)
        })
    }

    /// Return a segment to the pool for reuse.
    #[inline]
    pub fn release(&self, mut seg: Segment) {
        seg.reset();
        // Avoid unbounded growth by dropping when at capacity.
        if self.inner.len() < self.max_capacity {
            self.inner.push(seg);
        }
    }

    /// Total number of segments created (useful for diagnostics).
    #[inline]
    pub fn created(&self) -> u32 {
        self.created.load(Ordering::Relaxed)
    }

    /// Approximate number of segments currently in the pool.
    #[inline]
    pub fn len(&self) -> usize {
        self.inner.len()
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segment_encode_decode_roundtrip() {
        let mut seg = Segment::with_capacity(32);
        seg.conv = 0xDEAD;
        seg.cmd = Command::Push as u8;
        seg.frg = 1;
        seg.wnd = 512;
        seg.ts = 1000;
        seg.sn = 42;
        seg.una = 10;
        seg.len = 4;
        seg.data.extend_from_slice(b"test");

        let mut buf = Vec::with_capacity(KCP_OVERHEAD + 4);
        seg.encode(&mut buf);

        let (decoded, consumed) = Segment::decode(&buf).unwrap();
        assert_eq!(consumed, KCP_OVERHEAD + 4);
        assert_eq!(decoded.conv, 0xDEAD);
        assert_eq!(decoded.cmd, Command::Push as u8);
        assert_eq!(decoded.frg, 1);
        assert_eq!(decoded.wnd, 512);
        assert_eq!(decoded.ts, 1000);
        assert_eq!(decoded.sn, 42);
        assert_eq!(decoded.una, 10);
        assert_eq!(decoded.len, 4);
        assert_eq!(&decoded.data[..4], b"test");
    }

    #[test]
    fn segment_decode_short_buffer() {
        assert!(Segment::decode(&[0u8; 10]).is_none());
    }

    #[test]
    fn segment_pool_reuses_segments() {
        let pool = SegmentPool::new(10);
        let seg = pool.acquire();
        assert_eq!(seg.len, 0);
        pool.release(seg);
        let seg2 = pool.acquire();
        assert_eq!(seg2.len, 0);
        assert_eq!(pool.created(), 1, "pool should reuse the released segment");
    }

    #[test]
    fn segment_pool_respects_capacity() {
        let pool = SegmentPool::new(2);
        let mut segs = Vec::new();
        for _ in 0..10 {
            segs.push(pool.acquire());
        }
        for s in segs {
            pool.release(s);
        }
        assert!(pool.len() <= 2, "pool should not exceed max_capacity");
    }

    #[test]
    fn command_from_u8() {
        assert_eq!(Command::from_u8(81), Some(Command::Push));
        assert_eq!(Command::from_u8(82), Some(Command::Ack));
        assert_eq!(Command::from_u8(83), Some(Command::WAsk));
        assert_eq!(Command::from_u8(84), Some(Command::WIns));
        assert_eq!(Command::from_u8(0), None);
    }

    #[test]
    fn segment_reset_clears_fields() {
        let mut seg = Segment::with_capacity(64);
        seg.conv = 1;
        seg.cmd = 81;
        seg.frg = 2;
        seg.wnd = 100;
        seg.ts = 500;
        seg.sn = 10;
        seg.una = 5;
        seg.len = 8;
        seg.data.extend_from_slice(b"12345678");
        seg.resendts = 1000;
        seg.rto = 200;
        seg.fastack = 3;
        seg.acked = true;

        seg.reset();

        assert_eq!(seg.conv, 0);
        assert_eq!(seg.len, 0);
        assert!(seg.data.is_empty());
        assert!(!seg.acked);
        // Ensure capacity is preserved
        assert!(seg.data.capacity() >= 64);
    }
}
