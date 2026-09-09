//! TCP raw socket transport emulation (Linux only).
//!
//! Uses `TCP_REPAIR` to silence the kernel TCP stack after the three-way
//! handshake, then sends/receives **TCP segments** via a raw socket
//! (`SOCK_RAW` + `IPPROTO_TCP`, **no** `IP_HDRINCL`).
//!
//! This matches Go `xtaci/tcpraw` wire shape on the raw socket:
//!   - send: `[TCP Header 20B + 12B Timestamp Options][KCP Datagram]`
//!   - recv: same (kernel strips/adds the IP header)
//!
//! Go reference: `vendor/github.com/xtaci/tcpraw/tcp_linux.go`

use std::io;
use std::net::SocketAddr;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::thread;

use socket2::Socket;

// ─── TCP_REPAIR / queue constants (Linux uapi) ───────────────────────────────
const TCP_REPAIR: i32 = 19;
const TCP_REPAIR_QUEUE: i32 = 20;
const TCP_QUEUE_SEQ: i32 = 21;
const TCP_RECV_QUEUE: i32 = 1;
const TCP_SEND_QUEUE: i32 = 2;

// ─── TCP fingerprint (matching Go tcpraw fingerPrintLinux) ───────────────────
const TCP_WINDOW: u16 = 65535;
const MAX_PAYLOAD: usize = 1400;
const RAW_RECV_BUF: usize = 2 * 1024 * 1024;
const CHANNEL_CAP: usize = 256;
const TCP_HDR_LEN: usize = 32; // 20 base + 12 timestamp options

/// How the kernel TCP stack is silenced after the 3-way handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Takeover {
    Repair,
    Iptables,
}

/// Env override for tests / triage: `KCPTCP_TAKEOVER=repair|iptables`.
fn takeover_from_env() -> Option<Takeover> {
    match std::env::var("KCPTCP_TAKEOVER").ok().as_deref() {
        Some("repair") => Some(Takeover::Repair),
        Some("iptables") => Some(Takeover::Iptables),
        _ => None,
    }
}

/// Initial TCP flow state captured after the three-way handshake.
///
/// Uses `TCP_REPAIR` + `TCP_QUEUE_SEQ` rather than extended `tcp_info` fields
/// (absent from the `libc` crate). Peer timestamp echo (`ts_ecr`) starts at 0
/// and is filled from captured TCP options.
struct RepairState {
    seq: u32,
    ack: u32,
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn socket_addr_to_ipv4(addr: &SocketAddr) -> io::Result<([u8; 4], u16)> {
    match addr {
        SocketAddr::V4(v4) => Ok((v4.ip().octets(), v4.port())),
        SocketAddr::V6(_) => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "tcpraw only supports IPv4",
        )),
    }
}

