//! knet — Tokio-based network I/O extensions for kcptun.
//!
//! Provides mmsg batch UDP, TCP raw sockets, persistent CPU offload pool,
//! custom Notify/CancellationToken, and Go-compatible bidirectional copy
//! with idle/closeWait semantics — functionality not available from raw
//! tokio.

#![allow(clippy::needless_doctest_main)]

use std::time::Duration;

pub use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};

/// Bounded async channel.
pub use async_channel::{bounded, Receiver, Sender, TrySendError};

pub mod net;
pub mod sync;
pub mod task;
pub mod time;

// ─── Convenience re-exports ────────────────────────────────────────────────────
#[cfg(unix)]
pub use net::UnixListener;
pub use net::{tcpraw_dial, tcpraw_listen};
pub use net::{DatagramSocket, TcpListener, TcpStream, UdpSocket};
pub use net::{TcpRawConn, TcpRawListener};
pub use sync::cancel::{race, CancellationToken, Cancelled, Race, RaceOutcome};
pub use sync::Notify;
pub use task::{
    block_on, block_on_local, block_on_multi_thread, cpu_block, spawn_task, yield_now, JoinHandle,
};
pub use time::{mono_ms, sleep, sleep_ms, timeout, Elapsed};

/// Read a file to a string, using a blocking thread pool to avoid stalling
/// the async runtime. Replaces `tokio::fs::read_to_string`.
pub async fn read_to_string(
    path: impl AsRef<std::path::Path> + Send + 'static,
) -> std::io::Result<String> {
    let path = path.as_ref().to_owned();
    cpu_block(move || std::fs::read_to_string(&path)).await
}

/// Bidirectionally copy data with an **idle** timeout.
///
/// Breaks gracefully when no data flows in either direction for `idle_secs`
/// seconds. The idle timer resets after every data transfer, matching Go
/// kcptun's `closeWait` semantics (an idle/cleanup period, NOT a total pipe
/// duration limit).
///
/// If `idle_secs == 0`, behaves as a plain bidirectional copy without timeout.
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

/// Bidirectionally copy data with Go kcptun's `closeWait` grace period.
///
/// When either copy direction finishes, the other direction may continue for
/// `postwait_secs`. The first grace deadline then closes the logical pipe; the
/// second direction receives the same grace before the function returns. This
/// matches Go's two copy goroutines and `sync.Once` close sequence.
///
/// If `postwait_secs == 0`, returns immediately after copy completes
/// (no wait). This is the Go default for the client side.
pub async fn copy_bidirectional_postwait<A, B>(
    a: &mut A,
    b: &mut B,
    postwait_secs: u64,
) -> std::io::Result<(u64, u64)>
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    cfg_copy_bidirectional(a, b, Some(Duration::from_secs(postwait_secs))).await
}

// ─── copy_bidirectional shared state ───────────────────────────────────────────

const BIDI_BUF_SIZE: usize = 65536;

/// Shared state for bidirectional copy — buffers, counters, and EOF flags.
struct BidiState {
    buf_a: Box<[u8; BIDI_BUF_SIZE]>,
    buf_b: Box<[u8; BIDI_BUF_SIZE]>,
    /// Pending A→B data range in `buf_a`: [pending_ab, n_a).
    pending_ab: usize,
    n_a: usize,
    /// Pending B→A data range in `buf_b`: [pending_ba, n_b).
    pending_ba: usize,
    n_b: usize,
    total_a_to_b: u64,
    total_b_to_a: u64,
    a_eof: bool,
    b_eof: bool,
}

impl BidiState {
    fn new() -> Self {
        Self {
            buf_a: Box::new([0u8; BIDI_BUF_SIZE]),
            buf_b: Box::new([0u8; BIDI_BUF_SIZE]),
            pending_ab: 0,
            n_a: 0,
            pending_ba: 0,
            n_b: 0,
            total_a_to_b: 0,
            total_b_to_a: 0,
            a_eof: false,
            b_eof: false,
        }
    }

    #[inline]
    fn has_pending_ab(&self) -> bool {
        self.pending_ab < self.n_a
    }

