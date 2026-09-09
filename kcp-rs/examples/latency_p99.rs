//! Strict P99 / P999 round-trip latency probe for the raw kcp-rs KCP layer.
//!
//! Uses the **high-level API** exclusively:
//! - Client: `KcpStream::connect(addr).build().await`
//! - Server: `KcpListener::bind(addr).build().await` → `listener.accept().await`
//! - Echo:   `knet::spawn_task(echo_loop(conn, size))`
//!
//! **Open-model measurement** (fixed request rate), designed to avoid
//! coordinated omission and to produce a statistically valid latency
//! distribution:
//!
//! - Fixed-rate sends (--rps) independent of response timing; each request's
//!   latency is measured from its own send time to the arrival of its (ordered)
//!   echo, so slow periods inflate the tail instead of being hidden.
//! - Separate warm-up phase (--warmup, default 5s) whose samples are excluded.
//! - Measurement phase (--duration, default 60s); all raw per-request latencies
//!   are aggregated and one global P50/P90/P99/P999 is computed (never averaged).
//! - No kcptun / SMUX / snappy / crypto layers — the bare `KcpStream` over a
//!   `kio` UDP socket, mirroring `kcp-go`'s `UDPSession`.
//!
//! Runs on either runtime backend via the matching feature:
//!
//! ```text
//! # kcp-rs ↔ kcp-rs (tokio) — listener + connect, server echoes in a task
//! cargo run -p kcp-rs --features async --example latency_p99 -- --mode self
//! # kcp-rs client → external echo server (e.g. kcp-go `server`) — cross interop
//! cargo run -p kcp-rs --features async --example latency_p99 -- \
//!     --mode peer --addr 127.0.0.1:39000
//! # kcp-rs echo server (accepts clients and echoes)
//! cargo run -p kcp-rs --features async --example latency_p99 -- \
//!     --mode server --port 39001
//! ```
//!
//! Emits a machine-readable `RESULT` line plus a human table:
//!
//! ```text
//! RESULT combo=<id> samples=<N> ok=<N> shed=<N|na> inflight_end=<N> size=<B> rps=<R>
//!        p50_us=.. p90_us=.. p99_us=.. p999_us=.. avg_us=.. min_us=.. max_us=..
//! ```

#[cfg(any(feature = "async", feature = "async", feature = "async"))]
mod probe {
    use std::collections::VecDeque;
    use std::env;
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use kcp_rs::{KcpListener, KcpMode, KcpStream};
    use knet::AsyncReadExt;