fn set_tcp_repair(stream: &std::net::TcpStream, on: bool) -> io::Result<()> {
    let val: libc::c_int = if on { 1 } else { 0 };
    let ret = unsafe {
        libc::setsockopt(
            stream.as_raw_fd(),
            libc::SOL_TCP,
            TCP_REPAIR,
            &val as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if ret < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn set_repair_queue(stream: &std::net::TcpStream, queue: i32) -> io::Result<()> {
    let val: libc::c_int = queue;
    let ret = unsafe {
        libc::setsockopt(
            stream.as_raw_fd(),
            libc::SOL_TCP,
            TCP_REPAIR_QUEUE,
            &val as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if ret < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Non-blocking read-to-empty of an fd (drains the kernel recv queue).
fn drain_fd(fd: libc::c_int) {
    let mut buf = [0u8; 4096];
    loop {
        let n = unsafe {
            libc::recv(
                fd,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
                libc::MSG_DONTWAIT,
            )
        };
        if n <= 0 {
            break;
        }
    }
}

fn set_ttl(stream: &std::net::TcpStream, ttl: u8) -> io::Result<()> {
    let val: libc::c_int = ttl as libc::c_int;
    let ret = unsafe {
        libc::setsockopt(
            stream.as_raw_fd(),
            libc::IPPROTO_IP,
            libc::IP_TTL,
            &val as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Set DSCP on a raw socket fd (IPv4 `IP_TOS`, shifted into the 6-bit DSCP
/// field). Matches Go tcpraw's `setDSCP` (`dscp << 2`). Applies to all
/// segments sent on that raw socket.
fn set_raw_dscp(fd: libc::c_int, dscp: u32) -> io::Result<()> {
    let val: libc::c_int = ((dscp & 0x3F) << 2) as libc::c_int;
    let ret = unsafe {
        libc::setsockopt(
            fd,
            libc::IPPROTO_IP,
            libc::IP_TOS,
            &val as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if ret < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn ttl_drop_rule_client(
    local_ip: &str,
    local_port: u16,
    peer_ip: &str,
    peer_port: u16,
) -> Vec<String> {
    vec![
        "-m".into(),
        "ttl".into(),
        "--ttl-eq".into(),
        "1".into(),
        "-p".into(),
        "tcp".into(),
        "-s".into(),
        local_ip.into(),
        "--sport".into(),
        local_port.to_string(),
        "-d".into(),
        peer_ip.into(),
        "--dport".into(),
        peer_port.to_string(),
        "-j".into(),
        "DROP".into(),
    ]
}

fn ttl_drop_rule_server(port: u16) -> Vec<String> {
    vec![
        "-m".into(),
        "ttl".into(),
        "--ttl-eq".into(),
        "1".into(),
        "-p".into(),
        "tcp".into(),
        "--sport".into(),
        port.to_string(),
        "-j".into(),
        "DROP".into(),
    ]
}

fn iptables_status(verb: &str, rule: &[String]) -> io::Result<std::process::ExitStatus> {
    std::process::Command::new("iptables")
        .arg("-t")
        .arg("filter")
        .arg(verb)
        .arg("OUTPUT")
        .args(rule)
        .status()
}

/// Test-only: `-C` rule lookup is used solely by the `iptables_rule_cleaned_on_close`
/// integration test. Gated so the Linux lib build has no dead code.
#[cfg(test)]
fn rule_exists(rule: &[String]) -> bool {
    matches!(iptables_status("-C", rule), Ok(s) if s.success())
}

fn rule_add(rule: &[String]) -> io::Result<()> {
    let st = iptables_status("-A", rule)?;
    if st.success() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "iptables -A failed (needs root / CAP_NET_ADMIN + ttl match module)",
        ))
    }
}

fn rule_delete(rule: &[String]) {
    let _ = iptables_status("-D", rule);
}

/// Drains the real socket continuously (iptables path: the kernel is NOT in
/// repair mode, so it queues inbound into the recv buffer). Keeps the buffer
/// empty so close-with-unread-data never triggers RST.
fn spawn_drain_thread(mut stream: std::net::TcpStream) -> Option<thread::JoinHandle<()>> {
    thread::Builder::new()
        .name("tcpraw-drain".into())
        .spawn(move || {
            use std::io::Read;
            let mut buf = [0u8; 8192];
            loop {
                match stream.read(&mut buf) {
                    Ok(n) if n > 0 => continue, // discard
                    _ => break,                 // EOF (shutdown RD) or error
                }
            }
        })
        .ok()
}

fn get_queue_seq(stream: &std::net::TcpStream) -> io::Result<u32> {
    let mut seq: u32 = 0;
    let mut len = std::mem::size_of::<u32>() as libc::socklen_t;
    let ret = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_TCP,
            TCP_QUEUE_SEQ,
            &mut seq as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };
    if ret < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(seq)
}

/// Enter TCP_REPAIR and capture post-handshake seq/ack state.
fn capture_repair_state(stream: &std::net::TcpStream) -> io::Result<RepairState> {
    set_tcp_repair(stream, true)?;

    set_repair_queue(stream, TCP_SEND_QUEUE)?;
    let seq = get_queue_seq(stream)?;

    set_repair_queue(stream, TCP_RECV_QUEUE)?;
    let ack = get_queue_seq(stream)?;

    Ok(RepairState { seq, ack })
}

/// Open `SOCK_RAW` + `IPPROTO_TCP` **without** `IP_HDRINCL`.
///
/// Matches Go `net.DialIP("ip:tcp", …)` / `ListenIP("ip:tcp", …)`:
/// userspace supplies TCP segments only; the kernel adds/strips IP headers.
fn open_raw_tcp_socket() -> io::Result<OwnedFd> {
    let raw = Socket::new(
        socket2::Domain::IPV4,
        socket2::Type::RAW,
        Some(socket2::Protocol::from(libc::IPPROTO_TCP)),
    )?;
    let _ = raw.set_recv_buffer_size(RAW_RECV_BUF);
    let _ = raw.set_send_buffer_size(RAW_RECV_BUF);
    Ok(unsafe { OwnedFd::from_raw_fd(raw.into_raw_fd()) })
}

// ─── Checksum ────────────────────────────────────────────────────────────────

fn ones_complement_sum(data: &[u8]) -> u32 {
    let mut sum: u32 = 0;
    let chunks = data.chunks_exact(2);
    let remainder = chunks.remainder();
    for chunk in chunks {
        sum += u16::from_be_bytes([chunk[0], chunk[1]]) as u32;
    }
    if remainder.len() == 1 {
        sum += (remainder[0] as u32) << 8;
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    sum
}

fn tcp_checksum(src_ip: &[u8; 4], dst_ip: &[u8; 4], tcp_segment: &[u8]) -> u16 {
    let mut buf = Vec::with_capacity(12 + tcp_segment.len() + 1);
    buf.extend_from_slice(src_ip);
    buf.extend_from_slice(dst_ip);
    buf.push(0);
    buf.push(6); // TCP
    let tcp_len = tcp_segment.len() as u16;
    buf.extend_from_slice(&tcp_len.to_be_bytes());
    buf.extend_from_slice(tcp_segment);
    if buf.len() % 2 != 0 {
        buf.push(0);
    }
    !ones_complement_sum(&buf) as u16
}

// ─── Packet Construction ─────────────────────────────────────────────────────

/// Build a TCP segment only (no IP header) — Go tcpraw / IPPROTO_TCP raw shape.
fn build_tcp_segment(
    src_ip: &[u8; 4],
    dst_ip: &[u8; 4],
    src_port: u16,
    dst_port: u16,
    seq: u32,
    ack: u32,
    ts_val: u32,
    ts_ecr: u32,
    payload: &[u8],
) -> Vec<u8> {
    let total = TCP_HDR_LEN + payload.len();
    let mut pkt = vec![0u8; total];

    pkt[0..2].copy_from_slice(&src_port.to_be_bytes());
    pkt[2..4].copy_from_slice(&dst_port.to_be_bytes());
    pkt[4..8].copy_from_slice(&seq.to_be_bytes());
    pkt[8..12].copy_from_slice(&ack.to_be_bytes());
    pkt[12] = 0x80; // Data Offset = 8×4 = 32B
    pkt[13] = 0x18; // PSH + ACK (matching Go)
    pkt[14..16].copy_from_slice(&TCP_WINDOW.to_be_bytes());
    // checksum filled below

    // Options: NOP + NOP + Timestamp(kind=8,len=10,TSval,TSecr)
    pkt[20] = 1;
    pkt[21] = 1;
    pkt[22] = 8;
    pkt[23] = 10;
    pkt[24..28].copy_from_slice(&ts_val.to_be_bytes());
    pkt[28..32].copy_from_slice(&ts_ecr.to_be_bytes());

    pkt[TCP_HDR_LEN..].copy_from_slice(payload);

    let csum = tcp_checksum(src_ip, dst_ip, &pkt);
    pkt[16..18].copy_from_slice(&csum.to_be_bytes());
    pkt
}

// ─── TcpFlowState ────────────────────────────────────────────────────────────

struct TcpFlowState {
    seq: AtomicU32,
    ack: AtomicU32,
    /// Peer timestamp (ts_ecr). Our own ts_val is generated per-send via mono_ms().
    ts_ecr: AtomicU32,
}

// ─── TcpRawConn ──────────────────────────────────────────────────────────────

/// A point-to-point TCP raw connection — acts like a datagram socket to KCP.
pub struct TcpRawConn {
    /// Real TCP socket in TCP_REPAIR mode (held for cleanup).
    _real: std::net::TcpStream,
    /// Raw socket fd shared with capture thread.
    raw_fd: Arc<OwnedFd>,
    /// TCP flow state.
    flow: Arc<TcpFlowState>,
    /// Source/destination for this connection.
    src_ip: [u8; 4],
    dst_ip: [u8; 4],
    src_port: u16,
    dst_port: u16,
    /// Payload channel from capture thread.
    rx: async_channel::Receiver<Vec<u8>>,
    /// Close signal to capture thread.
    close_tx: async_channel::Sender<()>,
    /// Capture thread handle.
    _cap_thread: Option<thread::JoinHandle<()>>,
    /// Which takeover method this connection uses (drives close behavior).
    takeover: Takeover,
    /// Per-connection iptables OUTPUT rule (client only; empty for server conns).
    iptables_rule: Vec<String>,
    /// Drain thread handle (iptables path only).
    _drain: Option<thread::JoinHandle<()>>,
    /// Listener map refs + our peer key (server conns only) so Drop can
    /// deregister the stale flow/channel entries.
    listener_reg: Option<(
        Arc<
            std::sync::Mutex<std::collections::HashMap<SocketAddr, async_channel::Sender<Vec<u8>>>>,
        >,
        Arc<std::sync::Mutex<std::collections::HashMap<SocketAddr, Arc<TcpFlowState>>>>,
        SocketAddr,
    )>,
}

unsafe impl Send for TcpRawConn {}
unsafe impl Sync for TcpRawConn {}

impl TcpRawConn {
    fn raw_send(&self, buf: &[u8]) -> io::Result<usize> {
        if buf.len() > MAX_PAYLOAD {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("payload too large: {} > {}", buf.len(), MAX_PAYLOAD),
            ));
        }

        let seq = self.flow.seq.load(Ordering::Relaxed);
        let ack = self.flow.ack.load(Ordering::Relaxed);
        // Match Go makeOption: time.Now().UnixNano() / 1e6
        let ts_val = crate::mono_ms() as u32;
        let ts_ecr = self.flow.ts_ecr.load(Ordering::Relaxed);

        let pkt = build_tcp_segment(
            &self.src_ip,
            &self.dst_ip,
            self.src_port,
            self.dst_port,
            seq,
            ack,
            ts_val,
            ts_ecr,
            buf,
        );

        // IPPROTO_TCP raw send: destination is peer IP. Port in sockaddr is
        // ignored by the kernel for raw TCP; TCP header carries real ports.
        let dst = libc::sockaddr_in {
            sin_family: libc::AF_INET as libc::sa_family_t,
            sin_port: 0,
            sin_addr: libc::in_addr {
                s_addr: u32::from_ne_bytes(self.dst_ip),
            },
            sin_zero: [0; 8],
        };

        let sent = unsafe {
            libc::sendto(
                self.raw_fd.as_raw_fd(),
                pkt.as_ptr() as *const libc::c_void,
                pkt.len(),
                0,
                &dst as *const _ as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
            )
        };

        if sent < 0 {
            return Err(io::Error::last_os_error());
        }

        self.flow
            .seq
            .store(seq.wrapping_add(buf.len() as u32), Ordering::Relaxed);

        Ok(buf.len())
    }

    pub fn send_to(&self, buf: &[u8], _target: &SocketAddr) -> io::Result<usize> {
        self.raw_send(buf)
    }

    pub fn send(&self, buf: &[u8]) -> io::Result<usize> {
        self.raw_send(buf)
    }

    pub fn try_recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        match self.rx.try_recv() {
            Ok(payload) => {
                let n = payload.len().min(buf.len());
                buf[..n].copy_from_slice(&payload[..n]);
                let peer = SocketAddr::from((self.dst_ip, self.dst_port));
                Ok((n, peer))
            }
            Err(async_channel::TryRecvError::Empty) => {
                Err(io::Error::from(io::ErrorKind::WouldBlock))
            }
            Err(async_channel::TryRecvError::Closed) => {
                Err(io::Error::from(io::ErrorKind::ConnectionReset))
            }
        }
    }

    pub fn try_recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        match self.rx.try_recv() {
            Ok(payload) => {
                let n = payload.len().min(buf.len());
                buf[..n].copy_from_slice(&payload[..n]);
                Ok(n)
            }
            Err(async_channel::TryRecvError::Empty) => {
                Err(io::Error::from(io::ErrorKind::WouldBlock))
            }
            Err(async_channel::TryRecvError::Closed) => {
                Err(io::Error::from(io::ErrorKind::ConnectionReset))
            }
        }
    }

    pub async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        match self.rx.recv().await {
            Ok(payload) => {
                let n = payload.len().min(buf.len());
                buf[..n].copy_from_slice(&payload[..n]);
                let peer = SocketAddr::from((self.dst_ip, self.dst_port));
                Ok((n, peer))
            }
            Err(_) => Err(io::Error::from(io::ErrorKind::ConnectionReset)),
        }
    }

    pub async fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        match self.rx.recv().await {
            Ok(payload) => {
                let n = payload.len().min(buf.len());
                buf[..n].copy_from_slice(&payload[..n]);
                Ok(n)
            }
            Err(_) => Err(io::Error::from(io::ErrorKind::ConnectionReset)),
        }
    }

    pub async fn send_batch_to<B: AsRef<[u8]>>(
        &self,
        bufs: &[B],
        _target: &SocketAddr,
    ) -> io::Result<()> {
        for buf in bufs {
            self.raw_send(buf.as_ref())?;
        }
        Ok(())
    }

    pub fn try_recv_batch_from(
        &self,
        packet_bufs: &mut [Vec<u8>],
        out: &mut Vec<(Vec<u8>, SocketAddr)>,
    ) -> io::Result<usize> {
        out.clear();
        let peer = SocketAddr::from((self.dst_ip, self.dst_port));
        for slot in packet_bufs.iter_mut() {
            match self.rx.try_recv() {
                Ok(payload) => {
                    *slot = payload.clone();
                    out.push((payload, peer));
                }
                Err(async_channel::TryRecvError::Empty) => break,
                Err(async_channel::TryRecvError::Closed) => {
                    return Err(io::Error::from(io::ErrorKind::ConnectionReset));
                }
            }
        }
        Ok(out.len())
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        Ok(SocketAddr::from((self.src_ip, self.src_port)))
    }

    /// Set DSCP on the raw socket (IPv4 TOS). Matches Go `TCPConn.SetDSCP`.
    pub fn set_dscp(&self, dscp: u32) -> io::Result<()> {
        set_raw_dscp(self.raw_fd.as_raw_fd(), dscp)
    }

    /// Graceful close so the kernel never emits RST (repair path):
    /// 1. drain the recv queue while still in repair mode (a close with
    ///    unread data makes the kernel send RST);
    /// 2. exit TCP_REPAIR (kernel's seq view was frozen at capture time and
    ///    we only ever sent via the raw socket, so it is consistent);
    /// 3. graceful FIN via shutdown(SHUT_WR);
    /// 4. drain stragglers that raced in after step 1.
    fn close_repair(&self) {
        let stream = &self._real;
        let fd = stream.as_raw_fd();
        let _ = set_repair_queue(stream, TCP_RECV_QUEUE);
        drain_fd(fd);
        let _ = set_tcp_repair(stream, false);
        unsafe {
            libc::shutdown(fd, libc::SHUT_WR);
        }
        drain_fd(fd);
    }

    /// Graceful close for the iptables-TTL-DROP path: restore TTL so the FIN
    /// passes the OUTPUT rule, delete the per-conn rule, FIN, then unblock the
    /// drain thread (which finishes draining before the socket fully closes).
    fn close_iptables(&self) {
        let _ = set_ttl(&self._real, 64);
        if !self.iptables_rule.is_empty() {
            rule_delete(&self.iptables_rule);
        }
        let fd = self._real.as_raw_fd();
        unsafe {
            libc::shutdown(fd, libc::SHUT_WR);
            libc::shutdown(fd, libc::SHUT_RD);
        }
    }

    /// Idempotent close: graceful FIN per takeover method, then stop the
    /// capture thread.
    pub fn close(&self) {
        match self.takeover {
            Takeover::Repair => self.close_repair(),
            Takeover::Iptables => self.close_iptables(),
        }
        let _ = self.close_tx.try_send(());
    }
}

impl Drop for TcpRawConn {
    fn drop(&mut self) {
        self.close();
        if let Some(h) = self._drain.take() {
            let _ = h.join(); // drain exits on shutdown(SHUT_RD) → EOF
        }
        if let Some((channels, flows, peer)) = &self.listener_reg {
            let mut fl = flows.lock().unwrap();
            let is_mine = matches!(fl.get(peer), Some(f) if Arc::ptr_eq(f, &self.flow));
            if is_mine {
                fl.remove(peer);
                drop(fl);
                channels.lock().unwrap().remove(peer);
            }
        }
    }
}

// ─── TcpRawListener ──────────────────────────────────────────────────────────

/// Server-side TCP raw listener.
pub struct TcpRawListener {
    real: std::net::TcpListener,
    raw_fd: Arc<OwnedFd>,
    channels: Arc<
        std::sync::Mutex<std::collections::HashMap<SocketAddr, async_channel::Sender<Vec<u8>>>>,
    >,
    /// Per-peer flow state so the capture thread can update ack/seq/ts_ecr.
    flows: Arc<std::sync::Mutex<std::collections::HashMap<SocketAddr, Arc<TcpFlowState>>>>,
    _close_tx: async_channel::Sender<()>,
    /// Lazily installed OUTPUT TTL-DROP rule for the listen port (first
    /// accepted connection that takes the iptables path). Deleted on drop.
    iptables_rule: std::sync::Mutex<Option<Vec<String>>>,
}

impl TcpRawListener {
    pub fn bind(addr: &SocketAddr) -> io::Result<Self> {
        let real = std::net::TcpListener::bind(*addr)?;
        // Keep the listener **blocking**. `accept()` runs inside `cpu_block`.
        real.set_nonblocking(false)?;

        let raw_fd = Arc::new(open_raw_tcp_socket()?);
        let local_port = real.local_addr()?.port();

        let channels = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        let flows = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));

        let (close_tx, close_rx) = async_channel::bounded::<()>(1);

        {
            let raw_fd = raw_fd.clone();
            let channels = channels.clone();
            let flows = flows.clone();
            thread::Builder::new()
                .name("tcpraw-srv-capture".into())
                .spawn(move || {
                    server_capture_thread(raw_fd, channels, flows, local_port, close_rx);
                })?;
        }

        Ok(Self {
            real,
            raw_fd,
            channels,
            flows,
            _close_tx: close_tx,
            iptables_rule: std::sync::Mutex::new(None),
        })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.real.local_addr()
    }

    /// Set DSCP on the shared raw socket (all accepted connections). Matches
    /// Go `TCPConn.SetDSCP` applied to the server listener's handles.
    pub fn set_dscp(&self, dscp: u32) -> io::Result<()> {
        set_raw_dscp(self.raw_fd.as_raw_fd(), dscp)
    }

    pub async fn accept(&self) -> io::Result<(TcpRawConn, SocketAddr)> {
        let listener = self.real.try_clone()?;
        let (stream, peer_addr) = crate::task::cpu_block(move || listener.accept()).await?;
        let _ = stream.set_nonblocking(true);

        let local = stream.local_addr()?;
        let (src_ip, src_port) = socket_addr_to_ipv4(&local)?;
        let (dst_ip, dst_port) = socket_addr_to_ipv4(&peer_addr)?;

        // Register rx channel + flow BEFORE TCP_REPAIR so packets that race
        // in during repair setup are not dropped on the floor.
        let (tx, rx) = async_channel::bounded(CHANNEL_CAP);
        let flow = Arc::new(TcpFlowState {
            seq: AtomicU32::new(0),
            ack: AtomicU32::new(0),
            ts_ecr: AtomicU32::new(0),
        });
        {
            let mut channels = self.channels.lock().unwrap();
            channels.insert(peer_addr, tx);
            let mut flows = self.flows.lock().unwrap();
            flows.insert(peer_addr, flow.clone());
        }

        let (takeover, seq, ack, drain) = match takeover_from_env() {
            Some(Takeover::Repair) => {
                let st = capture_repair_state(&stream)?;
                (Takeover::Repair, st.seq, st.ack, None)
            }
            Some(Takeover::Iptables) => {
                let _ = set_ttl(&stream, 1);
                install_server_rule(self, src_port)?;
                let drain = spawn_drain_thread(stream.try_clone()?);
                (Takeover::Iptables, 0, 0, drain)
            }
            None => match capture_repair_state(&stream) {
                Ok(st) => (Takeover::Repair, st.seq, st.ack, None),
                Err(_) => {
                    let _ = set_ttl(&stream, 1);
                    install_server_rule(self, src_port)?;
                    let drain = spawn_drain_thread(stream.try_clone()?);
                    (Takeover::Iptables, 0, 0, drain)
                }
            },
        };
        flow.seq.store(seq, Ordering::Relaxed);
        flow.ack.store(ack, Ordering::Relaxed);

        let (close_tx, _close_rx) = async_channel::bounded::<()>(1);

        Ok((
            TcpRawConn {
                _real: stream,
                raw_fd: self.raw_fd.clone(),
                flow,
                src_ip,
                dst_ip,
                src_port,
                dst_port,
                rx,
                close_tx,
                _cap_thread: None,
                takeover,
                iptables_rule: vec![], // server rule is owned by the listener
                _drain: drain,
                listener_reg: Some((self.channels.clone(), self.flows.clone(), peer_addr)),
            },
            peer_addr,
        ))
    }
}

impl Drop for TcpRawListener {
    fn drop(&mut self) {
        if let Some(rule) = self.iptables_rule.lock().unwrap().take() {
            rule_delete(&rule);
        }
        let _ = self._close_tx.try_send(());
    }
}

// ─── Capture ─────────────────────────────────────────────────────────────────

/// Apply inbound TCP header to flow state (Go captureFlow semantics).
///
/// Caller must ensure `seg` is from the **peer**, not a loopback echo of our
/// own TX (raw IPPROTO_TCP delivers both).
fn update_flow_from_segment(flow: &TcpFlowState, seg: &TcpSegmentView<'_>) {
    // When peer ACKs, our next seq is peer's ACK number (Go: e.seq = tcp.Ack).
    if seg.ack_flag {
        flow.seq.store(seg.ack, Ordering::Relaxed);
    }
    if let Some(ts) = seg.ts_val {
        flow.ts_ecr.store(ts, Ordering::Relaxed);
    }
    // Advance our ACK to cover peer's payload (+ SYN/FIN).
    let mut next = seg.seq.wrapping_add(seg.payload.len() as u32);
    if seg.syn {
        next = next.wrapping_add(1);
    }
    if seg.fin {
        next = next.wrapping_add(1);
    }
    if next != seg.seq {
        // Go: if e.ack == 0 || e.ack == tcp.Seq { e.ack = nextSeq }
        let cur = flow.ack.load(Ordering::Relaxed);
        if cur == 0 || cur == seg.seq {
            flow.ack.store(next, Ordering::Relaxed);
        }
    }
}

/// Returns true when an inbound segment must be ignored entirely (no flow
/// update, no payload delivery). A TCP RST means "this TCP flow is broken" —
/// but KCP is the reliability layer, so RST is treated as noise, never as a
/// connection-death signal. The flow only dies via KCP's own timeout.
#[inline]
fn seg_ignored(seg: &TcpSegmentView<'_>) -> bool {
    seg.rst
}

fn server_capture_thread(
    raw_fd: Arc<OwnedFd>,
    channels: Arc<
        std::sync::Mutex<std::collections::HashMap<SocketAddr, async_channel::Sender<Vec<u8>>>>,
    >,
    flows: Arc<std::sync::Mutex<std::collections::HashMap<SocketAddr, Arc<TcpFlowState>>>>,
    local_port: u16,
    close_rx: async_channel::Receiver<()>,
) {
    let mut buf = vec![0u8; 65536];
    let fd = raw_fd.as_raw_fd();

    let tv = libc::timeval {
        tv_sec: 1,
        tv_usec: 0,
    };
    unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            &tv as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::timeval>() as libc::socklen_t,
        );
    }

    loop {
        if close_rx.try_recv().is_ok() {
            return;
        }

        let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
        let mut addr_len = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
        let n = unsafe {
            libc::recvfrom(
                fd,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
                0,
                &mut storage as *mut _ as *mut libc::sockaddr,
                &mut addr_len,
            )
        };

        if n <= 0 {
            continue;
        }
        let n = n as usize;
        let Some(seg) = parse_tcp_segment(&buf[..n]) else {
            continue;
        };
        // Port filter (Go captureFlow).
        if seg.dst_port != local_port {
            continue;
        }
        if seg_ignored(&seg) {
            continue; // RST = noise; never touch flow state or KCP
        }

        let peer_ip = match sockaddr_to_ipv4(&storage) {
            Some(ip) => ip,
            None => continue,
        };
        let peer = SocketAddr::from((peer_ip, seg.src_port));

        {
            let flows = flows.lock().unwrap();
            if let Some(flow) = flows.get(&peer) {
                update_flow_from_segment(flow, &seg);
            }
        }

        // Go only pushes PSH payloads for data delivery.
        if !seg.psh || seg.payload.is_empty() {
            continue;
        }
        let channels = channels.lock().unwrap();
        if let Some(tx) = channels.get(&peer) {
            let _ = tx.try_send(seg.payload.to_vec());
        }
    }
}

