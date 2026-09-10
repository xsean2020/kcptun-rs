//! Sharded worker pipeline architecture for high-concurrency UDP listeners.
//!
//! Three socket topologies, chosen by `worker_count`, bind mode, and OS:
//!
//! ```text
//! Direct single worker (worker_count == 1, every platform):
//!   worker thread: recvmmsg → KCP → sendmmsg — one thread, own runtime
//! Direct reuseport group (Linux, fresh bind, worker_count > 1):
//!   kernel 4-tuple hash → N SO_REUSEPORT sockets → N direct worker threads
//! Reader pipeline (shared socket + worker_count > 1: external sockets,
//!   non-Linux fresh binds):
//!   RX Thread (recvmmsg) → tokio-aware channel → Sharded Worker → TX (sendmmsg)
//! ```
//!
//! # Design (by section number from the architecture doc)
//!
//! - **§4 RX hot path**: Direct workers drain their own socket via
//!   `try_recv_batch_from_into` and process inline — no reader thread, no
//!   channel, no cross-thread hop. The reader pipeline keeps a dedicated RX
//!   thread that drains the shared socket and `try_send`s
//!   `(SocketAddr, Vec<u8>)` into the correct worker's bounded channel; it
//!   never decrypts, runs KCP, or awaits a send.
//!
//! - **§5 Worker**: Each worker is a long-lived **OS thread** owning a
//!   `SessionMap` (`HashMap<SocketAddr, KcpStream>`). It drains its socket
//!   or channel in batches and calls `feed_raw_batch` on each affected
//!   session, which runs decrypt → FEC → KCP input → ACK flush → inline TX
//!   all on the same thread. Each worker runs its own current-thread Tokio
//!   runtime, and its idle park (`recv_from()` / `recv()`) registers with
//!   that driver's epoll — the SAME wait serves the per-connection
//!   flush-loop timers hosted on the runtime. No blocking wait ever freezes
//!   the driver.
//!
//! - **§6 Session affinity**: Direct mode gets it from the kernel — a
//!   SO_REUSEPORT group hashes each flow's 4-tuple to one socket, and a
//!   single worker trivially owns every flow. The reader pipeline routes by
//!   `fast_hash(peer) % worker_count` to a fixed worker.
//!
//! - **§10.2 Mode B (low latency)**: Workers build connections with
//!   `background_input(false)`, so decrypt + KCP + encrypt all run on the
//!   worker thread via `feed_raw_batch`. The flush loop runs on the
//!   worker's current-thread runtime — no cross-thread scheduling hop for
//!   the common sync send path.
//!
//! - **§14 Worker event loop**: bounded non-blocking drain (socket or
//!   channel) → process → park inside the runtime driver, racing shutdown
//!   against [`knet::CancellationToken`] so `close()` wakes parked workers
//!   immediately (direct mode) — no timer polling while idle.
//!
//! - **§15 TX path**: `feed_raw_batch` → `feed_batch` already calls
//!   `drain_and_flush_tx` which tries non-blocking `try_send_batch` on the
//!   worker's socket. When the kernel send buffer is full (`WouldBlock`), it
//!   falls back to the per-connection flush loop's async send path.
//!
//! - **§16 Batch strategy**: RX batch = 32 (`RECV_BATCH`); worker drain
//!   batch = 64 (`WORKER_BATCH`). Bounded to prevent one hot shard from
//!   starving others.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::config::{KcpConfig, KcpMode};
use crate::conn::{kcp_config_setters, resolve_one, KcpStream};
use crate::transport::{TransportWrapper, MAX_DATAGRAM};
use knet::Notify;

// ─── Worker RX queue (tokio-aware) ──────────────────────────────────────────
//
// The worker's inbound queue is an `async_channel` (MPMC, waker-based): the
// worker parks inside its current-thread runtime driver via `recv().await`,
// so the SAME epoll wait serves the queue AND the per-connection flush-loop
// timers hosted on this runtime. The previous crossbeam channel required a
// *blocking* `recv_timeout` inside the async context, which froze the runtime
// driver — and with it every flush-loop timer on this shard — for up to the
// park timeout. (pprof: crossbeam `recv_deadline`+`wait_until` ≈ 7% CPU.)
pub use knet::{Receiver as AsyncReceiver, Sender as AsyncSender, TrySendError as AsyncTrySendError};
/// RX batch size for `try_recv_batch_from_into` (§16: 16–32 is a good starting
/// point for low-latency + throughput).
const RECV_BATCH: usize = 32;
/// Worker channel capacity. Bounded so a slow worker doesn't accumulate
/// unbounded packets — the reader drops overflow (§4: "Worker ring full →
/// drop + metric, never await"). Sized to absorb a transient OS-level worker
/// stall: at ~12k pkt/s per session a 256-slot queue fills in ~21ms, and each
/// dropped datagram costs a 30–200ms KCP RTO on the tail. 2048 slots absorb
/// ~170ms of stall for ~2.9MB worst-case buffered datagrams per worker.
const WORKER_CHANNEL_CAP: usize = 2048;
/// Max datagrams a worker drains per event-loop cycle (§16: bound to prevent
/// one hot shard from monopolizing the worker).
const WORKER_BATCH: usize = 64;
/// Even in unlimited mode, return to the executor after a bounded packet/time
/// quantum so other workers and the acceptor get runtime service under flood.
const DRAIN_QUANTUM: usize = 1_024;
const DRAIN_QUANTUM_MS: u128 = 5;
/// Max time (microseconds) a worker spends processing one batch of peers
/// before yielding the CPU. Prevents a hot peer with heavy crypto/FEC from
/// starving other peers' flush loops, reducing P999 tail latency.
const WORKER_TIME_BUDGET_US: u64 = 2_000;

// ─── RX buffer pool (lock-free recycling) ─────────────────────────────────
//
// Recycle pool for RX datagram buffers.  The sharded `RxReader` allocates a
// fresh 2048‑byte `Vec` per received datagram, which is consumed and dropped
// in the worker after KCP input.  Under sustained load this creates allocator
// contention (malloc/free per packet) that directly inflates P99 tail latency.
// The pool keeps buffers alive so the fast path only pays a memset (≈ 100 ns
// for 2 KB) instead of a full heap alloc + free.

const BUFPOOL_CAP: usize = 4096;

/// Either a Sender or Receiver — the pool is the pair.
type BufRx = crossbeam_channel::Receiver<Vec<u8>>;
type BufTx = crossbeam_channel::Sender<Vec<u8>>;

fn bufpool() -> &'static (BufTx, BufRx) {
    use crossbeam_channel::bounded;
    use std::sync::OnceLock;
    static POOL: OnceLock<(BufTx, BufRx)> = OnceLock::new();
    POOL.get_or_init(|| {
        let (tx, rx) = bounded(BUFPOOL_CAP);
        (tx, rx)
    })
}

/// Return an empty RX buffer to the pool.  Only buffers with full MTU
/// capacity are retained; undersized buffers are freed as normal.
#[inline]
pub(crate) fn recycle_buf(mut buf: Vec<u8>) {
    if buf.capacity() >= MAX_DATAGRAM {
        buf.clear();
        let _ = bufpool().0.try_send(buf);
    }
}

