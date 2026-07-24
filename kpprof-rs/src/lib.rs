//! kpprof — Go-compatible pprof HTTP server for kcptun-rs.
//!
//! Provides a `net/http/pprof`-compatible HTTP server that serves CPU profiles
//! as Go pprof protobuf, analyzable directly with `go tool pprof`.
//!
//! ## Endpoints
//!
//! | Endpoint | Description |
//! |----------|-------------|
//! | `GET /debug/pprof/` | HTML index listing all profile types |
//! | `GET /debug/pprof/profile?seconds=N` | CPU profile as gzip protobuf (default 30s) |
//! | `GET /debug/pprof/cmdline` | Command line args (`\x00` separated) |
//! | `GET /debug/pprof/symbol` | Symbol lookup (`?address=0x...`) |
//! | `GET /debug/pprof/heap` | Heap allocation profile (protobuf) |
//! | `GET /debug/pprof/allocs` | Cumulative allocation profile (protobuf) |
//! | `GET /debug/pprof/goroutine?debug=N` | Thread dump (Linux: `/proc/self/task`) |
//! | `GET /debug/pprof/deadlock` | Active deadlock check (requires `deadlock` feature) |
//!
//! ## Usage
//!
//! ```ignore
//! // In kcptun-server / kcptun-client main.rs:
//! #[cfg(feature = "pprof")]
//! if let Some(ref addr) = cli.pprof {
//!     let stop = stop_flag.clone();
//!     kio::spawn_task(async move {
//!         let _ = kpprof::run_pprof(&addr, stop).await;
//!     });
//! }
//! ```
//!
//! ## Analysis
//!
//! ```bash
//! # CPU profile
//! go tool pprof -http=:0 http://localhost:6060/debug/pprof/profile?seconds=30
//!
//! # Heap profile
//! go tool pprof -http=:0 http://localhost:6060/debug/pprof/heap
//!
//! # Thread dump (deadlock detection)
//! curl 'http://localhost:6060/debug/pprof/goroutine?debug=2'
//! ```

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};

// ─── Heap profiling (always compiled; zero-cost when sample_rate == 0) ───────

mod heap;
pub use heap::{build_heap_profile, build_allocs_profile, ProfilingAllocator};

// ─── Deadlock detection (optional) ──────────────────────────────────────────

#[cfg(feature = "deadlock")]
mod deadlock;

#[cfg(feature = "deadlock")]
pub use deadlock::{start_deadlock_detector, dump_deadlocks};

// ─── Main entry point ───────────────────────────────────────────────────────

