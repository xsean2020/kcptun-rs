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

use crate::segment::SegmentPool;
use crate::segment::{
    Command, Segment, KCP_ASK_SEND, KCP_ASK_TELL, KCP_DEFAULT_WND, KCP_MAX_FRAG, KCP_OVERHEAD, MTU,
};
use crate::snmp::{self as snmp, DEFAULT_SNMP};

// ─── Constants (matching Go kcp-go v5) ─────────────────────────────────────

/// No delay min RTO (enabled by NoDelay)
const IKCP_RTO_NDL: u32 = 30;
/// Normal min RTO
const IKCP_RTO_MIN: u32 = 100;
/// Default RTO (200ms, matching Go)
const IKCP_RTO_DEF: u32 = 200;
/// Max RTO cap
const IKCP_RTO_MAX: u32 = 60000;

/// The first probe interval (500ms, matching Go)
pub(crate) const IKCP_PROBE_INIT: u32 = 500;
/// Max probe interval (120s, matching Go)
pub const IKCP_PROBE_LIMIT: u32 = 120000;

/// After this many retransmits, mark the connection as dead
const IKCP_DEADLINK: u32 = 20;

/// Slow-start threshold minimum
const IKCP_THRESH_MIN: u32 = 2;

/// Initial slow-start threshold
const KCP_THRESHOLD_INIT: u32 = 2;

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

    // ── Congestion control ──
    dead_link: u32,
    incr: u32,

    // ── ACK list ──
    acklist: Vec<(u32, u32)>, // (sn, ts)

    // ── Output buffer ──
    buffer: BytesMut,

    // ── Callbacks ──
    output: Box<dyn FnMut(Bytes) + Send>,

    // ── Segment pool ──
    pool: SegmentPool,
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
/// Matches the original KCP C function `_itimediff(later, earlier)`.
#[inline]
fn _itimediff(later: u32, earlier: u32) -> i32 {
    later.wrapping_sub(earlier) as i32
}