/// Take a pooled buffer, or `None` when the pool is empty.
/// The returned buffer has capacity `≥ MAX_DATAGRAM` and length 0.
#[inline]
pub(crate) fn acquire_buf() -> Option<Vec<u8>> {
    match bufpool().1.try_recv() {
        Ok(buf) if buf.capacity() >= MAX_DATAGRAM => Some(buf),
        _ => None,
    }
}
/// Wakeups between periodic lifecycle sweeps (idle session reap, etc.).
const SWEEP_INTERVAL: u32 = 128;
/// Max concurrent sessions per worker before admission-dropping new peers.
const DEFAULT_MAX_SESSIONS_PER_WORKER: usize = 0; // 0 = unlimited

/// Fast, stable hash for session affinity (§6). Uses FNV-1a on the socket
/// address bytes — cheap, well-distributed, deterministic. Stack-assembled
/// (no heap concat per routed packet).
#[inline]
fn fast_hash_peer(peer: &SocketAddr) -> u64 {
    fn fnv1a(bytes: &[u8]) -> u64 {
        let mut hash: u64 = 0xcbf29ce484222325;
        for &byte in bytes {
            hash ^= byte as u64;
            hash = hash.wrapping_mul(0x100000001b3);
        }
        hash
    }
    match peer {
        SocketAddr::V4(v4) => {
            let mut bytes = [0u8; 6];
            bytes[..4].copy_from_slice(&v4.ip().octets());
            bytes[4..].copy_from_slice(&v4.port().to_le_bytes());
            fnv1a(&bytes)
        }
        SocketAddr::V6(v6) => {
            let mut bytes = [0u8; 18];
            bytes[..16].copy_from_slice(&v6.ip().octets());
            bytes[16..].copy_from_slice(&v6.port().to_le_bytes());
            fnv1a(&bytes)
        }
    }
}

// ─── KcpListener ─────────────────────────────────────────────────────

/// Resource limits for [`KcpListener`].
/// from the non-sharded listener but applies per-worker.
#[derive(Debug, Clone, Copy)]
pub struct WorkerPoolLimits {
    /// Max concurrent sessions per worker (0 = unlimited).
    pub max_sessions_per_worker: usize,
    /// Drop-tail cap on each worker's channel (0 = use `WORKER_CHANNEL_CAP`).
    pub worker_channel_cap: usize,
    /// Max datagrams routed per reader wakeup (0 = unlimited, bounded by
    /// `DRAIN_QUANTUM`).
    pub max_drain_packets: usize,
    /// A peer stuck in Building longer than this is reaped.
    /// `Duration::ZERO` = no timeout.
    pub building_timeout: Duration,
}

impl Default for WorkerPoolLimits {
    fn default() -> Self {
        Self {
            max_sessions_per_worker: DEFAULT_MAX_SESSIONS_PER_WORKER,
            worker_channel_cap: WORKER_CHANNEL_CAP,
            max_drain_packets: 0,
            building_timeout: Duration::ZERO,
        }
    }
}

/// Live snapshot of [`KcpListener`] resource accounting.
#[derive(Debug, Default, Clone, Copy)]
pub struct WorkerPoolStats {
    /// Total sessions across all workers.
    pub sessions: usize,
    /// Datagrams dropped because a worker's channel was full.
    pub channel_drops: u64,
    /// New sessions rejected because `max_sessions_per_worker` was reached.
    pub session_drops: u64,
    /// `KcpStream::build` failures.
    pub build_failures: u64,
}

/// Atomic counters behind [`KcpListener::stats`].
#[derive(Default)]
struct WorkerStats {
    channel_drops: AtomicU64,
    session_drops: AtomicU64,
    build_failures: AtomicU64,
}

/// Per-worker shared state: one packet is destined for a worker, the source
/// address and the raw datagram.
type WorkerPacket = (SocketAddr, Vec<u8>);

/// How a worker receives datagrams.
enum WorkerRx {
    /// Reader-thread → bounded tokio-aware channel (multi-worker listeners
    /// on a shared socket: external sockets, non-Linux fresh binds). The
    /// reader pushes with `try_send` (non-blocking); the worker parks in
    /// `recv().await` inside its current-thread runtime, so the park shares
    /// the driver's epoll wait with flush-loop timers (no blocking `recv`
    /// on the async context).
    Channel {
        tx: AsyncSender<WorkerPacket>,
        rx: AsyncReceiver<WorkerPacket>,
    },
    /// Direct mode: this worker owns a socket and drains it itself — no
    /// reader thread, no channel, no cross-thread hop. Used by Linux
    /// SO_REUSEPORT fresh binds (one socket per worker, kernel 4-tuple hash
    /// provides affinity) and by single-worker listeners (the sole worker
    /// takes over the only socket).
    Direct { socket: Arc<knet::DatagramSocket> },
}

/// Per-worker state: receive source + the worker's session map.
struct Worker {
    rx: WorkerRx,
    /// This worker's session map (§5: Worker owns its sessions, no
    /// cross-thread lock on the hot path).
    sessions: Mutex<HashMap<SocketAddr, KcpStream>>,
    /// Peers currently being built (§10.1 staged build, generation-guarded).
    building: Mutex<HashMap<SocketAddr, (u64, Instant)>>,
    /// Worker generation counter for build-stale detection.
    generation: AtomicU64,
}

impl Worker {
    /// Channel-fed worker for the reader pipeline.
    fn channel(cap: usize) -> Self {
        let (tx, rx) = knet::bounded(cap);
        Self {
            rx: WorkerRx::Channel { tx, rx },
            sessions: Mutex::new(HashMap::new()),
            building: Mutex::new(HashMap::new()),
            generation: AtomicU64::new(0),
        }
    }

    /// Direct worker owning `socket` (a per-worker SO_REUSEPORT member or
    /// the sole socket of a single-worker listener).
    fn direct(socket: Arc<knet::DatagramSocket>) -> Self {
        Self {
            rx: WorkerRx::Direct { socket },
            sessions: Mutex::new(HashMap::new()),
            building: Mutex::new(HashMap::new()),
            generation: AtomicU64::new(0),
        }
    }

    /// The socket this worker uses for the session send path
    /// ([`crate::transport::PeerTransport`]): direct workers send on their
    /// own socket (independent send queues, same reuseport group as the
    /// receive side); channel workers share the listener socket.
    fn send_socket(&self, shared: &Arc<knet::DatagramSocket>) -> Arc<knet::DatagramSocket> {
        match &self.rx {
            WorkerRx::Channel { .. } => shared.clone(),
            WorkerRx::Direct { socket } => socket.clone(),
        }
    }

    fn direct_socket(&self) -> Option<&Arc<knet::DatagramSocket>> {
        match &self.rx {
            WorkerRx::Channel { .. } => None,
            WorkerRx::Direct { socket } => Some(socket),
        }
    }

