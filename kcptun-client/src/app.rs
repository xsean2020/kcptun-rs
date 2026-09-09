//! Application lifecycle: async_main, configuration, and main accept loop.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use log::{error, info};

use crate::cli::{Cli, Config};
use crate::client::{self, ClientDialOptions};
use crate::socket;

enum LocalListener {
    Tcp(knet::TcpListener),
    #[cfg(unix)]
    Unix(knet::UnixListener),
}

impl LocalListener {
    async fn bind(addr: &str) -> Result<Self> {
        match kcptun_common::parse_multi_port(addr) {
            Ok(addrs) => {
                let listen_addr = addrs
                    .into_iter()
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("invalid local address"))?;
                Ok(Self::Tcp(knet::TcpListener::bind(listen_addr).await?))
            }
            Err(tcp_error) => {
                #[cfg(unix)]
                {
                    let listener = knet::UnixListener::bind(addr).await.map_err(|unix_error| {
                        anyhow::anyhow!(
                            "invalid TCP address ({tcp_error}); cannot bind unix socket {addr}: {unix_error}"
                        )
                    })?;
                    Ok(Self::Unix(listener))
                }
                #[cfg(not(unix))]
                Err(tcp_error)
            }
        }
    }

    async fn accept(&self) -> std::io::Result<(knet::TcpStream, String)> {
        match self {
            Self::Tcp(listener) => listener
                .accept()
                .await
                .map(|(stream, peer)| (stream, peer.to_string())),
            #[cfg(unix)]
            Self::Unix(listener) => listener.accept().await,
        }
    }

    fn try_accept(&self) -> std::io::Result<(knet::TcpStream, String)> {
        match self {
            Self::Tcp(listener) => listener
                .try_accept()
                .map(|(stream, peer)| (stream, peer.to_string())),
            #[cfg(unix)]
            Self::Unix(listener) => listener.try_accept(),
        }
    }
}

