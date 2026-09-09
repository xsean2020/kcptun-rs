//! kpprof — Go-compatible pprof HTTP server for kcptun-rs.
//!
//! Provides a `net/http/pprof`-compatible HTTP server that serves CPU profiles
//! as Go pprof protobuf, analyzable directly with `go tool pprof`.
//!
//! ## Endpoints (matching Go `net/http/pprof`)
//!
//! | Endpoint | Method | Description |
//! |----------|--------|-------------|
//! | `GET /debug/pprof/` | GET | HTML index listing all profile types |
//! | `GET /debug/pprof/profile?seconds=N` | GET | CPU profile as gzip protobuf (default 30s) |
//! | `GET /debug/pprof/cmdline` | GET | Command line args (`\x00` separated) |
//! | `GET/POST /debug/pprof/symbol` | GET/POST | Symbol lookup (Go `num_symbols` format) |
//! | `GET /debug/pprof/heap?gc=1` | GET | Heap allocation profile (gzip protobuf) |
//! | `GET /debug/pprof/allocs` | GET | Cumulative allocation profile (gzip protobuf) |
//! | `GET /debug/pprof/goroutine?debug=N` | GET | Thread dump (debug>0) or empty protobuf (debug=0) |
//! | `GET /debug/pprof/block` | GET | Empty block profile (no block profiling in Rust) |
//! | `GET /debug/pprof/mutex` | GET | Empty mutex profile |
//! | `GET /debug/pprof/threadcreate` | GET | Empty threadcreate profile |
//! | `GET /debug/pprof/deadlock` | GET | Active deadlock check (requires `deadlock` feature) |
//!
//! ## Go compatibility
//!
//! - All protobuf profiles are **application-level gzipped** (matching Go's
//!   `runtime/pprof` which gzips inside the encoder, not via HTTP
//!   `Content-Encoding`). `go tool pprof` detects gzip by magic bytes.
//! - `X-Content-Type-Options: nosniff` on all responses (matching Go).
//! - `Content-Disposition: attachment; filename="..."` on profile responses.
//! - Symbol endpoint supports POST with `+` separated addresses and returns
//!   `num_symbols: 1\n` format (matching Go exactly).
//! - GET-only enforcement (Go 1.22+), except symbol allows POST.
//!
//! ## Known profiling accuracy limitations (pprof-rs on macOS)
//!
//! The CPU profiler uses `pprof-rs` with **ITIMER_REAL** (wall-clock timer) and
//! **frame-pointer unwinding**. This differs from upstream pprof-rs defaults:
//!
//! - **ITIMER_REAL instead of ITIMER_PROF**: ITIMER_PROF only fires during CPU
//!   execution, not during syscall blocking. For I/O-bound tokio servers, 99% of
//!   time is in `kevent`/`epoll_wait` syscalls where ITIMER_PROF doesn't fire,
//!   yielding <1% sample coverage. ITIMER_REAL fires on wall-clock time, giving
//!   ~90% coverage. ITIMER_REAL generates SIGALRM (not SIGPROF); the signal
//!   handler is registered for SIGALRM accordingly.
//! - **Frame-pointer unwinding** (`frame-pointer` feature): Uses frame-pointer
//!   chain walking instead of DWARF unwinding. This automatically stops at
//!   libc/pthread boundaries (system libraries lack frame pointers), producing
//!   clean Rust-only backtraces. Samples with empty backtraces (signal fired
//!   in libc) are dropped.
//!
//! **⚠️ `make vendor` overwrites vendored pprof-rs.** The timer and signal
//! changes in `vendor/pprof/src/{timer.rs,profiler.rs}` must be re-applied
//! after `make vendor`. See `kpprof-rs/Cargo.toml` `frame-pointer` feature.
//!
//! ### Interpreting I/O-bound server profiles
//!
//! For I/O-bound servers (most kcptun-rs usage), the wall-clock CPU profile
//! will be dominated by `tokio::runtime::park::Inner::park` (99%+) — the
//! tokio worker thread waiting for I/O. This is **correct**: the server is
//! spending most wall time waiting, not computing.
//!
//! To find actual CPU hotspots:
//! ```bash
//! # Option 1: Filter out park to see work functions
//! go tool pprof -top -ignore="Inner::park" profile.pb.gz
//!
//! # Option 2: Use stress tests that generate heavy CPU load
//! make stress  # runs data integrity + concurrency stress
//!
//! # Option 3: Use CPU-intensive crypto (3des, small packets)
//! kcptun-server --crypt 3des --mode normal
//! ```
//!
//! ### AES symbol misattribution
//!
//! The `aes` crate's `autodetect` module contains both `armv8` (hardware) and
//! `soft::fixslice` (software) paths. With frame-pointer unwinding, this is
//! less likely to occur, but if you see `aes::soft::fixslice::aes256_encrypt`
//! in the profile, verify hardware AES via:
//! - `nm target/profiling/kcptun-server | grep armv8.*Aes` (symbols present)
//! - `.cargo/config.toml` has `--cfg aes_armv8` for aarch64 targets
//! - Runtime micro-benchmark (hardware: ~5-20 ns/block; software: ~80-150 ns)
//!
//! ## Usage
//!
//! ```ignore
//! // In kcptun-server / kcptun-client main.rs:
//! #[cfg(feature = "pprof")]
//! if let Some(ref addr) = cli.pprof {
//!     let stop = stop_flag.clone();
//!     knet::spawn_task(async move {
//!         let _ = kpprof::run_pprof(&addr, stop).await;
//!     });
//! }
//! ```
//!
//! ## Analysis
//!
//! ```bash
//! # CPU profile (direct HTTP fetch — go tool pprof handles gzip transparently)
//! go tool pprof -http=:0 http://localhost:6060/debug/pprof/profile?seconds=30
//!
//! # CPU profile (save to file, then analyze)
//! curl -o cpu.pb http://localhost:6060/debug/pprof/profile?seconds=30
//! go tool pprof -http=:0 cpu.pb
//!
//! # Heap profile
//! go tool pprof -http=:0 http://localhost:6060/debug/pprof/heap
//!
//! # Thread dump (deadlock detection)
//! curl 'http://localhost:6060/debug/pprof/goroutine?debug=2'
//! ```
//!
//! ## Profiling guide
//!
//! ### Build
//!
//! ```bash
//! # Always use make profiling-bins (bakes in force-frame-pointers=yes)
//! make profiling-bins
//! # Or manually:
//! RUSTFLAGS="-C force-frame-pointers=yes" cargo build --profile profiling --features pprof -p kcptun-server -p kcptun-client
//! ```
//!
//! ### Capture
//!
//! ```bash
//! # One-shot: Rust server → Go pprof protobuf under load
//! bash bench/profile_rust_go_pprof.sh server 20
//! # Or: make profile
//!
//! # Manual: start server with pprof, drive load, capture
//! ./target/profiling/kcptun-server -l :29900 -t 127.0.0.1:8080 \
//!     --key k --crypt aes --nocomp --pprof 127.0.0.1:6060
//! curl -o cpu.pb 'http://127.0.0.1:6060/debug/pprof/profile?seconds=20'
//! go tool pprof -top cpu.pb
//! ```
//!
//! ### Scenario matrix
//!
//! | ID | Command | Purpose |
//! |----|---------|---------|
//! | L1 | `CRYPT=null bash bench/profile_rust_go_pprof.sh server 20` | null/nocomp bulk |
//! | L2 | `CRYPT=aes bash bench/profile_rust_go_pprof.sh server 20` | aes bulk |
//! | L3 | `CRYPT=3des bash bench/profile_rust_go_pprof.sh server 20` | 3des bulk |
//! | L4 | `make stress` | stress 10-conn |
//!
//! ### Symbol map (frame patterns → layer)
//!
//! | Frame pattern | Layer |
//! |---------------|--------|
//! | `encrypt_batch` / `should_cpu_block_encrypt` | Crypto batch |
//! | `CryptEngine` / cipher `encrypt` / CFB | Block crypt |
//! | `aes::armv8::*` / `aes::ni::*` | Hardware AES |
//! | `TripleDesCipher::encrypt_block` | 3DES |
//! | `KCP::flush` / `input` / `send` / `SegmentPool` | ARQ |
//! | `encode_header_into` / SMUX flush | Mux |
//! | snappy | Compression (off with `--nocomp`) |
//! | `send_batch` / `UdpSocket::send_to` | UDP I/O |
//! | `tokio::runtime::park::Inner::park` | I/O wait (filter with `-ignore`) |
//! | `cpu_block` | Scheduling / offload |
//!
//! ### Optimization decision tree
//!
//! 1. **Profile shows 99% `Inner::park`** → I/O-bound; filter with `-ignore="Inner::park"`.
//! 2. **Cipher inner loop (L2/L3)** → algorithm micro-opts; verify not residual `dyn`.
//! 3. **Copy / Bytes churn (L1)** → ownership pipeline.
//! 4. **Lock / mutex (L1/L4)** → shorten critical sections; never hold KCP lock across encrypt/snappy.
//! 5. **Syscall / send** → batch send; Linux `sendmmsg` only if justified.
//! 6. **No actionable ≥~5% leaf** → stop coding; document in `bench/profiles/HOTSPOTS.md`.
//!
//! Hard rules: wire compatibility; no congestion cheats; one optimization class per change;
//! shared `encrypt_batch`.
//!
//! ## Vendored pprof-rs modifications
//!
//! The vendored `pprof` crate (`vendor/pprof/`) has two modifications vs upstream
//! that are essential for usable profiles on I/O-bound servers:
//!
//! 1. **`timer.rs`**: `ITIMER_PROF` → `ITIMER_REAL` (wall-clock timer).
//!    ITIMER_PROF only fires during CPU execution → <1% coverage for I/O-bound
//!    tokio servers. ITIMER_REAL gives ~90% coverage.
//!
//! 2. **`profiler.rs`**: `SIGPROF` → `SIGALRM` (ITIMER_REAL generates SIGALRM),
//!    and empty backtraces are dropped (signal fired in libc without frame pointers).
//!
//! **⚠️ `make vendor` overwrites these changes.** Re-apply them after vendoring:
//! ```bash
//! git diff vendor/pprof/src/timer.rs vendor/pprof/src/profiler.rs
//! git checkout -- vendor/pprof/src/timer.rs vendor/pprof/src/profiler.rs
//! # Then: python3 -c "update .cargo-checksum.json" (see Makefile vendor target)
//! ```

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};