    #[inline]
    fn has_pending_ba(&self) -> bool {
        self.pending_ba < self.n_b
    }

    #[inline]
    fn advance_ab(&mut self, n: usize) {
        self.total_a_to_b += n as u64;
        self.pending_ab += n;
    }

    #[inline]
    fn advance_ba(&mut self, n: usize) {
        self.total_b_to_a += n as u64;
        self.pending_ba += n;
    }

    #[inline]
    fn set_ab_read(&mut self, n: usize) {
        self.pending_ab = 0;
        self.n_a = n;
    }

    #[inline]
    fn set_ba_read(&mut self, n: usize) {
        self.pending_ba = 0;
        self.n_b = n;
    }

    #[inline]
    fn reset_ab(&mut self) {
        self.pending_ab = 0;
        self.n_a = 0;
    }

    #[inline]
    fn reset_ba(&mut self) {
        self.pending_ba = 0;
        self.n_b = 0;
    }

    #[inline]
    fn pending_ab_slice(&self) -> &[u8] {
        &self.buf_a[self.pending_ab..self.n_a]
    }

    #[inline]
    fn pending_ba_slice(&self) -> &[u8] {
        &self.buf_b[self.pending_ba..self.n_b]
    }

    #[inline]
    fn a_buf_mut(&mut self) -> &mut [u8] {
        &mut self.buf_a[..]
    }

    #[inline]
    fn b_buf_mut(&mut self) -> &mut [u8] {
        &mut self.buf_b[..]
    }

    #[inline]
    fn both_eof(&self) -> bool {
        self.a_eof && self.b_eof
    }

    fn into_result(self) -> (u64, u64) {
        (self.total_a_to_b, self.total_b_to_a)
    }
}

fn update_grace_deadlines(
    state: &BidiState,
    grace: Duration,
    a_deadline: &mut Option<std::time::Instant>,
    b_deadline: &mut Option<std::time::Instant>,
) {
    let now = std::time::Instant::now();
    if state.a_eof && a_deadline.is_none() {
        *a_deadline = Some(now + grace);
    }
    if state.b_eof && b_deadline.is_none() {
        *b_deadline = Some(now + grace);
    }
}

fn enforce_grace_deadlines(
    state: &mut BidiState,
    grace: Duration,
    a_deadline: &mut Option<std::time::Instant>,
    b_deadline: &mut Option<std::time::Instant>,
) -> bool {
    let now = std::time::Instant::now();
    let a_expired = a_deadline.is_some_and(|deadline| deadline <= now);
    let b_expired = b_deadline.is_some_and(|deadline| deadline <= now);

    if a_expired && b_deadline.is_none() {
        state.b_eof = true;
        state.pending_ba = state.n_b;
        *b_deadline = Some(now + grace);
    }
    if b_expired && a_deadline.is_none() {
        state.a_eof = true;
        state.pending_ab = state.n_a;
        *a_deadline = Some(now + grace);
    }

    a_deadline.is_some_and(|deadline| deadline <= now)
        && b_deadline.is_some_and(|deadline| deadline <= now)
}

fn next_grace_wait(
    a_deadline: Option<std::time::Instant>,
    b_deadline: Option<std::time::Instant>,
) -> Option<Duration> {
    let deadline = match (a_deadline, b_deadline) {
        (Some(a), Some(b)) => a.min(b),
        (Some(a), None) => a,
        (None, Some(b)) => b,
        (None, None) => return None,
    };
    Some(deadline.saturating_duration_since(std::time::Instant::now()))
}

