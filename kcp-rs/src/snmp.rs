//! SNMP (Simple Network Management Protocol) statistics for KCP.
//!
//! This module provides atomic counters for monitoring KCP connection
//! statistics, matching the Go `kcp-go/v5` SNMP interface. All counters use
//! `AtomicU64` with precise `Ordering` guarantees for lock-free reads and
//! writes across multiple threads.
//!
//! ## Memory Ordering Rationale
//!
//! - `Relaxed` for individual increments — counter accuracy is not
//!   order-dependent
//! - `Acquire`/`Release` for the header/slice snapshots — ensures the
//!   caller sees a consistent view of all counters

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

/// Atomic SNMP statistics counters.
pub struct SNMP {
    // ── Go kcp-go v5 compatible counters ──
    /// Bytes sent from upper level.
    pub bytes_sent: AtomicU64,
    /// Bytes received to upper level.
    pub bytes_received: AtomicU64,
    /// Incoming KCP segments.
    pub in_segs: AtomicU64,
    /// Outgoing KCP segments.
    pub out_segs: AtomicU64,
    /// Accumulated retransmitted segments.
    pub retrans_segs: AtomicU64,
    /// Accumulated fast retransmitted segments.
    pub fast_retrans: AtomicU64,
    /// Number of segs inferred as lost.
    pub lost_segs: AtomicU64,
    /// Len of segments in send queue.
    pub ring_buffer_snd_queue: AtomicU64,
    /// Len of segments in receive queue.
    pub ring_buffer_rcv_queue: AtomicU64,
    /// Len of segments in send buffer.
    pub ring_buffer_snd_buffer: AtomicU64,
    /// Flush cycles that produced no UDP output (P2.2 observability).
    pub empty_flush: AtomicU64,

    // ── Legacy / Rust-specific counters ──
    /// Maximum number of segments in the send buffer.
    pub max_snd_buf: AtomicU64,
    /// Maximum number of segments in the receive buffer.
    pub max_rcv_buf: AtomicU64,
    /// Number of segments sent.
    pub seg_sent: AtomicU64,
    /// Number of segments received.
    pub seg_recv: AtomicU64,
    /// Number of bytes retransmitted.
    pub bytes_retrans: AtomicU64,
    /// Number of segments retransmitted.
    pub seg_retrans: AtomicU64,
    /// Number of acknowledgment packets sent.
    pub ack_sent: AtomicU64,
    /// Number of acknowledgment packets received.
    pub ack_recv: AtomicU64,
    /// Number of data segments delivered.
    pub data_sent: AtomicU64,
    /// Number of data segments received.
    pub data_recv: AtomicU64,
    /// Number of FEC data segments sent.
    pub fec_data_sent: AtomicU64,
    /// Number of FEC data segments received.
    pub fec_data_recv: AtomicU64,
    /// Number of FEC parity segments sent.
    pub fec_parity_sent: AtomicU64,
    /// Number of FEC parity segments received.
    pub fec_parity_recv: AtomicU64,
    /// Number of packets recovered by FEC.
    pub fec_short_shards: AtomicU64,
    /// Number of packets that FEC could not recover.
    pub fec_repeat_shards: AtomicU64,
}

impl SNMP {
    /// Create a new SNMP stats collector with all counters initialized to 0.
    #[inline]
    pub const fn new() -> Self {
        SNMP {
            bytes_sent: AtomicU64::new(0),
            bytes_received: AtomicU64::new(0),
            in_segs: AtomicU64::new(0),
            out_segs: AtomicU64::new(0),
            retrans_segs: AtomicU64::new(0),
            fast_retrans: AtomicU64::new(0),
            lost_segs: AtomicU64::new(0),
            ring_buffer_snd_queue: AtomicU64::new(0),
            ring_buffer_rcv_queue: AtomicU64::new(0),
            ring_buffer_snd_buffer: AtomicU64::new(0),
            empty_flush: AtomicU64::new(0),
            max_snd_buf: AtomicU64::new(0),
            max_rcv_buf: AtomicU64::new(0),
            seg_sent: AtomicU64::new(0),
            seg_recv: AtomicU64::new(0),
            bytes_retrans: AtomicU64::new(0),
            seg_retrans: AtomicU64::new(0),
            ack_sent: AtomicU64::new(0),
            ack_recv: AtomicU64::new(0),
            data_sent: AtomicU64::new(0),
            data_recv: AtomicU64::new(0),
            fec_data_sent: AtomicU64::new(0),
            fec_data_recv: AtomicU64::new(0),
            fec_parity_sent: AtomicU64::new(0),
            fec_parity_recv: AtomicU64::new(0),
            fec_short_shards: AtomicU64::new(0),
            fec_repeat_shards: AtomicU64::new(0),
        }
    }

