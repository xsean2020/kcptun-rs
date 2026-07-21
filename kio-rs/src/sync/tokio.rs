//! tokio backend: Notify.

/// A notification primitive for waking tasks waiting on a condition.
///
/// Wraps `tokio::sync::Notify`.
pub struct Notify {
    inner: tokio::sync::Notify,
}

impl Notify {
    #[inline(always)]
    pub fn new() -> Self {
        Self {
            inner: tokio::sync::Notify::new(),
        }
    }

    /// Wait for a notification. If `notify_one` was called since the last
    /// notification, this returns immediately.
    ///
    /// Returns the future directly (not wrapped in an `async fn`) so that
    /// `tokio::time::timeout` can properly propagate the waker. Wrapping in
    /// `async fn` caused the notify to be silently dropped when used with
    /// `timeout`.
    pub fn notified(&self) -> impl std::future::Future<Output = ()> + '_ {
        self.inner.notified()
    }

    /// Wake one task currently waiting on `notified()`.
    /// If no task is waiting, the permit is stored and the next `notified()`
    /// call returns immediately.
    #[inline(always)]
    pub fn notify_one(&self) {
        self.inner.notify_one();
    }

    /// Wake all tasks currently waiting on `notified()`.
    /// Unlike `notify_one`, this does NOT store a permit for future callers.
    #[inline(always)]
    pub fn notify_waiters(&self) {
        self.inner.notify_waiters();
    }
}

impl Default for Notify {
    fn default() -> Self {
        Self::new()
    }
}
