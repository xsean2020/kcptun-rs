//! Session engine (the user-space `struct sock`): the shared per-connection
//! state (`SharedIoState`) and the background input/flush loops that drive
//! the KCP state machine. `KcpStream` (the facade in `conn.rs`) is a thin
//! view over this engine; sharded workers pump sessions via
//! `KcpStream::feed_raw_batch`/`tick` (`background_input = false`).

//! Split out of `conn.rs` (engine/facade layering); no behavior change.

use super::*;
use super::raw_queue::{READ_PREFETCH_MAX_BYTES, READ_PREFETCH_MAX_MESSAGES};

/// Slot count per `try_recv_batch` drain call in the input loop (matches the
/// listener's recvmmsg batch; the transport fills up to this many per call).
const INPUT_BATCH_GROW: usize = 16;
/// Max datagrams processed per input-loop cycle. Bounds one `feed_inbound_batch`
/// + deferred flush so a high-rate peer cannot starve the worker (v3 §5.4).
const MAX_INPUT_BATCH: usize = 64;

pub struct SharedIoState {
    pub(crate) transport: Arc<dyn PacketTransport>,
    pub(crate) kcp: Arc<Mutex<KCP>>,
    // read_buf is a small byte-bounded prefetch queue plus a possible partial
    // message when a caller buffer ends mid-read. Remaining complete messages
    // stay in KCP so rcv_wnd remains the primary flow-control boundary.
    pub(crate) read_buf: Mutex<ReadBuffer>,
    /// FIFO of produced wire packets (ACKs / data / probes). The flush loop is
    /// the ONLY drainer + sender — a single owner keeps wire order = flush
    /// order. Multiple concurrent drainers (write path, input loop, flush loop)
    /// interleaved batches on a FIFO link → receiver `rcv_nxt` gaps → spurious
    /// fastack → retransmit storm (256KB@RPS=500).
    pub(crate) raw_packets: Arc<Mutex<RawPacketQueue>>,
    pub(crate) flush_notify: Arc<knet::Notify>,
    pub(crate) write_notify: Arc<knet::Notify>,
    pub(crate) read_notify: Arc<knet::Notify>,
    pub(crate) read_waker: Mutex<Option<Waker>>,
    pub(crate) write_waker: Mutex<Option<Waker>>,
    pub(crate) wait_send: Arc<AtomicUsize>,
    pub(crate) snd_wnd: AtomicUsize,
    pub(crate) acknodelay: AtomicBool,
    pub(crate) remote_addr: SocketAddr,
    /// When true, use `send_batch` / `send_urgent` (connected). Else `*_to(remote)`.
    pub(crate) connected: bool,
    pub(crate) closed: Arc<AtomicBool>,
    /// Cancels the input loop's socket `recv` on `close()` so a silent peer's
    /// task exits immediately instead of waiting out the 100ms poll tick
    /// (removes the ~10 Hz × idle-connection timer churn).
    pub(crate) cancel_token: CancellationToken,
    /// Adopt the conversation ID from the first decrypted KCP segment.
    pub(crate) adopt_conv: AtomicBool,
    /// When false, no background input-loop task is spawned: an external
    /// driver (Acceptor + Worker sharding) feeds inbound via [`KcpStream::feed_input`].
    pub(crate) background_input: bool,
    /// Last successful inbound or outbound user-data activity (monotonic ms).
    pub(crate) last_activity_ms: AtomicU64,
    /// Send token: when `true`, either the flush loop or an inline writer is
    /// draining `raw_packets` + sending via `send_packets_with_fec().await`.
    /// Prevents wire-interleaving when both try to send concurrently.
    /// Acquired via `compare_exchange(false, true)`; released with `store(false)`.
    pub(crate) is_sending: AtomicBool,
    /// Optional FEC encoder (header_offset=0, matching client/server session layout).
    pub(crate) fec_encoder: Option<Mutex<FecEncoder>>,
    /// Optional FEC decoder.
    pub(crate) fec_decoder: Option<Mutex<FecDecoder>>,

