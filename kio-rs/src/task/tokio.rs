//! tokio backend: spawn_task, cpu_block, block_on.

use super::JoinHandle;
use std::future::Future;

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

/// Offload a CPU-intensive / blocking function to tokio's blocking thread pool.
#[inline(always)]
pub async fn cpu_block<F, R>(f: F) -> R
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .expect("spawn_blocking task panicked")
}

/// Block the current thread on a future, running a multi-threaded tokio runtime.
#[inline(always)]
pub fn block_on<F, T>(future: F) -> T
where
    F: Future<Output = T> + Send,
    T: Send,
{
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to create tokio runtime");
    runtime.block_on(future)
}
