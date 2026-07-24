#!/usr/bin/env python3
"""Re-apply reconnect/keepalive changes to kcptun-client/src/main.rs that were lost in stash/pop."""
from pathlib import Path

p = Path('kcptun-client/src/main.rs')
t = p.read_text()

# 1) dead field
t = t.replace(
    "    fec_decoder: Option<Arc<parking_lot::Mutex<FecDecoder>>>,\n}",
    "    fec_decoder: Option<Arc<parking_lot::Mutex<FecDecoder>>>,\n"
    "    /// Set when KCP dead_link or SMUX keepalive timeout is detected.\n"
    "    /// Background loops exit once this is true; accept path reconnects.\n"
    "    dead: Arc<AtomicBool>,\n}"
)

# 2) dead init
t = t.replace(
    "            flush_notify: Arc::new(kio::Notify::new()),\n            fec_encoder,\n            fec_decoder,\n        };",
    "            flush_notify: Arc::new(kio::Notify::new()),\n            fec_encoder,\n            fec_decoder,\n            dead: Arc::new(AtomicBool::new(false)),\n        };"
)

# 3) dead1 clone
t = t.replace(
    "        let fec_encoder1 = self.fec_encoder.clone();\n        let h1 = kio::spawn_task(async move {",
    "        let fec_encoder1 = self.fec_encoder.clone();\n        let dead1 = self.dead.clone();\n        let h1 = kio::spawn_task(async move {"
)

# 4) dead check in task1 loop
t = t.replace(
    "            loop {\n                let mut n = match udp1.recv(&mut buf).await {",
    "            loop {\n                if dead1.load(Ordering::Acquire) || smux1.is_closed() {\n                    break;\n                }\n                let mut n = match udp1.recv(&mut buf).await {"
)

# 5) dead2 + Phase 0 in Task 2
old_t2 = (
    '        let fec_encoder2 = self.fec_encoder.clone();\n'
    '        let h2 = kio::spawn_task(async move {\n'
    '            let mut next_update: u64 = KCP_UPDATE_INTERVAL_MS;\n'
    '            // Reused across iterations: single buffer for SMUX frame assembly (P0.3).\n'
    '            let mut out_buf = bytes::BytesMut::with_capacity(64 * 1024);\n\n'
    '            loop {\n'
    '                // Wait for either the dynamic interval (nearest RTO or\n'
    '                // default) or an immediate notify from SMUX stream writes.\n'
    '                let _ = kio::timeout(Duration::from_millis(next_update), flush_notify2.notified())\n'
    '                    .await;\n'
    '                // current_ms is no longer needed — flush() uses its own internal timestamp.\n\n'
    '                // ── Phase 1: Drain SMUX + encode frames into out_buf (NO KCP lock) ──\n'
)
if old_t2 not in t:
    print("WARN: Task2 anchor not found")
