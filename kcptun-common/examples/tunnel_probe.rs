//! Low-noise TCP probe for an already-running kcptun client listener.
//!
//! This deliberately uses only the standard library: the load generator must
//! not share an async runtime with the system being measured. Each worker owns
//! one blocking request at a time and reuses its TCP/SMUX stream. The
//! synchronous handoff drops an offered tick unless a worker is idle, so
//! `workers` is also the exact concurrency limit. Reuse is deliberate: a
//! connect-per-request driver exhausts macOS ephemeral ports long before it
//! reaches an I/O runtime's sustainable rate.
//!
//! Usage: `tunnel_probe <port> <rps> <size> <warmup-s> <duration-s> [workers]`

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::mpsc::{self, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Default)]
struct Samples {
    rtt_us: Vec<f64>,
    queue_us: Vec<f64>,
    offered: usize,
    sent: usize,
    dropped: usize,
    driver_late: usize,
    failed: usize,
    errors: BTreeMap<String, usize>,
}

fn percentile(values: &mut [f64], q: f64) -> f64 {
    values.sort_by(|a, b| a.total_cmp(b));
    values[((values.len() as f64 * q).ceil() as usize).min(values.len()) - 1]
}

fn connect(addr: SocketAddr) -> std::io::Result<TcpStream> {
    let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(10))?;
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    Ok(stream)
}

fn request(stream: &mut TcpStream, payload: &[u8]) -> std::io::Result<f64> {
    let started = Instant::now();
    stream.write_all(payload)?;
    let mut received = 0usize;
    let mut buf = vec![0u8; payload.len()];
    while received < payload.len() {
        let n = stream.read(&mut buf[received..])?;
        if n == 0 {
            return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof));
        }
        received += n;
    }
    Ok(started.elapsed().as_secs_f64() * 1e6)
}

