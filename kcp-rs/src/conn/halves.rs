//! `KcpStream` I/O plumbing: `knet::AsyncRead + AsyncWrite` impls and the
//! tokio-style split halves (`ReadHalf`/`WriteHalf`/`Owned*`) with their
//! lifecycle guard.

//! Split out of `conn.rs` (engine/facade layering); no behavior change.

use super::*;

// ─── AsyncRead / AsyncWrite ───────────────────────────────────────────────────

impl KcpStream {
    fn poll_read_into(&self, cx: &mut Context<'_>, out: &mut [u8]) -> Poll<io::Result<usize>> {
        if out.is_empty() {
            return Poll::Ready(Ok(0));
        }
        if self.shared.read_closed.load(Ordering::Acquire) {
            return Poll::Ready(Ok(0));
        }
        let waiter_changed = {
            let mut waiter = self.shared.read_waker.lock();
            let changed = waiter.as_ref().is_none_or(|old| !old.will_wake(cx.waker()));
            *waiter = Some(cx.waker().clone());
            changed
        };
        if waiter_changed {
            *self.shared.read_deadline.lock() = None;
        }
        // A read timeout armed by a previous `Pending` poll: fail once the
        // deadline passes (the timed wake re-polls the task). Copy the value
        // out so the `!Send` guard isn't held across the re-lock below.
        let deadline = *self.shared.read_deadline.lock();
        if let Some(dl) = deadline {
            if knet::mono_ms() >= dl {
                *self.shared.read_deadline.lock() = None;
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "read timed out",
                )));
            }
        }
        let filled = self.read_available(out);
        if filled == 0
            && (self.shared.is_closed() || self.shared.read_closed.load(Ordering::Acquire))
        {
            return Poll::Ready(Ok(0));
        }
        if filled > 0 {
            *self.shared.read_deadline.lock() = None;
            return Poll::Ready(Ok(filled));
        }
        // Arm a one-shot read-timeout wake when a timeout is configured. Only
        // spawned when the caller opted in (`read_timeout` set), so the
        // no-timeout hot path keeps zero extra task spawns.
        let rt_ms = *self.shared.read_timeout.lock();
        let mut rd = self.shared.read_deadline.lock();
        match rt_ms {
            Some(ms) => {
                if rd.is_none() {
                    let dl = knet::mono_ms() + ms;
                    *rd = Some(dl);
                    let shared = self.shared.clone();
                    drop(rd);
                    knet::spawn_task(async move {
                        let now = knet::mono_ms();
                        knet::sleep_ms(dl.saturating_sub(now).max(1)).await;
                        // Clone the waker inside the lock, wake outside. The
                        // slot keeps its waker so the re-polling reader's
                        // `will_wake` recognizes itself and preserves the read
                        // deadline (a `take()` would clear it and re-arm this
                        // timer forever — see kcpstream_read_timeout).
                        let waker = shared.read_waker.lock().clone();
                        if let Some(w) = waker {
                            w.wake_by_ref();
                        }
                    });
                }
            }
            None => {
                *rd = None;
            }
        }
        Poll::Pending
    }

    fn arm_backpressure_wake(&self, cx: &mut Context<'_>) {
        // Store the waker so the flush loop can wake us directly when space
        // becomes available. No spawned task needed for the common case —
        // the flush loop calls `wake_writer()` when `snd_wnd` opens up after
        // ACKs.
        //
        // This eliminates per-backpressure-event task allocation (~200-500
        // bytes + timer-wheel entry) and the 1ms timer quantization that
        // added to P999 tail latency under sustained backpressure.
        //
        // Only spawn a lightweight timeout task if a write timeout is
        // configured — without it, the writer would block indefinitely if
        // backpressure persists beyond the timeout.
        {
            let mut waiter = self.shared.write_waker.lock();
            *waiter = Some(cx.waker().clone());
        }
        // Check for race: backpressure might have been relieved between the
        // check in `do_poll_write` and now. If so, wake immediately.
        if self.shared.backpressure_relieved() {
            cx.waker().wake_by_ref();
            return;
        }
        // Spawn a one-shot timeout task if configured. This is rare (only
        // when user set write_timeout), so the allocation cost is bounded.
        let deadline = *self.shared.write_deadline.lock();
        if let Some(dl) = deadline {
            let shared = self.shared.clone();
            knet::spawn_task(async move {
                let now = knet::mono_ms();
                let wait_ms = dl.saturating_sub(now);
                if wait_ms > 0 {
                    knet::sleep_ms(wait_ms).await;
                }
                // Wake the writer — `do_poll_write` will check the deadline
                // and return TimedOut if expired.
                shared.wake_writer();
            });
        }
    }

    fn do_poll_write(&self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        if self.shared.is_closed() || self.shared.write_closed.load(Ordering::Acquire) {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "KcpStream closed",
            )));
        }
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let waiter_changed = {
            let mut waiter = self.shared.write_waker.lock();
            let changed = waiter.as_ref().is_none_or(|old| !old.will_wake(cx.waker()));
            *waiter = Some(cx.waker().clone());
            changed
        };
        if waiter_changed {
            *self.shared.write_deadline.lock() = None;
        }
        // A write timeout armed by a previous `Pending`: fail once the deadline
        // passes (the timed wake re-polls the task). Copy the value out so the
        // `!Send` guard isn't held across the re-lock below.
        let write_deadline = *self.shared.write_deadline.lock();
        if let Some(dl) = write_deadline {
            if knet::mono_ms() >= dl {
                *self.shared.write_deadline.lock() = None;
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "write timed out",
                )));
            }
        }

        // Inline fast path — mirrors kcp-go UDPSession.Write (kcp.Send +
        // kcp.flush under the KCP lock). When the send window is full, return
        // Pending and let the shared backpressure wake resume the writer.
        let sent = {
            let mut kcp = self.shared.kcp.lock();
            let ws = kcp.wait_send() as usize;
            if ws >= self.shared.snd_wnd.load(Ordering::Relaxed) {
                drop(kcp);
                // Arm the connection-wide backpressure wake, and record the
                // write deadline so a later poll fails with `TimedOut`.
                match *self.shared.write_timeout.lock() {
                    Some(ms) => {
                        let mut deadline = self.shared.write_deadline.lock();
                        if deadline.is_none() {
                            *deadline = Some(knet::mono_ms().saturating_add(ms));
                        }
                    }
                    None => *self.shared.write_deadline.lock() = None,
                }
                self.arm_backpressure_wake(cx);
                return Poll::Pending;
            }
            let sent = self.shared.send_to_kcp(&mut kcp, buf);
            drop(kcp);
            self.shared.write_notify.notify_one();
            sent
        };

        // Single-drainer send: produced segments stay in `raw_packets`; the
        // flush loop is the ONLY drainer+sender. Inline `try_send_batch` here
        // raced the flush loop's deferred sends — on a FIFO link (loopback)
        // out-of-order arrival means the sender interleaved batches, driving
        // receiver `rcv_nxt` gaps → spurious fastack → retransmit storm
        // (measured gap≈10K/2s, gmax≈511 at 256KB@RPS=500, no loss — in≈out).
        if sent > 0 {
            *self.shared.write_deadline.lock() = None;
            self.shared.mark_activity();
            self.shared.flush_notify.notify_one();
        }

        // Return partial write when not all data was sent.  write_all will
        // call poll_write again, re-acquiring the KCP mutex for the next
        // chunk.  This matches Go's Write behavior: block when the window is
        // full rather than buffering a second in-flight window, increasing
        // latency at high RPS.
        // 3. Go's UDPSession.Write blocks immediately on chWriteEvent when
        //    the window is full, providing tighter backpressure.
        Poll::Ready(Ok(sent))
    }

    #[inline]
    fn flush_notify_hint(&self) {
        self.shared.flush_notify.notify_one();
    }
}