/// Start the pprof HTTP server compatible with `go tool pprof`.
///
/// Listens on `addr` and serves `/debug/pprof/*` endpoints.
/// Returns when `stop` is set to `true`.
pub async fn run_pprof(addr: &str, stop: Arc<AtomicBool>) -> Result<()> {
    use kio::AsyncReadExt;
    use kio::AsyncWriteExt;

    let socket_addr: SocketAddr = addr.parse().context("invalid pprof address")?;
    let listener = kio::TcpListener::bind(socket_addr).await?;
    log::info!("pprof listening on http://{}/debug/pprof/", socket_addr);

    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let accepted = kio::timeout(Duration::from_millis(500), listener.accept()).await;
        let (mut stream, peer) = match accepted {
            Ok(Ok(v)) => v,
            _ => continue,
        };

        // Read HTTP request headers
        let mut buf = vec![0u8; 8192];
        let mut filled = 0usize;
        loop {
            if filled >= buf.len() {
                break;
            }
            match kio::timeout(Duration::from_secs(2), stream.read(&mut buf[filled..])).await {
                Ok(Ok(0)) => break,
                Ok(Ok(n)) => {
                    filled += n;
                    if buf[..filled].windows(4).any(|w| w == b"\r\n\r\n")
                        || buf[..filled].windows(2).any(|w| w == b"\n\n")
                    {
                        break;
                    }
                }
                _ => break,
            }
        }

        let req = String::from_utf8_lossy(&buf[..filled]);
        let headers_lower = req.to_lowercase();
        let first_line = req.lines().next().unwrap_or("");
        let path_q = first_line.split_whitespace().nth(1).unwrap_or("/");
        let (path, query) = match path_q.split_once('?') {
            Some((p, q)) => (p, q),
            None => (path_q, ""),
        };

        let accept_gzip = headers_lower.contains("accept-encoding: gzip");

        // ── Helper: send HTTP response ──────────────────────────────────
        async fn respond(
            stream: &mut kio::TcpStream,
            status: &str,
            ctype: &str,
            body: &[u8],
        ) {
            let header = format!(
                "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(header.as_bytes()).await;
            let _ = stream.write_all(body).await;
            let _ = stream.flush().await;
        }

        // ── Helper: send HTTP response with extra headers (e.g. gzip) ──
        async fn respond_with_extra(
            stream: &mut kio::TcpStream,
            status: &str,
            ctype: &str,
            extra_header: &str,
            body: &[u8],
        ) {
            let header = format!(
                "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\n{extra_header}Content-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(header.as_bytes()).await;
            let _ = stream.write_all(body).await;
            let _ = stream.flush().await;
        }

        // ── Helper: gzip compress if client accepts it ──────────────────
        fn maybe_gzip(accept_gzip: bool, body: &[u8]) -> (Vec<u8>, bool) {
            if accept_gzip && body.len() > 256 {
                use flate2::write::GzEncoder;
                use flate2::Compression;
                use std::io::Write;
                let mut encoder = GzEncoder::new(Vec::with_capacity(body.len() / 2), Compression::default());
                if encoder.write_all(body).is_ok() {
                    if let Ok(compressed) = encoder.finish() {
                        return (compressed, true);
                    }
                }
            }
            (body.to_vec(), false)
        }

        // ════════════════════════════════════════════════════════════════
        // Route dispatch
        // ════════════════════════════════════════════════════════════════

        // ── HTML index ──────────────────────────────────────────────────
        if path == "/debug/pprof/" || path == "/debug/pprof" {
            let body = build_index_html();
            respond(&mut stream, "200 OK", "text/html; charset=utf-8", body.as_bytes()).await;
            continue;
        }

        // ── CPU profile ─────────────────────────────────────────────────
        if path == "/debug/pprof/profile" {
            let mut seconds: u64 = 30;
            for part in query.split('&') {
                if let Some(v) = part.strip_prefix("seconds=") {
                    if let Ok(n) = v.parse::<u64>() {
                        seconds = n.clamp(1, 300);
                    }
                }
            }
            log::info!("pprof CPU profile {}s peer={}", seconds, peer);

            let profile_result = kio::cpu_block(move || -> std::result::Result<Vec<u8>, String> {
                use pprof::protos::Message;
                let builder = pprof::ProfilerGuardBuilder::default().frequency(997);
                #[cfg(any(
                    target_arch = "x86_64",
                    target_arch = "aarch64",
                    target_arch = "riscv64",
                    target_arch = "loongarch64"
                ))]
                let builder = builder.blocklist(&["libc", "libgcc", "pthread", "vdso"]);
                let guard = builder
                    .build()
                    .map_err(|e| format!("profiler start failed: {e}"))?;
                std::thread::sleep(Duration::from_secs(seconds));
                let report = guard
                    .report()
                    .build()
                    .map_err(|e| format!("report build failed: {e}"))?;
                let profile = report
                    .pprof()
                    .map_err(|e| format!("build pprof failed: {e}"))?;
                let mut content = Vec::new();
                profile
                    .write_to_vec(&mut content)
                    .map_err(|e| format!("encode pprof failed: {e}"))?;
                Ok(content)
            })
            .await;

            let profile_bytes = match profile_result {
                Ok(bytes) => bytes,
                Err(e) => {
                    let msg = format!("{e}\n");
                    respond(
                        &mut stream,
                        "500 Internal Server Error",
                        "text/plain; charset=utf-8",
                        msg.as_bytes(),
                    )
                    .await;
                    continue;
                }
            };

            let (body, gzipped) = maybe_gzip(accept_gzip, &profile_bytes);
            if gzipped {
                respond_with_extra(
                    &mut stream,
                    "200 OK",
                    "application/octet-stream",
                    "Content-Encoding: gzip\r\n",
                    &body,
                )
                .await;
            } else {
                respond(&mut stream, "200 OK", "application/octet-stream", &body).await;
            }
            log::info!("pprof CPU profile complete ({} bytes) peer={}", body.len(), peer);
            continue;
        }

        // ── Command line ────────────────────────────────────────────────
        if path == "/debug/pprof/cmdline" {
            let args: Vec<Vec<u8>> = std::env::args()
                .map(|s| {
                    let mut b = s.into_bytes();
                    b.push(0u8);
                    b
                })
                .collect();
            let body: Vec<u8> = args.into_iter().flatten().collect();
            respond(&mut stream, "200 OK", "text/plain; charset=utf-8", &body).await;
            continue;
        }

        // ── Symbol lookup ───────────────────────────────────────────────
        if path == "/debug/pprof/symbol" {
            let mut symbols = String::new();
            for part in query.split('&') {
                if let Some(addr_str) = part.strip_prefix("address=") {
                    let addr_parsed = if let Some(hex) = addr_str.strip_prefix("0x") {
                        usize::from_str_radix(hex, 16).ok()
                    } else {
                        addr_str.parse::<usize>().ok()
                    };
                    if let Some(addr) = addr_parsed {
                        let mut found = false;
                        backtrace::resolve(addr as *mut std::ffi::c_void, |sym| {
                            if let Some(name) = sym.name() {
                                symbols.push_str(&format!("{}\n", name));
                                found = true;
                            }
                        });
                        if !found {
                            symbols.push_str("?\n");
                        }
                    }
                }
            }
            if symbols.is_empty() {
                symbols = "num_symbols: 0\n".to_string();
            }
            respond(&mut stream, "200 OK", "text/plain; charset=utf-8", symbols.as_bytes()).await;
            continue;
        }

        // ── Heap profile ────────────────────────────────────────────────
        if path == "/debug/pprof/heap" {
            let profile = build_heap_profile();
            if profile.is_empty() {
                let msg = "heap profiling not enabled (build without ProfilingAllocator or rate=0)\n";
                respond(&mut stream, "200 OK", "text/plain; charset=utf-8", msg.as_bytes()).await;
            } else {
                let (body, gzipped) = maybe_gzip(accept_gzip, &profile);
                if gzipped {
                    respond_with_extra(
                        &mut stream,
                        "200 OK",
                        "application/octet-stream",
                        "Content-Encoding: gzip\r\n",
                        &body,
                    )
                    .await;
                } else {
                    respond(&mut stream, "200 OK", "application/octet-stream", &body).await;
                }
            }
            continue;
        }

        // ── Allocs profile ──────────────────────────────────────────────
        if path == "/debug/pprof/allocs" {
            let profile = build_allocs_profile();
            if profile.is_empty() {
                let msg = "allocation profiling not enabled\n";
                respond(&mut stream, "200 OK", "text/plain; charset=utf-8", msg.as_bytes()).await;
            } else {
                let (body, gzipped) = maybe_gzip(accept_gzip, &profile);
                if gzipped {
                    respond_with_extra(
                        &mut stream,
                        "200 OK",
                        "application/octet-stream",
                        "Content-Encoding: gzip\r\n",
                        &body,
                    )
                    .await;
                } else {
                    respond(&mut stream, "200 OK", "application/octet-stream", &body).await;
                }
            }
            continue;
        }

        // ── Goroutine (thread dump) ─────────────────────────────────────
        if path == "/debug/pprof/goroutine" {
            let debug_mode: u64 = query
                .split('&')
                .find_map(|p| p.strip_prefix("debug=")?.parse().ok())
                .unwrap_or(1);

            let body = dump_threads(debug_mode);
            respond(&mut stream, "200 OK", "text/plain; charset=utf-8", body.as_bytes()).await;
            continue;
        }

        // ── Deadlock check (requires feature) ───────────────────────────
        #[cfg(feature = "deadlock")]
        if path == "/debug/pprof/deadlock" {
            let body = dump_deadlocks();
            respond(&mut stream, "200 OK", "text/plain; charset=utf-8", body.as_bytes()).await;
            continue;
        }

        // ── Unsupported Go profile types → 400 ──────────────────────────
        let unsupported = [
            "/debug/pprof/block",
            "/debug/pprof/mutex",
            "/debug/pprof/threadcreate",
            "/debug/pprof/trace",
        ];
        if unsupported.contains(&path) {
            let msg = format!(
                "{} not supported in Rust runtime\nUse /debug/pprof/profile for CPU, /debug/pprof/heap for memory\n",
                path
            );
            respond(&mut stream, "400 Bad Request", "text/plain; charset=utf-8", msg.as_bytes()).await;
            continue;
        }

        // ── 404 ─────────────────────────────────────────────────────────
        respond(
            &mut stream,
            "404 Not Found",
            "text/plain; charset=utf-8",
            b"not found\ntry GET /debug/pprof/\n",
        )
        .await;
    }

    log::info!("pprof server stopped");
    Ok(())
}