impl KCP {
    /// Create a new KCP instance.
    ///
    /// `conv` is the conversation ID, `token` is an additional identifier, and
    /// `output` is called whenever the KCP instance has bytes to send over the
    /// wire.
    pub fn new(conv: u32, token: u32, output: Box<dyn FnMut(Bytes) + Send>) -> Self {
        let mtu = MTU as u32;
        KCP {
            conv,
            token,
            state: 0,
            snd_queue: VecDeque::with_capacity(128),
            snd_buf: VecDeque::with_capacity(128),
            snd_nxt: 0,
            snd_una: 0,
            rcv_queue: VecDeque::with_capacity(128),
            rcv_buf: VecDeque::with_capacity(128),
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
            rx_rto: IKCP_RTO_DEF,
            rx_minrto: IKCP_RTO_MIN,
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
            dead_link: IKCP_DEADLINK,
            incr: 0,
            acklist: Vec::with_capacity(64),
            buffer: BytesMut::with_capacity(MTU),
            output,
            pool: SegmentPool::new(4096),
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

    /// Set the maximum segment size.
    #[inline]
    pub fn set_mss(&mut self, mss: u32) {
        self.mss = mss;
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
            self.rx_minrto = IKCP_RTO_NDL; // Go: nodelay → rx_minrto = 30
        } else {
            self.rx_minrto = IKCP_RTO_MIN; // Go: no nodelay → rx_minrto = 100
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
            (data.len() as u32 + self.mss - 1) / self.mss
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
        let peeksize = self.peeksize();
        if peeksize < 0 {
            return Err(KcpError::NoData);
        }

        let fast_recover = self.rcv_queue.len() as u32 >= self.rcv_wnd;

        // Merge fragments
        let mut data = BytesMut::with_capacity(peeksize as usize);
        loop {
            match self.rcv_queue.pop_front() {
                Some(seg) => {
                    let size = cmp::min(seg.len as usize, seg.data.len());
                    if size > 0 {
                        data.extend_from_slice(&seg.data[..size]);
                    }
                    self.pool.release(seg);
                    if data.len() as i32 >= peeksize {
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
        let peeksize = self.peeksize();
        if peeksize < 0 {
            return Err(KcpError::NoData);
        }

        let fast_recover = self.rcv_queue.len() as u32 >= self.rcv_wnd;

        // ── Fast path: single-segment message (frg == 0) ──
        // This is the common case in stream mode. Zero-copy: just
        // freeze the segment's BytesMut data and return a slice.
        if peeksize == self.rcv_queue.front().map(|s| s.len as i32).unwrap_or(-1) {
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
        let mut data = BytesMut::with_capacity(peeksize as usize);
        loop {
            match self.rcv_queue.pop_front() {
                Some(seg) => {
                    let size = cmp::min(seg.len as usize, seg.data.len());
                    if size > 0 {
                        data.extend_from_slice(&seg.data[..size]);
                    }
                    self.pool.release(seg);
                    if data.len() as i32 >= peeksize {
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
    /// Returns -1 if no data is available.
    pub fn peeksize(&self) -> i32 {
        match self.rcv_queue.front() {
            None => return -1,
            Some(seg) => {
                if seg.frg == 0 {
                    return seg.len as i32;
                }
            }
        }

        if self.rcv_queue.is_empty() {
            return -1;
        }

        let count = self.rcv_queue.len();
        if count as u8 <= self.rcv_queue[0].frg {
            return -1;
        }

        let mut length = 0i32;
        for seg in &self.rcv_queue {
            length += seg.len as i32;
            if seg.frg == 0 {
                break;
            }
        }
        length
    }

    // ── Ack processing ────────────────────────────────────────────────────

    fn parse_ack(&mut self, sn: u32) {
        if _itimediff(sn, self.snd_una) < 0 || _itimediff(sn, self.snd_nxt) >= 0 {
            return;
        }

        for seg in &mut self.snd_buf {
            if sn == seg.sn {
                seg.acked = true;
                // Go: recycleSegment frees the data buffer but leaves the entry
                // so we don't need to remove from snd_buf here.
                break;
            }
            if _itimediff(sn, seg.sn) < 0 {
                break;
            }
        }
    }

    fn parse_fastack(&mut self, sn: u32, ts: u32) -> bool {
        if _itimediff(sn, self.snd_una) < 0 || _itimediff(sn, self.snd_nxt) >= 0 {
            return false;
        }

        let mut should_fast_ack = false;
        for seg in &mut self.snd_buf {
            if _itimediff(sn, seg.sn) < 0 {
                break;
            } else if sn != seg.sn && _itimediff(seg.ts, ts) <= 0 {
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
        let mut count = 0;
        for seg in &self.snd_buf {
            if _itimediff(una, seg.sn) > 0 {
                count += 1;
            } else {
                break;
            }
        }
        // Drain the first `count` segments in one pass (avoids O(n²) remove(0))
        if count > 0 {
            let drained: Vec<Segment> = self.snd_buf.drain(..count).collect();
            for seg in drained {
                self.pool.release(seg);
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
        self.acklist.push((sn, ts));
    }

    // ── Input processing ─────────────────────────────────────────────────

    /// Feed incoming raw data into the KCP state machine.
    ///
    /// `ack_no_delay` matches Go's `ackNoDelay` parameter — when true, ACKs
    /// are flushed immediately.
    pub fn input(&mut self, data: &[u8], ack_no_delay: bool) -> Result<usize, KcpError> {
        let snd_una = self.snd_una;
        let mut offset = 0usize;
        let mut latest_ts = 0u32;
        let mut update_rtt = false;
        let mut in_segs = 0u64;
        let mut flush_segments = 0u32;

        while offset + KCP_OVERHEAD <= data.len() {
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

            if offset + KCP_OVERHEAD + length > data.len() {
                return Err(KcpError::InvalidLength);
            }

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
                    if _itimediff(sn, self.rcv_nxt + self.rcv_wnd) < 0 {
                        self.ack_push(sn, ts);
                        if _itimediff(sn, self.rcv_nxt) >= 0 {
                            // Create segment from received data
                            let mut seg = self.pool.acquire();
                            seg.conv = conv;
                            seg.cmd = cmd as u8;
                            seg.frg = frg;
                            seg.wnd = wnd;
                            seg.ts = ts;
                            seg.sn = sn;
                            seg.una = una;
                            seg.len = length as u32;
                            if length > 0 {
                                seg.data.extend_from_slice(
                                    &data[offset + KCP_OVERHEAD..offset + KCP_OVERHEAD + length],
                                );
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
            offset += KCP_OVERHEAD + length;
        }

        // Update SNMP
        if in_segs > 0 {
            snmp::add(&DEFAULT_SNMP.in_segs, in_segs);
        }

        // Update RTT with the latest ts (matching Go: only for regular packets)
        if update_rtt {
            if let Some(current_ms) = self.current_ms() {
                if _itimediff(current_ms, latest_ts) >= 0 {
                    self.update_ack(_itimediff(current_ms, latest_ts));
                }
            }
        }

        // Congestion window update (matching Go: nocwnd check)
        if self.nocwnd == 0 {
            if _itimediff(self.snd_una, snd_una) > 0 {
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
                                self.cwnd = (self.incr + mss - 1) / mss;
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

        // Flush triggers (matching Go)
        if flush_segments != 0 {
            self.flush();
        } else if self.acklist.len() >= (self.mtu / KCP_OVERHEAD as u32) as usize {
            self.flush();
        } else if ack_no_delay && !self.acklist.is_empty() {
            self.flush();
        }

        Ok(offset)
    }

    /// Insert a segment into the receive buffer and try to move data to the
    /// receive queue. Matches Go's `parse_data`.
    ///
    /// Returns `true` if the segment was a duplicate (or outside window after
    /// already being acked), matching Go's `repeat` flag for SNMP.RepeatSegs.
    fn parse_data(&mut self, newseg: Segment) -> bool {
        let sn = newseg.sn;

        // Check if outside receive window
        if _itimediff(sn, self.rcv_nxt + self.rcv_wnd) >= 0 || _itimediff(sn, self.rcv_nxt) < 0 {
            return true;
        }

        // Check for duplicate by looking through rcv_buf
        let is_dup = self.rcv_buf.iter().any(|s| s.sn == sn);
        if !is_dup {
            // Insert in sorted order
            let pos = self.rcv_buf.iter().position(|s| _itimediff(s.sn, sn) > 0);
            match pos {
                Some(p) => self.rcv_buf.insert(p, newseg),
                None => self.rcv_buf.push_back(newseg),
            }
        }

        // Move available data from rcv_buf → rcv_queue
        self.move_receive_buffer();
        is_dup
    }

    /// Move segments from the receive buffer into the receive queue when
    /// they are in-order and within the receive window.
    fn move_receive_buffer(&mut self) {
        while let Some(seg) = self.rcv_buf.pop_front() {
            if seg.sn == self.rcv_nxt && (self.rcv_queue.len() as u32) < self.rcv_wnd {
                self.rcv_nxt += 1;
                self.rcv_queue.push_back(seg);
            } else {
                // Put back the segment we popped and stop
                self.rcv_buf.push_front(seg);
                break;
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
        self.rx_rto = rto.clamp(self.rx_minrto, IKCP_RTO_MAX);
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

        let mut slap = _itimediff(current, self.ts_flush);

        // Clock jump detection (matching Go)
        if slap >= 10000 || slap < -10000 {
            self.ts_flush = current;
            slap = 0; // Go resets slap to 0 after jump (kcp.go:1017)
        }

        if slap >= 0 {
            self.ts_flush = self.ts_flush.wrapping_add(self.interval);
            if _itimediff(current, self.ts_flush) >= 0 {
                self.ts_flush = current.wrapping_add(self.interval);
            }
            self.flush()
        } else {
            self.interval
        }
    }

    /// Determine when `update()` should next be called.
    /// Matches Go's `Check()` function.
    pub fn check(&self, current: u32) -> u32 {
        let mut ts_flush = self.ts_flush;
        let tm_flush: i32;
        let mut tm_packet: i32 = 0x7fffffff;

        if self.updated == 0 {
            return current;
        }

        if _itimediff(current, ts_flush) >= 10000 || _itimediff(current, ts_flush) < -10000 {
            ts_flush = current;
        }

        if _itimediff(current, ts_flush) >= 0 {
            return current;
        }

        tm_flush = _itimediff(ts_flush, current);

        for seg in &self.snd_buf {
            let diff = _itimediff(seg.resendts, current);
            if diff <= 0 {
                return current;
            }
            if diff < tm_packet {
                tm_packet = diff;
            }
        }

        let mut minimal = tm_packet as u32;
        if tm_packet >= tm_flush {
            minimal = tm_flush as u32;
        }
        if minimal >= self.interval {
            minimal = self.interval;
        }
        current.wrapping_add(minimal)
    }

    /// Force-flush all pending data.
    ///
    /// Returns `next_update` — the milliseconds until the next meaningful
    /// event (nearest RTO or interval), matching Go kcp-go's `flush()` return.
    pub fn flush(&mut self) -> u32 {
        let current = self.current_ms().unwrap_or(0);
        let mut next_update = self.interval;

        // Build single-use segment for ACK/WASK/WINS headers
        let mut ack_seg = self.pool.acquire();
        ack_seg.conv = self.conv;
        ack_seg.cmd = Command::Ack as u8;
        ack_seg.wnd = self.wnd_unused();
        ack_seg.una = self.rcv_nxt;

        let mtu = self.mtu as usize;

        // Helper: flush the output buffer and return remaining capacity
        let flush_buf = |buf: &mut BytesMut, output: &mut Box<dyn FnMut(Bytes) + Send>| {
            if !buf.is_empty() {
                let data = buf.split().freeze();
                output(data);
            }
        };

        // ── Flush ACKs ──
        if !self.acklist.is_empty() {
            let n = self.acklist.len();
            for i in 0..n {
                let (ack_sn, ack_ts) = self.acklist[i];
                // Check space
                if self.buffer.len() + KCP_OVERHEAD > mtu {
                    flush_buf(&mut self.buffer, &mut self.output);
                }
                // Filter jitter caused by bufferbloat (matching Go)
                if _itimediff(ack_sn, self.rcv_nxt) >= 0 || i == n - 1 {
                    ack_seg.sn = ack_sn;
                    ack_seg.ts = ack_ts;
                    ack_seg.encode(&mut self.buffer);
                }
            }
            self.acklist.clear();
        }

        // ── Window probing ──
        if self.rmt_wnd == 0 {
            if self.probe_wait == 0 {
                self.probe_wait = IKCP_PROBE_INIT;
                self.ts_probe = current + self.probe_wait;
            } else if _itimediff(current, self.ts_probe) >= 0 {
                if self.probe_wait < IKCP_PROBE_INIT {
                    self.probe_wait = IKCP_PROBE_INIT;
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
        let mut new_segs_count = 0;
        while _itimediff(self.snd_nxt, self.snd_una + cwnd) < 0 {
            match self.snd_queue.pop_front() {
                Some(mut newseg) => {
                    newseg.conv = self.conv;
                    newseg.cmd = Command::Push as u8;
                    // SN is assigned here (matching Go: when moving to snd_buf)
                    newseg.sn = self.snd_nxt;
                    self.snd_buf.push_back(newseg);
                    self.snd_nxt += 1;
                    new_segs_count += 1;
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
        let mut early_retrans_segs = 0u64;

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
                // Fast retransmit
                needsend = true;
                seg.fastack = 0xFFFFFFFF;
                seg.rto = self.rx_rto;
                seg.resendts = current + seg.rto;
                change += 1;
                fast_retrans_segs += 1;
            } else if seg.fastack > 0 && seg.fastack != 0xFFFFFFFF && new_segs_count == 0 {
                // Early retransmit (matching Go)
                needsend = true;
                seg.fastack = 0xFFFFFFFF;
                seg.rto = self.rx_rto;
                seg.resendts = current + seg.rto;
                change += 1;
                early_retrans_segs += 1;
            } else if _itimediff(current, seg.resendts) >= 0 {
                // RTO timeout
                needsend = true;
                if self.nodelay == 0 {
                    seg.rto += self.rx_rto; // Linear backoff
                } else {
                    seg.rto += self.rx_rto / 2; // Half-linear backoff
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

                // Freeze payload for zero-copy retransmit sharing.
                // Done here (not in send()) so stream-mode append in send()
                // can still extend seg.data before the segment is transmitted.
                if seg.payload.is_empty() && !seg.data.is_empty() {
                    seg.payload = seg.data.split_to(seg.data.len()).freeze();
                }

                let need = KCP_OVERHEAD + seg.len as usize;
                if self.buffer.len() + need > mtu {
                    flush_buf(&mut self.buffer, &mut self.output);
                }

                // Encode header + payload directly into self.buffer
                // (encode() writes both header and data, avoiding per-seg Vec alloc)
                seg.encode(&mut self.buffer);

                // Update SNMP
                snmp::add(&DEFAULT_SNMP.out_segs, 1);

                // Dead link check (matching Go)
                if seg.xmit >= self.dead_link {
                    self.state = 0xFFFFFFFF;
                }
            }

            // Track nearest RTO for nextUpdate (matching Go kcp-go)
            if !seg.acked {
                let rto = _itimediff(seg.resendts, current);
                if rto > 0 && (rto as u32) < next_update {
                    next_update = rto as u32;
                }
            }
        }

        // Update retransmission stats (matching Go)
        let retrans_sum = lost_segs + fast_retrans_segs + early_retrans_segs;
        if retrans_sum > 0 {
            snmp::add(&DEFAULT_SNMP.retrans_segs, retrans_sum);
        }
        if lost_segs > 0 {
            snmp::add(&DEFAULT_SNMP.lost_segs, lost_segs);
        }
        if fast_retrans_segs > 0 {
            snmp::add(&DEFAULT_SNMP.fast_retrans, fast_retrans_segs);
        }
        if early_retrans_segs > 0 {
            snmp::add(&DEFAULT_SNMP.early_retrans, early_retrans_segs);
        }

        // ── Congestion control (matching Go) ──
        if self.nocwnd == 0 {
            // Rate halving (RFC 6937)
            if change > 0 {
                let inflight = self.snd_nxt - self.snd_una;
                self.ssthresh = (inflight / 2).max(IKCP_THRESH_MIN);
                self.cwnd = self.ssthresh + resent;
                self.incr = self.cwnd * self.mss;
            }

            // Congestion control (RFC 5681)
            if lost_segs > 0 {
                self.ssthresh = (cwnd / 2).max(IKCP_THRESH_MIN);
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

        self.pool.release(ack_seg);

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

    /// Number of segments in the send buffer.
    #[inline]
    pub fn snd_buf_len(&self) -> usize {
        self.snd_buf.len()
    }

    /// Number of segments in the receive queue.
    #[inline]
    pub fn rcv_queue_len(&self) -> usize {
        self.rcv_queue.len()
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
        self.state == 0xFFFFFFFF
    }

    /// Get the current RTO value.
    #[inline]
    pub fn rx_rto(&self) -> u32 {
        self.rx_rto
    }

    /// Get the current SRTT.
    #[inline]
    pub fn rx_srtt(&self) -> i32 {
        self.rx_srtt
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

    /// Get the current timestamp (for matching Go).
    fn current_ms(&self) -> Option<u32> {
        Some(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?
                .as_millis() as u32,
        )
    }

    /// Get the maximum segment size.
    #[inline]
    pub fn mss(&self) -> u32 {
        self.mss
    }

    /// Get the remote window size.
    #[inline]
    pub fn rmt_wnd(&self) -> u32 {
        self.rmt_wnd
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

    /// Reset the KCP state.
    pub fn reset(&mut self) {
        self.snd_queue.clear();
        self.snd_buf.clear();
        self.snd_nxt = 0;
        self.snd_una = 0;
        self.rcv_queue.clear();
        self.rcv_buf.clear();
        self.rcv_nxt = 0;
        self.ts_flush = 0;
        self.rx_rto = IKCP_RTO_DEF;
        self.rx_srtt = 0;
        self.rx_rttvar = 0;
        self.cwnd = 0;
        self.ssthresh = KCP_THRESHOLD_INIT;
        self.probe = 0;
        self.probe_wait = 0;
        self.ts_probe = 0;
        self.incr = 0;
        self.state = 0;
        self.acklist.clear();
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

    /// Get next expected receive sequence number.
    #[inline]
    pub fn rcv_nxt(&self) -> u32 {
        self.rcv_nxt
    }
}

// ─── Errors ──────────────────────────────────────────────────────────────

/// Errors produced by the KCP state machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KcpError {
    /// No data available to receive.
    NoData,
    /// Too many fragments for a single send.
    TooManyFragments,
    /// Invalid segment - length exceeds remaining data.
    InvalidLength,
    /// Invalid segment - conversation ID mismatch.
    ConvMismatch { expected: u32, got: u32 },
    /// Invalid segment - unknown command byte.
    UnknownCommand(u8),
    /// Generic invalid segment.
    InvalidSegment,
    /// Buffer too small for received data.
    BufferTooSmall,
}

impl fmt::Display for KcpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KcpError::NoData => write!(f, "no data available"),
            KcpError::TooManyFragments => write!(f, "too many fragments"),
            KcpError::InvalidLength => write!(f, "invalid length"),
            KcpError::ConvMismatch { expected, got } => {
                write!(f, "conv mismatch: expected={}, got={}", expected, got)
            }
            KcpError::UnknownCommand(cmd) => write!(f, "unknown command: 0x{:02x}", cmd),
            KcpError::InvalidSegment => write!(f, "invalid segment"),
            KcpError::BufferTooSmall => write!(f, "buffer too small"),
        }
    }
}

impl std::error::Error for KcpError {}

// ─── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn create_kcp(conv: u32) -> KCP {
        KCP::new(conv, 0, Box::new(move |_data: Bytes| {}))
    }

    #[test]
    fn test_kcp_create() {
        let kcp = create_kcp(1);
        assert_eq!(kcp.conv(), 1);
        assert_eq!(kcp.rx_rto(), IKCP_RTO_DEF);
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
        assert_eq!(kcp.rx_minrto, IKCP_RTO_NDL); // 30
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
        let before_s = crate::snmp::DEFAULT_SNMP.bytes_sent.load(Ordering::SeqCst);
        let before_r = crate::snmp::DEFAULT_SNMP
            .bytes_received
            .load(Ordering::SeqCst);

        let mut kcp = create_kcp(7);
        kcp.set_stream_mode(true);
        kcp.send(b"hello-snmp").unwrap();
        let after_s = crate::snmp::DEFAULT_SNMP.bytes_sent.load(Ordering::SeqCst);
        assert_eq!(after_s - before_s, 10);

        let store: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
        let store2 = store.clone();
        let mut a = KCP::new(
            8,
            0,
            Box::new(move |data: bytes::Bytes| {
                store2.lock().unwrap().push(data.to_vec());
            }),
        );
        let mut b = KCP::new(8, 0, Box::new(|_| {}));
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
        let after_r = crate::snmp::DEFAULT_SNMP
            .bytes_received
            .load(Ordering::SeqCst);
        assert!(after_r - before_r >= 6);
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
        let mut kcp = KCP::new(
            1,
            0,
            Box::new(move |data: bytes::Bytes| {
                store2.lock().unwrap().push(data.to_vec());
            }),
        );
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
}