#[cfg(unix)]
use pprof::protos::{self as protos, Message};

// ─── Heap profiling (always compiled; zero-cost when sample_rate == 0) ───────

mod heap;
pub use heap::{build_allocs_profile, build_heap_profile, ProfilingAllocator};

// ─── Deadlock detection (optional) ──────────────────────────────────────────

#[cfg(feature = "deadlock")]
mod deadlock;

#[cfg(feature = "deadlock")]
pub use deadlock::{dump_deadlocks, start_deadlock_detector};

// ─── Main entry point ───────────────────────────────────────────────────────

/// Start the pprof HTTP server compatible with `go tool pprof`.
///
/// Listens on `addr` and serves `/debug/pprof/*` endpoints.
/// Returns when `stop` is set to `true`.
pub async fn run_pprof(addr: &str, stop: Arc<AtomicBool>) -> Result<()> {
    use knet::AsyncReadExt;

    let socket_addr: SocketAddr = addr.parse().context("invalid pprof address")?;
    let listener = knet::TcpListener::bind(socket_addr).await?;
    log::info!("pprof listening on http://{}/debug/pprof/", socket_addr);

    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let accepted = knet::timeout(Duration::from_millis(500), listener.accept()).await;
        #[allow(unused_variables)]
        let (mut stream, peer) = match accepted {
            Ok(Ok(v)) => v,
            _ => continue,
        };

        // Read HTTP request headers
        let mut buf = vec![0u8; 8192];
        let mut filled = 0usize;
        let mut header_end: Option<usize> = None;
        loop {
            if filled >= buf.len() {
                break;
            }
            match knet::timeout(Duration::from_secs(2), stream.read(&mut buf[filled..])).await {
                Ok(Ok(0)) => break,
                Ok(Ok(n)) => {
                    filled += n;
                    if let Some(pos) = buf[..filled].windows(4).position(|w| w == b"\r\n\r\n") {
                        header_end = Some(pos + 4);
                        break;
                    }
                    if buf[..filled].windows(2).any(|w| w == b"\n\n") {
                        // find the \n\n position
                        if let Some(pos) = buf[..filled].windows(2).position(|w| w == b"\n\n") {
                            header_end = Some(pos + 2);
                        }
                        break;
                    }
                }
                _ => break,
            }
        }

        let req = String::from_utf8_lossy(&buf[..filled]);
        let first_line = req.lines().next().unwrap_or("");
        let parts: Vec<&str> = first_line.split_whitespace().collect();
        let method = parts.first().copied().unwrap_or("");
        let path_q = parts.get(1).copied().unwrap_or("/");
        let (path, query) = match path_q.split_once('?') {
            Some((p, q)) => (p, q),
            None => (path_q, ""),
        };

        // Parse Content-Length for POST body
        let mut content_length: usize = 0;
        for line in req.lines().skip(1) {
            let lower = line.to_lowercase();
            if let Some(v) = lower.strip_prefix("content-length:") {
                if let Ok(n) = v.trim().parse() {
                    content_length = n;
                }
            }
        }

        // Read POST body if present (for symbol endpoint)
        let mut post_body: Vec<u8> = Vec::new();
        if method == "POST" && content_length > 0 {
            if let Some(hend) = header_end {
                if filled > hend {
                    post_body.extend_from_slice(&buf[hend..filled]);
                }
            }
            while post_body.len() < content_length {
                let mut tmp = [0u8; 4096];
                match knet::timeout(Duration::from_secs(2), stream.read(&mut tmp)).await {
                    Ok(Ok(0)) => break,
                    Ok(Ok(n)) => post_body.extend_from_slice(&tmp[..n]),
                    _ => break,
                }
            }
            post_body.truncate(content_length);
        }

        // ════════════════════════════════════════════════════════════════
        // Route dispatch
        // ════════════════════════════════════════════════════════════════

        // ── HTML index ──────────────────────────────────────────────────
        if path == "/debug/pprof/" || path == "/debug/pprof" {
            if method != "GET" && method != "HEAD" && !method.is_empty() {
                respond(
                    &mut stream,
                    "405 Method Not Allowed",
                    "text/plain; charset=utf-8",
                    "",
                    b"method not allowed\n",
                )
                .await;
                continue;
            }
            let body = build_index_html();
            respond(
                &mut stream,
                "200 OK",
                "text/html; charset=utf-8",
                "",
                body.as_bytes(),
            )
            .await;
            continue;
        }

        // ── CPU profile ─────────────────────────────────────────────────
        //
        // pprof-rs relies on POSIX signals (SIGPROF/SIGALRM) + setitimer(2),
        // which do not exist on Windows. On non-Unix platforms the CPU
        // profile endpoint returns 501 Not Implemented.
        #[cfg(unix)]
        if path == "/debug/pprof/profile" {
            if method != "GET" && method != "HEAD" && !method.is_empty() {
                respond(
                    &mut stream,
                    "405 Method Not Allowed",
                    "text/plain; charset=utf-8",
                    "",
                    b"method not allowed\n",
                )
                .await;
                continue;
            }
            let mut seconds: u64 = 30;
            for part in query.split('&') {
                if let Some(v) = part.strip_prefix("seconds=") {
                    if let Ok(n) = v.parse::<u64>() {
                        seconds = n.clamp(1, 300);
                    }
                }
            }
            log::info!("pprof CPU profile {}s peer={}", seconds, peer);

            let profile_result =
                knet::cpu_block(move || -> std::result::Result<Vec<u8>, String> {
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
                        "",
                        msg.as_bytes(),
                    )
                    .await;
                    continue;
                }
            };

            // Always gzip the protobuf body (application-level, matching Go's
            // runtime/pprof which gzips inside the encoder). Do NOT set
            // Content-Encoding header — go tool pprof detects gzip by magic
            // bytes, matching Go's net/http/pprof behavior exactly.
            let body = gzip_bytes(&profile_bytes);
            respond(
                &mut stream,
                "200 OK",
                "application/octet-stream",
                "Content-Disposition: attachment; filename=\"profile\"\r\n",
                &body,
            )
            .await;
            log::info!(
                "pprof CPU profile complete ({} bytes gzipped from {} raw) peer={}",
                body.len(),
                profile_bytes.len(),
                peer
            );
            continue;
        }

        #[cfg(not(unix))]
        if path == "/debug/pprof/profile" {
            let msg = "CPU profiling not supported on this platform\npprof-rs requires POSIX signals (SIGPROF/SIGALRM)\n";
            respond(
                &mut stream,
                "501 Not Implemented",
                "text/plain; charset=utf-8",
                "",
                msg.as_bytes(),
            )
            .await;
            continue;
        }

        // ── Command line ────────────────────────────────────────────────
        if path == "/debug/pprof/cmdline" {
            if method != "GET" && method != "HEAD" && !method.is_empty() {
                respond(
                    &mut stream,
                    "405 Method Not Allowed",
                    "text/plain; charset=utf-8",
                    "",
                    b"method not allowed\n",
                )
                .await;
                continue;
            }
            let args: Vec<Vec<u8>> = std::env::args()
                .map(|s| {
                    let mut b = s.into_bytes();
                    b.push(0u8);
                    b
                })
                .collect();
            let body: Vec<u8> = args.into_iter().flatten().collect();
            respond(
                &mut stream,
                "200 OK",
                "text/plain; charset=utf-8",
                "",
                &body,
            )
            .await;
            continue;
        }

        // ── Symbol lookup (Go-compatible: GET + POST, num_symbols format) ─
        if path == "/debug/pprof/symbol" {
            if method != "GET" && method != "POST" && method != "HEAD" && !method.is_empty() {
                respond(
                    &mut stream,
                    "405 Method Not Allowed",
                    "text/plain; charset=utf-8",
                    "",
                    b"method not allowed\n",
                )
                .await;
                continue;
            }

            // Go's symbol handler:
            // 1. Always writes "num_symbols: 1\n" (pprof only checks > 0)
            // 2. Reads addresses from raw query (GET) or body (POST)
            // 3. Addresses separated by '+'
            // 4. For each address: "0xADDR funcname\n"
            let addr_source: Vec<u8> = if method == "POST" {
                post_body
            } else {
                // Go uses r.URL.RawQuery, which is the raw query string.
                // The format is "0x123+0x456" (not "address=0x123&address=0x456").
                // But we also support "address=0x123&address=0x456" for convenience.
                query.as_bytes().to_vec()
            };

            let mut symbols = String::new();
            symbols.push_str("num_symbols: 1\n");

            // Parse addresses from the source string.
            // Go splits on '+' and parses each part as uint64 (base 0 = auto-detect 0x).
            // We also handle "address=0x..." format for backward compat.
            let source_str = String::from_utf8_lossy(&addr_source);
            let addresses: Vec<&str> = if source_str.contains("address=") {
                // Backward compat: "address=0x123&address=0x456"
                source_str
                    .split('&')
                    .filter_map(|p| p.strip_prefix("address="))
                    .collect()
            } else {
                // Go format: "0x123+0x456" or "0x123"
                source_str.split('+').collect()
            };

            for addr_str in &addresses {
                let addr_str = addr_str.trim();
                if addr_str.is_empty() {
                    continue;
                }
                let addr_parsed = if let Some(hex) = addr_str.strip_prefix("0x") {
                    usize::from_str_radix(hex, 16).ok()
                } else {
                    addr_str.parse::<usize>().ok()
                };
                if let Some(addr) = addr_parsed {
                    if addr == 0 {
                        continue;
                    }
                    let mut found = false;
                    backtrace::resolve(addr as *mut std::ffi::c_void, |sym| {
                        if let Some(name) = sym.name() {
                            symbols.push_str(&format!("{addr:#x} {name}\n"));
                            found = true;
                        }
                    });
                    if !found {
                        symbols.push_str(&format!("{addr:#x} ?\n"));
                    }
                }
            }

            respond(
                &mut stream,
                "200 OK",
                "text/plain; charset=utf-8",
                "",
                symbols.as_bytes(),
            )
            .await;
            continue;
        }

        // ── Heap profile ────────────────────────────────────────────────
        if path == "/debug/pprof/heap" {
            if method != "GET" && method != "HEAD" && !method.is_empty() {
                respond(
                    &mut stream,
                    "405 Method Not Allowed",
                    "text/plain; charset=utf-8",
                    "",
                    b"method not allowed\n",
                )
                .await;
                continue;
            }
            // Go supports ?gc=1 to run GC before sampling.
            // In Rust we can't force mimalloc GC, but we can hint.
            let gc_requested: bool = query.split('&').any(|p| {
                p.strip_prefix("gc=")
                    .is_some_and(|v| v.parse::<u32>().is_ok_and(|n| n > 0))
            });
            if gc_requested {
                // mimalloc exposes mi_collect via FFI; a lightweight hint.
                // For now just log — actual GC is handled by mimalloc internally.
                log::debug!("pprof heap: gc requested (mimalloc auto-manages)");
            }

            let debug_mode: u64 = query
                .split('&')
                .find_map(|p| p.strip_prefix("debug=")?.parse().ok())
                .unwrap_or(0);

            let profile = build_heap_profile();
            if profile.is_empty() {
                let msg =
                    "heap profiling not enabled (build without ProfilingAllocator or rate=0)\n";
                respond(
                    &mut stream,
                    "200 OK",
                    "text/plain; charset=utf-8",
                    "",
                    msg.as_bytes(),
                )
                .await;
            } else if debug_mode > 0 {
                // Text format: just report summary (full text format is complex)
                let summary = format!(
                    "heap profile (debug={}): {} bytes of profile data\nUse `go tool pprof` without debug for full analysis.\n",
                    debug_mode, profile.len()
                );
                respond(
                    &mut stream,
                    "200 OK",
                    "text/plain; charset=utf-8",
                    "",
                    summary.as_bytes(),
                )
                .await;
            } else {
                let body = gzip_bytes(&profile);
                respond(
                    &mut stream,
                    "200 OK",
                    "application/octet-stream",
                    "Content-Disposition: attachment; filename=\"heap\"\r\n",
                    &body,
                )
                .await;
            }
            continue;
        }

        // ── Allocs profile ──────────────────────────────────────────────
        if path == "/debug/pprof/allocs" {
            if method != "GET" && method != "HEAD" && !method.is_empty() {
                respond(
                    &mut stream,
                    "405 Method Not Allowed",
                    "text/plain; charset=utf-8",
                    "",
                    b"method not allowed\n",
                )
                .await;
                continue;
            }
            let debug_mode: u64 = query
                .split('&')
                .find_map(|p| p.strip_prefix("debug=")?.parse().ok())
                .unwrap_or(0);

            let profile = build_allocs_profile();
            if profile.is_empty() {
                let msg = "allocation profiling not enabled\n";
                respond(
                    &mut stream,
                    "200 OK",
                    "text/plain; charset=utf-8",
                    "",
                    msg.as_bytes(),
                )
                .await;
            } else if debug_mode > 0 {
                let summary = format!(
                    "allocs profile (debug={}): {} bytes of profile data\nUse `go tool pprof` without debug for full analysis.\n",
                    debug_mode, profile.len()
                );
                respond(
                    &mut stream,
                    "200 OK",
                    "text/plain; charset=utf-8",
                    "",
                    summary.as_bytes(),
                )
                .await;
            } else {
                let body = gzip_bytes(&profile);
                respond(
                    &mut stream,
                    "200 OK",
                    "application/octet-stream",
                    "Content-Disposition: attachment; filename=\"allocs\"\r\n",
                    &body,
                )
                .await;
            }
            continue;
        }

        // ── Goroutine (thread dump / empty protobuf) ────────────────────
        if path == "/debug/pprof/goroutine" {
            if method != "GET" && method != "HEAD" && !method.is_empty() {
                respond(
                    &mut stream,
                    "405 Method Not Allowed",
                    "text/plain; charset=utf-8",
                    "",
                    b"method not allowed\n",
                )
                .await;
                continue;
            }
            let debug_mode: u64 = query
                .split('&')
                .find_map(|p| p.strip_prefix("debug=")?.parse().ok())
                .unwrap_or(1);

            if debug_mode == 0 {
                // Go returns goroutine profile as gzipped protobuf.
                // We don't have a goroutine profiler, so return an empty valid
                // profile so `go tool pprof` doesn't error.
                let empty = empty_profile("goroutine", "count");
                let body = gzip_bytes(&empty);
                respond(
                    &mut stream,
                    "200 OK",
                    "application/octet-stream",
                    "Content-Disposition: attachment; filename=\"goroutine\"\r\n",
                    &body,
                )
                .await;
            } else {
                let body = dump_threads(debug_mode);
                respond(
                    &mut stream,
                    "200 OK",
                    "text/plain; charset=utf-8",
                    "",
                    body.as_bytes(),
                )
                .await;
            }
            continue;
        }

        // ── Empty profiles for Go runtime-only types ───────────────────
        // Go returns valid (possibly empty) profiles for these. We return
        // empty valid protobuf so `go tool pprof` doesn't error.
        if path == "/debug/pprof/block" {
            let body = gzip_bytes(&empty_profile("contentions", "events"));
            respond(
                &mut stream,
                "200 OK",
                "application/octet-stream",
                "Content-Disposition: attachment; filename=\"block\"\r\n",
                &body,
            )
            .await;
            continue;
        }
        if path == "/debug/pprof/mutex" {
            let body = gzip_bytes(&empty_profile("contentions", "events"));
            respond(
                &mut stream,
                "200 OK",
                "application/octet-stream",
                "Content-Disposition: attachment; filename=\"mutex\"\r\n",
                &body,
            )
            .await;
            continue;
        }
        if path == "/debug/pprof/threadcreate" {
            let body = gzip_bytes(&empty_profile("threads", "count"));
            respond(
                &mut stream,
                "200 OK",
                "application/octet-stream",
                "Content-Disposition: attachment; filename=\"threadcreate\"\r\n",
                &body,
            )
            .await;
            continue;
        }

        // ── Trace (not supported in Rust) ──────────────────────────────
        if path == "/debug/pprof/trace" {
            let msg =
                "trace not supported in Rust runtime\nUse /debug/pprof/profile for CPU profiling\n";
            respond(
                &mut stream,
                "400 Bad Request",
                "text/plain; charset=utf-8",
                "",
                msg.as_bytes(),
            )
            .await;
            continue;
        }

        // ── Deadlock check (requires feature) ───────────────────────────
        #[cfg(feature = "deadlock")]
        if path == "/debug/pprof/deadlock" {
            let body = dump_deadlocks();
            respond(
                &mut stream,
                "200 OK",
                "text/plain; charset=utf-8",
                "",
                body.as_bytes(),
            )
            .await;
            continue;
        }

        // ── 404 ─────────────────────────────────────────────────────────
        respond(
            &mut stream,
            "404 Not Found",
            "text/plain; charset=utf-8",
            "",
            b"not found\ntry GET /debug/pprof/\n",
        )
        .await;
    }

    log::info!("pprof server stopped");
    Ok(())
}

