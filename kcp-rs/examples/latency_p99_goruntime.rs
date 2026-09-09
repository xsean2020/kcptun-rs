//! PureGoruntime P99/P999 latency probe — raw KCP + std::net::UdpSocket + thread pool.
//!
//! No tokio, no async runtime. Uses kcp_rs::KCP directly with a thread pool
//! of N=num_cpus threads, mimicking Go's GOMAXPROCS.
//!
//! Usage:
//!   cargo run --release --example latency_p99_goruntime -- --mode self
//!   cargo run --release --example latency_p99_goruntime -- --mode server --port 39001
//!   cargo run --release --example latency_p99_goruntime -- --mode peer --addr 127.0.0.1:39001

use std::collections::VecDeque;
use std::env;
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use kcp_rs::kcp::KcpError;
use kcp_rs::KCP;

// ─── KCP Fast3 profile ──────────────────────────────────────────────────────
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
    concurrency: usize,
    workers: usize,
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
        concurrency: 0,
        workers: 0,
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
            "--concurrency" | "-c" => {
                a.concurrency = it
                    .next()
                    .expect("--concurrency needs n")
                    .parse()
                    .expect("bad concurrency")
            }
            "--workers" => {
                a.workers = it
                    .next()
                    .expect("--workers needs n")
                    .parse()
                    .expect("bad workers")
            }
            other => eprintln!("ignoring unknown arg: {other}"),
        }
    }
    a
}

// ─── Thread-safe KCP wrapper ─────────────────────────────────────────────────

struct SharedKcp {
    kcp: Mutex<KCP>,
    socket: Arc<UdpSocket>,
    peer: Mutex<Option<SocketAddr>>,
    out_queue: Arc<Mutex<VecDeque<Vec<u8>>>>,
}

impl SharedKcp {
    fn new(conv: u32, socket: Arc<UdpSocket>, peer: Option<SocketAddr>) -> Arc<Self> {
        let out_queue = Arc::new(Mutex::new(VecDeque::new()));
        let out_queue_clone = out_queue.clone();
        let mut kcp = KCP::new(conv, 0, move |data: bytes::Bytes| {
            out_queue_clone.lock().unwrap().push_back(data.to_vec());
        });
        kcp.set_nodelay(1, 10, 2, 1); // Fast3
        kcp.set_mtu(MTU);
        kcp.set_snd_wnd(SNDWND);
        kcp.set_rcv_wnd(RCVWND);
        kcp.set_stream_mode(true);

        Arc::new(Self {
            kcp: Mutex::new(kcp),
            socket,
            peer: Mutex::new(peer),
            out_queue,
        })
    }

    fn send(&self, data: &[u8]) -> Result<(), KcpError> {
        self.kcp.lock().unwrap().send(data)
    }

    fn recv(&self) -> Result<bytes::BytesMut, KcpError> {
        self.kcp.lock().unwrap().recv()
    }

    fn input(&self, data: &[u8]) -> Result<usize, KcpError> {
        self.kcp.lock().unwrap().input(data, false)
    }

    fn update(&self, current: u32) -> u32 {
        self.kcp.lock().unwrap().update(current)
    }

    fn flush_udp(&self) {
        let peer = self.peer.lock().unwrap();
        let mut q = self.out_queue.lock().unwrap();
        while let Some(pkt) = q.pop_front() {
            if let Some(addr) = *peer {
                let _ = self.socket.send_to(&pkt, addr);
            }
        }
    }

    fn can_recv(&self) -> bool {
        self.kcp.lock().unwrap().can_recv()
    }
}

fn current_ms() -> u32 {
    // KCP::current_ms is an instance method; use a static approach
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    (now.as_millis() as u64) as u32
}

// ─── Echo server (thread-based) ──────────────────────────────────────────────

fn echo_loop(conn: Arc<SharedKcp>, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Relaxed) {
        let now = current_ms();
        conn.update(now);
        conn.flush_udp();

        while conn.can_recv() {
            match conn.recv() {
                Ok(data) => {
                    if !data.is_empty() {
                        let _ = conn.send(&data);
                    }
                }
                Err(_) => return,
            }
        }
        thread::sleep(Duration::from_micros(50));
    }
}

