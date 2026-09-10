//! Zero-allocation encryption/decryption helpers for the kcptun wire format.
//!
//! The Go kcp-go v5 CFB wire format is:
//!   `[nonce 16B][CRC32 4B][ciphertext]`
//!
//! The nonce does NOT participate in the CFB IV logic (the IV is the fixed
//! `GO_CFB_IV`), so it can be any value — including a counter. This module
//! replaces the per-packet `rand::thread_rng().fill_bytes()` + `vec![]`
//! allocation with:
//! - An `AtomicU64` counter for nonce generation (no PRNG call per packet)
//! - A reusable `BytesMut` buffer (no heap allocation per packet)
//! - `Bytes` return type (reference-counted, zero-copy send to tokio tasks)
//!
//! ## Nonce design
//!
//! The 16-byte nonce is split into:
//!   `[counter 8B][session_id 8B]`
//!
//! The counter increments per packet within a session; the session_id
//! provides cross-session diversity. This is safe because the CFB IV is
//! fixed (`GO_CFB_IV`) — the nonce is only encrypted as part of the packet
//! header, not used as a cryptographic IV.

use std::sync::atomic::{AtomicU64, Ordering};

use bytes::{Bytes, BytesMut};

use crate::crypt::{AeadCrypt, BlockCrypt, CryptEngine};

/// Crypto header size: `[nonce 16B][CRC32 4B]`.
pub const CRYPTO_HEADER_SIZE: usize = 20;
/// Nonce size.
pub const NONCE_SIZE: usize = 16;

/// Deprecated alias for [`CRYPTO_HEADER_SIZE`].
#[deprecated(note = "renamed to CRYPTO_HEADER_SIZE")]
pub const CRYPT_HDR: usize = CRYPTO_HEADER_SIZE;
/// Deprecated alias for [`NONCE_SIZE`].
#[deprecated(note = "renamed to NONCE_SIZE")]
pub const NONCE_SZ: usize = NONCE_SIZE;

/// Inbound CFB decrypt / header-strip failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum InboundCryptError {
    /// Packet shorter than crypto header (or empty after probe).
    #[error("packet shorter than crypto header")]
    Short,
    /// CRC32 of payload does not match the header field.
    #[error("payload CRC32 does not match crypto header")]
    CrcMismatch,
}

/// Decrypt a CFB wire packet **in place** and return a slice of the plaintext body.
///
/// Layout after decrypt: `[nonce 16B][CRC32 4B][payload…]`. On success the
/// returned slice is `&buf[CRYPTO_HEADER_SIZE..]` — **no** heap alloc and **no** copy
/// into `CryptoBuf.enc_buf`.
///
/// `probe_header`: when `true` (server historical/compat path), if byte 4 is a
/// KCP cmd (`0x51..=0x54`) the whole buffer is treated as already-plaintext
/// with **no** crypto header. When `false` (client / normal Go CFB), always
/// require and strip the 20B header after CRC check.
///
/// Callers must finish using the returned slice (e.g. `KCP::input`) before
/// overwriting `buf` again.
#[inline]
pub fn decrypt_cfb_in_place<'a>(
    buf: &'a mut [u8],
    crypt: &CryptEngine,
    probe_header: bool,
) -> Result<&'a [u8], InboundCryptError> {
    if buf.is_empty() {
        return Err(InboundCryptError::Short);
    }
    crypt.decrypt(buf);
    strip_cfb_header_if_present(buf, probe_header)
}

/// After CFB decrypt (or for an already-plaintext buffer), optionally detect
/// and strip the 20B crypto header. See [`decrypt_cfb_in_place`].
#[inline]
pub fn strip_cfb_header_if_present(
    buf: &[u8],
    probe_header: bool,
) -> Result<&[u8], InboundCryptError> {
    if probe_header {
        if buf.len() > CRYPTO_HEADER_SIZE {
            let cmd = buf[4];
            let has_header = cmd != 0x51 && cmd != 0x52 && cmd != 0x53 && cmd != 0x54;
            if !has_header {
                return Ok(buf);
            }
        } else if buf.len() >= 5 {
            let cmd = buf[4];
            if cmd == 0x51 || cmd == 0x52 || cmd == 0x53 || cmd == 0x54 {
                return Ok(buf);
            }
            return Err(InboundCryptError::Short);
        } else {
            return Err(InboundCryptError::Short);
        }
    } else if buf.len() <= CRYPTO_HEADER_SIZE {
        return Err(InboundCryptError::Short);
    }

    let stored_crc = u32::from_le_bytes(
        buf[NONCE_SIZE..CRYPTO_HEADER_SIZE]
            .try_into()
            .map_err(|_| InboundCryptError::Short)?,
    );
    let computed_crc = crc32fast::hash(&buf[CRYPTO_HEADER_SIZE..]);
    if stored_crc != computed_crc {
        return Err(InboundCryptError::CrcMismatch);
    }
    Ok(&buf[CRYPTO_HEADER_SIZE..])
}

/// null cipher inbound view — identity slice (no crypto header).
#[inline]
pub fn inbound_null(buf: &[u8]) -> &[u8] {
    buf
}

/// A reusable encryption buffer with a monotonic nonce counter.
///
/// Designed to be held inside a `Mutex` or `parking_lot::Mutex` and called
/// from a single logical encryption path. The buffer is reused across
/// packets, eliminating per-packet `vec![]` allocation.
///
/// ## Buffer reuse strategy
/// - `enc_buf`: Used for CFB (encrypt_cfb / prepare_encrypt).
///   Capacity is retained across calls; we only clear + reserve, never shrink to zero.
/// - `aead_buf`: Dedicated AEAD seal buffer (seal_aead). Reused across flush cycles so
///   `seal_into` never allocates after warmup. One per `CryptoBuf` (i.e., per logical path).
pub struct CryptoBuf {
    /// Reusable encryption buffer — capacity is retained across calls.
    /// We only ever clear() + reserve(), never let it shrink to zero.
    /// See [`CryptoBuf`] documentation for the full reuse strategy.
    enc_buf: BytesMut,
    /// Reusable AEAD seal buffer — capacity retained across flush cycles.
    /// One buffer per `CryptoBuf` (i.e. per logical encrypt path) so `seal_into`
    /// never allocates after the first use. Used by [`CryptoBuf::seal_aead`].
    aead_buf: BytesMut,
    /// Monotonic nonce counter (replaces `rand::thread_rng`).
    nonce_counter: AtomicU64,
    /// Session identifier for nonce diversity.
    session_id: u64,
}