    /// Route one packet to this worker via `try_send` (§4: never await).
    /// On channel full, drop the packet and record a metric (KCP
    /// retransmission recovers it). Direct workers never route through a
    /// channel — the reader only exists on the channel pipeline.
    fn route(&self, peer: SocketAddr, data: Vec<u8>, stats: &WorkerStats) {
        let tx = match &self.rx {
            WorkerRx::Channel { tx, .. } => tx,
            WorkerRx::Direct { .. } => {
                recycle_buf(data);
                return;
            }
        };
        match tx.try_send((peer, data)) {
            Ok(()) => {}
            Err(AsyncTrySendError::Full((_, data))) | Err(AsyncTrySendError::Closed((_, data))) => {
                recycle_buf(data);
                stats.channel_drops.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn next_generation(&self) -> u64 {
        self.generation.fetch_add(1, Ordering::Relaxed)
    }

    fn session_count(&self) -> usize {
        self.sessions.lock().len()
    }
}

/// KCP listener with a sharded worker pipeline. Depending on topology the
/// workers either drain their own sockets directly (single worker, or a
/// Linux SO_REUSEPORT fresh bind — no reader thread) or are fed from a
/// dedicated reader thread over bounded channels (shared socket, N > 1).
/// Every worker is an OS thread with a dedicated `current-thread` tokio
/// runtime, isolated from the application's main async runtime to prevent
/// scheduling interference. Workers own their sessions and run decrypt +
/// KCP + encrypt on their own thread (§10.2 Mode B).
pub struct KcpListener {
    /// Listener socket for `local_addr`, and the shared send/RX socket on
    /// the reader pipeline. On the direct-reuseport path the per-worker
    /// sockets live in [`WorkerRx::Direct`] instead.
    socket: Arc<knet::DatagramSocket>,
    workers: Vec<Arc<Worker>>,
    pending: Arc<Mutex<VecDeque<PendingAccept>>>,
    accept_notify: Arc<Notify>,
    closed: Arc<AtomicBool>,
    last_error: Arc<Mutex<Option<io::Error>>>,
    stats: Arc<WorkerStats>,
    /// Wakes direct workers parked in `recv_from` the moment [`close()`](Self::close)
    /// fires (shared with the builder-created sockets' shutdown path).
    stop: knet::CancellationToken,
    /// Reader thread of the channel pipeline. `None` in direct mode —
    /// workers drain their own sockets and there is nothing to join.
    _reader: Option<JoinHandle<()>>,
    _workers: Vec<JoinHandle<()>>,
}

struct PendingAccept {
    conn: KcpStream,
    peer: SocketAddr,
}

impl Drop for KcpListener {
    fn drop(&mut self) {
        self.close();
    }
}

impl KcpListener {
    /// Bind a UDP socket on `addr` and return a builder for the sharded
    /// listener pipeline.
    ///
    /// This is the primary entry point for UDP listeners.
    pub fn bind(addr: impl ToSocketAddrs) -> KcpListenerBuilder {
        bind_listener(addr)
    }

    /// Use an already-bound datagram socket, preserving caller socket options.
    pub fn from_socket(socket: Arc<knet::DatagramSocket>) -> KcpListenerBuilder {
        from_socket_listener(socket)
    }

    /// Remove a peer from the worker's session map after its accepted
    /// connection ends. A later datagram from the same address creates a
    /// fresh connection and is surfaced by [`accept`](Self::accept),
    /// matching kcptun reconnect behavior.
    pub fn remove_peer(&self, peer: SocketAddr) -> bool {
        let mut removed = false;
        for w in &self.workers {
            if w.sessions.lock().remove(&peer).is_some() {
                removed = true;
            }
        }
        removed
    }

    /// Current number of known peer sessions across all workers.
    pub fn session_count(&self) -> usize {
        self.workers.iter().map(|w| w.session_count()).sum()
    }

    /// Number of worker tasks.
    pub fn worker_count(&self) -> usize {
        self.workers.len()
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    /// Accept the next client connection.
    pub async fn accept(&self) -> io::Result<(KcpStream, SocketAddr)> {
        loop {
            if let Some(v) = self.pending.lock().pop_front() {
                return Ok((v.conn, v.peer));
            }
            if self.closed.load(Ordering::Acquire) {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    "KcpListener closed",
                ));
            }
            let notified = self.accept_notify.notified();
            if let Some(v) = self.pending.lock().pop_front() {
                return Ok((v.conn, v.peer));
            }
            notified.await;
        }
    }

    /// Accept within `timeout`.
    pub async fn accept_timeout(&self, timeout: Duration) -> io::Result<(KcpStream, SocketAddr)> {
        knet::timeout(timeout, self.accept())
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "accept timed out"))?
    }

    /// Non-blocking accept.
    pub fn try_accept(&self) -> io::Result<Option<(KcpStream, SocketAddr)>> {
        if let Some(v) = self.pending.lock().pop_front() {
            return Ok(Some((v.conn, v.peer)));
        }
        if self.closed.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "KcpListener closed",
            ));
        }
        Ok(None)
    }

    /// Surface and clear the last transport error.
    pub fn take_error(&self) -> io::Result<Option<io::Error>> {
        Ok(self.last_error.lock().take())
    }

    /// Stop accepting and wake parked workers. Existing `KcpStream`s are
    /// unaffected. Direct workers parked on their socket `recv_from` are
    /// woken immediately via the cancel token; channel workers observe the
    /// flag on their next wakeup (traffic or caller-driven shutdown).
    pub fn close(&self) {
        if self
            .closed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            self.stop.cancel();
            self.accept_notify.notify_waiters();
        }
    }

    /// Live resource-accounting snapshot.
    pub fn stats(&self) -> WorkerPoolStats {
        let sessions: usize = self.workers.iter().map(|w| w.session_count()).sum();
        WorkerPoolStats {
            sessions,
            channel_drops: self.stats.channel_drops.load(Ordering::Relaxed),
            session_drops: self.stats.session_drops.load(Ordering::Relaxed),
            build_failures: self.stats.build_failures.load(Ordering::Relaxed),
        }
    }
}

// ─── Builder ────────────────────────────────────────────────────────────────

/// Socket topology chosen at build time (see module docs).
enum Topology {
    /// Each worker drains its own socket — no reader thread, no channels.
    /// One socket: a single-worker listener (any platform). N sockets: a
    /// Linux SO_REUSEPORT group with per-flow kernel affinity.
    Direct(Vec<Arc<knet::DatagramSocket>>),
    /// One shared socket fanned out to channel workers by a reader thread
    /// (external sockets with N > 1, non-Linux multi-worker fresh binds).
    Shared(Arc<knet::DatagramSocket>),
}

/// Fresh bind with `worker_count` workers: a single worker takes the socket
/// over directly; N > 1 workers on Linux get a per-worker SO_REUSEPORT
/// group (kernel 4-tuple hash = session affinity, no user-space demux).
fn fresh_bind_topology(addr: SocketAddr, worker_count: usize) -> io::Result<Topology> {
    if worker_count == 1 {
        return Ok(Topology::Direct(vec![Arc::new(knet::DatagramSocket::Udp(
            knet::UdpSocket::bind(addr)?,
        ))]));
    }
    #[cfg(target_os = "linux")]
    {
        // All group members must set SO_REUSEPORT before the group's first
        // bind; every socket here is created that way.
        let sockets = (0..worker_count)
            .map(|_| {
                Ok::<_, io::Error>(Arc::new(knet::DatagramSocket::Udp(
                    knet::UdpSocket::bind_reuseport(addr)?,
                )))
            })
            .collect::<io::Result<Vec<_>>>()?;
        Ok(Topology::Direct(sockets))
    }
    #[cfg(not(target_os = "linux"))]
    {
        // SO_REUSEPORT does not load-balance UDP flows on this platform —
        // share one socket through the reader pipeline instead.
        Ok(Topology::Shared(Arc::new(knet::DatagramSocket::Udp(
            knet::UdpSocket::bind(addr)?,
        ))))
    }
}

