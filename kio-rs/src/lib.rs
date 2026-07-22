//! kio — Async runtime + network I/O abstraction for kcptun.
//!
//! Provides a unified async API that compiles under either `tokio`
//! or `smol` feature. Business code calls `kio::sleep_ms`,
//! `kio::spawn_task`, etc. without knowing which runtime is active.
//!
//! ## Features
//!
//! - `tokio` (default): backed by tokio. For high-concurrency public servers.
//! - `smol`: backed by smol + async-executor. For embedded / router clients.
//!
//! The two features are **mutually exclusive**. Enabling both is a compile error.

#![allow(clippy::needless_doctest_main)]

use std::time::Duration;

// ─── Feature mutual-exclusion enforcement ──────────────────────────────────────
#[cfg(all(feature = "tokio", feature = "smol"))]
compile_error!("tokio and smol are mutually exclusive; enable only one");

#[cfg(not(any(feature = "tokio", feature = "smol")))]
compile_error!("Must enable either tokio or smol feature");

// ─── Async I/O trait re-exports ────────────────────────────────────────────────
// tokio and futures-lite define DIFFERENT AsyncRead/AsyncWrite traits (different
// poll_read signatures). Re-export the appropriate one so business code is
// runtime-agnostic at the trait-bound level. Concrete I/O wrapper impls
// (SmuxStreamAsync, QPPPort, etc.) must still be cfg-gated.
#[cfg(feature = "tokio")]
pub use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};

#[cfg(feature = "smol")]
pub use futures_lite::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

// ─── Runtime-agnostic channel re-export ───────────────────────────────────────
/// Bounded async channel — works on both tokio and smol runtimes.
///
/// Use `kio::bounded(capacity)` to create a sender/receiver pair.
/// `Sender::try_send` is non-blocking; `Receiver::recv` is async.
pub use async_channel::{bounded, Receiver, Sender};

pub mod net;
pub mod sync;
pub mod task;
pub mod time;

// ─── Convenience re-exports ────────────────────────────────────────────────────
pub use net::{TcpListener, TcpStream, UdpSocket};
pub use sync::Notify;
pub use task::{block_on, cpu_block, spawn_task, JoinHandle};
pub use time::{sleep, sleep_ms, timeout, Elapsed};

/// Read a file to a string, using a blocking thread pool to avoid stalling
/// the async runtime. Replaces `tokio::fs::read_to_string`.
pub async fn read_to_string(
    path: impl AsRef<std::path::Path> + Send + 'static,
) -> std::io::Result<String> {
    let path = path.as_ref().to_owned();
    cpu_block(move || std::fs::read_to_string(&path)).await
}

/// Bidirectionally copy data between two `AsyncRead + AsyncWrite` streams.
/// Returns `(a_to_b_bytes, b_to_a_bytes)`.
///
/// When one direction reaches EOF, the other's write side is shut down so the
/// peer can also complete gracefully.
pub async fn copy_bidirectional<A, B>(a: &mut A, b: &mut B) -> std::io::Result<(u64, u64)>
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    cfg_copy_bidirectional(a, b).await
}

/// Bidirectionally copy data with an **idle** timeout.
///
/// Like [`copy_bidirectional`], but breaks gracefully when no data flows in
/// either direction for `idle_secs` seconds. The idle timer resets after
/// every data transfer, matching Go kcptun's `closeWait` semantics (an
/// idle/cleanup period, NOT a total pipe duration limit).
///
/// If `idle_secs == 0`, behaves identically to [`copy_bidirectional`].
pub async fn copy_bidirectional_idle<A, B>(
    a: &mut A,
    b: &mut B,
    idle_secs: u64,
) -> std::io::Result<(u64, u64)>
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    cfg_copy_bidirectional_idle(a, b, idle_secs).await
}