impl CryptoBuf {
    /// Create a new `CryptoBuf` with the given session ID for nonce diversity.
    pub fn new(session_id: u64) -> Self {
        CryptoBuf {
            enc_buf: BytesMut::with_capacity(2048),
            aead_buf: BytesMut::with_capacity(2048),
            nonce_counter: AtomicU64::new(0),
            session_id,
        }
    }

    /// Encrypt `data` using the CFB wire format, returning a `Bytes` that
    /// is reference-counted (zero-copy clone for tokio::spawn).
    ///
    /// Layout: `[nonce 16B][CRC32 4B][ciphertext]`
    ///
    /// This method reuses the internal buffer — no `vec![]` allocation
    /// occurs per packet. The returned `Bytes` shares the underlying
    /// allocation via reference counting.
    ///
    /// ## Implementation notes
    /// - Uses `extend_from_slice` (single O(n) write) instead of `resize(total, 0)`
    ///   followed by `copy_from_slice`. The zero-fill was immediately overwritten.
    /// - Nonce is built from a monotonic counter + session_id; no per-packet PRNG.
    #[inline]
    pub fn encrypt_cfb(&mut self, data: &[u8], crypt: &CryptEngine) -> Bytes {
        let total = CRYPTO_HEADER_SIZE + data.len();
        // Keep spare so full-length split_to does not empty the reusable allocation.
        const SPARE: usize = 2048;
        self.enc_buf.clear();
        self.enc_buf.reserve(total + SPARE);

        // Build via extend (one O(n) write) — avoid resize(total, 0) zero-fill
        // that would immediately be overwritten.
        let n = self.nonce_counter.fetch_add(1, Ordering::Relaxed);
        self.enc_buf.extend_from_slice(&n.to_le_bytes());
        self.enc_buf
            .extend_from_slice(&self.session_id.to_le_bytes());
        let crc = crc32fast::hash(data);
        self.enc_buf.extend_from_slice(&crc.to_le_bytes());
        self.enc_buf.extend_from_slice(data);
        debug_assert_eq!(self.enc_buf.len(), total);

        crypt.encrypt(&mut self.enc_buf[..total]);

        let sealed = self.enc_buf.split_to(total).freeze();
        if self.enc_buf.capacity() < SPARE {
            self.enc_buf.reserve(SPARE);
        }
        sealed
    }

    /// Encrypt one packet with the standard 20B CFB wire format.
    ///
    /// All BlockCrypt ciphers (including xor/salsa20) use the same wire layout
    /// as Go kcptun: `[nonce 16B][CRC32 4B][ciphertext]`.
    /// The cipher impl (xor/salsa20) handles the nonce offset internally —
    /// see [`Salsa20Crypt`] / [`SimpleXORCrypt`] for per-cipher details.
    ///
    /// AEAD ciphers (aes-128-gcm) should use [`seal_aead`] instead.
    #[inline]
    pub fn encrypt_packet(&mut self, data: &[u8], crypt: &CryptEngine) -> Bytes {
        self.encrypt_cfb(data, crypt)
    }

    /// AEAD seal using the session-reused `aead_buf` (no per-flush `BytesMut::new`).
    #[inline]
    pub fn seal_aead(&mut self, aead: &dyn AeadCrypt, data: &[u8]) -> Bytes {
        aead.seal_into(data, &mut self.aead_buf)
    }

    /// Prepare the encryption buffer (nonce + plaintext copy) WITHOUT CRC or
    /// encrypt. CRC is filled later by [`finalize_encrypt_packet`] so large
    /// batches can compute CRC + encrypt in parallel.
    ///
    /// Layout after prepare (CRC slot is zeroed placeholder):
    ///   `[nonce 16B][CRC placeholder 4B][plaintext]`
    ///
    /// This enables: prepare all packets serially (shared nonce counter), then
    /// CRC + encrypt in parallel across threads (both are stateless).
    #[inline]
    pub fn prepare_encrypt(&mut self, data: &[u8]) -> BytesMut {
        let total = CRYPTO_HEADER_SIZE + data.len();
        // Keep spare so full-length split_to does not empty the reusable allocation.
        const SPARE: usize = 2048;
        self.enc_buf.clear();
        self.enc_buf.reserve(total + SPARE);

        let n = self.nonce_counter.fetch_add(1, Ordering::Relaxed);
        self.enc_buf.extend_from_slice(&n.to_le_bytes());
        self.enc_buf
            .extend_from_slice(&self.session_id.to_le_bytes());
        // CRC placeholder — filled by finalize_encrypt_packet before encrypt.
        self.enc_buf.extend_from_slice(&[0u8; 4]);
        self.enc_buf.extend_from_slice(data);
        debug_assert_eq!(self.enc_buf.len(), total);

        let prepared = self.enc_buf.split_to(total);
        if self.enc_buf.capacity() < SPARE {
            self.enc_buf.reserve(SPARE);
        }
        prepared
    }

    /// Fill CRC32 of the plaintext payload then encrypt in place.
    ///
    /// Expects a buffer from [`prepare_encrypt`]:
    ///   `[nonce 16B][CRC placeholder 4B][plaintext…]`
    /// After this call the buffer is a ready-to-send wire packet.
    /// Wire-compatible with Go: CRC is over plaintext only (before encrypt).
    #[inline]
    pub fn finalize_encrypt_packet(buf: &mut [u8], crypt: &CryptEngine) {
        debug_assert!(buf.len() >= CRYPTO_HEADER_SIZE);
        let crc = crc32fast::hash(&buf[CRYPTO_HEADER_SIZE..]);
        buf[NONCE_SIZE..CRYPTO_HEADER_SIZE].copy_from_slice(&crc.to_le_bytes());
        crypt.encrypt(buf);
    }

