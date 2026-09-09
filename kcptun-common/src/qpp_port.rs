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
    /// Reusable buffer for QPP encryption — eliminates buf.to_vec() per write.
    write_enc_buf: Vec<u8>,
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
        }
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
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        this.write_enc_buf.clear();
        this.write_enc_buf.extend_from_slice(buf);
        {
            let qpp = this.qpp.lock();
            let mut prng = this.prng_enc.lock();
            qpp_rs::encrypt_with_pads(&qpp.pads, &mut this.write_enc_buf, &mut prng, qpp.count());
        }
        Pin::new(&mut this.inner).poll_write(cx, &this.write_enc_buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}
