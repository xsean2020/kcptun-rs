//! Snappy session codec as an `AsyncRead + AsyncWrite` transport adapter.
//!
//! [`SnappyPipe`] wraps any `knet::AsyncRead + AsyncWrite` stream (e.g. a
//! [`kcp_rs::KcpStream`]) and transparently compresses / decompresses the byte
//! stream with Go-compatible snappy *framing* (the `snap` crate, CRC32C).
//!
//! Placement matches kcptun production: the whole SMUX frame byte stream is
//! compressed as **one** snappy stream (stream header written once) on the KCP
//! user data — see `AGENTS.md` "Snappy compression at SMUX session level".
//!
//! `compress = false` is a byte passthrough (`--nocomp`), preserving the exact
//! wire bytes.

use std::collections::VecDeque;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::{Buf, BytesMut};

use crate::SnappyStreamDecoder;

/// Snappy session codec adapter (`AsyncRead + AsyncWrite`).
///
/// - **Write side** buffers compressed bytes until the inner transport accepts
///   them; backpressure propagates — when inner `poll_write` returns `Pending`,
///   this adapter returns `Pending` before accepting more input.
/// - **Read side** keeps a persistent [`SnappyStreamDecoder`] so snappy frames
///   split across multiple KCP `recv` calls reassemble correctly.
pub struct SnappyPipe<T> {
    inner: T,
    compress: bool,
    // ── write side ──
    encoder: Option<snap::write::FrameEncoder<Vec<u8>>>,
    pending: BytesMut,
    // ── read side ──
    decoder: SnappyStreamDecoder,
    dec_out: VecDeque<u8>,
    read_buf: Vec<u8>,
    read_off: usize,
    read_len: usize,
    eof: bool,
}

