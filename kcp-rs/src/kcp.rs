//! Core KCP state machine.
//!
//! This module implements the KCP ARQ protocol state machine — the heart of
//! the reliable transport. It manages send/receive buffers, retransmission
//! timers, congestion control, and window management.
//!
//! ## One-to-one correspondence with Go kcp-go v5
//!
//! This implementation is a port of Go's `github.com/xtaci/kcp-go/v5/kcp.go`
//! and aims for **wire-level protocol compatibility** — packets produced by
//! this Rust implementation must be decodable by the Go implementation and
//! vice versa.

use std::cmp;
use std::collections::VecDeque;
use std::fmt;

use bytes::{Bytes, BytesMut};
use smallvec::SmallVec;

use crate::segment::SegmentPool;
use crate::segment::{
    Command, Segment, KCP_ASK_SEND, KCP_ASK_TELL, KCP_DEFAULT_WND, KCP_MAX_FRAG, KCP_OVERHEAD,
    KCP_THRESHOLD_INIT, MTU,
};
use crate::snmp::{self, DEFAULT_SNMP};

// ─── Constants (matching Go kcp-go v5) ─────────────────────────────────────

/// No delay min RTO (enabled by NoDelay).  Matches Go `IKCP_RTO_NDL`.
const RTO_NODELAY_MIN: u32 = 30;
/// Normal min RTO.  Matches Go `IKCP_RTO_MIN`.
const RTO_DEFAULT_MIN: u32 = 100;
/// Default RTO (200ms).  Matches Go `IKCP_RTO_DEF`.
const RTO_DEFAULT: u32 = 200;
/// Max RTO cap.  Matches Go `IKCP_RTO_MAX`.
const RTO_MAX: u32 = 60000;

/// The first probe interval (500ms, matching the Rust port's prior default).
/// kcp-go uses 7000ms — both are far slower than a tunnel wants to recover a
/// closed peer window under burst load.
pub(crate) const PROBE_INIT: u32 = 500;
/// Probe interval used in nodelay modes (P3). When the peer window closes
/// mid-burst, 500ms between WAsk probes stalls recovery; 50ms cuts the
/// window-reopen latency ~10× (measured 947ms→68ms on 256KB@RPS=250).
pub(crate) const PROBE_INIT_NODELAY: u32 = 50;
/// Max probe interval (120s, matching Go)
pub const IKCP_PROBE_LIMIT: u32 = 120000;
/// After this many retransmits, mark the connection as dead.
/// Matches Go `IKCP_DEADLINK`.
const DEAD_LINK_RETRIES: u32 = 20;

/// State value indicating a dead connection (matching Go kcp-go `dead_link`).
const DEAD_LINK_STATE: u32 = 0xFFFF_FFFF;

/// Slow-start threshold minimum.  Matches Go `IKCP_THRESH_MIN`.
const THRESH_MIN: u32 = 2;

/// A queued ACK entry: sequence number + timestamp of the received segment.
#[derive(Clone, Copy)]
struct AckEntry {
    sn: u32,
    ts: u32,
}

// ─── KCP State Machine ─────────────────────────────────────────────────────

/// The primary KCP protocol state machine.
pub struct KCP {
    // ── Conversation identity ──
    conv: u32,
    /// Authentication token (matching Go kcp-go).
    pub token: u32,
    state: u32, // 0xFFFFFFFF = dead (matching Go dead_link)

    // ── Send side ──
    snd_queue: VecDeque<Segment>,
    snd_buf: VecDeque<Segment>,
    snd_nxt: u32,
    snd_una: u32,

    // ── Receive side ──
    rcv_queue: VecDeque<Segment>,
    rcv_buf: VecDeque<Segment>,
    rcv_nxt: u32,

    // ── Windows ──
    snd_wnd: u32,
    rcv_wnd: u32,
    rmt_wnd: u32,
    cwnd: u32,
    ssthresh: u32,

    // ── MSS / MTU ──
    mss: u32,
    mtu: u32,

    // ── RTT estimation ──
    rx_srtt: i32,   // Go uses int32 for signed delta
    rx_rttvar: i32, // Go uses int32 for signed delta
    rx_rto: u32,
    rx_minrto: u32,

    // ── Timers ──
    interval: u32,
    ts_flush: u32,
    updated: u32, // whether first Update() has been called (matching Go)

    // ── Options ──
    nodelay: u32,
    fastresend: i32,
    nocwnd: i32,
    stream: i32,

    // ── Window probe ──
    probe: u32,
    probe_wait: u32,
    ts_probe: u32,
    /// Set when [`input_no_flush`] saw something that needs a flush (UNA
    /// advance / ACK queue fill / ack_no_delay ACKs). Deferred so a batch of
    /// input datagrams triggers ONE `flush_with_current` instead of one per
    /// segment (each full flush iterates the whole snd_buf).
    pending_flush: u32,

    // ── Congestion control ──
    dead_link: u32,
    incr: u32,

    // ── ACK list ──
    /// Most flush cycles accumulate ≤16 ACKs; SmallVec avoids heap alloc on
    /// the common path while spilling to the heap for large bursts.
    acklist: SmallVec<[AckEntry; 16]>,

    // ── Output buffer ──
    buffer: BytesMut,

    // ── Callbacks ──
    output: Box<dyn FnMut(Bytes) + Send>,

    // ── Segment pool ──
    pool: SegmentPool,

    // ── Reusable ACK segment (avoids pool acquire/release per flush) ──
    /// A reusable segment for ACK/WASK/WINS headers during flush.
    /// This avoids the overhead of pool.acquire()/release() on every flush cycle.
    /// The segment's data buffer is not used (ACKs have no payload), so we only
    /// need to reset the header fields before each use.
    ack_seg: Segment,
}

impl fmt::Debug for KCP {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KCP")
            .field("conv", &self.conv)
            .field("state", &self.state)
            .field("snd_una", &self.snd_una)
            .field("snd_nxt", &self.snd_nxt)
            .field("rcv_nxt", &self.rcv_nxt)
            .field("snd_wnd", &self.snd_wnd)
            .field("rcv_wnd", &self.rcv_wnd)
            .field("rmt_wnd", &self.rmt_wnd)
            .field("cwnd", &self.cwnd)
            .field("ssthresh", &self.ssthresh)
            .field("rx_rto", &self.rx_rto)
            .field("interval", &self.interval)
            .field("snd_queue.len", &self.snd_queue.len())
            .field("snd_buf.len", &self.snd_buf.len())
            .field("rcv_buf.len", &self.rcv_buf.len())
            .field("rcv_queue.len", &self.rcv_queue.len())
            .finish()
    }
}

/// Signed difference for wrapping-safe sequence number comparisons.
/// Matches the original KCP C function `itimediff(later, earlier)`.
///
/// Accepts any integer widening to `u64` (both `u32` KCP fields and `u64`
/// timestamps from `current_ms()`) without explicit casts at call sites.
/// The result is `i32` — KCP wire timestamps are 32-bit and `itimediff`
/// only cares about the low 32 bits of the difference.
#[inline]
fn itimediff(later: impl Into<u64>, earlier: impl Into<u64>) -> i32 {
    later.into().wrapping_sub(earlier.into()) as i32
}

/// Wall-clock ms since UNIX_EPOCH via a process-wide cached clock: one
/// `SystemTime::now()` at first use, then `Instant::elapsed()` per call —
/// removes the epoch-conversion syscall from the per-packet paths. Monotonic by
/// construction (ignores wall-clock adjustments), which KCP/FEC timing only
/// cares about via 32-bit `itimediff` gaps.
/// pprof evidence: `Timespec::now` was 6.52% of CPU (1489ms/20s) from ~2000
/// `SystemTime::now()` calls/sec across flush_data_only + input_no_flush +
/// flush_input_batch + send_to_kcp paths (and one call per FEC encode before
/// the clock was shared).
#[inline]
pub(crate) fn wall_ms() -> u64 {
    use std::sync::OnceLock;
    struct ClockOffset {
        epoch_ms: u64,
        instant: std::time::Instant,
    }
    static OFFSET: OnceLock<ClockOffset> = OnceLock::new();
    let offset = OFFSET.get_or_init(|| ClockOffset {
        epoch_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
        instant: std::time::Instant::now(),
    });
    offset.epoch_ms + offset.instant.elapsed().as_millis() as u64
}

