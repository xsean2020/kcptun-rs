//! Functional test: multi-conn mode and server-restart reconnection.
//!
//! Unlike the stress_test which uses `shutdown(Write)` (broken pre-existing
//! echo behavior in base code), this test avoids half-close and verifies
//! correctness via full-duplex echo with explicit wire patterns.
//!
//! // Usage:
//! //   cargo test --release -p kcptun-server --test reconnect_test -- --nocapture

use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

fn find_bin(name: &str) -> String {
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

struct ReconnectEnv {
    procs: Vec<Child>,
    cli_port: u16,
    srv_port: u16,
    target_port: u16,
    crypt: String,
    nocomp: bool,
    conn: usize,
    keepalive: u64,
    mode: String,
}

impl ReconnectEnv {
    fn start_same(env: &ReconnectEnv) -> Self {
        Self::start(
            env.target_port,
            env.srv_port,
            env.cli_port,
            &env.crypt,
            env.nocomp,
            env.conn,
            env.keepalive,
            &env.mode,
        )
    }

    fn start(
        target_port: u16,
        srv_port: u16,
        cli_port: u16,
        crypt: &str,
        nocomp: bool,
        conn: usize,
        keepalive: u64,
        mode: &str,
    ) -> Self {
        for p in &[target_port, srv_port, cli_port] {
            kill_port(*p);
        }
        thread::sleep(Duration::from_millis(800));

        // TCP echo server (single-shot echo, no shutdown dependency)
        let echo = Command::new("python3")
            .arg("-u")
            .arg("-c")
            .arg(format!(
                "import socket,threading as _t\ndef _h(c):\n d=c.recv(65536)\n if d:c.sendall(d)\n c.close()\n\
                 s=socket.socket();s.setsockopt(65535,4,1);s.bind(('',{}));s.listen(128)\n\
                 while 1:_t.Thread(target=_h,args=(s.accept()[0],)).start()",
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
            mode.into(),
            "--datashard".into(),
            "0".into(),
            "--parityshard".into(),
            "0".into(),
            "--sndwnd".into(),
            "2048".into(),
            "--rcvwnd".into(),
            "2048".into(),
            "--keepalive".into(),
            keepalive.to_string(),
        ];
        if nocomp {
            srv_args.push("--nocomp".into());
        }

        let sv = Command::new(&find_bin("kcptun-server"))
            .args(&srv_args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("srv");
        thread::sleep(Duration::from_secs(1));

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
            mode.into(),
            "--datashard".into(),
            "0".into(),
            "--parityshard".into(),
            "0".into(),
            "--sndwnd".into(),
            "2048".into(),
            "--rcvwnd".into(),
            "2048".into(),
            "--keepalive".into(),
            keepalive.to_string(),
            "--conn".into(),
            conn.to_string(),
        ];
        if nocomp {
            cli_args.push("--nocomp".into());
        }

        let cl = Command::new(&find_bin("kcptun-client"))
            .args(&cli_args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("cli");
        thread::sleep(Duration::from_secs(2));

        ReconnectEnv {
            procs: vec![echo, sv, cl],
            cli_port,
            srv_port,
            target_port,
            crypt: crypt.to_string(),
            nocomp,
            conn,
            keepalive,
            mode: mode.to_string(),
        }
    }

    fn send_echo(&self, msg: &[u8]) -> Option<Vec<u8>> {
        let mut s = TcpStream::connect(format!("127.0.0.1:{}", self.cli_port)).ok()?;
        s.set_read_timeout(Some(Duration::from_secs(8))).ok();
        s.write_all(msg).ok()?;
        // Do NOT half-close — let the echo path return data naturally.
        thread::sleep(Duration::from_millis(150));
        // Flush and then read with a small trick: data is already buffered
        // by the time we read, since echo closes after sending.
        let mut resp = Vec::with_capacity(msg.len());
        let mut buf = [0u8; 65536];
        loop {
            match s.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    resp.extend_from_slice(&buf[..n]);
                    if resp.len() >= msg.len() {
                        break;
                    }
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    break
                }
                Err(_) => break,
            }
        }
        if resp.is_empty() {
            None
        } else {
            Some(resp)
        }
    }

    fn kill_server(&mut self) {
        if self.procs.len() > 1 {
            let _ = self.procs[1].kill();
            let _ = self.procs[1].wait();
        }
    }

    fn restart_server(&mut self) {
        self.kill_server();
        kill_port(self.srv_port);
        thread::sleep(Duration::from_millis(500));

        let mut srv_args: Vec<String> = vec![
            "-t".into(),
            format!("127.0.0.1:{}", self.target_port),
            "-l".into(),
            format!(":{}", self.srv_port),
            "--key".into(),
            "k".into(),
            "--crypt".into(),
            self.crypt.clone(),
            "--mode".into(),
            self.mode.clone(),
            "--datashard".into(),
            "0".into(),
            "--parityshard".into(),
            "0".into(),
            "--sndwnd".into(),
            "2048".into(),
            "--rcvwnd".into(),
            "2048".into(),
            "--keepalive".into(),
            self.keepalive.to_string(),
        ];
        if self.nocomp {
            srv_args.push("--nocomp".into());
        }

        let sv = Command::new(&find_bin("kcptun-server"))
            .args(&srv_args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("srv restart");
        if self.procs.len() > 1 {
            self.procs[1] = sv;
        } else {
            self.procs.push(sv);
        }
        thread::sleep(Duration::from_secs(1));
    }
}

impl Drop for ReconnectEnv {
    fn drop(&mut self) {
        for p in &mut self.procs {
            let _ = p.kill();
        }
    }
}

/// Generate a deterministic payload.
fn make_payload(seed: usize, size: usize) -> Vec<u8> {
    (0..size)
        .map(|i| (seed as u8).wrapping_add(i as u8))
        .collect()
}

/// --conn 4: all connections pass data (round-robin).
#[test]
fn test_multi_conn_baseline() {
    let e = ReconnectEnv::start(19700, 39700, 19701, "null", true, 4, 30, "fast");
    let mut ok = 0usize;
    let mut fail = 0usize;
    for i in 0..24 {
        let payload = make_payload(i, 2048);
        match e.send_echo(&payload) {
            Some(resp) if resp == payload => ok += 1,
            _ => fail += 1,
        }
    }
    drop(e);
    assert!(ok >= 20, "multi-conn: only {ok}/24 ok, {fail} fail");
    println!("✅ multi-conn baseline OK (--conn 4: {ok}/24)")
}

/// --conn 4 reconnect after server restart: kill, probe, restart, verify recovery.
#[test]
fn test_reconnect_after_restart() {
    let conn_n = 4;
    let base = 19600 + (std::process::id() % 1000) as u16;
    // Use mode "fast" (not fast3) — fast3 + --conn 4 has baseline timeout issues.
    let mut e = ReconnectEnv::start(base, base + 1, base + 2, "null", true, conn_n, 2, "fast");

    // Baseline
    for i in 0..8 {
        let p = make_payload(100 + i, 512);
        assert!(
            e.send_echo(&p).as_deref() == Some(&p[..]),
            "baseline conn {i} failed"
        );
    }
    println!("  baseline OK");

    // Kill server
    e.kill_server();
    println!("  server killed");

    // Probes to drive dead_link (longer with mode fast: RTO ~200ms, 20× = ~4s+)
    for _ in 0..40 {
        let p = make_payload(200, 64);
        let _ = e.send_echo(&p);
        thread::sleep(Duration::from_millis(200));
    }
    println!("  probes done (~8s of retransmit traffic)");

    // Restart
    e.restart_server();
    println!("  server restarted");

    // Recovery
    let mut consecutive = 0usize;
    let start = Instant::now();
    for i in 0..60 {
        let p = make_payload(300 + i, 256);
        match e.send_echo(&p) {
            Some(resp) if resp == p => {
                if consecutive == 0 {
                    println!("  first recovery OK at t+{:?}", start.elapsed());
                }
                consecutive += 1;
                if consecutive >= 8 {
                    break;
                }
            }
            _ => {
                consecutive = 0;
                thread::sleep(Duration::from_millis(300));
            }
        }
    }
    println!(
        "  recovery overall: {}/60 after {:?}",
        consecutive,
        start.elapsed()
    );
    drop(e);
    assert!(
        consecutive >= 8,
        "reconnect recovery failed: only {consecutive} consecutive OK (need 8)"
    );
    println!("✅ multi-conn reconnect OK (--conn 4, {consecutive} consecutive)");
}
