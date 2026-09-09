//! Cooperative cancellation token + runtime-agnostic `race` combinator.
//!
//! A [`CancellationToken`] lets one task (e.g. a connection's input loop or a
//! listener reader) wait on a socket `recv` and be woken immediately by
//! `close()` instead of polling on a fixed 100 ms tick — removing the
//! per-idle-connection ~10 Hz timer churn. Built on the permit-storing
//! [`super::Notify`], so cancellation is race-safe with waiter registration.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};

use super::{Notify, NotifyFuture};

/// Cooperative cancellation token. `cancel()` wakes waiters; each recv loop
/// holds one and races its socket `recv` against [`CancellationToken::cancelled`].
#[derive(Clone, Default)]
pub struct CancellationToken {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    cancelled: AtomicBool,
    notify: Notify,
}

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    /// Trigger cancellation: set the flag and wake the waiting `cancelled()`
    /// future. The permit-storing `Notify` makes this race-safe with a waiter
    /// that registers after `cancel` — the stored permit resolves it.
    pub fn cancel(&self) {
        self.inner.cancelled.store(true, Ordering::Release);
        self.inner.notify.notify_waiters();
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::Acquire)
    }

    /// A future that resolves once the token is cancelled.
    pub fn cancelled(&self) -> Cancelled<'_> {
        Cancelled {
            token: self,
            notified: self.inner.notify.notified(),
        }
    }
}

/// Future that resolves when its token is cancelled.
pub struct Cancelled<'a> {
    token: &'a CancellationToken,
    notified: NotifyFuture<'a>,
}

impl Future for Cancelled<'_> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.token.is_cancelled() {
            return Poll::Ready(());
        }
        Pin::new(&mut self.get_mut().notified).poll(cx)
    }
}

/// Which future completed first.
pub enum RaceOutcome<A, B> {
    /// The first future completed.
    First(A),
    /// The second future completed.
    Second(B),
}

/// Wait for two futures and return the first to complete; the loser is dropped.
///
/// Runtime-agnostic: both futures are polled in turn on each wakeup, so it runs
/// on any executor without backend-specific `select!`. Both must be `Unpin`
/// (they are re-created per loop iteration and dropped when the other wins, so
/// no pinned state is lost).
pub struct Race<A, B> {
    a: A,
    b: B,
}

impl<A, B> Race<A, B> {
    pub fn new(a: A, b: B) -> Self {
        Self { a, b }
    }
}

impl<A: Future + Unpin, B: Future + Unpin> Future for Race<A, B> {
    type Output = RaceOutcome<A::Output, B::Output>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if let Poll::Ready(out) = Pin::new(&mut this.a).poll(cx) {
            return Poll::Ready(RaceOutcome::First(out));
        }
        if let Poll::Ready(out) = Pin::new(&mut this.b).poll(cx) {
            return Poll::Ready(RaceOutcome::Second(out));
        }
        Poll::Pending
    }
}

/// Await [`Race`]; convenience for `Race::new(a, b).await`.
pub async fn race<A, B>(a: A, b: B) -> RaceOutcome<A::Output, B::Output>
where
    A: Future + Unpin,
    B: Future + Unpin,
{
    Race::new(a, b).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{block_on, spawn_task, yield_now};

    /// Wait (bounded) for the spawned waiter to finish, backend-agnostic: the
    /// `JoinHandle::await` output type differs between tokio (`Result`) and
    /// detached tasks, so we signal via a shared flag instead.
    async fn wait_done(done: &AtomicBool) {
        for _ in 0..10_000 {
            if done.load(Ordering::Acquire) {
                return;
            }
            yield_now().await;
        }
        panic!("waiter did not complete");
    }

    #[test]
    fn cancelled_resolves_when_cancelled() {
        let token = CancellationToken::new();
        assert!(!token.is_cancelled());
        block_on(async move {
            let t = token.clone();
            let done = Arc::new(AtomicBool::new(false));
            let d = done.clone();
            drop(spawn_task(async move {
                t.cancelled().await;
                d.store(true, Ordering::Release);
            }));
            yield_now().await; // let the waiter arm its waker
            token.cancel();
            wait_done(&done).await;
        });
    }

    #[test]
    fn cancelled_fires_before_registration() {
        // cancel() before cancelled() is ever polled must still resolve it
        // (permit-storing Notify) — no lost wake.
        let token = CancellationToken::new();
        block_on(async move {
            token.cancel();
            token.cancelled().await;
        });
    }

    #[test]
    fn race_returns_second_when_cancelled() {
        let token = CancellationToken::new();
        block_on(async move {
            let t = token.clone();
            let done = Arc::new(AtomicBool::new(false));
            let d = done.clone();
            drop(spawn_task(async move {
                // Recv future never completes; only cancellation resolves it.
                match race(std::future::pending::<u32>(), t.cancelled()).await {
                    RaceOutcome::First(_) => {}
                    RaceOutcome::Second(_) => d.store(true, Ordering::Release),
                }
            }));
            yield_now().await;
            token.cancel();
            wait_done(&done).await;
        });
    }
}
