//! Shared kcptun session above the encrypted KCP transport.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use bytes::{Bytes, BytesMut};
use kcrypt_rs::wire::OffloadProfile;
use parking_lot::Mutex;

use crate::RateLimiter;

/// Configuration for one complete kcptun session.
#[derive(Clone, Debug)]
pub struct KcptunConfig {
    pub kcp: kcp_rs::KcpConfig,
    pub smux: smux_rs::Config,
    pub nocomp: bool,
    pub rate_limit: u32,
    pub offload_profile: OffloadProfile,
}

/// Shared client/server session composition: KCP transport + Snappy + SMUX.
///
/// This type owns the common KCP, Snappy, and SMUX scheduling so transport
/// variants and binaries do not duplicate those loops.
pub struct KcptunSession {
    kcp: Arc<kcp_rs::KcpStream>,
    smux: Arc<smux_rs::Session>,
    flush_notify: Arc<knet::Notify>,
    dead: Arc<AtomicBool>,
    created_ms: u64,
    _handles: Vec<knet::JoinHandle<()>>,
}

impl KcptunSession {
    /// Build a client session over an existing UDP or raw-TCP datagram socket.
    pub async fn connect(
        socket: Arc<knet::DatagramSocket>,
        remote: std::net::SocketAddr,
        key: &[u8],
        crypt: &str,
        config: &KcptunConfig,
    ) -> Result<Self> {
        let kcp = crate::kcp_transport::kcp_stream_with_socket_rate_limited(
            socket,
            remote,
            key,
            crypt,
            config.kcp.clone(),
            true,
            config.offload_profile,
            config.rate_limit,
        )
        .await?;
        Self::new(
            kcp,
            Arc::new(smux_rs::Session::new_client(&config.smux)?),
            config.nocomp,
            0,
            true,
        )
    }

    /// Build a server session over one accepted raw-TCP datagram socket.
    ///
    /// UDP servers assemble `kcp_rs::KcpListener` + `CryptoTransport` for
    /// shared-socket peer demultiplexing (see `kcptun-server`). A raw-TCP
    /// socket is already private to one peer, so it can directly adopt the
    /// conversation ID from its first valid packet.
    pub async fn serve_transport(
        socket: Arc<knet::DatagramSocket>,
        peer: std::net::SocketAddr,
        key: &[u8],
        crypt: &str,
        config: &KcptunConfig,
    ) -> Result<Self> {
        let kcp = crate::kcp_transport::server_kcp_stream_with_socket_rate_limited(
            socket,
            peer,
            key,
            crypt,
            config.kcp.clone(),
            config.offload_profile,
            config.rate_limit,
        )
        .await?;
        let smux = Arc::new(smux_rs::Session::new_server(&config.smux)?);
        smux.enable_accept();
        Self::new(kcp, smux, config.nocomp, 0, false)
    }

    /// Start a client-side session over an established KCP connection.
    pub fn client(kcp: kcp_rs::KcpStream, config: &KcptunConfig) -> Result<Self> {
        Self::new(
            kcp,
            Arc::new(smux_rs::Session::new_client(&config.smux)?),
            config.nocomp,
            config.rate_limit,
            true,
        )
    }

    /// Start a server-side session over an established KCP connection.
    pub fn server(kcp: kcp_rs::KcpStream, config: &KcptunConfig) -> Result<Self> {
        let smux = Arc::new(smux_rs::Session::new_server(&config.smux)?);
        smux.enable_accept();
        Self::new(kcp, smux, config.nocomp, config.rate_limit, false)
    }

    /// Start a server session whose packet transport already applies the
    /// configured on-wire rate limit (the shared-UDP listener path).
    pub fn server_with_limited_transport(
        kcp: kcp_rs::KcpStream,
        config: &KcptunConfig,
    ) -> Result<Self> {
        let smux = Arc::new(smux_rs::Session::new_server(&config.smux)?);
        smux.enable_accept();
        Self::new(kcp, smux, config.nocomp, 0, false)
    }