fn client_capture_thread(
    raw_fd: Arc<OwnedFd>,
    tx: async_channel::Sender<Vec<u8>>,
    close_rx: async_channel::Receiver<()>,
    flow: Arc<TcpFlowState>,
    local_port: u16,
    peer_port: u16,
) {
    let mut buf = vec![0u8; 65536];
    let fd = raw_fd.as_raw_fd();

    let tv = libc::timeval {
        tv_sec: 1,
        tv_usec: 0,
    };
    unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            &tv as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::timeval>() as libc::socklen_t,
        );
    }

    loop {
        if close_rx.try_recv().is_ok() {
            return;
        }

        let n = unsafe {
            libc::recvfrom(
                fd,
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };

        if n <= 0 {
            continue;
        }
        let n = n as usize;
        let Some(seg) = parse_tcp_segment(&buf[..n]) else {
            continue;
        };
        // Must match our local port AND come from the peer. On loopback,
        // IPPROTO_TCP raw sockets also deliver *our own* TX packets; applying
        // those would corrupt seq/ack and feed our payload back into KCP.
        if seg.dst_port != local_port || seg.src_port != peer_port {
            continue;
        }
        if seg_ignored(&seg) {
            continue; // RST = noise; never touch flow state or KCP
        }
        update_flow_from_segment(&flow, &seg);

        if !seg.psh || seg.payload.is_empty() {
            continue;
        }
        let _ = tx.try_send(seg.payload.to_vec());
    }
}

