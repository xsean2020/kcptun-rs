//! kcptun-server -- KCP-based TCP stream accelerator (server side).
//!
//! A Rust port of the Go kcptun server.
//! Listens on UDP for KCP connections, accepts SMUX streams, forwards to TCP targets.

#![allow(
    clippy::collapsible_match,
    clippy::question_mark,
    clippy::explicit_auto_deref,
    clippy::redundant_closure,
    clippy::too_many_arguments
)]

use mimalloc::MiMalloc;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

use dashmap::DashMap;
use std::collections::HashSet;
use std::io::{self, Write};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use anyhow::{Context as AnyContext, Result};
use bytes::{Buf, BytesMut};
use clap::Parser;
use log::{debug, error, info, warn};
use serde::Deserialize;
#[cfg(feature = "tokio")]
use kio::ReadBuf;
use kio::{self, AsyncRead, AsyncWrite};

use kcp_rs::FecDecoder;
use kcp_rs::KCP;
use kcrypt_rs::crypt as kcp_crypt;

// ─── Constants ──────────────────────────────────────────────────────────────────

/// PBKDF2 salt matching the Go kcp-go SALT value.
const SALT: &[u8] = b"kcp-go";

/// Maximum UDP datagram size.
const MAX_DATAGRAM: usize = 2048;

/// Pipe buffer size.
const PIPE_BUF_SIZE: usize = 65536;

/// How often the KCP update loop fires (milliseconds).
const KCP_UPDATE_INTERVAL_MS: u64 = 2;

// ─── KCP-level Snappy compression (matching Go) ────────────────────────────

// Note: Compression is handled by the persistent snap::write::FrameEncoder
// in the KcpServerSession.compressor field, matching Go's snappy.NewBufferedWriter.
// Decompression is handled by SnappyStreamDecoder, matching Go's snappy.NewReader.

/// Persistent Snappy stream decoder. Uses manual Snappy framed format
/// parsing (like client's GoSnappyStream) so it correctly handles the
/// stream identifier that only appears once from a persistent encoder.
/// This matches Go's snappy.NewReader behavior.
struct SnappyStreamDecoder {
    buf: Vec<u8>,
    pos: usize,
    hdr_ok: bool,
}

impl SnappyStreamDecoder {
    fn new() -> Self {
        SnappyStreamDecoder {
            buf: Vec::new(),
            pos: 0,
            hdr_ok: false,
        }
    }
    fn feed(&mut self, data: &[u8]) -> io::Result<Vec<u8>> {
        self.buf.extend_from_slice(data);
        if self.pos > 65536 {
            self.buf.drain(..self.pos);
            self.pos = 0;
        }
        let mut out = Vec::new();
        loop {
            let avail = self.buf.len() - self.pos;
            // Skip stream identifier (0xFF 0x06 0x00 0x00 "sNaPpY")
            if !self.hdr_ok {
                if avail < 10 {
                    break;
                }
                if self.buf[self.pos] != 0xff || &self.buf[self.pos + 4..self.pos + 10] != b"sNaPpY"
                {
                    // Not a stream identifier — skip one byte and try to resync
                    self.pos += 1;
                    continue;
                }
                self.pos += 10;
                self.hdr_ok = true;
                continue;
            }
            // Read chunk header: [type 1B][length 3B LE]
            if avail < 4 {
                break;
            }
            let ct = self.buf[self.pos];
            let chunk_len = u32::from_le_bytes([
                self.buf[self.pos + 1],
                self.buf[self.pos + 2],
                self.buf[self.pos + 3],
                0,
            ]) as usize;
            if chunk_len > 16_777_216 {
                self.pos += 4 + chunk_len.min(avail - 4);
                continue;
            }
            if 4 + chunk_len > avail {
                break;
            }
            let chunk_data = &self.buf[self.pos + 4..self.pos + 4 + chunk_len];
            self.pos += 4 + chunk_len;
            match ct {
                0x00 => {
                    // Compressed chunk: [CRC32 4B][snappy block]
                    if chunk_data.len() < 4 {
                        continue;
                    }
                    let snappy_data = &chunk_data[4..];
                    match snap::raw::Decoder::new().decompress_vec(snappy_data) {
                        Ok(d) => out.extend(d),
                        Err(_) => continue,
                    }
                }
                0x01 => {
                    // Uncompressed chunk: [CRC32 4B][raw data]
                    if chunk_data.len() >= 4 {
                        out.extend_from_slice(&chunk_data[4..]);
                    }
                }
                _ => {}
            }
        }
        Ok(out)
    }
}

// ─── Log file rotation ─────────────────────────────────────────────────────────

/// Rotate log file if it exceeds max_size bytes. Keeps up to 5 rotated copies.
fn rotate_log(log_path: &str, max_size: u64) {
    if let Ok(meta) = std::fs::metadata(log_path) {
        if meta.len() > max_size {
            for i in (1..5).rev() {
                let old = format!("{}.{}", log_path, i);
                let new = format!("{}.{}", log_path, i + 1);
                let _ = std::fs::rename(&old, &new);
            }
            let _ = std::fs::rename(log_path, format!("{}.1", log_path));
        }
    }
}

// ─── Config (JSON config file support) ──────────────────────────────────────────

/// Configuration struct matching the kcptun JSON config format.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub listen: Option<String>,
    pub target: Option<String>,
    pub key: Option<String>,
    pub crypt: Option<String>,
    pub mode: Option<String>,
    pub ratelimit: Option<u32>,
    pub mtu: Option<u32>,
    pub sndwnd: Option<u32>,
    pub rcvwnd: Option<u32>,
    pub datashard: Option<u32>,
    pub parityshard: Option<u32>,
    pub dscp: Option<u32>,
    pub nocomp: Option<bool>,
    pub acknodelay: Option<bool>,
    pub nodelay: Option<u32>,
    pub interval: Option<u32>,
    pub resend: Option<u32>,
    pub nc: Option<u32>,
    pub sockbuf: Option<u32>,
    pub smuxver: Option<u8>,
    pub smuxbuf: Option<usize>,
    pub streambuf: Option<usize>,
    pub framesize: Option<usize>,
    pub keepalive: Option<u64>,
    pub closewait: Option<u64>,
    pub snmplog: Option<String>,
    pub snmpperiod: Option<u64>,
    pub log: Option<String>,
    pub quiet: Option<bool>,
    pub tcp: Option<bool>,
    pub pprof: Option<String>,
    pub qpp: Option<bool>,
    pub qppcount: Option<u16>,
}

// ─── CLI Args ───────────────────────────────────────────────────────────────────

/// kcptun server -- accept KCP connections and forward to TCP targets.
#[derive(Debug, Parser)]
#[command(name = "kcptun-server", about, version, disable_version_flag = true)]
pub struct Cli {
    /// KCP listen address (UDP).
    #[arg(short = 'l', long, default_value = ":29900")]
    pub listen: Option<String>,

    /// TCP target address to forward connections to.
    #[arg(short = 't', long, default_value = "127.0.0.1:12948")]
    pub target: Option<String>,

    /// Pre-shared secret between client and server.
    #[arg(short, long, default_value = "it's a secrect", env = "KCPTUN_KEY")]
    pub key: Option<String>,

    /// Encryption method: aes, aes-128, aes-128-gcm, aes-192, salsa20, blowfish,
    /// twofish, cast5, 3des, tea, xtea, xor, sm4, none, null.
    #[arg(long, default_value = "aes")]
    pub crypt: Option<String>,

    /// Protocol mode: normal, fast, fast2, fast3.
    #[arg(short, long, default_value = "fast")]
    pub mode: Option<String>,

    /// Rate limit in bytes per second per connection (0 = disabled).
    #[arg(long, default_value_t = 0)]
    pub ratelimit: u32,

    /// MTU value.
    #[arg(long)]
    pub mtu: Option<u32>,

    /// Send window size.
    #[arg(long)]
    pub sndwnd: Option<u32>,

    /// Receive window size.
    #[arg(long)]
    pub rcvwnd: Option<u32>,

    /// FEC data shards.
    #[arg(long, default_value_t = 10)]
    pub datashard: u32,

    /// FEC parity shards.
    #[arg(long, default_value_t = 3)]
    pub parityshard: u32,

    /// DSCP value for IP packets.
    #[arg(long)]
    pub dscp: Option<u32>,

    /// Disable compression.
    #[arg(long, default_value_t = false, action = clap::ArgAction::SetTrue)]
    pub nocomp: bool,

    /// Enable ACK nodelay.
    #[arg(long, default_value_t = false, action = clap::ArgAction::SetTrue)]
    pub acknodelay: bool,

    /// Enable KCP nodelay.
    #[arg(long)]
    pub nodelay: Option<u32>,

    /// KCP update interval in ms.
    #[arg(long)]
    pub interval: Option<u32>,

    /// KCP fast resend threshold.
    #[arg(long)]
    pub resend: Option<u32>,

    /// KCP no congestion control flag.
    #[arg(long)]
    pub nc: Option<u32>,

    /// Socket buffer size in bytes.
    #[arg(long)]
    pub sockbuf: Option<u32>,

    /// SMUX protocol version (1 or 2).
    #[arg(long)]
    pub smuxver: Option<u8>,

    /// SMUX receive buffer size.
    #[arg(long)]
    pub smuxbuf: Option<usize>,

    /// SMUX stream buffer size.
    #[arg(long, default_value_t = 2097152)]
    pub streambuf: usize,

    /// SMUX max frame size.
    #[arg(long, default_value_t = 8192)]
    pub framesize: usize,

    /// SMUX keepalive interval in seconds.
    #[arg(long)]
    pub keepalive: Option<u64>,

    /// Close wait timeout in seconds.
    #[arg(long)]
    pub closewait: Option<u64>,

    /// SNMP log file path.
    #[arg(long)]
    pub snmplog: Option<String>,

    /// SNMP logging period in seconds.
    #[arg(long)]
    pub snmpperiod: Option<u64>,

    /// Log file path.
    #[arg(long)]
    pub log: Option<String>,

    /// Suppress log output.
    #[arg(long, default_value_t = false, action = clap::ArgAction::SetTrue)]
    pub quiet: bool,

    /// Use TCP instead of UDP for the underlying transport.
    #[arg(long, default_value_t = false, action = clap::ArgAction::SetTrue)]
    pub tcp: bool,

    /// Enable pprof HTTP server on the given address.
    #[arg(long)]
    pub pprof: Option<String>,

    /// Enable QPP encryption.
    #[arg(long, default_value_t = false, action = clap::ArgAction::SetTrue)]
    pub qpp: bool,

    /// QPP pad count (should be prime).
    #[arg(long)]
    pub qppcount: Option<u16>,

    /// Path to JSON config file.
    #[arg(short = 'c', long)]
    pub c: Option<String>,

    /// Print version and exit (Go-compatible: `-v` / `--version`).
    #[arg(short = 'v', long = "version", action = clap::ArgAction::SetTrue, default_value_t = false)]
    pub version_flag: bool,
}

