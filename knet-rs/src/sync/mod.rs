//! Synchronization primitives.
//!
//! `Notify` is a custom **permit-storing** notification primitive used for
//! backpressure signalling in KCP flush loops, read/write paths, etc.
//!
//! It replaces `tokio::sync::Notify` with a permit-storing variant that
//! avoids lost-wakeup races with a single, lightweight implementation:
//!
//! - `notify_one()` stores a permit via `AtomicUsize::fetch_or(1)` — O(1),
//!   and only touches the waiter list when somebody is actually parked.
//! - `notified()` checks the permit first (one atomic swap). If a permit
//!   exists, returns `Ready` immediately without registering a waker.
//!   Only when no permit is available does it take the waiter lock.
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
//! **Multiple waiters** are supported: every `notified()` future that parks
//! keeps its own slot in the waker list, so `notify_one()` wakes one of them
//! and `notify_waiters()` wakes all of them. The fast path (a stored permit)
//! never touches the list, so the cost for the common single-waiter call sites
//! is unchanged.
//!
//! The lock-free hand-off between a parking waiter and a notifier is a Dekker
//! pattern: the waiter publishes its slot count and *then* re-reads the permit,
//! while the notifier stores the permit and *then* reads the slot count. Both
//! sides must use `SeqCst` on those four accesses or the store-buffer
//! interleaving loses the wakeup.
//!
//! `Mutex` is re-exported from `async_lock` — runtime-agnostic.

pub use async_lock::Mutex;

pub mod cancel;

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::task::{Context, Poll, Waker};

// ─── Notify ───────────────────────────────────────────────────────────────────

/// One parked `notified()` future.
struct Waiter {
    /// Identifies the future that owns this slot, so a future's `Drop` only
    /// ever removes its own waker.
    id: u64,
    waker: Waker,
}

/// Notification state.
struct NotifyState {
    /// Number of stored permits (0 or 1).  Set by `notify_one`, cleared by
    /// `notified()` when it consumes a permit.
    permits: AtomicUsize,
    /// Number of entries in `waiters`; read on the notify fast path so an
    /// uncontended `notify_one` with nobody parked never takes the lock.
    waiter_count: AtomicUsize,
    /// Registered wakers, oldest first (FIFO wake order).
    waiters: std::sync::Mutex<Vec<Waiter>>,
    /// Hands out `Waiter::id` values.
    next_id: AtomicU64,
}

/// A notification primitive for waking tasks waiting on a condition.
///
/// `notify_one` stores a permit (like tokio's `Notify`), so the next
/// `notified()` call returns immediately even if no task is currently waiting.
///
/// Any number of tasks may await [`notified`](Self::notified) concurrently:
/// `notify_one` wakes the longest-parked one, `notify_waiters` wakes them all.
pub struct Notify {
    state: NotifyState,
}