    // ── TcpStream-aligned surface ──
    /// Read timeout in ms (`None` = block indefinitely). Honored by
    /// [`KcpStream::read_shared`], `poll_read`, and [`KcpStream::readable`].
    pub(crate) read_timeout: Mutex<Option<u64>>,
    /// Write timeout in ms (`None` = block indefinitely). Honored by
    /// [`KcpStream::write_all_shared`], `poll_write`, and [`KcpStream::writable`].
    pub(crate) write_timeout: Mutex<Option<u64>>,
    /// Mono-ms deadline for a blocked `poll_read` (checked on the next poll).
    pub(crate) read_deadline: Mutex<Option<u64>>,
    /// Mono-ms deadline for a blocked `poll_write` (checked on the next poll).
    pub(crate) write_deadline: Mutex<Option<u64>>,
    /// Last non-transient I/O error from the background loops, surfaced by
    /// [`KcpStream::take_error`].
    pub(crate) last_error: Mutex<Option<io::Error>>,
    /// Last [`KcpStream::set_nodelay`] bool value, for the [`KcpStream::nodelay`] getter.
    pub(crate) nodelay: AtomicBool,
    /// Write side half-closed via [`KcpStream::shutdown`] / `poll_shutdown`.
    pub(crate) write_closed: AtomicBool,
    /// Read side half-closed via [`KcpStream::shutdown`].
    pub(crate) read_closed: AtomicBool,
    /// When true, read/write return WouldBlock instead of parking.
    pub(crate) nonblocking: AtomicBool,

    /// Any conv-valid inbound datagram received (drives connect-timeout's
    /// first-packet wait).
    pub(crate) first_inbound: AtomicBool,
}

impl SharedIoState {
    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Backpressure has room: the send window is below `snd_wnd` (ACKs
    /// are flowing). Writes return partial counts when the KCP window fills,
    /// so there is no second user-space queue to include in this check.
    pub(crate) fn backpressure_relieved(&self) -> bool {
        self.wait_send.load(Ordering::Relaxed) < self.snd_wnd.load(Ordering::Relaxed)
    }

    pub(crate) fn close(&self) {
        if self
            .closed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            self.cancel_token.cancel();
            self.flush_notify.notify_one();
            self.write_notify.notify_waiters();
            self.read_notify.notify_waiters();
            self.wake_writer();
            if let Some(w) = self.read_waker.lock().take() {
                w.wake();
            }
        }
    }

    pub(crate) fn wake_reader(&self) {
        // `notify_one` stores a permit, so a notification that arrives before
        // the reader registers is retained.
        self.read_notify.notify_one();
        let waker = self.read_waker.lock().take();
        if let Some(w) = waker {
            w.wake();
        }
    }

    pub(crate) fn wake_writer(&self) {
        // Wake outside the `write_waker` lock: clone the waker out of the slot
        // (one Arc refcount inc), drop the guard, then wake. The slot keeps
        // its waker, so a re-polling writer's `will_wake` still recognizes
        // itself and does NOT reset the write deadline (a `take()` here would
        // clear it and roll the timeout forward forever). Waking an arbitrary
        // Waker while holding an internal mutex risks re-entrancy and lengthens
        // hold time.
        //
        // Also notify `write_notify` so any task waiting on `notified()` is
        // woken (matches `wake_reader` behavior for consistency).
        let waker = self.write_waker.lock().clone();
        self.write_notify.notify_one();
        if let Some(w) = waker {
            w.wake_by_ref();
        }
    }

    pub(crate) fn drain_raw_packets(&self) -> Vec<Bytes> {
        // Swap `pending`↔`spare` under the lock (no allocation) and hand the
        // accumulated batch to the sender; the recycled buffer becomes the next
        // accumulation target with its capacity preserved.
        self.raw_packets.lock().drain()
    }

    /// Return a drained batch's capacity to the queue for reuse (bounded by
    /// [`MAX_RETAINED_RAW_BATCH`]). Every sender calls this after sending, on
    /// both success and error paths, so a giant burst does not permanently pin
    /// memory and the steady state is allocation-free.
    pub(crate) fn recycle_raw_packets(&self, packets: Vec<Bytes>) {
        self.raw_packets.lock().recycle(packets);
    }

    /// Release the send token and preserve a wake-up for packets queued while
    /// the sender was awaiting UDP I/O. The flush task may have consumed the
    /// original notification while the token was held; without a fresh permit,
    /// those packets wait for the next 10ms KCP timer tick.
    pub(crate) fn finish_sending(&self) {
        self.is_sending.store(false, Ordering::Release);
        if !self.raw_packets.lock().is_empty() {
            self.flush_notify.notify_one();
        }
    }