impl Cli {
    /// Merge CLI args with config file, CLI taking precedence.
    fn merge(cli: Self, cfg: Config) -> Self {
        Cli {
            listen: cli.listen.or(cfg.listen),
            target: cli.target.or(cfg.target),
            key: cli.key.or(cfg.key),
            crypt: cli.crypt.or(cfg.crypt),
            mode: cli.mode.or(cfg.mode),
            ratelimit: {
                let v = cli.ratelimit;
                if v != 0 {
                    v
                } else {
                    cfg.ratelimit.unwrap_or(0)
                }
            },
            mtu: cli.mtu.or(cfg.mtu),
            sndwnd: cli.sndwnd.or(cfg.sndwnd),
            rcvwnd: cli.rcvwnd.or(cfg.rcvwnd),
            datashard: {
                let v = cli.datashard;
                if v != 10 {
                    v
                } else {
                    cfg.datashard.unwrap_or(10)
                }
            },
            parityshard: {
                let v = cli.parityshard;
                if v != 3 {
                    v
                } else {
                    cfg.parityshard.unwrap_or(3)
                }
            },
            dscp: cli.dscp.or(cfg.dscp),
            nocomp: if cli.nocomp {
                true
            } else {
                cfg.nocomp.unwrap_or(false)
            },
            acknodelay: if cli.acknodelay {
                true
            } else {
                cfg.acknodelay.unwrap_or(false)
            },
            nodelay: cli.nodelay.or(cfg.nodelay),
            interval: cli.interval.or(cfg.interval),
            resend: cli.resend.or(cfg.resend),
            nc: cli.nc.or(cfg.nc),
            sockbuf: cli.sockbuf.or(cfg.sockbuf),
            smuxver: cli.smuxver.or(cfg.smuxver),
            smuxbuf: cli.smuxbuf.or(cfg.smuxbuf),
            streambuf: {
                let v = cli.streambuf;
                if v != 2097152 {
                    v
                } else {
                    cfg.streambuf.unwrap_or(2097152)
                }
            },
            framesize: {
                let v = cli.framesize;
                if v != 8192 {
                    v
                } else {
                    cfg.framesize.unwrap_or(8192)
                }
            },
            keepalive: cli.keepalive.or(cfg.keepalive),
            closewait: cli.closewait.or(cfg.closewait),
            snmplog: cli.snmplog.or(cfg.snmplog),
            snmpperiod: cli.snmpperiod.or(cfg.snmpperiod),
            log: cli.log.or(cfg.log),
            quiet: if cli.quiet {
                true
            } else {
                cfg.quiet.unwrap_or(false)
            },
            tcp: if cli.tcp {
                true
            } else {
                cfg.tcp.unwrap_or(false)
            },
            pprof: cli.pprof.or(cfg.pprof),
            qpp: if cli.qpp {
                true
            } else {
                cfg.qpp.unwrap_or(false)
            },
            qppcount: cli.qppcount.or(cfg.qppcount),
            c: cli.c,
            version_flag: false, // never from config file
        }
    }
}

// ─── Key Derivation ─────────────────────────────────────────────────────────────

/// Derive a 32-byte key from a password using PBKDF2-HMAC-SHA1.
///
/// Matches the Go kcp-go key derivation:
/// `pkcs5.PBKDF2(password, salt, 4096, 32, sha1.New)`
fn derive_key(password: &str) -> [u8; 32] {
    let mut key = [0u8; 32];
    pbkdf2::pbkdf2_hmac::<sha1::Sha1>(password.as_bytes(), SALT, 4096, &mut key);
    key
}

// ─── Mode Profiles ──────────────────────────────────────────────────────────────

/// Apply a mode profile to a KCP instance.
fn apply_mode(kcp: &mut KCP, mode: &str) {
    let (nodelay, interval, resend, nc) = match mode {
        "normal" => (0, 40, 2, 1),
        "fast" => (0, 30, 2, 1),
        "fast2" => (1, 20, 2, 1),
        "fast3" => (1, 10, 2, 1),
        _ => {
            warn!("unknown mode '{}', falling back to 'fast'", mode);
            (0, 30, 2, 1)
        }
    };
    info!(
        "applying mode '{}': nodelay={}, interval={}, resend={}, nc={}",
        mode, nodelay, interval, resend, nc
    );
    kcp.set_nodelay(nodelay, interval, resend, nc);
}

// ─── MultiPort Parser ───────────────────────────────────────────────────────────

/// Parse a "host:port" string into a SocketAddr.
fn parse_addr(addr: &str) -> Result<SocketAddr> {
    // Handle ":port" shorthand by defaulting to "0.0.0.0"
    if addr.starts_with(':') {
        let host_addr = format!("0.0.0.0{}", addr);
        return host_addr.parse::<SocketAddr>().context("invalid address");
    }
    addr.parse::<SocketAddr>().context("invalid address")
}

/// Get the current wall clock time in milliseconds as a u32 (wrapping).
#[allow(dead_code)]
fn now_ms() -> u32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u32
}

// ─── SMUX Async Wrapper ─────────────────────────────────────────────────────────

/// An async wrapper around an SMUX stream, implementing AsyncRead + AsyncWrite.
struct SmuxStreamIo {
    stream: Arc<smux_rs::stream::Stream>,
    /// Notify the flush loop that new data is available for sending.
    flush_notify: Arc<kio::Notify>,
}

impl SmuxStreamIo {
    fn new(stream: Arc<smux_rs::stream::Stream>, flush_notify: Arc<kio::Notify>) -> Self {
        SmuxStreamIo { stream, flush_notify }
    }
}

// ── tokio AsyncRead/AsyncWrite (uses ReadBuf) ──
#[cfg(feature = "tokio")]
impl AsyncRead for SmuxStreamIo {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let space = buf.initialize_unfilled();
        match this.stream.read(space) {
            Ok((0, _)) => Poll::Ready(Ok(())),
            Ok((n, _)) => {
                buf.advance(n);
                Poll::Ready(Ok(()))
            }
            Err(smux_rs::stream::StreamError::WouldBlock) => {
                // Register waker; wakeup_reader() (called by push_data) will wake
                // us immediately when data arrives. Eliminates spawn(sleep(5ms)).
                this.stream.register_read_waker(cx.waker().clone());
                // Re-check after registering: data may have arrived between
                // the WouldBlock and the waker registration (lost-wakeup race).
                let space = buf.initialize_unfilled();
                match this.stream.read(space) {
                    Ok((0, _)) => Poll::Ready(Ok(())),
                    Ok((n, _)) => {
                        buf.advance(n);
                        Poll::Ready(Ok(()))
                    }
                    Err(smux_rs::stream::StreamError::WouldBlock) => Poll::Pending,
                    Err(smux_rs::stream::StreamError::Closed) => Poll::Ready(Ok(())),
                    Err(e) => Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::ConnectionReset,
                        format!("SMUX stream read error: {:?}", e),
                    ))),
                }
            }
            Err(smux_rs::stream::StreamError::Closed) => Poll::Ready(Ok(())),
            Err(e) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                format!("SMUX stream read error: {:?}", e),
            ))),
        }
    }
}

#[cfg(feature = "tokio")]
impl AsyncWrite for SmuxStreamIo {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        match this.stream.write(buf) {
            Ok(n) => {
                // Wake up the flush loop immediately so it drains SMUX
                // and sends through KCP without waiting for the timer.
                this.flush_notify.notify_one();
                Poll::Ready(Ok(n))
            },
            Err(_) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "SMUX stream write error",
            ))),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // Only mark local side as closed — do NOT call close() because that
        // would clear the send buffer (losing data not yet drained by the
        // flush loop) and set remote_closed=true (preventing FIN from being
        // sent). The flush loop will send a FIN frame and fully close the
        // stream after all pending data has been drained.
        let stream = self.get_mut().stream.clone();
        log::debug!(
            "SmuxStreamIo::poll_shutdown: marking stream {} local_closed",
            stream.id()
        );
        stream.mark_local_closed();
        Poll::Ready(Ok(()))
    }
}

// ── smol AsyncRead/AsyncWrite (uses &mut [u8]) ──
#[cfg(feature = "smol")]
impl AsyncRead for SmuxStreamIo {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        match this.stream.read(buf) {
            Ok((0, _)) => Poll::Ready(Ok(0)),
            Ok((n, _)) => Poll::Ready(Ok(n)),
            Err(smux_rs::stream::StreamError::WouldBlock) => {
                this.stream.register_read_waker(cx.waker().clone());
                match this.stream.read(buf) {
                    Ok((0, _)) => Poll::Ready(Ok(0)),
                    Ok((n, _)) => Poll::Ready(Ok(n)),
                    Err(smux_rs::stream::StreamError::WouldBlock) => Poll::Pending,
                    Err(smux_rs::stream::StreamError::Closed) => Poll::Ready(Ok(0)),
                    Err(e) => Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::ConnectionReset,
                        format!("SMUX stream read error: {:?}", e),
                    ))),
                }
            }
            Err(smux_rs::stream::StreamError::Closed) => Poll::Ready(Ok(0)),
            Err(e) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                format!("SMUX stream read error: {:?}", e),
            ))),
        }
    }
}

#[cfg(feature = "smol")]
impl AsyncWrite for SmuxStreamIo {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        match this.stream.write(buf) {
            Ok(n) => {
                // Wake up the flush loop immediately so it drains SMUX
                // and sends through KCP without waiting for the timer.
                this.flush_notify.notify_one();
                Poll::Ready(Ok(n))
            }
            Err(_) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "SMUX stream write error",
            ))),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let stream = self.get_mut().stream.clone();
        log::debug!(
            "SmuxStreamIo::poll_close: marking stream {} local_closed",
            stream.id()
        );
        stream.mark_local_closed();
        Poll::Ready(Ok(()))
    }
}

struct QPPPort<T: AsyncRead + AsyncWrite + Unpin> {
    inner: T,
    qpp: parking_lot::Mutex<qpp_rs::QuantumPermutationPad>,
    prng_enc: parking_lot::Mutex<qpp_rs::Rand>,
    prng_dec: parking_lot::Mutex<qpp_rs::Rand>,
    read_buf: BytesMut,
    /// Reusable buffer for inner.poll_read — eliminates vec![0u8; PIPE_BUF_SIZE] per call.
    read_io_buf: Vec<u8>,
    /// Reusable buffer for QPP encryption — eliminates buf.to_vec() per write.
    write_enc_buf: Vec<u8>,
}

impl<T: AsyncRead + AsyncWrite + Unpin> QPPPort<T> {
    fn new(inner: T, key: &[u8], count: u16) -> Self {
        QPPPort {
            inner,
            qpp: parking_lot::Mutex::new(qpp_rs::QuantumPermutationPad::new(key, count)),
            prng_enc: parking_lot::Mutex::new(qpp_rs::create_prng(key)),
            prng_dec: parking_lot::Mutex::new(qpp_rs::create_prng(key)),
            read_buf: BytesMut::with_capacity(PIPE_BUF_SIZE),
            read_io_buf: vec![0u8; PIPE_BUF_SIZE],
            write_enc_buf: Vec::with_capacity(PIPE_BUF_SIZE),
        }
    }
}

