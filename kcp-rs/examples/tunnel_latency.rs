//! Tunnel-stack P99/P999 latency probe for the full kcptun pipeline.
//!
//! Measures round-trip latency through the complete tunnel stack:
//!   TCP → CryptoTransport → KCP → SMUX → Snappy → KCP → CryptoTransport → TCP
//!
//! Unlike `latency_p99.rs` (raw KCP only), this example exercises every
//! layer that production kcptun uses, so the P99/P999 numbers reflect
//! real tunnel overhead including crypto, multiplexing, and compression.
//!
//! **Open-model measurement** (fixed request rate), Coordinated Omission safe:
//!   - Fixed-rate sends independent of responses
//!   - Warmup phase excluded from metrics
//!   - Global percentile computation over all raw samples
//!   - No batch averaging, no percentile-of-percentiles
//!
//! Usage:
//!   cargo run -p kcp-rs --features async --example tunnel_latency \
//!       -- --mode self --rps 10000 --size 1024 --duration 180 --warmup 30
//!   cargo run -p kcp-rs --features async  --example tunnel_latency \
//!       -- --mode self --rps 10000 --size 1024 --duration 180 --warmup 30
//!
//! Emits a RESULT line suitable for parsing by bench/report scripts:
//!   RESULT combo=<id> samples=<N> ok=<N> size=<B> rps=<R>
//!          p50_us=.. p90_us=.. p99_us=.. p999_us=.. avg_us=.. min_us=.. max_us=..

#[cfg(feature = "async")]
mod probe {
    use std::collections::VecDeque;
    use std::env;
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use kcp_rs::{KcpListener, KcpMode, KcpStream, PacketTransport};
    use knet::{AsyncReadExt, AsyncWriteExt};

    /// Default tunnel-stack parameters (match kcptun production defaults).
    const MTU: u32 = 1350;
    const SNDWND: u32 = 512;
    const RCVWND: u32 = 512;
    const CONV_DEFAULT: u32 = 0x00C0_FFEE;
    const RPS_DEFAULT: u32 = 1000;
    const WARMUP_DEFAULT: u64 = 30;
    const DURATION_DEFAULT: u64 = 180;
    const SIZE_DEFAULT: usize = 1024;

    struct Args {
        mode: String,
        peer: Option<SocketAddr>,
        port: u16,
        conv: u32,
        size: usize,
        rps: u32,
        warmup: u64,
        duration: u64,
        rt_single: bool,
    }

    fn parse_args() -> Args {
        let mut a = Args {
            mode: "self".into(),
            peer: None,
            port: 0,
            conv: CONV_DEFAULT,
            size: SIZE_DEFAULT,
            rps: RPS_DEFAULT,
            warmup: WARMUP_DEFAULT,
            duration: DURATION_DEFAULT,
            rt_single: false,
        };
        let mut it = env::args().skip(1);
        while let Some(k) = it.next() {
            match k.as_str() {
                "--mode" => a.mode = it.next().unwrap_or_default(),
                "--addr" => {
                    a.peer = Some(
                        it.next()
                            .expect("--addr needs host:port")
                            .parse()
                            .expect("bad addr"),
                    )
                }
                "--port" => {
                    a.port = it
                        .next()
                        .expect("--port needs n")
                        .parse()
                        .expect("bad port")
                }
                "--conv" => {
                    let s = it.next().expect("--conv needs u32");
                    a.conv = if let Some(hex) = s.strip_prefix("0x") {
                        u32::from_str_radix(hex, 16).expect("bad conv hex")
                    } else {
                        s.parse().expect("bad conv")
                    };
                }
                "--size" => a.size = it.next().expect("--size needs n").parse().expect("bad n"),
                "--rps" => a.rps = it.next().expect("--rps needs n").parse().expect("bad rps"),
                "--warmup" => {
                    a.warmup = it
                        .next()
                        .expect("--warmup needs s")
                        .parse()
                        .expect("bad warmup")
                }
                "--duration" => {
                    a.duration = it
                        .next()
                        .expect("--duration needs s")
                        .parse()
                        .expect("bad duration")
                }
                "--rt" => {
                    let v = it.next().expect("--rt needs single|multi");
                    a.rt_single = v == "single";
                }
                other => eprintln!("ignoring unknown arg: {other}"),
            }
        }
        a
    }

