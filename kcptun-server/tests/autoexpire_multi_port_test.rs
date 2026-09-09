//! Combined functional test: multi-port + client `--autoexpire`.
//!
//! Verifies in a single run:
//!   1. Multi-port — the server listens on `-l :min-max` and the client dials
//!      the whole range (`-r host:min-max --conn 2`) and selects a random port
//!      for each KCP session.
//!   2. `--autoexpire` — the client scavenger closes sessions after their
//!      absolute `creation + autoexpire + scavengettl` deadline.
//!
//! Autoexpire is observed via the client's stderr log (the only external
//! signal): "scavenger started: autoexpire=Ns, scavengettl=Ms" proves the flag
//! was parsed and the scavenger enabled, and "scavenger: session closed due to
//! ttl" proves it actually closed an expired session.
//!
//! Usage:
//!   cargo build --workspace
//!   cargo test -p kcptun-server --test autoexpire_multi_port_test -- --nocapture

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
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

/// Kills spawned processes on drop (runs on panic too).
struct Procs(Vec<Child>);

impl Drop for Procs {
    fn drop(&mut self) {
        for p in &mut self.0 {
            let _ = p.kill();
        }
    }
}

/// Generate a deterministic payload; any byte corruption is detectable.
fn make_payload(conn_id: usize, size: usize) -> Vec<u8> {
    let mut data = Vec::with_capacity(size);
    for i in 0..size {
        data.push(((conn_id as u8).wrapping_add(i as u8)) ^ 0xA5);
    }
    data
}

/// Send data through the tunnel and receive the full echo response.
fn send_and_recv(cli_port: u16, data: &[u8], timeout_secs: u64) -> Result<Vec<u8>, String> {
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);

    // Retry connect to be robust against the listener not being ready yet.
    let mut s = None;
    let addr = format!("127.0.0.1:{}", cli_port);
    for attempt in 0..20 {
        match TcpStream::connect(&addr) {
            Ok(stream) => {
                let _ = stream.set_nodelay(true);
                s = Some(stream);
                break;
            }
            Err(_e) => {
                thread::sleep(Duration::from_millis(10 + attempt as u64 * 5));
            }
        }
    }
    let mut s = match s {
        Some(stream) => stream,
        None => return Err(format!("connect: Connection refused")),
    };

    s.set_write_timeout(Some(Duration::from_secs(15))).ok();
    s.write_all(data).map_err(|e| format!("write: {}", e))?;
    s.flush().map_err(|e| format!("flush: {}", e))?;
    // Give the flush loop time to drain data through KCP before triggering FIN.
    thread::sleep(Duration::from_millis(500));

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

#[test]
fn test_autoexpire_and_multi_port() {
    let target_port: u16 = 19120;
    let srv_port_min: u16 = 29970;
    let srv_port_max: u16 = 29971;
    let cli_port: u16 = 13020;

    for p in &[target_port, srv_port_min, srv_port_max, cli_port] {
        kill_port(*p);
    }
    thread::sleep(Duration::from_millis(800));

    // TCP echo server (multithreaded)
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

    // Server listens on a multi-port range.
    let sv = Command::new(&find_bin("kcptun-server"))
        .args(&[
            "-t",
            &format!("127.0.0.1:{}", target_port),
            "-l",
            &format!(":{}-{}", srv_port_min, srv_port_max),
            "--key",
            "k",
            "--crypt",
            "null",
            "--mode",
            "fast",
            "--nocomp",
            "--datashard",
            "0",
            "--parityshard",
            "0",
            "--sndwnd",
            "2048",
            "--rcvwnd",
            "2048",
        ])
        .env("RUST_LOG", "")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("srv");
    thread::sleep(Duration::from_secs(2));

    // Client dials the whole range; each of the two sessions randomly selects
    // a configured server port. Stderr is captured for scavenger assertions.
    let stderr_buf: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let mut cli = Command::new(&find_bin("kcptun-client"))
        .args(&[
            "-r",
            &format!("127.0.0.1:{}-{}", srv_port_min, srv_port_max),
            "-l",
            &format!(":{}", cli_port),
            "--key",
            "k",
            "--crypt",
            "null",
            "--mode",
            "fast",
            "--nocomp",
            "--datashard",
            "0",
            "--parityshard",
            "0",
            "--sndwnd",
            "2048",
            "--rcvwnd",
            "2048",
            "--conn",
            "2",
            "--autoexpire",
            "5",
            "--scavengettl",
            "2",
        ])
        // RUST_LOG=info (not "") so the scavenger's info! lines emit — the
        // client's env_logger parses an empty RUST_LOG as error-level only.
        .env("RUST_LOG", "info")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("cli");
    let child_stderr = cli.stderr.take().expect("cli stderr");
    let sink = stderr_buf.clone();
    thread::spawn(move || {
        let mut reader = BufReader::new(child_stderr);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => sink.lock().unwrap().push_str(&line),
            }
        }
    });
    let procs = Procs(vec![echo, sv, cli]);
    thread::sleep(Duration::from_secs(3));

    // ── Phase 1: multi-port (server listening range + client remote range) ──
    // Each send_and_recv opens one TCP conn; the client's accept loop
    // round-robins them across the two KCP sessions.
    let data1 = make_payload(1, 4096);
    let resp1 = send_and_recv(cli_port, &data1, 30).expect("round1");
    assert_eq!(data1, resp1, "multi-port: first session data mismatch");
    println!("  multi-port: first session OK ({} bytes)", data1.len());

    let data2 = make_payload(2, 8192);
    let resp2 = send_and_recv(cli_port, &data2, 30).expect("round2");
    assert_eq!(data2, resp2, "multi-port: second session data mismatch");
    println!(
        "  multi-port: second session OK ({} bytes, round-robin)",
        data2.len()
    );

    // ── Phase 2: --autoexpire ──
    // creation + (5 + 2)s elapses, then the scavenger closes them. Poll for
    // lines proving the parameter took effect and the close actually happened.
    let deadline = Instant::now() + Duration::from_secs(40);
    loop {
        let (started, closes) = {
            let log = stderr_buf.lock().unwrap();
            (
                log.contains("scavenger started: autoexpire=5s, scavengettl=2s"),
                log.matches("scavenger: session closed due to ttl").count(),
            )
        };
        if started && closes >= 1 {
            println!(
                "  autoexpire: scavenger started, closed {} expired idle conn(s)",
                closes
            );
            break;
        }
        if Instant::now() > deadline {
            panic!(
                "autoexpire did not fire within 40s.\nclient stderr:\n{}",
                stderr_buf.lock().unwrap()
            );
        }
        thread::sleep(Duration::from_millis(2000));
    }

    // Cleanup
    drop(procs);
    for p in &[target_port, srv_port_min, srv_port_max, cli_port] {
        kill_port(*p);
    }
    println!("✅ autoexpire + multi-port combined test OK");
}