// ── tokio QPPPort AsyncRead/AsyncWrite (uses ReadBuf) ──
#[cfg(feature = "tokio")]
impl<T: AsyncRead + AsyncWrite + Unpin> AsyncRead for QPPPort<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();

        if !this.read_buf.is_empty() {
            let n = buf.remaining().min(this.read_buf.len());
            buf.put_slice(&this.read_buf[..n]);
            this.read_buf.advance(n);
            return Poll::Ready(Ok(()));
        }

        let mut tmp = std::mem::take(&mut this.read_io_buf);
        tmp.resize(PIPE_BUF_SIZE, 0);
        let mut read_buf = ReadBuf::new(&mut tmp);
        match Pin::new(&mut this.inner).poll_read(cx, &mut read_buf) {
            Poll::Ready(Ok(())) => {
                let filled = read_buf.filled().len();
                if filled == 0 {
                    this.read_io_buf = tmp;
                    return Poll::Ready(Ok(()));
                }
                // Decrypt in-place in the read buffer (eliminates to_vec())
                {
                    let qpp = this.qpp.lock();
                    let mut prng = this.prng_dec.lock();
                    qpp_rs::decrypt_with_pads(
                        &qpp.pads,
                        &mut tmp[..filled],
                        &mut prng,
                        qpp.count(),
                    );
                }
                let n = buf.remaining().min(filled);
                buf.put_slice(&tmp[..n]);
                if n < filled {
                    this.read_buf.extend_from_slice(&tmp[n..filled]);
                }
                this.read_io_buf = tmp;
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(e)) => {
                this.read_io_buf = tmp;
                Poll::Ready(Err(e))
            }
            Poll::Pending => {
                this.read_io_buf = tmp;
                Poll::Pending
            }
        }
    }
}

#[cfg(feature = "tokio")]
impl<T: AsyncRead + AsyncWrite + Unpin> AsyncWrite for QPPPort<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        this.write_enc_buf.clear();
        this.write_enc_buf.extend_from_slice(buf);
        {
            let qpp = this.qpp.lock();
            let mut prng = this.prng_enc.lock();
            qpp_rs::encrypt_with_pads(&qpp.pads, &mut this.write_enc_buf, &mut prng, qpp.count());
        }
        Pin::new(&mut this.inner).poll_write(cx, &this.write_enc_buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

// ── smol QPPPort AsyncRead/AsyncWrite (uses &mut [u8]) ──
#[cfg(feature = "smol")]
impl<T: AsyncRead + AsyncWrite + Unpin> AsyncRead for QPPPort<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();

        if !this.read_buf.is_empty() {
            let n = buf.len().min(this.read_buf.len());
            buf[..n].copy_from_slice(&this.read_buf[..n]);
            this.read_buf.advance(n);
            return Poll::Ready(Ok(n));
        }

        let mut tmp = std::mem::take(&mut this.read_io_buf);
        tmp.resize(PIPE_BUF_SIZE, 0);
        match Pin::new(&mut this.inner).poll_read(cx, &mut tmp) {
            Poll::Ready(Ok(0)) => {
                this.read_io_buf = tmp;
                Poll::Ready(Ok(0))
            }
            Poll::Ready(Ok(filled)) => {
                {
                    let qpp = this.qpp.lock();
                    let mut prng = this.prng_dec.lock();
                    qpp_rs::decrypt_with_pads(
                        &qpp.pads,
                        &mut tmp[..filled],
                        &mut prng,
                        qpp.count(),
                    );
                }
                let n = buf.len().min(filled);
                buf[..n].copy_from_slice(&tmp[..n]);
                if n < filled {
                    this.read_buf.extend_from_slice(&tmp[n..filled]);
                }
                this.read_io_buf = tmp;
                Poll::Ready(Ok(n))
            }
            Poll::Ready(Err(e)) => {
                this.read_io_buf = tmp;
                Poll::Ready(Err(e))
            }
            Poll::Pending => {
                this.read_io_buf = tmp;
                Poll::Pending
            }
        }
    }
}

#[cfg(feature = "smol")]
impl<T: AsyncRead + AsyncWrite + Unpin> AsyncWrite for QPPPort<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        this.write_enc_buf.clear();
        this.write_enc_buf.extend_from_slice(buf);
        {
            let qpp = this.qpp.lock();
            let mut prng = this.prng_enc.lock();
            qpp_rs::encrypt_with_pads(&qpp.pads, &mut this.write_enc_buf, &mut prng, qpp.count());
        }
        Pin::new(&mut this.inner).poll_write(cx, &this.write_enc_buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_close(cx)
    }
}

// ─── Pipe ───────────────────────────────────────────────────────────────────────

/// Bidirectional copy between two AsyncRead + AsyncWrite streams.
///
/// `idle_secs` is an **idle** timeout (not a total timeout): the timer
/// resets after every successful data transfer. If no data flows in either
/// direction for `idle_secs` seconds, the pipe breaks gracefully.
///
/// This matches Go kcptun's behavior where `closeWait` is an idle/cleanup
/// period, NOT a total pipe duration limit. Using a total timeout here caused
/// intermittent test failures under load: with 100 concurrent connections
/// each transferring 192 KB, the bidirectional copy could exceed the 30-second
/// `closewait` default, causing the server to close the SMUX stream before
/// all echo data was delivered.
async fn pipe<A, B>(a: &mut A, b: &mut B, idle_secs: u64) -> Result<(u64, u64)>
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    kio::copy_bidirectional_idle(a, b, idle_secs)
        .await
        .map_err(Into::into)
}

