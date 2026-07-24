//! SNMP statistics for KCP — wire-compatible with Go `kcp-go/v5` DefaultSnmp.
//!
//! Field names and `Header()` / `ToSlice()` order match Go's `snmp.go` so
//! `--snmplog` CSV is interchangeable with Go kcptun. Counters use
//! `AtomicU64` with `Relaxed` increments and `Acquire` snapshot loads.
//!
//! Rust-only: [`SNMP::empty_flush`] is kept for flush-loop observability but
//! is **not** part of the Go CSV header.

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// When false (default), all counter updates are no-ops for zero hot-path cost.
/// Enabled only when `--snmplog` is configured with a positive period.
static SNMP_ENABLED: AtomicBool = AtomicBool::new(false);

/// Enable process-wide SNMP collection (call once when starting snmp_logger).
#[inline]
pub fn enable() {
    SNMP_ENABLED.store(true, Ordering::Release);
}

/// Whether SNMP collection is active.
#[inline(always)]
pub(crate) fn is_enabled() -> bool {
    SNMP_ENABLED.load(Ordering::Relaxed)
}

/// Gated `fetch_add` — compiles to a single load+branch when disabled.
#[inline(always)]
pub fn add(counter: &AtomicU64, n: u64) {
    if is_enabled() {
        counter.fetch_add(n, Ordering::Relaxed);
    }
}

/// Gated `store`.
#[inline(always)]
pub fn store(counter: &AtomicU64, n: u64) {
    if is_enabled() {
        counter.store(n, Ordering::Relaxed);
    }
}

/// Atomic SNMP statistics counters (Go kcp-go v5 layout + Rust empty_flush).
pub struct SNMP {
    // ── Go kcp-go v5 ──
    /// Bytes sent from upper level (`KCP::send`).
    pub bytes_sent: AtomicU64,
    /// Bytes received to upper level (`KCP::recv` / `recv_bytes`).
    pub bytes_received: AtomicU64,
    /// Max number of connections ever reached.
    pub max_conn: AtomicU64,
    /// Accumulated active open connections (client).
    pub active_opens: AtomicU64,
    /// Accumulated passive open connections (server).
    pub passive_opens: AtomicU64,
    /// Current number of established connections.
    pub curr_estab: AtomicU64,
    /// UDP read errors reported from the packet socket.
    pub in_errs: AtomicU64,
    /// Checksum errors from CRC32 (crypto header).
    pub in_csum_errors: AtomicU64,
    /// Packet input errors reported from KCP.
    pub kcp_in_errors: AtomicU64,
    /// Incoming UDP packets count.
    pub in_pkts: AtomicU64,
    /// Outgoing UDP packets count.
    pub out_pkts: AtomicU64,
    /// Incoming KCP segments.
    pub in_segs: AtomicU64,
    /// Outgoing KCP segments.
    pub out_segs: AtomicU64,
    /// UDP bytes received.
    pub in_bytes: AtomicU64,
    /// UDP bytes sent.
    pub out_bytes: AtomicU64,
    /// Accumulated retransmitted segments (lost + fast + early).
    pub retrans_segs: AtomicU64,
    /// Accumulated fast retransmitted segments.
    pub fast_retrans: AtomicU64,
    /// Accumulated early retransmitted segments.
    pub early_retrans: AtomicU64,
    /// Number of segs inferred as lost.
    pub lost_segs: AtomicU64,
    /// Number of segs duplicated.
    pub repeat_segs: AtomicU64,
    /// Number of FEC segments that are full (ready to recover).
    pub fec_full_shards: AtomicU64,
    /// FEC parity shards received.
    pub fec_parity_shards: AtomicU64,
    /// Incorrect packets recovered from FEC.
    pub fec_errs: AtomicU64,
    /// Correct packets recovered from FEC.
    pub fec_recovered: AtomicU64,
    /// Number of parity shards that are not yet received (shard set size).
    pub fec_shard_set: AtomicU64,
    /// The newest / min tracking id of FEC shards (Go: FECShardMin).
    pub fec_shard_min: AtomicU64,
    /// Len of segments in send queue.
    pub ring_buffer_snd_queue: AtomicU64,
    /// Len of segments in receive queue.
    pub ring_buffer_rcv_queue: AtomicU64,
    /// Len of segments in send buffer.
    pub ring_buffer_snd_buffer: AtomicU64,

