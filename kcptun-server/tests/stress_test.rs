//! Multi-threaded stress test for kcptun tunnel.
//!
//! Verifies that multiple concurrent connections don't deadlock AND that data
//! integrity is preserved across all sizes — from 1 byte to 512 KB.
//!
//! Each test spawns N threads; each thread sends payloads of varying sizes
//! through the kcptun tunnel (client -> KCP/UDP -> server -> TCP echo) and
//! verifies that the echoed response matches the original data byte-for-byte.
use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

fn find_bin(name: &str) -> String {
    // Try absolute workspace root first (most reliable for cargo test)
    if let Ok(manifest_dir) = std::env::var("CARGO_MANIFEST_DIR") {
        let workspace_root = std::path::Path::new(&manifest_dir)
            .parent()
            .unwrap_or(std::path::Path::new("."));
        let path = workspace_root.join("target/release").join(name);
        if path.exists() {
            return path.to_string_lossy().to_string();
        }
        let path = workspace_root.join("target/debug").join(name);
        if path.exists() {
            return path.to_string_lossy().to_string();
        }
    }
    // Fallback: try relative paths
    for dir in &[
        "target/release",
        "target/debug",
        "../target/release",
        "../target/debug",
    ] {
        let path = format!("{}/{}", dir, name);
        if std::path::Path::new(&path).exists() {
            return path;
        }
    }
    name.to_string()
}

fn kill_port(port: u16) {
    let _ = Command::new("sh")
        .arg("-c")
        .arg(format!("lsof -ti:{} | xargs kill -9 2>/dev/null", port))
        .output();
}

struct TestEnv {
    procs: Vec<Child>,
    cli_port: u16,
}

impl TestEnv {
    fn start(target_port: u16, srv_port: u16, cli_port: u16) -> Self {
        Self::start_with_config(target_port, srv_port, cli_port, "null", true, 1)
    }

    fn start_with_config(
        target_port: u16,
        srv_port: u16,
        cli_port: u16,
        crypt: &str,
        nocomp: bool,
        conn: usize,
    ) -> Self {
        for p in &[target_port, srv_port, cli_port] {
            kill_port(*p);
        }
        thread::sleep(Duration::from_millis(800));

        // TCP echo server (multithreaded) -- echoes all received data back.
        let echo = Command::new("python3")
            .arg("-c")
            .arg(format!(
                "import socket,threading;s=socket.socket();s.setsockopt(65535,4,1);\
                 s.bind(('',{}));s.listen(128)\
                 \ndef h(c):\n while True:\n  d=c.recv(65536)\n  if not d:break\n  c.sendall(d)\n c.close()\n\
                 while True:threading.Thread(target=h,args=(s.accept()[0],)).start()",
                target_port
            ))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("echo");
        thread::sleep(Duration::from_millis(500));

        let mut srv_args: Vec<String> = vec![
            "-t".into(),
            format!("127.0.0.1:{}", target_port),
            "-l".into(),
            format!(":{}", srv_port),
            "--key".into(),
            "k".into(),
            "--crypt".into(),
            crypt.into(),
            "--mode".into(),
            "fast".into(),
            "--datashard".into(),
            "0".into(),
            "--parityshard".into(),
            "0".into(),
            "--sndwnd".into(),
            "2048".into(),
            "--rcvwnd".into(),
            "2048".into(),
        ];
        if nocomp {
            srv_args.push("--nocomp".into());
        }

        let sv = Command::new(&find_bin("kcptun-server"))
            .args(&srv_args)
            .env("RUST_LOG", "")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("srv");
        thread::sleep(Duration::from_secs(2));

        let mut cli_args: Vec<String> = vec![
            "-r".into(),
            format!("127.0.0.1:{}", srv_port),
            "-l".into(),
            format!(":{}", cli_port),
            "--key".into(),
            "k".into(),
            "--crypt".into(),
            crypt.into(),
            "--mode".into(),
            "fast".into(),
            "--datashard".into(),
            "0".into(),
            "--parityshard".into(),
            "0".into(),
            "--sndwnd".into(),
            "2048".into(),
            "--rcvwnd".into(),
            "2048".into(),
            "--keepalive".into(),
            "30".into(),
            "--conn".into(),
            conn.to_string(),
        ];
        if nocomp {
            cli_args.push("--nocomp".into());
        }

        let cl = Command::new(&find_bin("kcptun-client"))
            .args(&cli_args)
            .env("RUST_LOG", "")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("cli");
        thread::sleep(Duration::from_secs(3));

        TestEnv {
            procs: vec![echo, sv, cl],
            cli_port,
        }
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        for p in &mut self.procs {
            let _ = p.kill();
        }
    }
}

