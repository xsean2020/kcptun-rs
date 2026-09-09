//! Server-side stream handler and session loop.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{Context as AnyContext, Result};
use log::{debug, error, info, warn};

/// Handle a single SMUX stream: connect to the TCP target and pipe data
/// bidirectionally with optional QPP. Compression is handled at the
/// KCP/SMUX session level (matching Go kcptun architecture).
#[cfg_attr(not(feature = "qpp"), allow(unused_variables))]
pub(crate) async fn handle_stream(
    target: String,
    smux_stream: Arc<smux_rs::stream::Stream>,
    stream_id: u32,
    qpp_enabled: bool,
    qpp_key: Vec<u8>,
    qpp_count: u16,
    quiet: bool,
    close_wait: u64,
    flush_notify: Arc<knet::Notify>,
) -> Result<()> {
    let tcp = knet::TcpStream::connect(&target)
        .await
        .with_context(|| format!("failed to connect to target {}", target))?;

    if !quiet {
        info!("stream {} connected to target {}", stream_id, target);
    }

    smux_stream.set_flush_notify(flush_notify.clone());
    let smux_io = smux_rs::SmuxIo::new(smux_stream.clone(), flush_notify);

    let pipe_result = if qpp_enabled {
        #[cfg(feature = "qpp")]
        {
            let qpp_port = kcptun_common::QPPPort::new(smux_io, &qpp_key, qpp_count);
            let mut tcp_pin = tcp;
            let mut qpp_pin = qpp_port;
            kcptun_common::pipe(&mut tcp_pin, &mut qpp_pin, close_wait).await
        }
        #[cfg(not(feature = "qpp"))]
        {
            unreachable!("qpp_enabled should be false when qpp feature disabled")
        }
    } else {
        let mut tcp_pin = tcp;
        let mut smux_pin = smux_io;
        debug!("server pipe started for stream {}", stream_id);
        kcptun_common::pipe(&mut tcp_pin, &mut smux_pin, close_wait).await
    };

    // Local half-close only. Do NOT mark_fin_sent here — that made flush skip
    // encoding FIN (same class of leak as client; see BUGREPORT_PROXY_MEMORY_GROWTH).
    // Flush encodes FIN then marks fin_sent after kcp.send success; linger reaps zombies.
    //
    // IMPORTANT: Do NOT call clear_buffers() here. The flush loop may still have
    // data in the stream's send_buf that has been written by the pipe but not yet
    // drained and sent through KCP. Clearing buffers would discard unsent data,
    // causing silent data loss (observed in stress tests as truncated/zero responses).
    // The flush loop is responsible for draining send_buf, sending FIN after drain,
    // and the linger reaper will eventually clear buffers after is_fin_sent + timeout.
    smux_stream.mark_local_closed();
    // Do not clear buffers — let flush drain and linger reap.

    match pipe_result {
        Ok((a, b)) => {
            if !quiet {
                info!(
                    "stream {} pipe completed: {} sent, {} recv{}",
                    stream_id,
                    a,
                    b,
                    if qpp_enabled { " (QPP)" } else { "" }
                );
            }
        }
        Err(e) => {
            warn!("stream {} pipe error: {}", stream_id, e);
        }
    }

    Ok(())
}

/// Spawn a task that accepts streams from a session and dispatches each to
/// `handle_stream`.  Cleans up the peer in the UDP listener when done.
pub(crate) fn spawn_session_stream_loop(
    session: Arc<kcptun_common::KcptunSession>,
    peer: SocketAddr,
    target: String,
    qpp_enabled: bool,
    qpp_key: Vec<u8>,
    qpp_count: u16,
    quiet: bool,
    close_wait: u64,
    udp_listener: Option<Arc<kcp_rs::KcpListener>>,
) {
    knet::spawn_task(async move {
        loop {
            let stream = match session.accept().await {
                Ok(stream) => stream,
                Err(_) => break,
            };
            let stream_id = stream.id();
            if !quiet {
                info!(
                    "accepting stream {} from {} -> target {}",
                    stream_id, peer, target
                );
            }
            let target = target.clone();
            let qpp_key = qpp_key.clone();
            let notify = session.flush_notify();
            knet::spawn_task(async move {
                if let Err(error) = handle_stream(
                    target,
                    stream,
                    stream_id,
                    qpp_enabled,
                    qpp_key,
                    qpp_count,
                    quiet,
                    close_wait,
                    notify,
                )
                .await
                {
                    error!("stream {} handler error: {:?}", stream_id, error);
                }
            });
        }
        session.close();
        if let Some(listener) = udp_listener {
            listener.remove_peer(peer);
        }
    });
}
