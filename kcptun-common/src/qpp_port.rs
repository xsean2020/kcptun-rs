//! QPP stream wrapper (optional feature `qpp`).

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::{Buf, BytesMut};
use knet::AsyncRead;
use knet::AsyncWrite;
use knet::ReadBuf;

/// Same as binaries' pipe buffer (64 KiB).
const PIPE_BUF_SIZE: usize = 65536;

pub struct QPPPort<T: AsyncRead + AsyncWrite + Unpin> {
    inner: T,
    qpp: parking_lot::Mutex<qpp_rs::QuantumPermutationPad>,
    prng_enc: parking_lot::Mutex<qpp_rs::Rand>,
    prng_dec: parking_lot::Mutex<qpp_rs::Rand>,
    read_buf: BytesMut,
    /// Reusable buffer for inner.poll_read — eliminates vec![0u8; PIPE_BUF_SIZE] per call.
    read_io_buf: Vec<u8>,
    /// Ciphertext staged for the inner writer. QPP is a stateful stream
    /// cipher with no resynchronization, so bytes whose pads have already
    /// been consumed must reach the wire exactly once, in order.
    write_enc_buf: Vec<u8>,
    /// How much of `write_enc_buf` the inner writer has accepted.
    write_pos: usize,
}

impl<T: AsyncRead + AsyncWrite + Unpin> QPPPort<T> {
    pub fn new(inner: T, key: &[u8], count: u16) -> Self {
        QPPPort {
            inner,
            qpp: parking_lot::Mutex::new(qpp_rs::QuantumPermutationPad::new(key, count)),
            prng_enc: parking_lot::Mutex::new(qpp_rs::create_prng(key)),
            prng_dec: parking_lot::Mutex::new(qpp_rs::create_prng(key)),
            read_buf: BytesMut::with_capacity(PIPE_BUF_SIZE),
            read_io_buf: vec![0u8; PIPE_BUF_SIZE],
            write_enc_buf: Vec::with_capacity(PIPE_BUF_SIZE),
            write_pos: 0,
        }
    }

    /// Push staged ciphertext to the inner writer.
    ///
    /// Returns `Ready(Ok(()))` once nothing is staged. While ciphertext is
    /// staged the port accepts no new plaintext, so the stage never grows
    /// past one write and backpressure still reaches the caller.
    fn poll_drain(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        while self.write_pos < self.write_enc_buf.len() {
            let pos = self.write_pos;
            match Pin::new(&mut self.inner).poll_write(cx, &self.write_enc_buf[pos..]) {
                Poll::Ready(Ok(0)) => {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "QPP inner writer accepted no bytes",
                    )));
                }
                Poll::Ready(Ok(n)) => self.write_pos += n,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            }
        }
        self.write_enc_buf.clear();
        self.write_pos = 0;
        Poll::Ready(Ok(()))
    }
}