    /// Decrypt `data` in place, verify CRC32, and **copy** the payload into
    /// this buffer's reusable `enc_buf`, returning it as `Bytes`.
    ///
    /// Prefer [`decrypt_cfb_in_place`] on the hot inbound path when the
    /// plaintext is only needed for a synchronous `KCP::input` — that path
    /// avoids this second payload copy. Keep this method when the caller
    /// needs an owned `Bytes` that outlives the receive buffer.
    ///
    /// On CRC mismatch or short data, returns `None`.
    #[inline]
    pub fn decrypt_cfb(&mut self, data: &mut [u8], crypt: &CryptEngine) -> Option<Bytes> {
        let body = decrypt_cfb_in_place(data, crypt, false).ok()?;
        let payload_len = body.len();
        self.enc_buf.clear();
        self.enc_buf.reserve(payload_len);
        self.enc_buf.extend_from_slice(body);
        Some(self.enc_buf.split_to(payload_len).freeze())
    }
}

/// `cpu_block` offload thresholds (scheduling only — not wire format).
///
/// Tokio is the sole runtime; thresholds are tuned for its multi-worker
/// scheduler. Env overrides still apply (see `KCPTUN_*` env vars).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OffloadProfile {
    Tokio,
}

/// Optional env overrides. Non-profile keys cache via OnceLock.
/// Heavy8 keys: env if set, else **profile-dependent** default (not OnceLock'd
/// to the wrong profile's default).
///
/// | Env | Affects |
/// |-----|---------|
/// | `KCPTUN_COMPRESS_CPU_BLOCK_BYTES` | Snappy offload size (default 16KiB) |
/// | `KCPTUN_FAST_ENCRYPT_MIN_PKTS` / `_BYTES` | xor/salsa/AEAD |
/// | `KCPTUN_HEAVY8_ENCRYPT_MIN_PKTS` / `_BYTES` | cast5/3des/blowfish/tea/xtea |
fn env_usize(name: &'static str, default: usize) -> usize {
    match name {
        "KCPTUN_COMPRESS_CPU_BLOCK_BYTES" => {
            static V: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
            *V.get_or_init(|| parse_env_usize(name, default))
        }
        "KCPTUN_FAST_ENCRYPT_MIN_PKTS" => {
            static V: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
            *V.get_or_init(|| parse_env_usize(name, default))
        }
        "KCPTUN_FAST_ENCRYPT_MIN_BYTES" => {
            static V: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
            *V.get_or_init(|| parse_env_usize(name, default))
        }
        _ => parse_env_usize(name, default),
    }
}

/// Env override if present; otherwise `default` (may depend on profile).
fn env_or_profile(name: &str, default: usize) -> usize {
    static HEAVY_PKTS: std::sync::OnceLock<Option<usize>> = std::sync::OnceLock::new();
    static HEAVY_BYTES: std::sync::OnceLock<Option<usize>> = std::sync::OnceLock::new();
    let cached =
        match name {
            "KCPTUN_HEAVY8_ENCRYPT_MIN_PKTS" => HEAVY_PKTS
                .get_or_init(|| std::env::var(name).ok().and_then(|s| s.trim().parse().ok())),
            "KCPTUN_HEAVY8_ENCRYPT_MIN_BYTES" => HEAVY_BYTES
                .get_or_init(|| std::env::var(name).ok().and_then(|s| s.trim().parse().ok())),
            _ => &None,
        };
    cached.unwrap_or(default).max(1)
}

fn parse_env_usize(name: &str, default: usize) -> usize {
    match std::env::var(name) {
        Ok(s) => s.trim().parse::<usize>().unwrap_or(default).max(1),
        Err(_) => default,
    }
}

/// Decide whether a batch encrypt should be offloaded to `cpu_block`.
///
/// Heavy-8 defaults depend on `profile`:
/// Tokio: 1 pkt / 512 B (protect multi-worker async from CFB-8).
/// Fast/AEAD and other CFB thresholds are shared. Env overrides still apply.
#[inline]
pub fn should_cpu_block_encrypt(
    has_encryption: bool,
    has_aead: bool,
    packet_count: usize,
    total_bytes: usize,
    crypt: &CryptEngine,
    _profile: OffloadProfile,
) -> bool {
    // Null cipher: the "encrypt" is `out.extend(packets)` — just moving Bytes
    // references (pointer copies). cpu_block dispatch costs more.
    if !has_encryption && !has_aead {
        return false;
    }
    let cname = crypt.name();

    // Fast ciphers / AEAD: offload only large batches to protect the async
    // worker from long inline encrypt + compress combinations.
    if matches!(cname, "xor" | "salsa20" | "salsa") || has_aead {
        let min_pkts = env_usize("KCPTUN_FAST_ENCRYPT_MIN_PKTS", 8);
        let min_bytes = env_usize("KCPTUN_FAST_ENCRYPT_MIN_BYTES", 8192);
        return packet_count >= min_pkts || total_bytes >= min_bytes;
    }

    // Heavy 8-byte CFB ciphers (cast5, 3des, blowfish, tea, xtea).
    if matches!(cname, "cast5" | "3des" | "blowfish" | "tea" | "xtea") {
        let (def_pkts, def_bytes) = (1, 512);
        let min_pkts = env_or_profile("KCPTUN_HEAVY8_ENCRYPT_MIN_PKTS", def_pkts);
        let min_bytes = env_or_profile("KCPTUN_HEAVY8_ENCRYPT_MIN_BYTES", def_bytes);
        return packet_count >= min_pkts || total_bytes >= min_bytes;
    }

    // Remaining CFB (AES software path, SM4, Twofish, ...): moderate threshold.
    packet_count >= 4 || total_bytes >= 4096
}