    // ── Rust-only (not in Go CSV) ──
    /// Flush cycles that produced no UDP output (P2.2 observability).
    pub empty_flush: AtomicU64,
}

impl SNMP {
    /// Create a new SNMP stats collector with all counters initialized to 0.
    #[inline]
    pub const fn new() -> Self {
        SNMP {
            bytes_sent: AtomicU64::new(0),
            bytes_received: AtomicU64::new(0),
            max_conn: AtomicU64::new(0),
            active_opens: AtomicU64::new(0),
            passive_opens: AtomicU64::new(0),
            curr_estab: AtomicU64::new(0),
            in_errs: AtomicU64::new(0),
            in_csum_errors: AtomicU64::new(0),
            kcp_in_errors: AtomicU64::new(0),
            in_pkts: AtomicU64::new(0),
            out_pkts: AtomicU64::new(0),
            in_segs: AtomicU64::new(0),
            out_segs: AtomicU64::new(0),
            in_bytes: AtomicU64::new(0),
            out_bytes: AtomicU64::new(0),
            retrans_segs: AtomicU64::new(0),
            fast_retrans: AtomicU64::new(0),
            early_retrans: AtomicU64::new(0),
            lost_segs: AtomicU64::new(0),
            repeat_segs: AtomicU64::new(0),
            fec_full_shards: AtomicU64::new(0),
            fec_parity_shards: AtomicU64::new(0),
            fec_errs: AtomicU64::new(0),
            fec_recovered: AtomicU64::new(0),
            fec_shard_set: AtomicU64::new(0),
            fec_shard_min: AtomicU64::new(0),
            ring_buffer_snd_queue: AtomicU64::new(0),
            ring_buffer_rcv_queue: AtomicU64::new(0),
            ring_buffer_snd_buffer: AtomicU64::new(0),
            empty_flush: AtomicU64::new(0),
        }
    }

    /// Point-in-time snapshot of all counters (including Rust-only fields).
    pub(crate) fn snapshot(&self) -> SnmpSnapshot {
        SnmpSnapshot {
            bytes_sent: self.bytes_sent.load(Ordering::Acquire),
            bytes_received: self.bytes_received.load(Ordering::Acquire),
            max_conn: self.max_conn.load(Ordering::Acquire),
            active_opens: self.active_opens.load(Ordering::Acquire),
            passive_opens: self.passive_opens.load(Ordering::Acquire),
            curr_estab: self.curr_estab.load(Ordering::Acquire),
            in_errs: self.in_errs.load(Ordering::Acquire),
            in_csum_errors: self.in_csum_errors.load(Ordering::Acquire),
            kcp_in_errors: self.kcp_in_errors.load(Ordering::Acquire),
            in_pkts: self.in_pkts.load(Ordering::Acquire),
            out_pkts: self.out_pkts.load(Ordering::Acquire),
            in_segs: self.in_segs.load(Ordering::Acquire),
            out_segs: self.out_segs.load(Ordering::Acquire),
            in_bytes: self.in_bytes.load(Ordering::Acquire),
            out_bytes: self.out_bytes.load(Ordering::Acquire),
            retrans_segs: self.retrans_segs.load(Ordering::Acquire),
            fast_retrans: self.fast_retrans.load(Ordering::Acquire),
            early_retrans: self.early_retrans.load(Ordering::Acquire),
            lost_segs: self.lost_segs.load(Ordering::Acquire),
            repeat_segs: self.repeat_segs.load(Ordering::Acquire),
            fec_full_shards: self.fec_full_shards.load(Ordering::Acquire),
            fec_parity_shards: self.fec_parity_shards.load(Ordering::Acquire),
            fec_errs: self.fec_errs.load(Ordering::Acquire),
            fec_recovered: self.fec_recovered.load(Ordering::Acquire),
            fec_shard_set: self.fec_shard_set.load(Ordering::Acquire),
            fec_shard_min: self.fec_shard_min.load(Ordering::Acquire),
            ring_buffer_snd_queue: self.ring_buffer_snd_queue.load(Ordering::Acquire),
            ring_buffer_rcv_queue: self.ring_buffer_rcv_queue.load(Ordering::Acquire),
            ring_buffer_snd_buffer: self.ring_buffer_snd_buffer.load(Ordering::Acquire),
            empty_flush: self.empty_flush.load(Ordering::Acquire),
        }
    }