impl Notify {
    #[inline(always)]
    pub fn new() -> Self {
        Self {
            state: NotifyState {
                permits: AtomicUsize::new(0),
                waiter_count: AtomicUsize::new(0),
                waiters: std::sync::Mutex::new(Vec::new()),
                next_id: AtomicU64::new(0),
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
    /// Safe to await from several tasks at once; each parked future holds its
    /// own waker slot.
    pub fn notified(&self) -> NotifyFuture<'_> {
        NotifyFuture {
            notify: self,
            id: None,
        }
    }

    /// Wake one task currently waiting on `notified()`.
    /// If no task is waiting, the permit is stored and the next `notified()`
    /// call returns immediately.
    #[inline(always)]
    pub fn notify_one(&self) {
        // Store a permit first.  If a waiter is registered, wake it.
        if self.state.permits.fetch_or(1, Ordering::SeqCst) != 0 {
            // Already had a permit — nothing to do (coalesce).
            return;
        }
        // SeqCst: pairs with the parking waiter's `waiter_count` store, which
        // it publishes before its final permit re-read (see module docs).
        if self.state.waiter_count.load(Ordering::SeqCst) == 0 {
            return;
        }
        let waker = {
            let mut waiters = self.state.waiters.lock().unwrap();
            if waiters.is_empty() {
                None
            } else {
                let waiter = waiters.remove(0);
                self.state
                    .waiter_count
                    .store(waiters.len(), Ordering::SeqCst);
                Some(waiter.waker)
            }
        };
        if let Some(w) = waker {
            w.wake();
        }
    }

    /// Wake every task currently waiting on `notified()`, and store a permit
    /// so a task that parks afterwards also returns immediately.
    ///
    /// This is what a terminal transition (`close()`, `cancel()`) needs: with
    /// several readers/writers parked on the same `Notify`, waking only one
    /// leaves the rest hung until unrelated traffic arrives.
    #[inline(always)]
    pub fn notify_waiters(&self) {
        self.state.permits.store(1, Ordering::SeqCst);
        if self.state.waiter_count.load(Ordering::SeqCst) == 0 {
            return;
        }
        let woken = {
            let mut waiters = self.state.waiters.lock().unwrap();
            self.state.waiter_count.store(0, Ordering::SeqCst);
            std::mem::take(&mut *waiters)
        };
        for w in woken {
            w.waker.wake();
        }
    }

    /// Drop `id`'s waker slot if it is still registered.
    fn deregister(&self, id: u64) {
        let mut waiters = self.state.waiters.lock().unwrap();
        if let Some(pos) = waiters.iter().position(|w| w.id == id) {
            waiters.remove(pos);
            self.state
                .waiter_count
                .store(waiters.len(), Ordering::SeqCst);
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
    /// Set once this future has parked and owns a slot in the waker list.
    id: Option<u64>,
}

impl<'a> std::future::Future for NotifyFuture<'a> {
    type Output = ();

    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        // Fast path: consume a stored permit.
        if this.notify.state.permits.swap(0, Ordering::SeqCst) != 0 {
            if let Some(id) = this.id.take() {
                this.notify.deregister(id);
            }
            return Poll::Ready(());
        }
        {
            let mut waiters = this.notify.state.waiters.lock().unwrap();
            match this.id {
                Some(id) => match waiters.iter_mut().find(|w| w.id == id) {
                    // Re-poll of an already-parked future: refresh the waker
                    // in place, keeping our position in the FIFO.
                    Some(slot) => {
                        if !slot.waker.will_wake(cx.waker()) {
                            slot.waker = cx.waker().clone();
                        }
                    }
                    // Our slot was consumed by a `notify_one` that raced with
                    // this poll, and the permit check above already came back
                    // empty (another waiter took it) — park again.
                    None => {
                        let id = this.notify.state.next_id.fetch_add(1, Ordering::Relaxed);
                        this.id = Some(id);
                        waiters.push(Waiter {
                            id,
                            waker: cx.waker().clone(),
                        });
                    }
                },
                None => {
                    let id = this.notify.state.next_id.fetch_add(1, Ordering::Relaxed);
                    this.id = Some(id);
                    waiters.push(Waiter {
                        id,
                        waker: cx.waker().clone(),
                    });
                }
            }
            this.notify
                .state
                .waiter_count
                .store(waiters.len(), Ordering::SeqCst);
        }
        // Our slot is published; re-read the permit. A `notify_one` that
        // stored its permit before we published saw `waiter_count == 0` and
        // returned without waking anybody, so this read is the only thing
        // standing between it and a lost wakeup.
        if this.notify.state.permits.swap(0, Ordering::SeqCst) != 0 {
            if let Some(id) = this.id.take() {
                this.notify.deregister(id);
            }
            return Poll::Ready(());
        }
        Poll::Pending
    }
}

impl<'a> Drop for NotifyFuture<'a> {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            self.notify.deregister(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::sync::atomic::AtomicUsize as Counter;
    use std::sync::Arc;
    use std::task::Wake;

    struct CountingWaker(Counter);

    impl Wake for CountingWaker {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn counting() -> (Waker, Arc<CountingWaker>) {
        let inner = Arc::new(CountingWaker(Counter::new(0)));
        (Waker::from(inner.clone()), inner)
    }

    fn poll_once(fut: &mut NotifyFuture<'_>, waker: &Waker) -> Poll<()> {
        let mut cx = Context::from_waker(waker);
        std::pin::Pin::new(fut).poll(&mut cx)
    }

    #[test]
    fn notify_waiters_wakes_every_parked_task() {
        let notify = Notify::new();
        let (wa, ca) = counting();
        let (wb, cb) = counting();
        let mut a = notify.notified();
        let mut b = notify.notified();
        assert!(poll_once(&mut a, &wa).is_pending());
        assert!(poll_once(&mut b, &wb).is_pending());
        notify.notify_waiters();
        assert_eq!(ca.0.load(Ordering::Relaxed), 1);
        assert_eq!(cb.0.load(Ordering::Relaxed), 1);
        assert!(poll_once(&mut a, &wa).is_ready());
    }

    #[test]
    fn notify_one_wakes_the_longest_parked_waiter() {
        let notify = Notify::new();
        let (wa, ca) = counting();
        let (wb, cb) = counting();
        let mut a = notify.notified();
        let mut b = notify.notified();
        assert!(poll_once(&mut a, &wa).is_pending());
        assert!(poll_once(&mut b, &wb).is_pending());
        notify.notify_one();
        assert_eq!(ca.0.load(Ordering::Relaxed), 1);
        assert_eq!(cb.0.load(Ordering::Relaxed), 0);
        assert!(poll_once(&mut a, &wa).is_ready());
    }

    #[test]
    fn dropping_one_waiter_keeps_the_others_registered() {
        // The single-waker design used to let a dropped future clear whichever
        // waker happened to be in the slot, silently unregistering a live one.
        let notify = Notify::new();
        let (wa, _ca) = counting();
        let (wb, cb) = counting();
        let mut a = notify.notified();
        let mut b = notify.notified();
        assert!(poll_once(&mut a, &wa).is_pending());
        assert!(poll_once(&mut b, &wb).is_pending());
        drop(a);
        notify.notify_one();
        assert_eq!(cb.0.load(Ordering::Relaxed), 1);
        assert!(poll_once(&mut b, &wb).is_ready());
    }

    #[test]
    fn repolling_a_parked_waiter_does_not_leak_slots() {
        let notify = Notify::new();
        let (w, _c) = counting();
        let mut a = notify.notified();
        for _ in 0..8 {
            assert!(poll_once(&mut a, &w).is_pending());
        }
        assert_eq!(notify.state.waiter_count.load(Ordering::Acquire), 1);
        drop(a);
        assert_eq!(notify.state.waiter_count.load(Ordering::Acquire), 0);
    }

    #[test]
    fn permit_stored_before_any_waiter_is_consumed_once() {
        let notify = Notify::new();
        let (w, _c) = counting();
        notify.notify_one();
        let mut a = notify.notified();
        assert!(poll_once(&mut a, &w).is_ready());
        let mut b = notify.notified();
        assert!(poll_once(&mut b, &w).is_pending());
    }
}