/// Main async entry point — configuration, session setup, accept loop, and
/// graceful shutdown.  Mirrors the Go kcptun client lifecycle.
pub(crate) async fn async_main() -> Result<()> {
    // Ignore SIGPIPE to prevent crashes when writing to closed sockets.
    knet::ignore_sigpipe();
    // Install SIGUSR1 handler for SNMP stats dump (matching Go kcptun).
    knet::install_sigusr1_handler();
    kcp_rs::snmp_enable();

    let cli = Cli::parse_go_compatible();
    if cli.version_flag {
        println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    // Load config file if specified
    let cli = if let Some(ref config_path) = cli.c {
        let config_str = knet::read_to_string(config_path.clone()).await?;
        let cfg: Config = serde_json::from_str(&config_str)?;
        Cli::merge(cli, cfg)
    } else {
        cli
    };

    // Logging: controlled by RUST_LOG env var, defaults to "info".
    if let Some(ref log_path) = cli.log.as_ref().filter(|s| !s.is_empty()) {
        crate::rotate_log(log_path, 10 * 1024 * 1024);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)?;
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
            .format_timestamp_secs()
            .target(env_logger::Target::Pipe(Box::new(file)))
            .init();
    } else {
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
            .format_timestamp_secs()
            .init();
        info!(
            "log level: {} (set RUST_LOG=debug for verbose output)",
            std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into())
        );
    }

    let local_addr = cli.localaddr.as_deref().unwrap_or(":12948");
    let remote_addr_str = cli.remoteaddr.as_deref().unwrap_or("vps:29900");

    let key_str = cli.key.as_deref().unwrap_or("it's a secrect");
    let crypt = cli.crypt.as_deref().unwrap_or("aes");
    let mode = cli.mode.as_deref().unwrap_or("fast");
    let conn_count = cli.conn.unwrap_or(1);
    anyhow::ensure!(conn_count > 0, "conn must be greater than 0");
    let mtu = cli.mtu.unwrap_or(1350);
    let sndwnd = cli.sndwnd.unwrap_or(128);
    let rcvwnd = cli.rcvwnd.unwrap_or(512);
    let datashard = cli.datashard.unwrap_or(10);
    let parityshard = cli.parityshard.unwrap_or(3);
    let nocomp = cli.nocomp;
    let quiet = cli.quiet;
    let acknodelay = cli.acknodelay;
    let nodelay = cli.nodelay.unwrap_or(0);
    let interval = cli.interval.unwrap_or(50);
    let resend = cli.resend.unwrap_or(0);
    let nc = cli.nc.unwrap_or(0);
    let smuxver = cli.smuxver.unwrap_or(2);
    let smuxbuf = cli.smuxbuf.unwrap_or(4 * 1024 * 1024);
    let streambuf = cli.streambuf.unwrap_or(2097152);
    let framesize = cli.framesize.unwrap_or(8192);
    let sockbuf = cli.sockbuf.unwrap_or(4 * 1024 * 1024);
    let keepalive = cli.keepalive.unwrap_or(10);
    let autoexpire = cli.autoexpire.unwrap_or(0);
    let scavengettl = cli.scavengettl.unwrap_or(600);
    let closewait = cli.closewait.unwrap_or(0).max(0) as u64;
    let ratelimit = cli.ratelimit;
    let dscp = cli.dscp.unwrap_or(0);
    #[cfg(feature = "qpp")]
    let qpp_enabled = cli.qpp;
    #[cfg(not(feature = "qpp"))]
    let qpp_enabled = false;
    #[cfg(feature = "qpp")]
    let qpp_count = cli.qppcount.unwrap_or(61);
    #[cfg(not(feature = "qpp"))]
    let qpp_count = 0u16;

    // Validate QPP parameters (matching Go's ValidateQPPParams)
    #[cfg(feature = "qpp")]
    if qpp_enabled {
        match kcptun_common::validate_qpp_params(qpp_count, key_str.as_bytes()) {
            Ok(warnings) => {
                for w in &warnings {
                    log::warn!("{}", w);
                }
            }
            Err(e) => {
                error!("QPP configuration error: {}", e);
                return Err(anyhow::anyhow!("QPP: {}", e));
            }
        }
    }

    // Derive encryption key
    let key = kcptun_common::derive_key(key_str);
    let session_cfg = ClientDialOptions {
        crypt: crypt.to_string(),
        mode: mode.to_string(),
        mtu,
        sndwnd,
        rcvwnd,
        datashard,
        parityshard,
        acknodelay,
        nodelay,
        interval,
        resend,
        nc,
        smuxver,
        smuxbuf,
        streambuf,
        framesize,
        keepalive: keepalive.max(0) as u64,
        nocomp,
        ratelimit,
    };

    info!(
        "key derived: crypt={}, key={:02x}..{:02x}",
        crypt, key[0], key[31]
    );

    // Validate the remote address once; each actual dial re-resolves DNS and
    // randomly selects a configured port, matching Go.
    kcptun_common::parse_multi_port(remote_addr_str)?;

    if !cli.tcp {
        info!("using shared kcptun session stack");
    }

    // Create KCP connection pool (shared with scavenger for auto-expire)
    let conns: client::SessionPool = Arc::new(parking_lot::Mutex::new(Vec::with_capacity(
        conn_count as usize,
    )));
    // Go keeps every timed session in a separate scavenger list.  Keeping Arc
    // references here ensures a session remains closeable after its pool slot
    // is replaced by a reconnect.
    let tracked_sessions: client::SessionPool = Arc::new(parking_lot::Mutex::new(Vec::new()));
    if cli.tcp {
        // Go maintains `conn` independently of the underlying UDP/tcpraw
        // transport. Build the requested number of tcpraw sessions so the
        // round-robin pool and its configured size cannot diverge.
        for i in 0..conn_count as usize {
            let remote = kcptun_common::random_remote_addr(remote_addr_str)?;
            info!(
                "creating TCP raw KCP connection {}/{} -> {}",
                i + 1,
                conn_count,
                remote
            );
            let socket = socket::create_client_socket(remote, true, sockbuf, dscp)?;
            let conn = Arc::new(client::build_session(remote, &key, &session_cfg, socket).await?);
            conns.lock().push(conn.clone());
            if autoexpire > 0 {
                tracked_sessions.lock().push(conn);
            }
        }
    } else {
        // UDP mode: create conn_count connections
        for i in 0..conn_count as usize {
            let remote = kcptun_common::random_remote_addr(remote_addr_str)?;
            info!(
                "creating KCP connection {}/{} -> {}",
                i + 1,
                conn_count,
                remote
            );
            let socket = socket::create_client_udp_socket(remote, sockbuf, dscp)?;
            let socket = Arc::new(knet::DatagramSocket::Udp(socket));
            let conn = Arc::new(client::build_session(remote, &key, &session_cfg, socket).await?);
            conns.lock().push(conn.clone());
            if autoexpire > 0 {
                tracked_sessions.lock().push(conn);
            }
        }
    }

    info!("established {} KCP connections", conns.lock().len());
    if ratelimit > 0 {
        info!("ratelimit: {} bytes/sec", ratelimit);
    }
    if dscp > 0 {
        info!("dscp: {}", dscp);
    }
    info!("sockbuf: {}", sockbuf);

    // Start SNMP logger if configured
    let stop_flag = Arc::new(AtomicBool::new(false));
    {
        let signal_stop = stop_flag.clone();
        knet::spawn_task(async move {
            kcptun_common::snmp_signal_logger(signal_stop).await;
        });
    }
    if let Some(ref snmplog_path) = cli.snmplog {
        let secs = cli.snmpperiod.unwrap_or(60).max(0) as u64;
        if secs > 0 && !snmplog_path.is_empty() {
            let period = Duration::from_secs(secs);
            let s = stop_flag.clone();
            let p = snmplog_path.clone();
            knet::spawn_task(async move {
                kcptun_common::snmp_logger(p, period, s).await;
            });
        } else {
            log::warn!("snmplog set but snmpperiod=0 or empty path — SNMP collection disabled");
        }
    }

    // Start pprof if configured (requires --features pprof)
    #[cfg(feature = "pprof")]
    if cli.pprof {
        info!("starting pprof HTTP server on :6060");
        #[cfg(feature = "pprof-deadlock")]
        kpprof::start_deadlock_detector();
        let pprof_stop = stop_flag.clone();
        knet::spawn_task(async move {
            if let Err(e) = kpprof::run_pprof("0.0.0.0:6060", pprof_stop).await {
                error!("pprof server error: {}", e);
            }
        });
    }
    #[cfg(not(feature = "pprof"))]
    if cli.pprof {
        log::warn!("--pprof requested but binary built without `pprof` feature; rebuild with --features pprof");
    }

    // Start auto-expire scavenger if enabled (matching Go client).
    //
    // Go scavenger deadline = creation + autoexpire + scavengeTTL.
    // Uses absolute creation time, NOT last activity — keepalive does NOT
    // delay expiry.  The accept loop proactively replaces sessions at
    // `creation + autoexpire`; the scavenger force-closes any session still
    // alive past `creation + autoexpire + scavengeTTL`.
    if autoexpire > 0 {
        let s = stop_flag.clone();
        let scavenge_sessions = tracked_sessions.clone();
        let scavenge_autoexpire = autoexpire.max(0) as u64;
        let scavenge_ttl = scavengettl.max(0) as u64;
        knet::spawn_task(async move {
            info!(
                "scavenger started: autoexpire={}s, scavengettl={}s",
                scavenge_autoexpire, scavenge_ttl
            );
            loop {
                knet::sleep_ms(5000).await;
                if s.load(Ordering::Acquire) {
                    break;
                }
                scavenge_sessions.lock().retain(|conn| {
                    if conn.is_dead() {
                        info!("scavenger: session normally closed");
                        return false;
                    }
                    if client::is_session_scavenge_expired(conn, scavenge_autoexpire, scavenge_ttl)
                    {
                        info!("scavenger: session closed due to ttl");
                        conn.close();
                        return false;
                    }
                    true
                });
            }
        });
    }

    // Go accepts either a TCP address or a Unix-domain socket path here.
    let listener = LocalListener::bind(local_addr).await?;
    info!("listening on {}", local_addr);

    // Spawn Ctrl-C handler (runtime-agnostic)
    {
        let stop = stop_flag.clone();
        knet::spawn_task(async move {
            let _ = knet::ctrl_c().await;
            stop.store(true, Ordering::Relaxed);
        });
    }

    // Accept loop with round-robin across KCP connections
    let round_robin = Arc::new(AtomicUsize::new(0));
    let conn_count_usize = conns.lock().len();
    anyhow::ensure!(conn_count_usize > 0, "connection pool is empty");

    loop {
        if stop_flag.load(Ordering::Relaxed) {
            info!("shutting down...");
            break;
        }

        let (local, peer) = match knet::timeout(Duration::from_millis(500), listener.accept()).await
        {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                error!("accept error: {}", e);
                continue;
            }
            Err(_) => continue, // timeout, loop back to check stop_flag
        };

        // Process the accepted connection, then drain any already-queued
        // connections in the same wakeup. Without this, a burst of concurrent
        // connections is serialized behind per-connection reactor wakeups
        // (measured ~90ms stall on the 2nd accept under the first burst).
        let mut pending = Some((local, peer));
        while let Some((local, peer)) = pending.take() {
            if stop_flag.load(Ordering::Relaxed) {
                info!("shutting down, rejecting new connection from {}", peer);
                break;
            }

            let idx = round_robin.fetch_add(1, Ordering::Relaxed) % conn_count_usize;

            // Ensure a live KCP/SMUX session (Go muxSession.Open auto-redial).
            // Go also proactively reconnects when `now > creation + autoexpire`
            // (absolute deadline, independent of keepalive activity).
            let mut opened: Option<Arc<smux_rs::stream::Stream>> = None;
            for _attempt in 0..2 {
                let needs_reconnect = {
                    let guard = conns.lock();
                    guard[idx].is_dead()
                        || (autoexpire > 0
                            && client::is_session_expired(&guard[idx], autoexpire.max(0) as u64))
                };
                if needs_reconnect {
                    let ok = client::reconnect_session(
                        &conns,
                        (autoexpire > 0).then_some(&tracked_sessions),
                        idx,
                        remote_addr_str,
                        &key,
                        &session_cfg,
                        cli.tcp,
                        sockbuf,
                        dscp,
                    )
                    .await;
                    if !ok {
                        break;
                    }
                }

                let stream_result = {
                    let guard = conns.lock();
                    let c = &guard[idx];
                    match c.open_stream() {
                        Ok(stream) => Some(stream),
                        Err(e) => {
                            error!("failed to open SMUX stream: {:?}", e);
                            c.close();
                            None
                        }
                    }
                };

                match stream_result {
                    Some(s) => {
                        opened = Some(s);
                        break;
                    }
                    None => continue,
                }
            }

            let smux_stream = match opened {
                Some(s) => s,
                None => continue,
            };

            let stream_id = smux_stream.id();
            if !quiet {
                info!("accepted connection from {} (stream {})", peer, stream_id);
            }

            let flush_notify_ref = {
                let guard = conns.lock();
                guard[idx].flush_notify()
            };

            let qpp_key = key.to_vec();
            knet::spawn_task(async move {
                if let Err(e) = client::handle_client(
                    local,
                    smux_stream,
                    qpp_enabled,
                    qpp_key,
                    qpp_count,
                    quiet,
                    flush_notify_ref,
                    closewait,
                )
                .await
                {
                    error!("client handler error (stream {}): {:?}", stream_id, e);
                }
                if !quiet {
                    info!("stream {} closed", stream_id);
                }
            });

            // Drain any other connections already queued in the same wakeup.
            pending = listener.try_accept().ok();
        }
    }

    // Graceful shutdown
    info!("shutting down...");
    knet::sleep_ms(1000).await;
    info!("bye");

    Ok(())
}
