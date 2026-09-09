//! SMUX standalone performance benchmark — directly tests Session hot paths.
//!
//! Two Sessions are connected via an in-memory pipe. The driver drains outbound
//! from the client session and feeds it as inbound to the server session.
//! This isolates smux-rs performance from TCP/socket overhead.
//!
//! Hot paths exercised:
//!   - Session::prepare_outbound_into (write path: stream drain + frame encode)
//!   - Session::process_data (read path: frame decode + stream dispatch)
//!   - Stream::write_bytes / Stream::read (per-stream I/O)
//!
//! Usage:
//!   cargo run --release --example smux_bench -- [--streams N] [--size B] [--duration S]
//!   cargo run --release --example smux_bench -- --latency --size 1024 --duration 10

use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use clap::Parser;

use smux_rs::{Config, Session, DEFAULT_CONFIG};

/// Benchmark configuration.
#[derive(Parser, Debug)]
struct Args {
    /// Number of concurrent SMUX streams.
    #[arg(long, default_value = "4")]
    streams: usize,

    /// Payload size per write (bytes).
    #[arg(long, default_value = "65536")]
    size: usize,

    /// Test duration in seconds.
    #[arg(long, default_value = "5")]
    duration: u64,

    /// SMUX version (1 or 2).
    #[arg(long, default_value = "2")]
    version: u8,

    /// Max frame size.
    #[arg(long, default_value = "16384")]
    frame_size: usize,

    /// Run latency test instead of throughput.
    #[arg(long)]
    latency: bool,
}

/// Drive data from writer session to reader session.
/// Drains outbound from `writer`, feeds as inbound to `reader`.
fn pump(writer: &Session, reader: &Session, ver: u8) {
    let mut buf = BytesMut::with_capacity(128 * 1024);
    let _ = writer.prepare_outbound_into(&mut buf, 128 * 1024, ver);
    if !buf.is_empty() {
        let _ = reader.process_data(&buf);
    }
}

// ─── Throughput test ─────────────────────────────────────────────────────────
//
// Measures one-way throughput: client writes data → pump → server reads data.
// Each iteration: write N bytes on each client stream, pump all data through,
// read N bytes on each server stream. Total bytes = N * streams per iteration.

fn run_throughput(args: &Args) {
    let cfg = Config {
        version: args.version,
        max_frame_size: args.frame_size,
        ..DEFAULT_CONFIG.clone()
    };

    let client = Session::new_client(&cfg).unwrap();
    let server = Session::new_server(&cfg).unwrap();
    let ver = cfg.version;

    // Open streams on client side and queue SYN frames.
    let client_streams: Vec<_> = (0..args.streams)
        .map(|_| {
            let s = client.open_stream().unwrap();
            client.queue_syn(s.id());
            s
        })
        .collect();

    // Pump SYNs through so server creates the streams.
    for _ in 0..10 {
        pump(&client, &server, ver);
    }

    // Collect server-side stream handles.
    let server_streams: Vec<_> = {
        let map = server.streams();
        let guard = map.lock();
        let mut ids: Vec<u32> = guard.keys().copied().collect();
        ids.sort();
        ids.iter().filter_map(|id| guard.get(id).cloned()).collect()
    };
    assert_eq!(
        server_streams.len(),
        args.streams,
        "server should have {} streams",
        args.streams
    );

    let payload = Bytes::from(vec![0xABu8; args.size]);

    // Warmup: one full round-trip.
    for s in &client_streams {
        s.write_bytes(payload.clone()).unwrap();
    }
    for _ in 0..20 {
        pump(&client, &server, ver);
    }
    let mut tmp = vec![0u8; args.size];
    for s in &server_streams {
        let _ = s.read(&mut tmp);
    }

    // Measurement.
    let test_end = Instant::now() + Duration::from_secs(args.duration);
    let mut total_bytes: u64 = 0;
    let mut iterations: u64 = 0;

    while Instant::now() < test_end {
        // Write on all client streams.
        for s in &client_streams {
            s.write_bytes(payload.clone()).unwrap();
        }

        // Pump all data through (drain client → feed server).
        // Keep pumping until client has no pending send data.
        for _ in 0..50 {
            pump(&client, &server, ver);
            if client_streams.iter().all(|s| s.pending_send() == 0) {
                break;
            }
        }

        // Read on all server streams.
        for s in &server_streams {
            let mut got = 0;
            while got < args.size {
                match s.read(&mut tmp[got..]) {
                    Ok((n, _)) => got += n,
                    Err(_) => break,
                }
            }
            total_bytes += got as u64;
        }
        iterations += 1;
    }

    let elapsed = args.duration as f64;
    let mbps = (total_bytes as f64 / 1_048_576.0) / elapsed;
    let iters_per_sec = iterations as f64 / elapsed;

    println!(
        "RESULT mode=throughput streams={} size={} version={} frame_size={} \
         mbps={:.2} iters_per_sec={:.1} total_bytes={}",
        args.streams, args.size, args.version, args.frame_size, mbps, iters_per_sec, total_bytes
    );
    eprintln!(
        "Throughput: {:.2} MB/s | {:.1} iters/s | streams={} size={}B ver={} frame={}B",
        mbps, iters_per_sec, args.streams, args.size, args.version, args.frame_size
    );
}