// ─── HTML index ──────────────────────────────────────────────────────────────

fn build_index_html() -> String {
    let mut html = String::from(
        "<html>\n<head>\n<title>/debug/pprof/</title>\n<style>\n\
         body { font-family: monospace; margin: 1em; }\n\
         table { border-collapse: collapse; }\n\
         td { padding: 2px 8px; }\n\
         </style>\n</head>\n<body>\n\
         <h2>/debug/pprof/</h2>\n\
         <p>kcptun-rs pprof (Go protobuf format)</p>\n\
         <table>\n",
    );

    html.push_str("<tr><td><b>profile</b></td><td><a href='profile?seconds=30'>CPU profile</a> (go tool pprof -http=:0 .../profile?seconds=30)</td></tr>\n");
    html.push_str("<tr><td><b>heap</b></td><td><a href='heap'>Heap allocation profile</a></td></tr>\n");
    html.push_str("<tr><td><b>allocs</b></td><td><a href='allocs'>Cumulative allocation profile</a></td></tr>\n");
    html.push_str("<tr><td><b>goroutine</b></td><td><a href='goroutine?debug=1'>Thread dump</a> | <a href='goroutine?debug=2'>full stacks</a></td></tr>\n");
    html.push_str("<tr><td><b>cmdline</b></td><td><a href='cmdline'>Command line</a></td></tr>\n");
    html.push_str("<tr><td><b>symbol</b></td><td><a href='symbol'>Symbol lookup</a></td></tr>\n");

    #[cfg(feature = "deadlock")]
    html.push_str("<tr><td><b>deadlock</b></td><td><a href='deadlock'>Active deadlock check</a></td></tr>\n");

    html.push_str(
        "</table>\n\
         <hr>\n\
         <p>Usage: <code>go tool pprof -http=:0 http://ADDR:6060/debug/pprof/profile?seconds=30</code></p>\n\
         </body>\n</html>\n",
    );
    html
}