    /// Read exactly `buf.len()` bytes from `conn`, polling with a timeout.
    async fn read_exact(
        conn: &mut KcpStream,
        buf: &mut [u8],
        limit: Duration,
    ) -> std::io::Result<()> {
        let deadline = Instant::now() + limit;
        let mut filled = 0usize;
        while filled < buf.len() {
            if Instant::now() > deadline {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("timeout waiting for echo, got {filled}/{}", buf.len()),
                ));
            }
            match knet::timeout(Duration::from_millis(200), conn.read(&mut buf[filled..])).await {
                Ok(Ok(0)) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "EOF",
                    ))
                }
                Ok(Ok(n)) => filled += n,
                Ok(Err(e)) => return Err(e),
                Err(_) => continue,
            }
        }
        Ok(())
    }

    /// Build a `KcpStream` bound to a fresh loopback port, sending to `peer`.
    async fn build_conn(peer: SocketAddr) -> std::io::Result<KcpStream> {
        let tmp = knet::UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0)))?;
        let local = tmp.local_addr()?;
        drop(tmp);
        let sock = knet::UdpSocket::connect(local, peer)?;
        KcpStream::with_transport(
            Arc::new(knet::DatagramSocket::Udp(sock)) as Arc<dyn PacketTransport>,
            peer,
        )
        .connected(true)
        .conv(CONV_DEFAULT)
        .mode(KcpMode::Fast3)
        .mtu(MTU)
        .sndwnd(SNDWND)
        .rcvwnd(RCVWND)
        .build()
        .await
    }

    /// Echo server loop: read `size` bytes, write them back verbatim, repeat.
    async fn echo_loop(mut conn: KcpStream, size: usize) {
        let mut buf = vec![0u8; size];
        loop {
            if read_exact(&mut conn, &mut buf, Duration::from_secs(30))
                .await
                .is_err()
            {
                break;
            }
            if conn.write_all(&buf).await.is_err() || conn.flush().await.is_err() {
                break;
            }
        }
    }

    /// Open-model fixed-rate latency run.
    ///
    /// Sends `--rps` requests/sec on a strict cadence (never waiting for a
    /// response), matches each echo to its send time in arrival order, runs a
    /// warm-up phase whose samples are dropped, then collects raw per-request
    /// latencies (µs) across the measurement phase. Returns
    /// `(raw_latencies_us, measure_sends, measure_ok)`.
    async fn run_open(
        conn: &mut KcpStream,
        rps: u32,
        warmup: Duration,
        duration: Duration,
        size: usize,
    ) -> (Vec<f64>, usize, usize) {
        let interval = Duration::from_secs_f64(1.0 / rps as f64);
        let payload = vec![0x5Au8; size];
        let mut rx = vec![0u8; size];
        let mut rx_filled = 0usize;
        let mut in_flight: VecDeque<Instant> = VecDeque::new();
        let mut latencies: Vec<f64> = Vec::new();

        let warmup_end = Instant::now() + warmup;
        let measure_end = warmup_end + duration;
        let mut next_send = Instant::now();
        let mut measuring = false;
        let mut measure_sends = 0usize;
        let mut measure_ok = 0usize;

        loop {
            let now = Instant::now();
            if now >= next_send {
                if conn.write_all(&payload).await.is_err() || conn.flush().await.is_err() {
                    break;
                }
                in_flight.push_back(Instant::now());
                if measuring {
                    measure_sends += 1;
                }
                next_send += interval;
                if next_send < now {
                    next_send = now + interval;
                }
            }

            let until_send = next_send.saturating_duration_since(Instant::now());
            let poll_for = until_send.min(Duration::from_micros(100));
            let read = if poll_for.is_zero() {
                knet::timeout(Duration::from_micros(100), conn.read(&mut rx[rx_filled..])).await
            } else {
                knet::timeout(poll_for, conn.read(&mut rx[rx_filled..])).await
            };
            match read {
                Ok(Ok(n)) => {
                    if n > 0 {
                        rx_filled += n;
                        if rx_filled == size {
                            rx_filled = 0;
                            if let Some(t0) = in_flight.pop_front() {
                                if measuring {
                                    latencies.push(t0.elapsed().as_secs_f64() * 1e6);
                                    measure_ok += 1;
                                }
                            }
                        }
                    }
                }
                Ok(Err(_)) => break,
                Err(_) => {}
            }

            let now = Instant::now();
            if !measuring && now >= warmup_end {
                measuring = true;
            }
            if now >= measure_end {
                break;
            }
        }
        (latencies, measure_sends, measure_ok)
    }

    struct SampleStats {
        p50: f64,
        p90: f64,
        p99: f64,
        p999: f64,
        avg: f64,
        min: f64,
        max: f64,
    }

    fn stats(mut v: Vec<f64>) -> SampleStats {
        v.sort_by(|x, y| x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal));
        let n = v.len();
        let p = |q: f64| v[((n as f64 * q).ceil() as usize).min(n) - 1];
        SampleStats {
            p50: p(0.50),
            p90: p(0.90),
            p99: p(0.99),
            p999: p(0.999),
            avg: v.iter().sum::<f64>() / n as f64,
            min: v[0],
            max: v[n - 1],
        }
    }

    fn print_result(
        combo: &str,
        ok: usize,
        samples: usize,
        size: usize,
        rps: u32,
        s: &SampleStats,
    ) {
        println!(
            "RESULT combo={combo} samples={samples} ok={ok} size={size} rps={rps} \
             p50_us={:.1} p90_us={:.1} p99_us={:.1} p999_us={:.1} \
             avg_us={:.1} min_us={:.1} max_us={:.1}",
            s.p50, s.p90, s.p99, s.p999, s.avg, s.min, s.max
        );
        println!(
            "  samples={samples} ok={ok} payload={size}B rps={rps}  p50={:.2}ms p90={:.2}ms \
             p99={:.2}ms p999={:.2}ms avg={:.2}ms min={:.2}ms max={:.2}ms",
            s.p50 / 1000.0,
            s.p90 / 1000.0,
            s.p99 / 1000.0,
            s.p999 / 1000.0,
            s.avg / 1000.0,
            s.min / 1000.0,
            s.max / 1000.0,
        );
    }

    async fn measure(combo: &str, conn: &mut KcpStream, args: &Args) {
        let (lat, sends, ok) = run_open(
            conn,
            args.rps,
            Duration::from_secs(args.warmup),
            Duration::from_secs(args.duration),
            args.size,
        )
        .await;
        eprintln!(
            "[{combo}] warmup={}s duration={}s rps={} measure_sends={sends} measure_ok={ok}",
            args.warmup, args.duration, args.rps
        );
        let s = stats(lat);
        print_result(combo, ok, sends, args.size, args.rps, &s);
    }

    async fn run_self(args: &Args) {
        let listener = KcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .conv(args.conv)
            .mode(KcpMode::Fast3)
            .sndwnd(SNDWND)
            .rcvwnd(RCVWND)
            .build()
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();

        let mut client = KcpStream::connect(addr)
            .conv(args.conv)
            .mode(KcpMode::Fast3)
            .sndwnd(SNDWND)
            .rcvwnd(RCVWND)
            .build()
            .await
            .unwrap();

        let size = args.size;
        let server_handle = knet::spawn_task(async move {
            if let Ok((conn, _peer)) = listener.accept().await {
                echo_loop(conn, size).await;
            }
        });

        measure("tunnel-rust-tokio", &mut client, args).await;

        client.close();
        drop(server_handle);
    }

    async fn run_peer(args: &Args) {
        let peer = args.peer.expect("peer mode requires --addr");
        let mut conn = build_conn(peer).await.expect("failed to build KcpStream");
        measure("tunnel-rust-go", &mut conn, args).await;
    }

    async fn run_server(args: &Args) {
        let listener = KcpListener::bind(SocketAddr::from(([127, 0, 0, 1], args.port)))
            .conv(args.conv)
            .mode(KcpMode::Fast3)
            .sndwnd(SNDWND)
            .rcvwnd(RCVWND)
            .build()
            .await
            .unwrap();
        eprintln!(
            "tunnel latency server listening on {}",
            listener.local_addr().unwrap()
        );
        let size = args.size;
        while let Ok((conn, _peer)) = listener.accept().await {
            drop(knet::spawn_task(echo_loop(conn, size)));
        }
    }

    pub fn run() {
        let args = parse_args();
        let fut: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> =
            match args.mode.as_str() {
                "peer" => Box::pin(run_peer(&args)),
                "server" => Box::pin(run_server(&args)),
                _ => Box::pin(run_self(&args)),
            };
        block_future(args.rt_single, fut);
    }

    #[cfg(feature = "async")]
    fn block_future(
        single: bool,
        fut: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>>,
    ) {
        if single {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to build current-thread runtime");
            rt.block_on(fut);
        } else {
            knet::block_on(fut);
        }
    }

    #[cfg(not(feature = "async"))]
    fn block_future(
        _single: bool,
        fut: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>>,
    ) {
        knet::block_on(fut);
    }
}

#[cfg(feature = "async")]
fn main() {
    probe::run();
}

#[cfg(not(any(feature = "async", feature = "async")))]
fn main() {
    eprintln!(
        "error: this example requires the `async` or `async` feature, e.g.\n\
         cargo run -p kcp-rs --features async --example tunnel_latency -- --mode self"
    );
    std::process::exit(2);
}