// ─── Latency test ────────────────────────────────────────────────────────────
//
// Measures round-trip latency: write 1 byte chunk on client stream, pump
// through to server, read on server, write echo back, pump to client, read
// on client. Measures full RTT through the SMUX stack.

fn run_latency(args: &Args) {
    let cfg = Config {
        version: args.version,
        max_frame_size: args.frame_size,
        ..DEFAULT_CONFIG.clone()
    };

    let client = Session::new_client(&cfg).unwrap();
    let server = Session::new_server(&cfg).unwrap();
    let ver = cfg.version;

    let client_stream = client.open_stream().unwrap();
    client.queue_syn(client_stream.id());

    // Pump SYN through.
    for _ in 0..10 {
        pump(&client, &server, ver);
    }

    let server_stream = {
        let map = server.streams();
        let guard = map.lock();
        guard.values().next().cloned().unwrap()
    };

    let payload = Bytes::from(vec![0xCDu8; args.size]);
    let mut recv_buf = vec![0u8; args.size];

    // Warmup.
    for _ in 0..50 {
        client_stream.write_bytes(payload.clone()).unwrap();
        for _ in 0..20 {
            pump(&client, &server, ver);
        }
        let _ = server_stream.read(&mut recv_buf);
    }

    // Measure round-trip: write → pump → read (server) → write echo → pump → read (client).
    let test_end = Instant::now() + Duration::from_secs(args.duration);
    let mut latencies_us: Vec<f64> = Vec::new();

    while Instant::now() < test_end {
        let t0 = Instant::now();

        // Client writes.
        client_stream.write_bytes(payload.clone()).unwrap();

        // Pump to server.
        for _ in 0..20 {
            pump(&client, &server, ver);
        }

        // Server reads.
        let _ = server_stream.read(&mut recv_buf);

        // Server echoes back.
        server_stream.write_bytes(payload.clone()).unwrap();

        // Pump to client.
        for _ in 0..20 {
            pump(&server, &client, ver);
        }

        // Client reads echo.
        let _ = client_stream.read(&mut recv_buf);

        let dt = t0.elapsed().as_secs_f64() * 1_000_000.0;
        latencies_us.push(dt);
    }

    if latencies_us.is_empty() {
        println!("RESULT mode=latency error=no_data");
        return;
    }

    latencies_us.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = latencies_us.len();
    let p50 = latencies_us[n / 2];
    let p99 = latencies_us[(n as f64 * 0.99) as usize];
    let p999 = latencies_us[(n as f64 * 0.999) as usize];
    let avg = latencies_us.iter().sum::<f64>() / n as f64;

    println!(
        "RESULT mode=latency size={} version={} frame_size={} \
         p50={:.1} p99={:.1} p999={:.1} avg={:.1} samples={}",
        args.size, args.version, args.frame_size, p50, p99, p999, avg, n
    );
    eprintln!(
        "Latency: p50={:.1}µs p99={:.1}µs p999={:.1}µs avg={:.1}µs | \
         size={}B ver={} frame={}B samples={}",
        p50, p99, p999, avg, args.size, args.version, args.frame_size, n
    );
}

fn main() {
    let args = Args::parse();

    if args.latency {
        run_latency(&args);
    } else {
        run_throughput(&args);
    }
}