/// Handle a single SMUX stream: connect to the TCP target and pipe data
/// bidirectionally with optional QPP. Compression is handled at the
/// KCP/SMUX session level (matching Go kcptun architecture).
async fn handle_stream(
    target: String,
    smux_stream: Arc<smux_rs::stream::Stream>,
    stream_id: u32,
    qpp_enabled: bool,
    qpp_key: Vec<u8>,
    qpp_count: u16,
    quiet: bool,
    close_wait: u64,
    flush_notify: Arc<kio::Notify>,
) -> Result<()> {
    let tcp = kio::TcpStream::connect(&target)
        .await
        .with_context(|| format!("failed to connect to target {}", target))?;

    if !quiet {
        info!("stream {} connected to target {}", stream_id, target);
    }

    let smux_io = SmuxStreamIo::new(smux_stream.clone(), flush_notify);

    // Default idle timeout to prevent FD leaks when close_wait is 0.
    // If neither side sends data for this duration, the pipe is closed.
    const DEFAULT_IDLE_TIMEOUT: u64 = 300; // 5 minutes
    let effective_close_wait = if close_wait > 0 {
        close_wait
    } else {
        DEFAULT_IDLE_TIMEOUT
    };

    let pipe_result = if qpp_enabled {
        let qpp_port = QPPPort::new(smux_io, &qpp_key, qpp_count);
        let mut tcp_pin = tcp;
        let mut qpp_pin = qpp_port;
        pipe(&mut tcp_pin, &mut qpp_pin, effective_close_wait).await
    } else {
        let mut tcp_pin = tcp;
        let mut smux_pin = smux_io;
        debug!("server pipe started for stream {}", stream_id);
        pipe(&mut tcp_pin, &mut smux_pin, effective_close_wait).await
    };

    // Ensure the SMUX stream is properly closed regardless of pipe outcome.
    // This is critical: when the pipe times out, poll_shutdown is NOT called
    // by copy_bidirectional, so we must explicitly mark the stream as closed
    // to allow the flush loop's cleanup (Phase 1a) to remove it.
    smux_stream.mark_local_closed();
    if !smux_stream.is_fin_sent() {
        smux_stream.mark_fin_sent();
    }

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

// ─── KcpServerSession ───────────────────────────────────────────────────────────

/// A server-side KCP session representing one connection from a remote peer.
///
/// Each session owns a KCP state machine, an SMUX server session, and a
/// background flush loop. Incoming encrypted datagrams are fed via `feed_data`,
/// which decrypts, strips the Go kcp-go v5 outer header, and drives the KCP
/// state machine to extract reassembled user data (SMUX frames).
struct KcpServerSession {
    /// KCP state machine (shared between the recv and flush tasks).
    kcp: Arc<parking_lot::Mutex<KCP>>,
    /// Block cipher for encrypting/decrypting KCP wire data.
    /// Stored as `Arc<dyn BlockCrypt>` (no Mutex) because `encrypt`/`decrypt`
    /// take `&self` — the cipher is stateless after construction.
    crypt: Arc<dyn kcrypt_rs::BlockCrypt>,
    /// AEAD crypt for GCM mode (separate from BlockCrypt).
    aead: Option<Arc<dyn kcrypt_rs::AeadCrypt>>,
    /// SMUX server session multiplexing streams over KCP.
    smux: Arc<smux_rs::Session>,
    /// Set of SMUX stream IDs that have already been accepted and dispatched.
    handled_streams: Arc<parking_lot::Mutex<HashSet<u32>>>,
    /// Peer address for sending responses.
    peer: SocketAddr,
    /// Background task handles.
    _handles: Vec<kio::JoinHandle<()>>,
    /// Disable Snappy compression (matches Go --nocomp).
    nocomp: bool,
    /// Raw KCP segments collected by the output callback during flush/input.
    ///
    /// Each entry is a `Bytes` (reference-counted slice of the KCP output
    /// buffer) handed directly by the output callback — no per-packet
    /// `Vec` alloc + `extend_from_slice` copy (R2: output Bytes pipeline).
    raw_packets: Arc<parking_lot::Mutex<Vec<bytes::Bytes>>>,
    /// Persistent Snappy framing decoder (Go interop fallback).
    snappy_fallback: Option<parking_lot::Mutex<SnappyStreamDecoder>>,
    /// UDP socket for sending (shared with the recv loop).
    udp: Arc<kio::UdpSocket>,
    /// Whether encryption is enabled.
    has_encryption: bool,
    /// Whether ACK nodelay is enabled.
    ack_nodelay: bool,
    /// Optional FEC decoder for Reed-Solomon error correction recovery.
    fec_decoder: Option<parking_lot::Mutex<FecDecoder>>,
    /// Persistent Snappy framing compressor. Uses snap's FrameEncoder
    /// (CRC32C/Castagnoli) matching Go's golang/snappy for interop.
    compressor: Option<Arc<parking_lot::Mutex<snap::write::FrameEncoder<Vec<u8>>>>>,
    /// Reusable encryption buffer with counter-based nonce.
    /// Eliminates per-packet vec![] allocation and rand::thread_rng() calls.
    crypto_buf: Arc<parking_lot::Mutex<kcp_rs::CryptoBuf>>,
    /// Notify for waking up the flush loop immediately when SMUX streams
    /// have new data to send. Eliminates the 0~10ms wait of the fixed
    /// sleep interval.
    flush_notify: Arc<kio::Notify>,
}

impl KcpServerSession {
    /// Create a new server-side KCP session for the given peer.
    #[allow(clippy::too_many_arguments)]
    fn new(
        conv: u32,
        peer: SocketAddr,
        udp: &Arc<kio::UdpSocket>,
        key: &[u8; 32],
        crypt_method: &str,
        mode: &str,
        mtu: u32,
        sndwnd: u32,
        rcvwnd: u32,
        datashard: u32,
        parityshard: u32,
        acknodelay: bool,
        nodelay: u32,
        interval: u32,
        resend: u32,
        nc: u32,
        smuxver: u8,
        smuxbuf: usize,
        streambuf: usize,
        framesize: usize,
        keepalive: u64,
        nocomp: bool,
    ) -> Self {
        // CryptEngine: match-based dispatch on encrypt/decrypt hot path.
        let (engine, _) = kcrypt_rs::CryptEngine::select(crypt_method, &key[..]);
        let crypt_state: Arc<dyn kcrypt_rs::BlockCrypt> = Arc::new(engine);

        let aead: Option<Arc<dyn kcrypt_rs::AeadCrypt>> =
            kcp_crypt::select_aead_crypt(crypt_method, &key[..])
                .map(|a| Arc::from(a));
        let has_aead = aead.is_some();
        let _has_encryption = crypt_method != "null" && !has_aead;

        // Create KCP instance with output callback that collects raw packets.
        // Same optimization as client: the callback just collects raw KCP
        // data (fast), encryption + UDP send happens after KCP lock release.
        // R2: output callback receives owned `Bytes` (reference-counted slice
        // of the KCP output buffer) — no per-packet `Vec` alloc +
        // `extend_from_slice` copy.
        let raw_packets = Arc::new(parking_lot::Mutex::new(Vec::<bytes::Bytes>::new()));
        let raw_packets_cb = raw_packets.clone();
        let has_encryption = crypt_method != "null";
        let mut kcp = KCP::new(
            conv,
            0,
            Box::new(move |data: bytes::Bytes| {
                raw_packets_cb.lock().push(data);
            }),
        );

        // Configure KCP
        kcp.set_mtu(mtu);
        kcp.set_snd_wnd(sndwnd);
        kcp.set_rcv_wnd(rcvwnd);
        if datashard > 0 || parityshard > 0 {
            kcp.set_fec(datashard, parityshard);
        }
        kcp.set_stream_mode(true);

        // Apply mode profile or explicit parameters
        if !mode.is_empty() {
            apply_mode(&mut kcp, mode);
        } else {
            let n = if nodelay > 0 { nodelay } else { 0 };
            let i = if interval >= 10 { interval } else { 40 };
            kcp.set_nodelay(n, i, resend, nc);
        }

        let kcp = Arc::new(parking_lot::Mutex::new(kcp));

        // Create SMUX server config
        let smux_cfg = smux_rs::Config {
            version: smuxver,
            max_receive_buffer: smuxbuf,
            max_stream_buffer: streambuf,
            max_frame_size: framesize,
            keepalive_interval: keepalive,
            keepalive_timeout: 30,
        };
        let smux = match smux_rs::Session::new_server(&smux_cfg) {
            Ok(s) => Arc::new(s),
            Err(e) => {
                error!("failed to create SMUX server session: {:?}", e);
                // Return a placeholder that will fail gracefully
                let default_cfg = smux_rs::DEFAULT_CONFIG.clone();
                Arc::new(smux_rs::Session::new_server(&default_cfg).unwrap())
            }
        };

        let handled_streams = Arc::new(parking_lot::Mutex::new(HashSet::new()));

        // Create FEC decoder if FEC parameters are set
        let fec_decoder = if datashard > 0 && parityshard > 0 {
            FecDecoder::new(datashard as usize, parityshard as usize).map(parking_lot::Mutex::new)
        } else {
            None
        };

        let mut session = KcpServerSession {
            kcp,
            crypt: crypt_state,
            aead,
            smux,
            handled_streams,
            peer,
            _handles: Vec::new(),
            nocomp,
            raw_packets,
            snappy_fallback: if nocomp {
                None
            } else {
                Some(parking_lot::Mutex::new(SnappyStreamDecoder::new()))
            },
            udp: udp.clone(),
            has_encryption,
            ack_nodelay: acknodelay,
            fec_decoder,
            compressor: if nocomp {
                None
            } else {
                Some(Arc::new(parking_lot::Mutex::new(
                    snap::write::FrameEncoder::new(Vec::new()),
                )))
            },
            crypto_buf: Arc::new(parking_lot::Mutex::new(kcp_rs::CryptoBuf::new(conv as u64))),
            flush_notify: Arc::new(kio::Notify::new()),
        };

        session.start_flush_loop();
        session
    }

    /// Start the background KCP update/flush loop for this session.
    ///
    /// Event-driven flush loop (notify + next_update, max KCP_UPDATE_INTERVAL_MS) and:
    /// 1. Drains all SMUX streams' send buffers into SMUX Data frames
    /// 2. Sends the frames through KCP
    /// 3. Advances the KCP timer (update + flush)
    fn start_flush_loop(&mut self) {
        let kcp = self.kcp.clone();
        let smux = self.smux.clone();
        let _nocomp = self.nocomp;
        let raw_packets = self.raw_packets.clone();
        let compressor = self.compressor.clone();
        let smuxver = self.smux.version();
        let udp = self.udp.clone();
        let crypt = self.crypt.clone();
        let aead = self.aead.clone();
        let peer = self.peer;
        let has_encryption = self.has_encryption;
        let has_aead = aead.is_some();
        let handled_streams = self.handled_streams.clone();
        let crypto_buf = self.crypto_buf.clone();
        let flush_notify = self.flush_notify.clone();

        let h = kio::spawn_task(async move {
            let mut next_update: u64 = KCP_UPDATE_INTERVAL_MS;
            // Reused across iterations: single buffer for SMUX frame assembly (P0.3).
            let mut out_buf = BytesMut::with_capacity(64 * 1024);

            loop {
                // Wait for either the dynamic interval (nearest RTO or
                // default) or an immediate notify from SMUX stream writes.
                // notify_one() stores a permit, so there's no lost-wakeup.
                let _ = kio::timeout(
                    Duration::from_millis(next_update),
                    flush_notify.notified(),
                )
                .await;

                // ── Phase 1: Drain SMUX + encode frames into out_buf (NO KCP lock) ──
                // Header reserved, payload drained in place, length patched (P0.3).
                // Wrapped so stream MutexGuard is dropped before any .await.
                out_buf.clear();
                let fin_streams = {
                    let streams = smux.streams();
                    let stream_map = streams.lock();
                    // Drain ALL pending SMUX bytes (multiple frames per stream).
                    const MAX_DRAIN_BYTES: usize = 64 * 1024;
                    let mut drained_total = 0usize;
                    'outer: for (id, s) in stream_map.iter() {
                        loop {
                            if drained_total >= MAX_DRAIN_BYTES {
                                break 'outer;
                            }
                            let header_pos = out_buf.len();
                            smux_rs::frame::Frame::encode_header_into(
                                &mut out_buf,
                                smuxver,
                                smux_rs::frame::Cmd::Psh,
                                *id,
                                0,
                            );
                            let n = s.drain_send_max(
                                &mut out_buf,
                                smux_rs::frame::MAX_FRAME_SIZE,
                            );
                            if n == 0 {
                                out_buf.truncate(header_pos);
                                break;
                            }
                            smux_rs::frame::Frame::patch_header_length(
                                &mut out_buf,
                                header_pos,
                                n as u16,
                            );
                            drained_total += n;
                        }
                    }

                    // Collect streams that need FIN frames:
                    // - local_closed = true (we called poll_shutdown)
                    // - pending_send == 0 (all data already drained)
                    // - fin_sent == false (haven't sent FIN yet)
                    // Note: do NOT check is_remote_closed() — we must send FIN
                    // even if the remote already closed, to properly signal
                    // half-close to the peer.
                    let fin_streams: Vec<u32> = stream_map
                        .iter()
                        .filter(|(_, s)| {
                            s.is_local_closed() && s.pending_send() == 0 && !s.is_fin_sent()
                        })
                        .map(|(id, s)| {
                            s.mark_fin_sent();
                            *id
                        })
                        .collect();
                    fin_streams
                };

                // ── Phase 1a: Clean up fully closed streams (prevent FD leak) ──
                {
                    let streams = smux.streams();
                    let mut stream_map = streams.lock();
                    let to_remove: Vec<u32> = stream_map
                        .iter()
                        .filter(|(_, s)| {
                            s.is_local_closed() && s.is_remote_closed() && s.is_fin_sent()
                        })
                        .map(|(id, _)| *id)
                        .collect();
                    for id in &to_remove {
                        stream_map.remove(id);
                    }
                    if !to_remove.is_empty() {
                        debug!("SMUX: cleaned up {} closed streams", to_remove.len());
                        // Also clean up handled_streams to prevent unbounded growth
                        let mut handled = handled_streams.lock();
                        for id in &to_remove {
                            handled.remove(id);
                        }
                    }
                    drop(stream_map);
                }

                // Encode FIN frames into out_buf
                for &stream_id in &fin_streams {
                    debug!("flush: sending FIN for stream {}", stream_id);
                    smux_rs::frame::Frame::encode_header_into(
                        &mut out_buf,
                        smuxver,
                        smux_rs::frame::Cmd::Fin,
                        stream_id,
                        0,
                    );
                }

                // ── UPD frames (matching Go's sendWindowUpdate) ──
                smux.check_upd();
                for upd in smux.take_upd_frames() {
                    smux_rs::frame::Frame::encode_header_into(
                        &mut out_buf,
                        smuxver,
                        smux_rs::frame::Cmd::Upd,
                        upd.stream_id,
                        8,
                    );
                    out_buf.extend_from_slice(&upd.consumed.to_le_bytes());
                    out_buf.extend_from_slice(&upd.window.to_le_bytes());
                }

                // ── Phase 3: Snappy compress (NO KCP lock held) ──
                let send_data = if out_buf.is_empty() {
                    None
                } else {
                    let to_send = if !_nocomp {
                        // Use persistent FrameEncoder (CRC32C/Castagnoli) matching Go's golang/snappy
                        use std::io::Write;
                        let mut enc = compressor.as_ref().map(|c| c.lock()).unwrap();
                        enc.write_all(&out_buf).ok();
                        enc.flush().ok();
                        std::mem::take(enc.get_mut())
                    } else {
                        out_buf.split().to_vec()
                    };
                    if to_send.is_empty() {
                        None
                    } else {
                        Some(to_send)
                    }
                };

                // ── Phase 4: Send via KCP + update + flush (KCP lock held briefly) ──
                // Wrapped in a block so the MutexGuard is dropped before any
                // .await point (spawn_blocking below) — MutexGuard is !Send.
                {
                    let mut kcp_guard = kcp.lock();
                    let had_outbound = send_data.is_some();
                    if let Some(data) = send_data {
                        let mss = kcp_guard.mss() as usize;
                        let max_chunk = (kcp_rs::segment::KCP_MAX_FRAG as usize)
                            .saturating_sub(1)
                            .saturating_mul(mss)
                            .max(mss);
                        let mut offset = 0;
                        let mut total_sent = 0usize;
                        while offset < data.len() {
                            let end = (offset + max_chunk).min(data.len());
                            if let Err(e) = kcp_guard.send(&data[offset..end]) {
                                warn!(
                                    "[flush] KCP send error at offset {}/{}: {:?}",
                                    offset,
                                    data.len(),
                                    e
                                );
                                break;
                            }
                            total_sent += end - offset;
                            offset = end;
                        }
                        // Only log when there's backpressure
                        let ws = kcp_guard.wait_send();
                        if ws > 0 {
                            debug!(
                                "[flush] sent {} bytes, wait_send={}, snd_buf={}, snd_queue={}",
                                total_sent,
                                ws,
                                kcp_guard.snd_buf_len(),
                                kcp_guard.snd_queue_len()
                            );
                        }
                    }

                    // Call flush() directly (matching Go's UDPSession.update()
                    // which calls s.kcp.flush() directly, NOT the deprecated
                    // Update() that throttles via ts_flush). This avoids
                    // double-flushing (update() internally calls flush() too).
                    // The return value gives ms until the next meaningful event.
                    next_update = kcp_guard.flush() as u64;
                    // P2.2: pending send or in-flight → 1ms; idle → clamp to max.
                    if had_outbound || kcp_guard.wait_send() > 0 {
                        next_update = 1;
                    } else {
                        next_update = next_update.clamp(1, KCP_UPDATE_INTERVAL_MS);
                    }
                }

                // Batch-encrypt raw KCP packets. Offload to cpu_block only when the
                // batch is large enough that thread-pool scheduling tax is
                // amortized (P0.2). Small batches encrypt inline on this task.
                let packets: Vec<bytes::Bytes> = std::mem::take(&mut *raw_packets.lock());
                if packets.is_empty() {
                    kcp_rs::DEFAULT_SNMP.empty_flush.fetch_add(1, Ordering::Relaxed);
                }
                if !packets.is_empty() {
                    let total_bytes: usize = packets.iter().map(|p| p.len()).sum();
                    let use_cpu_block = kcp_rs::should_cpu_block_encrypt(
                        has_encryption,
                        has_aead,
                        packets.len(),
                        total_bytes,
                    );

                    let crypt_sb = crypt.clone();
                    let crypto_buf_sb = crypto_buf.clone();
                    let aead_sb = aead.clone();
                    let encrypt_fn = move || {
                        kcp_rs::encrypt_batch(
                            packets,
                            crypt_sb.as_ref(),
                            &crypto_buf_sb,
                            aead_sb.as_deref(),
                            has_encryption,
                        )
                    };

                    let encrypted: Vec<bytes::Bytes> = if use_cpu_block {
                        kio::cpu_block(encrypt_fn).await
                    } else {
                        encrypt_fn()
                    };

                    if let Err(e) = udp.send_batch_to(&encrypted, peer).await {
                        warn!("UDP send_to error ({}): {}", peer, e);
                    }
                }

                // If SMUX still has buffered data *and* peer window allows more
                // send, wake immediately. When peer_send_window==0 we must NOT
                // busy-spin — wait for an UPD (UDP path notifies flush).
                {
                    let streams = smux.streams();
                    let stream_map = streams.lock();
                    let still_pending = stream_map.values().any(|s| {
                        s.pending_send() > 0 && s.peer_send_window() > 0
                    });
                    drop(stream_map);
                    if still_pending {
                        next_update = 1;
                        flush_notify.notify_one();
                    }
                }
            }
        });
        self._handles.push(h);
    }

    /// Feed incoming encrypted data from the UDP socket into this session.
    ///
    /// This is the core data path:
    /// 1. Decrypt the payload using the block cipher
    /// 2. Verify CRC32 checksum (matching Go kcp-go v5 behavior)
    /// 3. Strip the 20-byte crypto header [nonce 16B][CRC32 4B]
    /// 4. Detect and handle FEC header (if present) — feed through FecDecoder
    ///    for erasure recovery
    /// 5. Feed the KCP segment to the KCP state machine
    /// 6. Extract all reassembled user messages via KCP recv
    ///
    /// Go kcp-go v5 wire format:
    ///   No FEC:  [nonce 16B][CRC32 4B][KCP segment 24+ bytes]
    ///   With FEC: [nonce 16B][CRC32 4B][FEC seq 4B][FEC type 2B][FEC plen 2B][KCP segment 24+ bytes]
    ///
    /// FEC types (matching Go): 0x00f1 = data, 0x00f2 = parity, 0x00f3 = OOB
    ///
    /// We distinguish header-wrapped packets from raw KCP segments by
    /// checking byte 4. KCP command bytes are 0x51-0x54. If byte 4 is
    /// NOT a valid KCP command, it carries the Go kcp-go v5 header.
    fn feed_data(&self, data: &[u8]) -> Vec<Vec<u8>> {
        // Step 1: Decrypt/authenticate data
        let feed_slice: Vec<u8> = if let Some(ref aead) = self.aead {
            // AEAD path: [nonce 12B][ciphertext+tag 16B] — open decrypts+verifies
            match aead.open(data) {
                Ok(plain) => plain,
                Err(_) => return Vec::new(),
            }
        } else if self.has_encryption {
            let mut dec_data = data.to_vec();
            // CFB path: decrypt in-place, then strip [nonce 16B][CRC32 4B] header
            self.crypt.decrypt(&mut dec_data);
            const CRYPT_HEADER: usize = 20;
            const NONCE_SIZE: usize = 16;

            if dec_data.len() > CRYPT_HEADER {
                let cmd = dec_data[4];
                let has_header = cmd != 0x51 && cmd != 0x52 && cmd != 0x53 && cmd != 0x54;
                if has_header {
                    let stored_crc =
                        u32::from_le_bytes(dec_data[NONCE_SIZE..CRYPT_HEADER].try_into().unwrap());
                    let computed_crc = crc32fast::hash(&dec_data[CRYPT_HEADER..]);
                    if stored_crc != computed_crc {
                        return Vec::new();
                    }
                    // Drop header in place (one allocation already owned by dec_data).
                    dec_data.drain(..CRYPT_HEADER);
                    dec_data
                } else {
                    dec_data
                }
            } else {
                dec_data
            }
        } else {
            // null cipher: no crypto header, raw data (may include FEC header)
            data.to_vec()
        };

        // Step 3: Handle FEC if decoder is present, then feed to KCP.
        // Feed incoming FEC packets to the FecDecoder for erasure recovery,
        // then feed the KCP segments (from data packets and recovered data)
        // to the KCP state machine.
        let mut all_results = Vec::new();
        if let Some(ref fec) = self.fec_decoder {
            const FEC_HDR: usize = 8; // fecHeaderSizePlus2
            let mut fec_dec = fec.lock();

            // Feed data and parity packets to FEC decoder for recovery
            let recovered = if feed_slice.len() >= FEC_HDR {
                fec_dec.decode(&feed_slice)
            } else {
                Vec::new()
            };

            // Prepare KCP input data: always feed data packets directly;
            // also feed any recovered packets.
            if feed_slice.len() >= FEC_HDR + 24 {
                let fec_type = u16::from_le_bytes(feed_slice[4..6].try_into().unwrap());
                let mut kcp_inputs: Vec<Vec<u8>> = Vec::with_capacity(1 + recovered.len());
                match fec_type {
                    0x00f1 => {
                        // FEC data packet: feed KCP data directly
                        kcp_inputs.push(feed_slice[FEC_HDR..].to_vec());
                        // Also add recovered data (strip FEC header)
                        for r in &recovered {
                            if r.len() > FEC_HDR {
                                kcp_inputs.push(r[FEC_HDR..].to_vec());
                            }
                        }
                    }
                    0x00f2 => {
                        // Parity: only feed recovered data to KCP
                        for r in &recovered {
                            if r.len() > FEC_HDR {
                                kcp_inputs.push(r[FEC_HDR..].to_vec());
                            }
                        }
                    }
                    0x00f3 => {
                        // OOB (Out-of-Band): log at trace level, pass to OOB handler
                        log::trace!("OOB packet received: {} bytes", feed_slice.len());
                        // Feed any recovered data from OOB packets
                        for r in &recovered {
                            if r.len() > FEC_HDR {
                                kcp_inputs.push(r[FEC_HDR..].to_vec());
                            }
                        }
                    }
                    _ => {
                        kcp_inputs.push(feed_slice.to_vec());
                    }
                }
                // Feed raw KCP segments to KCP input (NO decompression here).
                // Compression wraps the SMUX session data, which is sent via KCP.
                // KCP fragments the compressed data into segments; individual
                // segments are NOT valid Snappy framing data and cannot be
                // decompressed before KCP reassembly.
                // Decompression happens AFTER kcp.recv() returns reassembled data,
                // matching Go's architecture where CompStream.Read() calls
                // kcp.Recv() first, then snappy.Reader decompresses.
                for slice in &kcp_inputs {
                    let mut kcp = self.kcp.lock();
                    kcp.input(slice, self.ack_nodelay).ok();
                    drop(kcp);
                    let mut kcp = self.kcp.lock();
                    while let Ok(data) = kcp.recv_bytes() {
                        if !self.nocomp {
                            if let Some(ref fb) = self.snappy_fallback {
                                if let Ok(decompressed) = fb.lock().feed(&data) {
                                    if !decompressed.is_empty() {
                                        all_results.push(decompressed);
                                    }
                                }
                            }
                        } else {
                            all_results.push(data.to_vec());
                        }
                    }
                }
            } else {
                let mut kcp = self.kcp.lock();
                kcp.input(&feed_slice, self.ack_nodelay).ok();
                drop(kcp);
                let mut kcp = self.kcp.lock();
                while let Ok(data) = kcp.recv_bytes() {
                    if !self.nocomp {
                        if let Some(ref fb) = self.snappy_fallback {
                            if let Ok(decompressed) = fb.lock().feed(&data) {
                                if !decompressed.is_empty() {
                                    all_results.push(decompressed);
                                }
                            }
                        }
                    } else {
                        all_results.push(data.to_vec());
                    }
                }
            }
        } else {
            let mut kcp = self.kcp.lock();
            let conv = kcp.conv();
            let input_result = kcp.input(&feed_slice, self.ack_nodelay);
            debug!(
                "feed_data: KCP input result = {:?}, conv=0x{:08x}",
                input_result, conv
            );
            drop(kcp);
            let mut kcp = self.kcp.lock();
            while let Ok(data) = kcp.recv_bytes() {
                debug!("feed_data: KCP recv {} bytes", data.len());
                if !self.nocomp {
                    if let Some(ref fb) = self.snappy_fallback {
                        if let Ok(decompressed) = fb.lock().feed(&data) {
                            if !decompressed.is_empty() {
                                all_results.push(decompressed);
                            }
                        }
                    }
                } else {
                    all_results.push(data.to_vec());
                }
            }
        }

        // ── Wake flush loop to send ACKs immediately ──
        // kcp.input() (with ack_nodelay) generates ACK packets into
        // raw_packets via the KCP output callback. Rather than spawning a
        // fire-and-forget task per packet (spawn_task + cpu_block), which
        // creates thousands of micro-tasks/sec under load and overwhelms the
        // tokio runtime, we notify the flush loop. It wakes immediately,
        // drains raw_packets, encrypts in a single batch, and sends via UDP.
        if !self.raw_packets.lock().is_empty() {
            self.flush_notify.notify_one();
        }

        all_results
    }

    /// Check for newly accepted SMUX streams that need TCP handler tasks.
    ///
    /// Returns a list of (stream_id, Arc<Stream>) pairs for streams that
    /// were accepted by the SMUX session but have not yet been dispatched.
    fn drain_new_streams(&self) -> Vec<(u32, Arc<smux_rs::stream::Stream>)> {
        let handled = self.handled_streams.lock().clone();
        let streams = self.smux.streams();
        let stream_map = streams.lock();
        let new_streams: Vec<(u32, Arc<smux_rs::stream::Stream>)> = stream_map
            .iter()
            .filter(|(&id, s)| {
                if handled.contains(&id) {
                    return false;
                }
                // Accept streams that are ready (SYN received) OR have data buffered.
                // A FIN might arrive before the server reads the data, so we must
                // also accept streams with pending data even if state is FinReceived.
                s.is_ready() || s.available() > 0
            })
            .map(|(&id, s)| (id, s.clone()))
            .collect();
        drop(stream_map);

        // Mark as handled
        {
            let mut h = self.handled_streams.lock();
            for (id, _) in &new_streams {
                h.insert(*id);
            }
        }

        new_streams
    }
}