    fn new(
        kcp: kcp_rs::KcpStream,
        smux: Arc<smux_rs::Session>,
        nocomp: bool,
        rate_limit: u32,
        active_open: bool,
    ) -> Result<Self> {
        let kcp = Arc::new(kcp);
        let flush_notify = Arc::new(knet::Notify::new());
        let dead = Arc::new(AtomicBool::new(false));
        let compressor = Arc::new(Mutex::new(snap::write::FrameEncoder::new(Vec::new())));
        let limiter = Arc::new(RateLimiter::new(rate_limit));
        let handles = vec![
            knet::spawn_task(read_loop(
                kcp.clone(),
                smux.clone(),
                flush_notify.clone(),
                dead.clone(),
                nocomp,
            )),
            knet::spawn_task(write_loop(
                kcp.clone(),
                smux.clone(),
                compressor,
                flush_notify.clone(),
                dead.clone(),
                limiter,
                nocomp,
            )),
        ];
        let session = Self {
            kcp,
            smux,
            flush_notify,
            dead,
            created_ms: knet::mono_ms(),
            _handles: handles,
        };
        kcp_rs::DEFAULT_SNMP.session_opened(active_open);
        Ok(session)
    }

    /// Open and queue a client-side SMUX stream.
    pub fn open_stream(&self) -> Result<Arc<smux_rs::Stream>, smux_rs::SessionError> {
        let stream = self.smux.open_stream()?;
        self.smux.queue_syn(stream.id());
        stream.set_flush_notify(self.flush_notify.clone());
        self.flush_notify.notify_one();
        Ok(stream)
    }

    /// Accept the next server-side SMUX stream.
    pub async fn accept(&self) -> Result<Arc<smux_rs::Stream>, smux_rs::SessionError> {
        loop {
            if self.smux.is_closed() {
                return Err(smux_rs::SessionError::SessionClosed);
            }
            if let Some(id) = self.smux.pop_accepted_stream() {
                if let Some(stream) = self.smux.streams().lock().get(&id).cloned() {
                    stream.set_flush_notify(self.flush_notify.clone());
                    return Ok(stream);
                }
                continue;
            }
            self.smux.accept_notify().notified().await;
        }
    }

    /// Remove a stream from the session map.
    pub fn remove_stream(&self, id: u32) {
        self.smux.remove_stream(id);
    }

    /// Wake the shared SMUX writer.
    pub fn flush_notify(&self) -> Arc<knet::Notify> {
        self.flush_notify.clone()
    }

    /// Latest KCP transport activity, using the monotonic clock.
    pub fn last_activity_ms(&self) -> u64 {
        self.kcp.last_activity_ms()
    }

    /// Monotonic timestamp in milliseconds when this session was created.
    ///
    /// Used by `--autoexpire` to compute an absolute expiry deadline from
    /// creation time (matching Go kcptun), independent of keepalive activity.
    pub fn created_ms(&self) -> u64 {
        self.created_ms
    }

    /// Whether the KCP or SMUX session has failed or timed out.
    pub fn is_dead(&self) -> bool {
        self.dead.load(Ordering::Acquire)
            || self.kcp.is_dead()
            || self.kcp.is_closed()
            || self.smux.is_closed()
            || self.smux.is_keepalive_timeout()
    }

    /// Close KCP, SMUX, and all streams.
    pub fn close(&self) {
        self.dead.store(true, Ordering::Release);
        self.smux.close();
        self.kcp.close();
        self.flush_notify.notify_waiters();
    }
}

impl Drop for KcptunSession {
    fn drop(&mut self) {
        self.close();
        kcp_rs::DEFAULT_SNMP.session_closed();
    }
}

async fn read_loop(
    kcp: Arc<kcp_rs::KcpStream>,
    smux: Arc<smux_rs::Session>,
    flush: Arc<knet::Notify>,
    dead: Arc<AtomicBool>,
    nocomp: bool,
) {
    let mut buf = vec![0u8; 64 * 1024];
    let mut decoder = (!nocomp).then(crate::SnappyStreamDecoder::new);
    while !dead.load(Ordering::Acquire) && !smux.is_closed() && !kcp.is_closed() {
        let n = match kcp.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) if kcp.is_closed() => break,
            Err(_) => continue,
        };
        let result = if let Some(decoder) = decoder.as_mut() {
            decoder.feed(&buf[..n]).and_then(|data| {
                if data.is_empty() {
                    Ok(())
                } else {
                    smux.process_data(&data)
                        .map(|_| ())
                        .map_err(std::io::Error::other)
                }
            })
        } else {
            smux.process_data(&buf[..n])
                .map(|_| ())
                .map_err(std::io::Error::other)
        };
        if let Err(error) = result {
            log::warn!("SMUX process_data error: {error}");
        }
        flush.notify_one();
    }
    dead.store(true, Ordering::Release);
    smux.close();
    kcp.close();
}