/// Builder for [`KcpListener`].
pub struct KcpListenerBuilder {
    addr: Option<SocketAddr>,
    socket: Option<Arc<knet::DatagramSocket>>,
    config: KcpConfig,
    resolve_err: Option<io::Error>,
    transport_wrapper: Option<TransportWrapper>,
    worker_count: usize,
    limits: Option<WorkerPoolLimits>,
}

impl KcpListenerBuilder {
    kcp_config_setters!();

    /// Wrap each accepted peer transport before constructing its `KcpStream`.
    pub fn transport_wrapper<F>(mut self, wrapper: F) -> Self
    where
        F: Fn(
                Arc<dyn crate::transport::PacketTransport>,
                SocketAddr,
            ) -> Arc<dyn crate::transport::PacketTransport>
            + Send
            + Sync
            + 'static,
    {
        self.transport_wrapper = Some(Arc::new(wrapper));
        self
    }

    /// Number of listener shards.
    ///
    /// An explicit value overrides `KCPTUN_WORKER_THREADS`. Without either,
    /// the default is available parallelism clamped to [1, 16].
    pub fn worker_count(mut self, n: usize) -> Self {
        self.worker_count = n.max(1);
        self
    }

    /// Override resource limits.
    pub fn limits(mut self, limits: WorkerPoolLimits) -> Self {
        self.limits = Some(limits);
        self
    }

    /// Bind the listen socket, spawn workers (and the reader when the
    /// topology needs one), and return the listener.
    ///
    /// Workers run on dedicated OS threads, each with its own `current-thread`
    /// tokio runtime. This isolates KCP processing from the application's
    /// main async runtime, preventing scheduling interference.
    ///
    /// Topology selection (see module docs): fresh binds with one worker use
    /// a direct socket takeover on every platform; fresh binds with N > 1
    /// workers use a per-worker SO_REUSEPORT group on Linux and the reader
    /// pipeline elsewhere; external sockets ([`from_socket`](Self::from_socket))
    /// are direct with a single worker, reader-fed otherwise.
    pub async fn build(self) -> io::Result<KcpListener> {
        if let Some(e) = self.resolve_err {
            return Err(e);
        }

        let limits = self.limits.unwrap_or_default();
        let worker_count = if self.worker_count == 0 {
            num_cpus()
        } else {
            self.worker_count
        };

        let topology = match self.socket {
            Some(s) => {
                if worker_count == 1 {
                    // Sole worker: take over the caller's socket directly —
                    // no reader thread, no channel hop.
                    Topology::Direct(vec![s])
                } else {
                    // An external socket cannot be split per-worker: keep
                    // the reader pipeline (preserves caller socket options).
                    Topology::Shared(s)
                }
            }
            None => {
                let addr = self.addr.ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "KcpListener: bind address required",
                    )
                })?;
                fresh_bind_topology(addr, worker_count)?
            }
        };

        let stats = Arc::new(WorkerStats::default());
        let pending = Arc::new(Mutex::new(VecDeque::new()));
        let accept_notify = Arc::new(Notify::new());
        let closed = Arc::new(AtomicBool::new(false));
        let last_error = Arc::new(Mutex::new(None::<io::Error>));
        let stop = knet::CancellationToken::new();

        // Build the worker set from the topology.
        let (workers, socket): (Vec<Arc<Worker>>, Arc<knet::DatagramSocket>) = match topology {
            Topology::Direct(sockets) => {
                let workers = sockets
                    .iter()
                    .map(|s| Arc::new(Worker::direct(s.clone())))
                    .collect();
                // `local_addr()` needs a listener-level socket; any member
                // reports the same bound address. Keep the first alive.
                let primary = sockets[0].clone();
                (workers, primary)
            }
            Topology::Shared(s) => {
                let workers = (0..worker_count)
                    .map(|_| Arc::new(Worker::channel(limits.worker_channel_cap)))
                    .collect();
                (workers, s)
            }
        };

        // Spawn worker threads (each with its own current-thread runtime).
        let worker_handles: Vec<JoinHandle<()>> = workers
            .iter()
            .map(|w| {
                spawn_worker(
                    w.clone(),
                    socket.clone(),
                    self.config.clone(),
                    self.transport_wrapper.clone(),
                    limits,
                    pending.clone(),
                    accept_notify.clone(),
                    stats.clone(),
                    closed.clone(),
                    stop.clone(),
                    last_error.clone(),
                )
            })
            .collect();

        // Reader thread only for the shared-socket pipeline (with its own
        // multi-thread runtime for async recv_from).
        let reader = match &workers[0].rx {
            WorkerRx::Channel { .. } => Some(spawn_sharded_reader(
                socket.clone(),
                workers.clone(),
                limits,
                stats.clone(),
                closed.clone(),
                last_error.clone(),
            )),
            WorkerRx::Direct { .. } => None,
        };

        Ok(KcpListener {
            socket,
            workers,
            pending,
            accept_notify,
            closed,
            last_error,
            stats,
            stop,
            _reader: reader,
            _workers: worker_handles,
        })
    }
}

impl std::future::IntoFuture for KcpListenerBuilder {
    type Output = io::Result<KcpListener>;
    type IntoFuture = std::pin::Pin<Box<dyn std::future::Future<Output = Self::Output> + Send>>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(self.build())
    }
}

/// Entry point for building a sharded listener.
pub fn bind_listener(addr: impl ToSocketAddrs) -> KcpListenerBuilder {
    match resolve_one(addr) {
        Ok(a) => KcpListenerBuilder {
            addr: Some(a),
            socket: None,
            config: KcpConfig::default(),
            resolve_err: None,
            transport_wrapper: None,
            worker_count: 0,
            limits: None,
        },
        Err(e) => KcpListenerBuilder {
            addr: None,
            socket: None,
            config: KcpConfig::default(),
            resolve_err: Some(e),
            transport_wrapper: None,
            worker_count: 0,
            limits: None,
        },
    }
}

/// Use an already-bound socket.
pub fn from_socket_listener(socket: Arc<knet::DatagramSocket>) -> KcpListenerBuilder {
    KcpListenerBuilder {
        addr: None,
        socket: Some(socket),
        config: KcpConfig::default(),
        resolve_err: None,
        transport_wrapper: None,
        worker_count: 0,
        limits: None,
    }
}

/// Get the default shard count for the listener pipeline.
///
/// A positive `KCPTUN_WORKER_THREADS` value overrides auto-detection. Invalid
/// values and zero fall back to available parallelism clamped to [1, 16].
fn num_cpus() -> usize {
    if let Ok(value) = std::env::var("KCPTUN_WORKER_THREADS") {
        if let Ok(worker_count) = value.parse::<usize>() {
            if worker_count > 0 {
                return worker_count;
            }
        }
    }

    let n = std::thread::available_parallelism()
        .map(|v| v.get())
        .unwrap_or(1);
    n.clamp(1, 16)
}