// ─── HTTP response helper ────────────────────────────────────────────────────

/// Send an HTTP response.
///
/// Always includes `X-Content-Type-Options: nosniff` (matching Go's
/// `net/http/pprof`). `extra_headers` is inserted before Content-Length
/// (e.g. `Content-Disposition: attachment; filename="profile"\r\n`).
async fn respond(
    stream: &mut knet::TcpStream,
    status: &str,
    ctype: &str,
    extra_headers: &str,
    body: &[u8],
) {
    use knet::AsyncWriteExt;

    let header = format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: {ctype}\r\n\
         X-Content-Type-Options: nosniff\r\n\
         {extra_headers}\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(header.as_bytes()).await;
    let _ = stream.write_all(body).await;
    let _ = stream.flush().await;
}

// ─── Gzip helper ─────────────────────────────────────────────────────────────

/// Gzip-compress a byte slice (application-level, matching Go's `runtime/pprof`
/// which gzips inside the profile encoder, NOT via HTTP Content-Encoding).
///
/// `go tool pprof` detects gzip by magic bytes (`\x1f\x8b`), so we do not set
/// `Content-Encoding: gzip` — this matches Go's `net/http/pprof` exactly.
fn gzip_bytes(data: &[u8]) -> Vec<u8> {
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write;

    let mut encoder = GzEncoder::new(Vec::with_capacity(data.len() / 2), Compression::default());
    if encoder.write_all(data).is_ok() {
        if let Ok(compressed) = encoder.finish() {
            return compressed;
        }
    }
    // Fallback: return raw data if gzip fails (shouldn't happen in practice)
    data.to_vec()
}

