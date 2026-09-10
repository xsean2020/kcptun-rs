//! Micro-benchmark of `Session::prepare_outbound_into` alone.
//!
//! Isolates the flush-path cost of snapshot traversal + SYN/UPD/PSH/FIN
//! emission from process_data / transport I/O.
//!
//! Usage:
//!   cargo run --release -p smux-rs --example prepare_bench -- [streams] [iters]

use bytes::{BytesMut};
use smux_rs::{Config, Session, DEFAULT_CONFIG};

fn main() {
    let streams: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(64);
    let iters: usize = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(50_000);

    // v1 has no per-stream peer window, so a one-sided prepare bench cannot
    // stall itself out after the initial 256 KiB window is spent.
    let cfg = Config {
        version: 1,
        max_frame_size: 16 * 1024,
        ..DEFAULT_CONFIG.clone()
    };
    let session = Session::new_client(&cfg).unwrap();
    let handles: Vec<_> = (0..streams)
        .map(|_| {
            let s = session.open_stream().unwrap();
            session.queue_syn(s.id());
            // Keep each stream saturated so every prepare actually drains PSH.
            let _ = s.write(&[0xABu8; 8 * 1024]);
            s
        })
        .collect();

    let mut buf = BytesMut::with_capacity(1024 * 1024);
    // Warmup
    for _ in 0..1_000 {
        buf.clear();
        let _ = session.prepare_outbound_into(&mut buf, 256 * 1024, 1);
        for s in &handles {
            let _ = s.write(&[0xABu8; 8 * 1024]);
        }
    }

    let start = std::time::Instant::now();
    let mut total_bytes = 0usize;
    for _ in 0..iters {
        buf.clear();
        let fins = session.prepare_outbound_into(&mut buf, 256 * 1024, 1);
        if !fins.is_empty() {
            session.mark_fins_sent(&fins);
        }
        total_bytes += buf.len();
        // Refill so the next iteration has work to do.
        for s in &handles {
            let _ = s.write(&[0xABu8; 8 * 1024]);
        }
    }
    let elapsed = start.elapsed();
    let ns_per = elapsed.as_nanos() as f64 / iters as f64;
    let mbps = (total_bytes as f64 / (1024.0 * 1024.0)) / elapsed.as_secs_f64();
    println!(
        "prepare_bench streams={streams} iters={iters} ns/op={ns_per:.1} mbps={mbps:.2} bytes={total_bytes}"
    );
}