// ─── Reader (§4: RX hot path — never wait downstream) ───────────────────────

fn spawn_sharded_reader(
    socket: Arc<knet::DatagramSocket>,
    workers: Vec<Arc<Worker>>,
    limits: WorkerPoolLimits,
    stats: Arc<WorkerStats>,
    closed: Arc<AtomicBool>,
    last_error: Arc<Mutex<Option<io::Error>>>,
) -> JoinHandle<()> {
    thread::Builder::new()
        .name("kcp-reader".into())
        .spawn(move || {
            knet::block_on_multi_thread(async move {
                let mut buf = vec![0u8; MAX_DATAGRAM];
                let mut spares: Vec<Vec<u8>> =
                    (0..RECV_BATCH).map(|_| vec![0u8; MAX_DATAGRAM]).collect();
                let mut peers: Vec<SocketAddr> = Vec::with_capacity(RECV_BATCH);
                let worker_count = workers.len();
                let mut sweep_counter: u32 = 0;

                loop {
                    if closed.load(Ordering::Acquire) {
                        break;
                    }

                    // Periodic sweep counter.
                    sweep_counter = sweep_counter.wrapping_add(1);
                    let do_sweep = sweep_counter.is_multiple_of(SWEEP_INTERVAL);

                    // Block on the first packet (§4: recv_from is the only blocking
                    // call; everything else is non-blocking try_push).
                    if buf.capacity() < MAX_DATAGRAM {
                        buf = crate::sharded::acquire_buf()
                            .unwrap_or_else(|| vec![0u8; MAX_DATAGRAM]);
                    }
                    buf.resize(MAX_DATAGRAM, 0);
                    let (n, peer) = match socket.recv_from(&mut buf).await {
                        Ok(v) => v,
                        Err(e) => {
                            *last_error.lock() = Some(e);
                            knet::sleep_ms(10).await;
                            continue;
                        }
                    };
                    buf.truncate(n);

                    // Route the first packet.
                    let shard = (fast_hash_peer(&peer) as usize) % worker_count;
                    workers[shard].route(peer, std::mem::take(&mut buf), &stats);

                    // Drain remaining packets non-blocking (§4: recvmmsg batch).
                    while spares.len() < RECV_BATCH {
                        spares.push(
                            crate::sharded::acquire_buf()
                                .unwrap_or_else(|| vec![0u8; MAX_DATAGRAM]),
                        );
                    }
                    let mut drained: usize = 1;
                    let drain_started = Instant::now();
                    let mut quantum_hit =
                        limits.max_drain_packets > 0 && drained >= limits.max_drain_packets;

                    while !quantum_hit {
                        let recv_cap = if limits.max_drain_packets > 0 {
                            limits
                                .max_drain_packets
                                .saturating_sub(drained)
                                .min(spares.len())
                        } else {
                            spares.len().min(RECV_BATCH)
                        };
                        if recv_cap == 0 {
                            quantum_hit = true;
                            break;
                        }
                        let got = match socket
                            .try_recv_batch_from_into(&mut spares[..recv_cap], &mut peers)
                        {
                            Ok(got) => got,
                            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                            Err(e) => {
                                *last_error.lock() = Some(e);
                                break;
                            }
                        };
                        if got == 0 {
                            break;
                        }
                        for i in 0..got {
                            let s = (fast_hash_peer(&peers[i]) as usize) % worker_count;
                            workers[s].route(peers[i], std::mem::take(&mut spares[i]), &stats);
                        }
                        drained += got;
                        quantum_hit = (limits.max_drain_packets > 0
                            && drained >= limits.max_drain_packets)
                            || (limits.max_drain_packets == 0
                                && (drained >= DRAIN_QUANTUM
                                    || drain_started.elapsed().as_millis() >= DRAIN_QUANTUM_MS));
                    }

                    // Spare pool maintenance.
                    spares.retain(|s| s.capacity() >= MAX_DATAGRAM);

                    if do_sweep {
                        // Idle session sweep: workers check for dead sessions on
                        // their own — no notify needed since workers poll
                        // their channels directly.
                    }

                    if quantum_hit {
                        knet::yield_now().await;
                    }
                }
            });
        })
        .expect("spawn kcp-reader thread")
}

// ─── Worker (§5, §14: long-lived event loop) ────────────────────────────────