#[cfg(feature = "tokio")]
async fn cfg_copy_bidirectional<A, B>(a: &mut A, b: &mut B) -> std::io::Result<(u64, u64)>
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    // Custom implementation with 64KB buffers (matching Go kcptun's pipe buffer).
    // tokio::io::copy_bidirectional uses 8KB buffers, which causes excessive
    // flush-loop iterations (each 8KB write triggers a KCP flush cycle).
    use AsyncReadExt;
    use AsyncWriteExt;

    let mut total_a_to_b: u64 = 0;
    let mut total_b_to_a: u64 = 0;
    let mut buf_a = [0u8; 65536];
    let mut buf_b = [0u8; 65536];
    let mut a_eof = false;
    let mut b_eof = false;

    loop {
        if a_eof && b_eof {
            break;
        }

        tokio::select! {
            result = async {
                if a_eof { std::future::pending::<std::io::Result<usize>>().await }
                else { a.read(&mut buf_a).await }
            } => {
                match result {
                    Ok(0) => {
                        a_eof = true;
                        let _ = b.shutdown().await;
                    }
                    Ok(n) => {
                        b.write_all(&buf_a[..n]).await?;
                        total_a_to_b += n as u64;
                    }
                    Err(e) => return Err(e),
                }
            }
            result = async {
                if b_eof { std::future::pending::<std::io::Result<usize>>().await }
                else { b.read(&mut buf_b).await }
            } => {
                match result {
                    Ok(0) => {
                        b_eof = true;
                        let _ = a.shutdown().await;
                    }
                    Ok(n) => {
                        a.write_all(&buf_b[..n]).await?;
                        total_b_to_a += n as u64;
                    }
                    Err(e) => return Err(e),
                }
            }
        }
    }

    Ok((total_a_to_b, total_b_to_a))
}

#[cfg(feature = "smol")]
async fn cfg_copy_bidirectional<A, B>(a: &mut A, b: &mut B) -> std::io::Result<(u64, u64)>
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    use futures_lite::future::poll_fn;
    use std::pin::Pin;
    use std::task::Poll;

    // Heap buffers avoid ~128KiB async-state stack frames on smol workers.
    const BUF: usize = 65536;
    let mut ab_buf = vec![0u8; BUF]; // read-from-A buffer
    let mut ba_buf = vec![0u8; BUF]; // read-from-B buffer
    let mut ab_start = 0usize; // pending A→B data: [ab_start, ab_end) in ab_buf
    let mut ab_end = 0usize;
    let mut ba_start = 0usize; // pending B→A data: [ba_start, ba_end) in ba_buf
    let mut ba_end = 0usize;
    let mut ab_bytes: u64 = 0;
    let mut ba_bytes: u64 = 0;
    let mut a_eof = false;
    let mut b_eof = false;

    while !(a_eof && b_eof) {
        poll_fn(|cx| {
            let mut progress = false;

            // Write pending A→B data to B
            while ab_start < ab_end {
                match Pin::new(&mut *b).poll_write(cx, &ab_buf[ab_start..ab_end]) {
                    Poll::Ready(Ok(n)) if n > 0 => {
                        ab_bytes += n as u64;
                        ab_start += n;
                        progress = true;
                    }
                    Poll::Ready(Ok(_)) => break, // n == 0
                    Poll::Ready(Err(_)) => {
                        a_eof = true;
                        progress = true;
                        break;
                    }
                    Poll::Pending => break,
                }
            }

            // Read from A if no pending data
            if ab_start >= ab_end && !a_eof {
                ab_start = 0;
                ab_end = 0;
                match Pin::new(&mut *a).poll_read(cx, &mut ab_buf) {
                    Poll::Ready(Ok(0)) => {
                        a_eof = true;
                        progress = true;
                    }
                    Poll::Ready(Ok(n)) => {
                        ab_end = n;
                        progress = true;
                    }
                    Poll::Ready(Err(_)) => {
                        a_eof = true;
                        progress = true;
                    }
                    Poll::Pending => {}
                }
            }

            // Write pending B→A data to A
            while ba_start < ba_end {
                match Pin::new(&mut *a).poll_write(cx, &ba_buf[ba_start..ba_end]) {
                    Poll::Ready(Ok(n)) if n > 0 => {
                        ba_bytes += n as u64;
                        ba_start += n;
                        progress = true;
                    }
                    Poll::Ready(Ok(_)) => break,
                    Poll::Ready(Err(_)) => {
                        b_eof = true;
                        progress = true;
                        break;
                    }
                    Poll::Pending => break,
                }
            }

            // Read from B if no pending data
            if ba_start >= ba_end && !b_eof {
                ba_start = 0;
                ba_end = 0;
                match Pin::new(&mut *b).poll_read(cx, &mut ba_buf) {
                    Poll::Ready(Ok(0)) => {
                        b_eof = true;
                        progress = true;
                    }
                    Poll::Ready(Ok(n)) => {
                        ba_end = n;
                        progress = true;
                    }
                    Poll::Ready(Err(_)) => {
                        b_eof = true;
                        progress = true;
                    }
                    Poll::Pending => {}
                }
            }

            // Close write side when the corresponding read side hits EOF
            if a_eof {
                let _ = Pin::new(&mut *b).poll_close(cx);
            }
            if b_eof {
                let _ = Pin::new(&mut *a).poll_close(cx);
            }

            if progress {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })
        .await;
    }

    Ok((ab_bytes, ba_bytes))
}