    /// Go-compatible CSV header names (exact order of kcp-go `Snmp.Header()`).
    #[inline]
    pub fn header() -> Vec<String> {
        vec![
            "BytesSent".into(),
            "BytesReceived".into(),
            "MaxConn".into(),
            "ActiveOpens".into(),
            "PassiveOpens".into(),
            "CurrEstab".into(),
            "InErrs".into(),
            "InCsumErrors".into(),
            "KCPInErrors".into(),
            "InPkts".into(),
            "OutPkts".into(),
            "InSegs".into(),
            "OutSegs".into(),
            "InBytes".into(),
            "OutBytes".into(),
            "RetransSegs".into(),
            "FastRetransSegs".into(),
            "EarlyRetransSegs".into(),
            "LostSegs".into(),
            "RepeatSegs".into(),
            "FECFullShards".into(),
            "FECParityShards".into(),
            "FECErrs".into(),
            "FECRecovered".into(),
            "FECShardSet".into(),
            "FECShardMin".into(),
            "RingBufferSndQueue".into(),
            "RingBufferRcvQueue".into(),
            "RingBufferSndBuffer".into(),
        ]
    }

    /// Go-compatible CSV values (exact order of kcp-go `Snmp.ToSlice()`).
    pub fn to_slice(&self) -> Vec<String> {
        let s = self.snapshot();
        vec![
            s.bytes_sent.to_string(),
            s.bytes_received.to_string(),
            s.max_conn.to_string(),
            s.active_opens.to_string(),
            s.passive_opens.to_string(),
            s.curr_estab.to_string(),
            s.in_errs.to_string(),
            s.in_csum_errors.to_string(),
            s.kcp_in_errors.to_string(),
            s.in_pkts.to_string(),
            s.out_pkts.to_string(),
            s.in_segs.to_string(),
            s.out_segs.to_string(),
            s.in_bytes.to_string(),
            s.out_bytes.to_string(),
            s.retrans_segs.to_string(),
            s.fast_retrans.to_string(),
            s.early_retrans.to_string(),
            s.lost_segs.to_string(),
            s.repeat_segs.to_string(),
            s.fec_full_shards.to_string(),
            s.fec_parity_shards.to_string(),
            s.fec_errs.to_string(),
            s.fec_recovered.to_string(),
            s.fec_shard_set.to_string(),
            s.fec_shard_min.to_string(),
            s.ring_buffer_snd_queue.to_string(),
            s.ring_buffer_rcv_queue.to_string(),
            s.ring_buffer_snd_buffer.to_string(),
        ]
    }