fn run_phase(
    addr: SocketAddr,
    rps: u32,
    size: usize,
    warmup: Duration,
    duration: Duration,
    workers: usize,
) -> Samples {
    // Permits make `workers` the exact in-flight cap. Unlike a zero-capacity
    // rendezvous channel, they do not discard a healthy arrival just because a
    // receiver is a few microseconds away from parking in `recv`.
    let (permit_tx, permit_rx) = mpsc::sync_channel::<()>(workers);
    for _ in 0..workers {
        permit_tx.send(()).expect("initial probe permit");
    }
    let (tx, rx) = mpsc::sync_channel::<(Instant, bool)>(workers);
    let rx = Arc::new(Mutex::new(rx));
    let samples = Arc::new(Mutex::new(Samples::default()));
    let payload = Arc::new(vec![b'X'; size]);
    let mut handles = Vec::with_capacity(workers);
    for _ in 0..workers {
        let rx = rx.clone();
        let samples = samples.clone();
        let payload = payload.clone();
        let permit_tx = permit_tx.clone();
        handles.push(thread::spawn(move || {
            let mut stream = None;
            loop {
                let (scheduled, record) = match rx.lock().unwrap().recv() {
                    Ok(value) => value,
                    Err(_) => return,
                };
                let queue_us = scheduled.elapsed().as_secs_f64() * 1e6;
                let result = match stream.as_mut() {
                    Some(existing) => request(existing, &payload),
                    None => match connect(addr) {
                        Ok(mut new_stream) => {
                            let result = request(&mut new_stream, &payload);
                            stream = Some(new_stream);
                            result
                        }
                        Err(error) => Err(error),
                    },
                };
                let failed = result.is_err();
                if record {
                    let mut out = samples.lock().unwrap();
                    out.queue_us.push(queue_us);
                    match result {
                        Ok(rtt_us) => out.rtt_us.push(rtt_us),
                        Err(error) => {
                            out.failed += 1;
                            let kind = error.kind().to_string().replace(' ', "_");
                            *out.errors.entry(kind).or_default() += 1;
                        }
                    }
                }
                if failed {
                    stream = None;
                }
                permit_tx.send(()).expect("probe permit receiver dropped");
            }
        }));
    }

    let interval = Duration::from_secs_f64(1.0 / rps as f64);
    let recording_starts = Instant::now() + warmup;
    let end = recording_starts + duration;
    let mut next = Instant::now();
    let mut offered = 0usize;
    let mut sent = 0usize;
    let mut dropped = 0usize;
    let mut driver_late = 0usize;
    while Instant::now() < end {
        let now = Instant::now();
        if now >= next {
            let record = now >= recording_starts;
            if record {
                offered += 1;
            }
            match permit_rx.try_recv() {
                Ok(()) => match tx.try_send((now, record)) {
                    Ok(()) => {
                        if record {
                            sent += 1;
                        }
                    }
                    // A permit guarantees capacity unless a worker panicked.
                    Err(TrySendError::Full(_)) => {
                        permit_tx.send(()).expect("probe permit receiver dropped")
                    }
                    Err(TrySendError::Disconnected(_)) => break,
                },
                Err(TryRecvError::Empty) => {
                    if record {
                        dropped += 1;
                    }
                }
                Err(TryRecvError::Disconnected) => break,
            }
            next += interval;
            if next < now {
                if record {
                    driver_late += (now.duration_since(next).as_secs_f64() / interval.as_secs_f64())
                        .floor() as usize
                        + 1;
                }
                next = now + interval;
            }
        }
        let wait = next.saturating_duration_since(Instant::now());
        if !wait.is_zero() {
            thread::sleep(wait.min(Duration::from_micros(200)));
        }
    }
    drop(tx);
    for handle in handles {
        handle.join().expect("probe worker panicked");
    }
    let mut out = Arc::try_unwrap(samples).unwrap().into_inner().unwrap();
    out.offered = offered;
    out.sent = sent;
    out.dropped = dropped;
    out.driver_late = driver_late;
    out
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 6 {
        eprintln!("usage: tunnel_probe <port> <rps> <size> <warmup-s> <duration-s> [workers]");
        std::process::exit(2);
    }
    let port: u16 = args[1].parse().expect("bad port");
    let rps: u32 = args[2].parse().expect("bad rps");
    let size: usize = args[3].parse().expect("bad size");
    let warmup: u64 = args[4].parse().expect("bad warmup");
    let duration: u64 = args[5].parse().expect("bad duration");
    let workers = args.get(6).and_then(|v| v.parse().ok()).unwrap_or(32usize);
    let label = std::env::var("PROBE_LABEL").unwrap_or_default();
    assert!(
        rps > 0 && size > 0 && workers > 0,
        "rps, size and workers must be positive"
    );
    let addr = SocketAddr::from(([127, 0, 0, 1], port));

    let mut measured = run_phase(
        addr,
        rps,
        size,
        Duration::from_secs(warmup),
        Duration::from_secs(duration),
        workers,
    );
    if measured.rtt_us.is_empty() {
        println!(
            "RESULT label={label} probe=rust samples=0 ok=0 failed={} offered={} sent={} dropped={} driver_late={} errors={} FAILED",
            measured.failed,
            measured.offered,
            measured.sent,
            measured.dropped,
            measured.driver_late,
            summarize_errors(&measured.errors),
        );
        std::process::exit(1);
    }
    let count = measured.rtt_us.len();
    let avg = measured.rtt_us.iter().sum::<f64>() / count as f64;
    let p50 = percentile(&mut measured.rtt_us, 0.50);
    let p90 = percentile(&mut measured.rtt_us, 0.90);
    let p99 = percentile(&mut measured.rtt_us, 0.99);
    let p999 = percentile(&mut measured.rtt_us, 0.999);
    let max = *measured.rtt_us.last().unwrap();
    let min = measured.rtt_us[0];
    let queue_p999 = percentile(&mut measured.queue_us, 0.999);
    let queue_max = *measured.queue_us.last().unwrap();
    let errors = summarize_errors(&measured.errors);
    println!(
        "RESULT label={label} probe=rust samples={count} ok={count} failed={} offered={} sent={} dropped={} driver_late={} errors={errors} size={size} rps={rps} p50_us={p50:.1} p90_us={p90:.1} p99_us={p99:.1} p999_us={p999:.1} avg_us={avg:.1} min_us={min:.1} max_us={max:.1} queue_p999_us={queue_p999:.1} queue_max_us={queue_max:.1}",
        measured.failed,
        measured.offered,
        measured.sent,
        measured.dropped,
        measured.driver_late,
    );
    if measured.failed > 0 {
        std::process::exit(1);
    }
}

fn summarize_errors(errors: &BTreeMap<String, usize>) -> String {
    if errors.is_empty() {
        "none".to_owned()
    } else {
        errors
            .iter()
            .map(|(kind, count)| format!("{kind}:{count}"))
            .collect::<Vec<_>>()
            .join(",")
    }
}