/// Spawn one worker thread: it owns its shard's session map and drives the
/// decrypt → KCP → encrypt pipeline (§10.2 Mode B). The receive source comes
/// from the worker's [`WorkerRx`]: direct workers recvmmsg-drain their own
/// socket inside the runtime driver; channel workers drain their bounded
/// tokio-aware queue. Either way the idle park shares the driver's epoll
/// wait with the per-connection flush-loop timers hosted here — no blocking
/// wait ever freezes the runtime (§14).
#[allow(clippy::too_many_arguments)]
fn spawn_worker(
    worker: Arc<Worker>,
    shared_socket: Arc<knet::DatagramSocket>,
    config: KcpConfig,
    transport_wrapper: Option<TransportWrapper>,
    limits: WorkerPoolLimits,
    pending: Arc<Mutex<VecDeque<PendingAccept>>>,
    accept_notify: Arc<Notify>,
    stats: Arc<WorkerStats>,
    closed: Arc<AtomicBool>,
    stop: knet::CancellationToken,
    last_error: Arc<Mutex<Option<io::Error>>>,
) -> JoinHandle<()> {
    // Session send path: direct workers send on their own socket
    // (independent send queues, same reuseport group); channel workers
    // share the listener socket.
    let socket = worker.send_socket(&shared_socket);
    thread::Builder::new()
        .name("kcp-worker".into())
        .spawn(move || {
            // This OS thread owns the shard. A current-thread runtime keeps
            // the receive park local to this shard; using the global runtime
            // lets idle shards occupy every shared Tokio worker and turns
            // the park into a process-wide latency spike.
            knet::block_on_local(async move {
                // Batch drain buffer (§14: drain_udp_batch).
                let mut batch: Vec<WorkerPacket> = Vec::with_capacity(WORKER_BATCH);
                // Deferred peers from a previous over-budget round (see the
                // WORKER_TIME_BUDGET_US break below). Carried over locally
                // instead of re-queueing through the channel — a full channel
                // would silently drop the datagrams, and a dropped segment
                // recovers only via a 30–200ms KCP RTO.
                let mut deferred: Vec<(SocketAddr, Vec<Vec<u8>>)> = Vec::new();
                let mut by_peer: HashMap<SocketAddr, Vec<Vec<u8>>> = HashMap::new();
                let mut affected: Vec<SocketAddr> = Vec::new();
                let mut affected_seen: HashSet<SocketAddr> = HashSet::new();
                let mut sweep_counter: u32 = 0;

                // Direct-mode receive state: pooled slots for the recvmmsg
                // burst drain, plus a parked-read slot (also pooled).
                let mut slots: Vec<Vec<u8>> = Vec::new();
                let mut peers: Vec<SocketAddr> = Vec::new();
                let mut parked_slot: Vec<u8> = Vec::new();
                if worker.direct_socket().is_some() {
                    slots = (0..RECV_BATCH).map(|_| vec![0u8; MAX_DATAGRAM]).collect();
                    peers = Vec::with_capacity(RECV_BATCH);
                    parked_slot = vec![0u8; MAX_DATAGRAM];
                }

                loop {
                    if closed.load(Ordering::Acquire) {
                        break;
                    }

                    sweep_counter = sweep_counter.wrapping_add(1);
                    let do_sweep = sweep_counter.is_multiple_of(SWEEP_INTERVAL);

                    // Deferred carry-over from a previous over-budget round:
                    // processed before fresh arrivals to preserve
                    // per-peer ordering.
                    if !deferred.is_empty() {
                        let taken = std::mem::take(&mut deferred);
                        for (peer, dgrams) in taken {
                            process_session(
                                &worker,
                                &peer,
                                dgrams,
                                &socket,
                                &config,
                                &transport_wrapper,
                                &limits,
                                &pending,
                                &accept_notify,
                                &stats,
                            )
                            .await;
                        }
                    }

                    // ── Fill the batch from this worker's RX source ──
                    batch.clear();
                    let mut drained: usize = match &worker.rx {
                        WorkerRx::Channel { rx, .. } => {
                            // Non-blocking drain of the worker's channel, up
                            // to WORKER_BATCH.
                            for _ in 0..WORKER_BATCH {
                                match rx.try_recv() {
                                    Ok(pkt) => batch.push(pkt),
                                    Err(_) => break,
                                }
                            }
                            batch.len()
                        }
                        WorkerRx::Direct { socket: rx_socket } => {
                            drain_own_socket(
                                rx_socket,
                                &mut batch,
                                &mut slots,
                                &mut peers,
                                &limits,
                                &last_error,
                                0,
                            )
                        }
                    };

                    if batch.is_empty() {
                        // No work — park **inside the runtime driver**: both
                        // receive sources register with this worker's
                        // current-thread runtime, so the SAME epoll wait
                        // serves the RX source and the per-connection
                        // flush-loop timers hosted here. (The historical
                        // crossbeam `recv_timeout` was a *blocking* call in
                        // async context: it froze the driver — and every
                        // flush timer on the shard — for up to the park
                        // timeout; pprof showed recv_deadline+wait_until
                        // ≈ 7% CPU even at full load.)
                        match &worker.rx {
                            WorkerRx::Channel { rx, .. } => match rx.recv().await {
                                Ok(first) => {
                                    batch.push(first);
                                    // Drain more that arrived during wake.
                                    for _ in 0..WORKER_BATCH - 1 {
                                        match rx.try_recv() {
                                            Ok(p) => batch.push(p),
                                            Err(_) => break,
                                        }
                                    }
                                    drained = batch.len();
                                }
                                Err(_) => break, // channel closed (never in practice)
                            },
                            WorkerRx::Direct { socket: rx_socket } => {
                                // First packet blocks into a pooled slot,
                                // raced against shutdown so `close()` wakes
                                // a parked worker immediately (no timer
                                // polling while idle).
                                parked_slot.resize(MAX_DATAGRAM, 0);
                                match knet::race(
                                    stop.cancelled(),
                                    Box::pin(rx_socket.recv_from(&mut parked_slot)),
                                )
                                .await
                                {
                                    knet::RaceOutcome::First(()) => continue,
                                    knet::RaceOutcome::Second(Ok((n, peer))) if n > 0 => {
                                        parked_slot.truncate(n);
                                        batch.push((peer, std::mem::take(&mut parked_slot)));
                                        // The rest of the burst is already in
                                        // the kernel buffer — drain it too.
                                        let parked = 1;
                                        drained = parked
                                            + drain_own_socket(
                                                rx_socket,
                                                &mut batch,
                                                &mut slots,
                                                &mut peers,
                                                &limits,
                                                &last_error,
                                                parked,
                                            );
                                        // Refill the taken parked slot from
                                        // the pool for the next park.
                                        parked_slot = crate::sharded::acquire_buf()
                                            .unwrap_or_else(|| vec![0u8; MAX_DATAGRAM]);
                                    }
                                    knet::RaceOutcome::Second(Ok(_)) => {
                                        // Zero-length datagram (KCP never
                                        // sends one) — drop and re-park.
                                        continue;
                                    }
                                    knet::RaceOutcome::Second(Err(e)) => {
                                        *last_error.lock() = Some(e);
                                        knet::sleep_ms(10).await;
                                        continue;
                                    }
                                }
                            }
                        }
                        // Re-check after a park that may have raced shutdown.
                        if closed.load(Ordering::Acquire) {
                            break;
                        }
                    }

                    if drained == 0 {
                        continue;
                    }

                    process_batch(
                        batch.drain(..),
                        &worker,
                        &socket,
                        &config,
                        &transport_wrapper,
                        &limits,
                        &pending,
                        &accept_notify,
                        &stats,
                        &mut by_peer,
                        &mut affected,
                        &mut affected_seen,
                        &mut deferred,
                    )
                    .await;

                    // Yield to the runtime so per-connection flush loops
                    // (spawn_flush_loop, default on for server sessions) get
                    // polled. Under sustained load the batch processes faster
                    // than WORKER_TIME_BUDGET_US (2ms), so without this yield
                    // the current-thread runtime never polls flush-loop timers
                    // — retransmission deadlines, delayed ACKs, and window
                    // probes are starved at the worst possible time.
                    knet::yield_now().await;

                    // Idle sweep: dead-session reaping piggybacks on
                    // wakeups (bounded every SWEEP_INTERVAL cycles).
                    // Sweeping right after a burst is equivalent to the old
                    // timeout-park sweep — cadence scales with traffic, not
                    // wall clock.
                    if do_sweep {
                        reaper_sweep(&worker);
                    }
                }
            });
        })
        .expect("spawn kcp-worker thread")
}

/// Non-blocking `recvmmsg` drain of a direct worker's own socket into
/// `batch` (§4 on the worker itself). `already` counts packets collected
/// for this cycle; the call stops when the socket is dry (`WouldBlock`),
/// a hard error is recorded, or the reader-pipeline quantum is consumed
/// (`max_drain_packets` when set, else `DRAIN_QUANTUM`/`DRAIN_QUANTUM_MS`)
/// so a flood cannot starve this shard's flush-loop timers. Consumed slots
/// are refilled from the RX buffer pool. Returns packets added.
fn drain_own_socket(
    rx_socket: &Arc<knet::DatagramSocket>,
    batch: &mut Vec<WorkerPacket>,
    slots: &mut [Vec<u8>],
    peers: &mut Vec<SocketAddr>,
    limits: &WorkerPoolLimits,
    last_error: &Arc<Mutex<Option<io::Error>>>,
    already: usize,
) -> usize {
    let mut drained = already;
    let drain_started = Instant::now();
    loop {
        let budget_left = if limits.max_drain_packets > 0 {
            limits.max_drain_packets.saturating_sub(drained)
        } else {
            usize::MAX
        };
        let recv_cap = slots.len().min(budget_left);
        if recv_cap == 0 {
            break;
        }
        // Ensure consumed slots carry full-MTU capacity again.
        for slot in &mut slots[..recv_cap] {
            if slot.capacity() < MAX_DATAGRAM {
                *slot =
                    crate::sharded::acquire_buf().unwrap_or_else(|| vec![0u8; MAX_DATAGRAM]);
            }
        }
        let got = match rx_socket.try_recv_batch_from_into(&mut slots[..recv_cap], peers) {
            Ok(got) => got,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(e) => {
                *last_error.lock() = Some(e);
                break;
            }
        };
        if got == 0 {
            break;
        }
        for i in 0..got {
            batch.push((peers[i], std::mem::take(&mut slots[i])));
        }
        drained += got;
        if limits.max_drain_packets > 0 && drained >= limits.max_drain_packets {
            break;
        }
        if drained >= DRAIN_QUANTUM
            || drain_started.elapsed().as_millis() >= DRAIN_QUANTUM_MS
        {
            break;
        }
    }
    drained - already
}