// ─── Empty pprof profile builder ─────────────────────────────────────────────

/// Build a minimal valid pprof protobuf with the given sample type and 0 samples.
///
/// Used for Go runtime-only profile types (block, mutex, threadcreate, goroutine
/// debug=0) so `go tool pprof` doesn't error when fetching them.
#[cfg(unix)]
fn empty_profile(sample_type: &str, sample_unit: &str) -> Vec<u8> {
    // string_table: ["", sample_type, sample_unit]
    // sample_type: [{ty: 1, unit: 2}]
    // 0 samples, 0 functions, 0 locations, 0 mappings
    let profile = protos::Profile {
        sample_type: vec![protos::ValueType {
            ty: 1,
            unit: 2,
            ..Default::default()
        }],
        string_table: vec![
            "".to_string(),
            sample_type.to_string(),
            sample_unit.to_string(),
        ],
        ..Default::default()
    };
    let mut content = Vec::new();
    if profile.write_to_vec(&mut content).is_err() {
        log::error!("failed to serialize empty profile");
        return Vec::new();
    }
    content
}

/// Non-Unix stub: returns empty Vec. pprof-rs protobuf types are unavailable
/// on platforms without POSIX signals. The HTTP endpoints that call this will
/// serve a 0-length gzipped body, which `go tool pprof` handles gracefully.
#[cfg(not(unix))]
fn empty_profile(_sample_type: &str, _sample_unit: &str) -> Vec<u8> {
    Vec::new()
}

