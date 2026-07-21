//! Time-related primitives: sleep, timeout.

use std::time::Duration;

/// Future returned by [`sleep_ms`] / [`sleep`].
#[cfg(feature = "tokio")]
pub type Sleep = ::tokio::time::Sleep;

#[cfg(feature = "smol")]
pub type Sleep = ::smol::Timer;

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