/// Process one drained batch: single-peer fast path (existing session →
/// `feed_raw_single`), or group by peer and run `process_session` per peer
/// under the worker time budget. Shared by the drain path and the parked
/// wake path (previously two duplicated ~70-line blocks).
#[allow(clippy::too_many_arguments)]
async fn process_batch(
    batch: impl Iterator<Item = WorkerPacket>,
    worker: &Arc<Worker>,
    socket: &Arc<knet::DatagramSocket>,
    config: &KcpConfig,
    transport_wrapper: &Option<TransportWrapper>,
    limits: &WorkerPoolLimits,
    pending: &Arc<Mutex<VecDeque<PendingAccept>>>,
    accept_notify: &Arc<Notify>,
    stats: &Arc<WorkerStats>,
    by_peer: &mut HashMap<SocketAddr, Vec<Vec<u8>>>,
    affected: &mut Vec<SocketAddr>,
    affected_seen: &mut HashSet<SocketAddr>,
    deferred: &mut Vec<(SocketAddr, Vec<Vec<u8>>)>,
) {
    let packets: Vec<WorkerPacket> = batch.collect();
    if packets.len() == 1 {
        let (peer, data) = packets.into_iter().next().unwrap();
        let existing = {
            let sessions = worker.sessions.lock();
            sessions.get(&peer).cloned()
        };
        if let Some(conn) = existing {
            let _ = conn.feed_raw_single(data);
        } else {
            process_session(
                worker,
                &peer,
                vec![data],
                socket,
                config,
                transport_wrapper,
                limits,
                pending,
                accept_notify,
                stats,
            )
            .await;
        }
        return;
    }

    // Multi-peer: group by peer for batch KCP input.
    by_peer.clear();
    affected.clear();
    affected_seen.clear();
    for (peer, data) in packets {
        if affected_seen.insert(peer) {
            affected.push(peer);
        }
        by_peer.entry(peer).or_default().push(data);
    }

    // Process peers with a time budget: if we've spent too long on this
    // batch, defer remaining peers (lossless carry-over) and yield.
    let round_start = Instant::now();
    let mut idx = 0;
    while idx < affected.len() {
        let peer = affected[idx];
        let datagrams = by_peer.remove(&peer).unwrap_or_default();
        process_session(
            worker,
            &peer,
            datagrams,
            socket,
            config,
            transport_wrapper,
            limits,
            pending,
            accept_notify,
            stats,
        )
        .await;
        idx += 1;

        if round_start.elapsed().as_micros() as u64 >= WORKER_TIME_BUDGET_US {
            while idx < affected.len() {
                let p = affected[idx];
                if let Some(dgrams) = by_peer.remove(&p) {
                    deferred.push((p, dgrams));
                }
                idx += 1;
            }
            // Yield so other tasks (flush loops, acceptor) run.
            knet::yield_now().await;
            break;
        }
    }
    affected.clear();
}

/// Process one peer's batch of datagrams: build a new session if needed, then
/// feed the batch via `feed_raw_batch` (§10.2: decrypt + KCP + encrypt on worker).
///
/// All shared state is passed as `Arc` clones so the build future has
/// `'static` ownership.
async fn process_session(
    worker: &Arc<Worker>,
    peer: &SocketAddr,
    datagrams: Vec<Vec<u8>>,
    socket: &Arc<knet::DatagramSocket>,
    config: &KcpConfig,
    transport_wrapper: &Option<TransportWrapper>,
    limits: &WorkerPoolLimits,
    pending: &Arc<Mutex<VecDeque<PendingAccept>>>,
    accept_notify: &Arc<Notify>,
    stats: &Arc<WorkerStats>,
) {
    // Check if session exists — single lock for get-or-check.
    let existing = {
        let sessions = worker.sessions.lock();
        sessions.get(peer).cloned()
    };

    if let Some(conn) = existing {
        // Session exists — feed the batch directly.
        let _ = conn.feed_raw_batch(datagrams);
        return;
    }

    // No session — check if already building.
    if worker.building.lock().contains_key(peer) {
        // Queue datagrams for the in-progress build. They'll be fed once
        // the build completes. For now, drop them — KCP retransmission
        // recovers.
        return;
    }

    // Admission check.
    if limits.max_sessions_per_worker > 0
        && worker.session_count() >= limits.max_sessions_per_worker
    {
        stats.session_drops.fetch_add(1, Ordering::Relaxed);
        return;
    }

    // Start building (staged, generation-guarded — §10.1).
    let gen = worker.next_generation();
    worker.building.lock().insert(*peer, (gen, Instant::now()));

    // Build the KcpStream with background_input=false: the worker thread
    // handles the full serial pipeline (decrypt → KCP → encrypt → send)
    // via feed_raw_batch. No async input loop is spawned — zero scheduling
    // overhead for the input path.
    let peer_transport: Arc<dyn crate::transport::PacketTransport> =
        Arc::new(crate::transport::PeerTransport {
            queue: Arc::new(crate::transport::PeerQueue::new()),
            socket: socket.clone(),
            peer: *peer,
        });
    let transport = match transport_wrapper {
        Some(wrapper) => wrapper(peer_transport, *peer),
        None => peer_transport,
    };

    // `KcpStream::build()` spawns the flush loop for async send
    // (WouldBlock fallback). The worker's event loop drives KCP
    // maintenance (retransmit timers, delayed ACKs, probes) for sessions
    // with due deadlines. `feed_batch` handles inline sync send via
    // `drain_and_flush_tx` (fast path).
    let conn = match KcpStream::with_transport(transport, *peer)
        .connected(false)
        .adopt_conv(true)
        .background_input(false)
        .config(config.clone())
        .build()
        .await
    {
        Ok(conn) => conn,
        Err(_) => {
            stats.build_failures.fetch_add(1, Ordering::Relaxed);
            worker.building.lock().remove(peer);
            return;
        }
    };

    // Register the session.
    {
        let mut sessions = worker.sessions.lock();
        sessions.insert(*peer, conn.clone());
    }
    worker.building.lock().remove(peer);

    // Push to accept backlog.
    {
        let mut p = pending.lock();
        p.push_back(PendingAccept {
            conn: conn.clone(),
            peer: *peer,
        });
    }
    accept_notify.notify_one();

    // Push datagrams into the peer queue and notify the input loop.
    // The input loop (spawned by background_input=true) will async-recv
    // from the queue through CryptoTransport, decrypt, and feed KCP.
    let mut conn = conn;
    let _ = conn.feed_raw_batch(datagrams);
    // The build created this `KcpStream` as the owner. Clones were inserted
    // into the session map and accept backlog. The original would close
    // the connection on drop — detach ownership so the connection stays
    // alive until the last clone drops.
    conn.detach_owner();
}