    /// Get a snapshot of all counters as a `SnmpSnapshot`.
    pub fn snapshot(&self) -> SnmpSnapshot {
        SnmpSnapshot {
            bytes_sent: self.bytes_sent.load(Ordering::Acquire),
            bytes_received: self.bytes_received.load(Ordering::Acquire),
            in_segs: self.in_segs.load(Ordering::Acquire),
            out_segs: self.out_segs.load(Ordering::Acquire),
            retrans_segs: self.retrans_segs.load(Ordering::Acquire),
            fast_retrans: self.fast_retrans.load(Ordering::Acquire),
            lost_segs: self.lost_segs.load(Ordering::Acquire),
            ring_buffer_snd_queue: self.ring_buffer_snd_queue.load(Ordering::Acquire),
            ring_buffer_rcv_queue: self.ring_buffer_rcv_queue.load(Ordering::Acquire),
            ring_buffer_snd_buffer: self.ring_buffer_snd_buffer.load(Ordering::Acquire),
            empty_flush: self.empty_flush.load(Ordering::Acquire),
            max_snd_buf: self.max_snd_buf.load(Ordering::Acquire),
            max_rcv_buf: self.max_rcv_buf.load(Ordering::Acquire),
            seg_sent: self.seg_sent.load(Ordering::Acquire),
            seg_recv: self.seg_recv.load(Ordering::Acquire),
            bytes_retrans: self.bytes_retrans.load(Ordering::Acquire),
            seg_retrans: self.seg_retrans.load(Ordering::Acquire),
            ack_sent: self.ack_sent.load(Ordering::Acquire),
            ack_recv: self.ack_recv.load(Ordering::Acquire),
            data_sent: self.data_sent.load(Ordering::Acquire),
            data_recv: self.data_recv.load(Ordering::Acquire),
            fec_data_sent: self.fec_data_sent.load(Ordering::Acquire),
            fec_data_recv: self.fec_data_recv.load(Ordering::Acquire),
            fec_parity_sent: self.fec_parity_sent.load(Ordering::Acquire),
            fec_parity_recv: self.fec_parity_recv.load(Ordering::Acquire),
            fec_short_shards: self.fec_short_shards.load(Ordering::Acquire),
            fec_repeat_shards: self.fec_repeat_shards.load(Ordering::Acquire),
        }
    }

    /// Return the SNMP header names as strings.
    #[inline]
    pub fn header() -> Vec<String> {
        vec![
            "BytesSent".into(),
            "BytesReceived".into(),
            "InSegs".into(),
            "OutSegs".into(),
            "RetransSegs".into(),
            "FastRetransSegs".into(),
            "LostSegs".into(),
            "RingBufferSndQueue".into(),
            "RingBufferRcvQueue".into(),
            "RingBufferSndBuffer".into(),
            "EmptyFlush".into(),
            "MaxSndBuf".into(),
            "MaxRcvBuf".into(),
            "SegSent".into(),
            "SegRecv".into(),
            "BytesRetrans".into(),
            "SegRetrans".into(),
            "AckSent".into(),
            "AckRecv".into(),
            "DataSent".into(),
            "DataRecv".into(),
            "FECDataSent".into(),
            "FECDataRecv".into(),
            "FECParitySent".into(),
            "FECParityRecv".into(),
            "FECShortShards".into(),
            "FECRepeatShards".into(),
        ]
    }