async fn write_loop(
    kcp: Arc<kcp_rs::KcpStream>,
    smux: Arc<smux_rs::Session>,
    compressor: Arc<Mutex<snap::write::FrameEncoder<Vec<u8>>>>,
    flush: Arc<knet::Notify>,
    dead: Arc<AtomicBool>,
    limiter: Arc<RateLimiter>,
    nocomp: bool,
) {
    // KCP owns its own event-driven flush loop. Stream writes notify this task
    // directly, so this is only an idle health-check cadence. A 2ms timer per
    // session dominates short concurrent transfers.
    const IDLE_WAKE_MS: u64 = 10;
    let mut out = BytesMut::with_capacity(64 * 1024);
    let mut health = 0u32;
    while !dead.load(Ordering::Acquire) && !smux.is_closed() && !kcp.is_closed() {
        // Fast path: a wake is already pending (a stream write or the previous
        // iteration's `notify_one` preserved a permit) — await it directly so
        // idle iterations only pay the timer-wheel cost, not every iteration.
        if flush.has_pending() {
            flush.notified().await;
        } else {
            let _ = knet::timeout(Duration::from_millis(IDLE_WAKE_MS), flush.notified()).await;
        }
        if health == 0 {
            health = 50;
            if kcp.is_dead() || smux.is_keepalive_timeout() {
                break;
            }
            if smux.check_keepalive() {
                smux.keepalive_frame().encode(&mut out);
                smux.mark_keepalive_sent();
            }
        } else {
            health -= 1;
        }
        let has_pending_stream_data = smux
            .streams()
            .lock()
            .values()
            .any(|stream| stream.pending_send() > 0);
        let allow_fin = !has_pending_stream_data && kcp.wait_send() == 0;
        let fin_ids =
            smux.prepare_outbound_into_controlled(&mut out, 64 * 1024, smux.version(), allow_fin);

        // Match the production loop's stream cleanup semantics.
        {
            let streams = smux.streams();
            let mut stream_map = streams.lock();
            let linger = Duration::from_secs(30);
            let stale: Vec<u32> = stream_map
                .iter()
                .filter(|(_, stream)| {
                    (stream.is_local_closed() && stream.is_remote_closed() && stream.is_fin_sent())
                        || (stream.is_local_closed()
                            && stream.pending_send() == 0
                            && stream
                                .local_closed_elapsed()
                                .is_some_and(|elapsed| elapsed >= linger))
                })
                .map(|(id, _)| *id)
                .collect();
            for id in stale {
                if let Some(stream) = stream_map.remove(&id) {
                    stream.close();
                }
            }
        }

        let packet: Option<Bytes> = if out.is_empty() {
            None
        } else if nocomp {
            Some(out.split().freeze())
        } else {
            let plain = out.split().freeze();
            let plain_len = plain.len();
            let encode = {
                let compressor = compressor.clone();
                move || {
                    let mut encoder = compressor.lock();
                    if encoder.write_all(&plain).is_err() || encoder.flush().is_err() {
                        return Bytes::new();
                    }
                    std::mem::take::<Vec<u8>>(encoder.get_mut()).into()
                }
            };
            Some(if kcrypt_rs::should_cpu_block_compress(plain_len) {
                knet::cpu_block(encode).await
            } else {
                encode()
            })
        };
        if let Some(packet) = packet.filter(|p| !p.is_empty()) {
            // Compatibility fallback for callers that provide a pre-built
            // KcpStream. Production client/server transports rate-limit the
            // encrypted/FEC batch below KCP and pass a disabled limiter here.
            loop {
                let wait = limiter.acquire(packet.len());
                if wait.is_zero() {
                    break;
                }
                knet::sleep(wait).await;
            }
            if kcp.write_all(&packet).await.is_err() {
                break;
            }
            smux.mark_fins_sent(&fin_ids);

            // `prepare_outbound_into_controlled` deliberately caps each KCP
            // write to 64 KiB. If a stream still has queued data, preserve a
            // notify permit for the next iteration instead of imposing the
            // idle 2ms poll delay between chunks (a 128 KiB TCP write commonly
            // needs two iterations). Backpressure remains bounded by
            // `write_all`, which waits when the KCP window is full.
            if smux
                .streams()
                .lock()
                .values()
                .any(|stream| stream.pending_send() > 0)
            {
                flush.notify_one();
            }
        }
    }
    dead.store(true, Ordering::Release);
    smux.close();
    kcp.close();
}

