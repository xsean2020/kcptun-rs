//! Encrypted packet transport and `KcpStream` construction helpers.
//!
//! Protocol stack (matches Go kcp-go v5 / kcptun):
//!
//! ```text
//! inbound:  UDP → decrypt → FEC → KCP
//! outbound: KCP → FEC → encrypt → UDP
//! ```
//!
//! Encryption lives **outside** [`kcp_rs::KcpStream`]: this module wraps a plain
//! datagram socket and implements [`kcp_rs::PacketTransport`] so KcpStream talks
//! to ciphertext transparently.
//!
//! Feature-gated on `tokio` (needs knet + kcp-rs async).

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use parking_lot::Mutex;

use kcp_rs::{KcpConfig, KcpStream, PacketTransport};
use kcrypt_rs::crypt::CryptEngine;
use kcrypt_rs::wire::{
    decrypt_cfb_in_place, encrypt_batch, encrypt_batch_into, encrypt_batch_ref_into, inbound_null,
    should_cpu_block_encrypt, CryptoBuf, OffloadProfile,
};

use crate::RateLimiter;

/// Default conversation ID used as CryptoBuf session_id seed (matches client).
const DEFAULT_CONV: u32 = 0xDEAD_BEEF;
/// Distinct seed for ACK-path CryptoBuf so nonces never collide with data path.
const ACK_SESSION_XOR: u64 = 0xA11C_B0FF_u64;

// ─── CryptoTransport ──────────────────────────────────────────────────────────

/// Datagram transport that encrypts/decrypts packets around an inner socket.
///
/// - `"null"`: passthrough (no header)
/// - CFB ciphers / `"none"`: 20B header `[nonce 16][CRC32 4][payload]`
/// - `"aes-128-gcm"`: AEAD seal/open
///
/// Data path uses `data_crypto_buf`; ACK / urgent path uses `ack_crypto_buf`
/// (matches client `ack_crypto_buf` intent — avoid lock contention with flush).
pub struct CryptoTransport {
    inner: Arc<dyn PacketTransport>,
    crypt: Arc<CryptEngine>,
    /// `crypt != "null"` — `"none"` still packs a CFB header.
    has_encryption: bool,
    data_crypto_buf: Arc<Mutex<CryptoBuf>>,
    ack_crypto_buf: Arc<Mutex<CryptoBuf>>,
    /// Reusable output buffer for `encrypt_sync` — avoids per-flush `Vec<Bytes>`
    /// allocation on the sync TX path. Guarded by `is_sending` CAS in
    /// `drain_and_flush_tx`, so only one sender touches this at a time.
    data_tx_buf: Mutex<Vec<Bytes>>,
    /// Runtime-shaped CPU offload profile (per-session, not global).
    offload_profile: OffloadProfile,
    /// Go applies rate limiting after FEC and encryption, immediately before
    /// the datagram batch is transmitted.
    rate_limiter: RateLimiter,
}

impl CryptoTransport {
    /// Wrap `inner` with the given cipher method and raw 32-byte key material.
    ///
    /// `method` is the Go-compatible name (`"aes"`, `"null"`, `"aes-128-gcm"`, …).
    pub fn new(inner: Arc<knet::DatagramSocket>, key: &[u8], method: &str) -> Self {
        Self::with_transport(inner, key, method)
    }

    /// Wrap any packet transport, including a listener's per-peer queue.
    pub fn with_transport(inner: Arc<dyn PacketTransport>, key: &[u8], method: &str) -> Self {
        let (engine, _) = CryptEngine::select(method, key);
        Self::from_engine(inner, Arc::new(engine), method != "null")
    }

    /// Wrap with a pre-built [`CryptEngine`].
    pub fn from_engine(
        inner: Arc<dyn PacketTransport>,
        crypt: Arc<CryptEngine>,
        has_encryption: bool,
    ) -> Self {
        Self {
            inner,
            crypt,
            has_encryption,
            data_crypto_buf: Arc::new(Mutex::new(CryptoBuf::new(DEFAULT_CONV as u64))),
            ack_crypto_buf: Arc::new(Mutex::new(CryptoBuf::new(
                (DEFAULT_CONV as u64) ^ ACK_SESSION_XOR,
            ))),
            data_tx_buf: Mutex::new(Vec::new()),
            offload_profile: OffloadProfile::Tokio,
            rate_limiter: RateLimiter::new(0),
        }
    }