else:
    new_t2 = (
        '        let fec_encoder2 = self.fec_encoder.clone();\n'
        '        let dead2 = self.dead.clone();\n'
        '        let h2 = kio::spawn_task(async move {\n'
        '            let mut next_update: u64 = KCP_UPDATE_INTERVAL_MS;\n'
        '            // Reused across iterations: single buffer for SMUX frame assembly (P0.3).\n'
        '            let mut out_buf = bytes::BytesMut::with_capacity(64 * 1024);\n'
        '            // Throttle Phase 0 health checks — bulk flush every 1-2ms; dead_link\n'
        '            // / keepalive only need ~100ms resolution (matches Go pingLoop scale).\n'
        '            let mut health_checks_left: u32 = 0;\n\n'
        '            loop {\n'
        '                // Wait for either the dynamic interval (nearest RTO or\n'
        '                // default) or an immediate notify from SMUX stream writes.\n'
        '                let _ = kio::timeout(Duration::from_millis(next_update), flush_notify2.notified())\n'
        '                    .await;\n\n'
        '                // Fresh frame buffer each cycle (NOP + data frames assembled below).\n'
        '                out_buf.clear();\n\n'
        '                // ── Phase 0: dead-link + SMUX keepalive (Go smux pingLoop / kcp-go Die) ──\n'
        '                // Cheap flags every cycle; KCP lock + keepalive timers at ~100ms.\n'
        '                if dead2.load(Ordering::Acquire) || smux2.is_closed() {\n'
        '                    break;\n'
        '                }\n'
        '                if health_checks_left == 0 {\n'
        '                    health_checks_left = 50; // ~100ms at 2ms KCP_UPDATE_INTERVAL_MS\n'
        '                    {\n'
        '                        let kcp_dead = kcp2.lock().is_dead();\n'
        '                        if kcp_dead {\n'
        '                            error!("KCP dead_link detected — closing SMUX session");\n'
        '                            smux2.close();\n'
        '                            dead2.store(true, Ordering::Release);\n'
        '                            break;\n'
        '                        }\n'
        '                    }\n'
        '                    if smux2.is_keepalive_timeout() {\n'
        '                        error!("SMUX keepalive timeout — closing session");\n'
        '                        smux2.close();\n'
        '                        dead2.store(true, Ordering::Release);\n'
        '                        break;\n'
        '                    }\n'
        '                    if smux2.check_keepalive() {\n'
        '                        let nop = smux2.keepalive_frame();\n'
        '                        nop.encode(&mut out_buf);\n'
        '                        smux2.mark_keepalive_sent();\n'
        '                        debug!("SMUX: queued NOP keepalive");\n'
        '                    }\n'
        '                } else {\n'
        '                    health_checks_left -= 1;\n'
        '                }\n\n'
        '                // ── Phase 1: Drain SMUX + encode frames into out_buf (NO KCP lock) ──\n'
        '                // Header reserved first, payload drained in place, length patched —\n'
        '                // no to_vec / data.clone() chain (P0.3).\n'
        '                // Note: do not clear out_buf here — Phase 0 may have queued NOP.\n'
    )
    t = t.replace(old_t2, new_t2, 1)

# 6) is_dead method
t = t.replace(
    "    fn session(&self) -> &smux_rs::Session {\n        &self.smux\n    }\n}\n\n// ─── SMUX Async Wrapper ───",
    "    fn session(&self) -> &smux_rs::Session {\n        &self.smux\n    }\n\n"
    "    /// True if KCP dead_link / SMUX keepalive timeout closed this connection.\n"
    "    fn is_dead(&self) -> bool {\n"
    '        if self.dead.load(Ordering::Acquire) || self.smux.is_closed() {\n'
    "            return true;\n"
    "        }\n"
    "        // Flush loop may not have observed dead_link yet — check KCP directly.\n"
    '        if self.kcp.lock().is_dead() {\n'
    "            self.smux.close();\n"
    "            self.dead.store(true, Ordering::Release);\n"
    "            return true;\n"
    "        }\n"
    "        false\n"
    "    }\n"
    "}\n\n// ─── SMUX Async Wrapper ───"
)

# 7) keepalive_timeout dynamic
t = t.replace(
    "            keepalive_timeout: 30,",
    "            keepalive_timeout: if keepalive == 0 {\n"
    "                0\n"
    "            } else {\n"
    "                keepalive.saturating_mul(3).max(1)\n"
    "            },"
)

# 8) Accept loop
old_acc = (
    "                let idx = round_robin.fetch_add(1, Ordering::Relaxed) % conns.len();\n"
    "                let conn = &conns[idx];\n\n"
    "                let smux_stream = match conn.session().open_stream() {\n"
    "                    Ok(s) => s,\n"
    "                    Err(e) => {\n"
    '                        error!("failed to open SMUX stream: {:?}", e);\n'
    "                        continue;\n"
    "                    }\n"
    "                };\n\n"
    '                debug!("sending SYN for stream {}", smux_stream.id());\n'
    "                let syn_frame =\n"
    "                    smux_rs::Frame::new(smux_rs::Cmd::Syn, smux_stream.id(), Bytes::new())\n"
    "                        .with_ver(conn.session().version());\n"
    "                if let Err(e) = conn.send_frame(&syn_frame) {\n"
    '                    error!("failed to send Syn frame: {}", e);\n'
    "                    // open_stream already inserted into the session map — drop it.\n"
    "                    conn.session().remove_stream(smux_stream.id());\n"
    "                    continue;\n"
    "                }\n"
    '                trace!("SYN sent, flushing KCP");\n'
    "                conn.kcp.lock().flush();\n"
    '                trace!("KCP flushed for SYN");\n\n'
    '                let stream_id = smux_stream.id();\n'
    '                info!("accepted connection from {} (stream {})", peer, stream_id);\n\n'
    "                let qpp_key = key_str.as_bytes().to_vec();\n"
    "                let ws = conn.wait_send.clone();\n"
    "                let sw = conn.snd_wnd;\n"
    "                let flush_notify_ref = conn.flush_notify.clone();\n"
    "                let write_notify_ref = conn.write_notify.clone();"
)
if old_acc not in t:
    raise SystemExit("accept anchor not found")