// ─── SNMP Logger ────────────────────────────────────────────────────────────────

/// Periodically log KCP SNMP statistics to a CSV file.
async fn snmp_logger(path: String, period: Duration, stop: Arc<AtomicBool>) {
    kio::sleep_ms(period.as_millis() as u64).await;

    let file = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        Ok(f) => f,
        Err(e) => {
            error!("Failed to open SNMP log file '{}': {}", path, e);
            return;
        }
    };
    let mut writer = std::io::BufWriter::new(file);

    // Write CSV header
    let headers = kcp_rs::SNMP::header();
    if let Err(e) = writeln!(writer, "timestamp,{}", headers.join(",")) {
        error!("SNMP log write error: {}", e);
        return;
    }

    while !stop.load(Ordering::Relaxed) {
        kio::sleep_ms(period.as_millis() as u64).await;
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let snmp = kcp_rs::SNMP::new();
        let values = snmp.to_slice();
        if let Err(e) = writeln!(writer, "{},{}", ts, values.join(",")) {
            error!("SNMP log write error: {}", e);
        }
        if let Err(e) = writer.flush() {
            error!("SNMP log flush error: {}", e);
        }
    }
}

// ─── Main ───────────────────────────────────────────────────────────────────────

/// Get or create a KCP server session for a peer connection.
/// Extracted to avoid block-in-let parsing issues.
#[allow(clippy::too_many_arguments)]
fn get_or_create_session(
    sessions: &Arc<DashMap<SocketAddr, Arc<KcpServerSession>>>,
    peer: &SocketAddr,
    buf: &[u8],
    datashard: u32,
    parityshard: u32,
    crypt_method: &str,
    key_arr: &[u8; 32],
    udp: &Arc<kio::UdpSocket>,
    mode: &str,
    mtu: u32,
    sndwnd: u32,
    rcvwnd: u32,
    acknodelay: bool,
    nodelay: u32,
    interval: u32,
    resend: u32,
    nc: u32,
    smuxver: u8,
    smuxbuf: usize,
    streambuf: usize,
    framesize: usize,
    keepalive: u64,
    nocomp: bool,
) -> Arc<KcpServerSession> {
    // Step 1: Try to get an existing session WITHOUT locking (DashMap shard read)
    if let Some(s) = sessions.get(peer) {
        return s.clone();
    }

    // Step 2: Extract conv OUTSIDE any lock — this involves decryption
    // which is expensive and must not block other sessions.
    let fec_offset = if datashard > 0 || parityshard > 0 {
        8
    } else {
        0
    };
    let _conv_offset = 20 + fec_offset;
    let conv = if buf.len() >= 12 && crypt_method == "aes-128-gcm" {
        // AEAD: open the first packet to extract conv
        if let Some(aead) = kcp_crypt::select_aead_crypt(crypt_method, &key_arr[..]) {
            match aead.open(buf) {
                Ok(plain) if plain.len() >= 4 => {
                    // Check for FEC header (matching Go's kcpInput logic)
                    if plain.len() >= 6 {
                        let fec_flag = u16::from_le_bytes([plain[4], plain[5]]);
                        if fec_flag == 0x00f1 || fec_flag == 0x00f2 || fec_flag == 0x00f3 {
                            // FEC header present: conv is after 8-byte FEC header
                            if plain.len() >= 12 {
                                u32::from_le_bytes([plain[8], plain[9], plain[10], plain[11]])
                            } else {
                                0xDEADBEEF
                            }
                        } else {
                            u32::from_le_bytes([plain[0], plain[1], plain[2], plain[3]])
                        }
                    } else {
                        u32::from_le_bytes([plain[0], plain[1], plain[2], plain[3]])
                    }
                }
                _ => 0xDEADBEEF,
            }
        } else {
            0xDEADBEEF
        }
    } else if buf.len() >= 32 + fec_offset && crypt_method != "null" {
        // Go approach: decrypt first, then check FEC flag at data[4..6]
        // to determine conv offset (matching kcp-go's packetInput)
        let mut hdr = buf[..(32 + fec_offset).min(buf.len())].to_vec();
        let (block_crypt, _) = kcp_crypt::select_block_crypt(crypt_method, &key_arr[..]);
        block_crypt.decrypt(&mut hdr);
        // After decrypt: [nonce 16][CRC4][payload], strip nonce+CRC
        let payload = &hdr[20..];
        // Extract conv directly from the KCP segment header.
        // The KCP segment header (including conv) is NOT compressed — only
        // the KCP segment's DATA payload is compressed (and that's decompressed
        // later by SnappyStreamDecoder after kcp.recv()).
        // This matches Go's kcp-go Listener.packetInput which reads conv
        // directly from the KCP segment without any decompression.
        let flag = u16::from_le_bytes([payload[4], payload[5]]);
        let off = if flag == 0x00f1 || flag == 0x00f2 || flag == 0x00f3 {
            8
        } else {
            0
        };
        let conv_val = u32::from_le_bytes(payload[off..off + 4].try_into().unwrap());
        debug!(
            "get_or_create_session: extracted conv=0x{:08x}, nocomp={}",
            conv_val, nocomp
        );
        conv_val
    } else if buf.len() >= 4 {
        // null cipher: no crypto header. Check for FEC header to find conv.
        if buf.len() >= 6 {
            let fec_flag = u16::from_le_bytes([buf[4], buf[5]]);
            if fec_flag == 0x00f1 || fec_flag == 0x00f2 || fec_flag == 0x00f3 {
                // FEC header present: conv is after 8-byte FEC header
                if buf.len() >= 12 {
                    u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]])
                } else {
                    0xDEADBEEF
                }
            } else {
                u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]])
            }
        } else {
            u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]])
        }
    } else {
        0xDEADBEEF
    };
    info!("new KCP session from {} (conv=0x{:08x})", peer, conv);
    let session = Arc::new(KcpServerSession::new(
        conv,
        *peer,
        udp,
        key_arr,
        crypt_method,
        mode,
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
        keepalive,
        nocomp,
    ));
    // Insert with entry API — only locks one shard
    // If another thread inserted a session for this peer while we were
    // creating one, use the existing one and drop ours.
    match sessions.entry(*peer) {
        dashmap::mapref::entry::Entry::Occupied(e) => e.get().clone(),
        dashmap::mapref::entry::Entry::Vacant(e) => {
            let s = session.clone();
            e.insert(session);
            s
        }
    }
}