// ─── Packet Parsing ──────────────────────────────────────────────────────────

struct TcpSegmentView<'a> {
    src_port: u16,
    dst_port: u16,
    seq: u32,
    ack: u32,
    ack_flag: bool,
    psh: bool,
    syn: bool,
    fin: bool,
    rst: bool,
    ts_val: Option<u32>,
    payload: &'a [u8],
}

/// Parse an inbound raw packet.
///
/// Linux `raw(7)`: **receive always includes the IP header**, even for
/// `IPPROTO_TCP` sockets without `IP_HDRINCL`. Send is TCP-segment-only.
fn parse_tcp_segment(buf: &[u8]) -> Option<TcpSegmentView<'_>> {
    if buf.len() < 40 {
        return None;
    }
    // Prefer IP+TCP (normal recv path).
    let (tcp, rest_ok) = if (buf[0] >> 4) == 4 && buf[9] == 6 {
        let ihl = ((buf[0] & 0x0F) as usize) * 4;
        if ihl < 20 || buf.len() < ihl + 20 {
            return None;
        }
        (&buf[ihl..], true)
    } else {
        // Fallback: bare TCP segment (some stacks / loopback quirks).
        (buf, buf.len() >= 20)
    };
    if !rest_ok || tcp.len() < 20 {
        return None;
    }
    let data_off = ((tcp[12] >> 4) as usize) * 4;
    if data_off < 20 || tcp.len() < data_off {
        return None;
    }
    let flags = tcp[13];
    let src_port = u16::from_be_bytes([tcp[0], tcp[1]]);
    let dst_port = u16::from_be_bytes([tcp[2], tcp[3]]);
    let seq = u32::from_be_bytes([tcp[4], tcp[5], tcp[6], tcp[7]]);
    let ack = u32::from_be_bytes([tcp[8], tcp[9], tcp[10], tcp[11]]);
    let ts_val = if data_off > 20 {
        extract_ts_val(&tcp[20..data_off])
    } else {
        None
    };
    Some(TcpSegmentView {
        src_port,
        dst_port,
        seq,
        ack,
        ack_flag: flags & 0x10 != 0,
        psh: flags & 0x08 != 0,
        syn: flags & 0x02 != 0,
        fin: flags & 0x01 != 0,
        rst: flags & 0x04 != 0,
        ts_val,
        payload: &tcp[data_off..],
    })
}