// ── tokio QPPPort AsyncRead/AsyncWrite (uses ReadBuf) ──
impl<T: AsyncRead + AsyncWrite + Unpin> AsyncRead for QPPPort<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();

        if !this.read_buf.is_empty() {
            let n = buf.remaining().min(this.read_buf.len());
            buf.put_slice(&this.read_buf[..n]);
            this.read_buf.advance(n);
            return Poll::Ready(Ok(()));
        }

        let mut tmp = std::mem::take(&mut this.read_io_buf);
        tmp.resize(PIPE_BUF_SIZE, 0);
        let mut read_buf = ReadBuf::new(&mut tmp);
        match Pin::new(&mut this.inner).poll_read(cx, &mut read_buf) {
            Poll::Ready(Ok(())) => {
                let filled = read_buf.filled().len();
                if filled == 0 {
                    this.read_io_buf = tmp;
                    return Poll::Ready(Ok(()));
                }
                // Decrypt in-place in the read buffer (eliminates to_vec())
                {
                    let qpp = this.qpp.lock();
                    let mut prng = this.prng_dec.lock();
                    qpp_rs::decrypt_with_pads(
                        &qpp.rpads,
                        &mut tmp[..filled],
                        &mut prng,
                        qpp.count(),
                    );
                }
                let n = buf.remaining().min(filled);
                buf.put_slice(&tmp[..n]);
                if n < filled {
                    this.read_buf.extend_from_slice(&tmp[n..filled]);
                }
                this.read_io_buf = tmp;
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(e)) => {
                this.read_io_buf = tmp;
                Poll::Ready(Err(e))
            }
            Poll::Pending => {
                this.read_io_buf = tmp;
                Poll::Pending
            }
        }
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin> AsyncWrite for QPPPort<T> {
    /// Encrypt and write `buf`.
    ///
    /// The pads are consumed exactly once per plaintext byte: ciphertext the
    /// inner writer did not accept is staged here rather than re-encrypted on
    /// the caller's retry. Encrypting before the write is confirmed made a
    /// `Poll::Pending` or a short write desynchronize the two PRNG streams
    /// permanently, so everything after it decrypted to garbage.
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();

        // Never interleave new plaintext with staged ciphertext.
        match this.poll_drain(cx) {
            Poll::Ready(Ok(())) => {}
            Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
            Poll::Pending => return Poll::Pending,
        }

        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }

        this.write_enc_buf.clear();
        this.write_enc_buf.extend_from_slice(buf);
        this.write_pos = 0;
        {
            let qpp = this.qpp.lock();
            let mut prng = this.prng_enc.lock();
            qpp_rs::encrypt_with_pads(&qpp.pads, &mut this.write_enc_buf, &mut prng, qpp.count());
        }

        match this.poll_drain(cx) {
            // Fully written, or staged for the next drain: either way the
            // plaintext is now owned by this port.
            Poll::Ready(Ok(())) | Poll::Pending => Poll::Ready(Ok(buf.len())),
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        match this.poll_drain(cx) {
            Poll::Ready(Ok(())) => Pin::new(&mut this.inner).poll_flush(cx),
            other => other,
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        match this.poll_drain(cx) {
            Poll::Ready(Ok(())) => Pin::new(&mut this.inner).poll_shutdown(cx),
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::task::{RawWaker, RawWakerVTable, Waker};

    /// Inner writer that accepts at most `chunk` bytes per call and returns
    /// `Pending` on every `stall_every`-th call — the behaviour `SmuxIo`
    /// exhibits under KCP/SMUX window backpressure.
    struct ChokedWriter {
        written: Arc<Mutex<Vec<u8>>>,
        chunk: usize,
        calls: usize,
        stall_every: usize,
    }

    impl AsyncRead for ChokedWriter {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncWrite for ChokedWriter {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            let this = self.get_mut();
            this.calls += 1;
            if this.stall_every != 0 && this.calls.is_multiple_of(this.stall_every) {
                return Poll::Pending;
            }
            let n = buf.len().min(this.chunk);
            this.written.lock().unwrap().extend_from_slice(&buf[..n]);
            Poll::Ready(Ok(n))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    fn noop_waker() -> Waker {
        fn noop(_: *const ()) {}
        fn clone(_: *const ()) -> RawWaker {
            RawWaker::new(std::ptr::null(), &VTABLE)
        }
        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, noop, noop, noop);
        unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) }
    }

    #[test]
    fn short_writes_and_pending_do_not_desync_the_pads() {
        let key = b"qpp-test-key";
        let plaintext: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();

        let written = Arc::new(Mutex::new(Vec::new()));
        let mut port = QPPPort::new(
            ChokedWriter {
                written: Arc::clone(&written),
                chunk: 100,
                calls: 0,
                stall_every: 3,
            },
            key,
            61,
        );

        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        // Feed the plaintext the way a pipe loop does: keep offering the
        // unconsumed tail until the port has taken everything.
        let mut offset = 0;
        let mut guard = 0;
        while offset < plaintext.len() {
            guard += 1;
            assert!(guard < 100_000, "write loop failed to make progress");
            match Pin::new(&mut port).poll_write(&mut cx, &plaintext[offset..]) {
                Poll::Ready(Ok(n)) => offset += n,
                Poll::Ready(Err(e)) => panic!("write failed: {e}"),
                Poll::Pending => continue,
            }
        }
        // Flush the staged tail.
        for _ in 0..100_000 {
            match Pin::new(&mut port).poll_flush(&mut cx) {
                Poll::Ready(Ok(())) => break,
                Poll::Ready(Err(e)) => panic!("flush failed: {e}"),
                Poll::Pending => continue,
            }
        }

        let ciphertext = written.lock().unwrap().clone();
        assert_eq!(
            ciphertext.len(),
            plaintext.len(),
            "length must be preserved"
        );

        // A receiver with a fresh decrypt PRNG must recover the plaintext
        // exactly. Before the fix, the pads advanced on every retry of an
        // unaccepted buffer and the stream decrypted to garbage from the
        // first short write onwards.
        let qpp = qpp_rs::QuantumPermutationPad::new(key, 61);
        let mut prng = qpp_rs::create_prng(key);
        let mut decrypted = ciphertext;
        qpp_rs::decrypt_with_pads(&qpp.rpads, &mut decrypted, &mut prng, qpp.count());
        assert_eq!(decrypted, plaintext);
    }
}