fn main() -> Result<()> {
    kio::block_on(async_main())
}

async fn async_main() -> Result<()> {
    let cli = Cli::parse();
    if cli.version_flag {
        println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    // Load config file if specified
    let cli = if let Some(ref config_path) = cli.c {
        let config_str = kio::read_to_string(config_path.clone()).await?;
        let cfg: Config = serde_json::from_str(&config_str)?;
        Cli::merge(cli, cfg)
    } else {
        cli
    };

    // Set up logging: redirect to file if --log is specified
    if let Some(ref log_path) = cli.log {
        // Rotate log file if it exceeds 10MB
        rotate_log(log_path, 10 * 1024 * 1024);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)?;
        // Logging: controlled by RUST_LOG env var, defaults to "info".
        // Use RUST_LOG=debug for debug output.
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
            .format_timestamp_secs()
            .target(env_logger::Target::Pipe(Box::new(file)))
            .init();
    } else {
        // Logging: controlled by RUST_LOG env var, defaults to "info".
        // Use RUST_LOG=debug for debug output, RUST_LOG=trace for everything.
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
            .format_timestamp_secs()
            .init();
        info!(
            "log level: {} (set RUST_LOG=debug for verbose output)",
            std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into())
        );
    }

    let listen = cli.listen.as_deref().unwrap_or(":29900");
    let target = cli
        .target
        .as_deref()
        .context("target address (-t) is required")?;

    let key_str = cli.key.as_deref().unwrap();
    let crypt_method = cli.crypt.as_deref().unwrap();
    let mode = cli.mode.as_deref().unwrap();
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
    let interval = cli.interval.unwrap_or(30);
    let resend = cli.resend.unwrap_or(2);
    let nc = cli.nc.unwrap_or(1);
    let smuxver = cli.smuxver.unwrap_or(2);
    let smuxbuf = cli.smuxbuf.unwrap_or(4 * 1024 * 1024);
    let streambuf = cli.streambuf;
    let framesize = cli.framesize;
    let keepalive = cli.keepalive.unwrap_or(10);
    let ratelimit_val = cli.ratelimit;
    let close_wait_val = cli.closewait.unwrap_or(30);
    let quiet = cli.quiet;
    let qpp_enabled = cli.qpp;
    let qpp_count = cli.qppcount.unwrap_or(61);

    // Derive encryption key
    let key = derive_key(key_str);
    info!(
        "key derived: crypt={}, key={:02x}..{:02x}",
        crypt_method, key[0], key[31]
    );

    // Bind UDP listener
    let listen_addr: SocketAddr = parse_addr(listen).context("invalid listen address")?;
    let udp = {
        let socket = socket2::Socket::new(
            if listen_addr.is_ipv4() {
                socket2::Domain::IPV4
            } else {
                socket2::Domain::IPV6
            },
            socket2::Type::DGRAM,
            None,
        )?;

        // Apply socket buffer sizes
        if let Err(e) = socket.set_recv_buffer_size(sockbuf as usize) {
            warn!("set_recv_buffer_size failed: {}", e);
        }
        if let Err(e) = socket.set_send_buffer_size(sockbuf as usize) {
            warn!("set_send_buffer_size failed: {}", e);
        }

        // Apply DSCP
        if dscp_val > 0 {
            let dscp_shifted = dscp_val << 2; // DSCP is 6 bits, shift to DS field
            if let Err(e) = socket.set_tos(dscp_shifted as u32) {
                warn!("set_tos (DSCP) failed: {}", e);
            }
        }

        socket.bind(&listen_addr.into())?;
        socket.set_nonblocking(true)?;
        kio::UdpSocket::from_std(socket.into())?
    };
    let udp = Arc::new(udp);
    info!("listening on {} for KCP connections", listen_addr);
    info!("forwarding to TCP target {}", target);
    if ratelimit_val > 0 {
        info!("ratelimit: {} bytes/sec", ratelimit_val);
    }
    if dscp_val > 0 {
        info!("dscp: {}", dscp_val);
    }
    info!("sockbuf: {}", sockbuf);

    // Session map: peer address -> KcpServerSession
    let sessions: Arc<DashMap<SocketAddr, Arc<KcpServerSession>>> = Arc::new(DashMap::new());

    // Start SNMP logger if configured
    let stop_flag = Arc::new(AtomicBool::new(false));
    if let Some(ref snmplog_path) = cli.snmplog {
        let period = Duration::from_secs(cli.snmpperiod.unwrap_or(60));
        let s = stop_flag.clone();
        let p = snmplog_path.clone();
        kio::spawn_task(async move {
            snmp_logger(p, period, s).await;
        });
    }

    // Start pprof if configured (requires --features pprof)
    #[cfg(feature = "pprof")]
    if let Some(ref pprof_addr) = cli.pprof {
        info!("starting pprof HTTP server on {}", pprof_addr);
        let pprof_stop = stop_flag.clone();
        let addr = pprof_addr.clone();
        kio::spawn_task(async move {
            if let Err(e) = run_pprof(&addr, pprof_stop).await {
                error!("pprof server error: {}", e);
            }
        });
    }
    #[cfg(not(feature = "pprof"))]
    if cli.pprof.is_some() {
        log::warn!("--pprof requested but binary built without `pprof` feature; rebuild with --features pprof");
    }

    // Prepare per-connection parameters for session creation
    let target_str = target.to_string();
    let key_arr = key;

    // Spawn Ctrl-C handler (runtime-agnostic)
    {
        let stop = stop_flag.clone();
        kio::spawn_task(async move {
            let _ = kio::ctrl_c().await;
            stop.store(true, Ordering::Relaxed);
        });
    }

    // ── Main UDP recv loop ──
    // First packet via async recv_from; then drain ready packets with
    // try_recv_batch_from (Linux: recvmmsg) before awaiting again (P1.3).
    let mut buf = vec![0u8; MAX_DATAGRAM];
    let mut batch_slots: Vec<Vec<u8>> =
        (0..16).map(|_| Vec::with_capacity(MAX_DATAGRAM)).collect();
    let mut batch_extra: Vec<(Vec<u8>, SocketAddr)> = Vec::with_capacity(16);

    // Local helper: process one encrypted datagram for a peer.
    let process_datagram = |peer: SocketAddr, data: &[u8]| {
        let session = get_or_create_session(
            &sessions,
            &peer,
            data,
            datashard,
            parityshard,
            crypt_method,
            &key_arr,
            &udp,
            mode,
            mtu,
            sndwnd,
            rcvwnd,
            acknodelay,
            nodelay,
            interval,
            resend,
            nc,
            smuxver,
            smuxbuf,
            streambuf,
            framesize,
            keepalive,
            nocomp,
        );
        let new_streams = {
            let messages = session.feed_data(data);
            if !messages.is_empty() {
                for msg in &messages {
                    debug!("SMUX process_data: {} bytes", msg.len());
                    if let Err(e) = session.smux.process_data(msg) {
                        debug!("SMUX process_data error: {:?}", e);
                    }
                }
            }
            // Peer UDP may open send window — wake flush promptly.
            session.flush_notify.notify_one();
            session.drain_new_streams()
        };
        for (stream_id, smux_stream) in new_streams {
            if !quiet {
                info!(
                    "accepting stream {} from {} -> target {}",
                    stream_id, peer, target_str
                );
            }
            let target = target_str.clone();
            let qpp_key = key_arr.to_vec();
            let qp = quiet;
            let cw = close_wait_val;
            let fn_notify = session.flush_notify.clone();
            kio::spawn_task(async move {
                if let Err(e) = handle_stream(
                    target,
                    smux_stream,
                    stream_id,
                    qpp_enabled,
                    qpp_key,
                    qpp_count,
                    qp,
                    cw,
                    fn_notify,
                )
                .await
                {
                    error!("stream {} handler error: {:?}", stream_id, e);
                }
                if !qp {
                    info!("stream {} closed", stream_id);
                }
            });
        }
    };

    loop {
        if stop_flag.load(Ordering::Relaxed) {
            info!("received Ctrl+C, shutting down...");
            break;
        }

        match kio::timeout(Duration::from_millis(500), udp.recv_from(&mut buf)).await {
            Ok(Ok((n, peer))) => {
                if n == 0 {
                    continue;
                }
                process_datagram(peer, &buf[..n]);
                // Drain any further ready datagrams without yielding.
                batch_extra.clear();
                match udp.try_recv_batch_from(&mut batch_slots, &mut batch_extra) {
                    Ok(count) if count > 0 => {
                        for (pkt, peer) in batch_extra.drain(..) {
                            process_datagram(peer, &pkt);
                        }
                    }
                    Ok(_) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(e) => {
                        error!("UDP try_recv_batch_from error: {}", e);
                    }
                }
            }
            Ok(Err(e)) => {
                error!("UDP recv_from error: {}", e);
                continue;
            }
            Err(_) => continue, // timeout, loop back to check stop_flag
        }
    }

    // Graceful shutdown
    info!("shutting down...");
    kio::sleep(Duration::from_secs(1)).await;
    info!("bye");

    Ok(())
}