#[cfg(feature = "async")]
impl knet::AsyncRead for KcpStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut knet::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let space = buf.initialize_unfilled();
        match this.poll_read_into(cx, space) {
            Poll::Ready(Ok(0)) => Poll::Ready(Ok(())),
            Poll::Ready(Ok(n)) => {
                buf.advance(n);
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
    }
}

#[cfg(feature = "async")]
impl knet::AsyncWrite for KcpStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.do_poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.flush_notify_hint();
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // tokio `AsyncWrite::poll_shutdown` = write-half close (no more writes,
        // pending data still flushed). Full close stays explicit via `close()`.
        self.shared.write_closed.store(true, Ordering::Release);
        self.shared.flush_notify.notify_one();
        self.shared.wake_writer();
        Poll::Ready(Ok(()))
    }
}

// ─── Split halves (tokio-style) ───────────────────────────────────────────────

/// A read half from [`KcpStream::split`]. Implements `knet::AsyncRead`; the
/// underlying state is already shared, so concurrent read/write is safe.
pub struct ReadHalf<'a> {
    inner: &'a KcpStream,
}

/// A write half from [`KcpStream::split`]. Implements `knet::AsyncWrite`.
pub struct WriteHalf<'a> {
    inner: &'a KcpStream,
}

/// An owned read half from [`KcpStream::into_split`]. The connection is closed
/// when the **last** owned half is dropped.
pub struct OwnedReadHalf {
    inner: KcpStream,
    _life: Lifecycle,
}

