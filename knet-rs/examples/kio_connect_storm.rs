//! Connect storm benchmark: verify blocking TCP connect does not starve
//! the CPU work pool (K5).
//!
//! Run with:
//!   cargo run --release --example kio_connect_storm -- [--connects 100] [--crypto-jobs 500]
//!
//! Simultaneously issues TCP connects and CPU-block crypto work, measuring
//! whether connect latency pollutes the CPU pool queue-wait time.

use std::time::{Duration, Instant};

fn runtime_name() -> &'static str {
    "tokio"
}

fn percentile(sorted: &[u64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)] as f64
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut connects = 100usize;
    let mut crypto_jobs = 500u32;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--connects" => {
                i += 1;
                connects = args[i].parse().unwrap_or(100);
            }
            "--crypto-jobs" => {
                i += 1;
                crypto_jobs = args[i].parse().unwrap_or(500);
            }
            _ => {}
        }
        i += 1;
    }

    println!("[");

    knet::block_on(async {
        // Phase 1: crypto-only baseline (no connects)
        let mut latencies: Vec<u64> = Vec::new();
        for _ in 0..crypto_jobs {
            let t = Instant::now();
            let val: u64 = knet::cpu_block(|| (0..1000u64).sum()).await;
            latencies.push(t.elapsed().as_micros() as u64);
            debug_assert_eq!(val, 499500);
        }
        let mut sorted = latencies.clone();
        sorted.sort_unstable();
        println!(
            r#"  {{"scenario":"K5_crypto_only","runtime":"{}","jobs":{},"p50_us":{:.0},"p99_us":{:.0},"p999_us":{:.0}}},"#,
            runtime_name(),
            crypto_jobs,
            percentile(&sorted, 0.50),
            percentile(&sorted, 0.99),
            percentile(&sorted, 0.999)
        );

        // Phase 2: connect storm + crypto work simultaneously
        // Start a listener that immediately refuses (port closed)
        // We'll connect to a port that's not listening → fast refused
        // Spawn crypto jobs
        let crypto_handle = knet::spawn_task(async move {
            let mut lats: Vec<u64> = Vec::new();
            for _ in 0..crypto_jobs {
                let t = Instant::now();
                let val: u64 = knet::cpu_block(|| (0..1000u64).sum()).await;
                lats.push(t.elapsed().as_micros() as u64);
                debug_assert_eq!(val, 499500);
            }
            lats
        });

        // Spawn connect attempts (to refused port)
        let connect_handle = knet::spawn_task(async move {
            let mut lats: Vec<u64> = Vec::new();
            // Connect to a port that's almost certainly not listening
            let refused_addr: std::net::SocketAddr = "127.0.0.1:1".parse().unwrap();
            for _ in 0..connects {
                let t = Instant::now();
                let _ = knet::cpu_block(move || -> std::io::Result<std::net::TcpStream> {
                    std::net::TcpStream::connect_timeout(&refused_addr, Duration::from_millis(500))
                })
                .await;
                lats.push(t.elapsed().as_micros() as u64);
            }
            lats
        });

        let crypto_latencies = crypto_handle.await.expect("crypto task panicked");
        let connect_latencies = connect_handle.await.expect("connect task panicked");

        let mut crypto_sorted = crypto_latencies.clone();
        crypto_sorted.sort_unstable();
        let mut connect_sorted = connect_latencies.clone();
        connect_sorted.sort_unstable();

        println!(
            r#"  {{"scenario":"K5_connect_storm","runtime":"{}","connects":{},"crypto_jobs":{},"crypto_p50_us":{:.0},"crypto_p99_us":{:.0},"crypto_p999_us":{:.0},"connect_p50_us":{:.0},"connect_p99_us":{:.0}}},"#,
            runtime_name(),
            connects,
            crypto_jobs,
            percentile(&crypto_sorted, 0.50),
            percentile(&crypto_sorted, 0.99),
            percentile(&crypto_sorted, 0.999),
            percentile(&connect_sorted, 0.50),
            percentile(&connect_sorted, 0.99),
        );
    });

    println!("  null");
    println!("]");
}