    /// Set the runtime offload profile (defaults to Tokio).
    pub fn set_offload_profile(&mut self, profile: OffloadProfile) {
        self.offload_profile = profile;
    }

    /// Set the on-wire byte rate. Zero disables limiting.
    pub fn set_rate_limit(&mut self, bytes_per_second: u32) {
        self.rate_limiter = RateLimiter::new(bytes_per_second);
    }

    async fn wait_for_rate(&self, packets: &[Bytes]) {
        let bytes = packets.iter().map(Bytes::len).sum();
        loop {
            let wait = self.rate_limiter.acquire(bytes);
            if wait.is_zero() {
                return;
            }
            knet::sleep(wait).await;
        }
    }

    /// Access the underlying socket (diagnostics / local_addr).
    #[allow(dead_code)]
    pub(crate) fn inner(&self) -> &Arc<dyn PacketTransport> {
        &self.inner
    }

    #[allow(dead_code)]
    pub(crate) fn crypt(&self) -> &Arc<CryptEngine> {
        &self.crypt
    }

    #[allow(dead_code)]
    pub(crate) fn has_encryption(&self) -> bool {
        self.has_encryption
    }

    /// Encrypt a batch on the **data** path (flush loop). May offload heavy
    /// cipher batches to `cpu_block` (M0.2).
    async fn encrypt_data(&self, packets: Vec<Bytes>) -> Vec<Bytes> {
        self.encrypt_with(&self.data_crypto_buf, packets, false)
            .await
    }

    /// Encrypt a batch on the **ACK / urgent** path. **Never offloads** to
    /// `cpu_block`: ACKs must go out promptly or the peer's send window stalls
    /// (with FEC, a single ACK expands to a 13-packet batch that would trip the
    /// offload heuristic and add a blocking-pool hop). Matches the legacy
    /// binary's inline ACK encrypt.
    async fn encrypt_urgent(&self, packets: Vec<Bytes>) -> Vec<Bytes> {
        self.encrypt_with(&self.ack_crypto_buf, packets, true).await
    }

    async fn encrypt_with(
        &self,
        crypto_buf: &Arc<Mutex<CryptoBuf>>,
        packets: Vec<Bytes>,
        force_inline: bool,
    ) -> Vec<Bytes> {
        if packets.is_empty() {
            return packets;
        }
        let has_aead = self.crypt.is_aead();
        let total_bytes: usize = packets.iter().map(|p| p.len()).sum();
        let use_cpu_block = !force_inline
            && should_cpu_block_encrypt(
                self.has_encryption,
                has_aead,
                packets.len(),
                total_bytes,
                self.crypt.as_ref(),
                self.offload_profile,
            );
        let allow_parallel = !use_cpu_block;
        if use_cpu_block {
            // Heavy cipher + large batch: offload to the blocking pool so the
            // reactor can keep draining UDP / processing ACKs (matches the
            // legacy binary flush path; M0.2 — was previously done inline).
            let crypt = self.crypt.clone();
            let cb = crypto_buf.clone();
            let has_encryption = self.has_encryption;
            knet::cpu_block(move || {
                encrypt_batch(packets, crypt.as_ref(), &cb, has_encryption, allow_parallel)
            })
            .await
        } else {
            let mut out = Vec::with_capacity(packets.len());
            encrypt_batch_into(
                packets,
                self.crypt.as_ref(),
                crypto_buf,
                self.has_encryption,
                allow_parallel,
                &mut out,
            );
            out
        }
    }