impl<T> SnappyPipe<T>
where
    T: knet::AsyncRead + knet::AsyncWrite + Unpin,
{
    /// Wrap `inner`. When `compress` is `true`, writes are snappy-compressed
    /// (session level) and reads decompressed; `false` = byte passthrough.
    pub fn new(inner: T, compress: bool) -> Self {
        SnappyPipe {
            inner,
            compress,
            encoder: compress.then(|| snap::write::FrameEncoder::new(Vec::new())),
            pending: BytesMut::new(),
            decoder: SnappyStreamDecoder::new(),
            dec_out: VecDeque::new(),
            read_buf: vec![0u8; 64 * 1024],
            read_off: 0,
            read_len: 0,
            eof: false,
        }
    }

    /// Mutably borrow the inner transport.
    pub fn get_mut(&mut self) -> &mut T {
        &mut self.inner
    }

    /// Unwrap back to the inner transport.
    pub fn into_inner(self) -> T {
        self.inner
    }

    // ── read side ───────────────────────────────────────────────────────────

    fn poll_read_into(&mut self, cx: &mut Context<'_>, out: &mut [u8]) -> Poll<io::Result<usize>> {
        if out.is_empty() {
            return Poll::Ready(Ok(0));
        }
        // Serve buffered decompressed bytes first.
        if !self.dec_out.is_empty() {
            return Poll::Ready(Ok(self.take_decoded(out)));
        }
        if self.eof {
            return Poll::Ready(Ok(0));
        }

        loop {
            if self.compress {
                // 1. Feed any buffered raw bytes into the decoder.
                if self.read_off < self.read_len {
                    let data = &self.read_buf[self.read_off..self.read_len];
                    match self.decoder.feed(data) {
                        Ok(dec) => {
                            // feed() consumes the whole slice into its internal buffer.
                            self.read_off = self.read_len;
                            if !dec.is_empty() {
                                self.dec_out.extend(dec);
                            }
                        }
                        Err(e) => return Poll::Ready(Err(e)),
                    }
                    if !self.dec_out.is_empty() {
                        return Poll::Ready(Ok(self.take_decoded(out)));
                    }
                }
            } else {
                // Passthrough (`--nocomp`): return raw inner bytes directly — the
                // wire is not snappy-framed, so the decoder would never emit.
                if self.read_off < self.read_len {
                    let avail = self.read_len - self.read_off;
                    let n = out.len().min(avail);
                    out[..n].copy_from_slice(&self.read_buf[self.read_off..self.read_off + n]);
                    self.read_off += n;
                    if self.read_off == self.read_len {
                        self.read_off = 0;
                        self.read_len = 0;
                    }
                    return Poll::Ready(Ok(n));
                }
            }

            // 2. Reset the read buffer (fully consumed).
            self.read_off = 0;
            self.read_len = 0;

            // 3. Make room if a single inner read filled the buffer without
            //    producing decompressed output (incompressible large chunks).
            if self.read_len == self.read_buf.len() {
                let extra = self.read_buf.len();
                self.read_buf.resize(self.read_buf.len() + extra, 0);
            }

            // 4. Read more raw bytes from the inner transport.
            match inner_poll_read(&mut self.inner, cx, &mut self.read_buf[self.read_len..]) {
                Poll::Ready(Ok(0)) => {
                    self.eof = true;
                    return Poll::Ready(Ok(0));
                }
                Poll::Ready(Ok(n)) => self.read_len += n,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            }
        }
    }

    fn take_decoded(&mut self, out: &mut [u8]) -> usize {
        let n = out.len().min(self.dec_out.len());
        // Two memcpy operations instead of a per-byte drain loop.
        // VecDeque may wrap around the ring buffer, so we copy in
        // up to two contiguous chunks.
        let (front, back) = self.dec_out.as_slices();
        let first = n.min(front.len());
        out[..first].copy_from_slice(&front[..first]);
        if first < n {
            let rest = n - first;
            out[first..n].copy_from_slice(&back[..rest]);
        }
        self.dec_out.drain(..n);
        n
    }

    // ── write side ──────────────────────────────────────────────────────────

    fn poll_write_impl(&mut self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        // 1. Drain any pending bytes to the inner transport first; propagate
        //    backpressure (Pending) so we don't buffer unboundedly.
        if !self.pending.is_empty() {
            match self.drain_pending(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Ready(Ok(_)) => {}
            }
        }

        // 2. Accept new input into `pending` (compress or passthrough).
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        if self.compress {
            use std::io::Write;
            let enc = self.encoder.as_mut().expect("encoder set when compress");
            if enc.write_all(buf).is_err() || enc.flush().is_err() {
                return Poll::Ready(Err(io::Error::other("snappy encode failed")));
            }
            self.pending
                .extend_from_slice(&std::mem::take::<Vec<u8>>(enc.get_mut()));
        } else {
            self.pending.extend_from_slice(buf);
        }

        // 3. Best-effort drain; we've already accepted `buf` (buffered), so a
        //    blocked inner transport defers to the next poll_write call.
        match self.drain_pending(cx) {
            Poll::Pending | Poll::Ready(Ok(_)) => {}
            Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
        }
        Poll::Ready(Ok(buf.len()))
    }

    /// Flush the encoder and drain everything to the inner transport.
    fn finish_write_side(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.compress {
            use std::io::Write;
            let enc = self.encoder.as_mut().expect("encoder set when compress");
            let _ = enc.flush();
            if !enc.get_mut().is_empty() {
                self.pending
                    .extend_from_slice(&std::mem::take::<Vec<u8>>(enc.get_mut()));
            }
        }
        match self.drain_pending(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Ready(Ok(_)) => Poll::Ready(Ok(())),
        }
    }

    fn poll_flush_impl(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.finish_write_side(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
            Poll::Ready(Ok(())) => {}
        }
        inner_poll_flush(&mut self.inner, cx)
    }

    fn poll_shutdown_impl(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.finish_write_side(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
            Poll::Ready(Ok(())) => {}
        }
        inner_poll_shutdown(&mut self.inner, cx)
    }

    fn drain_pending(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        loop {
            if self.pending.is_empty() {
                return Poll::Ready(Ok(()));
            }
            match inner_poll_write(&mut self.inner, cx, &self.pending) {
                Poll::Ready(Ok(0)) => return Poll::Ready(Ok(())),
                Poll::Ready(Ok(n)) => self.pending.advance(n),
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

// ─── Inner transport poll helpers (trait signature differs per runtime) ───────

fn inner_poll_read<T: knet::AsyncRead + Unpin>(
    inner: &mut T,
    cx: &mut Context<'_>,
    buf: &mut [u8],
) -> Poll<io::Result<usize>> {
    let mut rb = knet::ReadBuf::new(buf);
    match Pin::new(inner).poll_read(cx, &mut rb) {
        Poll::Ready(Ok(())) => Poll::Ready(Ok(rb.filled().len())),
        Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
        Poll::Pending => Poll::Pending,
    }
}

fn inner_poll_write<T: knet::AsyncWrite + Unpin>(
    inner: &mut T,
    cx: &mut Context<'_>,
    buf: &[u8],
) -> Poll<io::Result<usize>> {
    Pin::new(inner).poll_write(cx, buf)
}

fn inner_poll_flush<T: knet::AsyncWrite + Unpin>(
    inner: &mut T,
    cx: &mut Context<'_>,
) -> Poll<io::Result<()>> {
    Pin::new(inner).poll_flush(cx)
}

fn inner_poll_shutdown<T: knet::AsyncWrite + Unpin>(
    inner: &mut T,
    cx: &mut Context<'_>,
) -> Poll<io::Result<()>> {
    Pin::new(inner).poll_shutdown(cx)
}

// ─── knet::AsyncRead / AsyncWrite impls (feature-gated signatures) ─────────────

#[cfg(feature = "tokio")]
impl<T> knet::AsyncRead for SnappyPipe<T>
where
    T: knet::AsyncRead + knet::AsyncWrite + Unpin,
{
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

#[cfg(feature = "tokio")]
impl<T> knet::AsyncWrite for SnappyPipe<T>
where
    T: knet::AsyncRead + knet::AsyncWrite + Unpin,
{
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.get_mut().poll_write_impl(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.get_mut().poll_flush_impl(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.get_mut().poll_shutdown_impl(cx)
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(all(test, feature = "tokio"))]
mod tests {
    use super::*;
    use kcp_rs::KcpConfig;
    use knet::AsyncReadExt;
    use knet::AsyncWriteExt;
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::time::Duration;

    use crate::kcp_transport::kcp_stream_with_socket;
    use kcrypt_rs::OffloadProfile;

    /// Pair of connected UDP sockets + null-crypt KcpStream.
    async fn make_pair(cfg: KcpConfig) -> (kcp_rs::KcpStream, kcp_rs::KcpStream) {
        let a_tmp = knet::UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        let b_tmp = knet::UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        let addr_a = a_tmp.local_addr().unwrap();
        let addr_b = b_tmp.local_addr().unwrap();
        drop(a_tmp);
        drop(b_tmp);

        let sock_a = Arc::new(knet::DatagramSocket::Udp(
            knet::UdpSocket::connect(addr_a, addr_b).unwrap(),
        ));
        let sock_b = Arc::new(knet::DatagramSocket::Udp(
            knet::UdpSocket::connect(addr_b, addr_a).unwrap(),
        ));

        let key = b"0123456789abcdef0123456789abcdef";
        let a = kcp_stream_with_socket(
            sock_a,
            addr_b,
            key,
            "null",
            cfg.clone(),
            true,
            OffloadProfile::Tokio,
        )
        .await
        .unwrap();
        let b = kcp_stream_with_socket(
            sock_b,
            addr_a,
            key,
            "null",
            cfg,
            true,
            OffloadProfile::Tokio,
        )
        .await
        .unwrap();
        (a, b)
    }

    fn test_cfg() -> KcpConfig {
        KcpConfig {
            conv: 0x5A11_5EED,
            mode: kcp_rs::KcpMode::Fast3,
            sndwnd: 512,
            rcvwnd: 512,
            ..KcpConfig::default()
        }
    }

    async fn read_exact(
        conn: &mut (impl knet::AsyncRead + Unpin),
        buf: &mut [u8],
        limit: Duration,
    ) {
        let deadline = std::time::Instant::now() + limit;
        let mut filled = 0usize;
        while filled < buf.len() {
            if std::time::Instant::now() > deadline {
                panic!("timeout waiting for data, got {}/{}", filled, buf.len());
            }
            match knet::timeout(Duration::from_millis(50), conn.read(&mut buf[filled..])).await {
                Ok(Ok(0)) => panic!("unexpected EOF at {}", filled),
                Ok(Ok(n)) => filled += n,
                Ok(Err(e)) => panic!("read error: {}", e),
                Err(_) => continue,
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn snappy_pipe_compressed_kcp_roundtrip() {
        let (a, b) = make_pair(test_cfg()).await;
        let mut a = SnappyPipe::new(a, true);
        let mut b = SnappyPipe::new(b, true);

        // Mixed payload: compressible prefix + varied tail, split across several
        // writes to exercise the persistent stream (single sNaPpY header).
        let mut payload = Vec::with_capacity(256 * 1024);
        payload.extend(std::iter::repeat_n(b'a', 128 * 1024));
        payload.extend((0u32..131072).map(|i| (i % 251) as u8));

        let mut written = 0usize;
        while written < payload.len() {
            let end = (written + 16 * 1024).min(payload.len());
            a.write_all(&payload[written..end]).await.unwrap();
            a.flush().await.unwrap();
            written = end;
        }

        let mut got = vec![0u8; payload.len()];
        read_exact(&mut b, &mut got, Duration::from_secs(10)).await;
        assert_eq!(got, payload);

        // Reverse direction too.
        let reply = b"snappy-pipe-reverse-reply!";
        b.write_all(reply).await.unwrap();
        b.flush().await.unwrap();
        let mut got_reply = vec![0u8; reply.len()];
        read_exact(&mut a, &mut got_reply, Duration::from_secs(10)).await;
        assert_eq!(&got_reply[..], reply);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn snappy_pipe_passthrough_kcp_roundtrip() {
        let (a, b) = make_pair(test_cfg()).await;
        let mut a = SnappyPipe::new(a, false);
        let mut b = SnappyPipe::new(b, false);

        let payload = (0u32..65536).map(|i| (i % 251) as u8).collect::<Vec<_>>();
        a.write_all(&payload).await.unwrap();
        a.flush().await.unwrap();

        let mut got = vec![0u8; payload.len()];
        read_exact(&mut b, &mut got, Duration::from_secs(10)).await;
        assert_eq!(got, payload);
    }
}
