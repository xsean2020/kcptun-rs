//! smol backend: Notify.
//!
//! Implemented with `event_listener::Event`.

/// A notification primitive for waking tasks waiting on a condition.
///
/// `notify_one` uses `event.notify(1)` which stores a permit (unlike tokio's
/// `notify_waiters`), so the next `notified()` call returns immediately even
/// if no task is currently waiting.
pub struct Notify {
    event: event_listener::Event,
}

impl Notify {
    #[inline(always)]
    pub fn new() -> Self {
        Self {
            event: event_listener::Event::new(),
        }
    }

    /// Wait for a notification.
    ///
    /// Returns the future directly (not wrapped in an `async fn`) for
    /// consistency with the tokio backend.
    pub fn notified(&self) -> impl std::future::Future<Output = ()> + '_ {
        self.event.listen()
    }

    /// Wake one task currently waiting on `notified()`.
    /// If no task is waiting, the permit is stored and the next `notified()`
    /// call returns immediately.
    #[inline(always)]
    pub fn notify_one(&self) {
        self.event.notify(1);
    }

    /// Wake all tasks currently waiting.
    #[inline(always)]
    pub fn notify_waiters(&self) {
        self.event.notify(usize::MAX);
    }
}

impl Default for Notify {
    fn default() -> Self {
        Self::new()
    }
}