// ─── UDP receive loop ────────────────────────────────────────────────────────

fn udp_recv_loop(socket: Arc<UdpSocket>, conn: Arc<SharedKcp>, stop: Arc<AtomicBool>) {
    let mut buf = vec![0u8; 65536];
    socket.set_nonblocking(true).ok();

    while !stop.load(Ordering::Relaxed) {
        match socket.recv_from(&mut buf) {
            Ok((n, addr)) => {
                {
                    let mut peer = conn.peer.lock().unwrap();
                    if peer.is_none() {
                        *peer = Some(addr);
                    }
                }
                let _ = conn.input(&buf[..n]);
            }
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_micros(50));
            }
            Err(_) => {
                thread::sleep(Duration::from_micros(100));
            }
        }
    }
}

// ─── Statistics ──────────────────────────────────────────────────────────────

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
    if v.is_empty() {
        return SampleStats {
            p50: 0.0,
            p90: 0.0,
            p99: 0.0,
            p999: 0.0,
            avg: 0.0,
            min: 0.0,
            max: 0.0,
        };
    }
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

fn print_result(combo: &str, ok: usize, samples: usize, size: usize, rps: u32, s: &SampleStats) {
    println!(
        "RESULT combo={combo} samples={samples} ok={ok} size={size} rps={rps} \
         p50_us={:.1} p90_us={:.1} p99_us={:.1} p999_us={:.1} \
         avg_us={:.1} min_us={:.1} max_us={:.1}",
        s.p50, s.p90, s.p99, s.p999, s.avg, s.min, s.max
    );
    println!(
        "  p50={:.2}ms p90={:.2}ms p99={:.2}ms p999={:.2}ms avg={:.2}ms min={:.2}ms max={:.2}ms",
        s.p50 / 1000.0,
        s.p90 / 1000.0,
        s.p99 / 1000.0,
        s.p999 / 1000.0,
        s.avg / 1000.0,
        s.min / 1000.0,
        s.max / 1000.0,
    );
}

// ─── Open-model measurement ──────────────────────────────────────────────────

