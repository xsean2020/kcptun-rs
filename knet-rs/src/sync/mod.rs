//! Synchronization primitives.
//!
//! `Notify` is a custom **permit-storing** notification primitive used for
//! backpressure signalling in KCP flush loops, read/write paths, etc.
//!
//! It replaces `tokio::sync::Notify` with a permit-storing variant that
//! avoids lost-wakeup races with a single, lightweight implementation:
//!
//! - `notify_one()` stores a permit via `AtomicUsize::fetch_or(1)` — O(1),
//!   no waiter list traversal, no linked-list node allocation.
//! - `notified()` checks the permit first (one atomic swap). If a permit
//!   exists, returns `Ready` immediately without registering a waker.
//!   Only when no permit is available does it register a waker via
//!   `Mutex<Option<Waker>>`.
//!
//! This eliminates the per-call overhead of:
//! - `tokio::sync::Notify`: `Notified` future state machine + waiter list
//!   registration/deregistration (Spinlock-protected doubly-linked list)
//! - `event_listener::Event`: `EventListener` linked-list node creation +
//!   registration/deregistration
//!
//! Both were significant under high RPS where the flush loop calls
//! `notified()` ~1000 times per second.
//!
//! **Single-waiter design**: all current callers (flush_notify, write_notify,
//! read_notify, PeerQueue::notify) have at most one task waiting at a time.
//! `notify_waiters()` behaves like `notify_one()` (wakes the single waiter +
//! stores a permit), which is correct for single-waiter usage.
//!
//! `Mutex` is re-exported from `async_lock` — runtime-agnostic.

pub use async_lock::Mutex;

pub mod cancel;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::{Context, Poll, Waker};

// ─── Notify ───────────────────────────────────────────────────────────────────

/// Notification state.
struct NotifyState {
    /// Number of stored permits (0 or 1).  Set by `notify_one`, cleared by
    /// `notified()` when it consumes a permit.
    permits: AtomicUsize,
    /// True when a waker is currently registered.
    has_waker: AtomicBool,
    /// The registered waker.
    waker: std::sync::Mutex<Option<Waker>>,
}

/// A notification primitive for waking tasks waiting on a condition.
///
/// `notify_one` stores a permit (like tokio's `Notify`), so the next
/// `notified()` call returns immediately even if no task is currently waiting.
///
/// # Single-waiter limitation
///
/// This `Notify` supports **only one concurrent waiter** at a time. If two
/// tasks call `notified()` simultaneously, only the last-registered waker
/// will be notified; earlier waiters will never wake. All current call
/// sites (flush loop, read/write paths, peer-queue) have at most one waiter.
pub struct Notify {
    state: NotifyState,
}

impl Notify {
    #[inline(always)]
    pub fn new() -> Self {
        Self {
            state: NotifyState {
                permits: AtomicUsize::new(0),
                has_waker: AtomicBool::new(false),
                waker: std::sync::Mutex::new(None),
            },
        }
    }

    /// Whether a `notify_one` permit is currently pending (no waiter consumes it
    /// until `notified()` is awaited). Used to skip timer-wheel churn: a caller
    /// that preserved a permit can await `notified()` directly without a timeout.
    #[inline(always)]
    pub fn has_pending(&self) -> bool {
        self.state.permits.load(Ordering::Acquire) != 0
    }

    /// Wait for a notification.
    ///
    /// If `notify_one` was called since the last consumption, returns
    /// immediately.  Otherwise, registers the current task's waker and
    /// returns Pending.
    ///
    /// # Single-waiter limitation
    ///
    /// This `Notify` supports **only one concurrent waiter**. Calling
    /// `notified()` from two tasks simultaneously is a logic error — only
    /// the last-registered waker will be notified. All current call sites
    /// guarantee at most one waiter at a time.
    pub fn notified(&self) -> NotifyFuture<'_> {
        NotifyFuture { notify: self }
    }

    /// Wake one task currently waiting on `notified()`.
    /// If no task is waiting, the permit is stored and the next `notified()`
    /// call returns immediately.
    #[inline(always)]
    pub fn notify_one(&self) {
        // Store a permit first.  If a waiter is registered, wake it.
        let had_permit = self.state.permits.fetch_or(1, Ordering::AcqRel) != 0;
        if had_permit {
            // Already had a permit — nothing to do (coalesce).
            return;
        } // Check if a waker is registered.
        if self.state.has_waker.load(Ordering::Acquire) {
            let waker = self.state.waker.lock().unwrap().take();
            self.state.has_waker.store(false, Ordering::Release);
            if let Some(w) = waker {
                w.wake();
            }
        }
    }

    /// Wake all tasks currently waiting.
    ///
    /// For the single-waker design, this is equivalent to `notify_one()`
    /// since we only track one waker.  Callers that need multi-waker
    /// semantics should use multiple `Notify` instances.
    #[inline(always)]
    pub fn notify_waiters(&self) {
        // Same as notify_one — wake the registered waker (if any) and store
        // a permit for the next waiter.
        self.state.permits.store(1, Ordering::Release);
        if self.state.has_waker.load(Ordering::Acquire) {
            let waker = self.state.waker.lock().unwrap().take();
            self.state.has_waker.store(false, Ordering::Release);
            if let Some(w) = waker {
                w.wake();
            }
        }
    }
}

impl Default for Notify {
    fn default() -> Self {
        Self::new()
    }
}

/// Future returned by [`Notify::notified`].
pub struct NotifyFuture<'a> {
    notify: &'a Notify,
}

impl<'a> std::future::Future for NotifyFuture<'a> {
    type Output = ();

    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        // Fast path: consume a stored permit.
        if self.notify.state.permits.swap(0, Ordering::AcqRel) != 0 {
            return Poll::Ready(());
        }
        // Register our waker.
        let mut waker_slot = self.notify.state.waker.lock().unwrap();
        // Check permit again after acquiring lock (notify may have fired).
        if self.notify.state.permits.swap(0, Ordering::AcqRel) != 0 {
            *waker_slot = None;
            self.notify.state.has_waker.store(false, Ordering::Release);
            return Poll::Ready(());
        }
        // If not already registered, set has_waker.
        if !self.notify.state.has_waker.load(Ordering::Acquire) {
            self.notify.state.has_waker.store(true, Ordering::Release);
        }
        *waker_slot = Some(cx.waker().clone());
        drop(waker_slot);
        // Final check after registration (handles race with notify_one).
        if self.notify.state.permits.swap(0, Ordering::AcqRel) != 0 {
            let mut waker_slot = self.notify.state.waker.lock().unwrap();
            *waker_slot = None;
            self.notify.state.has_waker.store(false, Ordering::Release);
            return Poll::Ready(());
        }
        Poll::Pending
    }
}

impl<'a> Drop for NotifyFuture<'a> {
    fn drop(&mut self) {
        // Clean up our waker registration if we're still the registered one.
        if self.notify.state.has_waker.load(Ordering::Acquire) {
            let mut waker_slot = self.notify.state.waker.lock().unwrap();
            *waker_slot = None;
            self.notify.state.has_waker.store(false, Ordering::Release);
        }
    }
}
