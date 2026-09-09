//! Bidirectional pipe with Go-compatible per-direction `closeWait` grace.

use std::io;

use knet::AsyncRead;
use knet::AsyncWrite;

/// Bidirectional copy between two AsyncRead + AsyncWrite streams.
///
/// When either direction completes, the reverse direction remains active for
/// `closewait_secs` before the shared endpoints close.
///
/// If `closewait_secs == 0`, returns immediately after copy completes.
pub async fn pipe<A, B>(a: &mut A, b: &mut B, closewait_secs: u64) -> io::Result<(u64, u64)>
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    knet::copy_bidirectional_postwait(a, b, closewait_secs).await
}