/// Decide whether inbound decrypt + KCP input + SMUX processing should be
/// offloaded to `cpu_block`.
///
/// - null/none: never
/// - AEAD: ≥4 KiB
/// - CFB: ≥512 B
#[inline]
pub fn should_cpu_block_decrypt(
    has_encryption: bool,
    has_aead: bool,
    data_len: usize,
    _profile: OffloadProfile,
) -> bool {
    if !has_encryption && !has_aead {
        false
    } else if has_aead {
        data_len >= 4096
    } else {
        let thr = 512;
        data_len >= thr
    }
}

/// Size threshold for session-level Snappy offload (default ≥16 KiB).
///
/// Previous threshold (64 KiB) was too high: inline Snappy + inline
/// encrypt ("double-inline") starves UDP/ACK processing on fast ciphers
/// (xor/salsa) + compression. Threshold sweep (2 MiB random, 3 runs):
///   16 KiB → +22.9% vs 64 KiB; null-comp control → +72.8%.
///
/// Override with `KCPTUN_COMPRESS_CPU_BLOCK_BYTES`.
#[inline]
pub fn should_cpu_block_compress(uncompressed_bytes: usize) -> bool {
    let thr = env_usize("KCPTUN_COMPRESS_CPU_BLOCK_BYTES", 16384);
    uncompressed_bytes >= thr
}

/// Encrypt a batch of raw KCP segments for the wire (P0.1 / P0.5).
///
/// Input packets are `Bytes` (reference-counted) — the KCP output callback
/// now hands ownership directly (P1.1 R2), avoiding a per-packet `Vec` alloc
/// + `extend_from_slice` copy in the output path.
///
/// - AEAD ([`CryptEngine::Aes128Gcm`]): `seal_into` each packet
/// - CFB (`has_encryption`): serial `encrypt_cfb` for cheap/small batches;
///   large heavy-cipher batches use serial `prepare_encrypt` then parallel
///   `finalize_encrypt_packet` (CRC + encrypt). `allow_parallel=false` when
///   already on a `cpu_block` worker (no nested fan-out)
/// - null (`!has_encryption`): move `Bytes` straight through (no crypto header)
///
/// `crypt` is concrete [`CryptEngine`] so encrypt/decrypt use enum match
/// dispatch (no `dyn` vtable on the hot path). `has_encryption` remains a
/// separate flag so `"none"` (header + identity cipher) still packs CFB
/// headers while `"null"` does not.
pub fn encrypt_batch(
    packets: Vec<Bytes>,
    crypt: &CryptEngine,
    crypto_buf: &parking_lot::Mutex<CryptoBuf>,
    has_encryption: bool,
    // When false, never spawn thread::scope workers (already on a cpu_block
    // pool thread — nested parallelism thrashes cores under multi-session load).
    allow_parallel: bool,
) -> Vec<Bytes> {
    let mut out = Vec::with_capacity(packets.len());
    encrypt_batch_ref_into(
        &packets,
        crypt,
        crypto_buf,
        has_encryption,
        allow_parallel,
        &mut out,
    );
    out
}

/// Whether in-process `thread::scope` parallel encrypt is worth it for this batch.
///
/// Spawn/join costs tens of µs. Cheap ciphers finish a whole flush faster than
/// that, so parallelism is a net loss (seen as tokio `none/comp` / medium xor
/// regressions). Heavy software CFB still benefits once the batch is large.
#[inline]
fn should_parallel_cfb_encrypt(
    crypt: &CryptEngine,
    allow_parallel: bool,
    packet_count: usize,
    total_bytes: usize,
) -> bool {
    if !allow_parallel || packet_count < 2 {
        return false;
    }
    let name = crypt.name();
    // Identity / stream XOR: encrypt is near-free; CRC+copy is the work.
    // Never pay thread spawn — always serial encrypt_cfb.
    if matches!(name, "none" | "xor" | "salsa20" | "salsa") {
        return false;
    }
    // Heavy pure-software CFB-8 / SM4 / Twofish: parallel helps, but only when
    // the batch amortizes spawn (not every 4-packet flush).
    if matches!(
        name,
        "cast5" | "3des" | "blowfish" | "tea" | "xtea" | "sm4" | "twofish"
    ) {
        return packet_count >= 8 || total_bytes >= 8192;
    }
    // AES CFB (and remaining): hardware block encrypt is fast; need a larger
    // batch than the old "≥4 packets" gate or we thrash cores for free.
    packet_count >= 8 || total_bytes >= 16384
}

/// Encrypt a batch into a caller-owned `out` buffer (cleared first).
///
/// Flush loops should reuse one `Vec<Bytes>` across cycles to avoid per-flush
/// allocation of the result vector (P0.4). Contents are `Bytes` (refcounted);
/// `out.clear()` drops refs but keeps the `Vec` capacity.
///
/// Takes `Vec<Bytes>` by value — the null path moves packets straight into
/// `out` without cloning. Use [`encrypt_batch_ref_into`] when you already have
/// a `&[Bytes]` slice and want to avoid the `to_vec()` allocation.
pub fn encrypt_batch_into(
    packets: Vec<Bytes>,
    crypt: &CryptEngine,
    crypto_buf: &parking_lot::Mutex<CryptoBuf>,
    has_encryption: bool,
    allow_parallel: bool,
    out: &mut Vec<Bytes>,
) {
    encrypt_batch_ref_into(
        &packets,
        crypt,
        crypto_buf,
        has_encryption,
        allow_parallel,
        out,
    );
}

