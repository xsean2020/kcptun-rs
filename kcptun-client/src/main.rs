//! kcptun-client -- KCP-based TCP stream accelerator.
//!
//! A Rust port of the Go kcptun client.
//! Listens locally, forwards connections over KCP/UDP multiplexed via SMUX.

#![allow(
    clippy::collapsible_match,
    clippy::question_mark,
    clippy::explicit_auto_deref
)]

use mimalloc::MiMalloc;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

use std::io::{self, Write};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use anyhow::{Context as AnyContext, Result};
use bytes::{Buf, Bytes, BytesMut};
use clap::Parser;
use log::{debug, error, info, trace, warn};
use parking_lot::Mutex;
use serde::Deserialize;

#[cfg(feature = "tokio")]
use kio::ReadBuf;
use kio::{self, AsyncRead, AsyncWrite};

use kcp_rs::{fec_kcp_from_recovered, FecDecoder, FecEncoder, KCP, SNMP};
use kcrypt_rs::crypt as kcp_crypt;

// ─── Constants ──────────────────────────────────────────────────────────────────

/// PBKDF2 salt matching the Go kcp-go SALT value.
const SALT: &[u8] = b"kcp-go";

/// Default KCP conversation ID.
const DEFAULT_CONV: u32 = 0xDEADBEEF;

/// Maximum UDP datagram size.
const MAX_DATAGRAM: usize = 2048;

/// Pipe buffer size.
const PIPE_BUF_SIZE: usize = 65536;

/// How often the KCP update loop fires (milliseconds).
const KCP_UPDATE_INTERVAL_MS: u64 = 2;

// ─── Config (JSON config file support) ─────────────────────────────────────────-

/// Configuration struct matching the kcptun JSON config format.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub localaddr: Option<String>,
    pub remoteaddr: Option<String>,
    pub key: Option<String>,
    pub crypt: Option<String>,
    pub mode: Option<String>,
    pub conn: Option<u32>,
    pub autoexpire: Option<u64>,
    pub scavengettl: Option<u64>,
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

/// kcptun client -- accelerate TCP over KCP.
#[derive(Debug, Parser)]
#[command(name = "kcptun-client", about, version, disable_version_flag = true)]
pub struct Cli {
    /// Local listening address.
    #[arg(short = 'l', long)]
    pub localaddr: Option<String>,

    /// Remote server address.
    #[arg(short = 'r', long)]
    pub remoteaddr: Option<String>,

    /// Pre-shared secret between client and server.
    #[arg(short, long, default_value = "it's a secrect", env = "KCPTUN_KEY")]
    pub key: Option<String>,

    /// Encryption method: null, none, xor, aes, aes-128, aes-192, aes-256,
    /// sm4, tea, xtea, salsa20, blowfish, twofish, cast5, 3des.
    #[arg(long, default_value = "aes")]
    pub crypt: Option<String>,

    /// Protocol mode: normal, fast, fast2, fast3.
    #[arg(short, long, default_value = "fast")]
    pub mode: Option<String>,

    /// Number of UDP connections to use.
    #[arg(long)]
    pub conn: Option<u32>,

    /// Auto-expire connections after N seconds of inactivity.
    #[arg(long)]
    pub autoexpire: Option<u64>,

    /// Scavenge TTL in seconds for expired connections.
    #[arg(long)]
    pub scavengettl: Option<u64>,

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
    #[arg(long)]
    pub datashard: Option<u32>,

    /// FEC parity shards.
    #[arg(long)]
    pub parityshard: Option<u32>,

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
    #[arg(long)]
    pub streambuf: Option<usize>,