    /// Reset all counters to zero (including Rust-only fields).
    pub fn reset(&self) {
        self.bytes_sent.store(0, Ordering::Release);
        self.bytes_received.store(0, Ordering::Release);
        self.max_conn.store(0, Ordering::Release);
        self.active_opens.store(0, Ordering::Release);
        self.passive_opens.store(0, Ordering::Release);
        self.curr_estab.store(0, Ordering::Release);
        self.in_errs.store(0, Ordering::Release);
        self.in_csum_errors.store(0, Ordering::Release);
        self.kcp_in_errors.store(0, Ordering::Release);
        self.in_pkts.store(0, Ordering::Release);
        self.out_pkts.store(0, Ordering::Release);
        self.in_segs.store(0, Ordering::Release);
        self.out_segs.store(0, Ordering::Release);
        self.in_bytes.store(0, Ordering::Release);
        self.out_bytes.store(0, Ordering::Release);
        self.retrans_segs.store(0, Ordering::Release);
        self.fast_retrans.store(0, Ordering::Release);
        self.early_retrans.store(0, Ordering::Release);
        self.lost_segs.store(0, Ordering::Release);
        self.repeat_segs.store(0, Ordering::Release);
        self.fec_full_shards.store(0, Ordering::Release);
        self.fec_parity_shards.store(0, Ordering::Release);
        self.fec_errs.store(0, Ordering::Release);
        self.fec_recovered.store(0, Ordering::Release);
        self.fec_shard_set.store(0, Ordering::Release);
        self.fec_shard_min.store(0, Ordering::Release);
        self.ring_buffer_snd_queue.store(0, Ordering::Release);
        self.ring_buffer_rcv_queue.store(0, Ordering::Release);
        self.ring_buffer_snd_buffer.store(0, Ordering::Release);
        self.empty_flush.store(0, Ordering::Release);
    }

    /// Record a new established session. `active=true` for client dial.
    pub fn session_opened(&self, active: bool) {
        if !is_enabled() {
            return;
        }
        if active {
            self.active_opens.fetch_add(1, Ordering::Relaxed);
        } else {
            self.passive_opens.fetch_add(1, Ordering::Relaxed);
        }
        let cur = self.curr_estab.fetch_add(1, Ordering::Relaxed) + 1;
        // CAS loop for max_conn (matching Go CompareAndSwap).
        let mut max = self.max_conn.load(Ordering::Relaxed);
        while cur > max {
            match self
                .max_conn
                .compare_exchange(max, cur, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => break,
                Err(actual) => max = actual,
            }
        }
    }

    /// Record session close (decrement CurrEstab).
    pub fn session_closed(&self) {
        if !is_enabled() {
            return;
        }
        // wrap is fine if mismatched open/close (same as Go ^uint64(0) add).
        self.curr_estab.fetch_sub(1, Ordering::Relaxed);
    }
}

/// A point-in-time snapshot of all SNMP counters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SnmpSnapshot {
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub max_conn: u64,
    pub active_opens: u64,
    pub passive_opens: u64,
    pub curr_estab: u64,
    pub in_errs: u64,
    pub in_csum_errors: u64,
    pub kcp_in_errors: u64,
    pub in_pkts: u64,
    pub out_pkts: u64,
    pub in_segs: u64,
    pub out_segs: u64,
    pub in_bytes: u64,
    pub out_bytes: u64,
    pub retrans_segs: u64,
    pub fast_retrans: u64,
    pub early_retrans: u64,
    pub lost_segs: u64,
    pub repeat_segs: u64,
    pub fec_full_shards: u64,
    pub fec_parity_shards: u64,
    pub fec_errs: u64,
    pub fec_recovered: u64,
    pub fec_shard_set: u64,
    pub fec_shard_min: u64,
    pub ring_buffer_snd_queue: u64,
    pub ring_buffer_rcv_queue: u64,
    pub ring_buffer_snd_buffer: u64,
    pub empty_flush: u64,
}