    /// Decrypt one inbound datagram in-place; returns plaintext length in `buf`.
    ///
    /// Failed decrypt returns `Ok(0)` so the input loop can skip (matches client
    /// which drains next datagram rather than killing the session on CRC fail).
    fn decrypt_in_place(&self, buf: &mut [u8], n: usize) -> io::Result<usize> {
        if n == 0 {
            return Ok(0);
        }
        if self.crypt.is_aead() {
            let aead = self.crypt.as_aead().expect("is_aead");
            match aead.open_in_place(buf, n) {
                Ok(len) => Ok(len),
                Err(_) => {
                    kcp_rs::snmp_add(&kcp_rs::DEFAULT_SNMP.in_csum_errors, 1);
                    Ok(0)
                }
            }
        } else if self.has_encryption {
            match decrypt_cfb_in_place(&mut buf[..n], self.crypt.as_ref(), false) {
                Ok(body) => {
                    let len = body.len();
                    // body is a subslice of buf; shift to front if needed.
                    let off = n - len;
                    if off > 0 {
                        buf.copy_within(off..n, 0);
                    }
                    Ok(len)
                }
                Err(_) => {
                    kcp_rs::snmp_add(&kcp_rs::DEFAULT_SNMP.in_csum_errors, 1);
                    Ok(0)
                }
            }
        } else {
            // null: identity
            let _ = inbound_null(&buf[..n]);
            Ok(n)
        }
    }
}

#[async_trait::async_trait]
impl PacketTransport for CryptoTransport {
    async fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        // Skip bad packets until one decrypts or the socket errors.
        loop {
            let n = self.inner.recv(buf).await?;
            let plain = self.decrypt_in_place(buf, n)?;
            if plain > 0 || n == 0 {
                return Ok(plain);
            }
            // CRC/AEAD fail → try next datagram (non-blocking drain first).
            match self.inner.try_recv(buf) {
                Ok(m) => {
                    let plain = self.decrypt_in_place(buf, m)?;
                    if plain > 0 || m == 0 {
                        return Ok(plain);
                    }
                    continue;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    // Fall through to another blocking recv.
                    continue;
                }
                Err(e) => return Err(e),
            }
        }
    }

    fn try_recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            let n = self.inner.try_recv(buf)?;
            let plain = self.decrypt_in_place(buf, n)?;
            if plain > 0 || n == 0 {
                return Ok(plain);
            }
            // bad packet — try another if ready
        }
    }

    fn decrypt_packet_in_place(&self, buf: &mut [u8], n: usize) -> usize {
        self.decrypt_in_place(buf, n).unwrap_or_default()
    }

    fn try_send_batch(&self, packets: &[Bytes]) -> io::Result<usize> {
        if packets.is_empty() {
            return Ok(0);
        }
        // Encrypt into the reusable `data_tx_buf` and send directly from it —
        // no intermediate `Vec<Bytes>` allocation. The `is_sending` CAS in
        // `drain_and_flush_tx` ensures single-owner access.
        let mut buf = self.data_tx_buf.lock();
        encrypt_batch_ref_into(
            packets,
            self.crypt.as_ref(),
            &self.data_crypto_buf,
            self.has_encryption,
            false,
            &mut buf,
        );
        self.inner.try_send_batch(&buf)
    }

    fn try_send_batch_to(&self, packets: &[Bytes], target: SocketAddr) -> io::Result<usize> {
        if packets.is_empty() {
            return Ok(0);
        }
        let mut buf = self.data_tx_buf.lock();
        encrypt_batch_ref_into(
            packets,
            self.crypt.as_ref(),
            &self.data_crypto_buf,
            self.has_encryption,
            false,
            &mut buf,
        );
        self.inner.try_send_batch_to(&buf, target)
    }

    async fn recv_vec(&self, buf: &mut Vec<u8>) -> io::Result<usize> {
        loop {
            let n = self.inner.recv_vec(buf).await?;
            let plain = self.decrypt_in_place(buf, n)?;
            buf.truncate(plain);
            if plain > 0 || n == 0 {
                return Ok(plain);
            }
        }
    }

    fn try_recv_vec(&self, buf: &mut Vec<u8>) -> io::Result<usize> {
        loop {
            let n = self.inner.try_recv_vec(buf)?;
            let plain = self.decrypt_in_place(buf, n)?;
            buf.truncate(plain);
            if plain > 0 || n == 0 {
                return Ok(plain);
            }
        }
    }

    fn try_recv_batch(&self, pool: &mut [Vec<u8>]) -> io::Result<usize> {
        let n = self.inner.try_recv_batch(pool)?;
        // Decrypt each slot in place and **stably compact** bad packets (CRC /
        // AEAD failures) out, preserving the relative order of good packets.
        // Bad slots keep their capacity: the swap below recycles them instead
        // of dropping the allocation, so the buffer pool does not churn.
        let mut write = 0;
        for read in 0..n {
            let len = pool[read].len();
            let plain = self.decrypt_in_place(&mut pool[read], len)?;
            pool[read].truncate(plain);
            if plain > 0 {
                pool.swap(write, read);
                write += 1;
            }
        }
        Ok(write)
    }

    fn supports_recv_batch(&self) -> bool {
        self.inner.supports_recv_batch()
    }

    async fn send_batch(&self, packets: &[Bytes]) -> io::Result<()> {
        if packets.is_empty() {
            return Ok(());
        }
        let encrypted = self.encrypt_data(packets.to_vec()).await;
        self.wait_for_rate(&encrypted).await;
        let r = self.inner.send_batch(&encrypted).await;
        r
    }

    async fn send_batch_to(&self, packets: &[Bytes], target: SocketAddr) -> io::Result<()> {
        if packets.is_empty() {
            return Ok(());
        }
        let encrypted = self.encrypt_data(packets.to_vec()).await;
        self.wait_for_rate(&encrypted).await;
        self.inner.send_batch_to(&encrypted, target).await
    }

    async fn send_urgent(&self, packets: &[Bytes]) -> io::Result<()> {
        if packets.is_empty() {
            return Ok(());
        }
        let encrypted = self.encrypt_urgent(packets.to_vec()).await;
        self.wait_for_rate(&encrypted).await;
        self.inner.send_urgent(&encrypted).await
    }

    async fn send_urgent_to(&self, packets: &[Bytes], target: SocketAddr) -> io::Result<()> {
        if packets.is_empty() {
            return Ok(());
        }
        let encrypted = self.encrypt_urgent(packets.to_vec()).await;
        self.wait_for_rate(&encrypted).await;
        self.inner.send_urgent_to(&encrypted, target).await
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }
}