    /// SMUX max frame size.
    #[arg(long)]
    pub framesize: Option<usize>,

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
            localaddr: cli.localaddr.or(cfg.localaddr),
            remoteaddr: cli.remoteaddr.or(cfg.remoteaddr),
            key: cli.key.or(cfg.key),
            crypt: cli.crypt.or(cfg.crypt),
            mode: cli.mode.or(cfg.mode),
            conn: cli.conn.or(cfg.conn),
            autoexpire: cli.autoexpire.or(cfg.autoexpire),
            scavengettl: cli.scavengettl.or(cfg.scavengettl),
            mtu: cli.mtu.or(cfg.mtu),
            sndwnd: cli.sndwnd.or(cfg.sndwnd),
            rcvwnd: cli.rcvwnd.or(cfg.rcvwnd),
            datashard: cli.datashard.or(cfg.datashard),
            parityshard: cli.parityshard.or(cfg.parityshard),
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
            streambuf: cli.streambuf.or(cfg.streambuf),
            framesize: cli.framesize.or(cfg.framesize),
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
            c: cli.c,            // CLI --c/-c flag only, not in Config struct
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

/// A persistent Snappy framing stream decoder that maintains state across
/// incremental data feeds. This is required because KCP stream mode can
/// split snappy frames across multiple recv() calls, and the snappy
/// framing format's stream header (0xff...) only appears once at the start.
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

// ─── MultiPort Parser ───────────────────────────────────────────────────────────

/// Parse a "host:minport-maxport" or "host:port" string into a list of SocketAddr.
/// Supports ":port" shorthand where host defaults to "0.0.0.0".
fn parse_multi_port(addr: &str) -> Result<Vec<SocketAddr>> {
    let colon = addr.rfind(':').context("address must include host:port")?;
    let host = if colon == 0 {
        "0.0.0.0"
    } else {
        &addr[..colon]
    };
    let port_spec = &addr[colon + 1..];

    if let Some(dash) = port_spec.find('-') {
        let min_port: u16 = port_spec[..dash].parse()?;
        let max_port: u16 = port_spec[dash + 1..].parse()?;
        let mut addrs = Vec::with_capacity((max_port - min_port + 1) as usize);
        for port in min_port..=max_port {
            addrs.push(format!("{}:{}", host, port).parse()?);
        }
        Ok(addrs)
    } else {
        let port: u16 = port_spec.parse()?;
        Ok(vec![format!("{}:{}", host, port).parse()?])
    }
}

// ─── KCP Connection ─────────────────────────────────────────────────────────────

/// A single KCP connection that carries an SMUX session.
struct KcpConn {
    /// UDP socket for network I/O.
    udp: Arc<kio::UdpSocket>,
    /// KCP state machine (shared between tasks).
    kcp: Arc<Mutex<KCP>>,
    /// SMUX session multiplexing streams over KCP.
    smux: Arc<smux_rs::Session>,
    /// Task handles for background loops.
    _handles: Vec<kio::JoinHandle<()>>,
    /// BlockCrypt instance for encrypting/decrypting KCP wire data.
    /// Stored as `Arc<dyn BlockCrypt>` (no Mutex) because `encrypt`/`decrypt`
    /// take `&self` — the cipher is stateless after construction.
    crypt: Arc<dyn kcrypt_rs::BlockCrypt>,
    /// AEAD crypt instance for GCM mode (separate from BlockCrypt).
    /// When set, the AEAD packet layout is used: [nonce 12B][ciphertext+tag].
    /// No CRC32 — authentication is built into the AEAD tag.
    aead: Option<Arc<dyn kcrypt_rs::AeadCrypt>>,
    /// Whether kcp-go v5 crypto headers (nonce+CRC32) are in use.
    /// True for all CFB encryption methods except null/none.
    /// False when using AEAD (GCM mode uses its own auth).
    has_encryption: bool,
    /// Raw KCP segments collected by the output callback during flush().
    /// Drained and encrypted+sent AFTER the KCP lock is released,
    /// to avoid starving the UDP reader task.
    ///
    /// Each entry is a `Bytes` (reference-counted slice of the KCP output
    /// buffer) handed directly by the output callback — no per-packet
    /// `Vec` alloc + `extend_from_slice` copy (R2: output Bytes pipeline).
    raw_packets: Arc<parking_lot::Mutex<Vec<bytes::Bytes>>>,
    /// Shared atomic counter of KCP wait_send, updated by the flush loop.
    /// Read by SmuxStreamAsync::poll_write for backpressure.
    wait_send: Arc<AtomicUsize>,
    /// KCP send window size, used for backpressure threshold.
    snd_wnd: usize,
    /// Notify for waking up writers blocked by backpressure.
    /// The flush loop calls notify_waiters() after each flush cycle,
    /// so blocked writers resume immediately when the window drains,
    /// instead of waiting for a 10ms sleep timeout.
    write_notify: Arc<kio::Notify>,
    /// Disable snappy compression at the SMUX session level.
    /// Must match Go kcptun's --nocomp flag for interop.
    nocomp: bool,
    /// Last activity time for auto-expire.
    last_activity: Arc<parking_lot::Mutex<std::time::Instant>>,
    /// Persistent Snappy framing encoder shared between send_frame and Task 2.
    /// snap::write::FrameEncoder uses CRC32C (Castagnoli) matching Go's golang/snappy.
    /// The stream identifier is written once by the first write; subsequent writes
    /// continue the same snappy stream without re-emitting the header.
    compressor: Arc<parking_lot::Mutex<snap::write::FrameEncoder<Vec<u8>>>>,
    /// Reusable encryption buffer with counter-based nonce.
    /// Eliminates per-packet vec![] allocation and rand::thread_rng() calls.
    crypto_buf: Arc<parking_lot::Mutex<kcp_rs::CryptoBuf>>,
    /// Notify for waking up the flush loop immediately when SMUX streams
    /// have new data to send. Eliminates the 0~10ms wait of the fixed
    /// sleep interval.
    flush_notify: Arc<kio::Notify>,
    /// Session-layer FEC encoder (Go fecEncoder). None when ds/ps == 0.
    fec_encoder: Option<Arc<parking_lot::Mutex<FecEncoder>>>,
    /// Session-layer FEC decoder (Go fecDecoder).
    fec_decoder: Option<Arc<parking_lot::Mutex<FecDecoder>>>,
}

impl KcpConn {
    /// Create a new KCP connection to the given remote address.
    #[allow(clippy::too_many_arguments)]
    async fn new(
        remote_addr: SocketAddr,
        key: &[u8; 32],
        crypt: &str,
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
    ) -> Result<Self> {
        // Bind a local UDP socket via kio (shared socket2 tuning)
        let bind_addr: SocketAddr = if remote_addr.is_ipv4() {
            "0.0.0.0:0".parse()?
        } else {
            "[::]:0".parse()?
        };
        let udp = kio::UdpSocket::connect(bind_addr, remote_addr)?;
        let udp = Arc::new(udp);

        // Validate crypt selection and create BlockCrypt / AEAD instances.
        // CryptEngine uses match-based static dispatch (no deep vtable).
        let (engine, _) = kcrypt_rs::CryptEngine::select(crypt, &key[..]);
        let crypt_state: Arc<dyn kcrypt_rs::BlockCrypt> = Arc::new(engine);

        let aead: Option<Arc<dyn kcrypt_rs::AeadCrypt>> =
            kcp_crypt::select_aead_crypt(crypt, &key[..]).map(Arc::from);
        let has_aead = aead.is_some();
        let _has_encryption = crypt != "null" && !has_aead;

        // Create KCP instance with output callback that collects raw KCP segments.
        //
        // CRITICAL: The output callback runs INSIDE the KCP lock (during flush()).
        // If it does encryption + UDP send + tokio::spawn per packet, it can take
        // 10+ ms with 240+ segments, starving the UDP reader that processes ACKs.
        //
        // Fix: The callback just pushes raw KCP data (as `Bytes` — the KCP
        // output buffer's reference-counted slice) to a shared Vec (nearly
        // instant, zero-copy). After flush() returns and the KCP lock is
        // released, the caller drains the Vec and does encryption + UDP send
        // outside the lock.
        let raw_packets = std::sync::Arc::new(parking_lot::Mutex::new(Vec::<bytes::Bytes>::new()));
        let raw_packets_cb = raw_packets.clone();
        let has_encryption = crypt != "null";

        let mut kcp = KCP::new(
            DEFAULT_CONV,
            0,
            Box::new(move |data: bytes::Bytes| {
                raw_packets_cb.lock().push(data);
            }),
        );

        // Configure KCP
        kcp.set_mtu(mtu);
        kcp.set_snd_wnd(sndwnd);
        kcp.set_rcv_wnd(rcvwnd);
        // Session-layer FEC (Go newUDPSession). header_offset=0: crypto wraps whole FEC frame.
        let fec_encoder = if datashard > 0 && parityshard > 0 {
            FecEncoder::new(datashard as usize, parityshard as usize, 0)
                .map(|e| Arc::new(parking_lot::Mutex::new(e)))
        } else {
            None
        };
        let fec_decoder = if datashard > 0 && parityshard > 0 {
            FecDecoder::new(datashard as usize, parityshard as usize)
                .map(|d| Arc::new(parking_lot::Mutex::new(d)))
        } else {
            None
        };
        if acknodelay {
            // kcp.set_ack_nodelay(true); // removed: pass ack_nodelay to input() instead
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

        let kcp = Arc::new(Mutex::new(kcp));

        // Create SMUX config
        let smux_cfg = smux_rs::Config {
            version: smuxver,
            max_receive_buffer: smuxbuf,
            max_stream_buffer: streambuf,
            max_frame_size: framesize,
            keepalive_interval: keepalive,
            keepalive_timeout: 30,
        };
        let smux = Arc::new(smux_rs::Session::new_client(&smux_cfg)?);

        let mut conn = KcpConn {
            udp: udp.clone(),
            kcp,
            smux,
            _handles: Vec::new(),
            crypt: crypt_state,
            aead,
            has_encryption,
            nocomp,
            raw_packets,
            wait_send: Arc::new(AtomicUsize::new(0)),
            snd_wnd: sndwnd as usize,
            write_notify: Arc::new(kio::Notify::new()),
            last_activity: Arc::new(parking_lot::Mutex::new(std::time::Instant::now())),
            compressor: Arc::new(parking_lot::Mutex::new(snap::write::FrameEncoder::new(
                Vec::new(),
            ))),
            crypto_buf: Arc::new(parking_lot::Mutex::new(kcp_rs::CryptoBuf::new(
                DEFAULT_CONV as u64,
            ))),
            flush_notify: Arc::new(kio::Notify::new()),
            fec_encoder,
            fec_decoder,
        };

        conn.start_background_loops();
        Ok(conn)
    }

    /// Start the background processing loops for this connection.
    ///
    /// Two tasks run in the background:
    /// 1. UDP reader + KCP input/output — reads datagrams, feeds them through
    ///    KCP, extracts user data (KCP recv) and dispatches it to the SMUX
    ///    session.
    /// 2. SMUX flush + KCP update — drains SMUX stream send buffers, wraps
    ///    them as SMUX Data frames, sends them through KCP, then advances the
    ///    KCP timer for retransmission.
    fn start_background_loops(&mut self) {
        let kcp = self.kcp.clone();
        let udp = self.udp.clone();
        let smux = self.smux.clone();
        let crypt = self.crypt.clone();
        let raw_packets = self.raw_packets.clone();
        let has_encryption = self.has_encryption;
        let aead = self.aead.clone();
        let last_activity = self.last_activity.clone();

        let nocomp = self.nocomp;

        // ── Task 1: UDP reader + decrypt + KCP input → decompress → SMUX recv ──
        let kcp1 = kcp.clone();
        let smux1 = smux.clone();
        let crypt1 = crypt.clone();
        let aead1 = aead.clone();
        let raw_packets1 = raw_packets.clone();
        let udp1 = udp.clone();
        let has_encryption1 = has_encryption;
        let has_aead1 = aead1.is_some();
        let crypto_buf1 = self.crypto_buf.clone();
        let write_notify1 = self.write_notify.clone();
        let wait_send1 = self.wait_send.clone();
        let flush_notify1 = self.flush_notify.clone();
        let fec_decoder1 = self.fec_decoder.clone();
        let fec_encoder1 = self.fec_encoder.clone();
        let h1 = kio::spawn_task(async move {
            let mut buf = vec![0u8; MAX_DATAGRAM];
            // Persistent Snappy framing decoder.
            // Handles Go kcptun's snappy.NewBufferedWriter framing format.
            let mut snappy_dec = if !nocomp {
                Some(SnappyStreamDecoder::new())
            } else {
                None
            };
            loop {
                let mut n = match udp1.recv(&mut buf).await {
                    Ok(n) if n > 0 => n,
                    Ok(_) => continue,
                    Err(e) => {
                        error!("UDP recv error: {}", e);
                        kcp_rs::snmp_add(&kcp_rs::DEFAULT_SNMP.in_errs, 1);
                        kio::sleep_ms(100).await;
                        continue;
                    }
                };
                // Process this datagram and any further ready ones (P1.3).
                loop {
                    kcp_rs::snmp_add(&kcp_rs::DEFAULT_SNMP.in_pkts, 1);
                    kcp_rs::snmp_add(&kcp_rs::DEFAULT_SNMP.in_bytes, n as u64);
                    // ── Decrypt and strip header (in-place on recv buf for CFB/null) ──
                    const FEC_HDR: usize = 8;

                    // AEAD still owns a Vec; CFB/null use slices of `buf` until
                    // KCP::input finishes (same task, before next recv/try_recv).
                    let aead_plain: Option<Vec<u8>> = if has_aead1 {
                        match aead1.as_ref().unwrap().open(&buf[..n]) {
                            Ok(plain) => Some(plain),
                            Err(_) => {
                                kcp_rs::snmp_add(&kcp_rs::DEFAULT_SNMP.in_csum_errors, 1);
                                None
                            }
                        }
                    } else {
                        None
                    };
                    if has_aead1 && aead_plain.is_none() {
                        match udp1.try_recv(&mut buf) {
                            Ok(m) if m > 0 => {
                                n = m;
                                continue;
                            }
                            _ => break,
                        }
                    }

                    // CFB body offset into buf after successful in-place decrypt (probe=false → CRYPT_HDR).
                    let mut cfb_body_off: Option<usize> = None;
                    if has_encryption1 && !has_aead1 {
                        match kcp_rs::decrypt_cfb_in_place(&mut buf[..n], crypt1.as_ref(), false) {
                            Ok(body) => {
                                // body is &buf[CRYPT_HDR..n]; record start offset.
                                cfb_body_off = Some(n - body.len());
                            }
                            Err(_) => {
                                kcp_rs::snmp_add(&kcp_rs::DEFAULT_SNMP.in_csum_errors, 1);
                                match udp1.try_recv(&mut buf) {
                                    Ok(m) if m > 0 => {
                                        n = m;
                                        continue;
                                    }
                                    _ => break,
                                }
                            }
                        }
                    }

                    // Body view: AEAD owned, CFB payload slice, or null full buf.
                    let input: &[u8] = if let Some(ref plain) = aead_plain {
                        plain.as_slice()
                    } else if let Some(off) = cfb_body_off {
                        &buf[off..n]
                    } else {
                        kcp_rs::inbound_null(&buf[..n])
                    };

                    // ── FEC handling & KCP input (matching Go's kcpInput) ──
                    // Feed KCP with slices only — no intermediate Vec of Bytes.
                    let mut had_input = false;
                    {
                        let mut kcp_guard = kcp1.lock();
                        if let Some(ref dec) = fec_decoder1 {
                            if input.len() >= 6 {
                                let fec_flag = u16::from_le_bytes(input[4..6].try_into().unwrap());
                                let recovered = {
                                    let mut d = dec.lock();
                                    d.decode(input)
                                };
                                match fec_flag {
                                    0x00f1 => {
                                        if input.len() > FEC_HDR {
                                            if kcp_guard.input(&input[FEC_HDR..], false).is_err() {
                                                kcp_rs::snmp_add(
                                                    &kcp_rs::DEFAULT_SNMP.kcp_in_errors,
                                                    1,
                                                );
                                            }
                                            had_input = true;
                                        }
                                        // recovered = [SIZE 2][KCP…][RS pad]; Go: Input(r[2:sz])
                                        for r in &recovered {
                                            if let Some(kcp_slice) = fec_kcp_from_recovered(r) {
                                                if kcp_guard.input(kcp_slice, false).is_err() {
                                                    kcp_rs::snmp_add(
                                                        &kcp_rs::DEFAULT_SNMP.kcp_in_errors,
                                                        1,
                                                    );
                                                }
                                                had_input = true;
                                            }
                                        }
                                    }
                                    0x00f2 => {
                                        for r in &recovered {
                                            if let Some(kcp_slice) = fec_kcp_from_recovered(r) {
                                                if kcp_guard.input(kcp_slice, false).is_err() {
                                                    kcp_rs::snmp_add(
                                                        &kcp_rs::DEFAULT_SNMP.kcp_in_errors,
                                                        1,
                                                    );
                                                }
                                                had_input = true;
                                            }
                                        }
                                    }
                                    0x00f3 => {
                                        log::trace!("OOB packet received: {} bytes", input.len());
                                    }
                                    _ => {
                                        if kcp_guard.input(input, false).is_err() {
                                            kcp_rs::snmp_add(
                                                &kcp_rs::DEFAULT_SNMP.kcp_in_errors,
                                                1,
                                            );
                                        }
                                        had_input = true;
                                    }
                                }
                            } else if input.len() >= 24 {
                                if kcp_guard.input(input, false).is_err() {
                                    kcp_rs::snmp_add(&kcp_rs::DEFAULT_SNMP.kcp_in_errors, 1);
                                }
                                had_input = true;
                            }
                        } else if input.len() >= 24 {
                            if kcp_guard.input(input, false).is_err() {
                                kcp_rs::snmp_add(&kcp_rs::DEFAULT_SNMP.kcp_in_errors, 1);
                            }
                            had_input = true;
                        }

                        if had_input {
                            *last_activity.lock() = std::time::Instant::now();
                        }

                        // Extract KCP recv data (decompressed on the KCP stream level)
                        while let Ok(d) = kcp_guard.recv_bytes() {
                            if !nocomp {
                                if let Some(ref mut sd) = snappy_dec {
                                    match sd.feed(&d) {
                                        Ok(decompressed) => {
                                            if !decompressed.is_empty() {
                                                if let Err(e) = smux1.process_data(&decompressed) {
                                                    warn!("SMUX process_data error: {:?}", e);
                                                }
                                                flush_notify1.notify_one();
                                            }
                                        }
                                        Err(e) => {
                                            warn!("Snappy decompress error: {:?}", e);
                                        }
                                    }
                                }
                            } else if let Err(e) = smux1.process_data(&d) {
                                warn!("SMUX process_data error: {:?}", e);
                                flush_notify1.notify_one();
                            } else {
                                flush_notify1.notify_one();
                            }
                        }
                    }

                    // ── Notify writers after ACK processing ──
                    // Go's kcpInput() calls notifyWriteEvent() when waitsnd < snd_wnd
                    // after processing incoming ACKs. This wakes up blocked writers
                    // immediately, rather than waiting for the next flush cycle.
                    {
                        let kcp_guard = kcp1.lock();
                        let ws = kcp_guard.wait_send() as usize;
                        let snd_wnd = kcp_guard.snd_wnd() as usize;
                        // Update the shared wait_send counter immediately so
                        // poll_write sees the current (post-ACK) value instead
                        // of the stale value from the last flush cycle. This
                        // prevents false backpressure from blocking writers.
                        wait_send1.store(ws, Ordering::Relaxed);
                        if ws < snd_wnd {
                            write_notify1.notify_waiters();
                        }
                    }

                    // ── Drain and send ACKs collected during input() ──
                    // FEC-encode then encrypt inline (ACKs are typically 1 small packet).
                    let acks: Vec<bytes::Bytes> = std::mem::take(&mut *raw_packets1.lock());
                    let acks: Vec<bytes::Bytes> = if let Some(ref enc) = fec_encoder1 {
                        let mut e = enc.lock();
                        kcp_rs::fec_expand_packets(&mut e, &acks, 500)
                    } else {
                        acks
                    };
                    let mut aead_out = bytes::BytesMut::new();
                    for data in acks {
                        let pkt: bytes::Bytes = if has_aead1 {
                            aead1.as_ref().unwrap().seal_into(&data, &mut aead_out)
                        } else if has_encryption1 {
                            crypto_buf1.lock().encrypt_cfb(&data, crypt1.as_ref())
                        } else {
                            // null: Bytes already owns the slice — pass through.
                            data
                        };
                        // Send ACK directly (not via spawn_task) — on smol backend,
                        // spawned tasks may not be scheduled promptly, causing
                        // ACKs to be delayed and KCP retransmissions to fire.
                        match udp1.send(&pkt).await {
                            Ok(sent) => {
                                kcp_rs::snmp_add(&kcp_rs::DEFAULT_SNMP.out_pkts, 1);
                                kcp_rs::snmp_add(&kcp_rs::DEFAULT_SNMP.out_bytes, sent as u64);
                            }
                            Err(e) => error!("UDP send error (ack): {}", e),
                        }
                    }
                    // Non-blocking drain of further ready UDP datagrams.
                    match udp1.try_recv(&mut buf) {
                        Ok(m) if m > 0 => {
                            n = m;
                            continue;
                        }
                        _ => break,
                    }
                } // end ready-packet drain loop
            }
        });
        self._handles.push(h1);

        // ── Task 2b: Decompression is now handled in h1 (before kcp.input()).
        // This task is no longer needed — keep an idle loop for compatibility.
        let h1b = kio::spawn_task(async move {
            loop {
                kio::sleep_ms(3_600_000).await;
            }
        });
        self._handles.push(h1b);

        // ── Task 2: SMUX stream drain + compress → KCP update/flush ──
        let kcp2 = kcp.clone();
        let smux2 = smux.clone();
        let raw_packets2 = raw_packets.clone();
        let nocomp2 = self.nocomp;
        let udp2 = udp.clone();
        let crypt2 = crypt.clone();
        let aead2 = aead.clone();
        let has_encryption2 = has_encryption;
        let has_aead2 = aead2.is_some();
        let wait_send2 = self.wait_send.clone();
        let write_notify2 = self.write_notify.clone();
        let compressor2 = self.compressor.clone();
        let crypto_buf2 = self.crypto_buf.clone();
        let smuxver = self.smux.version();
        let flush_notify2 = self.flush_notify.clone();
        let fec_encoder2 = self.fec_encoder.clone();
        let h2 = kio::spawn_task(async move {
            let mut next_update: u64 = KCP_UPDATE_INTERVAL_MS;
            // Reused across iterations: single buffer for SMUX frame assembly (P0.3).
            let mut out_buf = bytes::BytesMut::with_capacity(64 * 1024);

            loop {
                // Wait for either the dynamic interval (nearest RTO or
                // default) or an immediate notify from SMUX stream writes.
                let _ = kio::timeout(Duration::from_millis(next_update), flush_notify2.notified())
                    .await;
                // current_ms is no longer needed — flush() uses its own internal timestamp.

                // ── Phase 1: Drain SMUX + encode frames into out_buf (NO KCP lock) ──
                // Header reserved first, payload drained in place, length patched —
                // no to_vec / data.clone() chain (P0.3).
                out_buf.clear();
                {
                    let streams = smux2.streams();
                    let stream_map = streams.lock();
                    // Drain ALL pending SMUX bytes (multiple frames per stream).
                    // Cap total bytes per cycle to keep KCP send under control.
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
                            let n = s.drain_send_max(&mut out_buf, smux_rs::frame::MAX_FRAME_SIZE);
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
                }

                // ── Phase 1a: Collect FIN candidates (do NOT mark yet) ──
                // mark_fin_sent only after kcp.send succeeds (BUGREPORT.md deadlock class).
                let fin_candidates: Vec<u32> = {
                    let streams = smux2.streams();
                    let stream_map = streams.lock();
                    stream_map
                        .iter()
                        .filter(|(_, s)| {
                            s.is_local_closed() && s.pending_send() == 0 && !s.is_fin_sent()
                        })
                        .map(|(id, _)| *id)
                        .collect()
                };
                for &stream_id in &fin_candidates {
                    debug!("flush: encoding FIN for stream {}", stream_id);
                    smux_rs::frame::Frame::encode_header_into(
                        &mut out_buf,
                        smuxver,
                        smux_rs::frame::Cmd::Fin,
                        stream_id,
                        0,
                    );
                }

                // ── Phase 1b: Reap fully closed + local-closed past linger ──
                // Linger bounds map growth when peer FIN is lost (proxy short-connect leak).
                const STREAM_LINGER_SECS: u64 = 30;
                {
                    let streams = smux2.streams();
                    let mut stream_map = streams.lock();
                    let linger = std::time::Duration::from_secs(STREAM_LINGER_SECS);
                    let to_remove: Vec<u32> = stream_map
                        .iter()
                        .filter(|(_, s)| {
                            if s.is_local_closed() && s.is_remote_closed() && s.is_fin_sent() {
                                return true;
                            }
                            if s.is_local_closed() && s.pending_send() == 0 {
                                if let Some(e) = s.local_closed_elapsed() {
                                    return e >= linger;
                                }
                            }
                            false
                        })
                        .map(|(id, _)| *id)
                        .collect();
                    for id in &to_remove {
                        if let Some(s) = stream_map.remove(id) {
                            s.close();
                        }
                    }
                    if !to_remove.is_empty() {
                        debug!("SMUX: reaped {} closed/stale streams", to_remove.len());
                    }
                    drop(stream_map);
                }

                // ── Phase 1c: Drain UPD frames (matching Go's sendWindowUpdate) ──
                smux2.check_upd();
                let upd_before = out_buf.len();
                for upd in smux2.take_upd_frames() {
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
                if out_buf.len() > upd_before {
                    debug!(
                        "SMUX: UPD frames appended ({} -> {} bytes)",
                        upd_before,
                        out_buf.len()
                    );
                }

                // ── Phase 2: Snappy compress OUTSIDE KCP lock (P0.4) ──
                // Matches server Phase 3/4 split — keeps ACK path unblocked.
                // Large flushes offload to cpu_block so the reactor can process
                // UDP/ACKs concurrently (esp. smol). Small flushes stay inline.
                let send_data: Option<Vec<u8>> = if out_buf.is_empty() {
                    None
                } else if !nocomp2 {
                    use std::io::Write;
                    let plain = out_buf.split().to_vec();
                    let plain_len = plain.len();
                    let compress_fn = {
                        let compressor = compressor2.clone();
                        move || {
                            let mut enc = compressor.lock();
                            enc.write_all(&plain).ok();
                            enc.flush().ok();
                            std::mem::take(enc.get_mut())
                        }
                    };
                    let compressed = if kcp_rs::should_cpu_block_compress(plain_len)
                        && !has_encryption2
                        && !has_aead2
                    {
                        kio::cpu_block(compress_fn).await
                    } else {
                        compress_fn()
                    };
                    if compressed.is_empty() {
                        None
                    } else {
                        Some(compressed)
                    }
                } else {
                    Some(out_buf.split().to_vec())
                };

                // ── Phase 3: kcp.send + flush only (KCP lock held briefly) ──
                {
                    let mut kcp_guard = kcp2.lock();
                    let had_outbound = send_data.is_some();

                    if let Some(to_send) = send_data {
                        if !to_send.is_empty() {
                            // Split into chunks of at most (KCP_MAX_FRAG - 1) * MSS
                            // to avoid TooManyFragments error. Matches server behavior.
                            let mss = kcp_guard.mss() as usize;
                            let max_chunk = (kcp_rs::segment::KCP_MAX_FRAG as usize)
                                .saturating_sub(1)
                                .saturating_mul(mss)
                                .max(mss);
                            let mut offset = 0;
                            let mut send_ok = true;
                            while offset < to_send.len() {
                                let end = (offset + max_chunk).min(to_send.len());
                                if let Err(e) = kcp_guard.send(&to_send[offset..end]) {
                                    warn!(
                                        "KCP send error at offset {}/{}: {:?}",
                                        offset,
                                        to_send.len(),
                                        e
                                    );
                                    send_ok = false;
                                    break;
                                }
                                offset = end;
                            }
                            // Only mark FIN after the whole batch was accepted by KCP.
                            if send_ok && !fin_candidates.is_empty() {
                                let streams = smux2.streams();
                                let stream_map = streams.lock();
                                for id in &fin_candidates {
                                    if let Some(s) = stream_map.get(id) {
                                        s.mark_fin_sent();
                                    }
                                }
                            }
                        }
                    } else if !fin_candidates.is_empty() {
                        // FIN-only cycle (no PSH/UPD payload after compress empty path handled above).
                        // fin frames live in send_data when out_buf non-empty; if send_data is None
                        // there was nothing to send — leave fin_sent false for retry.
                    }

                    // Call flush() directly (matching Go's UDPSession.update()
                    // which calls s.kcp.flush() directly, NOT the deprecated
                    // Update() that throttles via ts_flush). This avoids
                    // double-flushing (update() internally calls flush() too).
                    next_update = kcp_guard.flush() as u64;
                    let ws = kcp_guard.wait_send() as usize;
                    // P2.2: data just queued or still in-flight → wake ASAP so
                    // ACKs/retrans and remaining SMUX bytes are not delayed by
                    // the full interval clamp.
                    if had_outbound || ws > 0 {
                        next_update = 1;
                    } else {
                        next_update = next_update.clamp(1, KCP_UPDATE_INTERVAL_MS);
                    }
                    // Update shared wait_send counter for poll_write backpressure
                    wait_send2.store(ws, Ordering::Relaxed);
                    // Wake up any writers blocked by backpressure — they'll
                    // re-check wait_send() and resume if the window drained.
                    write_notify2.notify_waiters();
                }

                // ── Encrypt + send raw KCP packets OUTSIDE the KCP lock ──
                // The output callback (called during flush) just collected
                // raw KCP segments into raw_packets. Now we drain and send
                // them. This allows the UDP reader task to acquire the KCP
                // lock concurrently to process incoming ACKs.
                let packets: Vec<bytes::Bytes> = std::mem::take(&mut *raw_packets2.lock());
                if packets.is_empty() {
                    // No KCP wire output this cycle (P2.2 empty_flush metric).
                    kcp_rs::snmp_add(&kcp_rs::DEFAULT_SNMP.empty_flush, 1);
                } else if packets.len() > 1 {
                    debug!("raw_packets drain: {} packets", packets.len());
                }
                if !packets.is_empty() {
                    // FEC encode (Go: KCP → FEC → encrypt → UDP), rto=500ms.
                    let packets: Vec<bytes::Bytes> = if let Some(ref enc) = fec_encoder2 {
                        let mut e = enc.lock();
                        kcp_rs::fec_expand_packets(&mut e, &packets, 500)
                    } else {
                        packets
                    };

                    let total_bytes: usize = packets.iter().map(|p| p.len()).sum();
                    let use_cpu_block = kcp_rs::should_cpu_block_encrypt(
                        has_encryption2,
                        has_aead2,
                        packets.len(),
                        total_bytes,
                    );

                    let crypt_sb = crypt2.clone();
                    let crypto_buf_sb = crypto_buf2.clone();
                    let aead_sb = aead2.clone();
                    // When offloaded to cpu_block, disable nested thread::scope
                    // parallel encrypt (already on a pool worker). Inline path
                    // may still parallelize large CFB batches (P1.1).
                    let allow_parallel = !use_cpu_block;
                    let encrypt_fn = move || {
                        kcp_rs::encrypt_batch(
                            packets,
                            crypt_sb.as_ref(),
                            &crypto_buf_sb,
                            aead_sb.as_deref(),
                            has_encryption2,
                            allow_parallel,
                        )
                    };

                    let encrypted: Vec<bytes::Bytes> = if use_cpu_block {
                        kio::cpu_block(encrypt_fn).await
                    } else {
                        encrypt_fn()
                    };

                    match udp2.send_batch(&encrypted).await {
                        Ok(()) => {
                            let nbytes: u64 = encrypted.iter().map(|b| b.len() as u64).sum();
                            kcp_rs::snmp_add(
                                &kcp_rs::DEFAULT_SNMP.out_pkts,
                                encrypted.len() as u64,
                            );
                            kcp_rs::snmp_add(&kcp_rs::DEFAULT_SNMP.out_bytes, nbytes);
                        }
                        Err(e) => error!("UDP send error: {}", e),
                    }
                }

                // If SMUX still has buffered data *and* peer window allows more
                // send, wake immediately. When peer_send_window==0 we must NOT
                // busy-spin — wait for an UPD (UDP reader notifies flush).
                {
                    let streams = smux2.streams();
                    let stream_map = streams.lock();
                    let still_pending = stream_map
                        .values()
                        .any(|s| s.pending_send() > 0 && s.peer_send_window() > 0);
                    drop(stream_map);
                    if still_pending {
                        next_update = 1;
                        flush_notify2.notify_one();
                    }
                }
            }
        });
        self._handles.push(h2);
    }

    /// Send an SMUX frame through KCP.
    /// When compression is enabled, the frame is passed through the persistent
    /// snap::write::FrameEncoder (CRC32C/Castagnoli, correct snappy framing format)
    /// matching Go kcptun's CompStream behaviour. The stream header is written once
    /// on the first call and subsequent calls continue the same snappy stream.
    fn send_frame(&self, frame: &smux_rs::Frame) -> Result<()> {
        let mut buf = BytesMut::with_capacity(12 + frame.data.len());
        frame.encode(&mut buf);
        trace!("send_frame: {} bytes, nocomp={}", buf.len(), self.nocomp);
        // Write to KCP — flush is handled by the flush loop (every 10ms)
        // and by the immediate flush in poll_write's backpressure path.
        // Calling flush() here would cause excessive lock contention when
        // many streams write concurrently.
        if !self.nocomp {
            use std::io::Write;
            let mut enc = self.compressor.lock();
            enc.write_all(&buf).ok();
            enc.flush().ok();
            let to_send = std::mem::take(enc.get_mut());
            self.kcp.lock().send(&to_send)?;
        } else {
            self.kcp.lock().send(&buf)?;
        }
        Ok(())
    }

    /// Access the SMUX session.
    fn session(&self) -> &smux_rs::Session {
        &self.smux
    }
}

// ─── SMUX Async Wrapper ─────────────────────────────────────────────────────────

/// An async wrapper around an SMUX stream, implementing AsyncRead + AsyncWrite.
struct SmuxStreamAsync {
    stream: Arc<smux_rs::stream::Stream>,
    /// Shared counter of KCP wait_send, updated by the flush loop.
    /// Used for backpressure: when wait_send is too high, poll_write
    /// returns Pending to stop the pipe from flooding KCP.
    wait_send: Arc<AtomicUsize>,
    /// KCP send window, used as the backpressure threshold.
    snd_wnd: usize,
    /// Notify for waking up the flush loop immediately on new data.
    flush_notify: Arc<kio::Notify>,
    /// Wakes writers blocked on KCP send-window backpressure.
    /// Signaled from the UDP-ACK path and the flush loop (same role as
    /// Go kcp-go's `chWriteEvent`).
    write_notify: Arc<kio::Notify>,
    /// Ensures at most one backpressure waiter task is armed.
    bp_armed: Arc<AtomicBool>,
}

impl SmuxStreamAsync {
    fn new(
        stream: Arc<smux_rs::stream::Stream>,
        wait_send: Arc<AtomicUsize>,
        snd_wnd: usize,
        flush_notify: Arc<kio::Notify>,
        write_notify: Arc<kio::Notify>,
    ) -> Self {
        SmuxStreamAsync {
            stream,
            wait_send,
            snd_wnd,
            flush_notify,
            write_notify,
            bp_armed: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Park until KCP send window has room, then wake the poller.
    ///
    /// Prefers `write_notify` (ACK / flush driven). A short timeout is only a
    /// safety net for the rare lost-wakeup race with `notify_waiters` (which
    /// does not store a permit). At most one waiter task is armed at a time.
    fn arm_backpressure_wake(this: &Self, cx: &mut Context<'_>) {
        // Always refresh the waker for the current poller.
        let waker = cx.waker().clone();
        if this
            .bp_armed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            // Already armed — still re-check after a short yield so a race
            // where the waiter just exited cannot stall forever.
            let bp_armed = this.bp_armed.clone();
            let wait_send = this.wait_send.clone();
            let snd_wnd = this.snd_wnd;
            kio::spawn_task(async move {
                kio::sleep_ms(1).await;
                if wait_send.load(Ordering::Relaxed) < snd_wnd {
                    waker.wake();
                } else if !bp_armed.load(Ordering::Acquire) {
                    // Previous waiter finished while still blocked; re-arm path
                    // will run on next poll. Wake so we re-enter poll_write.
                    waker.wake();
                }
            });
            return;
        }
        let write_notify = this.write_notify.clone();
        let wait_send = this.wait_send.clone();
        let snd_wnd = this.snd_wnd;
        let bp_armed = this.bp_armed.clone();
        kio::spawn_task(async move {
            loop {
                let _ = kio::timeout(Duration::from_millis(2), write_notify.notified()).await;
                if wait_send.load(Ordering::Relaxed) < snd_wnd {
                    bp_armed.store(false, Ordering::Release);
                    waker.wake();
                    return;
                }
            }
        });
    }
}

// ── tokio AsyncRead/AsyncWrite (uses ReadBuf) ──
#[cfg(feature = "tokio")]
impl AsyncRead for SmuxStreamAsync {
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
                this.stream.register_read_waker(cx.waker().clone());
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
impl AsyncWrite for SmuxStreamAsync {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let ws = this.wait_send.load(Ordering::Relaxed);
        if ws >= this.snd_wnd {
            Self::arm_backpressure_wake(this, cx);
            return Poll::Pending;
        }
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

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.get_mut().stream.mark_local_closed();
        Poll::Ready(Ok(()))
    }
}

// ── smol AsyncRead/AsyncWrite (uses &mut [u8]) ──
#[cfg(feature = "smol")]
impl AsyncRead for SmuxStreamAsync {
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
impl AsyncWrite for SmuxStreamAsync {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let ws = this.wait_send.load(Ordering::Relaxed);
        if ws >= this.snd_wnd {
            Self::arm_backpressure_wake(this, cx);
            return Poll::Pending;
        }
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
        self.get_mut().stream.mark_local_closed();
        Poll::Ready(Ok(()))
    }
}

struct QPPPort<T: AsyncRead + AsyncWrite + Unpin> {
    inner: T,
    qpp: Mutex<qpp_rs::QuantumPermutationPad>,
    prng_enc: Mutex<qpp_rs::Rand>,
    prng_dec: Mutex<qpp_rs::Rand>,
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
            qpp: Mutex::new(qpp_rs::QuantumPermutationPad::new(key, count)),
            prng_enc: Mutex::new(qpp_rs::create_prng(key)),
            prng_dec: Mutex::new(qpp_rs::create_prng(key)),
            read_buf: BytesMut::with_capacity(PIPE_BUF_SIZE),
            read_io_buf: vec![0u8; PIPE_BUF_SIZE],
            write_enc_buf: Vec::with_capacity(PIPE_BUF_SIZE),
        }
    }
}

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
                {
                    let qpp = this.qpp.lock();
                    let mut prng = this.prng_dec.lock();
                    qpp_rs::decrypt_with_pads(
                        &qpp.rpads,
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
                        &qpp.rpads,
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
async fn pipe<A, B>(a: &mut A, b: &mut B) -> Result<(u64, u64)>
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    let n = kio::copy_bidirectional(a, b).await?;
    Ok(n)
}

/// Handle a single client connection: pipe between local TCP and SMUX stream
/// with optional QPP. Compression is handled at the KCP/SMUX session level
/// (matching Go kcptun architecture).
#[allow(clippy::too_many_arguments)]
async fn handle_client(
    local: kio::TcpStream,
    smux_stream: Arc<smux_rs::stream::Stream>,
    wait_send: Arc<AtomicUsize>,
    snd_wnd: usize,
    qpp_enabled: bool,
    qpp_key: Vec<u8>,
    qpp_count: u16,
    flush_notify: Arc<kio::Notify>,
    write_notify: Arc<kio::Notify>,
) -> Result<()> {
    let smux_async = SmuxStreamAsync::new(
        smux_stream.clone(),
        wait_send,
        snd_wnd,
        flush_notify,
        write_notify,
    );

    // Timeout for idle connections to prevent file descriptor leaks.
    // If neither side sends data for this duration, the pipe is closed.
    const IDLE_TIMEOUT: Duration = Duration::from_secs(300); // 5 minutes

    let pipe_result = if qpp_enabled {
        let qpp_port = QPPPort::new(smux_async, &qpp_key, qpp_count);
        let mut local_pin = local;
        let mut qpp_pin = qpp_port;
        kio::timeout(IDLE_TIMEOUT, pipe(&mut local_pin, &mut qpp_pin)).await
    } else {
        let mut local_pin = local;
        let mut smux_pin = smux_async;
        kio::timeout(IDLE_TIMEOUT, pipe(&mut local_pin, &mut smux_pin)).await
    };

    // Local half-close only. Do NOT mark_fin_sent here — that blocked the flush
    // loop from ever encoding a real FIN (BUGREPORT_PROXY_MEMORY_GROWTH).
    // Flush marks fin_sent after FIN is queued; linger reaps if peer never FINs.
    smux_stream.mark_local_closed();
    smux_stream.clear_buffers();

    match pipe_result {
        Ok(Ok((a, b))) => {
            info!(
                "pipe completed: {} sent, {} recv{}",
                a,
                b,
                if qpp_enabled { " (QPP)" } else { "" }
            );
        }
        Ok(Err(e)) => {
            warn!("pipe error: {}", e);
        }
        Err(_) => {
            warn!(
                "pipe timed out after {}s (idle connection)",
                IDLE_TIMEOUT.as_secs()
            );
        }
    }

    Ok(())
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
    let headers = SNMP::header();
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
        // Read process-wide counters updated by KCP hot paths.
        // SNMP::new() is a fresh zeroed instance — that always prints zeros.
        let values = kcp_rs::DEFAULT_SNMP.to_slice();
        if let Err(e) = writeln!(writer, "{},{}", ts, values.join(",")) {
            error!("SNMP log write error: {}", e);
        }
        if let Err(e) = writer.flush() {
            error!("SNMP log flush error: {}", e);
        }
    }
}

// ─── Main ───────────────────────────────────────────────────────────────────────

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

