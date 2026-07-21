//! Task spawning and CPU-offload primitives.
//!
//! - [`spawn_task`]: fire-and-forget async task (tokio::spawn / global Executor)
//! - [`cpu_block`]: offload CPU-intensive work to a blocking thread pool
//! - [`block_on`]: runtime entry point (multi-threaded on both backends)

use std::future::Future;

/// Handle to a spawned task.
///
/// The handle can be awaited to retrieve the task's output. On both backends,
/// dropping the handle does NOT cancel the task — it continues running to
/// completion (matching tokio's default detached-spawn semantics).
#[cfg(feature = "tokio")]
pub struct JoinHandle<T> {
    inner: ::tokio::task::JoinHandle<T>,
}

#[cfg(feature = "tokio")]
impl<T> Future for JoinHandle<T> {
    type Output = Result<T, ::tokio::task::JoinError>;
    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll(cx)
    }
}

#[cfg(feature = "smol")]
pub struct JoinHandle<T> {
    /// `Option` so `Drop` can `take()` the task and `detach()` it (consuming
    /// the `Task`) instead of dropping it (which would cancel the task).
    inner: Option<async_executor::Task<T>>,
}

#[cfg(feature = "smol")]
impl<T> Future for JoinHandle<T> {
    type Output = T;
    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let this = self.get_mut();
        match &mut this.inner {
            Some(task) => match std::pin::Pin::new(task).poll(cx) {
                std::task::Poll::Ready(result) => {
                    // Task completed — drop the Task (safe: nothing to cancel).
                    this.inner = None;
                    std::task::Poll::Ready(result)
                }
                std::task::Poll::Pending => std::task::Poll::Pending,
            },
            None => panic!("JoinHandle polled after completion"),
        }
    }
}

#[cfg(feature = "smol")]
impl<T> Drop for JoinHandle<T> {
    fn drop(&mut self) {
        // Detach the task so it continues running even though the JoinHandle
        // is dropped. This matches tokio's behavior where dropping a JoinHandle
        // does NOT cancel the task. Without this, fire-and-forget spawn_task
        // calls (which drop the handle immediately) would cancel the task on
        // smol, silently killing stream handlers, flush loops, etc.
        if let Some(task) = self.inner.take() {
            task.detach();
        }
    }
}

// ─── Backend selection ────────────────────────────────────────────────────────
#[cfg(feature = "tokio")]
mod tokio;

#[cfg(feature = "smol")]
mod smol;

#[cfg(feature = "tokio")]
pub use self::tokio::{block_on, cpu_block, spawn_task};

#[cfg(feature = "smol")]
pub use self::smol::{block_on, cpu_block, spawn_task};
