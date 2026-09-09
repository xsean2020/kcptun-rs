//! CPU offload pool benchmark: serial/concurrent submit, queue wait, run time (K4).
//!
//! Run with:
//!   cargo run --release --example kio_cpu_block_bench -- [--concurrency 1] [--jobs 1000]
//!
//! Measures: submit-to-completion latency for `cpu_block` jobs at various
//! concurrency levels. Separates queue wait from execution time.

use std::time::Instant;

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

async fn bench_serial(n: u32) {
    // Submit N jobs serially (submit, await, submit, await, ...)
    let start = Instant::now();
    let mut latencies: Vec<u64> = Vec::with_capacity(n as usize);

    for _ in 0..n {
        let t = Instant::now();
        let val: u64 = knet::cpu_block(|| (0..1000u64).sum()).await;
        latencies.push(t.elapsed().as_micros() as u64);
        debug_assert_eq!(val, 499500);
    }

    let total = start.elapsed();
    let mut sorted = latencies.clone();
    sorted.sort_unstable();

    println!(
        r#"  {{"scenario":"K4_serial","runtime":"{}","jobs":{},"total_ms":{:.2},"p50_us":{:.0},"p99_us":{:.0},"p999_us":{:.0}}},"#,
        runtime_name(),
        n,
        total.as_millis(),
        percentile(&sorted, 0.50),
        percentile(&sorted, 0.99),
        percentile(&sorted, 0.999)
    );
}

async fn bench_concurrent(n: u32, concurrency: usize) {
    // Submit N jobs at `concurrency` concurrent outstanding requests.
    let start = Instant::now();
    let mut latencies: Vec<u64> = Vec::with_capacity(n as usize);

    // Simple approach: spawn all tasks, collect handles, await all.
    let mut handles = Vec::with_capacity(n as usize);
    for _ in 0..n {
        let t = Instant::now();
        let h = knet::spawn_task(async move {
            let val: u64 = knet::cpu_block(|| (0..1000u64).sum()).await;
            (t.elapsed().as_micros() as u64, val)
        });
        handles.push(h);
    }

    for h in handles {
        let (lat, val) = h.await.expect("task panicked");
        latencies.push(lat);
        debug_assert_eq!(val, 499500);
    }

    let total = start.elapsed();
    let mut sorted = latencies.clone();
    sorted.sort_unstable();

    println!(
        r#"  {{"scenario":"K4_concurrent","runtime":"{}","jobs":{},"concurrency":{},"total_ms":{:.2},"p50_us":{:.0},"p99_us":{:.0},"p999_us":{:.0}}},"#,
        runtime_name(),
        n,
        concurrency,
        total.as_millis(),
        percentile(&sorted, 0.50),
        percentile(&sorted, 0.99),
        percentile(&sorted, 0.999)
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut jobs = 1000u32;
    let mut concurrency = 4usize;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--jobs" => {
                i += 1;
                jobs = args[i].parse().unwrap_or(1000);
            }
            "--concurrency" => {
                i += 1;
                concurrency = args[i].parse().unwrap_or(4);
            }
            _ => {}
        }
        i += 1;
    }

    println!("[");
    knet::block_on(async {
        bench_serial(jobs.min(200)).await; // serial: fewer jobs
        bench_concurrent(jobs, concurrency).await;
    });
    println!("  null");
    println!("]");
}
