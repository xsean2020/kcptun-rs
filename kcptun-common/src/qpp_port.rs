//! QPP stream wrapper (optional feature `qpp`).
//!
//! The pad table (`pads` + `rpads`, ~160 KiB for 61 pads) depends only on the
//! key and pad count, not on the connection. Building it per connection costs
//! 7×PBKDF2 + ~2×10⁵ AES blocks and blocks the reactor thread. Instead, build
//! it once and share it via [`Arc`]; each connection keeps its own PRNG state.

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::{Buf, BytesMut};
use knet::AsyncRead;
use knet::AsyncWrite;
use knet::ReadBuf;

/// Same as binaries' pipe buffer (64 KiB).
const PIPE_BUF_SIZE: usize = 65536;

/// Shared pad table built once from `(key, count)`. Cheap to clone (Arc).
#[derive(Clone)]
struct SharedPads {
    pads: Arc<[u8]>,
    rpads: Arc<[u8]>,
    num_pads: u16,
}

impl SharedPads {
    fn new(key: &[u8], count: u16) -> Self {
        let qpp = qpp_rs::QuantumPermutationPad::new(key, count);
        // Take ownership of the pad vectors; the QPP's own enc_rand/dec_rand
        // are not needed — each connection has its own PRNG.
        SharedPads {
            pads: Arc::from(qpp.pads.as_slice()),
            rpads: Arc::from(qpp.rpads.as_slice()),
            num_pads: qpp.count(),
        }
    }
}

pub struct QPPPort<T: AsyncRead + AsyncWrite + Unpin> {
    inner: T,
    pads: SharedPads,
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
            pads: SharedPads::new(key, count),
            prng_enc: parking_lot::Mutex::new(qpp_rs::create_prng(key)),
            prng_dec: parking_lot::Mutex::new(qpp_rs::create_prng(key)),
            read_buf: BytesMut::with_capacity(PIPE_BUF_SIZE),
            read_io_buf: vec![0u8; PIPE_BUF_SIZE],
            write_enc_buf: Vec::with_capacity(PIPE_BUF_SIZE),
        }
    }

    /// Create a `QPPPort` that shares a pre-built pad table. Use this when
    /// many connections use the same key — the 7×PBKDF2 + ~2×10⁵ AES block
    /// pad construction is paid once, not per connection.
    pub fn new_with_shared(inner: T, shared: &SharedPads, key: &[u8]) -> Self {
        QPPPort {
            inner,
            pads: shared.clone(),
            prng_enc: parking_lot::Mutex::new(qpp_rs::create_prng(key)),
            prng_dec: parking_lot::Mutex::new(qpp_rs::create_prng(key)),
            read_buf: BytesMut::with_capacity(PIPE_BUF_SIZE),
            read_io_buf: vec![0u8; PIPE_BUF_SIZE],
            write_enc_buf: Vec::with_capacity(PIPE_BUF_SIZE),
        }
    }

    /// Build a shared pad table for use with [`new_with_shared`](Self::new_with_shared).
    pub fn build_shared_pads(key: &[u8], count: u16) -> SharedPads {
        SharedPads::new(key, count)
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
                // No lock on a shared QPP needed — only the per-connection
                // PRNG is mutable, and it is already behind its own Mutex.
                {
                    let mut prng = this.prng_dec.lock();
                    qpp_rs::decrypt_with_pads(
                        &this.pads.rpads,
                        &mut tmp[..filled],
                        &mut prng,
                        this.pads.num_pads,
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
            let mut prng = this.prng_enc.lock();
            qpp_rs::encrypt_with_pads(
                &this.pads.pads,
                &mut this.write_enc_buf,
                &mut prng,
                this.pads.num_pads,
            );
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