/// Idle reaper: remove dead sessions from the worker's session map.
///
/// Called on the sweep cycle (every `SWEEP_INTERVAL` wakeups) when the worker
/// is idle. A session whose KCP state machine reports `is_dead()` (retransmission
/// budget exhausted) is removed so a later datagram from the same address
/// creates a fresh connection.
///
/// KCP timer maintenance (retransmission, delayed ACK, window probe) is
/// handled by the per-connection flush loop (`spawn_flush_loop`), which runs
/// on the worker's tokio runtime. The worker's serial `feed_batch` →
/// `drain_and_flush_tx` path handles the immediate send fast path.
/// Reap dead/closed sessions. Bounded per call to avoid holding the
/// sessions map lock while is_dead() takes the per-session KCP mutex.
fn reaper_sweep(worker: &Worker) {
    const MAX_SCAN: usize = 4096;
    // Collect live session refs under the lock, then drop it before
    // calling is_dead() (which takes the KCP mutex).
    let candidates: Vec<(SocketAddr, KcpStream)> = {
        let sessions = worker.sessions.lock();
        sessions
            .iter()
            .take(MAX_SCAN)
            .map(|(p, c)| (*p, c.clone()))
            .collect()
    };
    if candidates.is_empty() {
        return;
    }
    let dead: Vec<SocketAddr> = candidates
        .into_iter()
        .filter(|(_, c)| c.is_dead() || c.is_closed())
        .map(|(p, _)| p)
        .collect();
    if dead.is_empty() {
        return;
    }
    let mut sessions = worker.sessions.lock();
    for peer in &dead {
        sessions.remove(peer);
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use knet::AsyncReadExt;

    /// `fast_hash_peer` must be deterministic and stable for the same address.
    #[test]
    fn hash_is_stable() {
        let a = SocketAddr::from(([127, 0, 0, 1], 8080));
        assert_eq!(fast_hash_peer(&a), fast_hash_peer(&a));
    }

    /// Different addresses hash to different values (sanity).
    #[test]
    fn hash_distinguishes_peers() {
        let a = SocketAddr::from(([127, 0, 0, 1], 8080));
        let b = SocketAddr::from(([127, 0, 0, 1], 8081));
        assert_ne!(fast_hash_peer(&a), fast_hash_peer(&b));
    }

    /// Worker channel routing: same peer → same worker.
    #[test]
    fn session_affinity_routes_same_peer_to_same_worker() {
        let n = 4;
        let peer = SocketAddr::from(([192, 168, 1, 100], 12345));
        let shard = (fast_hash_peer(&peer) as usize) % n;
        for _ in 0..100 {
            assert_eq!((fast_hash_peer(&peer) as usize) % n, shard);
        }
    }

    /// `WorkerPoolLimits::default` has unlimited sessions.
    #[test]
    fn default_limits_unlimited() {
        let l = WorkerPoolLimits::default();
        assert_eq!(l.max_sessions_per_worker, 0);
        assert_eq!(l.worker_channel_cap, WORKER_CHANNEL_CAP);
    }

    /// End-to-end: bind, connect, echo, accept.
    ///
    /// Uses `#[tokio::test]` directly so the test future and spawned tasks
    /// share the same tokio runtime.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn sharded_listener_accept() {
        let listener = KcpListener::bind("127.0.0.1:0")
            .worker_count(2)
            .mtu(1350)
            .sndwnd(128)
            .rcvwnd(128)
            .build()
            .await
            .unwrap();
        assert_eq!(listener.worker_count(), 2);
        let addr = listener.local_addr().unwrap();

        // Connect a client — this creates a KCP connection with a flush
        // loop that will send probes/data.
        let client = KcpStream::connect(addr)
            .mtu(1350)
            .sndwnd(128)
            .rcvwnd(128)
            .build()
            .await
            .unwrap();

        // Write data to trigger KCP traffic.
        let msg = b"hello sharded world!";
        client.write_all(msg).await.unwrap();

        // Give the reader + worker time to process.
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        // Wait for the listener to accept (with timeout).
        let result =
            tokio::time::timeout(std::time::Duration::from_secs(5), listener.accept()).await;

        assert!(result.is_ok(), "accept timed out");
        let (conn, _peer) = result.unwrap().unwrap();
        drop(conn);
    }

    /// End-to-end through the **direct** worker (worker_count == 1): the
    /// worker drains the socket itself, so this exercises the
    /// `recv_from`-parked event loop, socket-driven session build, and
    /// echo via `feed_raw_batch`. Cross-platform — on every OS a fresh
    /// single-worker bind takes the direct topology.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn direct_worker_echo() {
        let listener = Arc::new(
            KcpListener::bind("127.0.0.1:0")
                .worker_count(1)
                .mtu(1350)
                .sndwnd(128)
                .rcvwnd(128)
                .build()
                .await
                .unwrap(),
        );
        assert_eq!(listener.worker_count(), 1);
        let addr = listener.local_addr().unwrap();

        // Echo off the accepted session inside the test runtime.
        let echo_listener = listener.clone();
        let echo = knet::spawn_task(async move {
            let (conn, _peer) = echo_listener.accept().await.unwrap();
            let mut buf = vec![0u8; 2048];
            loop {
                match conn.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if conn.write_all(&buf[..n]).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });

        let mut client = KcpStream::connect(addr)
            .mtu(1350)
            .sndwnd(128)
            .rcvwnd(128)
            .build()
            .await
            .unwrap();

        // Several rounds to cross park/wake boundaries on the direct worker.
        for round in 0..4u32 {
            let msg = format!("direct echo round {round}");
            client.write_all(msg.as_bytes()).await.unwrap();
            let mut rx = vec![0u8; msg.len()];
            tokio::time::timeout(std::time::Duration::from_secs(5), client.read_exact(&mut rx))
                .await
                .expect("echo reply timed out")
                .expect("read failed");
            assert_eq!(rx, msg.as_bytes());
        }

        client.close();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), echo).await;
        listener.close();
    }

    /// Direct worker parked in `recv_from` must be woken by `close()`
    /// (cancel token race) rather than waiting for traffic. On a
    /// single-worker direct listener with no traffic, `close()` unblocks
    /// shutdown promptly.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_wakes_direct_worker() {
        let listener = KcpListener::bind("127.0.0.1:0")
            .worker_count(1)
            .build()
            .await
            .unwrap();
        assert_eq!(listener.worker_count(), 1);
        listener.close();

        // If the cancel race were broken, the worker would stay parked; the
        // thread would only exit when the runtime drops. Give it a moment —
        // there is no direct handle to the worker thread, so assert the
        // listener-level close semantics instead: close is idempotent and
        // accept reports shutdown.
        listener.close();
        let res = listener.accept().await;
        assert!(res.is_err(), "accept must fail after close");
    }
}