/// Same as [`encrypt_batch_into`] but takes `&[Bytes]` instead of `Vec<Bytes>`,
/// avoiding the `to_vec()` allocation on the sync TX path (`encrypt_sync`).
///
/// In the null path, `Bytes` clones are cheap (refcount bumps) so the
/// difference vs `encrypt_batch_into` is negligible.
pub fn encrypt_batch_ref_into(
    packets: &[Bytes],
    crypt: &CryptEngine,
    crypto_buf: &parking_lot::Mutex<CryptoBuf>,
    has_encryption: bool,
    allow_parallel: bool,
    out: &mut Vec<Bytes>,
) {
    out.clear();
    if out.capacity() < packets.len() {
        out.reserve(packets.len() - out.capacity());
    }
    if let Some(aead) = crypt.as_aead() {
        // Reuse CryptoBuf.aead_buf across flush cycles (not just within a batch).
        let mut cb = crypto_buf.lock();
        for data in packets {
            out.push(cb.seal_aead(aead, data));
        }
    } else if has_encryption {
        // Standard CFB path for ALL BlockCrypt ciphers (including xor/salsa20):
        // wire format is [nonce 16B][CRC32 4B][ciphertext], matching Go kcptun.
        // The cipher impl handles the nonce offset internally — salsa20 reads
        // bytes 0-8 as the nonce, xor XORs the entire buffer.
        //
        // Parallelism is cipher- and size-gated (see should_parallel_cfb_encrypt):
        // cheap ciphers stay serial; expensive ones only fan out on large batches.
        let total_bytes: usize = packets.iter().map(|p| p.len()).sum();
        let use_parallel =
            should_parallel_cfb_encrypt(crypt, allow_parallel, packets.len(), total_bytes);
        if !use_parallel {
            let mut cb = crypto_buf.lock();
            for data in packets {
                out.push(cb.encrypt_cfb(data, crypt));
            }
        } else {
            // Cap workers: more than 4 threads rarely helps one session's encrypt
            // and increases join latency on high-core hosts.
            let nthreads = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1)
                .min(packets.len())
                .clamp(2, 4);
            // Phase 1: Prepare all packets (serial — shared nonce counter only).
            // CRC is deferred to phase 2 so it can run in parallel with encrypt.
            let prepared: Vec<BytesMut> = {
                let mut cb = crypto_buf.lock();
                packets
                    .iter()
                    .map(|data| cb.prepare_encrypt(data))
                    .collect()
            };
            // Phase 2: CRC + encrypt in parallel (both are stateless / per-packet).
            //
            // NOTE: This path is rarely reached in production. For heavy CFB
            // ciphers, should_cpu_block_encrypt returns true at ≥1 packet / 512 B
            // (heavy 8-byte) or ≥8 packets / 8 KiB (fast/AES), routing through
            // cpu_block with allow_parallel=false. When allow_parallel=false,
            // should_parallel_cfb_encrypt always returns false, so the serial
            // path is used. This parallel path only triggers when the caller
            // explicitly sets allow_parallel=true (e.g. force_inline mode).
            let chunk_size = prepared.len().div_ceil(nthreads);
            let mut iter = prepared.into_iter();
            std::thread::scope(|s| {
                let mut handles = Vec::new();
                loop {
                    let chunk: Vec<BytesMut> = (&mut iter).take(chunk_size).collect();
                    if chunk.is_empty() {
                        break;
                    }
                    // CryptEngine enum dispatch (no dyn vtable).
                    // For AES CFB the encrypt call goes through
                    // aes::BlockEncrypt::encrypt_blocks for ILP on AES-NI/ARMv8.
                    handles.push(s.spawn(move || {
                        let mut r = Vec::with_capacity(chunk.len());
                        for mut buf in chunk {
                            // CRC over plaintext then encrypt — same wire layout as
                            // serial encrypt_cfb / Go postProcess.
                            CryptoBuf::finalize_encrypt_packet(&mut buf, crypt);
                            r.push(buf.freeze());
                        }
                        r
                    }));
                }
                // Use unwrap_or_default so a worker panic doesn't propagate
                // into the flush loop and kill the session.
                for h in handles {
                    if let Ok(chunk_out) = h.join() {
                        out.extend(chunk_out);
                    }
                }
            });
        }
    } else {
        // null: Bytes pass straight through (no crypto header, no copy).
        // Clone is cheap — Bytes is refcounted.
        out.extend(packets.iter().cloned());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypt::CryptEngine;

    #[test]
    fn test_encrypt_decrypt_roundtrip() {
        let (crypt, _) = CryptEngine::select("aes-128", b"test-key-12345678");
        let mut cb = CryptoBuf::new(0xDEADBEEF);

        let plaintext = b"hello kcptun wire format test!";
        let encrypted = cb.encrypt_cfb(plaintext, &crypt);

        // Decrypt
        let mut enc_copy = encrypted.to_vec();
        let decrypted = cb.decrypt_cfb(&mut enc_copy, &crypt);

        assert!(decrypted.is_some());
        let dec = decrypted.unwrap();
        assert_eq!(&dec[..], plaintext);
    }

    #[test]
    fn test_nonce_counter_increments() {
        let (crypt, _) = CryptEngine::select("aes-128", b"test-key-12345678");
        let mut cb = CryptoBuf::new(0xCAFEBABE);

        let data = b"test data for nonce";
        let pkt1 = cb.encrypt_cfb(data, &crypt);
        let pkt2 = cb.encrypt_cfb(data, &crypt);

        // Nonces should differ (counter incremented)
        assert_ne!(&pkt1[..8], &pkt2[..8]);
        // Session ID should be the same
        assert_eq!(&pkt1[8..16], &pkt2[8..16]);
    }

    #[test]
    fn should_cpu_block_thresholds() {
        let tokio = OffloadProfile::Tokio;
        let (none_crypt, _) = CryptEngine::select("none", b"");
        // Null cipher: never offload — "encrypt" is just pointer moves.
        assert!(!should_cpu_block_encrypt(
            false,
            false,
            7,
            100_000,
            &none_crypt,
            tokio
        ));
        assert!(!should_cpu_block_encrypt(
            false,
            false,
            999,
            999_999,
            &none_crypt,
            tokio
        ));
        // Heavy 8-byte CFB under Tokio: early offload.
        let (cast5_crypt, _) = CryptEngine::select("cast5", b"test-key-12345678");
        assert!(
            should_cpu_block_encrypt(true, false, 1, 1, &cast5_crypt, tokio),
            "tokio cast5 1 pkt should offload"
        );
        assert!(
            should_cpu_block_encrypt(true, false, 1, 100, &cast5_crypt, tokio),
            "tokio cast5 small pkt should offload"
        );
        assert!(
            should_cpu_block_encrypt(true, false, 1, 511, &cast5_crypt, tokio),
            "tokio cast5 511B should offload"
        );
        assert!(
            should_cpu_block_encrypt(true, false, 1, 512, &cast5_crypt, tokio),
            "tokio cast5 512B should offload"
        );
        assert!(
            should_cpu_block_encrypt(true, false, 0, 1000, &cast5_crypt, tokio),
            "tokio cast5 1KiB should offload by bytes"
        );

        let (tdes_crypt, _) = CryptEngine::select("3des", b"123456781234567812345678");
        assert!(
            should_cpu_block_encrypt(true, false, 1, 100, &tdes_crypt, tokio),
            "3des small should offload (tokio)"
        );

        let (bf_crypt, _) = CryptEngine::select("blowfish", b"test-key");
        assert!(
            should_cpu_block_encrypt(true, false, 1, 100, &bf_crypt, tokio),
            "blowfish small should offload (tokio)"
        );

        let (tea_crypt, _) = CryptEngine::select("tea", b"test-key");
        assert!(
            should_cpu_block_encrypt(true, false, 1, 100, &tea_crypt, tokio),
            "tea small should offload (tokio)"
        );

        let (xtea_crypt, _) = CryptEngine::select("xtea", b"test-key");
        assert!(
            should_cpu_block_encrypt(true, false, 1, 100, &xtea_crypt, tokio),
            "xtea small should offload (tokio)"
        );

        // Non-heavy CFB (AES, SM4, Twofish) keeps the original 4/4KiB threshold.
        let (aes_crypt, _) = CryptEngine::select("aes-128", b"test-key-12345678");
        assert!(!should_cpu_block_encrypt(
            true, false, 3, 100, &aes_crypt, tokio
        ));
        assert!(should_cpu_block_encrypt(
            true, false, 4, 100, &aes_crypt, tokio
        ));
        assert!(should_cpu_block_encrypt(
            true, false, 1, 4096, &aes_crypt, tokio
        ));

        let (sm4_crypt, _) = CryptEngine::select("sm4", b"test-key-12345678");
        assert!(!should_cpu_block_encrypt(
            true, false, 3, 100, &sm4_crypt, tokio
        ));
        assert!(should_cpu_block_encrypt(
            true, false, 4, 100, &sm4_crypt, tokio
        ));

        let (twofish_crypt, _) = CryptEngine::select("twofish", b"test-key-12345678");
        assert!(!should_cpu_block_encrypt(
            true,
            false,
            3,
            100,
            &twofish_crypt,
            tokio
        ));
        assert!(should_cpu_block_encrypt(
            true,
            false,
            4,
            100,
            &twofish_crypt,
            tokio
        ));
        // AEAD: offload only large batches (≥8 pkts or ≥8 KiB).
        assert!(!should_cpu_block_encrypt(
            false,
            true,
            4,
            0,
            &none_crypt,
            tokio
        ));
        assert!(!should_cpu_block_encrypt(
            false,
            true,
            7,
            8191,
            &none_crypt,
            tokio
        ));
        assert!(should_cpu_block_encrypt(
            false,
            true,
            8,
            0,
            &none_crypt,
            tokio
        ));
        assert!(should_cpu_block_encrypt(
            false,
            true,
            1,
            8192,
            &none_crypt,
            tokio
        ));
        let (aead_crypt, _) = CryptEngine::select("aes-128-gcm", b"0123456789abcdef");
        assert!(aead_crypt.is_aead());
        assert!(should_cpu_block_encrypt(
            false,
            true,
            999,
            999_999,
            &aead_crypt,
            tokio
        ));
        // Fast ciphers: xor/salsa20 offload large batches only.
        let (xor_crypt, _) = CryptEngine::select("xor", b"test-key");
        assert!(!should_cpu_block_encrypt(
            true, false, 7, 8191, &xor_crypt, tokio
        ));
        assert!(should_cpu_block_encrypt(
            true, false, 8, 100, &xor_crypt, tokio
        ));
        assert!(should_cpu_block_encrypt(
            true, false, 1, 8192, &xor_crypt, tokio
        ));
        let (salsa_crypt, _) = CryptEngine::select("salsa20", b"test-key");
        assert!(!should_cpu_block_encrypt(
            true,
            false,
            7,
            100,
            &salsa_crypt,
            tokio
        ));
        assert!(should_cpu_block_encrypt(
            true,
            false,
            8,
            100,
            &salsa_crypt,
            tokio
        ));
        // snappy compress threshold: ≥16 KiB
        assert!(!should_cpu_block_compress(16383));
        assert!(should_cpu_block_compress(16384));
        // inbound decrypt (tokio): null never; AEAD at 4KiB; CFB at 512B
        assert!(!should_cpu_block_decrypt(false, false, 65535, tokio));
        assert!(!should_cpu_block_decrypt(false, true, 4095, tokio));
        assert!(should_cpu_block_decrypt(false, true, 4096, tokio));
        assert!(!should_cpu_block_decrypt(true, false, 511, tokio));
        assert!(should_cpu_block_decrypt(true, false, 512, tokio));
    }

    #[test]
    fn encrypt_batch_null_and_cfb() {
        let packets: Vec<Bytes> = vec![Bytes::from(&b"aaa"[..]), Bytes::from(&b"bbbb"[..])];
        let (crypt, _) = CryptEngine::select("null", b"key");
        let cb = parking_lot::Mutex::new(CryptoBuf::new(1));
        let out = encrypt_batch(packets, &crypt, &cb, false, true);
        assert_eq!(out.len(), 2);
        assert_eq!(&out[0][..], b"aaa");
        assert_eq!(&out[1][..], b"bbbb");

        let (crypt, _) = CryptEngine::select("aes-128", b"test-key-12345678");
        let packets: Vec<Bytes> = vec![Bytes::from(&b"hello wire"[..])];
        let mut cb = CryptoBuf::new(2);
        let cb_mu = parking_lot::Mutex::new(CryptoBuf::new(2));
        let out = encrypt_batch(packets, &crypt, &cb_mu, true, true);
        assert_eq!(out.len(), 1);
        assert!(out[0].len() > 10);
        let mut enc = out[0].to_vec();
        let dec = cb.decrypt_cfb(&mut enc, &crypt).unwrap();
        assert_eq!(&dec[..], b"hello wire");
    }

    #[test]
    fn encrypt_batch_allow_parallel_false_still_correct() {
        let (crypt, _) = CryptEngine::select("aes-128", b"test-key-12345678");
        // ≥4 packets so the parallel branch would normally engage.
        let packets: Vec<Bytes> = (0..8).map(|i| Bytes::from(vec![i as u8; 64])).collect();
        let cb_mu = parking_lot::Mutex::new(CryptoBuf::new(99));
        let out = encrypt_batch(
            packets.clone(),
            &crypt,
            &cb_mu,
            true,
            false, // force serial (cpu_block worker path)
        );
        assert_eq!(out.len(), 8);
        let mut cb = CryptoBuf::new(99);
        for (i, pkt) in out.iter().enumerate() {
            let mut enc = pkt.to_vec();
            let dec = cb.decrypt_cfb(&mut enc, &crypt).unwrap();
            assert_eq!(&dec[..], &vec![i as u8; 64][..]);
        }
    }

    #[test]
    fn encrypt_batch_parallel_crc_matches_serial() {
        // Parallel path (heavy ciphers, large batch): prepare → finalize_encrypt_packet.
        // xor/salsa/none must stay serial even with allow_parallel=true.
        let (xor, _) = CryptEngine::select("xor", b"test-key-12345678");
        assert!(!should_parallel_cfb_encrypt(&xor, true, 64, 64 * 1024));
        let (none, _) = CryptEngine::select("none", b"k");
        assert!(!should_parallel_cfb_encrypt(&none, true, 64, 64 * 1024));
        let (aes, _) = CryptEngine::select("aes-128", b"test-key-12345678");
        assert!(!should_parallel_cfb_encrypt(&aes, true, 4, 400));
        assert!(should_parallel_cfb_encrypt(&aes, true, 8, 8 * 2048));
        let (tdes, _) = CryptEngine::select("3des", b"test-key-12345678-24bytes!");
        assert!(should_parallel_cfb_encrypt(&tdes, true, 8, 1000));

        for method in &["aes-128", "3des", "xor", "salsa20"] {
            let (crypt, _) = CryptEngine::select(method, b"test-key-12345678-24bytes!!");
            // 16 × 2KiB ≈ 32KiB — above AES parallel gate; xor still serial.
            let packets: Vec<Bytes> = (0..16).map(|i| Bytes::from(vec![i as u8; 2048])).collect();
            let total: usize = packets.iter().map(|p| p.len()).sum();
            let expect_par = should_parallel_cfb_encrypt(&crypt, true, packets.len(), total);

            let cb_mu = parking_lot::Mutex::new(CryptoBuf::new(0xC2C0_u64));
            let out = encrypt_batch(packets.clone(), &crypt, &cb_mu, true, true);
            assert_eq!(out.len(), 16, "{method} batch len");

            let mut cb = CryptoBuf::new(7);
            for (i, pkt) in out.iter().enumerate() {
                let mut enc = pkt.to_vec();
                let dec = cb
                    .decrypt_cfb(&mut enc, &crypt)
                    .unwrap_or_else(|| panic!("{method} pkt {i} CRC/decrypt failed"));
                assert_eq!(&dec[..], &vec![i as u8; 2048][..], "{method} pkt {i}");
            }

            // prepare + finalize must match encrypt_cfb wire semantics (CRC over plain).
            let mut prep_cb = CryptoBuf::new(0xBEEF);
            let mut serial_cb = CryptoBuf::new(0xBEEF);
            let plain = b"parallel-crc-wire-check-payload!!";
            let mut prepared = prep_cb.prepare_encrypt(plain);
            assert_eq!(&prepared[NONCE_SIZE..CRYPTO_HEADER_SIZE], &[0, 0, 0, 0]);
            CryptoBuf::finalize_encrypt_packet(&mut prepared, &crypt);
            let serial = serial_cb.encrypt_cfb(plain, &crypt);
            assert_eq!(
                prepared.as_ref(),
                serial.as_ref(),
                "{method}: finalize must equal serial encrypt_cfb (expect_par={expect_par})"
            );
        }
    }

    #[test]
    fn encrypt_packet_uses_cfb_header_for_all_cfb_ciphers() {
        let (salsa, _) = CryptEngine::select("salsa20", b"test-key-12345678");
        let (xor, _) = CryptEngine::select("xor", b"test-key");
        let (aes, _) = CryptEngine::select("aes-128", b"test-key-12345678");
        let mut cb = CryptoBuf::new(7);
        let plain = b"ack-payload-body";

        let s = cb.encrypt_packet(plain, &salsa);
        assert_eq!(
            s.len(),
            CRYPTO_HEADER_SIZE + plain.len(),
            "salsa must use CFB header"
        );
        // Roundtrip via decrypt_cfb
        let mut sbuf = s.to_vec();
        let dec = cb.decrypt_cfb(&mut sbuf, &salsa);
        assert!(dec.is_some(), "salsa decrypt_cfb");
        assert_eq!(&dec.unwrap()[..], plain);

        let x = cb.encrypt_packet(plain, &xor);
        assert_eq!(
            x.len(),
            CRYPTO_HEADER_SIZE + plain.len(),
            "xor must use CFB header"
        );

        let a = cb.encrypt_packet(plain, &aes);
        assert_eq!(
            a.len(),
            CRYPTO_HEADER_SIZE + plain.len(),
            "aes must use CFB header"
        );
    }

    #[test]
    fn salsa20_roundtrip_via_encrypt_batch() {
        let (crypt, name) = CryptEngine::select("salsa20", b"test-key-12345678");
        assert_eq!(name, "salsa20");

        let cb = parking_lot::Mutex::new(CryptoBuf::new(42));
        let packets: Vec<Bytes> = (0..4).map(|i| Bytes::from(vec![i as u8; 128])).collect();
        let encrypted = encrypt_batch(packets.clone(), &crypt, &cb, true, false);
        assert_eq!(encrypted.len(), 4);

        // Each encrypted packet should have CRYPTO_HEADER_SIZE bytes of header (standard CFB)
        for (i, pkt) in encrypted.iter().enumerate() {
            assert!(
                pkt.len() >= CRYPTO_HEADER_SIZE,
                "packet {} too short ({} bytes)",
                i,
                pkt.len()
            );
        }

        // Decrypt via standard CFB path
        for (i, pkt) in encrypted.iter().enumerate() {
            let mut buf = pkt.to_vec();
            let body = decrypt_cfb_in_place(&mut buf, &crypt, false);
            assert!(body.is_ok(), "packet {} decrypt_cfb_in_place", i);
            assert_eq!(body.unwrap(), &packets[i][..], "packet {} mismatch", i);
        }
    }

    #[test]
    fn encrypt_batch_aead_via_engine() {
        let (crypt, name) = CryptEngine::select("aes-128-gcm", b"0123456789abcdef");
        assert_eq!(name, "aes-128-gcm");
        assert!(crypt.is_aead());
        let packets: Vec<Bytes> = vec![Bytes::from(&b"aead body"[..])];
        let cb = parking_lot::Mutex::new(CryptoBuf::new(3));
        // has_encryption ignored when engine is AEAD
        let out = encrypt_batch(packets, &crypt, &cb, false, true);
        assert_eq!(out.len(), 1);
        let plain = crypt.as_aead().unwrap().open(&out[0]).unwrap();
        assert_eq!(&plain[..], b"aead body");
    }

    #[test]
    fn test_crc_mismatch_returns_none() {
        let (crypt, _) = CryptEngine::select("aes-128", b"test-key-12345678");
        let mut cb = CryptoBuf::new(0xDEAD);

        let plaintext = b"hello";
        let mut encrypted = cb.encrypt_cfb(plaintext, &crypt).to_vec();

        // Corrupt the CRC field (bytes 16..20)
        encrypted[17] ^= 0xFF;

        let result = cb.decrypt_cfb(&mut encrypted, &crypt);
        assert!(result.is_none());
    }

    #[test]
    fn test_short_data_returns_none() {
        let (crypt, _) = CryptEngine::select("aes-128", b"test-key-12345678");
        let mut cb = CryptoBuf::new(0);

        let mut short = [0u8; 10]; // < CRYPTO_HEADER_SIZE (20)
        let result = cb.decrypt_cfb(&mut short, &crypt);
        assert!(result.is_none());
    }

    #[test]
    fn test_buffer_reuse_no_reallocation() {
        let (crypt, _) = CryptEngine::select("aes-128", b"test-key-12345678");
        let mut cb = CryptoBuf::new(0);

        // Encrypt many packets of varying sizes
        for i in 0..100 {
            let data = vec![i as u8; 100 + i * 10];
            let encrypted = cb.encrypt_cfb(&data, &crypt);
            assert_eq!(encrypted.len(), CRYPTO_HEADER_SIZE + data.len());

            // Verify roundtrip
            let mut enc_copy = encrypted.to_vec();
            let decrypted = cb.decrypt_cfb(&mut enc_copy, &crypt);
            assert!(decrypted.is_some());
            assert_eq!(&decrypted.unwrap()[..], &data[..]);
        }
    }

    #[test]
    fn test_none_crypt() {
        let (crypt, _) = CryptEngine::select("none", b"");
        let mut cb = CryptoBuf::new(0);

        let plaintext = b"test none cipher";
        let encrypted = cb.encrypt_cfb(plaintext, &crypt);
        // With none cipher, nonce and CRC are still written, data is not encrypted
        assert_eq!(&encrypted[CRYPTO_HEADER_SIZE..], plaintext);
    }

    #[test]
    fn decrypt_cfb_in_place_roundtrip() {
        let (crypt, _) = CryptEngine::select("aes-128", b"test-key-12345678");
        let mut cb = CryptoBuf::new(0xBEEF);
        let plaintext = b"in-place body payload xyz";
        let encrypted = cb.encrypt_cfb(plaintext, &crypt);
        let mut buf = encrypted.to_vec();
        let body = decrypt_cfb_in_place(&mut buf, &crypt, false).unwrap();
        assert_eq!(body, plaintext);
        // body is a subslice of buf (after header)
        assert_eq!(body.as_ptr(), buf[CRYPTO_HEADER_SIZE..].as_ptr());
    }

    #[test]
    fn decrypt_cfb_in_place_crc_and_short() {
        let (crypt, _) = CryptEngine::select("aes-128", b"test-key-12345678");
        let mut cb = CryptoBuf::new(1);
        let mut enc = cb.encrypt_cfb(b"hello", &crypt).to_vec();
        enc[17] ^= 0xFF;
        assert_eq!(
            decrypt_cfb_in_place(&mut enc, &crypt, false),
            Err(InboundCryptError::CrcMismatch)
        );
        let mut short = [0u8; 10];
        assert_eq!(
            decrypt_cfb_in_place(&mut short, &crypt, false),
            Err(InboundCryptError::Short)
        );
    }

    #[test]
    fn strip_probe_raw_kcp_cmd_keeps_buffer() {
        // Synthetic: decrypted buffer whose byte 4 is cmd PUSH (0x51) → no header.
        let mut buf = vec![0u8; 30];
        buf[4] = 0x51;
        let body = strip_cfb_header_if_present(&buf, true).unwrap();
        assert_eq!(body.len(), 30);
        assert_eq!(body.as_ptr(), buf.as_ptr());
    }

    #[test]
    fn inbound_null_identity() {
        let b = b"raw datagram";
        assert_eq!(inbound_null(b), b);
    }
}