#[cfg(feature = "pprof")]
/// HTTP pprof server compatible with `go tool pprof`.
///
/// Endpoints (same shape as Go `net/http/pprof`):
///   GET /debug/pprof/                      — short index
///   GET /debug/pprof/profile?seconds=N     — CPU profile as **protobuf** (default 30s)
///
/// Examples:
///   ./target/profiling/kcptun-server -l :29900 -t 127.0.0.1:8080 --pprof 127.0.0.1:6060
///   go tool pprof -http=:0 'http://127.0.0.1:6060/debug/pprof/profile?seconds=20'
///   curl -o cpu.pb 'http://127.0.0.1:6060/debug/pprof/profile?seconds=20'
///   go tool pprof -http=:0 cpu.pb
async fn run_pprof(addr: &str, stop: Arc<AtomicBool>) -> Result<()> {
    use kio::AsyncReadExt;
    use kio::AsyncWriteExt;
    let socket_addr: SocketAddr = addr.parse().context("invalid pprof address")?;
    let listener = kio::TcpListener::bind(socket_addr).await?;
    info!("pprof listening on http://{}/debug/pprof/", socket_addr);

    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let accepted = kio::timeout(Duration::from_millis(500), listener.accept()).await;
        let (mut stream, peer) = match accepted {
            Ok(Ok(v)) => v,
            _ => continue,
        };

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
        let first_line = req.lines().next().unwrap_or("");
        let path_q = first_line.split_whitespace().nth(1).unwrap_or("/");
        let (path, query) = match path_q.split_once('?') {
            Some((p, q)) => (p, q),
            None => (path_q, ""),
        };

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

        if path == "/debug/pprof/" || path == "/debug/pprof" {
            let body = concat!(
                "kcptun-rs pprof (Go protobuf format)\n",
                "  GET /debug/pprof/profile?seconds=30\n",
                "Use: go tool pprof -http=:0 http://ADDR/debug/pprof/profile?seconds=20\n",
            );
            respond(
                &mut stream,
                "200 OK",
                "text/plain; charset=utf-8",
                body.as_bytes(),
            )
            .await;
            continue;
        }

        if path == "/debug/pprof/profile" {
            let mut seconds: u64 = 30;
            for part in query.split('&') {
                if let Some(v) = part.strip_prefix("seconds=") {
                    if let Ok(n) = v.parse::<u64>() {
                        seconds = n.clamp(1, 300);
                    }
                }
            }
            info!("pprof CPU profile {}s peer={}", seconds, peer);

            // ProfilerGuard / ReportBuilder are !Send; sample on a blocking thread
            // so the async task remains Send for kio::spawn_task.
            let profile_result = kio::cpu_block(move || -> std::result::Result<Vec<u8>, String> {
                use pprof::protos::Message;
                // blocklist() is only available on arches with findshlibs support
                // (x86_64/aarch64/riscv64/loongarch64) — not on armv7.
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

            respond(
                &mut stream,
                "200 OK",
                "application/octet-stream",
                &profile_bytes,
            )
            .await;
            info!(
                "pprof profile complete ({} bytes) peer={}",
                profile_bytes.len(),
                peer
            );
            continue;
        }

        respond(
            &mut stream,
            "404 Not Found",
            "text/plain; charset=utf-8",
            b"not found\ntry GET /debug/pprof/\n",
        )
        .await;
    }
    Ok(())
}

// ─── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;

    #[test]
    fn test_derive_key() {
        let key = derive_key("test-password");
        assert_eq!(key.len(), 32);
        let key2 = derive_key("test-password");
        assert_eq!(key, key2);
    }

    #[test]
    fn test_derive_key_different() {
        let key1 = derive_key("password1");
        let key2 = derive_key("password2");
        assert_ne!(key1, key2);
    }

    #[test]
    fn test_parse_addr() {
        let addr = parse_addr("127.0.0.1:29900").unwrap();
        assert_eq!(addr.port(), 29900);
        assert!(addr.ip().is_loopback());
    }

    #[test]
    fn test_parse_addr_ipv6() {
        let addr = parse_addr("[::1]:29900").unwrap();
        assert_eq!(addr.port(), 29900);
    }

    #[test]
    fn test_parse_addr_invalid() {
        assert!(parse_addr("not-an-address").is_err());
    }

    #[test]
    fn test_apply_mode_normal() {
        let mut kcp = KCP::new(1, 0, Box::new(|_| {}));
        apply_mode(&mut kcp, "normal");
        assert_eq!(kcp.interval(), 40);
    }

    #[test]
    fn test_apply_mode_fast() {
        let mut kcp = KCP::new(1, 0, Box::new(|_| {}));
        apply_mode(&mut kcp, "fast");
        assert_eq!(kcp.interval(), 30);
    }

    #[test]
    fn test_apply_mode_fast3() {
        let mut kcp = KCP::new(1, 0, Box::new(|_| {}));
        apply_mode(&mut kcp, "fast3");
        assert_eq!(kcp.interval(), 10);
    }

    #[test]
    fn test_apply_mode_unknown() {
        let mut kcp = KCP::new(1, 0, Box::new(|_| {}));
        apply_mode(&mut kcp, "unknown");
        // Falls back to "fast" with interval 30
        assert_eq!(kcp.interval(), 30);
    }

    #[test]
    fn test_config_deserialize() {
        let json = r#"{
            "listen": ":29900",
            "target": "127.0.0.1:8080",
            "key": "test-key",
            "crypt": "aes-128",
            "mode": "fast2",
            "mtu": 1350,
            "sndwnd": 1024,
            "rcvwnd": 1024,
            "datashard": 10,
            "parityshard": 3,
            "nocomp": false,
            "smuxver": 2,
            "keepalive": 10
        }"#;
        let cfg: Config = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.listen.as_deref(), Some(":29900"));
        assert_eq!(cfg.target.as_deref(), Some("127.0.0.1:8080"));
        assert_eq!(cfg.mode.as_deref(), Some("fast2"));
        assert_eq!(cfg.smuxver, Some(2));
    }

    #[test]
    fn test_empty_config() {
        let cfg: Config = serde_json::from_str("{}").unwrap();
        assert!(cfg.listen.is_none());
        assert!(cfg.target.is_none());
    }

    #[test]
    fn test_cli_merge() {
        let cli = Cli {
            listen: Some("0.0.0.0:29900".into()),
            target: None,
            key: None,
            crypt: None,
            mode: None,
            ratelimit: 0,
            mtu: None,
            sndwnd: None,
            rcvwnd: None,
            datashard: 10,
            parityshard: 3,
            dscp: None,
            nocomp: false,
            acknodelay: false,
            nodelay: None,
            interval: None,
            resend: None,
            nc: None,
            sockbuf: None,
            smuxver: None,
            smuxbuf: None,
            streambuf: 2097152,
            framesize: 8192,
            keepalive: None,
            closewait: None,
            snmplog: None,
            snmpperiod: None,
            log: None,
            quiet: false,
            tcp: false,
            pprof: None,
            qpp: false,
            qppcount: None,
            c: None,
            version_flag: false,
        };
        let cfg = Config {
            target: Some("127.0.0.1:8080".into()),
            key: Some("cfg-key".into()),
            mtu: Some(1400),
            ..Default::default()
        };
        let merged = Cli::merge(cli, cfg);
        assert_eq!(merged.listen.as_deref(), Some("0.0.0.0:29900"));
        assert_eq!(merged.target.as_deref(), Some("127.0.0.1:8080"));
        assert_eq!(merged.key.as_deref(), Some("cfg-key"));
        assert_eq!(merged.mtu, Some(1400));
    }

    #[test]
    fn test_cli_merge_cli_precedence() {
        let cli = Cli {
            listen: Some("0.0.0.0:29900".into()),
            target: Some("10.0.0.1:8080".into()),
            key: Some("cli-key".into()),
            crypt: None,
            mode: None,
            ratelimit: 0,
            mtu: None,
            sndwnd: None,
            rcvwnd: None,
            datashard: 10,
            parityshard: 3,
            dscp: None,
            nocomp: false,
            acknodelay: false,
            nodelay: None,
            interval: None,
            resend: None,
            nc: None,
            sockbuf: None,
            smuxver: None,
            smuxbuf: None,
            streambuf: 2097152,
            framesize: 8192,
            keepalive: None,
            closewait: None,
            snmplog: None,
            snmpperiod: None,
            log: None,
            quiet: false,
            tcp: false,
            pprof: None,
            qpp: false,
            qppcount: None,
            c: None,
            version_flag: false,
        };
        let cfg = Config {
            target: Some("cfg-target:8080".into()),
            key: Some("cfg-key".into()),
            ..Default::default()
        };
        let merged = Cli::merge(cli, cfg);
        // CLI values take precedence
        assert_eq!(merged.target.as_deref(), Some("10.0.0.1:8080"));
        assert_eq!(merged.key.as_deref(), Some("cli-key"));
    }

    #[test]
    fn test_smux_frame_encode_decode() {
        use smux_rs::{Cmd, Frame};
        let frame = Frame::new(Cmd::Psh, 42, Bytes::from("test data"));
        let mut buf = BytesMut::new();
        frame.encode(&mut buf);
        let (decoded, _) = Frame::decode(&buf).unwrap();
        assert_eq!(decoded.cmd, Cmd::Psh);
        assert_eq!(decoded.stream_id, 42);
        assert_eq!(&decoded.data[..], b"test data");
    }

    #[test]
    fn test_smux_syn_frame() {
        use smux_rs::{Cmd, Frame};
        let frame = Frame::new(Cmd::Syn, 0, Bytes::new());
        let mut buf = BytesMut::new();
        frame.encode(&mut buf);
        let (decoded, _) = Frame::decode(&buf).unwrap();
        assert_eq!(decoded.cmd, Cmd::Syn);
        assert_eq!(decoded.stream_id, 0);
        assert_eq!(buf.len(), 8);
    }

    #[test]
    fn test_smux_fin_frame() {
        use smux_rs::{Cmd, Frame};
        let frame = Frame::new(Cmd::Fin, 2, Bytes::new());
        let mut buf = BytesMut::new();
        frame.encode(&mut buf);
        let (decoded, _) = Frame::decode(&buf).unwrap();
        assert_eq!(decoded.cmd, Cmd::Fin);
        assert_eq!(decoded.stream_id, 2);
    }

    #[test]
    fn test_kcp_roundtrip() {
        // Verify that KCP can send and receive data via the output callback
        let output_data = Arc::new(std::sync::Mutex::new(Vec::new()));
        let out = output_data.clone();

        let mut sender = KCP::new(
            1,
            0,
            Box::new(move |data: bytes::Bytes| {
                out.lock().unwrap().extend_from_slice(&data);
            }),
        );

        sender.set_stream_mode(true);
        sender.send(b"hello kcp server").unwrap();

        // Flush to trigger output callback
        sender.update(100);
        sender.flush();

        // Verify something was emitted
        let sent = output_data.lock().unwrap().clone();
        assert!(!sent.is_empty(), "KCP should have produced output bytes");

        // Feed back into a receiver
        let mut receiver = KCP::new(1, 0, Box::new(|_| {}));
        receiver.set_stream_mode(true);
        receiver.input(&sent, false).unwrap();
        receiver.update(200);

        let recvd = receiver.recv().unwrap();
        assert_eq!(&recvd[..], b"hello kcp server");
    }

    #[test]
    fn test_kcp_fec_config() {
        let mut kcp = KCP::new(1, 0, Box::new(|_| {}));
        kcp.set_fec(10, 3);
        // No assertion for FEC state since it's internal,
        // but we verify the call doesn't panic
    }

    #[test]
    fn test_smux_server_session() {
        let cfg = smux_rs::DEFAULT_CONFIG.clone();
        let session = smux_rs::Session::new_server(&cfg).unwrap();
        assert!(!session.is_closed());
        assert_eq!(session.stream_count(), 0);
    }

    #[test]
    fn test_smux_server_accept_stream() {
        let cfg = smux_rs::DEFAULT_CONFIG.clone();
        let session = smux_rs::Session::new_server(&cfg).unwrap();
        let stream = session.accept_stream(0).unwrap();
        assert_eq!(stream.id(), 0);
        assert!(stream.is_ready());
    }

    #[test]
    fn test_smux_server_process_syn() {
        let cfg = smux_rs::DEFAULT_CONFIG.clone();
        let session = smux_rs::Session::new_server(&cfg).unwrap();

        // Encode a Syn frame
        let syn = smux_rs::Frame::new(smux_rs::Cmd::Syn, 0, Bytes::new());
        let mut buf = BytesMut::new();
        syn.encode(&mut buf);

        // Process it
        let results = session.process_data(&buf).unwrap();
        assert!(results.is_empty()); // Syn doesn't return data

        // Stream should be accepted
        assert_eq!(session.stream_count(), 1);
    }

    #[test]
    fn test_qpp_port_smoke() {
        // Test that QPPPort can encrypt/decrypt a round-trip over a real TCP pair.
        use kio::{AsyncReadExt, AsyncWriteExt};

        let key = b"test-key-for-qpp-smoke-test-32bytes!";
        let listen_addr: SocketAddr = "127.0.0.1:18888".parse().unwrap();
        let connect_addr = listen_addr.to_string();

        kio::block_on(async {
            // Create a TCP listener on a fixed port.
            let listener = kio::TcpListener::bind(listen_addr).await.unwrap();

            let writer = kio::spawn_task(async move {
                let mut a = kio::TcpStream::connect(&connect_addr).await.unwrap();
                a.write_all(b"hello qpp").await.unwrap();
                // Dropping `a` sends a FIN, signaling EOF to the reader.
            });

            let (b, _) = listener.accept().await.unwrap();
            let qpp = QPPPort::new(b, key, 61);
            let mut qpp = qpp;
            let mut result = Vec::new();
            qpp.read_to_end(&mut result).await.unwrap();
            assert!(!result.is_empty(), "should have decrypted data");

            let _ = writer.await;
        });
    }

    #[test]
    fn test_now_ms() {
        let t1 = now_ms();
        std::thread::sleep(Duration::from_millis(10));
        let t2 = now_ms();
        assert!(t2.wrapping_sub(t1) >= 10);
    }

    #[test]
    fn test_smux_stream_write_read() {
        let stream = smux_rs::stream::Stream::with_buffer(1, 65536);
        stream.push_data(b"hello from server").unwrap();

        let mut buf = [0u8; 64];
        let (n, _) = stream.read(&mut buf).unwrap();
        assert_eq!(n, 17);
        assert_eq!(&buf[..n], b"hello from server");

        stream.write(b"response data").unwrap();
        assert_eq!(stream.pending_send(), 13);
    }

    #[test]
    fn test_kcp_default_config() {
        // Verify KCP starts with reasonable defaults
        let kcp = KCP::new(42, 0, Box::new(|_| {}));
        assert_eq!(kcp.conv(), 42);
        assert!(kcp.mtu() >= 50);
        assert!(kcp.snd_wnd() > 0);
        assert!(kcp.rcv_wnd() > 0);
    }
}

#[test]
fn test_snappy_framing_comparison() {
    use std::io::Write;
    let mut buf = Vec::new();
    {
        let mut enc = snap::write::FrameEncoder::new(&mut buf);
        enc.write_all(b"OK\n").unwrap();
        enc.flush().unwrap();
    }
    eprintln!(
        "Rust framed: {}",
        buf.iter().map(|b| format!("{:02x}", b)).collect::<String>()
    );
    // Go produces: ff060000734e61507059010700002598a89a4f4b0a
}