    /// Synchronous drain-and-flush for the external-worker event loop
    /// (`background_input=false` + `feed_batch`). Drains `raw_packets` and
    /// flushes them via non-blocking `try_send_batch` / `try_send_batch_to`,
    /// **without** an `.await` — the caller is a sync worker event loop
    /// (pipeline §14: `drain_udp_batch` + `flush_ready_sessions`).
    ///
    /// On a partial send or `WouldBlock`, ownership moves to a short async
    /// continuation which sends only the remaining wire suffix while retaining
    /// the single-sender token. This preserves FIFO order without blocking the
    /// synchronous worker event loop.
    ///
    /// Returns `true` if the send token was acquired (caller should NOT
    /// notify the flush loop for the immediate burst). Returns `false` if
    /// another sender holds the token (caller should `flush_notify`).
    pub(crate) fn drain_and_flush_tx(self: &Arc<Self>) -> bool {
        if self
            .is_sending
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false; // flush loop is sending — let it handle our packets
        }
        let packets = self.drain_raw_packets();
        if packets.is_empty() {
            self.finish_sending();
            return true;
        }
        // FEC-expand if configured (sync — no await needed).
        // No FEC: skip the clone, send directly from `packets`.
        let fec_wire = if let Some(ref enc) = self.fec_encoder {
            let mut e = enc.lock();
            Some(fec_expand_packets(&mut e, &packets, 500))
        } else {
            None
        };
        let wire = fec_wire.as_deref().unwrap_or(&packets);
        // Non-blocking send. A partial send (including Ok(0)) or WouldBlock is
        // completed by an async continuation that retains the send token.
        let result = if self.connected {
            self.transport.try_send_batch(wire)
        } else {
            self.transport.try_send_batch_to(wire, self.remote_addr)
        };
        match result {
            Ok(sent) if sent >= wire.len() => {
                // Successfully handed packets to the kernel. Recycle buffers.
                self.recycle_raw_packets(packets);
                self.finish_sending();
                true
            }
            Ok(sent) => {
                // `sendmmsg` may legally accept only a prefix. Keep ownership
                // of the send token and finish the exact remaining wire suffix
                // asynchronously so later KCP output cannot overtake it.
                self.spawn_send_remainder(packets, fec_wire, sent);
                true
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                // Socket send buffer full before the first datagram. Preserve
                // the already-expanded FEC sequence and finish asynchronously.
                self.spawn_send_remainder(packets, fec_wire, 0);
                true
            }
            Err(e) => {
                *self.last_error.lock() = Some(e);
                self.recycle_raw_packets(packets);
                self.finish_sending();
                true // consumed the burst (error logged); no need to re-notify
            }
        }
    }

    /// Finish a partially-sent wire batch without releasing the single-sender
    /// token. `fec_wire` is retained when present so retrying never re-encodes
    /// the KCP packets with new FEC sequence numbers.
    pub(crate) fn spawn_send_remainder(
        self: &Arc<Self>,
        packets: Vec<Bytes>,
        fec_wire: Option<Vec<Bytes>>,
        sent: usize,
    ) {
        let shared = self.clone();
        drop(knet::spawn_task(async move {
            let result = if let Some(ref wire) = fec_wire {
                shared.send_packets(&wire[sent.min(wire.len())..]).await
            } else {
                shared
                    .send_packets(&packets[sent.min(packets.len())..])
                    .await
            };
            if let Err(e) = result {
                *shared.last_error.lock() = Some(e);
            }
            shared.recycle_raw_packets(packets);
            shared.finish_sending();
        }));
    }

    pub(crate) async fn send_packets(&self, packets: &[Bytes]) -> io::Result<()> {
        if packets.is_empty() {
            return Ok(());
        }
        if self.connected {
            self.transport.send_batch(packets).await
        } else {
            self.transport
                .send_batch_to(packets, self.remote_addr)
                .await
        }
    }

    /// Flush a batch of wire packets through the TX path: try non-blocking
    /// `sendmmsg` (Linux) first, fall back to async `send_batch` /
    /// `send_batch_to` on `WouldBlock` (pipeline §15–§16: TX thread
    /// `sendmmsg(batch)`). FEC-expand if an encoder is configured.
    ///
    /// This keeps the flush loop on a fast sync path when the kernel send
    /// buffer has room, avoiding a reactor scheduling hop per burst.
    ///
    /// Returns `Ok(())` on success. On `WouldBlock`, the async send path
    /// runs and its result is returned.
    pub(crate) async fn flush_tx_batch(&self, packets: &[Bytes]) -> io::Result<()> {
        // FEC-expand if configured (sync).
        let wire: Vec<Bytes> = if let Some(ref enc) = self.fec_encoder {
            let mut e = enc.lock();
            fec_expand_packets(&mut e, packets, 500)
        } else {
            // Non-FEC fast path: try_send_batch with the original slice,
            // no clone needed. `try_send_batch` returns Ok(0) on
            // WouldBlock (non-Linux) — treat that as a fallthrough to the
            // async path so the reactor handles the writable() wait.
            let non_fec_result = if self.connected {
                self.transport.try_send_batch(packets)
            } else {
                self.transport.try_send_batch_to(packets, self.remote_addr)
            };
            match non_fec_result {
                Ok(sent) if sent >= packets.len() => return Ok(()),
                Ok(sent) => {
                    // `sendmmsg` can return a partial prefix. Send only the
                    // remaining suffix; replaying the prefix wastes bandwidth
                    // and dropping the suffix creates an artificial KCP loss.
                    return self.send_packets(&packets[sent.min(packets.len())..]).await;
                }
                Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                    return self.send_packets(packets).await;
                }
                Err(e) => return Err(e),
            }
        };
        // FEC path: try non-blocking, fall back to async.
        let fec_result = if self.connected {
            self.transport.try_send_batch(&wire)
        } else {
            self.transport.try_send_batch_to(&wire, self.remote_addr)
        };
        match fec_result {
            Ok(sent) if sent >= wire.len() => Ok(()),
            Ok(sent) => self.send_packets(&wire[sent.min(wire.len())..]).await,
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => self.send_packets(&wire).await,
            Err(e) => Err(e),
        }
    }

    /// Try to acquire the send token (`is_sending`).  If acquired, drain
    /// `raw_packets` and send them inline.  Returns `true` if the send token
    /// was acquired (caller should NOT notify the flush loop — we handled it).
    /// Returns `false` if another sender holds the token (caller should
    /// `flush_notify.notify_one()` to let the flush loop handle it).
    ///
    /// This eliminates the task-scheduling hop of the notify→wake→drain→send
    /// path on the write hot path, matching Go's synchronous `Write` send.
    pub(crate) async fn try_drain_and_send(&self) -> bool {
        if self
            .is_sending
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false; // flush loop is sending — let it handle our packets
        }
        let packets = self.drain_raw_packets();
        if packets.is_empty() {
            self.finish_sending();
            return true; // nothing to send, but we acquired the token (no notify needed)
        }
        // Try non-blocking sendmmsg first (sync fast path); falls back to
        // async send_batch on WouldBlock. Avoids a reactor scheduling hop
        // per burst when the kernel send buffer has room.
        if let Err(e) = self.flush_tx_batch(&packets).await {
            *self.last_error.lock() = Some(e);
        }
        self.recycle_raw_packets(packets);
        self.finish_sending();
        true
    }

    /// Inline `kcp.send` + `kcp.flush` under the KCP lock, matching kcp-go's
    /// `UDPSession.Write`. Returns the number of bytes accepted into the KCP
    /// send window (possibly < `buf.len()` when the window fills mid-buffer
    /// or the per-call chunk cap is reached).
    #[inline]
    pub(crate) fn send_to_kcp(&self, kcp: &mut KCP, buf: &[u8]) -> usize {
        let mss = kcp.mss() as usize;
        // Respect the configured send window for this call, not only at the
        // caller's pre-check.  A single 64 KiB write can span dozens of KCP
        // segments; accepting all of it when only one window slot remains
        // defeats backpressure and creates a latency/RSS spike per writer.
        let queued = kcp.wait_send() as usize;
        let available_segments = self.snd_wnd.load(Ordering::Relaxed).saturating_sub(queued);
        let window_bytes = available_segments.saturating_mul(mss);
        if window_bytes == 0 {
            self.wait_send.store(queued, Ordering::Relaxed);
            return 0;
        }
        let max_chunk = (KCP_MAX_FRAG as usize)
            .saturating_sub(1)
            .saturating_mul(mss)
            .max(mss);
        // Cap total bytes per call to limit KCP mutex hold time.  A 256KB
        // write would otherwise hold the mutex for ~195 kcp.send() iterations
        // + data-only flush, blocking the input loop from processing ACKs.
        // Capping at 64KB (~49 segments) matches Go's Write pattern where the
        // echo loop's 64KB buffer naturally chunks sends.  The caller's
        // write_all loop re-acquires the mutex between chunks.
        let buf = &buf[..buf.len().min(KCP_SEND_CHUNK).min(window_bytes)];
        let mut offset = 0usize;
        while offset < buf.len() {
            let end = (offset + max_chunk).min(buf.len());
            if kcp.send(&buf[offset..end]).is_err() {
                break;
            }
            offset = end;
        }
        if offset > 0 {
            // Data-only: writes must not drain ACKs; inbound processing emits
            // immediate ACKs and the protocol deadline emits delayed ACKs.
            // Pass current to avoid a redundant current_ms() call.
            let current = kcp.current_ms() as u32;
            kcp.flush_data_only_with_current(current);
        }
        let ws = kcp.wait_send() as usize;
        self.wait_send.store(ws, Ordering::Relaxed);
        offset
    }
}