// ─── copy_bidirectional_idle backend implementations ──────────────────────────

#[cfg(feature = "tokio")]
async fn cfg_copy_bidirectional_idle<A, B>(
    a: &mut A,
    b: &mut B,
    idle_secs: u64,
) -> std::io::Result<(u64, u64)>
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    if idle_secs == 0 {
        return cfg_copy_bidirectional(a, b).await;
    }

    use AsyncReadExt;
    use AsyncWriteExt;

    let mut total_a_to_b: u64 = 0;
    let mut total_b_to_a: u64 = 0;
    let mut buf_a = [0u8; 65536];
    let mut buf_b = [0u8; 65536];
    let mut a_eof = false;
    let mut b_eof = false;
    let idle_duration = Duration::from_secs(idle_secs);

    loop {
        if a_eof && b_eof {
            break;
        }

        // Fresh idle timer each iteration — resets on every data transfer.
        let idle = tokio::time::sleep(idle_duration);
        tokio::pin!(idle);

        tokio::select! {
            result = async {
                if a_eof { std::future::pending::<std::io::Result<usize>>().await }
                else { a.read(&mut buf_a).await }
            } => {
                match result {
                    Ok(0) => {
                        a_eof = true;
                        let _ = b.shutdown().await;
                    }
                    Ok(n) => {
                        b.write_all(&buf_a[..n]).await?;
                        total_a_to_b += n as u64;
                    }
                    Err(e) => return Err(e),
                }
            }
            result = async {
                if b_eof { std::future::pending::<std::io::Result<usize>>().await }
                else { b.read(&mut buf_b).await }
            } => {
                match result {
                    Ok(0) => {
                        b_eof = true;
                        let _ = a.shutdown().await;
                    }
                    Ok(n) => {
                        a.write_all(&buf_b[..n]).await?;
                        total_b_to_a += n as u64;
                    }
                    Err(e) => return Err(e),
                }
            }
            _ = &mut idle => {
                break;
            }
        }
    }

    Ok((total_a_to_b, total_b_to_a))
}