impl KCP {
    /// Create a new KCP instance.
    ///
    /// `conv` is the conversation ID, `token` is an additional identifier, and
    /// `output` is called whenever the KCP instance has bytes to send over the
    /// wire.  The callback is boxed internally — callers can pass closures or
    /// function pointers without manual `Box::new`.
    pub fn new<F: FnMut(Bytes) + Send + 'static>(conv: u32, token: u32, output: F) -> Self {
        let mtu = MTU as u32;
        KCP {
            conv,
            token,
            state: 0,
            // High-concurrency servers may hold thousands of mostly idle KCP
            // states. Allocate segment slots on first traffic instead of
            // reserving four 128-entry Segment arrays for every connection.
            snd_queue: VecDeque::new(),
            snd_buf: VecDeque::new(),
            snd_nxt: 0,
            snd_una: 0,
            rcv_queue: VecDeque::new(),
            rcv_buf: VecDeque::new(),
            rcv_nxt: 0,
            snd_wnd: KCP_DEFAULT_WND,
            rcv_wnd: KCP_DEFAULT_WND,
            rmt_wnd: KCP_DEFAULT_WND,
            cwnd: KCP_DEFAULT_WND, // Go: 0, but we follow C KCP (ikcp_create sets cwnd = IKCP_WND_SND)
            ssthresh: KCP_THRESHOLD_INIT,
            mss: mtu.saturating_sub(KCP_OVERHEAD as u32),
            mtu,
            rx_srtt: 0,
            rx_rttvar: 0,
            rx_rto: RTO_DEFAULT,
            rx_minrto: RTO_DEFAULT_MIN,
            interval: 100,
            ts_flush: 100, // Go initializes ts_flush = IKCP_INTERVAL = 100
            updated: 0,
            nodelay: 0,
            fastresend: 0,
            nocwnd: 0,
            stream: 0,
            probe: 0,
            probe_wait: 0,
            ts_probe: 0,
            pending_flush: 0,
            dead_link: DEAD_LINK_RETRIES,
            incr: 0,
            acklist: SmallVec::new(),
            buffer: BytesMut::with_capacity(MTU),
            output: Box::new(output),
            pool: SegmentPool::new(4096),
            // Pre-allocate ACK segment for flush path (no data buffer needed)
            ack_seg: Segment::with_capacity(0),
        }
    }

    /// Compute the unused window (wnd_unused in Go).
    fn wnd_unused(&self) -> u16 {
        let queued = self.rcv_queue.len() as u32;
        if queued < self.rcv_wnd {
            (self.rcv_wnd - queued) as u16
        } else {
            0
        }
    }

    // ── Configuration ─────────────────────────────────────────────────────

    /// Set the conversation ID. Used by a server session to adopt a client's
    /// (random) conv from the first inbound packet — matching Go kcp-go's
    /// server, which keys each accepted session by the first packet's conv.
    pub fn set_conv(&mut self, conv: u32) {
        self.conv = conv;
    }

    /// Set the MTU (Maximum Transmission Unit).
    /// Returns true if successful.
    pub fn set_mtu(&mut self, mtu: u32) -> bool {
        if mtu < 50 || mtu < KCP_OVERHEAD as u32 {
            return false;
        }
        self.mtu = mtu;
        self.mss = mtu - KCP_OVERHEAD as u32;
        self.buffer = BytesMut::with_capacity(self.mtu as usize);
        true
    }

    /// Get the current MTU.
    #[inline]
    pub fn mtu(&self) -> u32 {
        self.mtu
    }

    /// Set the send window size.
    #[inline]
    pub fn set_snd_wnd(&mut self, wnd: u32) {
        // Go: no clamping, just assign if > 0
        if wnd > 0 {
            self.snd_wnd = wnd;
        }
    }

    /// Set the receive window size.
    #[inline]
    pub fn set_rcv_wnd(&mut self, wnd: u32) {
        if wnd > 0 {
            self.rcv_wnd = wnd;
        }
    }

    /// Set nodelay parameters (matches the Go kcp-go `NoDelay` API).
    ///
    /// - `nodelay`: 0 = default, 1 = enable nodelay
    /// - `interval`: internal update interval in milliseconds
    /// - `resend`: fast retransmit threshold (0 = disabled)
    /// - `nc`: no congestion control (0 = default, 1 = off)
    pub fn set_nodelay(&mut self, nodelay: u32, interval: u32, resend: u32, nc: u32) {
        if nodelay != 0 {
            self.nodelay = nodelay;
            self.rx_minrto = RTO_NODELAY_MIN; // Go: nodelay → rx_minrto = 30
        } else {
            self.rx_minrto = RTO_DEFAULT_MIN; // Go: no nodelay → rx_minrto = 100
        }

        let interval = interval as i32;
        if interval > 5000 {
            self.interval = 5000;
        } else if interval < 10 {
            self.interval = 10;
        } else {
            self.interval = interval as u32;
        }

        if resend > 0 {
            self.fastresend = resend as i32;
        }
        if nc > 0 {
            self.nocwnd = nc as i32;
        }
    }

    /// Enable or disable stream mode.
    #[inline]
    pub fn set_stream_mode(&mut self, enable: bool) {
        self.stream = if enable { 1 } else { 0 };
    }

    /// Ask KCP to emit a window-size probe (`WASK`, cmd 83) on the next flush.
    ///
    /// A live peer replies with `WINS` (cmd 84), so a dialing side can observe
    /// that response to confirm reachability + conv match without sending user
    /// data (used by [`crate::KcpStream`]'s connect-timeout first-packet wait).
    #[inline]
    pub fn request_probe(&mut self) {
        self.probe |= KCP_ASK_SEND;
    }

    // ── User API ──────────────────────────────────────────────────────────

    /// Send data through the KCP connection.
    ///
    /// This queues the data for transmission. SN is assigned during flush()
    /// when the segment is moved to snd_buf, matching Go behavior.
    pub fn send(&mut self, data: &[u8]) -> Result<(), KcpError> {
        if data.is_empty() {
            return Err(KcpError::NoData);
        }

        let original_len = data.len() as u64;

        // Stream mode: append to previous segment in snd_queue if possible
        let data = if self.stream != 0 {
            if let Some(last) = self.snd_queue.back_mut() {
                if (last.data.len() as u32) < self.mss {
                    let capacity = self.mss as usize - last.data.len();
                    let extend = cmp::min(data.len(), capacity);
                    last.data.extend_from_slice(&data[..extend]);
                    last.len = last.data.len() as u32;
                    if extend >= data.len() {
                        snmp::add(&DEFAULT_SNMP.bytes_sent, original_len);
                        return Ok(());
                    }
                    // Advance past appended bytes (matching Go: buffer = buffer[extend:])
                    &data[extend..]
                } else {
                    data
                }
            } else {
                data
            }
        } else {
            data
        };

        // Calculate fragment count
        let count = if data.len() as u32 <= self.mss {
            1
        } else {
            (data.len() as u32).div_ceil(self.mss)
        };

        if count > KCP_MAX_FRAG {
            return Err(KcpError::TooManyFragments);
        }

        let mut offset = 0usize;
        for i in 0..count {
            let mut seg = self.pool.acquire();
            let size = cmp::min(data.len() - offset, self.mss as usize);
            seg.conv = self.conv;
            seg.cmd = Command::Push as u8;
            if self.stream == 0 {
                seg.frg = (count - 1 - i) as u8;
            } else {
                seg.frg = 0;
            }
            seg.data.extend_from_slice(&data[offset..offset + size]);
            seg.len = size as u32;
            // Note: payload freezing is deferred to flush() — see the freeze
            // step before encode() in the snd_buf transmit loop.  Freezing
            // here would empty `data` and break stream-mode append on the
            // next send() call (capacity check uses data.len()).
            self.snd_queue.push_back(seg);
            offset += size;
        }

        // Upper-level bytes accepted for send (Go UDPSession.WriteBuffers).
        snmp::add(&DEFAULT_SNMP.bytes_sent, original_len);

        Ok(())
    }

    /// Receive data from the KCP connection. Returns `None` if no data is
    /// available.
    pub fn recv(&mut self) -> Result<BytesMut, KcpError> {
        let peeksize = self.peeksize().ok_or(KcpError::NoData)?;

        let fast_recover = self.rcv_queue.len() as u32 >= self.rcv_wnd;

        // Merge fragments
        let mut data = BytesMut::with_capacity(peeksize);
        loop {
            match self.rcv_queue.pop_front() {
                Some(seg) => {
                    let size = cmp::min(seg.len as usize, seg.data.len());
                    if size > 0 {
                        data.extend_from_slice(&seg.data[..size]);
                    }
                    self.pool.release(seg);
                    if data.len() >= peeksize {
                        break;
                    }
                }
                None => break,
            }
        }

        // Move available data from rcv_buf → rcv_queue
        self.move_receive_buffer();

        // Fast recover
        if (self.rcv_queue.len() as u32) < self.rcv_wnd && fast_recover {
            self.probe |= KCP_ASK_TELL;
        }

        snmp::add(&DEFAULT_SNMP.bytes_received, data.len() as u64);

        Ok(data)
    }

    /// Receive data as `Bytes` (zero-copy when single-segment).
    ///
    /// When the message fits in a single KCP segment (frg == 0, the
    /// common case in stream mode), this returns a reference-counted
    /// `Bytes` slice of the segment's data — no `extend_from_slice`
    /// copy is performed.
    ///
    /// When the message spans multiple segments, they are merged via
    /// `extend_from_slice` (same as `recv()`).
    pub fn recv_bytes(&mut self) -> Result<bytes::Bytes, KcpError> {
        let peeksize = self.peeksize().ok_or(KcpError::NoData)?;

        let fast_recover = self.rcv_queue.len() as u32 >= self.rcv_wnd;

        // ── Fast path: single-segment message (frg == 0) ──
        // This is the common case in stream mode. Zero-copy: just
        // freeze the segment's BytesMut data and return a slice.
        if peeksize == self.rcv_queue.front().map(|s| s.len as usize).unwrap_or(0) {
            if let Some(mut seg) = self.rcv_queue.pop_front() {
                let size = cmp::min(seg.len as usize, seg.data.len());
                let data = seg.data.split_to(size).freeze();
                self.pool.release(seg);

                // Move available data from rcv_buf → rcv_queue
                self.move_receive_buffer();

                // Fast recover
                if (self.rcv_queue.len() as u32) < self.rcv_wnd && fast_recover {
                    self.probe |= KCP_ASK_TELL;
                }

                snmp::add(&DEFAULT_SNMP.bytes_received, data.len() as u64);

                return Ok(data);
            }
        }

        // ── Slow path: multi-segment message ──
        let mut data = BytesMut::with_capacity(peeksize);
        loop {
            match self.rcv_queue.pop_front() {
                Some(seg) => {
                    let size = cmp::min(seg.len as usize, seg.data.len());
                    if size > 0 {
                        data.extend_from_slice(&seg.data[..size]);
                    }
                    self.pool.release(seg);
                    if data.len() >= peeksize {
                        break;
                    }
                }
                None => break,
            }
        }

        // Move available data from rcv_buf → rcv_queue
        self.move_receive_buffer();

        // Fast recover
        if (self.rcv_queue.len() as u32) < self.rcv_wnd && fast_recover {
            self.probe |= KCP_ASK_TELL;
        }

        let out = data.freeze();
        snmp::add(&DEFAULT_SNMP.bytes_received, out.len() as u64);
        Ok(out)
    }

    /// Check the size of the next message in the receive queue.
    /// Returns `None` if no complete message is available.
    pub fn peeksize(&self) -> Option<usize> {
        let front = self.rcv_queue.front()?;
        if front.frg == 0 {
            return Some(front.len as usize);
        }

        let count = self.rcv_queue.len();
        // A fragmented message needs at least `frg + 1` segments.  Keep the
        // queue length as `usize`; narrowing it to u8 made a full 256-entry
        // queue look empty when the fragment count was 255.
        if count <= front.frg as usize {
            return None;
        }

        let mut length = 0usize;
        for seg in &self.rcv_queue {
            length = length.checked_add(seg.len as usize)?;
            if seg.frg == 0 {
                break;
            }
        }
        Some(length)
    }

    // ── Ack processing ────────────────────────────────────────────────────

    fn parse_ack(&mut self, sn: u32) {
        if itimediff(sn, self.snd_una) < 0 || itimediff(sn, self.snd_nxt) >= 0 {
            return;
        }

        // snd_buf is contiguous from snd_una, including across u32 wrap.  Use
        // the wrapping sequence delta as the deque index, then verify the SN
        // at that index so malformed/gapped state cannot ACK a neighbour.
        let index = sn.wrapping_sub(self.snd_una) as usize;
        let Some(seg) = self.snd_buf.get_mut(index) else {
            return;
        };
        if seg.sn == sn {
            seg.acked = true;
            // The flush loop skips acked segments (`if seg.acked { continue }`),
            // so the payload is dead weight until `parse_una` eventually pops
            // the segment from the front of `snd_buf`. With `snd_wnd = 1024`
            // that can be ~1.4 MB per connection of payloads already accepted
            // by the peer but still pinned in memory.
            seg.data.clear();
            seg.len = 0;
            // Go: recycleSegment frees the data buffer but leaves the entry
            // so we don't need to remove from snd_buf here.
        }
    }

    fn parse_fastack(&mut self, sn: u32, ts: u32) -> bool {
        // Fast retransmit disabled: with `fastresend <= 0` the flush loop sets
        // `resent = 0xFFFFFFFF`, so no fastack count can ever trigger a
        // retransmit — skip the O(inflight) snd_buf scan entirely (every
        // inbound ACK would otherwise walk the deque for nothing).
        if self.fastresend <= 0 {
            return false;
        }
        if itimediff(sn, self.snd_una) < 0 || itimediff(sn, self.snd_nxt) >= 0 {
            return false;
        }

        let mut should_fast_ack = false;
        for seg in &mut self.snd_buf {
            if itimediff(sn, seg.sn) < 0 {
                break;
            } else if sn != seg.sn && itimediff(seg.ts, ts) <= 0 {
                if seg.fastack != 0xFFFFFFFF {
                    seg.fastack += 1;
                    if seg.fastack >= self.fastresend as u32 {
                        should_fast_ack = true;
                    }
                }
            }
        }

        should_fast_ack
    }

    fn parse_una(&mut self, una: u32) -> usize {
        // snd_buf is ordered by SN.  Pop from the front while UNA advances;
        // this performs one scan and releases in the same front-to-back order
        // as Go's shrink_buf path.
        let mut count = 0;
        loop {
            let remove = self
                .snd_buf
                .front()
                .is_some_and(|seg| itimediff(una, seg.sn) > 0);
            if !remove {
                break;
            }
            if let Some(seg) = self.snd_buf.pop_front() {
                self.pool.release(seg);
                count += 1;
            }
        }
        count
    }

    fn shrink_buf(&mut self) {
        if let Some(seg) = self.snd_buf.front() {
            self.snd_una = seg.sn;
        } else {
            self.snd_una = self.snd_nxt;
        }
    }

    fn ack_push(&mut self, sn: u32, ts: u32) {
        self.acklist.push(AckEntry { sn, ts });
    }

    // ── Input processing ─────────────────────────────────────────────────

    /// Feed incoming raw data into the KCP state machine.
    ///
    /// Process one inbound datagram **without** triggering a flush.
    ///
    /// `ack_no_delay` matches Go's `ackNoDelay` parameter — when true, ACKs
    /// are requested immediately, but the actual `flush_with_current` is
    /// deferred: the caller batches many datagrams (each typically advancing
    /// `una` and thus marking `flush_segments`) and calls
    /// [`flush_if_pending`](Self::flush_if_pending) once. A per-segment flush
    /// iterates the whole `snd_buf` (~O(500)), so at 50k+ pkt/s that alone is
    /// ~30M snd_buf iterations/s — the dominant per-segment cost.
    pub fn input_no_flush(&mut self, data: &[u8], ack_no_delay: bool) -> Result<usize, KcpError> {
        let snd_una = self.snd_una;
        let mut offset = 0usize;
        let mut latest_ts = 0u32;
        let mut update_rtt = false;
        let mut in_segs = 0u64;
        let mut flush_segments = 0u32;

        while data.len().saturating_sub(offset) >= KCP_OVERHEAD {
            // Decode 24B header from stack slice (P2.1: no alloc; single copy of header words).
            let hdr = &data[offset..offset + KCP_OVERHEAD];
            let conv = u32::from_le_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
            let cmd = hdr[4];
            let frg = hdr[5];
            let wnd = u16::from_le_bytes([hdr[6], hdr[7]]);
            let ts = u32::from_le_bytes([hdr[8], hdr[9], hdr[10], hdr[11]]);
            let sn = u32::from_le_bytes([hdr[12], hdr[13], hdr[14], hdr[15]]);
            let una = u32::from_le_bytes([hdr[16], hdr[17], hdr[18], hdr[19]]);
            let length = u32::from_le_bytes([hdr[20], hdr[21], hdr[22], hdr[23]]) as usize;

            // Validate conv FIRST (matching Go: check conv before length)
            if conv != self.conv {
                return Err(KcpError::ConvMismatch {
                    expected: self.conv,
                    got: conv,
                });
            }

            let total_len = match offset
                .checked_add(KCP_OVERHEAD)
                .and_then(|header_end| header_end.checked_add(length))
            {
                Some(total_len) if total_len <= data.len() => total_len,
                _ => return Err(KcpError::InvalidLength),
            };

            // Validate command
            if cmd != Command::Push as u8
                && cmd != Command::Ack as u8
                && cmd != Command::WAsk as u8
                && cmd != Command::WIns as u8
            {
                return Err(KcpError::UnknownCommand(cmd));
            }

            // Trust window updates from data/ack packets (matching Go)
            self.rmt_wnd = wnd as u32;

            // Process UNA (matching Go: parse_una → shrink_buf)
            if self.parse_una(una) > 0 {
                flush_segments |= 1;
            }
            self.shrink_buf();

            match cmd {
                c if c == Command::Ack as u8 => {
                    self.parse_ack(sn);
                    if self.parse_fastack(sn, ts) {
                        flush_segments |= 2;
                    }
                    update_rtt = true;
                    latest_ts = ts;
                }
                c if c == Command::Push as u8 => {
                    // Window check: only ack if within window
                    if itimediff(sn, self.rcv_nxt.wrapping_add(self.rcv_wnd)) < 0 {
                        self.ack_push(sn, ts);
                        if itimediff(sn, self.rcv_nxt) >= 0 {
                            // Create segment from received data
                            let mut seg = self.pool.acquire();
                            seg.conv = conv;
                            seg.cmd = cmd;
                            seg.frg = frg;
                            seg.wnd = wnd;
                            seg.ts = ts;
                            seg.sn = sn;
                            seg.una = una;
                            seg.len = length as u32;
                            if length > 0 {
                                seg.data
                                    .extend_from_slice(&data[offset + KCP_OVERHEAD..total_len]);
                            }

                            // Insert into receive buffer (matching Go parse_data).
                            // Go increments RepeatSegs when parse_data reports a duplicate.
                            if self.parse_data(seg) {
                                snmp::add(&DEFAULT_SNMP.repeat_segs, 1);
                            }
                        }
                    }
                }
                c if c == Command::WAsk as u8 => {
                    // Ready to send back WIns in flush
                    self.probe |= KCP_ASK_TELL;
                }
                c if c == Command::WIns as u8 => {
                    // Window update received, nothing to do
                }
                _ => {}
            }

            in_segs += 1;
            offset = total_len;
        }

        // Update SNMP
        if in_segs > 0 {
            snmp::add(&DEFAULT_SNMP.in_segs, in_segs);
        }

        // Update RTT with the latest ts (matching Go: only for regular packets).
        // Fetch the current timestamp once and reuse it for flush triggers below.
        let now_ms = self.current_ms();
        if update_rtt {
            if itimediff(now_ms, latest_ts) >= 0 {
                self.update_ack(itimediff(now_ms, latest_ts));
            }
        }

        // Congestion window update (matching Go: nocwnd check)
        if self.nocwnd == 0 {
            if itimediff(self.snd_una, snd_una) > 0 {
                if self.cwnd < self.rmt_wnd {
                    let mss = self.mss;
                    if self.cwnd < self.ssthresh {
                        self.cwnd += 1;
                        self.incr += mss;
                    } else {
                        if self.incr < mss {
                            self.incr = mss;
                        }
                        self.incr += (mss * mss) / self.incr + (mss / 16);
                        if (self.cwnd + 1) * mss <= self.incr {
                            if mss > 0 {
                                self.cwnd = self.incr.div_ceil(mss);
                            } else {
                                self.cwnd = self.incr + mss - 1;
                            }
                        }
                    }
                    if self.cwnd > self.rmt_wnd {
                        self.cwnd = self.rmt_wnd;
                        self.incr = self.rmt_wnd * mss;
                    }
                }
            }
        }

        // Deferred flush triggers (matching Go's intent, but batched): record
        // what needs flushing; `flush_if_pending()` performs ONE
        // `flush_with_current` for the whole batch instead of one per segment.
        if flush_segments != 0 {
            self.pending_flush |= 1;
        } else if self.acklist.len() >= (self.mtu / KCP_OVERHEAD as u32) as usize {
            self.pending_flush |= 1;
        } else if ack_no_delay && !self.acklist.is_empty() {
            self.pending_flush |= 1;
        }

        Ok(offset)
    }

    /// Run one `flush_with_current` if any [`input_no_flush`](Self::input_no_flush)
    /// since the last flush requested it. Returns the next update interval
    /// (`flush_with_current`'s value, or `self.interval` when nothing pending).
    pub fn flush_if_pending(&mut self, current: u32) -> u32 {
        if self.pending_flush != 0 {
            self.pending_flush = 0;
            self.flush_with_current(current, true)
        } else {
            self.interval
        }
    }

    /// Backward-compatible single-datagram input: process then flush
    /// immediately (matches Go `kcp.Input` + `flush(true)` semantics for
    /// callers that don't batch).
    pub fn input(&mut self, data: &[u8], ack_no_delay: bool) -> Result<usize, KcpError> {
        let r = self.input_no_flush(data, ack_no_delay)?;
        let current = self.current_ms() as u32;
        self.flush_if_pending(current);
        Ok(r)
    }

    /// Insert a segment into the receive buffer and try to move data to the
    /// receive queue. Matches Go's `parse_data`.
    ///
    /// Returns `true` if the segment was a duplicate (or outside window after
    /// already being acked), matching Go's `repeat` flag for SNMP.RepeatSegs.
    fn parse_data(&mut self, newseg: Segment) -> bool {
        let sn = newseg.sn;

        // Check if outside receive window
        if itimediff(sn, self.rcv_nxt.wrapping_add(self.rcv_wnd)) >= 0
            || itimediff(sn, self.rcv_nxt) < 0
        {
            // The caller acquired this segment from the pool.  Recycle it on
            // every rejected path (including duplicate/out-of-window input),
            // rather than silently dropping its allocation.
            self.pool.release(newseg);
            return true;
        }

        // The common case is an in-order segment.  Bypass the receive-buffer
        // search and enqueue it directly when the local receive queue has
        // room; any already-buffered successor segments are then promoted by
        // `move_receive_buffer`.
        if sn == self.rcv_nxt && (self.rcv_queue.len() as u32) < self.rcv_wnd {
            self.rcv_nxt = self.rcv_nxt.wrapping_add(1);
            self.rcv_queue.push_back(newseg);
            self.move_receive_buffer();
            return false;
        }

        // Single pass: detect duplicate and find insertion position simultaneously
        // (replaces two separate O(n) scans with one).
        let mut is_dup = false;
        let mut insert_pos = self.rcv_buf.len();
        for (i, s) in self.rcv_buf.iter().enumerate() {
            if s.sn == sn {
                is_dup = true;
                break;
            }
            if itimediff(s.sn, sn) > 0 {
                insert_pos = i;
                break;
            }
        }
        if !is_dup {
            self.rcv_buf.insert(insert_pos, newseg);
        } else {
            // Duplicate segment storage is no longer needed; return it to the
            // pool while retaining the existing buffered copy.
            self.pool.release(newseg);
        }

        // Move available data from rcv_buf → rcv_queue
        self.move_receive_buffer();
        is_dup
    }

    /// Move segments from the receive buffer into the receive queue when
    /// they are in-order and within the receive window.
    fn move_receive_buffer(&mut self) {
        while (self.rcv_queue.len() as u32) < self.rcv_wnd {
            let in_order = self
                .rcv_buf
                .front()
                .is_some_and(|seg| seg.sn == self.rcv_nxt);
            if !in_order {
                break;
            }
            if let Some(seg) = self.rcv_buf.pop_front() {
                self.rcv_nxt = self.rcv_nxt.wrapping_add(1);
                self.rcv_queue.push_back(seg);
            }
        }
    }

    // ── RTT estimation (matching Go's update_ack) ─────────────────────────

    fn update_ack(&mut self, rtt: i32) {
        // https://tools.ietf.org/html/rfc6298
        if self.rx_srtt == 0 {
            self.rx_srtt = rtt;
            self.rx_rttvar = rtt >> 1;
        } else {
            let mut delta = rtt - self.rx_srtt;
            self.rx_srtt += delta >> 3;
            if delta < 0 {
                delta = -delta;
            }
            if rtt < self.rx_srtt - self.rx_rttvar {
                // Low-RTT special case: 8x reduced weight
                self.rx_rttvar += (delta - self.rx_rttvar) >> 5;
            } else {
                self.rx_rttvar += (delta - self.rx_rttvar) >> 2;
            }
        }
        let rto = self.rx_srtt as u32 + self.interval.max((self.rx_rttvar as u32) << 2);
        self.rx_rto = rto.clamp(self.rx_minrto, RTO_MAX);
    }

    // ── Update / flush ────────────────────────────────────────────────────

    /// Advance the KCP state machine by one tick.
    ///
    /// `current` is the current timestamp in milliseconds.
    /// Should be called at a regular interval (typically 10–100ms).
    /// Returns the milliseconds until the next meaningful event (matching
    /// Go's `flush()` return value used by `SystemTimedSched`).
    pub fn update(&mut self, current: u32) -> u32 {
        if self.updated == 0 {
            self.updated = 1;
            self.ts_flush = current;
        }

        let mut slap = itimediff(current, self.ts_flush);

        // Clock jump detection (matching Go)
        if !(-10000..10000).contains(&slap) {
            self.ts_flush = current;
            slap = 0; // Go resets slap to 0 after jump (kcp.go:1017)
        }

        if slap >= 0 {
            self.ts_flush = self.ts_flush.wrapping_add(self.interval);
            if itimediff(current, self.ts_flush) >= 0 {
                self.ts_flush = current.wrapping_add(self.interval);
            }
            self.flush_with_current(current, true)
        } else {
            self.interval
        }
    }

    /// Force-flush all pending data.
    ///
    /// Returns `next_update` — the milliseconds until the next meaningful
    /// event (nearest RTO or interval), matching Go kcp-go's `flush()` return.
    #[inline]
    pub fn flush(&mut self) -> u32 {
        let current = self.current_ms() as u32;
        self.flush_with_current(current, true)
    }

    /// Flush WITHOUT emitting ACK segments (data / retransmit / window-probe
    /// only). `KcpStream` uses this on the inline write path so writes cannot
    /// consume ACKs before inbound processing or the protocol-deadline flush
    /// emits them in queue order.
    #[inline]
    pub fn flush_data_only(&mut self) -> u32 {
        let current = self.current_ms() as u32;
        self.flush_with_current(current, false)
    }

    /// Same as [`flush_data_only`](Self::flush_data_only) but with a pre-computed
    /// `current` timestamp, avoiding a redundant `current_ms()` call when the
    /// caller already has the value (e.g., flush loop, `send_to_kcp`).
    /// pprof: eliminates ~1000 redundant `current_ms()` calls/sec.
    #[inline]
    pub fn flush_data_only_with_current(&mut self, current: u32) -> u32 {
        self.flush_with_current(current, false)
    }

    /// Flush with an externally-provided timestamp.
    ///
    /// Use this when the caller already has the current monotonic millisecond
    /// value (e.g., `update()` receives `current`), avoiding a redundant
    /// `SystemTime::now()` syscall inside the flush loop.
    pub fn flush_with_current(&mut self, current: u32, flush_acks: bool) -> u32 {
        let mut next_update = self.interval;

        // Reuse the pre-allocated ACK segment (avoids pool acquire/release per flush)
        // Reset header fields before reuse
        // Compute values first to avoid borrow conflict with &mut self.ack_seg
        let wnd = self.wnd_unused();
        let ack_seg = &mut self.ack_seg;
        ack_seg.conv = self.conv;
        ack_seg.cmd = Command::Ack as u8;
        ack_seg.wnd = wnd;
        ack_seg.una = self.rcv_nxt;
        ack_seg.len = 0; // ACK has no payload

        let mtu = self.mtu as usize;

        // Helper: flush the output buffer and return remaining capacity
        let flush_buf = |buf: &mut BytesMut, output: &mut Box<dyn FnMut(Bytes) + Send>| {
            if !buf.is_empty() {
                let data = buf.split().freeze();
                // split() moves the allocation out; re-reserve so the next
                // packet encode does not pay a fresh malloc per output packet.
                buf.reserve(mtu);
                output(data);
            }
        };

        // ── Flush ACKs ──
        // The inline write path passes `false`; inbound immediate flushes and
        // the serialized protocol-deadline flush pass `true`. All operate
        // under the KCP mutex, preserving ACK queue order.
        if flush_acks && !self.acklist.is_empty() {
            let n = self.acklist.len();
            for i in 0..n {
                let (ack_sn, ack_ts) = (self.acklist[i].sn, self.acklist[i].ts);
                // Check space
                if self.buffer.len() + KCP_OVERHEAD > mtu {
                    flush_buf(&mut self.buffer, &mut self.output);
                }
                // Filter jitter caused by bufferbloat (matching Go)
                if itimediff(ack_sn, self.rcv_nxt) >= 0 || i == n - 1 {
                    ack_seg.sn = ack_sn;
                    ack_seg.ts = ack_ts;
                    ack_seg.encode(&mut self.buffer);
                }
            }
            self.acklist.clear();
        }

        // ── Window probing ──
        // P3: nodelay modes probe the closed peer window 10× sooner (50ms vs
        // 500ms) so a burst-induced wnd=0 recovers in one RTO instead of
        // stalling on the probe backoff (measured 947ms→68ms at 256KB@RPS=250).
        let probe_init = if self.nodelay != 0 {
            PROBE_INIT_NODELAY
        } else {
            PROBE_INIT
        };
        if self.rmt_wnd == 0 {
            if self.probe_wait == 0 {
                self.probe_wait = probe_init;
                self.ts_probe = current + self.probe_wait;
            } else if itimediff(current, self.ts_probe) >= 0 {
                if self.probe_wait < probe_init {
                    self.probe_wait = probe_init;
                }
                self.probe_wait += self.probe_wait / 2;
                if self.probe_wait > IKCP_PROBE_LIMIT {
                    self.probe_wait = IKCP_PROBE_LIMIT;
                }
                self.ts_probe = current + self.probe_wait;
                self.probe |= KCP_ASK_SEND;
            }
        } else {
            self.ts_probe = 0;
            self.probe_wait = 0;
        }

        // ── Flush WAsk ──
        if (self.probe & KCP_ASK_SEND) != 0 {
            ack_seg.cmd = Command::WAsk as u8;
            if self.buffer.len() + KCP_OVERHEAD > mtu {
                flush_buf(&mut self.buffer, &mut self.output);
            }
            ack_seg.encode(&mut self.buffer);
        }

        // ── Flush WIns ──
        if (self.probe & KCP_ASK_TELL) != 0 {
            ack_seg.cmd = Command::WIns as u8;
            if self.buffer.len() + KCP_OVERHEAD > mtu {
                flush_buf(&mut self.buffer, &mut self.output);
            }
            ack_seg.encode(&mut self.buffer);
        }

        self.probe = 0;

        // ── Calculate window ──
        let mut cwnd = self.snd_wnd.min(self.rmt_wnd);
        if self.nocwnd == 0 {
            cwnd = self.cwnd.min(cwnd);
        }

        // ── Move segments from snd_queue to snd_buf ──
        while itimediff(self.snd_nxt, self.snd_una + cwnd) < 0 {
            match self.snd_queue.pop_front() {
                Some(mut newseg) => {
                    newseg.conv = self.conv;
                    newseg.cmd = Command::Push as u8;
                    // SN is assigned here (matching Go: when moving to snd_buf)
                    newseg.sn = self.snd_nxt;
                    self.snd_buf.push_back(newseg);
                    self.snd_nxt += 1;
                }
                None => break,
            }
        }

        // ── Calculate resent threshold ──
        let resent = if self.fastresend <= 0 {
            0xFFFFFFFF
        } else {
            self.fastresend as u32
        };

        // Pre-compute window for use in the loop (avoids borrow conflict)
        let current_wnd = self.wnd_unused();

        // ── Flush data segments ──
        let mut change = 0u64;
        let mut lost_segs = 0u64;
        let mut fast_retrans_segs = 0u64;
        let mut out_segs = 0u64;

        for seg in &mut self.snd_buf {
            if seg.acked {
                continue;
            }

            let mut needsend = false;

            if seg.xmit == 0 {
                // First transmission
                needsend = true;
                seg.rto = self.rx_rto;
                seg.resendts = current + seg.rto;
            } else if seg.fastack >= resent && seg.fastack != 0xFFFFFFFF {
                // Fast retransmit — matching Go kcp-go exactly: fire when
                // `fastack >= fastresend` (typically 2 duplicate ACKs),
                // regardless of whether new data is being sent. The previous
                // `new_segs_count > 0` gate disabled fast retransmit whenever
                // the send window was full — exactly when loss is most likely
                // — forcing ALL retransmissions through RTO timeout (200ms+),
                // which was the direct cause of P99/P999 tail latency spikes.
                //
                // The earlier "fast-retransmit storm" was caused by the
                // non-Go early-retransmit path (firing on `fastack > 0`, i.e.
                // just 1 dup ACK, which triggers on delayed ACKs, not loss).
                // That path is now removed; standard Go fast retransmit with
                // `fastresend=2` requires 2 dup ACKs and is stable.
                needsend = true;
                seg.fastack = 0xFFFFFFFF;
                seg.rto = self.rx_rto;
                seg.resendts = current + seg.rto;
                change += 1;
                fast_retrans_segs += 1;
            } else if itimediff(current, seg.resendts) >= 0 {
                // RTO timeout
                needsend = true;
                if self.nodelay == 0 {
                    seg.rto += self.rx_rto; // Linear backoff
                } else {
                    seg.rto += self.rx_rto / 2; // Half-linear backoff (keep original)
                }
                seg.fastack = 0;
                seg.resendts = current + seg.rto;
                lost_segs += 1;
            }

            if needsend {
                seg.xmit += 1;
                seg.ts = current;
                // Note: Go refreshes currentMs() per segment, but Rust borrow
                // rules prevent calling self.current_ms() inside the snd_buf loop.
                // This is a known minor deviation.
                seg.wnd = current_wnd;
                seg.una = self.rcv_nxt;

                let need = KCP_OVERHEAD + seg.len as usize;
                if self.buffer.len() + need > mtu {
                    flush_buf(&mut self.buffer, &mut self.output);
                }

                // Encode header + payload directly into self.buffer
                // (encode() writes both header and data, avoiding per-seg Vec alloc)
                seg.encode(&mut self.buffer);

                // Aggregate SNMP updates once per flush instead of paying an
                // atomic gated increment for every segment in a burst.
                out_segs += 1;

                // Dead link check (matching Go)
                if seg.xmit >= self.dead_link {
                    self.state = DEAD_LINK_STATE;
                }
            }

            // Track nearest RTO for nextUpdate (matching Go kcp-go)
            if !seg.acked {
                let rto = itimediff(seg.resendts, current);
                if rto > 0 && (rto as u32) < next_update {
                    next_update = rto as u32;
                }
            }
        }

        // Update retransmission stats (matching Go)
        let retrans_sum = lost_segs + fast_retrans_segs;
        if retrans_sum > 0 {
            snmp::add(&DEFAULT_SNMP.retrans_segs, retrans_sum);
        }
        if lost_segs > 0 {
            snmp::add(&DEFAULT_SNMP.lost_segs, lost_segs);
        }
        if fast_retrans_segs > 0 {
            snmp::add(&DEFAULT_SNMP.fast_retrans, fast_retrans_segs);
        }
        if out_segs > 0 {
            snmp::add(&DEFAULT_SNMP.out_segs, out_segs);
        }

        // ── Congestion control (matching Go) ──
        if self.nocwnd == 0 {
            // Rate halving (RFC 6937)
            if change > 0 {
                let inflight = self.snd_nxt - self.snd_una;
                self.ssthresh = (inflight / 2).max(THRESH_MIN);
                self.cwnd = self.ssthresh + resent;
                self.incr = self.cwnd * self.mss;
            }

            // Congestion control (RFC 5681)
            if lost_segs > 0 {
                self.ssthresh = (cwnd / 2).max(THRESH_MIN);
                self.cwnd = 1;
                self.incr = self.mss;
            }

            if self.cwnd < 1 {
                self.cwnd = 1;
                self.incr = self.mss;
            }
        }

        // ── Flush remaining buffer ──
        flush_buf(&mut self.buffer, &mut self.output);

        // Update SNMP queue stats (matching Go)
        snmp::store(
            &DEFAULT_SNMP.ring_buffer_snd_queue,
            self.snd_queue.len() as u64,
        );
        snmp::store(
            &DEFAULT_SNMP.ring_buffer_rcv_queue,
            self.rcv_queue.len() as u64,
        );
        snmp::store(
            &DEFAULT_SNMP.ring_buffer_snd_buffer,
            self.snd_buf.len() as u64,
        );

        // Note: ack_seg is now a reusable field, no need to release back to pool

        next_update
    }

    // ── Helpers ───────────────────────────────────────────────────────────

    /// Current send window (packets in-flight).
    #[inline]
    pub fn wait_send(&self) -> u32 {
        self.snd_buf.len() as u32 + self.snd_queue.len() as u32
    }

    /// Number of segments in the send queue.
    #[inline]
    pub fn snd_queue_len(&self) -> usize {
        self.snd_queue.len()
    }

    /// Number of unacknowledged ACKs queued for the next flush.
    #[inline]
    pub fn acklist_len(&self) -> usize {
        self.acklist.len()
    }

    /// Whether the async driver still has protocol maintenance to schedule
    /// after sending the current output batch.
    #[inline]
    #[cfg(feature = "async")]
    pub(crate) fn needs_update(&self) -> bool {
        self.wait_send() > 0
            || !self.acklist.is_empty()
            || self.pending_flush != 0
            || self.probe != 0
    }

    /// Whether a deferred flush is pending (ACKs / data to emit).
    #[inline]
    pub fn pending_flush_flag(&self) -> u32 {
        self.pending_flush
    }

    /// Check if there's data ready to receive.
    #[inline]
    pub fn can_recv(&self) -> bool {
        !self.rcv_queue.is_empty()
    }

    /// Peek at the receive queue without consuming.
    #[inline]
    pub fn peek_recv(&self) -> Option<&Segment> {
        self.rcv_queue.front()
    }

    /// Get the conversation ID.
    #[inline]
    pub fn conv(&self) -> u32 {
        self.conv
    }

    /// KCP connection state. `0xFFFFFFFF` means dead (dead_link exceeded).
    #[inline]
    pub fn state(&self) -> u32 {
        self.state
    }

    /// Returns true if KCP connection is dead (state == 0xFFFFFFFF).
    #[inline]
    pub fn is_dead(&self) -> bool {
        self.state == DEAD_LINK_STATE
    }

    /// Get the current RTO value.
    #[inline]
    pub fn rx_rto(&self) -> u32 {
        self.rx_rto
    }

    /// Get the current congestion window.
    #[inline]
    pub fn cwnd(&self) -> u32 {
        self.cwnd
    }

    /// Get the current update interval.
    #[inline]
    pub fn interval(&self) -> u32 {
        self.interval
    }

    /// Get the current timestamp in milliseconds (for matching Go).
    ///
    /// Returns `u64` to avoid 32-bit overflow after ~49.7 days of uptime.
    /// Callers that need the KCP 32-bit wire timestamp should truncate with
    /// `as u32` — `itimediff` handles wraparound correctly.
    ///
    /// **Optimization**: delegates to the shared cached clock [`wall_ms`].
    pub fn current_ms(&self) -> u64 {
        wall_ms()
    }

    /// Get the maximum segment size.
    #[inline]
    pub fn mss(&self) -> u32 {
        self.mss
    }

    /// Get the send window size.
    #[inline]
    pub fn snd_wnd(&self) -> u32 {
        self.snd_wnd
    }

    /// Get the receive window size.
    #[inline]
    pub fn rcv_wnd(&self) -> u32 {
        self.rcv_wnd
    }

    /// Get the segment pool reference.
    #[inline]
    pub fn pool(&self) -> &SegmentPool {
        &self.pool
    }

    /// Get next sequence number.
    #[inline]
    pub fn snd_nxt(&self) -> u32 {
        self.snd_nxt
    }

    /// Get oldest unacknowledged sequence number.
    #[inline]
    pub fn snd_una(&self) -> u32 {
        self.snd_una
    }
}

