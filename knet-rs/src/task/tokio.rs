//! tokio backend: spawn_task, cpu_block, block_on.
//!
//! `cpu_block` uses a **persistent blocking thread pool** (separate from
//! tokio's async workers) whose threads stay alive for the process lifetime.
//! This replaces `tokio::task::spawn_blocking`, which has per-call overhead
//! of task allocation + scheduling + wake + dealloc. The flush loop calls
//! `cpu_block` every 10–100ms, so a persistent pool with channel dispatch
//! is significantly cheaper.

use super::JoinHandle;
use std::future::Future;
use std::sync::OnceLock;

/// Yield control to the tokio scheduler.  See `mod.rs` docs.
pub async fn yield_now() {
    tokio::task::yield_now().await;
}

// ─── CPU affinity pinning (Linux only) ─────────────────────────────────────
/// Pin the current thread to a specific CPU core via `sched_setaffinity`.
///
/// On Linux, pinning each blocking-pool worker to a dedicated core reduces
/// cache-line bouncing and context-switch overhead under sustained load.
/// On non-Linux targets this is a no-op.
#[cfg(target_os = "linux")]
fn pin_to_core(core_id: usize) {
    // SAFETY: `sched_setaffinity` reads the `cpu_set_t` by reference; we
    // initialize it fully via `CPU_ZERO` + `CPU_SET`. The size argument
    // matches `std::mem::size_of::<cpu_set_t>()`.
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        libc::CPU_ZERO(&mut set);
        libc::CPU_SET(core_id, &mut set);
        let _ = libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set);
    }
}

#[cfg(not(target_os = "linux"))]
#[inline(always)]
fn pin_to_core(_core_id: usize) {}

/// Spawn a fire-and-forget async task.
#[inline(always)]
pub fn spawn_task<F, T>(future: F) -> JoinHandle<T>
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    JoinHandle {
        inner: tokio::spawn(future),
    }
}

// ─── Persistent blocking thread pool ──────────────────────────────────────────
//
// A dedicated pool of N worker threads (N = CPU count, clamped to [2, 8])
// that live for the entire process lifetime. Jobs are type-erased closures
// sent via an **unbounded async_channel** (MPMC: each Receiver is Clone, so
// workers do not share a Mutex around recv). Results return via a per-job
// bounded(1) async_channel so the caller can `.await` without blocking the
// async executor.
//
// This eliminates:
// - `tokio::task::spawn_blocking` per-call task alloc + schedule + wake + dealloc
// - Worker thread creation latency (workers are pre-spawned at first use)

/// A type-erased, boxed, sendable one-shot closure.
type Job = Box<dyn FnOnce() + Send + 'static>;

/// Handle to the persistent blocking pool (initialized lazily on first use).
struct BlockingPool {
    sender: async_channel::Sender<Job>,
}

/// Global singleton pool — workers are spawned once and never exit.
static BLOCKING_POOL: OnceLock<BlockingPool> = OnceLock::new();

/// Lazily initialize the blocking pool (idempotent).
fn blocking_pool() -> &'static BlockingPool {
    BLOCKING_POOL.get_or_init(|| {
        let ncpus = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
            .clamp(2, 8);
        // Unbounded: submit path never blocks the async worker. Capacity is
        // naturally limited by how many flush loops await a result at once.
        let (sender, receiver) = async_channel::unbounded::<Job>();
        for i in 0..ncpus {
            let receiver = receiver.clone();
            std::thread::Builder::new()
                .name(format!("tokio-cpu-{i}"))
                .stack_size(2 * 1024 * 1024)
                .spawn(move || {
                    pin_to_core(i);
                    // Blocking multi-consumer recv — no mutex among workers.
                    // Channel close (all senders dropped) ends the loop.
                    while let Ok(f) = receiver.recv_blocking() {
                        f();
                    }
                })
                .expect("failed to spawn tokio-cpu blocking worker");
        }
        BlockingPool { sender }
    })
}

/// Offload a CPU-intensive / blocking function to the persistent thread pool.
///
/// Workers stay alive for the process lifetime (unlike
/// `tokio::task::spawn_blocking` which lazily spawns threads and incurs
/// per-call task allocation + scheduling overhead), eliminating per-call
/// overhead. The flush loop calls this every 10–100ms under load.
#[inline(always)]
pub async fn cpu_block<F, R>(f: F) -> R
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    let (tx, rx) = async_channel::bounded::<R>(1);
    let job: Job = Box::new(move || {
        let r = f();
        // try_send is non-blocking; bounded(1) always has room for the first
        // item, and the receiver is the only consumer.
        let _ = tx.try_send(r);
    });
    // Unbounded sender: never blocks (unless OOM).
    blocking_pool()
        .sender
        .try_send(job)
        .expect("tokio-cpu blocking pool workers died");
    rx.recv()
        .await
        .expect("tokio-cpu blocking pool worker dropped result")
}

// ─── block_on ──────────────────────────────────────────────────────────────────

/// Global multi-threaded tokio runtime — created once and reused across
/// `block_on` calls. This avoids the ~5-10 ms overhead of constructing a new
/// `Runtime` (OS threads + I/O driver) on every invocation, which matters in
/// test suites and embedded callers that call `block_on` repeatedly.
static GLOBAL_RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

fn global_rt() -> &'static tokio::runtime::Runtime {
    GLOBAL_RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("failed to create tokio runtime")
    })
}

/// Block the current thread on a future, running a multi-threaded tokio runtime.
#[inline(always)]
pub fn block_on<F, T>(future: F) -> T
where
    F: Future<Output = T> + Send,
    T: Send,
{
    global_rt().block_on(future)
}

/// Drive `future` on a fresh **current-thread** tokio runtime (single worker).
///
/// All tasks spawned inside via [`spawn_task`] (which calls `tokio::spawn`)
/// run on this one worker — no cross-thread work-stealing, no multi-worker
/// socket contention. This is used by the server's `--event-loop
/// current-thread` mode (and later by per-SO_REUSEPORT-shard workers) to get
/// serial I/O on a single core while keeping the tokio backend.
///
/// A fresh runtime is built per call; callers are whole servers or shard
/// workers, so the one-time build cost is amortized over the process lifetime.
pub fn block_on_local<F, T>(future: F) -> T
where
    F: Future<Output = T>,
{
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to create current-thread tokio runtime")
        .block_on(future)
}

/// Like `block_on_local` but builds a **multi-thread** runtime.
/// Used by the sharded KCP listener's reader/worker threads
/// so that per-connection input loops and flush loops can run **in parallel**
/// on the runtime's worker pool — matching the old single-runtime architecture
/// that achieved peak throughput.
///
/// The runtime is isolated from the application's main runtime: tasks spawned
/// here (`knet::spawn_task`) stay on this runtime's worker pool.
pub fn block_on_multi_thread<F, T>(future: F) -> T
where
    F: Future<Output = T>,
{
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to create multi-thread tokio runtime")
        .block_on(future)
}