async fn cfg_copy_bidirectional<A, B>(
    a: &mut A,
    b: &mut B,
    closewait: Option<Duration>,
) -> std::io::Result<(u64, u64)>
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    // poll_fn-based fair bidirectional copy.
    use std::pin::Pin;
    use std::task::Poll;
    use tokio::io::ReadBuf;

    let mut s = BidiState::new();
    let mut first_error: Option<std::io::Error> = None;
    let mut a_deadline = None;
    let mut b_deadline = None;

    loop {
        if let Some(grace) = closewait {
            if grace.is_zero() {
                // Go-compatible half-close semantics: with closeWait==0 the pipe
                // must STILL wait for the other direction to finish. The peer
                // receives a half-close FIN (poll_shutdown below) and flushes its
                // reply; force-closing here would drop the in-flight reply (e.g.
                // the `printf | nc` echo through a kcptun proxy). A single-side
                // EOF with a dead peer hangs here, matching Go's documented
                // "wait for both EOF" behavior.
                if s.both_eof() {
                    break;
                }
            } else {
                update_grace_deadlines(&s, grace, &mut a_deadline, &mut b_deadline);
                if enforce_grace_deadlines(&mut s, grace, &mut a_deadline, &mut b_deadline) {
                    break;
                }
            }
        } else if s.both_eof() {
            break;
        }

        let step = std::future::poll_fn(|cx| {
            let mut progress = false;

            // Write pending A→B data to B (one write per poll cycle)
            if s.has_pending_ab() {
                match Pin::new(&mut *b).poll_write(cx, s.pending_ab_slice()) {
                    Poll::Ready(Ok(n)) if n > 0 => {
                        s.advance_ab(n);
                        progress = true;
                    }
                    Poll::Ready(Ok(_)) => {} // n == 0: write side full
                    Poll::Ready(Err(e)) => {
                        first_error.get_or_insert(e);
                        s.pending_ab = s.n_a;
                        s.a_eof = true;
                        progress = true;
                    }
                    Poll::Pending => {}
                }
            }

            // Read from A if no pending A→B data
            if !s.has_pending_ab() && !s.a_eof {
                s.reset_ab();
                let mut read_buf = ReadBuf::new(s.a_buf_mut());
                match Pin::new(&mut *a).poll_read(cx, &mut read_buf) {
                    Poll::Ready(Ok(())) => {
                        let n = read_buf.filled().len();
                        if n == 0 {
                            s.a_eof = true;
                        } else {
                            s.set_ab_read(n);
                        }
                        progress = true;
                    }
                    Poll::Ready(Err(e)) => {
                        first_error.get_or_insert(e);
                        s.a_eof = true;
                        progress = true;
                    }
                    Poll::Pending => {}
                }
            }

            // Write pending B→A data to A (one write per poll cycle)
            if s.has_pending_ba() {
                match Pin::new(&mut *a).poll_write(cx, s.pending_ba_slice()) {
                    Poll::Ready(Ok(n)) if n > 0 => {
                        s.advance_ba(n);
                        progress = true;
                    }
                    Poll::Ready(Ok(_)) => {} // n == 0: write side full
                    Poll::Ready(Err(e)) => {
                        first_error.get_or_insert(e);
                        s.pending_ba = s.n_b;
                        s.b_eof = true;
                        progress = true;
                    }
                    Poll::Pending => {}
                }
            }

            // Read from B if no pending B→A data
            if !s.has_pending_ba() && !s.b_eof {
                s.reset_ba();
                let mut read_buf = ReadBuf::new(s.b_buf_mut());
                match Pin::new(&mut *b).poll_read(cx, &mut read_buf) {
                    Poll::Ready(Ok(())) => {
                        let n = read_buf.filled().len();
                        if n == 0 {
                            s.b_eof = true;
                        } else {
                            s.set_ba_read(n);
                        }
                        progress = true;
                    }
                    Poll::Ready(Err(e)) => {
                        first_error.get_or_insert(e);
                        s.b_eof = true;
                        progress = true;
                    }
                    Poll::Pending => {}
                }
            }

            // Half-close the peer on EOF so it knows no more data follows,
            // while this direction keeps draining the other side (Go `Pipe`
            // semantics: CloseWrite on one direction, keep reading the other).
            if s.a_eof {
                let _ = Pin::new(&mut *b).poll_shutdown(cx);
            }
            if s.b_eof {
                let _ = Pin::new(&mut *a).poll_shutdown(cx);
            }

            if progress {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        });

        let grace_wait = next_grace_wait(a_deadline, b_deadline);
        if let Some(wait) = grace_wait {
            let _ = tokio::time::timeout(wait, step).await;
        } else {
            step.await;
        }
    }

    if let Some(error) = first_error {
        Err(error)
    } else {
        Ok(s.into_result())
    }
}

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
        return cfg_copy_bidirectional(a, b, None).await;
    }

    use AsyncReadExt;
    use AsyncWriteExt;

    let mut s = BidiState::new();
    let idle_duration = Duration::from_secs(idle_secs);
    let mut idle_deadline = tokio::time::Instant::now() + idle_duration;

    loop {
        if s.both_eof() {
            break;
        }

        let mut data_flowed = false;

        tokio::select! {
            result = async {
                if s.a_eof { std::future::pending::<std::io::Result<usize>>().await }
                else { a.read(&mut s.buf_a[..]).await }
            } => {
                match result {
                    Ok(0) => {
                        s.a_eof = true;
                        let _ = b.shutdown().await;
                    }
                    Ok(n) => {
                        b.write_all(&s.buf_a[..n]).await?;
                        s.total_a_to_b += n as u64;
                        data_flowed = true;
                    }
                    Err(e) => return Err(e),
                }
            }
            result = async {
                if s.b_eof { std::future::pending::<std::io::Result<usize>>().await }
                else { b.read(&mut s.buf_b[..]).await }
            } => {
                match result {
                    Ok(0) => {
                        s.b_eof = true;
                        let _ = a.shutdown().await;
                    }
                    Ok(n) => {
                        a.write_all(&s.buf_b[..n]).await?;
                        s.total_b_to_a += n as u64;
                        data_flowed = true;
                    }
                    Err(e) => return Err(e),
                }
            }
            _ = tokio::time::sleep_until(idle_deadline) => {
                break;
            }
        }

        if data_flowed {
            idle_deadline = tokio::time::Instant::now() + idle_duration;
        }
    }

    Ok(s.into_result())
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
        // SAFETY: the installed handler only performs an atomic store. It does
        // not allocate, lock, or perform I/O while running in signal context.
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

