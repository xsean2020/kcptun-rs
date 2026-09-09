//! End-to-end data-integrity tests for the async [`KcpStream`] (`feature = "async"`).
//!
//! Run on their own with either runtime backend:
//!
//! ```text
//! cargo test -p kcp-rs --features async --test kcpstream_integrity
//! cargo test -p kcp-rs --features async  --test kcpstream_integrity
//! ```
//!
//! Two real [`KcpStream`]s over localhost UDP transfer large deterministic
//! payloads (with and without Reed-Solomon FEC); the received bytes must be
//! byte-for-byte identical to what was written.

#![cfg(feature = "async")]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use kcp_rs::{snmp_enable, KcpMode, KcpStream, PacketTransport, DEFAULT_SNMP};
use knet::{AsyncReadExt, AsyncWriteExt};

/// Connected transport wrapper that silently drops its first non-empty send.
/// This exercises KcpStream's background retransmission deadline rather than a
/// socket error/retry path.
struct DropFirstSend {
    inner: Arc<dyn PacketTransport>,
    dropped: AtomicBool,
}

#[async_trait::async_trait]
impl PacketTransport for DropFirstSend {
    async fn recv(&self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.inner.recv(buf).await
    }

    fn try_recv(&self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.inner.try_recv(buf)
    }

    async fn send_batch(&self, packets: &[Bytes]) -> std::io::Result<()> {
        if !packets.is_empty() && !self.dropped.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        self.inner.send_batch(packets).await
    }

    async fn send_batch_to(&self, packets: &[Bytes], target: SocketAddr) -> std::io::Result<()> {
        if !packets.is_empty() && !self.dropped.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        self.inner.send_batch_to(packets, target).await
    }

    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.inner.local_addr()
    }
}

/// Deterministic, non-trivial payload pattern.
fn make_payload(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| ((i * 131 + seed as usize) % 251) as u8)
        .collect()
}

/// FNV-1a 64 — independent checksum cross-check on top of byte equality.
fn fnv1a(data: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for &b in data {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01B3);
    }
    h
}

/// Build a connected pair of `KcpStream` over localhost UDP, optionally with FEC.
async fn pair_conns(fec: Option<(u32, u32)>) -> (KcpStream, KcpStream) {
    let a_tmp = knet::UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
    let b_tmp = knet::UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
    let addr_a = a_tmp.local_addr().unwrap();
    let addr_b = b_tmp.local_addr().unwrap();
    drop(a_tmp);
    drop(b_tmp);

    let sock_a = knet::UdpSocket::connect(addr_a, addr_b).unwrap();
    let sock_b = knet::UdpSocket::connect(addr_b, addr_a).unwrap();

    let mut ba = KcpStream::with_transport(
        Arc::new(knet::DatagramSocket::Udp(sock_a)) as Arc<dyn PacketTransport>,
        addr_b,
    )
    .connected(true)
    .conv(0x00C0_FFEE)
    .mode(KcpMode::Fast3)
    .mtu(1350)
    .sndwnd(512)
    .rcvwnd(512);
    let mut bb = KcpStream::with_transport(
        Arc::new(knet::DatagramSocket::Udp(sock_b)) as Arc<dyn PacketTransport>,
        addr_a,
    )
    .connected(true)
    .conv(0x00C0_FFEE)
    .mode(KcpMode::Fast3)
    .mtu(1350)
    .sndwnd(512)
    .rcvwnd(512);
    if let Some((d, p)) = fec {
        ba = ba.fec(d, p);
        bb = bb.fec(d, p);
    }
    let conn_a = ba.build().await.unwrap();
    let conn_b = bb.build().await.unwrap();
    (conn_a, conn_b)
}