fn sockaddr_to_ipv4(storage: &libc::sockaddr_storage) -> Option<[u8; 4]> {
    if storage.ss_family as i32 != libc::AF_INET as i32 {
        return None;
    }
    let sin = unsafe { &*(storage as *const _ as *const libc::sockaddr_in) };
    // s_addr is network-order in memory; copy the four bytes as-is.
    Some(sin.sin_addr.s_addr.to_ne_bytes())
}

fn extract_ts_val(options: &[u8]) -> Option<u32> {
    let mut i = 0;
    while i + 1 < options.len() {
        let kind = options[i];
        if kind <= 1 {
            i += 1;
            continue;
        }
        if i + 1 >= options.len() {
            break;
        }
        let len = options[i + 1] as usize;
        if len < 2 || i + len > options.len() {
            break;
        }
        if kind == 8 && len == 10 {
            return Some(u32::from_be_bytes([
                options[i + 2],
                options[i + 3],
                options[i + 4],
                options[i + 5],
            ]));
        }
        i += len;
    }
    None
}

/// Establish takeover of `stream` after its 3-way handshake.
///
/// Returns (method, installed iptables rule — client only, empty otherwise,
/// drain thread handle, initial (seq, ack)). Repair is preferred; on probe
/// failure we fall back to the iptables TTL-DROP method. If both fail the
/// error propagates (no silent UDP fallback).
fn takeover_stream(
    stream: &std::net::TcpStream,
    local: &SocketAddr,
    remote: &SocketAddr,
) -> io::Result<(
    Takeover,
    Vec<String>,
    Option<thread::JoinHandle<()>>,
    (u32, u32),
)> {
    let (l_ip, l_port) = socket_addr_to_ipv4(local)?;
    let (r_ip, r_port) = socket_addr_to_ipv4(remote)?;

    let try_repair = || -> io::Result<(u32, u32)> {
        match capture_repair_state(stream) {
            Ok(st) => Ok((st.seq, st.ack)),
            Err(e) => {
                let _ = set_tcp_repair(stream, false);
                Err(e)
            }
        }
    };

    let try_iptables = || -> io::Result<(
        Takeover,
        Vec<String>,
        Option<thread::JoinHandle<()>>,
        (u32, u32),
    )> {
        let rule = ttl_drop_rule_client(
            &std::net::Ipv4Addr::from(l_ip).to_string(),
            l_port,
            &std::net::Ipv4Addr::from(r_ip).to_string(),
            r_port,
        );
        set_ttl(stream, 1)?;
        rule_delete(&rule);
        rule_add(&rule)?;
        let drain = spawn_drain_thread(stream.try_clone()?);
        Ok((Takeover::Iptables, rule, drain, (0, 0)))
    };

    match takeover_from_env() {
        Some(Takeover::Repair) => {
            let (seq, ack) = try_repair()?;
            Ok((Takeover::Repair, vec![], None, (seq, ack)))
        }
        Some(Takeover::Iptables) => try_iptables(),
        None => match try_repair() {
            Ok((seq, ack)) => Ok((Takeover::Repair, vec![], None, (seq, ack))),
            Err(_) => try_iptables(),
        },
    }
}