// ─── Background loops ─────────────────────────────────────────────────────────

pub(crate) fn spawn_input_loop(shared: Arc<SharedIoState>) -> knet::JoinHandle<()> {
    knet::spawn_task(async move {
        // Pre-allocate burst capacity to avoid dynamic growth spikes.
        // MAX_INPUT_BATCH slots × MAX_DATAGRAM bytes = ~96KB per connection,
        // allocated once at task start and recycled across all cycles.
        let mut burst: Vec<Vec<u8>> = Vec::with_capacity(MAX_INPUT_BATCH);
        burst.push(vec![0u8; MAX_DATAGRAM]);
        loop {
            if shared.is_closed() {
                break;
            }
            burst[0].resize(MAX_DATAGRAM, 0);
            // `close()` cancels the socket recv directly via the cancellation
            // token, so a closed-but-silent peer's task exits immediately
            // instead of waiting out a 100ms poll tick — no ~10 Hz timer churn
            // per idle connection. Active links complete `recv` normally.
            let n = match knet::race(
                Box::pin(shared.transport.recv_vec(&mut burst[0])),
                shared.cancel_token.cancelled(),
            )
            .await
            {
                knet::RaceOutcome::First(Ok(n)) if n > 0 => n,
                knet::RaceOutcome::First(Ok(_)) => continue,
                knet::RaceOutcome::First(Err(_)) if shared.is_closed() => break,
                knet::RaceOutcome::First(Err(e)) => {
                    *shared.last_error.lock() = Some(e);
                    knet::sleep_ms(10).await;
                    continue;
                }
                knet::RaceOutcome::Second(_) => break, // close() cancelled the recv
            };
            shared
                .last_activity_ms
                .store(knet::mono_ms(), Ordering::Relaxed);

            // Collect the full recv burst first, then process all datagrams
            // in one batch: FEC decode outside the KCP lock, one KCP lock for
            // input + deferred flush, and one reader wake. Leaving payload in
            // KCP preserves its receive-window backpressure; Reed-Solomon
            // decode remains outside the KCP state-machine lock.
            burst[0].truncate(n);
            let mut burst_len = 1;
            if shared.transport.supports_recv_batch() {
                // Batch drain: fill pre-sized slots via `try_recv_batch` (the
                // listener's `PeerTransport` pops the whole queue under one
                // lock; a direct-UDP client gets recvmmsg). Bounded by
                // MAX_INPUT_BATCH per cycle so a high-rate peer cannot starve
                // the worker (v3 §5.4). Slots are recycled across cycles.
                loop {
                    while burst.len() < burst_len + INPUT_BATCH_GROW {
                        burst.push(vec![0u8; MAX_DATAGRAM]);
                    }
                    let pool_end = (burst_len + INPUT_BATCH_GROW).min(MAX_INPUT_BATCH);
                    if pool_end <= burst_len {
                        break; // per-cycle budget reached
                    }
                    for s in &mut burst[burst_len..pool_end] {
                        s.resize(MAX_DATAGRAM, 0);
                    }
                    match shared
                        .transport
                        .try_recv_batch(&mut burst[burst_len..pool_end])
                    {
                        Ok(k) if k > 0 => burst_len += k,
                        _ => break, // WouldBlock / empty — peer drained
                    }
                    if burst_len >= MAX_INPUT_BATCH {
                        break; // defer the rest to the next cycle
                    }
                }
            } else {
                // Fallback: sequential single-packet drain.
                loop {
                    if burst_len == burst.len() {
                        burst.push(vec![0u8; MAX_DATAGRAM]);
                    } else {
                        burst[burst_len].resize(MAX_DATAGRAM, 0);
                    }
                    match shared.transport.try_recv_vec(&mut burst[burst_len]) {
                        Ok(m) if m > 0 => {
                            burst[burst_len].truncate(m);
                            burst_len += 1;
                        }
                        _ => break,
                    }
                }
            }
            // KCP input + the burst's single deferred flush share ONE mutex
            // acquisition (see `process_inbound_batch`). Produced packets stay
            // in `raw_packets`; the flush loop is the ONLY drainer + sender, so
            // wire order = flush order (single-owner — the old inline ACK send
            // here raced the flush loop and interleaved batches on the wire).
            let (data_ready, protocol_pending) =
                process_inbound_batch(&shared, &burst[..burst_len]);
            if data_ready {
                shared.wake_reader();
            }
            // Inline send: drain + send produced packets directly from the
            // input loop, bypassing the flush loop's notify→wake→drain→send
            // scheduling hop. The `is_sending` CAS ensures single-owner wire
            // order: if the flush loop (or a writer) is already sending, fall
            // back to notify_one() so the flush loop handles the packets.
            //
            // This eliminates the P999 tail latency caused by tokio timer-wheel
            // scheduling jitter on the notify→wake critical path (A/B verified:
            // notify P999 up to 32ms vs inline P999 ~1.3ms, 3 rounds). The
            // flush loop still owns retransmission / delayed-ACK / probe
            // deadlines; inline-send only handles the immediate burst.
            let sent_inline = shared.try_drain_and_send().await;
            if !sent_inline || protocol_pending {
                // If another sender owns the raw queue, or KCP still has a
                // delayed ACK/probe/retransmission deadline, arm the
                // maintenance loop. A completed inline ACK-only burst does
                // not need a redundant task hop.
                shared.flush_notify.notify_one();
            }
        }
    })
}