/// Write `payload` end-to-end and verify the peer reads it back intact.
async fn roundtrip(from: &mut KcpStream, to: &mut KcpStream, payload: &[u8]) {
    from.write_all(payload).await.unwrap();
    from.flush().await.unwrap();
    let mut got = vec![0u8; payload.len()];
    read_exact_timeout(to, &mut got, Duration::from_secs(30)).await;
    assert_eq!(got, payload, "byte-for-byte mismatch");
    assert_eq!(fnv1a(&got), fnv1a(payload), "checksum mismatch");
}

/// Read exactly `buf.len()` bytes, polling with a timeout.
async fn read_exact_timeout<R: knet::AsyncRead + Unpin>(
    conn: &mut R,
    buf: &mut [u8],
    limit: Duration,
) {
    let deadline = std::time::Instant::now() + limit;
    let mut filled = 0usize;
    while filled < buf.len() {
        if std::time::Instant::now() > deadline {
            panic!("timeout waiting for data, got {}/{}", filled, buf.len());
        }
        match knet::timeout(Duration::from_millis(200), conn.read(&mut buf[filled..])).await {
            Ok(Ok(0)) => panic!("unexpected EOF at {}", filled),
            Ok(Ok(n)) => filled += n,
            Ok(Err(e)) => panic!("read error: {}", e),
            Err(_) => continue,
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

/// Bidirectional integrity over localhost UDP (no FEC).
#[test]
fn kcpstream_bidirectional_integrity() {
    knet::block_on(async {
        let (mut conn_a, mut conn_b) = pair_conns(None).await;
        let payload = make_payload(256 * 1024, 11);
        roundtrip(&mut conn_a, &mut conn_b, &payload).await;
        let payload2 = make_payload(512 * 1024, 77);
        roundtrip(&mut conn_b, &mut conn_a, &payload2).await;
        conn_a.close();
        conn_b.close();
    });
}

/// Integrity with Reed-Solomon FEC 10/3 (Go-compatible defaults).
#[test]
fn kcpstream_fec_integrity() {
    knet::block_on(async {
        let (mut conn_a, mut conn_b) = pair_conns(Some((10, 3))).await;
        let payload = make_payload(512 * 1024, 23);
        roundtrip(&mut conn_a, &mut conn_b, &payload).await;
        conn_a.close();
        conn_b.close();
    });
}

/// `into_split` yields two owned halves that both drive the same connection;
/// data written on one half is read on the other.
#[test]
fn kcpstream_into_split_echo() {
    knet::block_on(async {
        let (conn_a, conn_b) = pair_conns(None).await;
        let (mut rh_a, mut wh_a) = conn_a.into_split();
        let (mut rh_b, mut wh_b) = conn_b.into_split();

        let payload = make_payload(64 * 1024, 5);
        wh_a.write_all(&payload).await.unwrap();
        wh_a.flush().await.unwrap();

        let mut got = vec![0u8; payload.len()];
        read_exact_timeout(&mut rh_b, &mut got, Duration::from_secs(10)).await;
        assert_eq!(got, payload);

        // Second direction for good measure.
        wh_b.write_all(&payload).await.unwrap();
        wh_b.flush().await.unwrap();
        let mut back = vec![0u8; payload.len()];
        read_exact_timeout(&mut rh_a, &mut back, Duration::from_secs(10)).await;
        assert_eq!(back, payload);

        drop(wh_a);
        drop(wh_b);
    });
}

/// `shutdown(Write)` rejects subsequent writes but still delivers data written
/// before the shutdown (write-half close).
#[test]
fn kcpstream_shutdown_write_rejects_writes_after_flush() {
    knet::block_on(async {
        let (conn_a, mut conn_b) = pair_conns(None).await;
        let payload = make_payload(64 * 1024, 9);
        conn_a.write_all(&payload).await.unwrap();
        conn_a.shutdown(std::net::Shutdown::Write).unwrap();

        // Writes after shutdown(Write) fail with BrokenPipe.
        let err = conn_a.write_all(&b"x"[..]).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);

        // Data written before shutdown still arrives intact.
        let mut got = vec![0u8; payload.len()];
        read_exact_timeout(&mut conn_b, &mut got, Duration::from_secs(10)).await;
        assert_eq!(got, payload);

        conn_a.close();
        conn_b.close();
    });
}

/// A configured read timeout surfaces as `TimedOut` on a silent peer.
#[test]
fn kcpstream_read_timeout() {
    knet::block_on(async {
        let (conn_a, _conn_b) = pair_conns(None).await;
        conn_a
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        assert_eq!(
            conn_a.read_timeout().unwrap(),
            Some(Duration::from_millis(100))
        );
        let mut buf = [0u8; 16];
        let err = conn_a.read(&mut buf).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        conn_a.close();
    });
}

/// A no-timeout read waits on Notify rather than a periodic fallback timer.
/// The external timeout bounds this test; the internal fallback counter must
/// remain unchanged while the peer is silent.
#[test]
fn kcpstream_no_timeout_read_has_no_fallback_tick() {
    knet::block_on(async {
        snmp_enable();
        let (conn_a, conn_b) = pair_conns(None).await;
        let before = DEFAULT_SNMP.read_fallback_timeout();
        let mut buf = [0u8; 8];
        let timed = knet::timeout(Duration::from_millis(30), conn_a.read(&mut buf)).await;
        assert!(timed.is_err(), "silent read should remain pending");
        assert_eq!(DEFAULT_SNMP.read_fallback_timeout(), before);
        conn_a.close();
        conn_b.close();
    });
}

/// One write call must not enqueue more KCP segments than the remaining send
/// window, even when the caller supplies a large buffer.
#[test]
fn kcpstream_single_write_respects_remaining_window() {
    knet::block_on(async {
        let (mut conn_a, conn_b) = pair_conns(None).await;
        conn_a.set_kcp_window_size(1, 512);
        let payload = vec![0x5Au8; 64 * 1024];
        let written = conn_a.write(&payload).await.unwrap();
        assert!(written > 0);
        assert!(
            conn_a.wait_send() <= conn_a.snd_wnd(),
            "one write overshot the configured send window: wait_send={} wnd={}",
            conn_a.wait_send(),
            conn_a.snd_wnd()
        );
        conn_a.close();
        conn_b.close();
    });
}

/// Repeated short reads exercise the single partial-message spill slot and
/// must preserve the byte stream exactly.
#[test]
fn kcpstream_short_reads_preserve_order() {
    knet::block_on(async {
        let (conn_a, conn_b) = pair_conns(None).await;
        let payload = make_payload(32 * 1024, 39);
        conn_a.write_all(&payload).await.unwrap();
        let mut received = Vec::with_capacity(payload.len());
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while received.len() < payload.len() {
            assert!(std::time::Instant::now() < deadline, "short-read timeout");
            let mut chunk = [0u8; 7];
            match knet::timeout(Duration::from_millis(200), conn_b.read(&mut chunk)).await {
                Ok(Ok(0)) => panic!("unexpected EOF after {} bytes", received.len()),
                Ok(Ok(n)) => received.extend_from_slice(&chunk[..n]),
                Ok(Err(e)) => panic!("short-read error: {e}"),
                Err(_) => continue,
            }
        }
        assert_eq!(received, payload);
        conn_a.close();
        conn_b.close();
    });
}

/// A successful inline UDP send still has to arm KCP maintenance: if that
/// datagram disappears, the parked flush task must wake at RTO and retransmit.
#[test]
fn kcpstream_idle_flush_retransmits_dropped_first_send() {
    knet::block_on(async {
        let a_tmp = knet::UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        let b_tmp = knet::UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        let addr_a = a_tmp.local_addr().unwrap();
        let addr_b = b_tmp.local_addr().unwrap();
        drop(a_tmp);
        drop(b_tmp);

        let socket_a: Arc<dyn PacketTransport> = Arc::new(knet::DatagramSocket::Udp(
            knet::UdpSocket::connect(addr_a, addr_b).unwrap(),
        ));
        let drop_first: Arc<dyn PacketTransport> = Arc::new(DropFirstSend {
            inner: socket_a,
            dropped: AtomicBool::new(false),
        });
        let socket_b: Arc<dyn PacketTransport> = Arc::new(knet::DatagramSocket::Udp(
            knet::UdpSocket::connect(addr_b, addr_a).unwrap(),
        ));

        let conn_a = KcpStream::with_transport(drop_first, addr_b)
            .connected(true)
            .conv(0x00C0_FFEE)
            .mode(KcpMode::Fast3)
            .build()
            .await
            .unwrap();
        let mut conn_b = KcpStream::with_transport(socket_b, addr_a)
            .connected(true)
            .conv(0x00C0_FFEE)
            .mode(KcpMode::Fast3)
            .build()
            .await
            .unwrap();

        // Let the initial protocol tick enter its one-second idle grace before
        // writing. A stale grace deadline used to postpone this dropped
        // packet's first RTO until nearly one second after connection start.
        knet::sleep_ms(400).await;
        let payload = make_payload(1024, 83);
        conn_a.write_all(&payload).await.unwrap();
        let mut received = vec![0u8; payload.len()];
        knet::timeout(Duration::from_millis(450), conn_b.read_exact(&mut received))
            .await
            .expect("idle-grace activity did not arm the first RTO")
            .expect("retransmitted read failed");
        assert_eq!(received, payload);
        conn_a.close();
        conn_b.close();
    });
}

/// Closing a connection wakes a writer blocked on a full send window instead
/// of leaving it pending until a timer tick.
#[test]
fn kcpstream_close_wakes_blocked_writer() {
    knet::block_on(async {
        // Keep a UDP endpoint bound so sends succeed, but never construct a
        // KCP peer to read/ACK them. The one-slot send window therefore
        // deterministically blocks after the first segment.
        let silent_peer = knet::UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        let conn_a = KcpStream::connect(silent_peer.local_addr().unwrap())
            .mode(KcpMode::Fast3)
            .sndwnd(1)
            .rcvwnd(1)
            .build()
            .await
            .unwrap();
        let closer = conn_a.clone();
        knet::spawn_task(async move {
            knet::sleep_ms(25).await;
            closer.close();
        });
        let payload = vec![0xA5u8; 256 * 1024];
        let result = knet::timeout(Duration::from_secs(2), conn_a.write_all(&payload)).await;
        let err = result
            .expect("close should wake blocked writer before test timeout")
            .expect_err("writer must observe close as BrokenPipe");
        assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
        drop(silent_peer);
    });
}

/// AsyncWrite close is a write-half shutdown and must wake a
/// clone which is blocked on the shared send window.
#[cfg(feature = "async")]
#[test]
fn kcpstream_smol_poll_close_wakes_blocked_writer() {
    knet::block_on(async {
        let silent_peer = knet::UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        let conn = KcpStream::connect(silent_peer.local_addr().unwrap())
            .mode(KcpMode::Fast3)
            .sndwnd(1)
            .rcvwnd(1)
            .build()
            .await
            .unwrap();
        let writer = conn.clone();
        let writer_task =
            knet::spawn_task(async move { writer.write_all(&vec![0x5Au8; 256 * 1024]).await });
        knet::sleep_ms(25).await;

        let mut closer = conn.clone();
        knet::AsyncWriteExt::shutdown(&mut closer).await.unwrap();
        let _ = knet::timeout(Duration::from_secs(2), writer_task)
            .await
            .expect("shutdown should wake a blocked writer")
            .expect("join should succeed")
            .expect_err("writer must observe write-half close as BrokenPipe");
        conn.close();
        drop(silent_peer);
    });
}

/// TcpStream-aligned surface: set_nodelay+getter / peek / take_error / readable
/// / writable.
#[test]
fn kcpstream_tcp_stream_surface() {
    knet::block_on(async {
        let (mut conn_a, mut conn_b) = pair_conns(None).await;

        // Fast3 default → nodelay(true); toggle + getter round-trips.
        assert!(conn_a.nodelay());
        conn_a.set_nodelay(false);
        assert!(!conn_a.nodelay());
        conn_a.set_nodelay(true);
        assert!(conn_a.nodelay());

        // No data yet → peek returns WouldBlock.
        let mut buf = [0u8; 8];
        let err = conn_b.peek(&mut buf).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::WouldBlock);

        // take_error is empty until a background-loop error is recorded.
        assert!(conn_a.take_error().unwrap().is_none());

        // writable() returns while the send window is open.
        conn_a.writable().await.unwrap();

        // Send "hello"; readable() + peek() see it without consuming.
        conn_a.write_all(&b"hello"[..]).await.unwrap();
        conn_a.flush().await.unwrap();
        conn_b.readable().await.unwrap();
        let n = conn_b.peek(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"hello");
        // peek must not have consumed it.
        let mut got = [0u8; 5];
        read_exact_timeout(&mut conn_b, &mut got, Duration::from_secs(5)).await;
        assert_eq!(&got, b"hello");

        conn_a.close();
        conn_b.close();
    });
}

/// Read shutdown discards buffered data and keeps subsequent inbound data
/// from becoming visible through the read half.
#[test]
fn kcpstream_shutdown_read_is_eof() {
    knet::block_on(async {
        let (mut conn_a, conn_b) = pair_conns(None).await;
        conn_a.write_all(b"buffered-before-shutdown").await.unwrap();
        conn_a.flush().await.unwrap();
        conn_b.readable().await.unwrap();

        conn_b.shutdown(std::net::Shutdown::Read).unwrap();
        let mut buf = [0u8; 32];
        assert_eq!(conn_b.peek(&mut buf).unwrap(), 0);
        assert_eq!(conn_b.read(&mut buf).await.unwrap(), 0);

        conn_a.write_all(b"after-shutdown").await.unwrap();
        conn_a.flush().await.unwrap();
        assert_eq!(conn_b.read(&mut buf).await.unwrap(), 0);

        conn_a.close();
        conn_b.close();
    });
}

/// Readiness helpers honor the configured read timeout instead of waiting on
/// the fixed lost-wake fallback forever.
#[test]
fn kcpstream_readable_timeout() {
    knet::block_on(async {
        let (conn_a, conn_b) = pair_conns(None).await;
        conn_a
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        let result = knet::timeout(Duration::from_secs(1), conn_a.readable())
            .await
            .expect("readable must honor its configured timeout");
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::TimedOut);
        conn_a.close();
        conn_b.close();
    });
}

/// A full send window produces a bounded write timeout even though the flush
/// loop continues to wake periodically while the peer is silent.
#[test]
fn kcpstream_write_shared_timeout() {
    knet::block_on(async {
        let probe = knet::UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        let dead = probe.local_addr().unwrap();
        drop(probe);

        let conn = KcpStream::connect(dead).sndwnd(1).build().await.unwrap();
        conn.set_write_timeout(Some(Duration::from_millis(80)))
            .unwrap();
        conn.write_all(&[1u8; 1200]).await.unwrap();
        let result = knet::timeout(Duration::from_secs(1), conn.write_all(&[2u8; 1200]))
            .await
            .expect("write_shared must not outlive its configured timeout");
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::TimedOut);
        conn.close();
    });
}

/// A zero builder window keeps the KCP default instead of making the shared
/// backpressure state permanently full.
#[test]
fn kcpstream_zero_sndwnd_uses_effective_window() {
    knet::block_on(async {
        let conn = KcpStream::connect("127.0.0.1:9")
            .sndwnd(0)
            .build()
            .await
            .unwrap();
        assert!(conn.snd_wnd() > 0);
        conn.close();
    });
}