fn install_server_rule(listener: &TcpRawListener, port: u16) -> io::Result<()> {
    let mut guard = listener.iptables_rule.lock().unwrap();
    if guard.is_none() {
        let rule = ttl_drop_rule_server(port);
        rule_delete(&rule);
        rule_add(&rule)?;
        *guard = Some(rule);
    }
    Ok(())
}

// ─── Entry Points ────────────────────────────────────────────────────────────

/// Dial a TCP raw connection to `remote_addr` (client side).
pub fn dial(remote_addr: &SocketAddr) -> io::Result<TcpRawConn> {
    let stream = std::net::TcpStream::connect(*remote_addr)?;
    let local = stream.local_addr()?;
    let (src_ip, src_port) = socket_addr_to_ipv4(&local)?;
    let (dst_ip, dst_port) = socket_addr_to_ipv4(remote_addr)?;

    let (takeover, iptables_rule, drain, (seq, ack)) =
        takeover_stream(&stream, &local, remote_addr)?;

    let raw_fd = Arc::new(open_raw_tcp_socket()?);
    let flow = Arc::new(TcpFlowState {
        seq: AtomicU32::new(seq),
        ack: AtomicU32::new(ack),
        ts_ecr: AtomicU32::new(0),
    });

    let (tx, rx) = async_channel::bounded(CHANNEL_CAP);
    let (close_tx, close_rx) = async_channel::bounded::<()>(1);

    let cap_handle = {
        let raw_fd = raw_fd.clone();
        let flow = flow.clone();
        thread::Builder::new()
            .name("tcpraw-cli-capture".into())
            .spawn(move || {
                client_capture_thread(raw_fd, tx, close_rx, flow, src_port, dst_port);
            })?
    };

    Ok(TcpRawConn {
        _real: stream,
        raw_fd,
        flow,
        src_ip,
        dst_ip,
        src_port,
        dst_port,
        rx,
        close_tx,
        _cap_thread: Some(cap_handle),
        takeover,
        iptables_rule,
        _drain: drain,
        listener_reg: None,
    })
}