#[cfg(all(test, feature = "tokio"))]
mod tests {
    use super::*;
    use knet::{AsyncReadExt, AsyncWriteExt};
    use std::net::SocketAddr;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shared_client_server_session_roundtrip() {
        let a = knet::UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        let b = knet::UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        let addr_a = a.local_addr().unwrap();
        let addr_b = b.local_addr().unwrap();
        drop(a);
        drop(b);
        let socket_a = Arc::new(knet::DatagramSocket::Udp(
            knet::UdpSocket::connect(addr_a, addr_b).unwrap(),
        ));
        let socket_b = Arc::new(knet::DatagramSocket::Udp(
            knet::UdpSocket::connect(addr_b, addr_a).unwrap(),
        ));
        let config = kcp_rs::KcpConfig {
            conv: 0x51_55_58,
            mode: kcp_rs::KcpMode::Fast3,
            datashard: 3,
            parityshard: 1,
            ..Default::default()
        };
        let session_config = KcptunConfig {
            kcp: config,
            smux: smux_rs::Config {
                keepalive_interval: 0,
                keepalive_timeout: 0,
                ..smux_rs::DEFAULT_CONFIG.clone()
            },
            nocomp: false,
            rate_limit: 0,
            offload_profile: OffloadProfile::Tokio,
        };
        let key = b"0123456789abcdef0123456789abcdef";
        let client = KcptunSession::connect(socket_a, addr_b, key, "aes", &session_config)
            .await
            .unwrap();
        let server = Arc::new(
            KcptunSession::serve_transport(socket_b, addr_a, key, "aes", &session_config)
                .await
                .unwrap(),
        );

        let server_task = {
            let server = server.clone();
            knet::spawn_task(async move {
                let stream = server.accept().await.unwrap();
                let mut stream = smux_rs::SmuxIo::new(stream, server.flush_notify());
                let mut input = [0u8; 14];
                stream.read_exact(&mut input).await.unwrap();
                assert_eq!(&input, b"common-session");
                stream.write_all(b"roundtrip-ok").await.unwrap();
            })
        };
        let stream = client.open_stream().unwrap();
        let mut stream = smux_rs::SmuxIo::new(stream, client.flush_notify());
        stream.write_all(b"common-session").await.unwrap();
        let mut output = [0u8; 12];
        knet::timeout(Duration::from_secs(5), stream.read_exact(&mut output))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&output, b"roundtrip-ok");
        server_task.await.unwrap();
        client.close();
        server.close();
    }
}

/// Goroutine-specific latency regression for the complete KCP + SMUX session
/// path. It deliberately opens a fresh SMUX stream per message, mirroring the
/// proxy's TCP-accept path while excluding the outer TCP sockets. If this path
/// regresses to the writer loop's 10ms idle wake, the assertion catches it
/// before a full tunnel benchmark does.
#[cfg(test)]
mod goroutine_tests {
    use super::*;
    use knet::{AsyncReadExt, AsyncWriteExt};
    use std::net::SocketAddr;

