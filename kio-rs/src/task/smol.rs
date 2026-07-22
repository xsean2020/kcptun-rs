//! smol backend: spawn_task, cpu_block, block_on.
//!
//! `block_on` creates a multi-threaded pool:
//! - N-1 worker threads run the global `async_executor::Executor` to handle
//!   spawned tasks (flush loops, stream handlers, UDP readers, etc.)
//! - The main thread runs `exec.run(future)`, which concurrently drives the
//!   user's future AND processes spawned tasks via `run_forever`.
//!
//! When the main future completes, the stop channel is closed, and all
//! worker threads exit cleanly (joined via `std::thread::scope`).
//!
//! `cpu_block` uses a **persistent blocking thread pool** (separate from the
//! async executor workers) whose threads stay alive for the process lifetime.
//! This replaces `smol::unblock`, which kills idle threads after 500ms —
//! kcptun's flush loop calls `cpu_block` every 10–100ms, so `smol::unblock`
//! was constantly recreating threads (~10–50µs overhead per call).

use super::JoinHandle;
use std::future::Future;
use std::sync::OnceLock;

use async_executor::Executor;

// ─── CPU affinity pinning (Linux only) ─────────────────────────────────────
/// Pin the current thread to a specific CPU core via `sched_setaffinity`.
///
/// On Linux, pinning each smol worker thread to a dedicated core reduces
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

// ─── Global executor ──────────────────────────────────────────────────────────

static GLOBAL_EXEC: OnceLock<Executor<'static>> = OnceLock::new();

/// Get the process-global smol Executor.
fn global_exec() -> &'static Executor<'static> {
    GLOBAL_EXEC.get_or_init(Executor::new)
}

// ─── spawn_task ───────────────────────────────────────────────────────────────

/// Spawn an async task on the global executor.
///
/// The returned `JoinHandle` may be awaited to retrieve the task's output, or
/// dropped for fire-and-forget usage. Dropping the handle does NOT cancel the
/// task (the `JoinHandle`'s `Drop` impl calls `Task::detach()`), matching
/// tokio's `tokio::spawn` semantics.
#[inline(always)]
pub fn spawn_task<F, T>(future: F) -> JoinHandle<T>
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    JoinHandle {
        inner: Some(global_exec().spawn(future)),
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
// - `smol::unblock` thread create/destroy (~10–50µs) on every 10–100ms flush
// - `Mutex<mpsc::Receiver>` contention when many sessions offload encrypt

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
        let ncpus = num_cpus::get().clamp(2, 8);
        // Unbounded: submit path never blocks the async worker. Capacity is
        // naturally limited by how many flush loops await a result at once.
        let (sender, receiver) = async_channel::unbounded::<Job>();
        for i in 0..ncpus {
            let receiver = receiver.clone();
            std::thread::Builder::new()
                .name(format!("smol-cpu-{i}"))
                .stack_size(2 * 1024 * 1024)
                .spawn(move || {
                    pin_to_core(i);
                    // Blocking multi-consumer recv — no mutex among workers.
                    // Channel close (all senders dropped) ends the loop.
                    while let Ok(f) = receiver.recv_blocking() {
                        f();
                    }
                })
                .expect("failed to spawn smol-cpu blocking worker");
        }
        BlockingPool { sender }
    })
}

/// Offload a CPU-intensive / blocking function to the persistent thread pool.
///
/// Workers stay alive for the process lifetime (unlike `smol::unblock` which
/// kills idle threads after 500ms), eliminating per-call thread creation
/// overhead (~10–50µs). The flush loop calls this every 10–100ms under load.
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
        .expect("smol-cpu blocking pool workers died");
    rx.recv()
        .await
        .expect("smol-cpu blocking pool worker dropped result")
}

// ─── block_on (multi-threaded runtime entry point) ────────────────────────────

/// Block the current thread on a future, running a multi-threaded smol runtime.
///
/// Creates `N-1` worker threads, each running `exec.run(pending()).race(stop)`
/// to process spawned tasks until the stop signal arrives. The main thread
/// runs `exec.run(future)`, which concurrently drives the user's future AND
/// processes spawned tasks via the executor's internal `run_forever` loop.
///
/// Both the main thread and worker threads share the same global `Executor`,
/// so spawned tasks (UDP readers, flush loops, stream handlers) are picked up
/// by whichever thread is ready. The `async-io` reactor is thread-safe and
/// shared across all worker threads.
///
/// When the main future completes, the stop channel is closed and all worker
/// threads exit cleanly via `std::thread::scope`.
///
/// On single-core systems, runs single-threaded (no worker threads).
#[inline(always)]
pub fn block_on<F, T>(future: F) -> T
where
    F: Future<Output = T>,
{
    let exec = global_exec();
    let ncpus = num_cpus::get();

    if ncpus > 1 {
        let (stop_tx, stop_rx) = async_channel::bounded::<()>(1);

        let result = std::thread::scope(|s| {
            // Spawn N-1 worker threads to process spawned tasks.
            // Each worker runs exec.run(pending()) which blocks on the
            // executor's run loop, processing spawned tasks as they become
            // ready. The race with stop_rx ensures clean shutdown.
            for i in 0..(ncpus - 1) {
                let stop_rx = stop_rx.clone();
                std::thread::Builder::new()
                    .name(format!("smol-worker-{i}"))
                    .stack_size(2 * 1024 * 1024)
                    .spawn_scoped(s, move || {
                        // Pin this worker to core `i` on Linux to reduce
                        // cache-line bouncing and context-switch overhead.
                        pin_to_core(i);
                        smol::block_on(async {
                            // race() polls both futures; exec.run(pending())
                            // never completes (pending), so this effectively
                            // runs the executor until stop_rx fires.
                            futures_lite::future::race(
                                exec.run(std::future::pending::<()>()),
                                async move {
                                    let _ = stop_rx.recv().await;
                                },
                            )
                            .await;
                        });
                    })
                    .expect("failed to spawn smol worker thread");
            }

            // Main thread: run the user future via exec.run() so that
            // spawned tasks are also processed on this thread.
            let result = smol::block_on(exec.run(future));

            // Signal workers to stop before scope ends.
            stop_tx.close();

            result
        });

        result
    } else {
        // Single-core: run future + executor on the same thread.
        smol::block_on(exec.run(future))
    }
}
