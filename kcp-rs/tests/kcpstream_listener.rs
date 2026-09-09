//! End-to-end tests for the server **listen** / client **connect** path:
//! [`KcpListener`] multi-peer accept + [`KcpStream::connect`] dial.
//!
//! Run on their own with either runtime backend:
//!
//! ```text
//! cargo test -p kcp-rs --features async --test kcpstream_listener
//! cargo test -p kcp-rs --features async  --test kcpstream_listener
//! ```
//!
//! A client dials a real listener over localhost UDP, the listener accepts a
//! per-peer `KcpStream`, and payloads must round-trip byte-for-byte.

#![cfg(feature = "async")]

use std::net::SocketAddr;
use std::time::Duration;

use kcp_rs::{KcpListener, KcpMode, KcpStream};
use knet::{AsyncReadExt, AsyncWriteExt};

const CONV: u32 = 0x00C0_FFEE;

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

/// Read exactly `buf.len()` bytes, polling with a timeout.
async fn read_exact_timeout(conn: &mut KcpStream, buf: &mut [u8], limit: Duration) {
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

/// Client dials the listener, server accepts, 256 KiB echoes back byte-exact.
#[test]
fn listener_accept_echo_roundtrip() {
    knet::block_on(async {
        let listener = KcpListener::bind("127.0.0.1:0")
            .conv(CONV)
            .mode(KcpMode::Fast3)
            .sndwnd(512)
            .rcvwnd(512)
            .build()
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();

        // Dial path: KcpStream::connect (fresh ephemeral UDP socket).
        let mut client = KcpStream::connect(addr)
            .conv(CONV)
            .mode(KcpMode::Fast3)
            .sndwnd(512)
            .rcvwnd(512)
            .build()
            .await
            .unwrap();

        // The client's first flush creates the server-side session.
        let payload = make_payload(256 * 1024, 11);
        client.write_all(&payload).await.unwrap();
        client.flush().await.unwrap();

        let (mut server, peer) = listener.accept().await.unwrap();
        assert_eq!(peer, client.local_addr().unwrap(), "accepted peer addr");

        // client → server
        let mut got = vec![0u8; payload.len()];
        read_exact_timeout(&mut server, &mut got, Duration::from_secs(30)).await;
        assert_eq!(got, payload, "server received wrong bytes");
        assert_eq!(fnv1a(&got), fnv1a(&payload), "server checksum mismatch");

        // server → client (echo)
        server.write_all(&got).await.unwrap();
        server.flush().await.unwrap();
        let mut back = vec![0u8; payload.len()];
        read_exact_timeout(&mut client, &mut back, Duration::from_secs(30)).await;
        assert_eq!(back, payload, "client received wrong bytes");
        assert_eq!(fnv1a(&back), fnv1a(&payload), "client checksum mismatch");

        drop(client);
        drop(server);
        listener.close();
    });
}

/// One listener, two clients: each accepted `KcpStream` sees exactly its own
/// peer's bytes (peer demux).
#[test]
fn listener_multiple_peers_demux() {
    knet::block_on(async {
        let listener = KcpListener::bind("127.0.0.1:0")
            .conv(CONV)
            .mode(KcpMode::Fast3)
            .sndwnd(512)
            .rcvwnd(512)
            .build()
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();

        let mut c1 = KcpStream::connect(addr)
            .conv(CONV)
            .mode(KcpMode::Fast3)
            .sndwnd(512)
            .rcvwnd(512)
            .build()
            .await
            .unwrap();
        let mut c2 = KcpStream::connect(addr)
            .conv(CONV)
            .mode(KcpMode::Fast3)
            .sndwnd(512)
            .rcvwnd(512)
            .build()
            .await
            .unwrap();

        let p1_data = make_payload(64 * 1024, 1);
        let p2_data = make_payload(64 * 1024, 2);
        c1.write_all(&p1_data).await.unwrap();
        c2.write_all(&p2_data).await.unwrap();
        c1.flush().await.unwrap();
        c2.flush().await.unwrap();

        let addr_c1 = c1.local_addr().unwrap();
        let addr_c2 = c2.local_addr().unwrap();
        let (mut s_a, p_a) = listener.accept().await.unwrap();
        let (mut s_b, p_b) = listener.accept().await.unwrap();

        // Each accepted conn is the peer that dialed it — data must not leak.
        let (expected_a, expected_b) = if p_a == addr_c1 {
            (p1_data.clone(), p2_data.clone())
        } else {
            assert_eq!(p_a, addr_c2, "unexpected peer address");
            (p2_data.clone(), p1_data.clone())
        };
        assert_eq!(p_b, if p_a == addr_c1 { addr_c2 } else { addr_c1 });

        let mut got_a = vec![0u8; expected_a.len()];
        let mut got_b = vec![0u8; expected_b.len()];
        read_exact_timeout(&mut s_a, &mut got_a, Duration::from_secs(30)).await;
        read_exact_timeout(&mut s_b, &mut got_b, Duration::from_secs(30)).await;
        assert_eq!(got_a, expected_a, "peer A received another peer's bytes");
        assert_eq!(got_b, expected_b, "peer B received another peer's bytes");

        drop(c1);
        drop(c2);
        drop(s_a);
        drop(s_b);
        listener.close();
    });
}

/// After a connection is fully closed on both sides, the listener keeps
/// accepting and serving fresh clients.
///
/// (A true "same socket re-dials" reconnect is not viable at the KCP layer:
/// the continuing client SN stream would not match the fresh server session's
/// `rcv_nxt = 0`. Real reconnects start a new client session with SN from 0.)
#[test]
fn listener_serves_new_client_after_previous_closed() {
    knet::block_on(async {
        let listener = KcpListener::bind("127.0.0.1:0")
            .conv(CONV)
            .mode(KcpMode::Fast3)
            .sndwnd(512)
            .rcvwnd(512)
            .build()
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();

        // Connection 1: fully served, then closed on both sides.
        let mut c1 = KcpStream::connect(addr)
            .conv(CONV)
            .mode(KcpMode::Fast3)
            .build()
            .await
            .unwrap();
        c1.write_all(b"one").await.unwrap();
        c1.flush().await.unwrap();
        let (mut s1, _p1) = listener.accept().await.unwrap();
        let mut g1 = vec![0u8; 3];
        read_exact_timeout(&mut s1, &mut g1, Duration::from_secs(10)).await;
        assert_eq!(&g1, b"one");
        drop(s1);
        drop(c1);

        // Connection 2: fresh client session (SN starts at 0) is served.
        let mut c2 = KcpStream::connect(addr)
            .conv(CONV)
            .mode(KcpMode::Fast3)
            .build()
            .await
            .unwrap();
        c2.write_all(b"two").await.unwrap();
        c2.flush().await.unwrap();
        let (mut s2, p2) = listener.accept().await.unwrap();
        assert_eq!(p2, c2.local_addr().unwrap());
        let mut g2 = vec![0u8; 3];
        read_exact_timeout(&mut s2, &mut g2, Duration::from_secs(10)).await;
        assert_eq!(&g2, b"two");

        drop(s2);
        drop(c2);
        listener.close();
    });
}

/// `connect_timeout` succeeds when a live, conv-compatible listener responds to
/// the forced `WASK` probe with `WINS` (first-packet reachability check).
#[test]
fn connect_timeout_live_listener_succeeds() {
    knet::block_on(async {
        let listener = KcpListener::bind("127.0.0.1:0")
            .conv(CONV)
            .mode(KcpMode::Fast3)
            .build()
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();

        let client = KcpStream::connect(addr)
            .conv(CONV)
            .mode(KcpMode::Fast3)
            .connect_timeout(Duration::from_secs(5))
            .build()
            .await
            .expect("live listener should answer the probe within the timeout");

        // The probe-triggered session should be accepted by the listener.
        let (_server, _peer) = listener.accept().await.unwrap();
        client.close();
        listener.close();
    });
}

/// `connect_timeout` fails with `TimedOut` (after roughly the full timeout)
/// when nothing responds — UDP has no RST-style fast failure.
#[test]
fn connect_timeout_dead_port_times_out() {
    knet::block_on(async {
        // Grab an ephemeral port then release it: nothing listens there.
        let probe = knet::UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        let dead = probe.local_addr().unwrap();
        drop(probe);

        let start = std::time::Instant::now();
        let err = match KcpStream::connect(dead)
            .conv(CONV)
            .connect_timeout(Duration::from_millis(300))
            .build()
            .await
        {
            Ok(_) => panic!("connect to a dead port should time out"),
            Err(e) => e,
        };
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        assert!(
            start.elapsed() >= Duration::from_millis(280),
            "should wait roughly the full timeout before failing"
        );
    });
}

/// `KcpListener::bind(addr).await` works without an explicit `.build()`.
#[test]
fn listener_bind_into_future() {
    knet::block_on(async {
        let listener = KcpListener::bind("127.0.0.1:0")
            .conv(CONV)
            .await
            .expect("bind via IntoFuture");
        assert!(listener.local_addr().unwrap().port() != 0);
        listener.close();
    });
}

/// `KcpStream::connect(addr).await` works without an explicit `.build()`.
#[test]
fn kcpstream_connect_into_future() {
    knet::block_on(async {
        let listener = KcpListener::bind("127.0.0.1:0").conv(CONV).await.unwrap();
        let addr = listener.local_addr().unwrap();

        let client = KcpStream::connect(addr).conv(CONV).await.unwrap();
        client.write_all(b"hi").await.unwrap();
        let (_server, _peer) = listener.accept().await.unwrap();
        client.close();
        listener.close();
    });
}

/// `accept_timeout` fails with `TimedOut` when no client connects in time.
#[test]
fn listener_accept_timeout() {
    knet::block_on(async {
        let listener = KcpListener::bind("127.0.0.1:0").conv(CONV).await.unwrap();
        let err = match listener.accept_timeout(Duration::from_millis(100)).await {
            Ok(_) => panic!("expected accept timeout"),
            Err(e) => e,
        };
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        listener.close();
    });
}

/// `try_accept` returns `None` when nothing is pending, then `Some` once a
/// client's first datagram registers a peer session (non-blocking poll).
#[test]
fn listener_try_accept() {
    knet::block_on(async {
        let listener = KcpListener::bind("127.0.0.1:0").conv(CONV).await.unwrap();
        let addr = listener.local_addr().unwrap();

        // Nothing connected yet → None.
        assert!(listener.try_accept().unwrap().is_none());

        // Dial + write → the demux reader registers a peer session.
        let client = KcpStream::connect(addr).conv(CONV).await.unwrap();
        client.write_all(b"hi").await.unwrap();

        // Poll until the accepted conn is pending (reader is async).
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Some((_server, peer)) = listener.try_accept().unwrap() {
                assert_eq!(peer, client.local_addr().unwrap());
                break;
            }
            if std::time::Instant::now() > deadline {
                panic!("timed out waiting for try_accept");
            }
            knet::sleep_ms(10).await;
        }
        client.close();
        listener.close();
    });
}

/// `take_error` starts empty.
#[test]
fn listener_take_error_initial_none() {
    knet::block_on(async {
        let listener = KcpListener::bind("127.0.0.1:0").conv(CONV).await.unwrap();
        assert!(listener.take_error().unwrap().is_none());
        listener.close();
    });
}