/// Feed a burst of inbound datagrams into KCP and run the burst's single
/// deferred flush.
///
/// Three-phase design (P0 #3, P1 #4, P1 #5):
///
/// 1. **FEC decode OUTSIDE the KCP lock** — Reed-Solomon matrix operations are
///    CPU-heavy and used to run while holding the KCP mutex, blocking writes,
///    flushes, and ACK processing. Now the FEC decoder lock is acquired and
///    released before the KCP lock is touched.
///
/// 2. **One KCP lock for the whole burst** — all `input_no_flush` calls and the
///    deferred ACK flush happen under a single mutex acquisition. At most a
///    small byte-bounded batch is prefetched for read/input pipelining; excess
///    data stays in KCP so receive-window backpressure remains bounded.
///
/// Produced wire packets (ACKs / data / probes) stay in `raw_packets`; the
/// caller chooses the send strategy (inline vs. flush-loop notify).
///
/// Returns `(data_ready, protocol_pending)` for reader and maintenance wakes.
pub(crate) fn process_inbound_batch(shared: &SharedIoState, datagrams: &[Vec<u8>]) -> (bool, bool) {
    // ── Phase 1: FEC decode all datagrams OUTSIDE the KCP lock ──
    // For non-FEC mode, datagrams are fed directly in Phase 2 (no clone).
    // For FEC mode, original data shards stay borrowed. Only reconstructed
    // shards need owned storage because the decoder's result is temporary.
    let has_fec = shared.fec_decoder.is_some();
    let mut kcp_slices: Vec<Cow<'_, [u8]>> = Vec::new();

    if has_fec {
        // Decode the complete burst while holding the decoder mutex once.
        // Reed-Solomon state is per-peer, and repeatedly locking it for every
        // datagram amplified contention under recvmmsg bursts.
        let dec = shared.fec_decoder.as_ref().unwrap();
        let mut decoder = dec.lock();
        for input in datagrams {
            crate::snmp::add(&crate::snmp::DEFAULT_SNMP.in_pkts, 1);
            if input.len() >= 6 {
                let fec_flag = u16::from_le_bytes([input[4], input[5]]);
                let recovered = decoder.decode(input);
                match fec_flag {
                    FEC_TYPE_DATA => {
                        if input.len() > FEC_HDR {
                            kcp_slices.push(Cow::Borrowed(&input[FEC_HDR..]));
                        }
                        for r in &recovered {
                            if let Some(kcp_slice) = fec_kcp_from_recovered(r) {
                                kcp_slices.push(Cow::Owned(kcp_slice.to_vec()));
                            }
                        }
                    }
                    FEC_TYPE_PARITY => {
                        for r in &recovered {
                            if let Some(kcp_slice) = fec_kcp_from_recovered(r) {
                                kcp_slices.push(Cow::Owned(kcp_slice.to_vec()));
                            }
                        }
                    }
                    _ => {
                        if input.len() >= 24 {
                            kcp_slices.push(Cow::Borrowed(input));
                        }
                    }
                }
            } else if input.len() >= 24 {
                kcp_slices.push(Cow::Borrowed(input));
            }
        }
    } else {
        // Non-FEC: count packets, feed directly in Phase 2 (no clone).
        for _ in datagrams {
            crate::snmp::add(&crate::snmp::DEFAULT_SNMP.in_pkts, 1);
        }
    }

    // ── Phase 2: KCP input + deferred ACK flush, one lock ──
    let mut had_input = false;
    let (ws, data_ready, protocol_pending) = {
        let mut kcp = shared.kcp.lock();
        if has_fec {
            for slice in &kcp_slices {
                if input_with_optional_conv(&mut kcp, shared, slice.as_ref()) {
                    had_input = true;
                }
            }
        } else {
            for input in datagrams {
                if input_with_optional_conv(&mut kcp, shared, input) {
                    had_input = true;
                }
            }
        }
        // `input_no_flush` records whether ACKs/data need a flush. This keeps
        // the default ack-no-delay=true behavior immediate while avoiding a
        // sticky unconditional flush for every inbound burst.
        let current = kcp.current_ms() as u32;
        kcp.flush_if_pending(current);

        // Prefetch directly into the read buffer while KCP is held: holding
        // the read-buf lock across the loop preserves FIFO against direct KCP
        // readers, avoids one mutex round-trip per small message, and needs
        // no intermediate Vec (zero allocation on the per-packet path).
        // Concurrent readers may only decrease the byte/message counts,
        // making the budget conservative but exact.
        let queued_ready = {
            let mut read_buf = shared.read_buf.lock();
            let mut prefetched_bytes = read_buf.bytes();
            let mut prefetched_messages = read_buf.len();
            while let Some(size) = kcp.peeksize() {
                let remaining = READ_PREFETCH_MAX_BYTES.saturating_sub(prefetched_bytes);
                if size == 0
                    || size > remaining
                    || prefetched_messages >= READ_PREFETCH_MAX_MESSAGES
                {
                    break;
                }
                let Ok(data) = kcp.recv_bytes() else {
                    break;
                };
                prefetched_bytes = prefetched_bytes.saturating_add(data.len());
                prefetched_messages += 1;
                read_buf.push_back(data);
            }
            !read_buf.is_empty()
        };
        (
            kcp.wait_send() as usize,
            queued_ready || kcp.peeksize().is_some(),
            kcp.needs_update(),
        )
    };

    // First conv-valid inbound from the peer (probe WINS / ACK / data) drives
    // the connect-timeout first-packet wait. Notify only on that one-shot
    // transition; data-ready bursts use the normal reader wake below.
    if had_input && !shared.first_inbound.swap(true, Ordering::AcqRel) {
        shared.read_notify.notify_one();
    }

    // Publish the post-flush send window: the deferred flush is what removes
    // ACKed segments from `snd_buf`, so `wait_send` is only accurate here.
    shared.wait_send.store(ws, Ordering::Relaxed);
    if ws < shared.snd_wnd.load(Ordering::Relaxed) {
        // Directly wake any blocked writer — eliminates the need for a
        // spawned backpressure task (see arm_backpressure_wake).
        shared.wake_writer();
    }

    (
        data_ready && !shared.read_closed.load(Ordering::Acquire),
        protocol_pending,
    )
}