// ─── Errors ──────────────────────────────────────────────────────────────

/// Errors produced by the KCP state machine.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KcpError {
    /// No data available to receive.
    #[error("no data available")]
    NoData,
    /// Too many fragments for a single send.
    #[error("too many fragments")]
    TooManyFragments,
    /// Invalid segment - length exceeds remaining data.
    #[error("invalid length")]
    InvalidLength,
    /// Invalid segment - conversation ID mismatch.
    #[error("conv mismatch: expected={expected}, got={got}")]
    ConvMismatch { expected: u32, got: u32 },
    /// Invalid segment - unknown command byte.
    #[error("unknown command: 0x{0:02x}")]
    UnknownCommand(u8),
}

// ─── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn create_kcp(conv: u32) -> KCP {
        KCP::new(conv, 0, move |_data: Bytes| {})
    }

    #[test]
    fn test_kcp_create() {
        let kcp = create_kcp(1);
        assert_eq!(kcp.conv(), 1);
        assert_eq!(kcp.rx_rto(), RTO_DEFAULT);
        assert_eq!(kcp.mss(), (MTU - KCP_OVERHEAD) as u32);
    }

    #[test]
    fn test_kcp_send_recv() {
        let mut kcp = create_kcp(1);
        kcp.send(b"hello kcp").unwrap();
        assert_eq!(kcp.snd_queue_len(), 1);
        // After update + flush should move to snd_buf
        kcp.update(100);
        assert_eq!(kcp.snd_queue_len(), 0);
    }

    #[test]
    fn test_kcp_set_nodelay() {
        let mut kcp = create_kcp(1);
        kcp.set_nodelay(1, 10, 2, 1);
        assert_eq!(kcp.rx_minrto, RTO_NODELAY_MIN); // 30
        assert_eq!(kcp.nodelay, 1);
        assert_eq!(kcp.fastresend, 2);
        assert_eq!(kcp.nocwnd, 1);
    }

    #[test]
    fn test_kcp_set_mtu() {
        let mut kcp = create_kcp(1);
        assert!(kcp.set_mtu(500));
        assert_eq!(kcp.mtu(), 500);
        assert_eq!(kcp.mss(), 500 - KCP_OVERHEAD as u32);
        assert!(!kcp.set_mtu(20)); // Too small
    }

    #[test]
    fn test_kcp_wait_send() {
        let mut kcp = create_kcp(1);
        assert_eq!(kcp.wait_send(), 0);
        kcp.send(b"test").unwrap();
        assert_eq!(kcp.wait_send(), 1);
    }

    #[test]
    fn test_kcp_wnd_unused() {
        let kcp = create_kcp(1);
        assert_eq!(kcp.wnd_unused(), KCP_DEFAULT_WND as u16);
    }

    #[test]
    fn test_kcp_input_conv_mismatch() {
        let mut kcp = create_kcp(42);
        // Craft a packet with conv=99 (wrong)
        let mut buf = Vec::with_capacity(KCP_OVERHEAD);
        buf.extend_from_slice(&99u32.to_le_bytes()); // conv = 99
        buf.push(Command::Push as u8);
        buf.push(0u8); // frg
        buf.extend_from_slice(&512u16.to_le_bytes()); // wnd
        buf.extend_from_slice(&100u32.to_le_bytes()); // ts
        buf.extend_from_slice(&1u32.to_le_bytes()); // sn
        buf.extend_from_slice(&0u32.to_le_bytes()); // una
        buf.extend_from_slice(&0u32.to_le_bytes()); // len

        let result = kcp.input(&buf, false);
        assert!(result.is_err()); // conv mismatch
    }

    #[test]
    fn test_input_rejects_wrapping_payload_length() {
        let mut kcp = create_kcp(1);
        let mut buf = vec![0u8; KCP_OVERHEAD];
        buf[..4].copy_from_slice(&1u32.to_le_bytes());
        buf[4] = Command::Push as u8;
        buf[20..].copy_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(
            kcp.input_no_flush(&buf, false),
            Err(KcpError::InvalidLength)
        );
    }

    #[test]
    fn test_peeksize_handles_256_fragment_queue() {
        let mut kcp = create_kcp(1);
        for i in 0..=u8::MAX {
            let mut seg = Segment::with_capacity(1);
            seg.frg = u8::MAX - i;
            seg.len = 1;
            seg.data.extend_from_slice(&[0]);
            kcp.rcv_queue.push_back(seg);
        }
        assert_eq!(kcp.peeksize(), Some(usize::from(u8::MAX) + 1));
    }

    #[test]
    fn test_pending_flush_is_consumed_and_reset() {
        let mut kcp = create_kcp(1);
        let mut buf = vec![0u8; KCP_OVERHEAD];
        buf[..4].copy_from_slice(&1u32.to_le_bytes());
        buf[4] = Command::Push as u8;
        buf[12..16].copy_from_slice(&0u32.to_le_bytes());

        kcp.input_no_flush(&buf, true).unwrap();
        assert_ne!(kcp.pending_flush_flag(), 0);
        kcp.flush_if_pending(100);
        assert_eq!(kcp.pending_flush_flag(), 0);
        assert_eq!(kcp.acklist_len(), 0);
        assert_eq!(kcp.flush_if_pending(100), kcp.interval());

        kcp.input_no_flush(&buf, true).unwrap();
        assert_ne!(kcp.pending_flush_flag(), 0);
    }

    #[test]
    fn test_parse_ack_wrap_uses_index_and_sn_verification() {
        let mut kcp = create_kcp(1);
        kcp.snd_una = u32::MAX - 1;
        kcp.snd_nxt = 2;
        for sn in [u32::MAX - 1, u32::MAX, 0, 1] {
            let mut seg = Segment::with_capacity(0);
            seg.sn = sn;
            kcp.snd_buf.push_back(seg);
        }

        kcp.parse_ack(0);
        assert!(kcp.snd_buf[2].acked);
        kcp.parse_ack(1);
        assert!(kcp.snd_buf[3].acked);

        // A valid wrapping index must still verify the actual SN before
        // marking a malformed/gapped deque entry as acknowledged.
        kcp.snd_buf[2].sn = 7;
        kcp.snd_buf[2].acked = false;
        kcp.parse_ack(0);
        assert!(!kcp.snd_buf[2].acked);
    }

    #[test]
    fn test_duplicate_segment_is_returned_to_pool() {
        let mut kcp = create_kcp(1);
        let mut buf = vec![0u8; KCP_OVERHEAD];
        buf[..4].copy_from_slice(&1u32.to_le_bytes());
        buf[4] = Command::Push as u8;
        buf[12..16].copy_from_slice(&1u32.to_le_bytes());

        kcp.input_no_flush(&buf, false).unwrap();
        assert_eq!(kcp.pool().len(), 0);
        kcp.input_no_flush(&buf, false).unwrap();
        assert_eq!(kcp.pool().len(), 1);

        let mut out_of_window = kcp.pool.acquire();
        out_of_window.sn = kcp.rcv_nxt.wrapping_add(kcp.rcv_wnd);
        assert!(kcp.parse_data(out_of_window));
        assert_eq!(kcp.pool().len(), 1);
    }

    #[test]
    fn test_kcp_send_fragment_too_large() {
        let mut kcp = create_kcp(1);
        // Create data larger than KCP_MAX_FRAG * mss
        let large_data = vec![0u8; (KCP_MAX_FRAG + 1) as usize * (MTU - KCP_OVERHEAD)];
        let result = kcp.send(&large_data);
        // Go would return -2 for count > 255
        assert!(result.is_err());
    }

    #[test]
    fn snmp_send_recv_counts_upper_bytes() {
        use std::sync::atomic::Ordering;
        use std::sync::{Arc, Mutex};

        crate::snmp::enable();
        // Reset counters so we measure the absolute effect of our operations
        // rather than computing a delta (which races with other tests sharing
        // the process-global DEFAULT_SNMP).
        crate::snmp::DEFAULT_SNMP
            .bytes_sent
            .store(0, Ordering::SeqCst);
        crate::snmp::DEFAULT_SNMP
            .bytes_received
            .store(0, Ordering::SeqCst);

        let mut kcp = create_kcp(7);
        kcp.set_stream_mode(true);
        kcp.send(b"hello-snmp").unwrap();
        assert_eq!(
            crate::snmp::DEFAULT_SNMP.bytes_sent.load(Ordering::SeqCst),
            10,
        );

        let store: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
        let store2 = store.clone();
        let mut a = KCP::new(8, 0, move |data: bytes::Bytes| {
            store2.lock().unwrap().push(data.to_vec());
        });
        let mut b = KCP::new(8, 0, |_| {});
        a.set_nodelay(1, 10, 2, 1);
        b.set_nodelay(1, 10, 2, 1);
        a.send(b"abcdef").unwrap();
        a.update(10);
        a.flush();
        let pkts = store.lock().unwrap().clone();
        assert!(!pkts.is_empty());
        for p in &pkts {
            b.input(p, true).unwrap();
        }
        b.update(20);
        let got = b.recv().unwrap();
        assert_eq!(&got[..], b"abcdef");
        assert!(
            crate::snmp::DEFAULT_SNMP
                .bytes_received
                .load(Ordering::SeqCst)
                >= 6,
        );
    }

    /// Regression test: stream-mode append must not panic when the second
    /// `send()` writes more data than the first segment's payload.
    ///
    /// Before the fix, `send()` did `seg.payload = seg.data.split_to(...).freeze()`
    /// which emptied `seg.data`. The next `send()` in stream mode then appended
    /// to the empty `data` but set `len` to the new (larger) length, while
    /// `payload` still held the old (shorter) data. On `encode()`,
    /// `payload[..len]` panicked with "range end index out of range".
    #[test]
    fn test_stream_mode_append_no_panic() {
        use std::sync::{Arc, Mutex};

        let store: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
        let store2 = store.clone();
        let mut kcp = KCP::new(1, 0, move |data: bytes::Bytes| {
            store2.lock().unwrap().push(data.to_vec());
        });
        kcp.set_stream_mode(true);
        // Use a large MTU so the second send fits in one segment via append
        kcp.set_mtu(1400);
        let mss = kcp.mss() as usize;

        // First send: small payload (e.g. 100 bytes) → creates a segment
        let small = vec![0xAAu8; 100];
        kcp.send(&small).unwrap();
        assert_eq!(kcp.snd_queue_len(), 1);

        // Second send: larger than the first payload but still ≤ mss.
        // In stream mode this appends to the last segment in snd_queue.
        // With the old bug, payload stayed at 100 bytes but len was set to 500,
        // causing encode() to panic on payload[..500].
        let large = vec![0xBBu8; 500];
        kcp.send(&large).unwrap();

        // The segment should now contain 100 + 500 = 600 bytes.
        // Flush triggers encode() — must not panic.
        kcp.update(100);

        // Verify the encoded data round-trips correctly
        let pkts = store.lock().unwrap().clone();
        assert!(!pkts.is_empty(), "flush should produce output packets");

        // No packet may exceed MTU (1400).  An earlier intermediate fix
        // computed capacity from `data.len()` (which was 0 after split_to)
        // instead of the real payload length, letting segments grow past
        // MSS and producing oversized UDP packets.
        for (i, p) in pkts.iter().enumerate() {
            assert!(p.len() <= 1400, "pkt {} exceeds MTU: {}", i, p.len());
        }

        // Feed packets into a receiver and check data integrity
        let mut recv_kcp = create_kcp(1);
        recv_kcp.set_stream_mode(true);
        for p in &pkts {
            recv_kcp.input(p, false).unwrap();
        }
        let got = recv_kcp.recv().unwrap();
        assert_eq!(got.len(), 600);
        assert_eq!(&got[..100], &small[..]);
        assert_eq!(&got[100..], &large[..]);

        // Also test the case where the second send exceeds mss (creates new
        // segments after appending). This is the exact scenario from the bug
        // report where len=1326 and payload.len()=692.
        let _ = mss; // suppress unused warning
        let huge = vec![0xCCu8; mss * 2]; // 2 full segments worth
        kcp.send(&huge).unwrap();
        kcp.update(200);
    }

    /// Inline writes must not consume ACKs. Immediate inbound flushes and the
    /// serialized protocol deadline are the ACK-emitting paths.
    #[test]
    fn test_ack_emission_single_owner() {
        use std::sync::{Arc, Mutex};

        let store: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
        let store2 = store.clone();
        let mut kcp = KCP::new(1, 0, move |data: bytes::Bytes| {
            store2.lock().unwrap().push(data.to_vec());
        });

        // Feed one Push segment → queues an ACK in the acklist.
        let mut buf = Vec::with_capacity(KCP_OVERHEAD);
        buf.extend_from_slice(&1u32.to_le_bytes()); // conv = 1
        buf.push(Command::Push as u8); // cmd = PUSH
        buf.push(0u8); // frg
        buf.extend_from_slice(&512u16.to_le_bytes()); // wnd
        buf.extend_from_slice(&100u32.to_le_bytes()); // ts
        buf.extend_from_slice(&1u32.to_le_bytes()); // sn = 1
        buf.extend_from_slice(&0u32.to_le_bytes()); // una
        buf.extend_from_slice(&0u32.to_le_bytes()); // len = 0
        kcp.input_no_flush(&buf, true).unwrap();

        let count_acks = |store: &Arc<Mutex<Vec<Vec<u8>>>>| {
            store
                .lock()
                .unwrap()
                .iter()
                .filter(|p| p.len() >= 5 && p[4] == Command::Ack as u8)
                .count()
        };

        // flush_data_only must NOT emit ACKs (used by write path + flush loop).
        store.lock().unwrap().clear();
        kcp.flush_data_only();
        let after_data_only = count_acks(&store);
        assert_eq!(
            after_data_only, 0,
            "flush_data_only must not drain the acklist (single-owner ACK); got {after_data_only} ACK segs"
        );

        // The real flush must emit the pending ACK.
        store.lock().unwrap().clear();
        kcp.flush();
        let after_flush = count_acks(&store);
        assert!(
            after_flush >= 1,
            "flush() must emit the queued ACK; got {after_flush} ACK segs"
        );
    }

    /// Fast retransmit: when `fastack >= fastresend` (2 duplicate ACKs),
    /// the lost segment is retransmitted immediately via the fast retransmit
    /// path, not RTO timeout. This matches Go kcp-go's flush logic exactly.
    ///
    /// Steps:
    /// 1. Sender sends 3 data segments (SN 0, 1, 2)
    /// 2. Receiver gets segments 1 and 2 (segment 0 is "lost")
    /// 3. Receiver flushes → sends ACKs for SN 1 and 2
    /// 4. Sender processes the ACKs → fastack[0] incremented to 2
    /// 5. Sender flushes → segment 0 is fast-retransmitted (not RTO)
    #[test]
    fn test_fast_retransmit_fires_on_duplicate_acks() {
        use std::sync::{Arc, Mutex};

        crate::snmp::enable();
        crate::snmp::DEFAULT_SNMP
            .fast_retrans
            .store(0, std::sync::atomic::Ordering::SeqCst);
        crate::snmp::DEFAULT_SNMP
            .lost_segs
            .store(0, std::sync::atomic::Ordering::SeqCst);

        let sender_out: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
        let sender_out2 = sender_out.clone();
        let mut sender = KCP::new(42, 0, move |data: bytes::Bytes| {
            sender_out2.lock().unwrap().push(data.to_vec());
        });

        let receiver_out: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
        let receiver_out2 = receiver_out.clone();
        let mut receiver = KCP::new(42, 0, move |data: bytes::Bytes| {
            receiver_out2.lock().unwrap().push(data.to_vec());
        });

        // Fast3: nodelay=1, interval=10, resend=2, nc=1
        sender.set_nodelay(1, 10, 2, 1);
        receiver.set_nodelay(1, 10, 2, 1);
        // Disable stream mode so each send() creates a separate segment
        sender.set_stream_mode(false);
        receiver.set_stream_mode(false);

        // Step 1: Sender sends 3 small data segments
        sender.send(b"AAA").unwrap();
        sender.send(b"BBB").unwrap();
        sender.send(b"CCC").unwrap();

        // Flush at t=100 → moves segments to snd_buf, sends them
        sender.update(100);
        sender.flush();

        // Extract individual KCP segments from wire packets (a wire packet
        // may contain multiple KCP segments packed up to MTU).
        fn extract_segments(wire_pkts: &[Vec<u8>]) -> Vec<Vec<u8>> {
            let mut segs = Vec::new();
            for pkt in wire_pkts {
                let mut off = 0;
                while off + KCP_OVERHEAD <= pkt.len() {
                    let length = u32::from_le_bytes([
                        pkt[off + 20],
                        pkt[off + 21],
                        pkt[off + 22],
                        pkt[off + 23],
                    ]) as usize;
                    let seg = pkt[off..off + KCP_OVERHEAD + length].to_vec();
                    segs.push(seg);
                    off += KCP_OVERHEAD + length;
                }
            }
            segs
        }

        let sender_segs = extract_segments(&sender_out.lock().unwrap().clone());
        let push_segs: Vec<_> = sender_segs
            .iter()
            .filter(|s| s.len() >= 5 && s[4] == Command::Push as u8)
            .collect();
        assert!(
            push_segs.len() >= 3,
            "expected at least 3 Push segments, got {}",
            push_segs.len()
        );

        // Step 2: Receiver processes segments 1 and 2 (skip segment 0 = "lost")
        for (i, seg) in push_segs.iter().enumerate() {
            if i == 0 {
                continue; // "lose" segment 0
            }
            receiver.input_no_flush(seg, false).unwrap();
        }

        // Step 3: Receiver flushes → sends ACKs for received segments
        receiver.update(110);
        receiver.flush();

        let receiver_segs = extract_segments(&receiver_out.lock().unwrap().clone());
        let ack_segs: Vec<_> = receiver_segs
            .iter()
            .filter(|s| s.len() >= 5 && s[4] == Command::Ack as u8)
            .collect();

        // Receiver should have sent at least 2 ACKs (for segments 1 and 2)
        assert!(
            ack_segs.len() >= 2,
            "expected at least 2 ACK segments, got {}",
            ack_segs.len()
        );

        // Step 4: Sender processes the ACKs
        for seg in &ack_segs {
            sender.input_no_flush(seg, false).unwrap();
        }

        // Step 5: Sender flushes → should fast retransmit segment 0
        sender_out.lock().unwrap().clear();
        sender.update(120);
        sender.flush();

        let fast_retrans = crate::snmp::DEFAULT_SNMP
            .fast_retrans
            .load(std::sync::atomic::Ordering::SeqCst);
        let lost = crate::snmp::DEFAULT_SNMP
            .lost_segs
            .load(std::sync::atomic::Ordering::SeqCst);

        assert!(
            fast_retrans > 0,
            "expected fast_retrans > 0, got {fast_retrans}; lost={lost}"
        );
        assert_eq!(lost, 0, "expected 0 RTO-based retransmissions, got {lost}");
    }
}