    // Logging: controlled by RUST_LOG env var, defaults to "info".
    // Use RUST_LOG=debug for debug output, RUST_LOG=trace for everything.
    // Example: RUST_LOG=kcptun_client=debug,kcp_rs=info cargo run --release
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_secs()
        .init();
    info!(
        "log level: {} (set RUST_LOG=debug for verbose output)",
        std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into())
    );

    let local_addr = cli.localaddr.as_deref().unwrap_or("127.0.0.1:12948");
    let remote_addr_str = cli
        .remoteaddr
        .as_deref()
        .context("remote address (-r) is required")?;

    let key_str = cli.key.as_deref().unwrap();
    let crypt = cli.crypt.as_deref().unwrap();
    let mode = cli.mode.as_deref().unwrap();
    let conn_count = cli.conn.unwrap_or(1).max(1);
    let mtu = cli.mtu.unwrap_or(1350);
    let sndwnd = cli.sndwnd.unwrap_or(128);
    let rcvwnd = cli.rcvwnd.unwrap_or(512);
    let datashard = cli.datashard.unwrap_or(10);
    let parityshard = cli.parityshard.unwrap_or(3);
    let nocomp = cli.nocomp;
    let acknodelay = cli.acknodelay;
    let nodelay = cli.nodelay.unwrap_or(0);
    let interval = cli.interval.unwrap_or(30);
    let resend = cli.resend.unwrap_or(2);
    let nc = cli.nc.unwrap_or(1);
    let smuxver = cli.smuxver.unwrap_or(2);
    let smuxbuf = cli.smuxbuf.unwrap_or(4 * 1024 * 1024);
    let streambuf = cli.streambuf.unwrap_or(2097152);
    let framesize = cli.framesize.unwrap_or(8192);
    let keepalive = cli.keepalive.unwrap_or(10);
    let autoexpire = cli.autoexpire.unwrap_or(0);
    let scavengettl = cli.scavengettl.unwrap_or(180);
    let qpp_enabled = cli.qpp;
    let qpp_count = cli.qppcount.unwrap_or(61);