/// Create a TCP raw listener bound to `addr` (server side).
pub fn listen(addr: &SocketAddr) -> io::Result<TcpRawListener> {
    TcpRawListener::bind(addr)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// TCP flag bits (RFC 793).
    const F_RST: u8 = 0x04;
    const F_PSH: u8 = 0x08;
    const F_ACK: u8 = 0x10;

    /// Build a bare IP+TCP packet for the parser (no options, 20B each).
    fn ip_tcp_packet(flags: u8, seq: u32, ack_num: u32, payload: &[u8]) -> Vec<u8> {
        let mut pkt = vec![0u8; 40 + payload.len()];
        pkt[0] = 0x45; // IPv4, IHL 5
        pkt[9] = 6; // protocol = TCP
        let pkt_len = pkt.len() as u16;
        pkt[2..4].copy_from_slice(&pkt_len.to_be_bytes());
        pkt[20..22].copy_from_slice(&12345u16.to_be_bytes()); // src
        pkt[20 + 2..20 + 4].copy_from_slice(&29900u16.to_be_bytes()); // dst
        pkt[20 + 4..20 + 8].copy_from_slice(&seq.to_be_bytes());
        pkt[20 + 8..20 + 12].copy_from_slice(&ack_num.to_be_bytes());
        pkt[20 + 12] = 0x50; // data offset 5
        pkt[20 + 13] = flags;
        if !payload.is_empty() {
            pkt[40..].copy_from_slice(payload);
        }
        pkt
    }

    #[test]
    fn parse_detects_rst_flag() {
        let pkt = ip_tcp_packet(F_RST | F_ACK, 100, 200, b"");
        let seg = parse_tcp_segment(&pkt).expect("parse");
        assert!(seg.rst);
        assert!(seg.ack_flag);
        assert_eq!(seg.seq, 100);
        assert_eq!(seg.ack, 200);
    }

    #[test]
    fn seg_ignored_true_only_for_rst() {
        let pkt_rst = ip_tcp_packet(F_RST | F_ACK, 0, 0, b"");
        let pkt_data = ip_tcp_packet(F_PSH | F_ACK, 0, 0, b"x");
        let pkt_ack = ip_tcp_packet(F_ACK, 0, 0, b"");
        let rst = parse_tcp_segment(&pkt_rst).unwrap();
        let data = parse_tcp_segment(&pkt_data).unwrap();
        let ack = parse_tcp_segment(&pkt_ack).unwrap();
        assert!(seg_ignored(&rst));
        assert!(!seg_ignored(&data));
        assert!(!seg_ignored(&ack));
    }
}
#[cfg(all(test, target_os = "linux"))]
mod integration_tests {
    use super::*;
    use std::net::SocketAddr;
    use std::time::Duration;

    fn root_test() -> bool {
        std::env::var("KCPTCP_ROOT_TEST").is_ok() && unsafe { libc::geteuid() } == 0
    }

    fn skip() {
        eprintln!("skipped (needs Linux root + KCPTCP_ROOT_TEST=1)");
    }

    async fn poll_accept(listener: &TcpRawListener) -> io::Result<(TcpRawConn, SocketAddr)> {
        crate::timeout(Duration::from_secs(3), listener.accept())
            .await
            .unwrap()
    }

    /// Returns (client conn, accepted server conn, server listener). The
    /// listener MUST stay alive for the duration of the test: dropping it
    /// signals the shared capture thread to exit, silently breaking delivery
    /// of packets that arrive afterward.
    async fn pair_conns() -> io::Result<(TcpRawConn, TcpRawConn, TcpRawListener)> {
        let server = TcpRawListener::bind(&SocketAddr::from(([127, 0, 0, 1], 0)))?;
        let addr = server.local_addr()?;
        let client = dial(&addr)?;
        let (accepted, _) = poll_accept(&server).await?;
        Ok((client, accepted, server))
    }

    /// Sends a forged RST+ACK segment for the given flow.
    ///
    /// Loopback-only: `sendto` targets the remote's IP, but the kernel assigns
    /// the *local* IP as the packet's source. On 127.0.0.1 the two IPs coincide,
    /// so the forged RST's source IP matches the real flow entry and the
    /// RST-suppression path is exercised. On any non-loopback address the source
    /// IP would be the local external IP rather than the peer's, the flow lookup
    /// would fail, and the forged RST would be silently ignored — testing nothing.
    fn send_forged_rst(local: SocketAddr, remote: SocketAddr) -> io::Result<()> {
        let fd = open_raw_tcp_socket()?;
        let (l_ip, l_port) = socket_addr_to_ipv4(&local)?;
        let (r_ip, r_port) = socket_addr_to_ipv4(&remote)?;
        let pkt = build_tcp_segment(&l_ip, &r_ip, l_port, r_port, 0, 0, 0, 0, &[]);
        let mut pkt = pkt;
        // build_tcp_segment returns a bare TCP segment (no IP prefix); the
        // flags byte is at offset 13. Default is PSH|ACK (0x18) — forge RST|ACK.
        pkt[13] = 0x14; // RST | ACK
        let dst = libc::sockaddr_in {
            sin_family: libc::AF_INET as libc::sa_family_t,
            sin_port: 0,
            sin_addr: libc::in_addr {
                s_addr: u32::from_ne_bytes(r_ip),
            },
            sin_zero: [0; 8],
        };
        unsafe {
            libc::sendto(
                fd.as_raw_fd(),
                pkt.as_ptr() as *const libc::c_void,
                pkt.len(),
                0,
                &dst as *const _ as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
            );
        }
        Ok(())
    }