    #[test]
    fn fresh_stream_echoes_do_not_wait_for_idle_flush_tick() {
        const ROUNDS: usize = 32;
        knet::block_on(async {
            let a = knet::UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
            let b = knet::UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
            let addr_a = a.local_addr().unwrap();
            let addr_b = b.local_addr().unwrap();
            drop(a);
            drop(b);
            let socket_a = Arc::new(knet::DatagramSocket::Udp(
                knet::UdpSocket::connect(addr_a, addr_b).unwrap(),
            ));
            let socket_b = Arc::new(knet::DatagramSocket::Udp(
                knet::UdpSocket::connect(addr_b, addr_a).unwrap(),
            ));
            let session_config = KcptunConfig {
                kcp: kcp_rs::KcpConfig {
                    conv: 0x51_55_58,
                    mode: kcp_rs::KcpMode::Fast3,
                    datashard: 0,
                    parityshard: 0,
                    ..Default::default()
                },
                smux: smux_rs::Config {
                    keepalive_interval: 0,
                    keepalive_timeout: 0,
                    ..smux_rs::DEFAULT_CONFIG.clone()
                },
                nocomp: true,
                rate_limit: 0,
                offload_profile: OffloadProfile::Tokio,
            };
            let key = b"0123456789abcdef0123456789abcdef";
            let client = KcptunSession::connect(socket_a, addr_b, key, "null", &session_config)
                .await
                .unwrap();
            let server = Arc::new(
                KcptunSession::serve_transport(socket_b, addr_a, key, "null", &session_config)
                    .await
                    .unwrap(),
            );
            let server_task = {
                let server = server.clone();
                knet::spawn_task(async move {
                    for _ in 0..ROUNDS {
                        let stream = server.accept().await.unwrap();
                        let mut stream = smux_rs::SmuxIo::new(stream, server.flush_notify());
                        let mut byte = [0u8; 1];
                        stream.read_exact(&mut byte).await.unwrap();
                        stream.write_all(&byte).await.unwrap();
                    }
                })
            };

            let mut worst = Duration::ZERO;
            let mut samples: Vec<Duration> = Vec::with_capacity(ROUNDS);
            for value in 0..ROUNDS as u8 {
                let started = std::time::Instant::now();
                let stream = client.open_stream().unwrap();
                let mut stream = smux_rs::SmuxIo::new(stream, client.flush_notify());
                stream.write_all(&[value]).await.unwrap();
                let mut echoed = [0u8; 1];
                // Bounded read: an unbounded read_exact turned any lost-wake /
                // starvation bug into a >60 s test hang instead of a fast panic.
                knet::timeout(Duration::from_secs(3), stream.read_exact(&mut echoed))
                    .await
                    .expect("echo read timed out (stream write stalled)")
                    .unwrap();
                assert_eq!(echoed, [value]);
                samples.push(started.elapsed());
                worst = worst.max(started.elapsed());
            }
            server_task.await.unwrap();
            client.close();
            server.close();
            // Regression guard for "writer waits for the idle flush tick":
            // the MEDIAN must stay in the sub-millisecond notify path. The
            // worst case on a loaded desktop can absorb one 10 ms idle tick
            // through scheduler jitter alone (IDLE_WAKE_MS), so a hard worst
            // bound only guards gross stalls, not the tick itself.
            samples.sort();
            let median = samples[samples.len() / 2];
            assert!(
                median < Duration::from_millis(5),
                "fresh stream echo median waited for idle flush tick: {median:?} (worst {worst:?})"
            );
            assert!(
                worst < Duration::from_millis(50),
                "fresh stream echo worst-case stall: {worst:?}"
            );
        });
    }

