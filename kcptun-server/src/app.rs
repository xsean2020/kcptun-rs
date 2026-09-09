//! Application lifecycle: async_main, configuration, and server accept loop.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as AnyContext, Result};
use kcp_rs::PacketTransport;
use log::{error, info, warn};

use crate::cli::{Cli, Config};
use crate::server;
use crate::socket;

pub(crate) async fn async_main(cli: Cli) -> Result<()> {
    // Ignore SIGPIPE to prevent crashes when writing to closed sockets.
    knet::ignore_sigpipe();
    // Install SIGUSR1 handler for SNMP stats dump (matching Go kcptun).
    knet::install_sigusr1_handler();
    kcp_rs::snmp_enable();

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

    // Set up logging: redirect to file if --log is specified
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

    let listen = cli.listen.as_deref().unwrap_or(":29900");
    let target = cli.target.as_deref().unwrap_or("127.0.0.1:12948");

    let key_str = cli.key.as_deref().unwrap_or("it's a secrect");
    let crypt_method = cli.crypt.as_deref().unwrap_or("aes");
    let mode = cli.mode.as_deref().unwrap_or("fast");
    let mtu = cli.mtu.unwrap_or(1350);
    let sndwnd = cli.sndwnd.unwrap_or(1024);
    let rcvwnd = cli.rcvwnd.unwrap_or(1024);
    let datashard = cli.datashard;
    let parityshard = cli.parityshard;
    let dscp_val = cli.dscp.unwrap_or(0);
    let sockbuf = cli.sockbuf.unwrap_or(4 * 1024 * 1024);
    let nocomp = cli.nocomp;
    let acknodelay = cli.acknodelay;
    let nodelay = cli.nodelay.unwrap_or(0);
    let interval = cli.interval.unwrap_or(50);
    let resend = cli.resend.unwrap_or(0);
    let nc = cli.nc.unwrap_or(0);
    let smuxver = cli.smuxver.unwrap_or(2);
    let smuxbuf = cli.smuxbuf.unwrap_or(4 * 1024 * 1024);
    let streambuf = cli.streambuf;
    let framesize = cli.framesize;
    let keepalive = cli.keepalive.unwrap_or(10);
    let ratelimit_val = cli.ratelimit;
    let close_wait_val = cli.closewait.unwrap_or(30).max(0) as u64;
    let quiet = cli.quiet;
    #[cfg(feature = "qpp")]
    let qpp_enabled = cli.qpp;
    #[cfg(not(feature = "qpp"))]
    let qpp_enabled = false;
    #[cfg(feature = "qpp")]
    let qpp_count = cli.qppcount.unwrap_or(61);
    #[cfg(not(feature = "qpp"))]
    let qpp_count: u16 = 0;

    // Validate QPP parameters (matching Go's ValidateQPPParams)
    #[cfg(feature = "qpp")]
    if qpp_enabled {
        match kcptun_common::validate_qpp_params(qpp_count, key_str.as_bytes()) {
            Ok(warnings) => {
                for w in &warnings {
                    warn!("{}", w);
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
    info!(
        "key derived: crypt={}, key={:02x}..{:02x}",
        crypt_method, key[0], key[31]
    );

    // Bind listen address(es) — multi-port "host:min-max" matches Go ParseMultiPort.
    let listen_addrs = kcptun_common::parse_multi_port(listen).context("invalid listen address")?;

    // Prepare shared state (needed by both TCP and UDP paths).
    let stop_flag = Arc::new(AtomicBool::new(false));
    {
        let signal_stop = stop_flag.clone();
        knet::spawn_task(async move {
            kcptun_common::snmp_signal_logger(signal_stop).await;
        });
    }
    let target_str = target.to_string();
    let key_arr = key;
    let kcp_config = kcptun_common::KcpCliParams {
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
        conv: kcptun_common::DEFAULT_CONV,
        token: 0,
    }
    .to_kcp_config();
    let smux_config = smux_rs::Config {
        version: smuxver,
        max_receive_buffer: smuxbuf,
        max_stream_buffer: streambuf,
        max_frame_size: framesize,
        keepalive_interval: keepalive.max(0) as u64,
        // Go's BuildSmuxConfig changes only the interval; timeout remains 30s.
        keepalive_timeout: 30,
    };
    let session_config = kcptun_common::KcptunConfig {
        kcp: kcp_config,
        smux: smux_config,
        nocomp,
        rate_limit: ratelimit_val,
        offload_profile: kcrypt_rs::OffloadProfile::Tokio,
    };

    // TCP mode: additionally accept raw TCP connections alongside the
    // always-on UDP listener, each TCP conn a dedicated KCP session
    // (matches Go: `--tcp` exposes a tcpraw listener alongside UDP).
    if cli.tcp {
        #[cfg(not(target_os = "linux"))]
        warn!("--tcp requires Linux (raw sockets + TCP_REPAIR) — serving UDP only");

        if cfg!(target_os = "linux") {
            let key = key_arr;
            for &addr in &listen_addrs {
                let listener = match knet::tcpraw_listen(&addr) {
                    Ok(l) => l,
                    Err(e) => {
                        warn!("tcpraw listen on {} failed: {}", addr, e);
                        continue;
                    }
                };
                if dscp_val > 0 {
                    if let Err(e) = listener.set_dscp(dscp_val) {
                        warn!("SetDSCP({}) failed on tcpraw listener: {}", dscp_val, e);
                    }
                }
                info!("listening on {} for TCP raw KCP connections", addr);
                let crypt = crypt_method.to_string();
                let session_config = session_config.clone();
                let target_loop = target.to_string();
                let qpp_key_loop = key.to_vec();
                knet::spawn_task(async move {
                    loop {
                        let (conn, peer) = match listener.accept().await {
                            Ok(c) => c,
                            Err(e)
                                if e.kind() == std::io::ErrorKind::WouldBlock
                                    || e.kind() == std::io::ErrorKind::Interrupted =>
                            {
                                knet::sleep_ms(10).await;
                                continue;
                            }
                            Err(e) => {
                                error!("TCP accept error on {}: {}", addr, e);
                                break;
                            }
                        };
                        info!("TCP raw session from {}", peer);
                        let socket = Arc::new(knet::DatagramSocket::TcpRaw(conn));
                        let session = match kcptun_common::KcptunSession::serve_transport(
                            socket,
                            peer,
                            &key,
                            &crypt,
                            &session_config,
                        )
                        .await
                        {
                            Ok(session) => Arc::new(session),
                            Err(error) => {
                                warn!("failed to create TCP raw session from {}: {}", peer, error);
                                continue;
                            }
                        };
                        server::spawn_session_stream_loop(
                            session,
                            peer,
                            target_loop.clone(),
                            qpp_enabled,
                            qpp_key_loop.clone(),
                            qpp_count,
                            quiet,
                            close_wait_val,
                            None,
                        );
                    }
                });
            }
            info!("forwarding to TCP target {}", target);
            if ratelimit_val > 0 {
                info!("ratelimit: {} bytes/sec", ratelimit_val);
            }
            info!("sockbuf: {}", sockbuf);
        }
    }

    // SO_REUSEPORT shard count: 0 (default) is platform-aware — Linux binds
    // one shard per logical CPU (kernel hashes peers across the sockets →
    // parallel workers, no shared-fd send contention); non-Linux (Darwin does
    // not SO_REUSEPORT-distribute) defaults to a single socket. Explicit
    // `--shards N` overrides either default.
    let shards = if cli.shards == 0 {
        #[cfg(target_os = "linux")]
        {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1)
        }
        #[cfg(not(target_os = "linux"))]
        {
            1
        }
    } else {
        cli.shards as usize
    };
    let mut udp_sockets: Vec<std::net::UdpSocket> = Vec::with_capacity(listen_addrs.len() * shards);
    for addr in &listen_addrs {
        for s in 0..shards {
            let socket = if shards > 1 {
                socket::create_udp_socket_shard_std(*addr, sockbuf, dscp_val)?
            } else {
                socket::create_udp_socket_std(*addr, sockbuf, dscp_val)?
            };
            if shards > 1 {
                info!(
                    "listening on {} for KCP connections (shard {}/{})",
                    addr,
                    s + 1,
                    shards
                );
            } else {
                info!("listening on {} for KCP connections", addr);
            }
            udp_sockets.push(socket);
        }
    }
    info!("forwarding to TCP target {}", target);
    if ratelimit_val > 0 {
        info!("ratelimit: {} bytes/sec", ratelimit_val);
    }
    if dscp_val > 0 {
        info!("dscp: {}", dscp_val);
    }
    info!("sockbuf: {}", sockbuf);

    // Start SNMP logger if configured
    if let Some(ref snmplog_path) = cli.snmplog {
        let secs = cli.snmpperiod.unwrap_or(60).max(0) as u64;
        if secs > 0 && !snmplog_path.is_empty() {
            kcp_rs::snmp_enable();
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

    // Spawn Ctrl-C handler (runtime-agnostic)
    {
        let stop = stop_flag.clone();
        knet::spawn_task(async move {
            let _ = knet::ctrl_c().await;
            stop.store(true, Ordering::Relaxed);
        });
    }

    info!("using shared kcptun server session stack");

    for udp in udp_sockets {
        // Encrypt each accepted peer's transport via kcp-rs' listener wrapper
        // (direct KcpListener use — no kcptun-common KcptunListener layer).
        let qpp_key = key_arr.to_vec();
        let key = Arc::<[u8]>::from(key_arr);
        let crypt = Arc::<str>::from(crypt_method);
        let offload = session_config.offload_profile;
        let rate_limit = session_config.rate_limit;
        let target = target_str.clone();
        let stop = stop_flag.clone();
        let shard_config = session_config.clone();

        // A single shard uses the shared tokio runtime directly.
        // A multi-shard deployment keeps one worker per SO_REUSEPORT fd,
        // preserving strict fd affinity and independent socket send queues.
        if shards == 1 {
            let udp = Arc::new(knet::DatagramSocket::Udp(knet::UdpSocket::from_std(udp)?));
            let listener = Arc::new(
                kcp_rs::KcpListener::from_socket(udp)
                    .config(shard_config.kcp.clone())
                    .transport_wrapper(move |transport: Arc<dyn PacketTransport>, _peer| {
                        let mut ct = kcptun_common::CryptoTransport::with_transport(
                            transport,
                            key.as_ref(),
                            crypt.as_ref(),
                        );
                        ct.set_offload_profile(offload);
                        ct.set_rate_limit(rate_limit);
                        Arc::new(ct)
                    })
                    .build()
                    .await?,
            );
            knet::spawn_task(serve_udp_shard(
                listener,
                target,
                qpp_enabled,
                qpp_key,
                qpp_count,
                quiet,
                close_wait_val,
                shard_config,
                stop,
            ));
            continue;
        }

        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<Result<()>>(1);
        // Each shard runs on a dedicated OS thread + current-thread runtime:
        // this shard's fd is only touched by one worker → no shared-socket
        // send contention (Linux SO_REUSEPORT hashes peers across shards).
        std::thread::Builder::new()
            .name("kcptun-shard".into())
            .spawn(move || {
                knet::block_on_local(async move {
                    // Register the already-bound fd only after entering this shard's P.
                    let udp = match knet::UdpSocket::from_std(udp) {
                        Ok(udp) => Arc::new(knet::DatagramSocket::Udp(udp)),
                        Err(error) => {
                            let _ = ready_tx.send(Err(error.into()));
                            return;
                        }
                    };
                    let listener = match kcp_rs::KcpListener::from_socket(udp)
                        .config(shard_config.kcp.clone())
                        .transport_wrapper(move |transport: Arc<dyn PacketTransport>, _peer| {
                            let mut ct = kcptun_common::CryptoTransport::with_transport(
                                transport,
                                key.as_ref(),
                                crypt.as_ref(),
                            );
                            ct.set_offload_profile(offload);
                            ct.set_rate_limit(rate_limit);
                            Arc::new(ct)
                        })
                        .build()
                        .await
                    {
                        Ok(listener) => Arc::new(listener),
                        Err(error) => {
                            let _ = ready_tx.send(Err(error.into()));
                            return;
                        }
                    };
                    let _ = ready_tx.send(Ok(()));
                    serve_udp_shard(
                        listener,
                        target,
                        qpp_enabled,
                        qpp_key,
                        qpp_count,
                        quiet,
                        close_wait_val,
                        shard_config,
                        stop,
                    )
                    .await;
                })
            })
            .expect("spawn shard worker");
        ready_rx
            .recv()
            .context("UDP shard startup channel closed")??;
    }

    // Main task waits for stop signal (Ctrl-C).
    loop {
        knet::sleep_ms(500).await;
        if stop_flag.load(Ordering::Relaxed) {
            info!("received Ctrl+C, shutting down...");
            break;
        }
    }

    // Graceful shutdown
    info!("shutting down...");
    knet::sleep(Duration::from_secs(1)).await;
    info!("bye");

    Ok(())
}

/// Serve one UDP shard: accept KCP sessions off this shard's `kcp_rs::KcpListener`
/// and forward their SMUX streams to `target`. Single-shard builds run on the
/// shared tokio runtime; explicit multi-shard SO_REUSEPORT builds keep every fd
/// on one worker to avoid shared-socket send contention.
async fn serve_udp_shard(
    listener: Arc<kcp_rs::KcpListener>,
    target: String,
    qpp_enabled: bool,
    qpp_key: Vec<u8>,
    qpp_count: u16,
    quiet: bool,
    close_wait: u64,
    session_config: kcptun_common::KcptunConfig,
    stop: Arc<AtomicBool>,
) {
    loop {
        if stop.load(Ordering::Relaxed) {
            listener.close();
            break;
        }
        let (kcp, peer) = match listener.accept().await {
            Ok(value) => value,
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionAborted => break,
            Err(error) => {
                error!("KCP accept error: {}", error);
                knet::sleep_ms(10).await;
                continue;
            }
        };
        info!("new shared KCP session from {}", peer);
        let session =
            match kcptun_common::KcptunSession::server_with_limited_transport(kcp, &session_config)
            {
                Ok(s) => Arc::new(s),
                Err(e) => {
                    error!("session create failed for {}: {}", peer, e);
                    continue;
                }
            };
        server::spawn_session_stream_loop(
            session,
            peer,
            target.clone(),
            qpp_enabled,
            qpp_key.clone(),
            qpp_count,
            quiet,
            close_wait,
            Some(listener.clone()),
        );
    }
}