    /// Restores `KCPTCP_TAKEOVER` on drop (even on test failure/panic).
    struct TakeoverEnv;
    impl TakeoverEnv {
        fn set(v: &str) -> Self {
            std::env::set_var("KCPTCP_TAKEOVER", v);
            TakeoverEnv
        }
    }
    impl Drop for TakeoverEnv {
        fn drop(&mut self) {
            std::env::remove_var("KCPTCP_TAKEOVER");
        }
    }

    /// Spawns a thread that sniffs the loopback interface for TCP RST segments
    /// belonging to the flow of `flow_addr` (its port is one endpoint of the
    /// 4-tuple). Runs for a bounded ~2s window, then returns every matching RST
    /// as a `Vec` (empty = the kernel/peer never emitted RST for the flow).
    fn spawn_rst_sniffer(flow_addr: SocketAddr) -> thread::JoinHandle<Vec<String>> {
        thread::Builder::new()
            .name("tcpraw-rst-sniffer".into())
            .spawn(move || {
                let fd = match open_raw_tcp_socket() {
                    Ok(fd) => fd,
                    Err(_) => return Vec::new(), // can't sniff; treat as "no RST"
                };
                let tv = libc::timeval {
                    tv_sec: 1,
                    tv_usec: 0,
                };
                unsafe {
                    libc::setsockopt(
                        fd.as_raw_fd(),
                        libc::SOL_SOCKET,
                        libc::SO_RCVTIMEO,
                        &tv as *const _ as *const libc::c_void,
                        std::mem::size_of::<libc::timeval>() as libc::socklen_t,
                    );
                }
                let port = flow_addr.port();
                let deadline = std::time::Instant::now() + Duration::from_secs(2);
                let mut buf = vec![0u8; 65536];
                let mut rsts = Vec::new();
                while std::time::Instant::now() < deadline {
                    let n = unsafe {
                        libc::recvfrom(
                            fd.as_raw_fd(),
                            buf.as_mut_ptr() as *mut libc::c_void,
                            buf.len(),
                            0,
                            std::ptr::null_mut(),
                            std::ptr::null_mut(),
                        )
                    };
                    if n < 0 {
                        thread::sleep(Duration::from_millis(10)); // EAGAIN (SO_RCVTIMEO) etc.
                        continue;
                    }
                    if n == 0 {
                        continue;
                    }
                    if let Some(seg) = parse_tcp_segment(&buf[..n as usize]) {
                        if seg.rst && (seg.src_port == port || seg.dst_port == port) {
                            rsts.push(format!(
                                "RST src={} dst={} seq={} ack={}",
                                seg.src_port, seg.dst_port, seg.seq, seg.ack
                            ));
                        }
                    }
                }
                rsts
            })
            .unwrap()
    }

    #[test]
    fn loopback_roundtrip_repair() {
        if !root_test() {
            skip();
            return;
        }
        // Force the repair takeover explicitly: these tests read a process-global
        // env var, so they must run serially (--test-threads=1) and each pin its
        // own mode to avoid a parallel env race.
        let _env = TakeoverEnv::set("repair");
        crate::block_on(async {
            let (c, s, _server) = pair_conns().await.unwrap();
            assert!(matches!(c.takeover, Takeover::Repair));
            c.send(b"ping-repair").unwrap();
            let mut buf = [0u8; 64];
            let n = crate::timeout(Duration::from_secs(2), s.recv(&mut buf))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(&buf[..n], b"ping-repair");
        });
    }

    #[test]
    fn loopback_roundtrip_iptables() {
        if !root_test() {
            skip();
            return;
        }
        let _env = TakeoverEnv::set("iptables");
        crate::block_on(async {
            let (c, s, _server) = pair_conns().await.unwrap();
            assert!(matches!(c.takeover, Takeover::Iptables));
            c.send(b"ping-iptables").unwrap();
            let mut buf = [0u8; 64];
            let n = crate::timeout(Duration::from_secs(2), s.recv(&mut buf))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(&buf[..n], b"ping-iptables");
        });
    }

    #[test]
    fn forged_rst_does_not_kill_flow() {
        if !root_test() {
            skip();
            return;
        }
        crate::block_on(async {
            let (c, s, _server) = pair_conns().await.unwrap();
            send_forged_rst(c.local_addr().unwrap(), s.local_addr().unwrap()).unwrap();
            c.send(b"after-rst").unwrap();
            let mut buf = [0u8; 64];
            let n = crate::timeout(Duration::from_secs(2), s.recv(&mut buf))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(&buf[..n], b"after-rst");
        });
    }

    #[test]
    fn no_rst_on_wire_during_transfer_and_close() {
        if !root_test() {
            skip();
            return;
        }
        crate::block_on(async {
            let (c, s, _server) = pair_conns().await.unwrap();
            let flow_addr = s.local_addr().unwrap(); // server conn: its port is one 4-tuple end
            let sniffer = spawn_rst_sniffer(flow_addr);
            for i in 0..50 {
                c.send(&[i as u8; 16]).unwrap();
            }
            c.close();
            s.close();
            drop(c);
            drop(s);
            let rsts = sniffer.join().unwrap();
            assert!(rsts.is_empty(), "kernel/peer emitted RST: {rsts:?}");
        });
    }

    #[test]
    fn iptables_rule_cleaned_on_close() {
        if !root_test() {
            skip();
            return;
        }
        let _guard = TakeoverEnv::set("iptables");
        crate::block_on(async {
            let (c, s, _server) = pair_conns().await.unwrap();
            let client = c.local_addr().unwrap();
            let server = s.local_addr().unwrap();
            let rule = ttl_drop_rule_client(
                &client.ip().to_string(),
                client.port(),
                &server.ip().to_string(),
                server.port(),
            );
            assert!(rule_exists(&rule));
            c.close();
            assert!(!rule_exists(&rule));
        });
    }
}
