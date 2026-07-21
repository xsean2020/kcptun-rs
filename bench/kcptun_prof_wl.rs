// Native root process for samply on macOS (must be locally built).
// Spawns python echo + release server/client, runs concurrent bulk (throughput.py pattern).
use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let crypt = args.get(1).map(|s| s.as_str()).unwrap_or("null");
    let data_mb: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(4);
    let server_bin = args.get(3).map(|s| s.as_str()).unwrap_or("./target/release/kcptun-server");
    let client_bin = args.get(4).map(|s| s.as_str()).unwrap_or("./target/release/kcptun-client");
    let latency_iters: usize = args.get(5).and_then(|s| s.parse().ok()).unwrap_or(5);
    let key = "bench-key";

    let base = 31000 + (std::process::id() % 500) * 3;
    let echo_port = base as u16;
    let server_port = (base + 1) as u16;
    let client_port = (base + 2) as u16;

    // Python echo — same as bench/run_bench.sh (known-good on this host).
    let mut echo = Command::new("python3")
        .arg("-u")
        .arg("-c")
        .arg(format!(
            r#"
import socket, threading
def echo(s,a):
    try:
        while True:
            d=s.recv(65536)
            if not d: break
            s.sendall(d)
    except: pass
    s.close()
s=socket.socket(); s.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1)
s.bind(('0.0.0.0',{echo_port})); s.listen(10)
while True: threading.Thread(target=echo,args=s.accept(),daemon=True).start()
"#
        ))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn python echo");
    thread::sleep(Duration::from_millis(200));

    let mut server = Command::new(server_bin)
        .args([
            "-l",
            &format!("0.0.0.0:{server_port}"),
            "-t",
            &format!("127.0.0.1:{echo_port}"),
            "--key",
            key,
            "--crypt",
            crypt,
            "--nocomp",
            "--mode",
            "fast",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn server");
    thread::sleep(Duration::from_millis(400));
    if server.try_wait().ok().flatten().is_some() {
        eprintln!("server exited early");
        let _ = echo.kill();
        std::process::exit(1);
    }

    let mut client = Command::new(client_bin)
        .args([
            "-l",
            &format!("127.0.0.1:{client_port}"),
            "-r",
            &format!("127.0.0.1:{server_port}"),
            "--key",
            key,
            "--crypt",
            crypt,
            "--nocomp",
            "--mode",
            "fast",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn client");

    let ready = Instant::now();
    loop {
        if TcpStream::connect(("127.0.0.1", client_port)).is_ok() {
            break;
        }
        if ready.elapsed() > Duration::from_secs(5) {
            eprintln!("client not ready");
            let _ = client.kill();
            let _ = server.kill();
            let _ = echo.kill();
            std::process::exit(1);
        }
        thread::sleep(Duration::from_millis(50));
    }
    thread::sleep(Duration::from_millis(200));

    // Warmup
    let _ = run_bulk(client_port, 2 * 1024 * 1024);

    let total = data_mb * 1024 * 1024;
    let t0 = Instant::now();
    let got = run_bulk(client_port, total);
    let elapsed = t0.elapsed().as_secs_f64().max(1e-6);
    let thr = (got as f64 / (1024.0 * 1024.0)) / elapsed;
    eprintln!(
        "throughput: {:.2} MB/s recv={got}/{total} crypt={crypt}",
        thr
    );

    if latency_iters > 0 {
        if let Ok(mut s) = TcpStream::connect(("127.0.0.1", client_port)) {
            let _ = s.set_nodelay(true);
            let payload = [b'X'; 1024];
            let mut lat = Vec::new();
            for _ in 0..latency_iters {
                let start = Instant::now();
                if s.write_all(&payload).is_err() {
                    break;
                }
                let mut gotn = 0usize;
                let mut buf = [0u8; 1024];
                while gotn < 1024 {
                    match s.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => gotn += n,
                    }
                }
                if gotn == 1024 {
                    lat.push(start.elapsed().as_secs_f64() * 1000.0);
                }
            }
            if !lat.is_empty() {
                lat.sort_by(|a, b| a.partial_cmp(b).unwrap());
                eprintln!("latency_median_ms: {:.3}", lat[lat.len() / 2]);
            }
        }
    }

    let _ = client.kill();
    let _ = server.kill();
    let _ = echo.kill();
    let _ = client.wait();
    let _ = server.wait();
    let _ = echo.wait();
}

fn run_bulk(client_port: u16, total: usize) -> usize {
    let mut stream = match TcpStream::connect(("127.0.0.1", client_port)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("connect failed: {e}");
            return 0;
        }
    };
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(60)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(60)));
    let mut stream_rx = stream.try_clone().expect("clone");
    let received = Arc::new(AtomicUsize::new(0));
    let r2 = received.clone();
    let rx = thread::spawn(move || {
        let mut buf = vec![0u8; 65536];
        while r2.load(Ordering::Relaxed) < total {
            match stream_rx.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    r2.fetch_add(n, Ordering::Relaxed);
                }
                Err(_) => break,
            }
        }
    });
    let chunk_sz = 128 * 1024;
    let payload = vec![0xABu8; chunk_sz];
    let mut sent = 0usize;
    while sent < total {
        let n = (total - sent).min(chunk_sz);
        if stream.write_all(&payload[..n]).is_err() {
            break;
        }
        sent += n;
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    while received.load(Ordering::Relaxed) < total && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    drop(stream);
    let _ = rx.join();
    received.load(Ordering::Relaxed)
}
