//! Task spawning and CPU-offload primitives.
//!
//! - [`spawn_task`]: fire-and-forget async task (tokio::spawn)
//! - [`cpu_block`]: offload CPU-intensive work to a blocking thread pool
//! - [`block_on`]: runtime entry point (multi-threaded)
//! - [`runtime_kind`]: compile-time tokio (offload policy only)

use std::future::Future;

/// Handle to a spawned task.
///
/// The handle can be awaited to retrieve the task's output. Dropping the
/// handle does NOT cancel the task — it continues running to completion
/// (matching tokio's default detached-spawn semantics).
pub struct JoinHandle<T> {
    inner: ::tokio::task::JoinHandle<T>,
}

impl<T> Future for JoinHandle<T> {
    type Output = Result<T, ::tokio::task::JoinError>;
    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll(cx)
    }
}

mod tokio;

pub use self::tokio::{
    block_on, block_on_local, block_on_multi_thread, cpu_block, spawn_task, yield_now,
};