// ─── HTML index (Go-style) ───────────────────────────────────────────────────

fn build_index_html() -> String {
    // Match Go's net/http/pprof index page format closely.
    // Go lists profiles in a table with Count + Profile name, plus descriptions.
    let mut html = String::from(
        "<html>\n\
         <head>\n\
         <title>/debug/pprof/</title>\n\
         <style>\n\
         .profile-name{\n\
         \tdisplay:inline-block;\n\
         \twidth:6rem;\n\
         }\n\
         </style>\n\
         </head>\n\
         <body>\n\
         /debug/pprof/\n\
         <br>\n\
         <p>Set debug=1 as a query parameter to export in legacy text format</p>\n\
         <br>\n\
         Types of profiles available:\n\
         <table>\n\
         <thead><td>Count</td><td>Profile</td></thead>\n",
    );

    // Profile entries: (name, count, description)
    // Count is 0 for most since we don't track Go-style profile counts.
    let entries: &[(&str, &str)] = &[
        ("allocs", "A sampling of all past memory allocations"),
        ("block", "Stack traces that led to blocking on synchronization primitives (empty — no block profiling in Rust)"),
        ("cmdline", "The command line invocation of the current program"),
        ("goroutine", "Stack traces of all current threads. Use debug=2 as a query parameter to export in the same format as an unrecovered panic."),
        ("heap", "A sampling of memory allocations of live objects. You can specify the gc GET parameter to run GC before taking the heap sample."),
        ("mutex", "Stack traces of holders of contended mutexes (empty — no mutex profiling in Rust)"),
        ("profile", "CPU profile. You can specify the duration in the seconds GET parameter. After you get the profile file, use the go tool pprof command to investigate the profile."),
        ("symbol", "Maps given program counters to function names. Counters can be specified in a GET raw query or POST body, multiple counters are separated by '+'."),
        ("threadcreate", "Stack traces that led to the creation of new OS threads (empty — no threadcreate profiling in Rust)"),
        ("trace", "A trace of execution of the current program (not supported in Rust runtime)"),
    ];

    for (name, _desc) in entries {
        let debug_link = if *name == "profile" {
            format!("{name}?seconds=30")
        } else if *name == "trace" {
            format!("{name}?seconds=1")
        } else {
            format!("{name}?debug=1")
        };
        html.push_str(&format!(
            "<tr><td>0</td><td><a href='{debug_link}'>{name}</a></td></tr>\n"
        ));
    }

    html.push_str("</table>\n");
    html.push_str("<a href=\"goroutine?debug=2\">full goroutine stack dump</a>\n<br>\n");

    // Deadlock link (if feature enabled)
    #[cfg(feature = "deadlock")]
    html.push_str("<a href=\"deadlock\">deadlock check</a>\n<br>\n");

    html.push_str("<p>\nProfile Descriptions:\n<ul>\n");
    for (name, desc) in entries {
        html.push_str(&format!(
            "<li><div class=profile-name>{name}: </div> {desc}</li>\n"
        ));
    }
    html.push_str("</ul>\n</p>\n");

    html.push_str("<hr>\n");
    html.push_str("<p>kcptun-rs pprof (Go protobuf format, application-level gzip)\n<br>\n");
    html.push_str("Usage: <code>go tool pprof -http=:0 http://ADDR:6060/debug/pprof/profile?seconds=30</code>\n</p>\n");
    html.push_str("</body>\n</html>\n");

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
            let stack = std::fs::read_to_string(format!("/proc/self/task/{tid}/stack"))
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
            out.push_str(&format!(
                "\n=== {} DEADLOCK CYCLES ({} threads) ===\n",
                deadlocks.len(),
                total
            ));
            for (i, threads) in deadlocks.iter().enumerate() {
                out.push_str(&format!(
                    "\nDeadlock cycle #{} ({} threads):\n",
                    i,
                    threads.len()
                ));
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