#[cfg(feature = "smol")]
async fn cfg_copy_bidirectional_idle<A, B>(
    a: &mut A,
    b: &mut B,
    idle_secs: u64,
) -> std::io::Result<(u64, u64)>
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    // True idle timeout (matches tokio + Go closeWait): the timer resets on
    // every successful data transfer. A total-duration `timeout()` wrapper
    // would kill long-lived pipes that stay busy — wrong for closeWait.
    if idle_secs == 0 {
        return cfg_copy_bidirectional(a, b).await;
    }

    use futures_lite::future::poll_fn;
    use std::future::Future;
    use std::pin::Pin;
    use std::task::Poll;

    // Heap-allocate 64KiB buffers. Stack arrays of 128KiB plus the async
    // state machine overflow smol worker stacks in debug builds.
    const BUF: usize = 65536;
    let mut ab_buf = vec![0u8; BUF];
    let mut ba_buf = vec![0u8; BUF];
    let mut ab_start = 0usize;
    let mut ab_end = 0usize;
    let mut ba_start = 0usize;
    let mut ba_end = 0usize;
    let mut ab_bytes: u64 = 0;
    let mut ba_bytes: u64 = 0;
    let mut a_eof = false;
    let mut b_eof = false;
    let idle_duration = Duration::from_secs(idle_secs);
    let mut idle = async_io::Timer::after(idle_duration);

    while !(a_eof && b_eof) {
        // Reset idle deadline after every progress step (or on first wait).
        idle.set_after(idle_duration);

        let made_progress = poll_fn(|cx| {
            let mut progress = false;

            while ab_start < ab_end {
                match Pin::new(&mut *b).poll_write(cx, &ab_buf[ab_start..ab_end]) {
                    Poll::Ready(Ok(n)) if n > 0 => {
                        ab_bytes += n as u64;
                        ab_start += n;
                        progress = true;
                    }
                    Poll::Ready(Ok(_)) => break,
                    Poll::Ready(Err(_)) => {
                        a_eof = true;
                        progress = true;
                        break;
                    }
                    Poll::Pending => break,
                }
            }

            if ab_start >= ab_end && !a_eof {
                ab_start = 0;
                ab_end = 0;
                match Pin::new(&mut *a).poll_read(cx, &mut ab_buf[..]) {
                    Poll::Ready(Ok(0)) => {
                        a_eof = true;
                        progress = true;
                    }
                    Poll::Ready(Ok(n)) => {
                        ab_end = n;
                        progress = true;
                    }
                    Poll::Ready(Err(_)) => {
                        a_eof = true;
                        progress = true;
                    }
                    Poll::Pending => {}
                }
            }

            while ba_start < ba_end {
                match Pin::new(&mut *a).poll_write(cx, &ba_buf[ba_start..ba_end]) {
                    Poll::Ready(Ok(n)) if n > 0 => {
                        ba_bytes += n as u64;
                        ba_start += n;
                        progress = true;
                    }
                    Poll::Ready(Ok(_)) => break,
                    Poll::Ready(Err(_)) => {
                        b_eof = true;
                        progress = true;
                        break;
                    }
                    Poll::Pending => break,
                }
            }

            if ba_start >= ba_end && !b_eof {
                ba_start = 0;
                ba_end = 0;
                match Pin::new(&mut *b).poll_read(cx, &mut ba_buf[..]) {
                    Poll::Ready(Ok(0)) => {
                        b_eof = true;
                        progress = true;
                    }
                    Poll::Ready(Ok(n)) => {
                        ba_end = n;
                        progress = true;
                    }
                    Poll::Ready(Err(_)) => {
                        b_eof = true;
                        progress = true;
                    }
                    Poll::Pending => {}
                }
            }

            // Do not busy-poll poll_close: only attempt once per progress step
            // when the peer has EOF'd. Repeated Ready(close) would not set
            // progress, so it is safe here only when progress is already true
            // or we are about to wait.
            if a_eof {
                let _ = Pin::new(&mut *b).poll_close(cx);
            }
            if b_eof {
                let _ = Pin::new(&mut *a).poll_close(cx);
            }

            if progress {
                return Poll::Ready(true);
            }

            match Pin::new(&mut idle).poll(cx) {
                Poll::Ready(_) => Poll::Ready(false),
                Poll::Pending => Poll::Pending,
            }
        })
        .await;

        if !made_progress {
            break;
        }
    }

    Ok((ab_bytes, ba_bytes))
}

/// Wait for Ctrl-C (SIGINT). Uses a dedicated blocking thread with a libc
/// signal handler, so it works on both runtimes without tokio::signal.
pub async fn ctrl_c() -> std::io::Result<()> {
    use std::sync::atomic::{AtomicBool, Ordering};

    static CTRL_C_FIRED: AtomicBool = AtomicBool::new(false);
    static INSTALLED: std::sync::Once = std::sync::Once::new();

    INSTALLED.call_once(|| {
        // Install a minimal SIGINT handler that sets a flag.
        // On non-Unix targets this is a no-op.
        #[cfg(unix)]
        unsafe {
            libc::signal(
                libc::SIGINT,
                sigint_handler as *const () as libc::sighandler_t,
            );
        }
    });

    #[cfg(unix)]
    extern "C" fn sigint_handler(_sig: i32) {
        CTRL_C_FIRED.store(true, Ordering::SeqCst);
    }

    // Poll the flag — cheap and avoids complex async signal machinery.
    loop {
        if CTRL_C_FIRED.load(Ordering::SeqCst) {
            return Ok(());
        }
        sleep_ms(100).await;
    }
}

#[cfg(test)]
mod tests;
