//! smol backend: sleep, timeout.

use super::Elapsed;
use std::time::Duration;

/// Sleep for the given duration.
#[inline(always)]
pub async fn sleep(dur: Duration) {
    let _ = smol::Timer::after(dur).await;
}

/// Wait for `future` to complete, or return [`Elapsed`] after `dur`.
#[inline(always)]
pub async fn timeout<T>(
    dur: Duration,
    future: impl std::future::Future<Output = T>,
) -> Result<T, Elapsed> {
    // `futures_lite::future::or` requires both futures to have the same output
    // type, so we use Option<T> and map None to Elapsed.
    let result = futures_lite::future::or(async move { Some(future.await) }, async move {
        let _ = smol::Timer::after(dur).await;
        None
    })
    .await;
    result.ok_or(Elapsed)
}