    // Derive encryption key
    let key = derive_key(key_str);
    info!(
        "key derived: crypt={}, key={:02x}..{:02x}",
        crypt, key[0], key[31]
    );

    // Parse remote addresses (supports multi-port format)
    let remote_addrs = parse_multi_port(remote_addr_str)?;

    // Create KCP connection pool
    let mut conns = Vec::with_capacity(conn_count as usize);
    for i in 0..conn_count as usize {
        let remote = remote_addrs[i % remote_addrs.len()];
        info!(
            "creating KCP connection {}/{} -> {}",
            i + 1,
            conn_count,
            remote
        );

        let conn = KcpConn::new(
            remote,
            &key,
            crypt,
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
        )
        .await?;

        kcp_rs::DEFAULT_SNMP.session_opened(true);
        conns.push(conn);
    }

    info!("established {} KCP connections", conns.len());

    // Parse local listen address
    let listen_addr: SocketAddr = parse_multi_port(local_addr)?
        .into_iter()
        .next()
        .context("invalid local address")?;

    // Start SNMP logger if configured
    let stop_flag = Arc::new(AtomicBool::new(false));
    // SNMP collection is off by default (zero hot-path cost). Enable only when
    // a log path is set and period > 0.
    if let Some(ref snmplog_path) = cli.snmplog {
        let secs = cli.snmpperiod.unwrap_or(60);
        if secs > 0 && !snmplog_path.is_empty() {
            kcp_rs::snmp_enable();
            let period = Duration::from_secs(secs);
            let s = stop_flag.clone();
            let p = snmplog_path.clone();
            kio::spawn_task(async move {
                snmp_logger(p, period, s).await;
            });
        } else {
            log::warn!("snmplog set but snmpperiod=0 or empty path — SNMP collection disabled");
        }
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

    // Start auto-expire scavenger if enabled (matching Go client)
    if autoexpire > 0 {
        let ttl = scavengettl.max(10);
        let s = stop_flag.clone();
        kio::spawn_task(async move {
            // In Go, the scavenger uses timedSession channel + expiryDate tracking.
            // Rust's KcpConn has last_activity/is_expired() built in.
            // For single-connection clients, the connection stays alive as long
            // as the process runs. For multi-connection, a full timedSession
            // tracker would be needed.
            info!("scavenger: autoexpire={}, scavengettl={}", autoexpire, ttl);
            loop {
                kio::sleep_ms(30_000).await;
                if s.load(Ordering::Acquire) {
                    break;
                }
            }
        });
    }

    // Start the TCP listener
    let listener = kio::TcpListener::bind(listen_addr).await?;
    info!("listening on {}", listen_addr);

    // Spawn Ctrl-C handler (runtime-agnostic)
    {
        let stop = stop_flag.clone();
        kio::spawn_task(async move {
            let _ = kio::ctrl_c().await;
            stop.store(true, Ordering::Relaxed);
        });
    }

    // Accept loop with round-robin across KCP connections
    let round_robin = Arc::new(AtomicUsize::new(0));

    loop {
        if stop_flag.load(Ordering::Relaxed) {
            info!("shutting down...");
            break;
        }

        match kio::timeout(Duration::from_millis(500), listener.accept()).await {
            Ok(Ok((local, peer))) => {
                if stop_flag.load(Ordering::Relaxed) {
                    info!("shutting down, rejecting new connection from {}", peer);
                    break;
                }

                let idx = round_robin.fetch_add(1, Ordering::Relaxed) % conns.len();
                let conn = &conns[idx];

                let smux_stream = match conn.session().open_stream() {
                    Ok(s) => s,
                    Err(e) => {
                        error!("failed to open SMUX stream: {:?}", e);
                        continue;
                    }
                };

                debug!("sending SYN for stream {}", smux_stream.id());
                let syn_frame =
                    smux_rs::Frame::new(smux_rs::Cmd::Syn, smux_stream.id(), Bytes::new())
                        .with_ver(conn.session().version());
                if let Err(e) = conn.send_frame(&syn_frame) {
                    error!("failed to send Syn frame: {}", e);
                    // open_stream already inserted into the session map — drop it.
                    conn.session().remove_stream(smux_stream.id());
                    continue;
                }
                trace!("SYN sent, flushing KCP");
                conn.kcp.lock().flush();
                trace!("KCP flushed for SYN");

                let stream_id = smux_stream.id();
                info!("accepted connection from {} (stream {})", peer, stream_id);

                let qpp_key = key_str.as_bytes().to_vec();
                let ws = conn.wait_send.clone();
                let sw = conn.snd_wnd;
                let flush_notify_ref = conn.flush_notify.clone();
                let write_notify_ref = conn.write_notify.clone();
                kio::spawn_task(async move {
                    if let Err(e) = handle_client(
                        local,
                        smux_stream,
                        ws,
                        sw,
                        qpp_enabled,
                        qpp_key,
                        qpp_count,
                        flush_notify_ref,
                        write_notify_ref,
                    )
                    .await
                    {
                        error!("client handler error (stream {}): {:?}", stream_id, e);
                    }
                    info!("stream {} closed", stream_id);
                });
            }
            Ok(Err(e)) => {
                error!("accept error: {}", e);
                continue;
            }
            Err(_) => continue, // timeout, loop back to check stop_flag
        }
    }

    // Graceful shutdown
    info!("shutting down...");
    kio::sleep_ms(1000).await;
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

        async fn respond(stream: &mut kio::TcpStream, status: &str, ctype: &str, body: &[u8]) {
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
    fn test_parse_multi_port_single() {
        let addrs = parse_multi_port("127.0.0.1:1234").unwrap();
        assert_eq!(addrs.len(), 1);
        assert_eq!(addrs[0].port(), 1234);
    }

    #[test]
    fn test_parse_multi_port_range() {
        let addrs = parse_multi_port("127.0.0.1:1000-1003").unwrap();
        assert_eq!(addrs.len(), 4);
        assert_eq!(addrs[0].port(), 1000);
        assert_eq!(addrs[3].port(), 1003);
    }

    #[test]
    fn test_parse_multi_port_ipv6() {
        let addrs = parse_multi_port("[::1]:1234").unwrap();
        assert_eq!(addrs.len(), 1);
    }

    #[test]
    fn test_apply_mode_normal() {
        let mut kcp = KCP::new(1, 0, Box::new(|_| {}));
        apply_mode(&mut kcp, "normal");
        assert_eq!(kcp.interval(), 40);
    }

    #[test]
    fn test_apply_mode_fast3() {
        let mut kcp = KCP::new(1, 0, Box::new(|_| {}));
        apply_mode(&mut kcp, "fast3");
        assert_eq!(kcp.interval(), 10);
    }

    #[test]
    fn test_config_deserialize() {
        let json = r#"{
            "localaddr": "127.0.0.1:12948",
            "remoteaddr": "127.0.0.1:29900",
            "key": "test-key",
            "crypt": "aes-128",
            "mode": "fast2",
            "conn": 2,
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
        assert_eq!(cfg.localaddr.as_deref(), Some("127.0.0.1:12948"));
        assert_eq!(cfg.mode.as_deref(), Some("fast2"));
        assert_eq!(cfg.conn, Some(2));
        assert_eq!(cfg.smuxver, Some(2));
    }

    #[test]
    fn test_cli_merge() {
        let cli = Cli {
            localaddr: Some("0.0.0.0:8080".into()),
            remoteaddr: None,
            key: None,
            crypt: None,
            mode: None,
            conn: None,
            autoexpire: None,
            scavengettl: None,
            mtu: None,
            sndwnd: None,
            rcvwnd: None,
            datashard: None,
            parityshard: None,
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
            streambuf: None,
            framesize: None,
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
            remoteaddr: Some("server:1234".into()),
            key: Some("cfg-key".into()),
            ..Default::default()
        };
        let merged = Cli::merge(cli, cfg);
        assert_eq!(merged.localaddr.as_deref(), Some("0.0.0.0:8080"));
        assert_eq!(merged.remoteaddr.as_deref(), Some("server:1234"));
        assert_eq!(merged.key.as_deref(), Some("cfg-key"));
    }

    #[test]
    fn test_empty_config() {
        let cfg: Config = serde_json::from_str("{}").unwrap();
        assert!(cfg.localaddr.is_none());
    }

    #[test]
    fn test_smux_frame_roundtrip() {
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
        let frame = Frame::new(Cmd::Syn, 1, Bytes::new());
        let mut buf = BytesMut::new();
        frame.encode(&mut buf);
        let (decoded, _) = Frame::decode(&buf).unwrap();
        assert_eq!(decoded.cmd, Cmd::Syn);
        assert_eq!(decoded.stream_id, 1);
        assert_eq!(
            buf.len(),
            8,
            "Go smux frame header is 8 bytes (ver|cmd|sid|len)"
        );
    }
}