    /// Exercise the production forwarding shape end to end: a fresh local TCP
    /// connection is piped to a fresh SMUX stream, the server side opens a
    /// fresh target TCP connection and pipes it back to a TCP echo listener.
    /// The lighter stream-only test above cannot catch a wake lost between the
    /// two `copy_bidirectional` state machines.
    #[test]
    fn tcp_smux_tcp_proxy_does_not_wait_for_idle_ticks() {
        const ROUNDS: usize = 16;
        knet::block_on(async {
            let a = knet::UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
            let b = knet::UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
            let addr_a = a.local_addr().unwrap();
            let addr_b = b.local_addr().unwrap();
            drop(a);
            drop(b);
            let socket_a = Arc::new(knet::DatagramSocket::Udp(
                knet::UdpSocket::connect(addr_a, addr_b).unwrap(),
            ));
            let socket_b = Arc::new(knet::DatagramSocket::Udp(
                knet::UdpSocket::connect(addr_b, addr_a).unwrap(),
            ));
            let session_config = KcptunConfig {
                kcp: kcp_rs::KcpConfig {
                    conv: 0x51_55_58,
                    mode: kcp_rs::KcpMode::Fast3,
                    datashard: 0,
                    parityshard: 0,
                    ..Default::default()
                },
                smux: smux_rs::Config {
                    keepalive_interval: 0,
                    keepalive_timeout: 0,
                    ..smux_rs::DEFAULT_CONFIG.clone()
                },
                nocomp: true,
                rate_limit: 0,
                offload_profile: OffloadProfile::Tokio,
            };
            let key = b"0123456789abcdef0123456789abcdef";
            let client = Arc::new(
                KcptunSession::connect(socket_a, addr_b, key, "null", &session_config)
                    .await
                    .unwrap(),
            );
            let server = Arc::new(
                KcptunSession::serve_transport(socket_b, addr_a, key, "null", &session_config)
                    .await
                    .unwrap(),
            );

            let target_listener = knet::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
                .await
                .unwrap();
            let target_addr = target_listener.local_addr().unwrap();
            let target_task = knet::spawn_task(async move {
                let mut handlers = Vec::with_capacity(ROUNDS);
                for _ in 0..ROUNDS {
                    let (mut target, _) = target_listener.accept().await.unwrap();
                    handlers.push(knet::spawn_task(async move {
                        let mut byte = [0u8; 1];
                        target.read_exact(&mut byte).await.unwrap();
                        target.write_all(&byte).await.unwrap();
                    }));
                }
                for handler in handlers {
                    handler.await.unwrap();
                }
            });

            let server_task = {
                let server = server.clone();
                knet::spawn_task(async move {
                    let mut handlers = Vec::with_capacity(ROUNDS);
                    for _ in 0..ROUNDS {
                        let stream = server.accept().await.unwrap();
                        let notify = server.flush_notify();
                        handlers.push(knet::spawn_task(async move {
                            let mut target = knet::TcpStream::connect(target_addr.to_string())
                                .await
                                .unwrap();
                            let mut smux = smux_rs::SmuxIo::new(stream, notify);
                            crate::pipe(&mut target, &mut smux, 0).await.unwrap();
                        }));
                    }
                    for handler in handlers {
                        handler.await.unwrap();
                    }
                })
            };

            let local_listener = knet::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
                .await
                .unwrap();
            let local_addr = local_listener.local_addr().unwrap();
            let client_task = {
                let client = client.clone();
                knet::spawn_task(async move {
                    let mut handlers = Vec::with_capacity(ROUNDS);
                    for _ in 0..ROUNDS {
                        let (mut local, _) = local_listener.accept().await.unwrap();
                        let stream = client.open_stream().unwrap();
                        let notify = client.flush_notify();
                        handlers.push(knet::spawn_task(async move {
                            let mut smux = smux_rs::SmuxIo::new(stream, notify);
                            crate::pipe(&mut local, &mut smux, 0).await.unwrap();
                        }));
                    }
                    for handler in handlers {
                        handler.await.unwrap();
                    }
                })
            };

            let mut worst = Duration::ZERO;
            let mut samples: Vec<Duration> = Vec::with_capacity(ROUNDS);
            for value in 0..ROUNDS as u8 {
                let started = std::time::Instant::now();
                let mut local = knet::TcpStream::connect(local_addr.to_string())
                    .await
                    .unwrap();
                local.write_all(&[value]).await.unwrap();
                let mut echoed = [0u8; 1];
                local.read_exact(&mut echoed).await.unwrap();
                assert_eq!(echoed, [value]);
                samples.push(started.elapsed());
                worst = worst.max(started.elapsed());
            }
            client_task.await.unwrap();
            server_task.await.unwrap();
            target_task.await.unwrap();
            client.close();
            server.close();
            samples.sort();
            let median = samples[samples.len() / 2];
            assert!(
                median < Duration::from_millis(5),
                "full TCP/SMUX/TCP forwarding median waited for an idle tick: {median:?} (worst {worst:?})"
            );
            assert!(
                worst < Duration::from_millis(50),
                "full TCP/SMUX/TCP forwarding worst-case stall: {worst:?}"
            );
        });
    }
}