fn run_open(
    conn: Arc<SharedKcp>,
    rps: u32,
    warmup: Duration,
    duration: Duration,
    size: usize,
) -> (Vec<f64>, usize, usize) {
    let interval = Duration::from_secs_f64(1.0 / rps as f64);
    let payload = vec![0x5Au8; size];

    let in_flight: Arc<Mutex<VecDeque<(Instant, bool)>>> = Arc::new(Mutex::new(VecDeque::new()));
    let sends = Arc::new(AtomicUsize::new(0));
    let ok = Arc::new(AtomicUsize::new(0));
    let stop = Arc::new(AtomicBool::new(false));

    let warmup_end = Instant::now() + warmup;
    let measure_end = warmup_end + duration;

    // Sender thread
    let conn_tx = conn.clone();
    let inflight_tx = in_flight.clone();
    let sends_c = sends.clone();
    let stop_c = stop.clone();
    let sender = thread::spawn(move || {
        let mut next_send = Instant::now();
        loop {
            if stop_c.load(Ordering::Relaxed) {
                break;
            }
            let now = Instant::now();
            if now >= next_send {
                if conn_tx.send(&payload).is_err() {
                    break;
                }
                let sent_at = Instant::now();
                let measuring = sent_at >= warmup_end;
                inflight_tx.lock().unwrap().push_back((sent_at, measuring));
                if measuring {
                    sends_c.fetch_add(1, Ordering::Relaxed);
                }
                next_send += interval;
                if next_send < Instant::now() {
                    next_send = Instant::now() + interval;
                }
            } else {
                thread::sleep(next_send - now);
            }
        }
    });

    // Reader + KCP update loop
    let mut latencies: Vec<f64> = Vec::new();
    let mut rx = vec![0u8; size];
    let mut rx_filled = 0usize;

    loop {
        let now = current_ms();
        conn.update(now);
        conn.flush_udp();

        while conn.can_recv() {
            match conn.recv() {
                Ok(data) => {
                    let data_len = data.len();
                    if rx_filled + data_len <= rx.len() {
                        rx[rx_filled..rx_filled + data_len].copy_from_slice(&data);
                        rx_filled += data_len;
                    } else {
                        rx = data.to_vec();
                        rx_filled = rx.len();
                    }
                    while rx_filled >= size {
                        rx_filled -= size;
                        if let Some((t0, measuring)) = in_flight.lock().unwrap().pop_front() {
                            if measuring {
                                latencies.push(t0.elapsed().as_secs_f64() * 1e6);
                                ok.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                }
                Err(_) => break,
            }
        }

        if Instant::now() >= measure_end {
            break;
        }
        thread::sleep(Duration::from_micros(50));
    }

    stop.store(true, Ordering::Relaxed);
    let _ = sender.join();

    (
        latencies,
        sends.load(Ordering::Relaxed),
        ok.load(Ordering::Relaxed),
    )
}

// ─── Closed-loop measurement ─────────────────────────────────────────────────

fn run_closed_loop(
    conn: Arc<SharedKcp>,
    concurrency: usize,
    warmup: Duration,
    duration: Duration,
    size: usize,
) -> (Vec<f64>, usize, usize) {
    let payload = vec![0x5Au8; size];
    let mut in_flight: VecDeque<Instant> = VecDeque::new();
    let mut latencies: Vec<f64> = Vec::new();
    let warmup_end = Instant::now() + warmup;
    let measure_end = warmup_end + duration;
    let mut sends = 0usize;
    let mut ok = 0usize;

    loop {
        while in_flight.len() < concurrency {
            if conn.send(&payload).is_err() {
                break;
            }
            let sent_at = Instant::now();
            in_flight.push_back(sent_at);
            if sent_at >= warmup_end {
                sends += 1;
            }
        }

        let now = current_ms();
        conn.update(now);
        conn.flush_udp();

        while conn.can_recv() {
            match conn.recv() {
                Ok(data) => {
                    if data.len() >= size {
                        if let Some(t0) = in_flight.pop_front() {
                            if t0 >= warmup_end {
                                latencies.push(t0.elapsed().as_secs_f64() * 1e6);
                                ok += 1;
                            }
                        }
                    }
                }
                Err(_) => break,
            }
        }

        if Instant::now() >= measure_end {
            break;
        }
        thread::sleep(Duration::from_micros(50));
    }
    (latencies, sends, ok)
}

// ─── Modes ─────────────────────────────────*──────────────────────────────────

fn run_self(args: &Args) {
    let n_workers = if args.workers > 0 {
        args.workers
    } else {
        num_cpus::get()
    };
    eprintln!(
        "[goruntime] workers={n_workers} (num_cpus={})",
        num_cpus::get()
    );

    let server_sock = UdpSocket::bind("127.0.0.1:0").expect("bind server");
    let server_addr = server_sock.local_addr().unwrap();
    server_sock.set_nonblocking(true).ok();
    let server_sock = Arc::new(server_sock);

    let client_sock = UdpSocket::bind("127.0.0.1:0").expect("bind client");
    client_sock.set_nonblocking(true).ok();
    let client_sock = Arc::new(client_sock);

    let server_kcp = SharedKcp::new(args.conv, server_sock.clone(), Some(server_addr));
    let client_kcp = SharedKcp::new(args.conv, client_sock.clone(), Some(server_addr));

    let stop = Arc::new(AtomicBool::new(false));

    // Echo server thread
    let echo_kcp = server_kcp.clone();
    let echo_stop = stop.clone();
    thread::spawn(move || echo_loop(echo_kcp, echo_stop));

    // UDP recv threads
    let srv_recv_kcp = server_kcp.clone();
    let srv_recv_stop = stop.clone();
    let srv_recv_sock = server_sock.clone();
    thread::spawn(move || udp_recv_loop(srv_recv_sock, srv_recv_kcp, srv_recv_stop));

    let cli_recv_kcp = client_kcp.clone();
    let cli_recv_stop = stop.clone();
    let cli_recv_sock = client_sock.clone();
    thread::spawn(move || udp_recv_loop(cli_recv_sock, cli_recv_kcp, cli_recv_stop));

    eprintln!("[goruntime] warmup {}s...", args.warmup);
    thread::sleep(Duration::from_secs(args.warmup));

    let (lat, sends, ok) = if args.concurrency > 0 {
        run_closed_loop(
            client_kcp.clone(),
            args.concurrency,
            Duration::from_secs(0),
            Duration::from_secs(args.duration),
            args.size,
        )
    } else {
        run_open(
            client_kcp.clone(),
            args.rps,
            Duration::from_secs(0),
            Duration::from_secs(args.duration),
            args.size,
        )
    };

    stop.store(true, Ordering::Relaxed);

    let actual_rps = if args.duration > 0 {
        (ok as f64 / args.duration as f64).round() as u32
    } else {
        args.rps
    };
    eprintln!("[goruntime] sends={sends} ok={ok} workers={n_workers}");
    let s = stats(lat);
    print_result("goruntime-goruntime", ok, sends, args.size, actual_rps, &s);
}

fn run_server(args: &Args) {
    let addr = format!("127.0.0.1:{}", args.port);
    let sock = UdpSocket::bind(&addr).expect("bind server");
    eprintln!(
        "[goruntime] echo server listening on {}",
        sock.local_addr().unwrap()
    );
    sock.set_nonblocking(true).ok();
    let sock = Arc::new(sock);

    let kcp = SharedKcp::new(args.conv, sock.clone(), None);
    let stop = Arc::new(AtomicBool::new(false));

    let echo_kcp = kcp.clone();
    let echo_stop = stop.clone();
    thread::spawn(move || echo_loop(echo_kcp, echo_stop));

    let recv_kcp = kcp.clone();
    let recv_stop = stop.clone();
    let recv_sock = sock.clone();
    thread::spawn(move || udp_recv_loop(recv_sock, recv_kcp, recv_stop));

    while !stop.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_secs(1));
    }
}

fn run_peer(args: &Args) {
    let peer = args.peer.expect("peer mode requires --addr");
    let sock = UdpSocket::bind("127.0.0.1:0").expect("bind client");
    sock.set_nonblocking(true).ok();
    let sock = Arc::new(sock);

    let kcp = SharedKcp::new(args.conv, sock.clone(), Some(peer));
    let stop = Arc::new(AtomicBool::new(false));

    let recv_kcp = kcp.clone();
    let recv_stop = stop.clone();
    let recv_sock = sock.clone();
    thread::spawn(move || udp_recv_loop(recv_sock, recv_kcp, recv_stop));

    eprintln!("[goruntime] warmup {}s...", args.warmup);
    thread::sleep(Duration::from_secs(args.warmup));

    let (lat, sends, ok) = if args.concurrency > 0 {
        run_closed_loop(
            kcp.clone(),
            args.concurrency,
            Duration::from_secs(0),
            Duration::from_secs(args.duration),
            args.size,
        )
    } else {
        run_open(
            kcp.clone(),
            args.rps,
            Duration::from_secs(0),
            Duration::from_secs(args.duration),
            args.size,
        )
    };

    stop.store(true, Ordering::Relaxed);

    let actual_rps = if args.duration > 0 {
        (ok as f64 / args.duration as f64).round() as u32
    } else {
        args.rps
    };
    eprintln!("[goruntime] sends={sends} ok={ok}");
    let s = stats(lat);
    print_result("goruntime-peer", ok, sends, args.size, actual_rps, &s);
}

// ─── Main ────────────────────────────────────────────────────────────────────

fn main() {
    let args = parse_args();
    eprintln!(
        "[goruntime] mode={} conv=0x{:08X} size={}B rps={} warmup={}s duration={}s workers={}",
        args.mode,
        args.conv,
        args.size,
        args.rps,
        args.warmup,
        args.duration,
        if args.workers > 0 {
            args.workers
        } else {
            num_cpus::get()
        },
    );

    match args.mode.as_str() {
        "peer" => run_peer(&args),
        "server" => run_server(&args),
        _ => run_self(&args),
    }
}