new_acc = (
    "                let idx = round_robin.fetch_add(1, Ordering::Relaxed) % conns.len();\n\n"
    "                // Ensure a live KCP/SMUX session (Go muxSession.Open auto-redial).\n"
    "                let mut opened = None;\n"
    "                for attempt in 0..2 {\n"
    "                    if conns[idx].is_dead() {\n"
    "                        let remote = remote_addrs[idx % remote_addrs.len()];\n"
    '                        info!(\n'
    '                            "connection {} is dead, reconnecting to {} (attempt {})...",\n'
    "                            idx, remote, attempt + 1\n"
    "                        );\n"
    "                        match KcpConn::new(\n"
    "                            remote, &key, crypt, mode, mtu, sndwnd, rcvwnd,\n"
    "                            datashard, parityshard, acknodelay, nodelay, interval,\n"
    "                            resend, nc, smuxver, smuxbuf, streambuf, framesize,\n"
    "                            keepalive, nocomp,\n"
    "                        ).await {\n"
    "                            Ok(new_conn) => {\n"
    "                                conns[idx] = new_conn;\n"
    "                                kcp_rs::DEFAULT_SNMP.session_opened(true);\n"
    '                                info!("connection {} reconnected", idx);\n'
    "                            }\n"
    '                            Err(e) => {\n'
    '                                error!("reconnect connection {} failed: {:#}", idx, e);\n'
    "                                break;\n"
    "                            }\n"
    "                        }\n"
    "                    }\n\n"
    "                    let c = &conns[idx];\n"
    "                    match c.session().open_stream() {\n"
    "                        Ok(s) => {\n"
    '                            let stream_id = s.id();\n'
    '                            debug!("sending SYN for stream {}", stream_id);\n'
    "                            let syn_frame =\n"
    "                                smux_rs::Frame::new(smux_rs::Cmd::Syn, stream_id, Bytes::new())\n"
    "                                    .with_ver(c.session().version());\n"
    "                            if let Err(e) = c.send_frame(&syn_frame) {\n"
    '                                error!("failed to send Syn frame: {}", e);\n'
    "                                c.session().remove_stream(stream_id);\n"
    "                                c.dead.store(true, Ordering::Release);\n"
    "                                c.session().close();\n"
    "                                continue;\n"
    "                            }\n"
    "                            c.kcp.lock().flush();\n"
    "                            c.flush_notify.notify_one();\n"
    "                            opened = Some(s);\n"
    "                            break;\n"
    "                        }\n"
    '                        Err(e) => {\n'
    '                            error!("failed to open SMUX stream: {:?}", e);\n'
    "                            c.dead.store(true, Ordering::Release);\n"
    "                            c.session().close();\n"
    "                        }\n"
    "                    }\n"
    "                }\n\n"
    "                let smux_stream = match opened {\n"
    "                    Some(s) => s,\n"
    "                    None => { continue; }\n"
    "                };\n\n"
    '                let stream_id = smux_stream.id();\n'
    '                info!("accepted connection from {} (stream {})", peer, stream_id);\n\n'
    "                let conn = &conns[idx];\n"
    "                let qpp_key = key_str.as_bytes().to_vec();\n"
    "                let ws = conn.wait_send.clone();\n"
    "                let sw = conn.snd_wnd;\n"
    "                let flush_notify_ref = conn.flush_notify.clone();\n"
    "                let write_notify_ref = conn.write_notify.clone();"
)
t = t.replace(old_acc, new_acc, 1)

p.write_text(t)
print("client main changes re-applied successfully")
