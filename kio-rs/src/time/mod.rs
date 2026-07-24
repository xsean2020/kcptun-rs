//! Time-related primitives: sleep, timeout.

use std::time::Duration;

/// Monotonic timestamp in milliseconds since an arbitrary start point (process
/// start). Avoids repeated `Instant::now()` + locking on the hot path.
/// Matches the semantics of the previous local `mono_ms` helpers.
#[inline]
pub fn mono_ms() -> u64 {
    static BASE: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    let base = BASE.get_or_init(std::time::Instant::now);
    base.elapsed().as_millis() as u64
}

/// Sleep for `ms` milliseconds.
#[inline(always)]
pub async fn sleep_ms(ms: u64) {
    sleep(Duration::from_millis(ms)).await
}

// ─── Backend selection ────────────────────────────────────────────────────────
#[cfg(feature = "tokio")]
mod tokio;

#[cfg(feature = "smol")]
mod smol;

#[cfg(feature = "tokio")]
pub use self::tokio::{sleep, timeout};

#[cfg(feature = "smol")]
pub use self::smol::{sleep, timeout};

// ─── Elapsed error ────────────────────────────────────────────────────────────

/// Error returned when a [`timeout`] elapses before the future completes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Elapsed;

impl std::fmt::Display for Elapsed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "operation timed out")
    }
}

impl std::error::Error for Elapsed {}
