//! Bidirectional copy benchmark: throughput, half-close, backpressure,
//! reverse small-packet latency (K1/K2/K3).
//!
//! Run with:
//!   cargo run --release --example kio_bidi_bench -- [--scenario K2] [--size 65536] [--duration 10]
//!
//! Output: JSON on stdout (hand-serialized, no extra deps).

use std::time::{Duration, Instant};

use knet::{AsyncReadExt, AsyncWriteExt, TcpListener, TcpStream};

fn percentile(sorted: &[u64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)] as f64
}

fn runtime_name() -> &'static str {
    "tokio"
}

/// Unwrap a spawned task result.
fn join<T>(r: Result<T, tokio::task::JoinError>) -> T {
    r.expect("task panicked")
}

/// Close the write side of a TcpStream.
async fn close_write(s: &mut TcpStream) {
    use knet::AsyncWriteExt;
    let _ = s.shutdown().await;
}

// ─── K1: idle pipe creation ───────────────────────────────────────────────────

async fn scenario_k1_idle_pipes(count: usize) {
    let start = Instant::now();
    let mut left: Vec<TcpStream> = Vec::with_capacity(count);
    let mut right: Vec<TcpStream> = Vec::with_capacity(count);

    for _ in 0..count {
        let listener = TcpListener::bind("127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(addr.to_string()).await.unwrap();
        let (server, _) = listener.accept().await.unwrap();
        left.push(client);
        right.push(server);
    }

    let elapsed = start.elapsed();
    drop(left);
    drop(right);

    println!(
        r#"  {{"scenario":"K1_idle","runtime":"{}","pipe_count":{},"create_secs":{:.6}}},"#,
        runtime_name(),
        count,
        elapsed.as_secs_f64()
    );
}

// ─── K2: unidirectional bulk throughput ──────────────────────────────────────

async fn scenario_k2_bulk(size: usize, duration: Duration) {
    let listener = TcpListener::bind("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();

    let sender = knet::spawn_task(async move {
        let mut s = TcpStream::connect(addr.to_string()).await.unwrap();
        let data = vec![0xABu8; size];
        let deadline = Instant::now() + duration;
        let mut total = 0u64;
        while Instant::now() < deadline {
            match s.write_all(&data).await {
                Ok(_) => total += size as u64,
                Err(_) => break,
            }
        }
        let _ = close_write(&mut s).await;
        total
    });

    let (mut server, _) = listener.accept().await.unwrap();
    let receiver = knet::spawn_task(async move {
        let mut buf = vec![0u8; size];
        let mut total = 0u64;
        loop {
            match server.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => total += n as u64,
            }
        }
        total
    });

    let sent = join(sender.await);
    let received = join(receiver.await);
    let throughput_mbps = received as f64 * 8.0 / 1e6 / duration.as_secs_f64();

    println!(
        r#"  {{"scenario":"K2_bulk","runtime":"{}","chunk_size":{},"sent":{},"received":{},"throughput_mbps":{:.2}}},"#,
        runtime_name(),
        size,
        sent,
        received,
        throughput_mbps
    );
}

// ─── K3: backpressure reverse latency ────────────────────────────────────────
//
// Topology:
//   writer ──► l1 ──► relay_left ◄──► relay_right ◄── l2 ──◄ reader
//
// The relay accepts on both l1 and l2. Writer connects to l1 (forward),
// reader connects to l2 (reverse). Relay copies bidirectionally.
//
// - Writer: writes 64 KB chunks continuously (forward saturation)
// - Reader: drains forward data, sends 64 B "ping" every 10 ms (reverse)
// - Relay:  copy_bidirectional_postwait(closewait=0)
// - Measure: write-completion latency for each reverse ping (proxy for
//   how long the reverse write takes when the forward path is saturated).

async fn scenario_k3_backpressure(duration: Duration) {
    let chunk = 65536usize;
    let ping_size = 64usize;
    let ping_interval = Duration::from_millis(10);

    // l1: writer → relay_left
    let l1 = TcpListener::bind("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let a1 = l1.local_addr().unwrap();
    // l2: reader → relay_right
    let l2 = TcpListener::bind("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let a2 = l2.local_addr().unwrap();

    let deadline = Instant::now() + duration;

    // Writer: writes 64 KB chunks continuously.
    let writer_task = knet::spawn_task(async move {
        let mut s = TcpStream::connect(a1.to_string()).await.unwrap();
        let data = vec![0xABu8; chunk];
        let mut total_written = 0u64;

        while Instant::now() < deadline {
            knet::timeout(Duration::from_millis(5), async { s.write_all(&data).await })
                .await
                .ok();
            total_written += chunk as u64;
        }
        let _ = close_write(&mut s).await;
        total_written
    });

    // Reader: drains forward data, sends 64 B pings periodically.
    let reader_task = knet::spawn_task(async move {
        let mut s = TcpStream::connect(a2.to_string()).await.unwrap();
        let mut drain_buf = vec![0u8; chunk];
        let ping = vec![0xCDu8; ping_size];
        let mut latencies: Vec<u64> = Vec::new();
        let mut total_drained = 0u64;

        while Instant::now() < deadline {
            // Drain forward data with short timeout
            match knet::timeout(Duration::from_millis(5), s.read(&mut drain_buf)).await {
                Ok(Ok(0)) | Ok(Err(_)) => break,
                Ok(Ok(n)) => total_drained += n as u64,
                Err(_) => {} // timeout, continue
            }

            // Send reverse ping and measure write completion time
            let send_time = Instant::now();
            if s.write_all(&ping).await.is_err() {
                break;
            }
            latencies.push(send_time.elapsed().as_micros() as u64);

            knet::sleep_ms(ping_interval.as_millis() as u64).await;
        }
        let _ = close_write(&mut s).await;
        (total_drained, latencies)
    });

    // Accept both relay sides (writer's connection on l1, reader's on l2)
    let (mut relay_left, _) = l1.accept().await.unwrap();
    let (mut relay_right, _) = l2.accept().await.unwrap();

    // Run relay: copy bidirectionally between the two accepted connections
    let relay_task = knet::spawn_task(async move {
        let _ = knet::copy_bidirectional_postwait(&mut relay_left, &mut relay_right, 0).await;
    });

    let forward_bytes = join(writer_task.await);
    let (reverse_drained, ping_latencies) = join(reader_task.await);
    drop(relay_task);

    let mut sorted = ping_latencies.clone();
    sorted.sort_unstable();

    let p50 = percentile(&sorted, 0.50);
    let p99 = percentile(&sorted, 0.99);
    let p999 = percentile(&sorted, 0.999);
    let throughput_mbps = forward_bytes as f64 * 8.0 / 1e6 / duration.as_secs_f64();

    println!(
        r#"  {{"scenario":"K3_backpressure","runtime":"{}","forward_bytes":{},"reverse_bytes":{},"throughput_mbps":{:.2},"reverse_count":{},"reverse_p50_us":{:.0},"reverse_p99_us":{:.0},"reverse_p999_us":{:.0}}},"#,
        runtime_name(),
        forward_bytes,
        reverse_drained,
        throughput_mbps,
        sorted.len(),
        p50,
        p99,
        p999
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut scenario = "all".to_string();
    let mut size = 65536usize;
    let mut duration_secs = 10u64;
    let mut pipe_count = 100usize;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--scenario" => {
                i += 1;
                scenario = args[i].clone();
            }
            "--size" => {
                i += 1;
                size = args[i].parse().unwrap_or(65536);
            }
            "--duration" => {
                i += 1;
                duration_secs = args[i].parse().unwrap_or(10);
            }
            "--pipes" => {
                i += 1;
                pipe_count = args[i].parse().unwrap_or(100);
            }
            _ => {}
        }
        i += 1;
    }

    let duration = Duration::from_secs(duration_secs);

    println!("[");
    let _ = size;

    knet::block_on(async {
        if scenario == "all" || scenario == "K1" {
            scenario_k1_idle_pipes(pipe_count).await;
        }
        if scenario == "all" || scenario == "K2" {
            for &s in &[4096usize, 8192, 16384, 32768, 65536] {
                scenario_k2_bulk(s, duration).await;
            }
        }
        if scenario == "all" || scenario == "K3" {
            scenario_k3_backpressure(duration).await;
        }
    });

    println!("  null");
    println!("]");
}