    /// KCP Fast3 profile (nodelay=1, interval=10, resend=2, nc=1) — matches Go
    /// `kcp-go` `SetNoDelay(1, 10, 2, 1)`.
    const MTU: u32 = 1350;
    const SNDWND: u32 = 512;
    const RCVWND: u32 = 512;
    const CONV_DEFAULT: u32 = 0x00C0_FFEE;
    const RPS_DEFAULT: u32 = 500;
    const WARMUP_DEFAULT: u64 = 5;
    const DURATION_DEFAULT: u64 = 60;
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
        concurrency: usize,
        mtu: u32,
        pprof_addr: Option<String>,
        snmp: bool,
        workers: usize,
        reader_poll_us: u64,
        diag: bool,
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
            concurrency: 0,
            mtu: MTU,
            pprof_addr: None,
            snmp: false,
            workers: 0,
            reader_poll_us: 100,
            diag: false,
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
                "--concurrency" | "-c" => {
                    a.concurrency = it
                        .next()
                        .expect("--concurrency needs n")
                        .parse()
                        .expect("bad concurrency")
                }
                "--mtu" => a.mtu = it.next().expect("--mtu needs n").parse().expect("bad mtu"),
                "--pprof" => a.pprof_addr = Some(it.next().expect("--pprof needs host:port")),
                "--snmp" => a.snmp = true,
                "--workers" => {
                    a.workers = it
                        .next()
                        .expect("--workers needs n")
                        .parse()
                        .expect("bad workers");
                }
                "--reader-poll" => {
                    a.reader_poll_us = it
                        .next()
                        .expect("--reader-poll needs us (0=event-driven)")
                        .parse()
                        .expect("bad reader-poll");
                }
                "--diag" => a.diag = true,
                other => eprintln!("ignoring unknown arg: {other}"),
            }
        }
        a
    }

    // ─── Echo server ────────────────────────────────────────────────────────
    //
    // Standard async echo: read from the KCP stream and write it back.

    async fn echo_loop(conn: KcpStream, _size: usize) {
        let mut buf = vec![0u8; 65536];
        loop {
            match conn.read(&mut buf).await {
                Ok(0) => return,
                Ok(n) => {
                    if conn.write_all(&buf[..n]).await.is_err() {
                        return;
                    }
                }
                Err(_) => return,
            }
        }
    }

    // ─── Measurement: open model (fixed RPS) ─────────────────────────────────

    fn count_shed_slots(
        first_slot: Instant,
        now: Instant,
        warmup_end: Instant,
        measure_end: Instant,
        interval: Duration,
    ) -> usize {
        let interval_ns = interval.as_nanos();
        if interval_ns == 0 {
            return 0;
        }
        let upper = now
            .checked_add(Duration::from_nanos(1))
            .unwrap_or(now)
            .min(measure_end);
        let lower = first_slot.max(warmup_end);
        if lower >= upper {
            return 0;
        }
        let delta_ns = lower.duration_since(first_slot).as_nanos();
        let steps = delta_ns.div_ceil(interval_ns);
        let offset_ns = steps.saturating_mul(interval_ns);
        let first = first_slot
            .checked_add(Duration::from_nanos(offset_ns as u64))
            .unwrap_or(upper);
        if first >= upper {
            return 0;
        }
        let span_ns = upper.duration_since(first).as_nanos();
        span_ns.div_ceil(interval_ns).min(usize::MAX as u128) as usize
    }

    /// Open-model fixed-rate latency run (runtime-agnostic).
    ///
    /// Splits into a **sender task** and a **reader task** to decouple send
    /// cadence from read latency. The sender normally advances by one fixed
    /// interval and sleeps between sends; lag beyond 50ms is resynchronized
    /// and every skipped measurement slot is reported as `shed`.
    ///
    /// The reader drains responses and matches them to send timestamps (FIFO,
    /// since KCP is an ordered stream). `reader_poll_us=0` uses event-driven
    /// reads; a positive value wraps each read in a bounded timeout. Go uses a
    /// 100µs `SetReadDeadline`, which is similar control flow but not an
    /// identical runtime scheduling mechanism.
    async fn run_open(
        conn: &mut KcpStream,
        rps: u32,
        warmup: Duration,
        duration: Duration,
        size: usize,
        reader_poll_us: u64,
    ) -> (Vec<f64>, usize, usize, usize, usize) {
        /// Lag past which the sender sheds its backlog instead of catching up.
        /// Must exceed timer granularity (tokio rounds up to 1ms) so ordinary
        /// wake jitter is absorbed, and stay well under a stalled-write
        /// timescale so a real block still sheds.
        const MAX_LAG: Duration = Duration::from_millis(50);

        let interval = Duration::from_secs_f64(1.0 / rps as f64);
        let payload = vec![0x5Au8; size];

        // Shared state between sender and reader.
        let in_flight: Arc<parking_lot::Mutex<VecDeque<(Instant, bool)>>> =
            Arc::new(parking_lot::Mutex::new(VecDeque::new()));
        let sends = Arc::new(AtomicUsize::new(0));
        let ok = Arc::new(AtomicUsize::new(0));
        let shed = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));

        let schedule_start = Instant::now();
        let warmup_end = schedule_start + warmup;
        let measure_end = warmup_end + duration;

        // ── Sender task: fires at `rps` on one timer-paced schedule ──
        let conn_tx = conn.clone();
        let inflight_tx = in_flight.clone();
        let sends_c = sends.clone();
        let shed_c = shed.clone();
        let stop_c = stop.clone();
        let sender = knet::spawn_task(async move {
            let mut next_send = schedule_start;
            loop {
                if stop_c.load(Ordering::Relaxed) {
                    break;
                }
                if next_send >= measure_end {
                    break;
                }
                let now = Instant::now();
                if now >= next_send {
                    let scheduled_at = next_send;
                    if conn_tx.write_all(&payload).await.is_err() {
                        break;
                    }
                    let sent_at = Instant::now();
                    let measuring = scheduled_at >= warmup_end;
                    inflight_tx.lock().push_back((sent_at, measuring));
                    if measuring {
                        sends_c.fetch_add(1, Ordering::Relaxed);
                    }
                    next_send += interval;
                    // Resync only on a *genuine* backlog. A blocked write (echo
                    // peer stalled on a full send window) makes lag compound
                    // without bound, and shedding is what keeps that from
                    // deadlocking; timer overshoot does not compound, so folding
                    // it into the same branch would discard throughput to
                    // measure an artifact. `shed` records real drops so the
                    // coordinated omission stays visible in the RESULT line.
                    let now = Instant::now();
                    if now.saturating_duration_since(next_send) > MAX_LAG {
                        // Count every scheduled slot skipped by the resync, but
                        // only slots inside the measurement window. This keeps
                        // shed an offered-load accounting field rather than a
                        // count of resynchronization events.
                        shed_c.fetch_add(
                            count_shed_slots(next_send, now, warmup_end, measure_end, interval),
                            Ordering::Relaxed,
                        );
                        next_send = now + interval;
                    }
                } else {
                    // Park instead of spinning: a `yield_now` loop burns ~45% of
                    // a core and starves the reader / KCP flush tasks sharing the
                    // runtime, which shows up as a multi-ms p99 that has nothing
                    // to do with the transport. tokio's timer rounds up to the
                    // next 1ms tick, so a 2ms interval routinely wakes late; the
                    // absolute `next_send` schedule absorbs that by firing
                    // back-to-back until caught up, holding the average rate.
                    knet::sleep(next_send - now).await;
                }
            }
        });

        // ── Reader (inline): drains responses ──
        // A positive timeout mirrors Go's bounded ReadDeadline control flow,
        // while zero keeps Rust event-driven. These are runtime-native wakeup
        // paths rather than identical polling implementations, so the result
        // includes probe/runtime scheduling as well as KCP processing.
        let mut rx = vec![0u8; size];
        let mut rx_filled = 0usize;
        let mut latencies: Vec<f64> = Vec::new();

        loop {
            if reader_poll_us == 0 {
                // Event-driven: 1s safety timeout only to check measure_end
                match knet::timeout(Duration::from_secs(1), conn.read(&mut rx[rx_filled..])).await {
                    Ok(Ok(0)) => break,
                    Ok(Ok(n)) => {
                        rx_filled += n;
                        while rx_filled >= size {
                            rx_filled -= size;
                            if let Some((t0, measuring)) = in_flight.lock().pop_front() {
                                if measuring {
                                    latencies.push(t0.elapsed().as_secs_f64() * 1e6);
                                    ok.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                        }
                    }
                    Ok(Err(_)) => break,
                    Err(_) => {} // 1s elapsed — just check measure_end
                }
            } else {
                match knet::timeout(
                    Duration::from_micros(reader_poll_us),
                    conn.read(&mut rx[rx_filled..]),
                )
                .await
                {
                    Ok(Ok(0)) => break,
                    Ok(Ok(n)) => {
                        rx_filled += n;
                        while rx_filled >= size {
                            rx_filled -= size;
                            if let Some((t0, measuring)) = in_flight.lock().pop_front() {
                                if measuring {
                                    latencies.push(t0.elapsed().as_secs_f64() * 1e6);
                                    ok.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                        }
                    }
                    Ok(Err(_)) => break,
                    Err(_) => {} // timeout elapsed with no data — continue polling
                }
            }

            if Instant::now() >= measure_end {
                break;
            }
        }

        stop.store(true, Ordering::Relaxed);
        let _ = knet::timeout(Duration::from_millis(200), sender).await;

        // Explicitly close the connection to unblock the reader's `conn.read()`.
        KcpStream::close(conn);

        let measure_sends = sends.load(Ordering::Relaxed);
        let measure_ok = ok.load(Ordering::Relaxed);
        let inflight_end = in_flight
            .lock()
            .iter()
            .filter(|(_, measuring)| *measuring)
            .count();
        (
            latencies,
            measure_sends,
            measure_ok,
            shed.load(Ordering::Relaxed),
            inflight_end,
        )
    }

    // ─── Measurement: closed-loop (max throughput) ───────────────────────────

    /// Closed-loop concurrency model: maintain exactly `concurrency` in-flight
    /// requests.  When a slot frees up (response received), immediately send
    /// the next.  Both implementations run at their own max sustainable speed,
    /// so throughput and latency are directly comparable.
    ///
    /// KCP is an ordered stream — responses arrive in send order, so we match
    /// each echo to the oldest in-flight request (FIFO).
    async fn run_closed_loop(
        conn: &mut KcpStream,
        concurrency: usize,
        warmup: Duration,
        duration: Duration,
        size: usize,
    ) -> (Vec<f64>, usize, usize, usize, usize) {
        let payload = vec![0x5Au8; size];
        let mut rx = vec![0u8; size];
        let mut rx_filled = 0usize;
        let mut in_flight: VecDeque<Instant> = VecDeque::new();
        let mut latencies: Vec<f64> = Vec::new();

        let warmup_end = Instant::now() + warmup;
        let measure_end = warmup_end + duration;
        let mut sends = 0usize;
        let mut ok = 0usize;

        loop {
            // Fill up to `concurrency` in-flight requests.
            while in_flight.len() < concurrency {
                if conn.write_all(&payload).await.is_err() {
                    break;
                }
                let sent_at = Instant::now();
                in_flight.push_back(sent_at);
                if sent_at >= warmup_end {
                    sends += 1;
                }
            }

            // Read one response chunk.
            match knet::timeout(Duration::from_secs(10), conn.read(&mut rx[rx_filled..])).await {
                Ok(Ok(0)) => break,
                Ok(Ok(n)) => {
                    rx_filled += n;
                    while rx_filled >= size {
                        // Complete one response → free one slot.
                        rx_filled -= size;
                        if let Some(t0) = in_flight.pop_front() {
                            if t0 >= warmup_end {
                                latencies.push(t0.elapsed().as_secs_f64() * 1e6);
                                ok += 1;
                            }
                        }
                    }
                }
                Ok(Err(_)) => break,
                Err(_) => break, // 10s timeout with no data → connection dead
            }

            let now = Instant::now();
            if now >= measure_end {
                break;
            }
        }
        let inflight_end = in_flight.iter().filter(|t0| **t0 >= warmup_end).count();
        (latencies, sends, ok, 0, inflight_end)
    }

    // ─── Statistics ──────────────────────────────────────────────────────────

    struct SampleStats {
        p50: f64,
        p90: f64,
        p99: f64,
        p999: f64,
        avg: f64,
        min: f64,
        max: f64,
    }

    /// Nearest-rank percentiles over a sorted list of microsecond samples.
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

    #[allow(clippy::too_many_arguments)]
    fn print_result(
        combo: &str,
        ok: usize,
        samples: usize,
        shed: Option<usize>,
        inflight_end: usize,
        size: usize,
        rps: u32,
        s: &SampleStats,
    ) {
        println!(
            "RESULT combo={combo} samples={samples} ok={ok} shed={} inflight_end={inflight_end} size={size} rps={rps} \
             p50_us={:.1} p90_us={:.1} p99_us={:.1} p999_us={:.1} \
             avg_us={:.1} min_us={:.1} max_us={:.1}",
            shed.map_or("na".to_string(), |v| v.to_string()),
            s.p50, s.p90, s.p99, s.p999, s.avg, s.min, s.max
        );
        println!(
            "  samples={samples} ok={ok} shed={} inflight_end={inflight_end} payload={size}B rps={rps}  p50={:.2}ms p90={:.2}ms \
             p99={:.2}ms p999={:.2}ms avg={:.2}ms min={:.2}ms max={:.2}ms",
            shed.map_or("na".to_string(), |v| v.to_string()),
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
        let (lat, sends, ok, shed, inflight_end) = if args.concurrency > 0 {
            run_closed_loop(
                conn,
                args.concurrency,
                Duration::from_secs(args.warmup),
                Duration::from_secs(args.duration),
                args.size,
            )
            .await
        } else {
            run_open(
                conn,
                args.rps,
                Duration::from_secs(args.warmup),
                Duration::from_secs(args.duration),
                args.size,
                args.reader_poll_us,
            )
            .await
        };
        // Actual throughput = completed requests / measurement duration.
        let actual_rps = if args.duration > 0 {
            (ok as f64 / args.duration as f64).round() as u32
        } else {
            args.rps
        };
        eprintln!(
            "[{combo}] warmup={}s duration={}s {} measure_sends={sends} measure_ok={ok} shed={} inflight_end={inflight_end}",
            args.warmup,
            args.duration,
            if args.concurrency > 0 {
                format!("concurrency={}", args.concurrency)
            } else {
                format!("rps={}", args.rps)
            },
            if args.concurrency > 0 {
                "na".to_string()
            } else {
                shed.to_string()
            },
        );
        if args.diag {
            let mut h2 = 0u32;
            let mut h5 = 0u32;
            let mut h10 = 0u32;
            let mut h20 = 0u32;
            for &l in &lat {
                if l > 2000.0 {
                    h2 += 1;
                }
                if l > 5000.0 {
                    h5 += 1;
                }
                if l > 10000.0 {
                    h10 += 1;
                }
                if l > 20000.0 {
                    h20 += 1;
                }
            }
            eprintln!(
                "HISTOGRAM >2ms={} >5ms={} >10ms={} >20ms={}",
                h2, h5, h10, h20
            );
        }
        let s = stats(lat);
        print_result(
            combo,
            ok,
            sends,
            if args.concurrency > 0 {
                None
            } else {
                Some(shed)
            },
            inflight_end,
            args.size,
            actual_rps,
            &s,
        );
        if args.snmp {
            let d = &kcp_rs::DEFAULT_SNMP;
            let g = |c: &std::sync::atomic::AtomicU64| c.load(Ordering::Relaxed);
            println!(
                "SNMP retrans={} fast_retrans={} early_retrans={} lost={} repeat={} \
                 in_pkts={} out_pkts={} in_segs={} out_segs={} in_errs={} \
                 read_fallback={}",
                g(&d.retrans_segs),
                g(&d.fast_retrans),
                g(&d.early_retrans),
                g(&d.lost_segs),
                g(&d.repeat_segs),
                g(&d.in_pkts),
                g(&d.out_pkts),
                g(&d.in_segs),
                g(&d.out_segs),
                g(&d.in_errs),
                d.read_fallback_timeout(),
            );
        }
    }

    // ─── Modes ───────────────────────────────────────────────────────────────

    /// Self mode: listener + connect + spawn echo, all in one process.
    ///
    /// Uses the high-level API:
    ///   1. `KcpListener::bind(addr).build().await` — create listener
    ///   2. `KcpStream::connect(addr).build().await` — dial the listener
    ///   3. `knet::spawn_task(echo_loop(conn, size))` — spawn echo server
    ///   4. Client measures round-trip latency
    async fn run_self(args: &Args) {
        // 1. Server: bind listener on a random localhost port.
        let listener = Arc::new(
            KcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
                .conv(args.conv)
                .mode(KcpMode::Fast3)
                .mtu(args.mtu)
                .sndwnd(SNDWND)
                .rcvwnd(RCVWND)
                .build()
                .await
                .unwrap(),
        );
        let addr = listener.local_addr().unwrap();

        // 2. Client: connect to the listener via the high-level API.
        let mut client = KcpStream::connect(addr)
            .conv(args.conv)
            .mode(KcpMode::Fast3)
            .mtu(args.mtu)
            .sndwnd(SNDWND)
            .rcvwnd(RCVWND)
            .build()
            .await
            .unwrap();

        // 3. Spawn the echo server: accept the client and echo back.
        let size = args.size;
        let accept_listener = listener.clone();
        let server_handle = knet::spawn_task(async move {
            if let Ok((conn, _peer)) = accept_listener.accept().await {
                echo_loop(conn, size).await;
            }
        });

        // 4. Measure round-trip latency (open model or closed loop).
        //    The warm-up phase doubles as the accept handshake.
        measure("rust-rust", &mut client, args).await;

        // Diagnostics: worker-channel drops convert into RTO-scale tail
        // latency, so surface them alongside the KCP SNMP counters.
        if args.snmp {
            let st = listener.stats();
            eprintln!(
                "LISTENER channel_drops={} session_drops={} build_failures={}",
                st.channel_drops, st.session_drops, st.build_failures
            );
        }

        client.close();
        drop(server_handle); // detach; the process exits right after
    }

    /// Server mode: listen and spawn an echo task per accepted connection.
    async fn run_server(args: &Args) {
        let listener = KcpListener::bind(SocketAddr::from(([127, 0, 0, 1], args.port)))
            .conv(args.conv)
            .mode(KcpMode::Fast3)
            .mtu(args.mtu)
            .sndwnd(SNDWND)
            .rcvwnd(RCVWND)
            .build()
            .await
            .unwrap();
        eprintln!(
            "kcp-rs echo server listening on {}",
            listener.local_addr().unwrap()
        );
        let size = args.size;
        while let Ok((conn, _peer)) = listener.accept().await {
            // Spawn one echo task per accepted connection.
            drop(knet::spawn_task(echo_loop(conn, size)));
        }
    }

    /// Peer mode: connect to an external echo server and measure.
    async fn run_peer(args: &Args) {
        let peer = args.peer.expect("peer mode requires --addr");
        let mut conn = KcpStream::connect(peer)
            .conv(args.conv)
            .mode(KcpMode::Fast3)
            .mtu(args.mtu)
            .sndwnd(SNDWND)
            .rcvwnd(RCVWND)
            .build()
            .await
            .expect("failed to connect KcpStream");
        measure("rust-go", &mut conn, args).await;
    }

    // ─── Entrypoint ──────────────────────────────────────────────────────────

    pub fn run() {
        let args = parse_args();
        if args.snmp {
            kcp_rs::snmp_enable();
        }

        let pprof_addr = args.pprof_addr.clone();
        let mode = args.mode.clone();
        let rt_single = args.rt_single;
        let workers = args.workers;
        let diag = args.diag;
        // Use single-threaded runtime to avoid cross-thread wake overhead
        let fut: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>> =
            Box::pin(async move {
                // Spawn watchdog if --diag is enabled
                if diag {
                    knet::spawn_task(async move {
                        let mut last = Instant::now();
                        loop {
                            knet::sleep(Duration::from_millis(1)).await;
                            let now = Instant::now();
                            let gap = now.duration_since(last);
                            last = now;
                            if gap > Duration::from_millis(2) {
                                eprintln!("WATCHDOG SCHED_GAP={}us", gap.as_micros());
                            }
                        }
                    });
                }
                // Start pprof HTTP server if --pprof is specified (requires --features pprof).
                #[cfg(feature = "pprof")]
                if let Some(ref addr) = pprof_addr {
                    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
                    let pprof_bind = addr.clone();
                    knet::spawn_task(async move {
                        let _ = kpprof::run_pprof(&pprof_bind, stop).await;
                    });
                    eprintln!(
                        "[pprof] HTTP server on {} (endpoints: /debug/pprof/profile?seconds=N, /debug/pprof/heap)",
                        addr
                    );
                }
                #[cfg(not(feature = "pprof"))]
                if pprof_addr.is_some() {
                    eprintln!("[pprof] --pprof requested but binary built without `pprof` feature; rebuild with --features pprof");
                }

                match mode.as_str() {
                    "peer" => run_peer(&args).await,
                    "server" => run_server(&args).await,
                    _ => run_self(&args).await,
                }
            });
        block_future(rt_single, workers, fut);
    }

    /// `--rt single` uses a current-thread tokio runtime (diagnostic: avoids
    /// cross-thread task wakes; with `--rt multi` (default) the shared
    /// multi-thread runtime is used, as in kcptun production.
    #[cfg(feature = "async")]
    fn block_future(
        single: bool,
        workers: usize,
        fut: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>>,
    ) {
        if single {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_io()
                .enable_time()
                .build()
                .expect("failed to build current-thread runtime");
            rt.block_on(fut);
        } else if workers > 0 {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(workers)
                .enable_all()
                .build()
                .expect("failed to create tokio runtime");
            rt.block_on(fut);
        } else {
            knet::block_on(fut);
        }
    }

    #[cfg(not(feature = "async"))]
    fn block_future(
        _single: bool,
        _workers: usize,
        fut: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>,
    ) {
        knet::block_on(fut);
    }
}

// Use the profiling allocator when pprof is enabled (required for heap profiles).
#[cfg(feature = "pprof")]
#[global_allocator]
static GLOBAL: kpprof::ProfilingAllocator = kpprof::ProfilingAllocator::new();

#[cfg(any(feature = "async", feature = "async", feature = "async"))]
fn main() {
    probe::run();
}

#[cfg(not(any(feature = "async", feature = "async", feature = "async")))]
fn main() {
    eprintln!(
        "error: this example requires the `async`, `async`, or `async` feature, e.g.\n\
         cargo run -p kcp-rs --features async --example latency_p99 -- --mode self"
    );
    std::process::exit(2);
}
