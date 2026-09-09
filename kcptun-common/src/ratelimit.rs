//! Token-bucket rate limiter for per-connection packet pacing.
//!
//! Matches Go kcptun's use of `golang.org/x/time/rate` applied in the KCP
//! flush loop before `tx()`. Tokens refill at `rate` bytes/second and use
//! kcp-go's fixed `maxBatchSize * mtuLimit` burst. Zero rate = unlimited.

use std::time::{Duration, Instant};

const GO_RATE_LIMIT_BURST: f64 = 64.0 * 1500.0;

/// A rate limiter that paces the caller without ever blocking a thread.
///
/// Thread-safe (`Mutex`-protected). Designed for per-connection use in the
/// KCP flush/send path. `acquire` never sleeps: it either consumes tokens and
/// returns `Duration::ZERO`, or returns the wait needed without consuming.
/// Callers in async contexts should `knet::sleep(wait).await` and re-acquire.
pub struct RateLimiter {
    inner: parking_lot::Mutex<Inner>,
}

struct Inner {
    /// Configured rate in bytes/sec. 0 = unlimited.
    rate: f64,
    /// Go kcp-go burst size (`maxBatchSize * mtuLimit` = 96,000 bytes).
    burst: f64,
    /// Current token balance (capped at burst).
    tokens: f64,
    /// Last time tokens were refilled.
    last: Instant,
}

impl RateLimiter {
    /// Create a new rate limiter.
    ///
    /// `bytes_per_sec` — maximum sustained rate. 0 means no rate limit.
    /// The burst is fixed at 96,000 bytes, matching kcp-go.
    pub fn new(bytes_per_sec: u32) -> Self {
        let rate = bytes_per_sec as f64;
        RateLimiter {
            inner: parking_lot::Mutex::new(Inner {
                rate,
                burst: GO_RATE_LIMIT_BURST,
                tokens: GO_RATE_LIMIT_BURST,
                last: Instant::now(),
            }),
        }
    }

    /// Reserve tokens for sending `n` bytes under the rate limit, without
    /// blocking. Returns `Duration::ZERO` if `n` bytes were granted
    /// immediately (tokens consumed, or the batch exceeds the burst and is
    /// passed through un-paced, Go `ErrBurst` parity). Otherwise returns the
    /// time the caller must wait and does **not** consume tokens — the caller
    /// should sleep asynchronously (e.g. `knet::sleep(wait).await`) and
    /// re-call `acquire`. `--ratelimit 0` (rate == 0) always grants
    /// immediately.
    pub fn acquire(&self, n: usize) -> Duration {
        let nf = n as f64;
        if nf == 0.0 {
            return Duration::ZERO;
        }
        let mut inner = self.inner.lock();
        if inner.rate <= 0.0 {
            return Duration::ZERO;
        }
        // Go parity: rate.Limiter.WaitN returns ErrBurst when n > burst — it
        // never blocks. tokens is capped at burst, so a batch larger than the
        // burst could never be granted and the caller's pacing loop would
        // stall forever. Grant immediately WITHOUT consuming so the caller's
        // loop breaks and the batch is sent un-paced (forward progress, same
        // as Go / the old blocking acquire clamped to zero and sent anyway).
        if nf > inner.burst {
            return Duration::ZERO;
        }
        inner.refill();
        if inner.tokens >= nf {
            inner.tokens -= nf;
            Duration::ZERO
        } else {
            // Compute the wait to top up the deficit; do not sleep and do
            // not consume — the caller paces asynchronously and re-acquires.
            let deficit = nf - inner.tokens;
            Duration::from_secs_f64(deficit / inner.rate)
        }
    }

    /// Reset the rate (e.g., on reconfiguration).
    pub fn set_rate(&self, bytes_per_sec: u32) {
        let mut inner = self.inner.lock();
        inner.rate = bytes_per_sec as f64;
        inner.burst = GO_RATE_LIMIT_BURST;
        inner.tokens = inner.burst;
        inner.last = Instant::now();
    }

    /// Current configured rate in bytes/sec (0 = unlimited).
    pub fn rate(&self) -> u32 {
        self.inner.lock().rate as u32
    }
}

impl Inner {
    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(self.last);
        self.last = now;
        if elapsed.is_zero() {
            return;
        }
        let added = elapsed.as_secs_f64() * self.rate;
        self.tokens = (self.tokens + added).min(self.burst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_rate_is_unlimited() {
        let lim = RateLimiter::new(0);
        assert_eq!(lim.acquire(1_000_000), Duration::ZERO);
        assert_eq!(lim.acquire(1_000_000_000), Duration::ZERO);
    }

    #[test]
    fn small_burst_passes_immediately() {
        let lim = RateLimiter::new(1_000_000);
        assert_eq!(lim.acquire(90_000), Duration::ZERO);
    }

    #[test]
    fn rate_limit_enforces_wait() {
        let lim = RateLimiter::new(1_000_000);
        assert_eq!(lim.acquire(GO_RATE_LIMIT_BURST as usize), Duration::ZERO);
        // Not enough tokens: returns a wait, does NOT consume. The wait can never
        // exceed the full deficit (tokens are never negative), so the upper bound
        // is deterministic regardless of how much time elapsed between acquires —
        // the old >=80ms floor was CI-load-sensitive.
        let w1 = lim.acquire(90_000);
        assert!(w1 > Duration::ZERO, "should not grant when bucket is empty");
        assert!(
            w1 <= Duration::from_millis(90),
            "wait {:?} exceeds full deficit",
            w1
        );
        // Tokens were NOT consumed by the failed acquire: the deficit persists.
        let w2 = lim.acquire(90_000);
        assert!(
            w2 > Duration::ZERO,
            "deficit must not be consumed on a non-granting acquire"
        );
    }

    #[test]
    fn oversized_batch_is_granted_immediately() {
        // Go parity: rate.Limiter.WaitN returns ErrBurst for n > the fixed
        // 96,000-byte burst, so acquire must grant immediately WITHOUT
        // consuming tokens, letting the caller's pacing loop break and the
        // batch go un-paced (no permanent stall).
        let lim = RateLimiter::new(1024);
        assert_eq!(lim.acquire(100_000), Duration::ZERO);
        assert_eq!(lim.acquire(100_000), Duration::ZERO);
    }

    #[test]
    fn set_rate_dynamically() {
        let lim = RateLimiter::new(1_000_000);
        lim.set_rate(2_000_000);
        assert_eq!(lim.rate(), 2_000_000);
        assert_eq!(lim.acquire(GO_RATE_LIMIT_BURST as usize), Duration::ZERO);
    }

    #[test]
    fn zero_n_returns_immediately() {
        let lim = RateLimiter::new(1_000);
        assert_eq!(lim.acquire(0), Duration::ZERO);
    }
}