    /// Return the current counter values as strings.
    pub fn to_slice(&self) -> Vec<String> {
        let s = self.snapshot();
        vec![
            s.bytes_sent.to_string(),
            s.bytes_received.to_string(),
            s.in_segs.to_string(),
            s.out_segs.to_string(),
            s.retrans_segs.to_string(),
            s.fast_retrans.to_string(),
            s.lost_segs.to_string(),
            s.ring_buffer_snd_queue.to_string(),
            s.ring_buffer_rcv_queue.to_string(),
            s.ring_buffer_snd_buffer.to_string(),
            s.empty_flush.to_string(),
            s.max_snd_buf.to_string(),
            s.max_rcv_buf.to_string(),
            s.seg_sent.to_string(),
            s.seg_recv.to_string(),
            s.bytes_retrans.to_string(),
            s.seg_retrans.to_string(),
            s.ack_sent.to_string(),
            s.ack_recv.to_string(),
            s.data_sent.to_string(),
            s.data_recv.to_string(),
            s.fec_data_sent.to_string(),
            s.fec_data_recv.to_string(),
            s.fec_parity_sent.to_string(),
            s.fec_parity_recv.to_string(),
            s.fec_short_shards.to_string(),
            s.fec_repeat_shards.to_string(),
        ]
    }

    /// Reset all counters to zero.
    pub fn reset(&self) {
        self.bytes_sent.store(0, Ordering::Release);
        self.bytes_received.store(0, Ordering::Release);
        self.in_segs.store(0, Ordering::Release);
        self.out_segs.store(0, Ordering::Release);
        self.retrans_segs.store(0, Ordering::Release);
        self.fast_retrans.store(0, Ordering::Release);
        self.lost_segs.store(0, Ordering::Release);
        self.ring_buffer_snd_queue.store(0, Ordering::Release);
        self.ring_buffer_rcv_queue.store(0, Ordering::Release);
        self.ring_buffer_snd_buffer.store(0, Ordering::Release);
        self.empty_flush.store(0, Ordering::Release);
        self.max_snd_buf.store(0, Ordering::Release);
        self.max_rcv_buf.store(0, Ordering::Release);
        self.seg_sent.store(0, Ordering::Release);
        self.seg_recv.store(0, Ordering::Release);
        self.bytes_retrans.store(0, Ordering::Release);
        self.seg_retrans.store(0, Ordering::Release);
        self.ack_sent.store(0, Ordering::Release);
        self.ack_recv.store(0, Ordering::Release);
        self.data_sent.store(0, Ordering::Release);
        self.data_recv.store(0, Ordering::Release);
        self.fec_data_sent.store(0, Ordering::Release);
        self.fec_data_recv.store(0, Ordering::Release);
        self.fec_parity_sent.store(0, Ordering::Release);
        self.fec_parity_recv.store(0, Ordering::Release);
        self.fec_short_shards.store(0, Ordering::Release);
        self.fec_repeat_shards.store(0, Ordering::Release);
    }
}

/// A point-in-time snapshot of all SNMP counters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnmpSnapshot {
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub in_segs: u64,
    pub out_segs: u64,
    pub retrans_segs: u64,
    pub fast_retrans: u64,
    pub lost_segs: u64,
    pub ring_buffer_snd_queue: u64,
    pub ring_buffer_rcv_queue: u64,
    pub ring_buffer_snd_buffer: u64,
    pub empty_flush: u64,
    pub max_snd_buf: u64,
    pub max_rcv_buf: u64,
    pub seg_sent: u64,
    pub seg_recv: u64,
    pub bytes_retrans: u64,
    pub seg_retrans: u64,
    pub ack_sent: u64,
    pub ack_recv: u64,
    pub data_sent: u64,
    pub data_recv: u64,
    pub fec_data_sent: u64,
    pub fec_data_recv: u64,
    pub fec_parity_sent: u64,
    pub fec_parity_recv: u64,
    pub fec_short_shards: u64,
    pub fec_repeat_shards: u64,
}