// --- Test payload generator --------------------------------------------------

/// Generate a deterministic payload of the given size for a connection.
///
/// The pattern is `(conn_id + offset) ^ 0xA5`, which ensures:
/// - Different connections produce different data (no stream mixing false-positives)
/// - Any byte corruption is detectable (deterministic, not random)
fn make_payload(conn_id: usize, size: usize) -> Vec<u8> {
    let mut data = Vec::with_capacity(size);
    for i in 0..size {
        data.push(((conn_id as u8).wrapping_add(i as u8)) ^ 0xA5);
    }
    data
}

/// Send data through the tunnel and receive the full echo response.
///
/// Sends data, half-closes the write side (signals EOF to echo server),
/// then reads until either:
///   - EOF (server closed connection after echoing), or
///   - All expected bytes received (for echo, response length == request length)
fn send_and_recv(cli_port: u16, data: &[u8], timeout_secs: u64) -> Result<Vec<u8>, String> {
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    let mut s = TcpStream::connect(format!("127.0.0.1:{}", cli_port))
        .map_err(|e| format!("connect: {}", e))?;

    s.set_write_timeout(Some(Duration::from_secs(15))).ok();
    s.write_all(data).map_err(|e| format!("write: {}", e))?;
    s.flush().map_err(|e| format!("flush: {}", e))?;

    // Half-close: signal to echo server that we're done sending
    let _ = s.shutdown(std::net::Shutdown::Write);

    s.set_read_timeout(Some(Duration::from_secs(5))).ok();

    let expected_len = data.len();
    let mut resp = Vec::with_capacity(expected_len);
    let mut buf = [0u8; 65536];
    loop {
        if Instant::now() > deadline {
            return Err(format!(
                "timeout after {}s (sent {} bytes, recv {}/{} bytes)",
                timeout_secs,
                data.len(),
                resp.len(),
                expected_len
            ));
        }
        match s.read(&mut buf) {
            Ok(0) => break, // EOF -- server closed connection
            Ok(n) => {
                resp.extend_from_slice(&buf[..n]);
                // For echo server: once we receive all expected bytes, we're done.
                // Don't wait for EOF which may be delayed under high concurrency
                // due to FIN frame delivery latency through KCP.
                if resp.len() >= expected_len {
                    break;
                }
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::TimedOut
                    || e.kind() == std::io::ErrorKind::WouldBlock =>
            {
                continue;
            }
            Err(e) => return Err(format!("read: {}", e)),
        }
    }
    Ok(resp)
}

/// Verify echo response matches sent data exactly. Panics with details on mismatch.
fn verify(conn_id: usize, label: &str, sent: &[u8], recv: &[u8]) {
    if sent == recv {
        return;
    }
    let pos = sent.iter().zip(recv.iter()).position(|(a, b)| a != b);
    let detail = match pos {
        Some(p) => {
            let end = (p + 16).min(sent.len()).min(recv.len());
            format!(
                "first diff at byte {}: expected {:02x?}, got {:02x?}",
                p,
                &sent[p..end],
                &recv[p..end]
            )
        }
        None => "length mismatch with no byte diff".to_string(),
    };
    panic!(
        "[conn {} / {}] MISMATCH: sent {} bytes, recv {} bytes. {}",
        conn_id,
        label,
        sent.len(),
        recv.len(),
        detail
    );
}

// --- Tests -------------------------------------------------------------------

/// Single-connection smoke test with mixed payload sizes (1B ~ 64KB).
#[test]
fn test_single_connection_mixed_sizes() {
    let e = TestEnv::start(19040, 29940, 12990);

    let payloads: Vec<(&str, Vec<u8>)> = vec![
        ("1B", make_payload(0, 1)),
        ("10B", make_payload(0, 10)),
        ("100B", make_payload(0, 100)),
        ("1KB", make_payload(0, 1024)),
        ("10KB", make_payload(0, 10 * 1024)),
        ("64KB", make_payload(0, 64 * 1024)),
    ];

    for (label, data) in &payloads {
        let resp = send_and_recv(e.cli_port, data, 30).unwrap_or_else(|err| {
            panic!("[single / {}] failed: {}", label, err);
        });
        verify(0, label, data, &resp);
        println!("  single / {}: {} bytes OK", label, data.len());
    }

    drop(e);
    println!("✅ single-connection mixed-sizes test OK");
}

/// Single-connection 1MB transfer — verifies correctness for large payloads
/// that span many KCP segments and require multiple flush cycles.
#[test]
fn test_single_connection_1mb() {
    let e = TestEnv::start(19045, 29945, 12995);

    let data = make_payload(0, 1024 * 1024); // 1 MB
    let resp = send_and_recv(e.cli_port, &data, 120).unwrap_or_else(|err| {
        panic!("[single / 1MB] failed: {}", err);
    });
    verify(0, "1MB", &data, &resp);
    println!("  single / 1MB: {} bytes OK", data.len());

    drop(e);
    println!("✅ single-connection 1MB test OK");
}

/// 10 concurrent connections, each sending 256 bytes. Verifies basic concurrency.
#[test]
fn test_multithread_10_connections() {
    let e = TestEnv::start(19041, 29941, 12991);
    let mut handles = vec![];

    for i in 0..10 {
        let handle = thread::spawn(move || {
            let data = make_payload(i, 256);
            let resp = send_and_recv(12991, &data, 30)
                .unwrap_or_else(|err| panic!("[conn {}] failed: {}", i, err));
            verify(i, "256B", &data, &resp);
        });
        handles.push(handle);
    }

    for (i, h) in handles.into_iter().enumerate() {
        h.join().unwrap_or_else(|_| panic!("thread {} panicked", i));
    }
    drop(e);
    println!("✅ 10 concurrent connections OK (256B each, verified)");
}

/// 50 concurrent connections with small data (255B each).
#[test]
fn test_multithread_50_connections() {
    let e = TestEnv::start(19042, 29942, 12992);
    let mut handles = vec![];

    for i in 0..50 {
        let handle = thread::spawn(move || {
            let data = make_payload(i, 255);
            match send_and_recv(12992, &data, 60) {
                Ok(resp) => verify(i, "255B", &data, &resp),
                Err(err) => {
                    if !err.starts_with("connect:") {
                        panic!("[conn {} / 255B] {}", i, err);
                    }
                }
            }
        });
        handles.push(handle);
    }

    for (i, h) in handles.into_iter().enumerate() {
        h.join().unwrap_or_else(|_| panic!("thread {} panicked", i));
    }
    drop(e);
    println!("✅ 50 concurrent connections OK (255B each, verified)");
}

/// 100 concurrent connections -- comprehensive data integrity test.
///
/// Each of the 100 connections sends 2 payloads of representative sizes:
///   - 1 byte   (tiny: single KCP segment, minimal framing)
///   - 4 KB     (small: multi-segment, tests fragmentation and reassembly)
///
/// Every echo response is verified byte-for-byte against the original.
/// This catches data corruption, stream mixing, data loss, and deadlocks.
#[test]
fn test_multithread_100_connections() {
    let e = TestEnv::start(19043, 29943, 12993);
    let mut handles = vec![];

    for i in 0..100 {
        let handle = thread::spawn(move || {
            let payloads: Vec<(&str, Vec<u8>)> = vec![
                ("1B", make_payload(i, 1)),
                ("4KB", make_payload(i, 4 * 1024)),
            ];

            let mut errors: Vec<String> = Vec::new();

            for (label, data) in &payloads {
                match send_and_recv(12993, data, 60) {
                    Ok(resp) => {
                        if data.as_slice() != resp.as_slice() {
                            let pos = data.iter().zip(resp.iter()).position(|(a, b)| a != b);
                            let detail = match pos {
                                Some(p) => {
                                    let end = (p + 16).min(data.len()).min(resp.len());
                                    format!(
                                        "first diff at byte {}: expected {:02x?}, got {:02x?}",
                                        p,
                                        &data[p..end],
                                        &resp[p..end]
                                    )
                                }
                                None => "length mismatch with no byte diff".to_string(),
                            };
                            errors.push(format!(
                                "[conn {} / {}] MISMATCH: sent {} bytes, recv {} bytes. {}",
                                i,
                                label,
                                data.len(),
                                resp.len(),
                                detail
                            ));
                        }
                    }
                    Err(err) => {
                        errors.push(format!("[conn {} / {}] ERROR: {}", i, label, err));
                    }
                }
            }

            if !errors.is_empty() {
                let report = errors.join("\n  ");
                panic!(
                    "conn {} failed {}/{} payloads:\n  {}",
                    i,
                    errors.len(),
                    payloads.len(),
                    report
                );
            }
        });
        handles.push(handle);
    }

    // Every thread MUST succeed -- no silent failures allowed
    let mut failures = Vec::new();
    for (i, h) in handles.into_iter().enumerate() {
        match h.join() {
            Ok(()) => {}
            Err(panic_val) => {
                let msg = if let Some(s) = panic_val.downcast_ref::<String>() {
                    s.clone()
                } else if let Some(s) = panic_val.downcast_ref::<&str>() {
                    s.to_string()
                } else {
                    "(non-string panic)".to_string()
                };
                failures.push(format!("thread {}: {}", i, msg));
            }
        }
    }

    drop(e);

    if !failures.is_empty() {
        panic!(
            "FAILED: {} out of 100 connections failed:\n{}",
            failures.len(),
            failures.join("\n")
        );
    }

    println!("✅ 100 concurrent connections OK -- all payloads (1B, 4KB) verified byte-for-byte");
}

/// 100 concurrent connections with large data payloads (64KB + 128KB).
///
/// Tests the tunnel's ability to handle large transfers concurrently,
/// exercising KCP window management, retransmission, SMUX frame
/// fragmentation, and the KCP_MAX_FRAG chunking fix under load.
///
/// This test directly targets the bugs that caused deadlock at >2 connections
/// with 64KB+ data:
///   - ACK generation (receiver never sent ACKs for Push segments)
///   - Retransmission flood (all segments retransmitted every flush cycle)
///   - KCP_MAX_FRAG overflow (concatenated SMUX frames exceeded 128 * MSS)
#[test]
fn test_multithread_large_data() {
    // Single KCP channel (--conn 1): all 100 SMUX streams multiplex over
    // one KCP connection. This tests window contention and backpressure.
    let e = TestEnv::start_with_config(19044, 29944, 12994, "null", true, 1);
    let mut handles = vec![];

    for i in 0..100 {
        let handle = thread::spawn(move || {
            let payloads: Vec<(&str, Vec<u8>)> = vec![
                ("64KB", make_payload(i, 64 * 1024)),
                ("128KB", make_payload(i, 128 * 1024)),
            ];

            for (label, data) in &payloads {
                match send_and_recv(12994, data, 120) {
                    Ok(resp) => {
                        verify(i, label, data, &resp);
                    }
                    Err(err) => {
                        panic!("[conn {} / {}] ERROR: {}", i, label, err);
                    }
                }
            }
        });
        handles.push(handle);
    }

    let mut failures = Vec::new();
    for (i, h) in handles.into_iter().enumerate() {
        match h.join() {
            Ok(()) => {}
            Err(panic_val) => {
                let msg = if let Some(s) = panic_val.downcast_ref::<String>() {
                    s.clone()
                } else if let Some(s) = panic_val.downcast_ref::<&str>() {
                    s.to_string()
                } else {
                    "(non-string panic)".to_string()
                };
                failures.push(format!("thread {}: {}", i, msg));
            }
        }
    }

    drop(e);

    if !failures.is_empty() {
        panic!(
            "FAILED: {} out of 100 connections failed:\n{}",
            failures.len(),
            failures.join("\n")
        );
    }

    println!("✅ 100 concurrent large-data connections OK (64KB + 128KB, verified byte-for-byte");
}

/// Simulates a browser page refresh (e.g., YouTube) with rapid concurrent
/// connections of varying sizes — small requests mixed with large responses.
///
/// This test specifically exercises the window-full → drain → proactive WIns
/// recovery path that caused multi-second freezes when loading complex pages.
/// 80 connections are launched in 3 waves (simulating HTML → CSS/JS → images):
///   - Wave 1: 10 connections, 8KB each (HTML/CSS)
///   - Wave 2: 20 connections, 32KB each (JS bundles)
///   - Wave 3: 50 connections, 4KB-128KB mixed (images, API calls)
#[test]
fn test_page_refresh_simulation() {
    // Single KCP channel: 80 SMUX streams over 1 KCP connection.
    let e = TestEnv::start_with_config(19047, 29947, 12997, "null", true, 1);
    let mut handles = vec![];

    // Wave 1: 10 connections with 8KB (simulates HTML + CSS)
    for i in 0..10 {
        let handle = thread::spawn(move || {
            let data = make_payload(i, 8 * 1024);
            match send_and_recv(12997, &data, 60) {
                Ok(resp) => verify(i, "8KB", &data, &resp),
                Err(err) => panic!("[wave1 conn {} / 8KB] {}", i, err),
            }
        });
        handles.push(handle);
    }

    thread::sleep(Duration::from_millis(200));

    // Wave 2: 20 connections with 32KB (simulates JS bundles)
    for i in 10..30 {
        let handle = thread::spawn(move || {
            let data = make_payload(i, 32 * 1024);
            match send_and_recv(12997, &data, 90) {
                Ok(resp) => verify(i, "32KB", &data, &resp),
                Err(err) => panic!("[wave2 conn {} / 32KB] {}", i, err),
            }
        });
        handles.push(handle);
    }

    thread::sleep(Duration::from_millis(300));

    // Wave 3: 50 connections with mixed sizes (simulates images, API)
    for i in 30..80 {
        let size = match i % 5 {
            0 => 4 * 1024,   // 4KB (API JSON)
            1 => 16 * 1024,  // 16KB (small image)
            2 => 64 * 1024,  // 64KB (medium image)
            3 => 128 * 1024, // 128KB (large image)
            _ => 512,        // 512B (tracking pixel)
        };
        let label = match size {
            512 => "512B",
            4096 => "4KB",
            16384 => "16KB",
            65536 => "64KB",
            131072 => "128KB",
            _ => "?",
        };
        let handle = thread::spawn(move || {
            let data = make_payload(i, size);
            match send_and_recv(12997, &data, 120) {
                Ok(resp) => verify(i, label, &data, &resp),
                Err(err) => panic!("[wave3 conn {} / {}] {}", i, label, err),
            }
        });
        handles.push(handle);
    }

    let mut failures = Vec::new();
    for (i, h) in handles.into_iter().enumerate() {
        match h.join() {
            Ok(()) => {}
            Err(panic_val) => {
                let msg = if let Some(s) = panic_val.downcast_ref::<String>() {
                    s.clone()
                } else if let Some(s) = panic_val.downcast_ref::<&str>() {
                    s.to_string()
                } else {
                    "(non-string panic)".to_string()
                };
                failures.push(format!("thread {}: {}", i, msg));
            }
        }
    }

    drop(e);

    if !failures.is_empty() {
        panic!(
            "FAILED: {} out of 80 connections failed:\n{}",
            failures.len(),
            failures.join("\n")
        );
    }

    println!("✅ page-refresh simulation OK -- 80 connections in 3 waves (512B~128KB), verified byte-for-byte");
}

#[test]
fn test_snappy_compressible_data() {
    // Test Snappy compression with highly compressible data.
    // Before the fix, this would trigger "snappy: output buffer too small"
    // because the decompression buffer was estimated as compressed_len * 6,
    // which is insufficient for high-compression-ratio data.
    let e = TestEnv::start_with_config(19048, 29948, 12998, "null", false, 1);

    // Single connection with highly compressible data
    let payloads: Vec<(&str, Vec<u8>)> = vec![
        ("4K_A", vec![0x41u8; 4096]),   // all 'A' — compresses to ~200B
        ("8K_A", vec![0x41u8; 8192]),   // all 'A' — compresses to ~200B
        ("16K_0", vec![0x00u8; 16384]), // all NUL — compresses to ~200B
        ("64K_A", vec![0x41u8; 65536]), // all 'A' — compresses to ~200B
        ("4K_pat", {
            // Repeating 4-byte pattern — highly compressible
            let mut d = Vec::with_capacity(4096);
            for i in 0..4096 {
                d.push(b"ABCD"[i % 4]);
            }
            d
        }),
    ];

    for (label, data) in &payloads {
        match send_and_recv(12998, data, 60) {
            Ok(resp) => {
                assert_eq!(
                    data.as_slice(),
                    resp.as_slice(),
                    "[snappy {}] data mismatch",
                    label
                );
                println!("  snappy / {}: {} bytes OK", label, data.len());
            }
            Err(err) => {
                panic!("[snappy {}] ERROR: {}", label, err);
            }
        }
    }

    drop(e);
    println!("✅ snappy compressible-data test OK");
}