/// An owned write half from [`KcpStream::into_split`]. The connection is closed
/// when the **last** owned half is dropped.
pub struct OwnedWriteHalf {
    inner: KcpStream,
    _life: Lifecycle,
}

/// Close-on-last-half-drop guard: each owned half holds one `Lifecycle`, and
/// `remaining` (shared via `Arc`) starts at 2 — the last half to drop closes
/// the connection, so background loops don't leak when both halves are gone.
struct Lifecycle {
    shared: Arc<SharedIoState>,
    remaining: Arc<AtomicUsize>,
}

impl Drop for Lifecycle {
    fn drop(&mut self) {
        if self.remaining.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.shared.close();
        }
    }
}

/// Internal accessor so the half trait impls share one code path.
trait HalfConn {
    fn conn(&self) -> &KcpStream;
}
impl<'a> HalfConn for ReadHalf<'a> {
    fn conn(&self) -> &KcpStream {
        self.inner
    }
}
impl<'a> HalfConn for WriteHalf<'a> {
    fn conn(&self) -> &KcpStream {
        self.inner
    }
}
impl HalfConn for OwnedReadHalf {
    fn conn(&self) -> &KcpStream {
        &self.inner
    }
}
impl HalfConn for OwnedWriteHalf {
    fn conn(&self) -> &KcpStream {
        &self.inner
    }
}

impl KcpStream {
    /// Split into borrowing read/write halves. Mirrors `tokio::net::TcpStream::split`.
    pub fn split(&self) -> (ReadHalf<'_>, WriteHalf<'_>) {
        (ReadHalf { inner: self }, WriteHalf { inner: self })
    }

    /// Split into owned read/write halves. Mirrors
    /// `tokio::net::TcpStream::into_split`.
    ///
    /// The connection is closed when the **last** owned half is dropped (a
    /// shared [`Lifecycle`] refcount). The original `KcpStream` is consumed; its
    /// owner-`Drop` would otherwise close immediately.
    pub fn into_split(mut self) -> (OwnedReadHalf, OwnedWriteHalf) {
        self.owns_connection = false;
        let shared = self.shared.clone();
        let remaining = Arc::new(AtomicUsize::new(2));
        let read = OwnedReadHalf {
            inner: self.clone(),
            _life: Lifecycle {
                shared: shared.clone(),
                remaining: remaining.clone(),
            },
        };
        let write = OwnedWriteHalf {
            inner: self.clone(),
            _life: Lifecycle { shared, remaining },
        };
        // `self` drops here with owns_connection=false: no close, no leak.
        (read, write)
    }
}

macro_rules! impl_half_read {
    ($ty:ty) => {
        #[cfg(feature = "async")]
        impl knet::AsyncRead for $ty {
            fn poll_read(
                self: Pin<&mut Self>,
                cx: &mut Context<'_>,
                buf: &mut knet::ReadBuf<'_>,
            ) -> Poll<io::Result<()>> {
                let space = buf.initialize_unfilled();
                match self.conn().poll_read_into(cx, space) {
                    Poll::Ready(Ok(0)) => Poll::Ready(Ok(())),
                    Poll::Ready(Ok(n)) => {
                        buf.advance(n);
                        Poll::Ready(Ok(()))
                    }
                    Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
                    Poll::Pending => Poll::Pending,
                }
            }
        }
    };
}

macro_rules! impl_half_write {
    ($ty:ty) => {
        #[cfg(feature = "async")]
        impl knet::AsyncWrite for $ty {
            fn poll_write(
                self: Pin<&mut Self>,
                cx: &mut Context<'_>,
                buf: &[u8],
            ) -> Poll<io::Result<usize>> {
                self.conn().do_poll_write(cx, buf)
            }

            fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
                self.conn().flush_notify_hint();
                Poll::Ready(Ok(()))
            }

            fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
                let _ = self.conn().shutdown(Shutdown::Write);
                Poll::Ready(Ok(()))
            }
        }
    };
}

impl_half_read!(ReadHalf<'_>);
impl_half_write!(WriteHalf<'_>);

impl OwnedReadHalf {
    /// Async `read` with inline receive — delegates to [`KcpStream::read`].
    pub async fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf).await
    }
}

impl OwnedWriteHalf {
    /// Async `write_all` with inline send — delegates to
    /// [`KcpStream::write_all`], which drains and sends wire segments
    /// directly (async `try_drain_and_send`), bypassing the flush loop's
    /// notify→wake scheduling hop.
    pub async fn write_all(&self, buf: &[u8]) -> io::Result<()> {
        self.inner.write_all(buf).await
    }
}

impl_half_read!(OwnedReadHalf);
impl_half_write!(OwnedWriteHalf);