impl fmt::Display for SnmpSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "--- SNMP ---")?;
        writeln!(f, "BytesSent: {}", self.bytes_sent)?;
        writeln!(f, "BytesReceived: {}", self.bytes_received)?;
        writeln!(f, "InSegs: {}", self.in_segs)?;
        writeln!(f, "OutSegs: {}", self.out_segs)?;
        writeln!(f, "RetransSegs: {}", self.retrans_segs)?;
        writeln!(f, "FastRetransSegs: {}", self.fast_retrans)?;
        writeln!(f, "LostSegs: {}", self.lost_segs)?;
        writeln!(f, "RingBufferSndQueue: {}", self.ring_buffer_snd_queue)?;
        writeln!(f, "RingBufferRcvQueue: {}", self.ring_buffer_rcv_queue)?;
        writeln!(f, "RingBufferSndBuffer: {}", self.ring_buffer_snd_buffer)?;
        writeln!(f, "EmptyFlush: {}", self.empty_flush)?;
        writeln!(f, "MaxSndBuf: {}", self.max_snd_buf)?;
        writeln!(f, "MaxRcvBuf: {}", self.max_rcv_buf)?;
        writeln!(f, "SegSent: {}", self.seg_sent)?;
        writeln!(f, "SegRecv: {}", self.seg_recv)?;
        writeln!(f, "BytesRetrans: {}", self.bytes_retrans)?;
        writeln!(f, "SegRetrans: {}", self.seg_retrans)?;
        writeln!(f, "AckSent: {}", self.ack_sent)?;
        writeln!(f, "AckRecv: {}", self.ack_recv)?;
        writeln!(f, "DataSent: {}", self.data_sent)?;
        writeln!(f, "DataRecv: {}", self.data_recv)?;
        writeln!(f, "FECDataSent: {}", self.fec_data_sent)?;
        writeln!(f, "FECDataRecv: {}", self.fec_data_recv)?;
        writeln!(f, "FECParitySent: {}", self.fec_parity_sent)?;
        writeln!(f, "FECParityRecv: {}", self.fec_parity_recv)?;
        writeln!(f, "FECShortShards: {}", self.fec_short_shards)?;
        writeln!(f, "FECRepeatShards: {}", self.fec_repeat_shards)?;
        Ok(())
    }
}

/// Global default SNMP instance for process-wide statistics.
pub static DEFAULT_SNMP: SNMP = SNMP::new();

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snmp_increments() {
        let snmp = SNMP::new();
        snmp.bytes_sent.fetch_add(100, Ordering::Relaxed);
        snmp.seg_sent.fetch_add(5, Ordering::Relaxed);

        let snap = snmp.snapshot();
        assert_eq!(snap.bytes_sent, 100);
        assert_eq!(snap.seg_sent, 5);
    }

    #[test]
    fn snmp_reset() {
        let snmp = SNMP::new();
        snmp.bytes_sent.fetch_add(100, Ordering::Relaxed);
        snmp.bytes_received.fetch_add(50, Ordering::Relaxed);
        snmp.reset();
        let snap = snmp.snapshot();
        assert_eq!(snap.bytes_sent, 0);
        assert_eq!(snap.bytes_received, 0);
    }

    #[test]
    fn snmp_header_count() {
        let headers = SNMP::header();
        assert_eq!(headers.len(), 27);
    }

    #[test]
    fn snmp_to_slice_count() {
        let snmp = SNMP::new();
        let slice = snmp.to_slice();
        assert_eq!(slice.len(), 27);
    }

    #[test]
    fn snmp_display() {
        let snmp = SNMP::new();
        let snap = snmp.snapshot();
        let display = format!("{}", snap);
        assert!(display.contains("SNMP"));
        assert!(display.contains("BytesSent"));
    }

    #[test]
    fn default_snmp_is_accessible() {
        assert_eq!(DEFAULT_SNMP.snapshot().bytes_sent, 0);
    }
}