/// Ignore SIGPIPE to prevent process termination when writing to a closed
/// socket/pipe. Matches Go kcptun's `signal.Ignore(syscall.SIGPIPE)`.
///
/// Call once at process startup. On non-Unix targets this is a no-op.
pub fn ignore_sigpipe() {
    #[cfg(unix)]
    // SAFETY: SIG_IGN is a valid process-wide disposition for SIGPIPE and no
    // Rust data is accessed by a signal callback.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
}

/// Install a handler for SIGUSR1 that logs KCP SNMP statistics.
/// Matches Go kcptun's SIGUSR1 → `log.Printf("KCP SNMP:%+v", kcp.DefaultSnmp.Copy())`.
///
/// Call once at process startup. On non-Unix targets this is a no-op.
pub fn install_sigusr1_handler() {
    #[cfg(unix)]
    {
        extern "C" fn sigusr1_handler(_sig: i32) {
            // Read process-wide counters — minimal work inside signal handler.
            // The actual log output is deferred to the next SNMP poll or
            // handled by reading the static counters from user code.
            // SAFETY: only reads AtomicU64 fields; safe in signal context.
            crate::SIGUSR1_FIRED.store(true, std::sync::atomic::Ordering::SeqCst);
        }

        // SAFETY: the installed handler only performs an atomic store. It does
        // not allocate, lock, or perform I/O while running in signal context.
        unsafe {
            libc::signal(
                libc::SIGUSR1,
                sigusr1_handler as *const () as libc::sighandler_t,
            );
        }
    }
}

/// True if SIGUSR1 was received since the last call to this function.
/// Resets the flag on each call (one-shot semantics matching Go).
pub fn sigusr1_received() -> bool {
    SIGUSR1_FIRED.swap(false, std::sync::atomic::Ordering::SeqCst)
}

#[cfg(unix)]
static SIGUSR1_FIRED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
#[cfg(not(unix))]
static SIGUSR1_FIRED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
mod tests;