/// Feed one decrypted KCP packet, committing server-side conv adoption only
/// after the packet passes KCP validation. Invalid traffic must not pin a
/// listener session to an attacker-controlled conversation ID.
pub(crate) fn input_with_optional_conv(
    kcp: &mut KCP,
    shared: &SharedIoState,
    input: &[u8],
) -> bool {
    if input.len() < 24 {
        return false;
    }
    if !shared.adopt_conv.load(Ordering::Acquire) {
        return kcp
            .input_no_flush(input, shared.acknodelay.load(Ordering::Acquire))
            .is_ok();
    }

    let configured = kcp.conv();
    let candidate = u32::from_le_bytes(input[..4].try_into().unwrap());
    kcp.set_conv(candidate);
    if kcp
        .input_no_flush(input, shared.acknodelay.load(Ordering::Acquire))
        .is_ok()
    {
        shared.adopt_conv.store(false, Ordering::Release);
        true
    } else {
        kcp.set_conv(configured);
        false
    }
}

pub(crate) fn spawn_flush_loop(shared: Arc<SharedIoState>) -> knet::JoinHandle<()> {
    knet::spawn_task(async move {
        // Absolute deadline-based scheduling (P0 #1+#2): replaces the old
        // fixed 2ms timer task + static `next_update` counter that never
        // decreased. The old design had two bugs:
        //
        // 1. `next_update` was a static "ms until next event" value that never
        //    decreased between KCP flushes — the loop's `if next_update > 1`
        //    guard caused it to skip the KCP state machine indefinitely until
        //    a write or ACK happened to wake it. On a silent link with
        //    un-ACKed data, RTO expiry was effectively starved.
        //
        // 2. The 2ms timer task fired every 2ms per connection, even when idle
        //    — 10K idle connections → ~5M wakeups/sec of pure timer-wheel churn.
        //
        // Now: sleep until the next KCP event deadline, woken early by Notify
        // when data/ACK/close arrives. Under load, the notify fires before the
        // timer; when entirely idle there is no timer at all.
        // An entirely idle connection has no retransmission, ACK, or probe
        // deadline to service. Park on Notify until the first activity instead
        // of creating one timer-wheel wake per KCP interval per connection.
        // Once activity arrives we retain the configured KCP interval before
        // the first maintenance flush, preserving delayed-ACK timing.
        // Run one initial protocol tick. Besides initializing KCP scheduling,
        // this guarantees a probe requested immediately after task spawn is
        // observed even on runtimes where a pre-listener notification is not
        // retained. The connection parks after that tick if it is still idle.
        let mut next_deadline: Option<Instant> = Some(Instant::now());
        let mut idle_candidate = false;
        loop {
            if shared.is_closed() {
                break;
            }

            // Wait for activity when idle, otherwise race activity against the
            // next retransmission / protocol-maintenance deadline.
            let was_notified = if let Some(deadline) = next_deadline {
                let remaining = deadline.saturating_duration_since(Instant::now());
                knet::timeout(remaining, shared.flush_notify.notified())
                    .await
                    .is_ok()
            } else {
                shared.flush_notify.notified().await;
                true
            };
            if was_notified && idle_candidate {
                // Activity arrived while the connection was only waiting out
                // the one-second idle grace. That grace deadline is not a KCP
                // retransmission deadline: discard it so a sparse write arms
                // maintenance from the current protocol interval instead of
                // inheriting up to ~1s of stale delay before its first RTO.
                next_deadline = None;
            }
            if was_notified {
                idle_candidate = false;
            }

            if shared.is_closed() {
                break;
            }

            // ── Fast send: drain + send write-path packets BEFORE touching the
            // KCP mutex ──
            //
            // Quick check: if raw_packets is empty, skip the CAS + drain +
            // recycle cycle entirely. Under high throughput, feed_batch's
            // inline drain_and_flush_tx already sent the immediate burst,
            // so raw_packets is usually empty when the flush loop wakes.
            if !shared.raw_packets.lock().is_empty() {
                let fast_acquired = shared
                    .is_sending
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok();
                if fast_acquired {
                    let fast_packets = shared.drain_raw_packets();
                    if !fast_packets.is_empty() {
                        // Try non-blocking sendmmsg first (sync fast path);
                        // falls back to async send_batch on WouldBlock.
                        if let Err(e) = shared.flush_tx_batch(&fast_packets).await {
                            *shared.last_error.lock() = Some(e);
                        }
                        crate::snmp::add(&crate::snmp::DEFAULT_SNMP.write_flush_sends, 1);
                    }
                    shared.recycle_raw_packets(fast_packets);
                    shared.finish_sending();
                }
            }

            // ── KCP state-machine phase ──
            // Writes flush data inline; this protocol-deadline path owns
            // maintenance and may emit delayed ACKs (ack-no-delay=false).
            let ws = {
                let now = Instant::now();
                match next_deadline {
                    Some(deadline) if now < deadline => continue,
                    None => {
                        // The write/input path already emitted any immediate
                        // data or ACK batch before notifying us. Arm the first
                        // maintenance deadline using the configured protocol
                        // interval; probes and delayed ACKs are serviced then.
                        let delay_ms = shared.kcp.lock().interval() as u64;
                        next_deadline = Some(
                            now + Duration::from_millis(delay_ms.clamp(1, MAX_IDLE_UPDATE_MS)),
                        );
                        continue;
                    }
                    Some(_) => {}
                }
                let mut kcp = shared.kcp.lock();
                let current = kcp.current_ms() as u32;
                let delay_ms = kcp.flush_with_current(current, true) as u64;
                let ws = kcp.wait_send() as usize;
                next_deadline = if ws > 0 {
                    idle_candidate = false;
                    Some(now + Duration::from_millis(delay_ms.clamp(1, ACTIVE_UPDATE_MAX_MS)))
                } else if idle_candidate {
                    idle_candidate = false;
                    None
                } else {
                    idle_candidate = true;
                    Some(now + Duration::from_millis(IDLE_PARK_GRACE_MS))
                };
                ws
            };

            shared.wait_send.store(ws, Ordering::Relaxed);
            if ws < shared.snd_wnd.load(Ordering::Relaxed) {
                shared.write_notify.notify_one();
            }

            // ── Second drain: send packets produced by protocol flush ──
            let second_acquired = shared
                .is_sending
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok();
            if second_acquired {
                let packets = shared.drain_raw_packets();
                if !packets.is_empty() {
                    // Try non-blocking sendmmsg first (sync fast path);
                    // falls back to async send_batch on WouldBlock.
                    if let Err(e) = shared.flush_tx_batch(&packets).await {
                        *shared.last_error.lock() = Some(e);
                    }
                    crate::snmp::add(&crate::snmp::DEFAULT_SNMP.write_flush_sends, 1);
                }
                shared.recycle_raw_packets(packets);
                shared.finish_sending();
            } else {
                // Writer is sending — ensure we wake to retry sending these
                // packets on the next iteration.
                shared.flush_notify.notify_one();
            }
        }
    })
}