// ─── Thread dump (goroutine equivalent) ─────────────────────────────────────

fn dump_threads(debug: u64) -> String {
    #[cfg(target_os = "linux")]
    {
        dump_threads_linux(debug)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = debug;
        #[cfg(feature = "deadlock")]
        {
            let deadlocks = parking_lot::deadlock::check_deadlock();
            if deadlocks.is_empty() {
                "no deadlocks detected\nthread dump requires Linux (/proc/self/task)\n".to_string()
            } else {
                dump_deadlocks()
            }
        }
        #[cfg(not(feature = "deadlock"))]
        {
            "thread dump requires Linux (/proc/self/task)\n".to_string()
        }
    }
}

#[cfg(target_os = "linux")]
fn dump_threads_linux(debug: u64) -> String {
    let mut out = String::new();
    let entries = match std::fs::read_dir("/proc/self/task") {
        Ok(e) => e,
        Err(_) => return "cannot read /proc/self/task\n".to_string(),
    };

    let mut threads: Vec<(String, String)> = Vec::new();
    for entry in entries.flatten() {
        let tid = entry.file_name();
        let tid_str = tid.to_string_lossy().to_string();
        let comm = std::fs::read_to_string(format!("/proc/self/task/{tid_str}/comm"))
            .unwrap_or_default()
            .trim()
            .to_string();
        threads.push((tid_str, comm));
    }

    out.push_str(&format!("=== {} threads ===\n\n", threads.len()));

    for (tid, comm) in &threads {
        out.push_str(&format!("thread {tid} ({comm})\n"));

        if debug >= 2 {
            // /proc/self/task/TID/stack requires CAP_SYS_PTRACE or root
            let stack =
                std::fs::read_to_string(format!("/proc/self/task/{tid}/stack"))
                    .unwrap_or_else(|_| "  (no kernel stack available — need CAP_SYS_PTRACE)\n".into());
            out.push_str("kernel stack:\n");
            out.push_str(&stack);

            // Also try /proc/self/task/TID/syscall for current syscall info
            if let Ok(syscall) = std::fs::read_to_string(format!("/proc/self/task/{tid}/syscall")) {
                out.push_str(&format!("syscall: {syscall}"));
            }

            // /proc/self/task/TID/status for state info
            if let Ok(status) = std::fs::read_to_string(format!("/proc/self/task/{tid}/status")) {
                // Extract State and Name lines
                for line in status.lines() {
                    if line.starts_with("State:") || line.starts_with("Name:") {
                        out.push_str(&format!("{line}\n"));
                    }
                }
            }
        }
        out.push('\n');
    }

    // Check for deadlocks via parking_lot if available
    #[cfg(feature = "deadlock")]
    {
        let deadlocks = parking_lot::deadlock::check_deadlock();
        if !deadlocks.is_empty() {
            let total: usize = deadlocks.iter().map(|v| v.len()).sum();
            out.push_str(&format!("\n=== {} DEADLOCK CYCLES ({} threads) ===\n", deadlocks.len(), total));
            for (i, threads) in deadlocks.iter().enumerate() {
                out.push_str(&format!("\nDeadlock cycle #{} ({} threads):\n", i, threads.len()));
                for t in threads {
                    out.push_str(&format!("  Thread Id {:#?}\n", t.thread_id()));
                    out.push_str(&format!("  {:#?}\n", t.backtrace()));
                }
            }
        } else {
            out.push_str("\nno deadlocks detected\n");
        }
    }

    out
}