impl fmt::Display for SnmpSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "--- SNMP ---")?;
        writeln!(f, "BytesSent: {}", self.bytes_sent)?;
        writeln!(f, "BytesReceived: {}", self.bytes_received)?;
        writeln!(f, "MaxConn: {}", self.max_conn)?;
        writeln!(f, "ActiveOpens: {}", self.active_opens)?;
        writeln!(f, "PassiveOpens: {}", self.passive_opens)?;
        writeln!(f, "CurrEstab: {}", self.curr_estab)?;
        writeln!(f, "InErrs: {}", self.in_errs)?;
        writeln!(f, "InCsumErrors: {}", self.in_csum_errors)?;
        writeln!(f, "KCPInErrors: {}", self.kcp_in_errors)?;
        writeln!(f, "InPkts: {}", self.in_pkts)?;
        writeln!(f, "OutPkts: {}", self.out_pkts)?;
        writeln!(f, "InSegs: {}", self.in_segs)?;
        writeln!(f, "OutSegs: {}", self.out_segs)?;
        writeln!(f, "InBytes: {}", self.in_bytes)?;
        writeln!(f, "OutBytes: {}", self.out_bytes)?;
        writeln!(f, "RetransSegs: {}", self.retrans_segs)?;
        writeln!(f, "FastRetransSegs: {}", self.fast_retrans)?;
        writeln!(f, "EarlyRetransSegs: {}", self.early_retrans)?;
        writeln!(f, "LostSegs: {}", self.lost_segs)?;
        writeln!(f, "RepeatSegs: {}", self.repeat_segs)?;
        writeln!(f, "FECFullShards: {}", self.fec_full_shards)?;
        writeln!(f, "FECParityShards: {}", self.fec_parity_shards)?;
        writeln!(f, "FECErrs: {}", self.fec_errs)?;
        writeln!(f, "FECRecovered: {}", self.fec_recovered)?;
        writeln!(f, "FECShardSet: {}", self.fec_shard_set)?;
        writeln!(f, "FECShardMin: {}", self.fec_shard_min)?;
        writeln!(f, "RingBufferSndQueue: {}", self.ring_buffer_snd_queue)?;
        writeln!(f, "RingBufferRcvQueue: {}", self.ring_buffer_rcv_queue)?;
        writeln!(f, "RingBufferSndBuffer: {}", self.ring_buffer_snd_buffer)?;
        writeln!(f, "EmptyFlush: {}", self.empty_flush)?;
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
        snmp.in_segs.fetch_add(5, Ordering::Relaxed);

        let snap = snmp.snapshot();
        assert_eq!(snap.bytes_sent, 100);
        assert_eq!(snap.in_segs, 5);
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
    fn snmp_header_matches_go_count() {
        let headers = SNMP::header();
        // Go kcp-go Snmp.Header() has 29 fields.
        assert_eq!(headers.len(), 29);
        assert_eq!(headers[0], "BytesSent");
        assert_eq!(headers[11], "InSegs");
        assert_eq!(headers[28], "RingBufferSndBuffer");
    }

    #[test]
    fn snmp_to_slice_count() {
        let snmp = SNMP::new();
        let slice = snmp.to_slice();
        assert_eq!(slice.len(), SNMP::header().len());
    }

    #[test]
    fn snmp_display() {
        let snmp = SNMP::new();
        let snap = snmp.snapshot();
        let display = format!("{}", snap);
        assert!(display.contains("SNMP"));
        assert!(display.contains("BytesSent"));
        assert!(display.contains("InPkts"));
    }

    #[test]
    fn session_open_close_tracks_estab() {
        enable();
        let snmp = SNMP::new();
        snmp.session_opened(true);
        snmp.session_opened(false);
        let s = snmp.snapshot();
        assert_eq!(s.active_opens, 1);
        assert_eq!(s.passive_opens, 1);
        assert_eq!(s.curr_estab, 2);
        assert_eq!(s.max_conn, 2);
        snmp.session_closed();
        assert_eq!(snmp.snapshot().curr_estab, 1);
    }

    #[test]
    fn default_snmp_is_accessible() {
        // Don't assert absolute zeros — other tests may have touched the global.
        let _ = DEFAULT_SNMP.snapshot().bytes_sent;
    }
}
