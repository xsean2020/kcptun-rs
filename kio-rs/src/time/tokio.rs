//! tokio backend: sleep, timeout.

use super::Elapsed;
use std::time::Duration;

/// Sleep for the given duration.
#[inline(always)]
pub async fn sleep(dur: Duration) {
    tokio::time::sleep(dur).await
}

/// Wait for `future` to complete, or return [`Elapsed`] after `dur`.
#[inline(always)]
pub async fn timeout<T>(
    dur: Duration,
    future: impl std::future::Future<Output = T>,
) -> Result<T, Elapsed> {
    tokio::time::timeout(dur, future).await.map_err(|_| Elapsed)
}