/// Build the encrypted KCP layer over an existing datagram socket.
#[cfg(all(test, feature = "tokio"))]
pub(crate) async fn kcp_stream_with_socket(
    socket: Arc<knet::DatagramSocket>,
    remote: SocketAddr,
    key: &[u8],
    crypt: &str,
    config: KcpConfig,
    connected: bool,
    offload_profile: OffloadProfile,
) -> io::Result<KcpStream> {
    kcp_stream_with_socket_rate_limited(
        socket,
        remote,
        key,
        crypt,
        config,
        connected,
        offload_profile,
        0,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn kcp_stream_with_socket_rate_limited(
    socket: Arc<knet::DatagramSocket>,
    remote: SocketAddr,
    key: &[u8],
    crypt: &str,
    config: KcpConfig,
    connected: bool,
    offload_profile: OffloadProfile,
    rate_limit: u32,
) -> io::Result<KcpStream> {
    kcp_stream_with_socket_options(
        socket,
        remote,
        key,
        crypt,
        config,
        connected,
        false,
        offload_profile,
        rate_limit,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn server_kcp_stream_with_socket_rate_limited(
    socket: Arc<knet::DatagramSocket>,
    peer: SocketAddr,
    key: &[u8],
    crypt: &str,
    config: KcpConfig,
    offload_profile: OffloadProfile,
    rate_limit: u32,
) -> io::Result<KcpStream> {
    kcp_stream_with_socket_options(
        socket,
        peer,
        key,
        crypt,
        config,
        true,
        true,
        offload_profile,
        rate_limit,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn kcp_stream_with_socket_options(
    socket: Arc<knet::DatagramSocket>,
    remote: SocketAddr,
    key: &[u8],
    crypt: &str,
    config: KcpConfig,
    connected: bool,
    adopt_conv: bool,
    offload_profile: OffloadProfile,
    rate_limit: u32,
) -> io::Result<KcpStream> {
    let mut ct = CryptoTransport::new(socket, key, crypt);
    ct.set_offload_profile(offload_profile);
    ct.set_rate_limit(rate_limit);
    let transport: Arc<dyn PacketTransport> = Arc::new(ct);
    let mut builder = KcpStream::with_transport(transport, remote)
        .connected(connected)
        .adopt_conv(adopt_conv)
        .config(config.clone());
    if config.datashard > 0 && config.parityshard > 0 {
        builder = builder.fec(config.datashard, config.parityshard);
    }
    builder.build().await
}

#[cfg(all(test, feature = "tokio"))]
async fn test_kcp_stream(
    socket: Arc<knet::DatagramSocket>,
    remote: SocketAddr,
    key: &[u8],
    crypt: &str,
    params: &crate::KcpCliParams,
) -> io::Result<KcpStream> {
    let config = params.to_kcp_config();
    // Client sockets are typically `connect()`ed to the remote.
    kcp_stream_with_socket(
        socket,
        remote,
        key,
        crypt,
        config,
        true,
        OffloadProfile::Tokio,
    )
    .await
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(all(test, feature = "tokio"))]
mod tests {
    use super::*;
    use std::time::Duration;

    #[cfg(feature = "tokio")]
    #[tokio::test]
    async fn null_encrypt_passthrough() {
        // Build a dummy socket we won't actually send on — only exercise encrypt helpers.
        let sock = knet::UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        let inner = Arc::new(knet::DatagramSocket::Udp(sock));
        let ct = CryptoTransport::new(inner, b"unused-key-pad-to-32-bytes!!!!!", "null");
        assert!(!ct.has_encryption());
        let plain = vec![
            Bytes::from_static(b"hello-null"),
            Bytes::from_static(b"world"),
        ];
        let out = ct.encrypt_data(plain.clone()).await;
        assert_eq!(out.len(), 2);
        assert_eq!(&out[0][..], b"hello-null");
        assert_eq!(&out[1][..], b"world");
    }

    #[cfg(feature = "tokio")]
    #[tokio::test]
    async fn aes_cfb_roundtrip_via_crypto_bufs() {
        let sock = knet::UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        let inner = Arc::new(knet::DatagramSocket::Udp(sock));
        let key = b"0123456789abcdef0123456789abcdef";
        let ct = CryptoTransport::new(inner, key, "aes");
        assert!(ct.has_encryption());
        assert!(!ct.crypt.is_aead());

        let plain = b"kcp-segment-payload-for-cfb-test!!";
        let encrypted = ct.encrypt_data(vec![Bytes::copy_from_slice(plain)]).await;
        assert_eq!(encrypted.len(), 1);
        assert!(encrypted[0].len() > plain.len()); // 20B header

        let mut buf = encrypted[0].to_vec();
        let nlen = buf.len();
        let n = ct.decrypt_in_place(&mut buf, nlen).unwrap();
        assert_eq!(n, plain.len());
        assert_eq!(&buf[..n], plain);
    }

    #[cfg(feature = "tokio")]
    #[tokio::test]
    async fn ack_and_data_bufs_are_independent() {
        let sock = knet::UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        let inner = Arc::new(knet::DatagramSocket::Udp(sock));
        let key = b"0123456789abcdef0123456789abcdef";
        let ct = CryptoTransport::new(inner, key, "aes");

        let data = ct.encrypt_data(vec![Bytes::from_static(b"data-pkt")]).await;
        let ack = ct
            .encrypt_urgent(vec![Bytes::from_static(b"ack-pkt!")])
            .await;
        // Different session_id seeds → different nonces in first 8 bytes after
        // counter (bytes 8..16 hold session_id). Counters both start at 0 so
        // bytes 0..8 may match; session half must differ.
        assert_ne!(&data[0][8..16], &ack[0][8..16]);
    }

    #[cfg(feature = "tokio")]
    #[tokio::test]
    async fn aead_roundtrip() {
        let sock = knet::UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        let inner = Arc::new(knet::DatagramSocket::Udp(sock));
        let key = b"0123456789abcdef0123456789abcdef";
        let ct = CryptoTransport::new(inner, key, "aes-128-gcm");
        assert!(ct.has_encryption());
        assert!(ct.crypt.is_aead());

        let plain = b"aead-payload-test";
        let encrypted = ct.encrypt_data(vec![Bytes::copy_from_slice(plain)]).await;
        let mut buf = encrypted[0].to_vec();
        // Need room: open copies plaintext back into buf
        let nlen = buf.len();
        let n = ct.decrypt_in_place(&mut buf, nlen).unwrap();
        assert_eq!(n, plain.len());
        assert_eq!(&buf[..n], plain);
    }

    #[cfg(feature = "tokio")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn kcp_stream_null_localhost_roundtrip() {
        use knet::AsyncWriteExt;

        let key = b"0123456789abcdef0123456789abcdef";
        // Bind two ports, connect both sides via kcp_stream_with_socket.
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

        let cfg = KcpConfig {
            conv: 0xC0FFEE,
            mode: kcp_rs::KcpMode::Fast3,
            ..KcpConfig::default()
        };

        let mut conn_a = kcp_stream_with_socket(
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
        let mut conn_b = kcp_stream_with_socket(
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

        let payload = b"session-null-roundtrip-payload!!";
        conn_a.write_all(payload).await.unwrap();
        conn_a.flush().await.unwrap();

        let mut got = vec![0u8; payload.len()];
        read_exact(&mut conn_b, &mut got, Duration::from_secs(5)).await;
        assert_eq!(&got[..], payload);

        conn_a.close();
        conn_b.close();
    }

    #[cfg(feature = "tokio")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn kcp_stream_aes_localhost_roundtrip() {
        use knet::AsyncWriteExt;

        let key = b"0123456789abcdef0123456789abcdef";
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

        let cfg = KcpConfig {
            conv: 0xAE5AE5,
            datashard: 10,
            parityshard: 3,
            ..KcpConfig::default()
        };

        let mut conn_a = kcp_stream_with_socket(
            sock_a,
            addr_b,
            key,
            "aes",
            cfg.clone(),
            true,
            OffloadProfile::Tokio,
        )
        .await
        .unwrap();
        let mut conn_b =
            kcp_stream_with_socket(sock_b, addr_a, key, "aes", cfg, true, OffloadProfile::Tokio)
                .await
                .unwrap();

        let mut payload = vec![0u8; 8 * 1024];
        for (i, b) in payload.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        conn_a.write_all(&payload).await.unwrap();
        conn_a.flush().await.unwrap();

        let mut got = vec![0u8; payload.len()];
        read_exact(&mut conn_b, &mut got, Duration::from_secs(5)).await;
        assert_eq!(got, payload);

        conn_a.close();
        conn_b.close();
    }

    /// Client-shaped dial helper roundtrip (null crypt, CLI params).
    #[cfg(feature = "tokio")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn client_shaped_kcp_stream_roundtrip() {
        use knet::AsyncWriteExt;

        let key = b"0123456789abcdef0123456789abcdef";
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

        // Client-like defaults: fast3, FEC 10/3, acknodelay, rcvwnd 512.
        let params = crate::KcpCliParams {
            mode: "fast3".into(),
            mtu: 1350,
            sndwnd: 128,
            rcvwnd: 512,
            datashard: 10,
            parityshard: 3,
            acknodelay: true,
            conv: 0xD1A1_C11E,
            ..crate::KcpCliParams::default()
        };

        let mut client = test_kcp_stream(sock_a, addr_b, key, "null", &params)
            .await
            .unwrap();
        // The connected peer uses the same lower-layer dial helper.
        let mut server = test_kcp_stream(sock_b, addr_a, key, "null", &params)
            .await
            .unwrap();

        let payload = b"client-shaped-dial-helper-payload!";
        client.write_all(payload).await.unwrap();
        client.flush().await.unwrap();

        let mut got = vec![0u8; payload.len()];
        read_exact(&mut server, &mut got, Duration::from_secs(5)).await;
        assert_eq!(&got[..], payload);

        // reverse
        let reply = b"server-peer-reply-ok";
        server.write_all(reply).await.unwrap();
        server.flush().await.unwrap();
        let mut got2 = vec![0u8; reply.len()];
        read_exact(&mut client, &mut got2, Duration::from_secs(5)).await;
        assert_eq!(&got2[..], reply);

        client.close();
        server.close();
    }

    /// AES + dial/accept helpers with CLI params (manual mode knobs stored).
    #[cfg(feature = "tokio")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dial_accept_aes_with_cli_params() {
        use knet::AsyncWriteExt;

        let key = b"0123456789abcdef0123456789abcdef";
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

        let params = crate::KcpCliParams {
            mode: "fast".into(),
            mtu: 1350,
            sndwnd: 64,
            rcvwnd: 64,
            datashard: 0,
            parityshard: 0,
            acknodelay: false,
            conv: 0xAE5_C11,
            ..crate::KcpCliParams::default()
        };

        // Sanity: config helper produces expected mode.
        let cfg = params.to_kcp_config();
        assert_eq!(cfg.mode, kcp_rs::KcpMode::Fast);
        assert_eq!(cfg.datashard, 0);

        let mut a = test_kcp_stream(sock_a, addr_b, key, "aes", &params)
            .await
            .unwrap();
        let mut b = test_kcp_stream(sock_b, addr_a, key, "aes", &params)
            .await
            .unwrap();

        let mut payload = vec![0u8; 4096];
        for (i, byte) in payload.iter_mut().enumerate() {
            *byte = (i % 251) as u8;
        }
        a.write_all(&payload).await.unwrap();
        a.flush().await.unwrap();
        let mut got = vec![0u8; payload.len()];
        read_exact(&mut b, &mut got, Duration::from_secs(5)).await;
        assert_eq!(got, payload);

        a.close();
        b.close();
    }

    /// M0.1 — the M1-A SMUX→KcpStream write path must sense backpressure:
    /// when the peer never ACKs, `wait_send` stays ≤ `snd_wnd` and repeated
    /// writes stall (Pending) instead of buffering unboundedly in `snd_queue`.
    #[cfg(feature = "tokio")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn kcp_stream_write_backpressure_bounds_inflight() {
        // Bind-then-drop a port so nothing listens on it: the peer never ACKs.
        let tmp = knet::UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        let dead = tmp.local_addr().unwrap();
        drop(tmp);

        let sock = Arc::new(knet::DatagramSocket::Udp(
            knet::UdpSocket::connect(SocketAddr::from(([127, 0, 0, 1], 0)), dead).unwrap(),
        ));
        let key = b"0123456789abcdef0123456789abcdef";
        let cfg = KcpConfig {
            conv: 0xBA0C_B00D,
            mode: kcp_rs::KcpMode::Fast3,
            sndwnd: 4,
            rcvwnd: 64,
            ..KcpConfig::default()
        };
        let conn = kcp_stream_with_socket(
            sock,
            dead,
            key,
            "null",
            cfg.clone(),
            true,
            OffloadProfile::Tokio,
        )
        .await
        .unwrap();

        let chunk = vec![0xEEu8; 4096];
        let mut accepted = 0usize;
        let mut stalled = false;
        for _ in 0..100 {
            match knet::timeout(Duration::from_millis(200), conn.write_all(&chunk)).await {
                Ok(Ok(())) => accepted += chunk.len(),
                Ok(Err(e)) => panic!("write error: {}", e),
                Err(_) => {
                    stalled = true;
                    break;
                }
            }
            // `wait_send` = snd_buf + snd_queue (Go semantics). Backpressure
            // gates on it, so it must stay bounded — a small multiple of the
            // window, never the full 100 chunks.
            assert!(
                conn.wait_send() <= (cfg.sndwnd.saturating_mul(4)) as usize,
                "wait_send {} grew unbounded (snd_wnd {})",
                conn.wait_send(),
                cfg.sndwnd
            );
        }

        assert!(
            stalled,
            "writer never stalled; accepted {accepted} bytes to a dead peer"
        );
        // The writer must stall before buffering everything: some chunks were
        // blocked by backpressure. (`wait_send` staying ≤ snd_wnd*4 above is
        // the hard bound on in-flight; write_buf may overshoot by one flush
        // cycle because the gate reads a flush-loop-cached snapshot — see
        // plan M0.1 note. M1-A bounds per-call writes, so this is bounded.)
        assert!(
            accepted < 100 * chunk.len(),
            "accepted all {accepted} bytes without stalling — no backpressure"
        );

        conn.close();
    }

    #[cfg(feature = "tokio")]
    async fn read_exact(conn: &mut KcpStream, buf: &mut [u8], limit: Duration) {
        use knet::AsyncReadExt;
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

    // ── Batch receive: bad-ciphertext compaction (v3 §5.3) ──

    /// In-memory `PacketTransport` queueing caller-supplied packets so
    /// `try_recv_batch` can be exercised deterministically.
    #[cfg(feature = "tokio")]
    struct MockInner {
        packets: std::sync::Mutex<std::collections::VecDeque<Vec<u8>>>,
    }

    #[cfg(feature = "tokio")]
    #[async_trait::async_trait]
    impl PacketTransport for MockInner {
        async fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
            self.try_recv(buf)
        }
        fn try_recv(&self, buf: &mut [u8]) -> io::Result<usize> {
            let mut q = self.packets.lock().unwrap();
            match q.pop_front() {
                Some(p) => {
                    let n = p.len().min(buf.len());
                    buf[..n].copy_from_slice(&p[..n]);
                    Ok(n)
                }
                None => Err(io::Error::from(io::ErrorKind::WouldBlock)),
            }
        }
        async fn send_batch(&self, _packets: &[Bytes]) -> io::Result<()> {
            Ok(())
        }
        async fn send_batch_to(&self, _packets: &[Bytes], _target: SocketAddr) -> io::Result<()> {
            Ok(())
        }
        fn try_recv_batch(&self, pool: &mut [Vec<u8>]) -> io::Result<usize> {
            let mut q = self.packets.lock().unwrap();
            let mut n = 0;
            while n < pool.len() {
                match q.pop_front() {
                    Some(p) => {
                        pool[n].clear();
                        pool[n].extend_from_slice(&p);
                        n += 1;
                    }
                    None => break,
                }
            }
            if n == 0 {
                Err(io::Error::from(io::ErrorKind::WouldBlock))
            } else {
                Ok(n)
            }
        }
        fn supports_recv_batch(&self) -> bool {
            true
        }
        fn local_addr(&self) -> io::Result<SocketAddr> {
            Ok(SocketAddr::from(([127, 0, 0, 1], 0)))
        }
    }

    #[tokio::test]
    async fn rate_limit_counts_encrypted_wire_bytes() {
        let key = b"0123456789abcdef0123456789abcdef";
        let inner: Arc<dyn PacketTransport> = Arc::new(MockInner {
            packets: std::sync::Mutex::new(std::collections::VecDeque::new()),
        });
        let mut transport = CryptoTransport::with_transport(inner, key, "aes");
        transport.set_rate_limit(1_000);

        // CFB adds a 20-byte wire header. Consume 95,990 of the fixed 96,000
        // burst, then verify the next 100-byte plaintext packet is paced as
        // 120 on-wire bytes (110-byte deficit => about 110 ms at 1 KB/s).
        transport
            .send_batch(&[Bytes::from(vec![0u8; 95_970])])
            .await
            .unwrap();
        let started = std::time::Instant::now();
        transport
            .send_batch(&[Bytes::from(vec![0u8; 100])])
            .await
            .unwrap();
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_millis(90),
            "encrypted wire bytes were not rate-limited: {elapsed:?}"
        );
        assert!(elapsed < Duration::from_secs(1));
    }

    #[cfg(feature = "tokio")]
    #[tokio::test]
    async fn try_recv_batch_compacts_bad_packets() {
        let key = b"0123456789abcdef0123456789abcdef";
        let mock = Arc::new(MockInner {
            packets: std::sync::Mutex::new(std::collections::VecDeque::new()),
        });
        let inner: Arc<dyn PacketTransport> = mock.clone();
        let ct = CryptoTransport::with_transport(inner, key, "aes");

        let g0 = ct.encrypt_data(vec![Bytes::from_static(b"good-0")]).await;
        let g1 = ct.encrypt_data(vec![Bytes::from_static(b"good-1")]).await;
        let g2 = ct.encrypt_data(vec![Bytes::from_static(b"good-2")]).await;

        // Corrupt the middle packet's ciphertext so its CRC check fails.
        let mut bad = g1[0].to_vec();
        let mid = bad.len() / 2;
        bad[mid] ^= 0xFF;

        {
            let mut q = mock.packets.lock().unwrap();
            q.push_back(g0[0].to_vec());
            q.push_back(bad);
            q.push_back(g2[0].to_vec());
        }

        let mut pool: Vec<Vec<u8>> = (0..4).map(|_| vec![0u8; 2048]).collect();
        let n = ct.try_recv_batch(&mut pool).unwrap();
        // Bad packet compacted out; good packets keep relative order.
        assert_eq!(n, 2);
        assert_eq!(&pool[0][..], b"good-0");
        assert_eq!(&pool[1][..], b"good-2");
    }
}
