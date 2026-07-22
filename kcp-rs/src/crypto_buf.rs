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

use crate::crypt::{AeadCrypt, BlockCrypt};

/// Crypto header size: `[nonce 16B][CRC32 4B]`.
pub const CRYPT_HDR: usize = 20;
/// Nonce size.
pub const NONCE_SZ: usize = 16;

/// Inbound CFB decrypt / header-strip failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboundCryptError {
    /// Packet shorter than crypto header (or empty after probe).
    Short,
    /// CRC32 of payload does not match the header field.
    CrcMismatch,
}

/// Decrypt a CFB wire packet **in place** and return a slice of the plaintext body.
///
/// Layout after decrypt: `[nonce 16B][CRC32 4B][payload…]`. On success the
/// returned slice is `&buf[CRYPT_HDR..]` — **no** heap alloc and **no** copy
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
    crypt: &dyn BlockCrypt,
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
        if buf.len() > CRYPT_HDR {
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
    } else if buf.len() <= CRYPT_HDR {
        return Err(InboundCryptError::Short);
    }

    let stored_crc = u32::from_le_bytes(
        buf[NONCE_SZ..CRYPT_HDR]
            .try_into()
            .map_err(|_| InboundCryptError::Short)?,
    );
    let computed_crc = crc32fast::hash(&buf[CRYPT_HDR..]);
    if stored_crc != computed_crc {
        return Err(InboundCryptError::CrcMismatch);
    }
    Ok(&buf[CRYPT_HDR..])
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
pub struct CryptoBuf {
    /// Reusable encryption buffer — capacity is retained across calls.
    enc_buf: BytesMut,
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
    #[inline]
    pub fn encrypt_cfb(&mut self, data: &[u8], crypt: &dyn BlockCrypt) -> Bytes {
        let total = CRYPT_HDR + data.len();
        // Keep spare so full-length split_to does not empty the reusable allocation.
        const SPARE: usize = 2048;
        self.enc_buf.reserve(total + SPARE);
        self.enc_buf.clear();
        self.enc_buf.resize(total, 0);

        // ── Fill nonce (counter + session_id, no PRNG call) ──
        let n = self.nonce_counter.fetch_add(1, Ordering::Relaxed);
        self.enc_buf[..8].copy_from_slice(&n.to_le_bytes());
        self.enc_buf[8..NONCE_SZ].copy_from_slice(&self.session_id.to_le_bytes());

        // ── Fill CRC32 of the plaintext ──
        let crc = crc32fast::hash(data);
        self.enc_buf[NONCE_SZ..CRYPT_HDR].copy_from_slice(&crc.to_le_bytes());

        // ── Copy plaintext into buffer (unavoidable — must be contiguous for CFB) ──
        self.enc_buf[CRYPT_HDR..].copy_from_slice(data);

        // ── Encrypt in place ──
        crypt.encrypt(&mut self.enc_buf[..total]);

        // ── Extract as Bytes; warm leftover capacity for next call ──
        let sealed = self.enc_buf.split_to(total).freeze();
        if self.enc_buf.capacity() < SPARE {
            self.enc_buf.reserve(SPARE);
        }
        sealed
    }

    /// Prepare the encryption buffer (nonce + CRC32 + plaintext copy) WITHOUT
    /// encrypting. The returned `BytesMut` can be encrypted separately via
    /// `crypt.encrypt(&mut buf)`.
    ///
    /// This enables parallel encryption: prepare all packets serially (using
    /// the shared nonce counter), then encrypt them in parallel across threads
    /// (the cipher is stateless and thread-safe).
    #[inline]
    pub fn prepare_encrypt(&mut self, data: &[u8]) -> BytesMut {
        let total = CRYPT_HDR + data.len();
        // Keep spare so full-length split_to does not empty the reusable allocation.
        const SPARE: usize = 2048;
        self.enc_buf.reserve(total + SPARE);
        self.enc_buf.clear();
        self.enc_buf.resize(total, 0);

        let n = self.nonce_counter.fetch_add(1, Ordering::Relaxed);
        self.enc_buf[..8].copy_from_slice(&n.to_le_bytes());
        self.enc_buf[8..NONCE_SZ].copy_from_slice(&self.session_id.to_le_bytes());

        let crc = crc32fast::hash(data);
        self.enc_buf[NONCE_SZ..CRYPT_HDR].copy_from_slice(&crc.to_le_bytes());

        self.enc_buf[CRYPT_HDR..].copy_from_slice(data);

        let prepared = self.enc_buf.split_to(total);
        if self.enc_buf.capacity() < SPARE {
            self.enc_buf.reserve(SPARE);
        }
        prepared
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
    pub fn decrypt_cfb(&mut self, data: &mut [u8], crypt: &dyn BlockCrypt) -> Option<Bytes> {
        let body = decrypt_cfb_in_place(data, crypt, false).ok()?;
        let payload_len = body.len();
        self.enc_buf.clear();
        self.enc_buf.resize(payload_len, 0);
        self.enc_buf.copy_from_slice(body);
        Some(self.enc_buf.split_to(payload_len).freeze())
    }
}

/// Decide whether a batch encrypt should be offloaded to `cpu_block` (P0.2).
///
/// | Condition | Strategy |
/// |-----------|----------|
/// | null/none and packet count < 8 | inline |
/// | encrypt and (pkts < 4 OR total bytes < 4KB) | inline |
/// | otherwise | offload |
#[inline]
pub fn should_cpu_block_encrypt(
    has_encryption: bool,
    has_aead: bool,
    packet_count: usize,
    total_bytes: usize,
) -> bool {
    if !has_encryption && !has_aead {
        packet_count >= 8
    } else {
        packet_count >= 4 || total_bytes >= 4096
    }
}

/// Size threshold for session-level Snappy offload (≥4 KiB).
///
/// Callers should **also** require null/none crypto: when encrypt is itself
/// offloaded to `cpu_block`, keeping Snappy inline avoids a double pool hop
/// and keeps the plaintext warm for the subsequent encrypt_batch.
#[inline]
pub fn should_cpu_block_compress(uncompressed_bytes: usize) -> bool {
    uncompressed_bytes >= 4096
}

/// Encrypt a batch of raw KCP segments for the wire (P0.1 / P0.5).
///
/// Input packets are `Bytes` (reference-counted) — the KCP output callback
/// now hands ownership directly (P1.1 R2), avoiding a per-packet `Vec` alloc
/// + `extend_from_slice` copy in the output path.
///
/// - AEAD: `seal_into` each packet
/// - CFB: serial `prepare_encrypt` then parallel `crypt.encrypt` when ≥4
///   and `allow_parallel` (disable when already on a `cpu_block` worker)
/// - null: move `Bytes` straight through (no crypto header, no copy)
pub fn encrypt_batch(
    packets: Vec<Bytes>,
    crypt: &dyn BlockCrypt,
    crypto_buf: &parking_lot::Mutex<CryptoBuf>,
    aead: Option<&dyn AeadCrypt>,
    has_encryption: bool,
    // When false, never spawn thread::scope workers (already on a cpu_block
    // pool thread — nested parallelism thrashes cores under multi-session load).
    allow_parallel: bool,
) -> Vec<Bytes> {
    let mut results = Vec::with_capacity(packets.len());
    if let Some(aead) = aead {
        // Reuse one BytesMut across the batch (P1.5 seal_into).
        let mut aead_buf = BytesMut::new();
        for data in &packets {
            results.push(aead.seal_into(data, &mut aead_buf));
        }
    } else if has_encryption {
        // Small batches: encrypt_cfb reuses CryptoBuf's internal buffer (no
        // per-packet BytesMut from prepare_encrypt). Large batches: prepare
        // then parallel encrypt (P1.1) only when `allow_parallel`.
        let nthreads = if allow_parallel {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1)
                .min(packets.len())
        } else {
            1
        };
        if nthreads <= 1 || packets.len() < 4 {
            let mut cb = crypto_buf.lock();
            for data in &packets {
                results.push(cb.encrypt_cfb(data, crypt));
            }
        } else {
            // Phase 1: Prepare all packets (serial — shared nonce counter)
            let prepared: Vec<BytesMut> = {
                let mut cb = crypto_buf.lock();
                packets
                    .iter()
                    .map(|data| cb.prepare_encrypt(data))
                    .collect()
            };
            // Phase 2: Encrypt in parallel (cipher is stateless)
            let chunk_size = prepared.len().div_ceil(nthreads);
            let mut iter = prepared.into_iter();
            std::thread::scope(|s| {
                let mut handles = Vec::new();
                loop {
                    let chunk: Vec<BytesMut> = (&mut iter).take(chunk_size).collect();
                    if chunk.is_empty() {
                        break;
                    }
                    handles.push(s.spawn(move || {
                        let mut r = Vec::with_capacity(chunk.len());
                        for mut buf in chunk {
                            crypt.encrypt(&mut buf);
                            r.push(buf.freeze());
                        }
                        r
                    }));
                }
                for h in handles {
                    results.extend(h.join().unwrap());
                }
            });
        }
    } else {
        // null: Bytes pass straight through (no crypto header, no copy).
        results.extend(packets);
    }
    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypt::select_block_crypt;

    #[test]
    fn test_encrypt_decrypt_roundtrip() {
        let (crypt, _) = select_block_crypt("aes-128", b"test-key-12345678");
        let mut cb = CryptoBuf::new(0xDEADBEEF);

        let plaintext = b"hello kcptun wire format test!";
        let encrypted = cb.encrypt_cfb(plaintext, crypt.as_ref());

        // Decrypt
        let mut enc_copy = encrypted.to_vec();
        let decrypted = cb.decrypt_cfb(&mut enc_copy, crypt.as_ref());

        assert!(decrypted.is_some());
        let dec = decrypted.unwrap();
        assert_eq!(&dec[..], plaintext);
    }

    #[test]
    fn test_nonce_counter_increments() {
        let (crypt, _) = select_block_crypt("aes-128", b"test-key-12345678");
        let mut cb = CryptoBuf::new(0xCAFEBABE);

        let data = b"test data for nonce";
        let pkt1 = cb.encrypt_cfb(data, crypt.as_ref());
        let pkt2 = cb.encrypt_cfb(data, crypt.as_ref());

        // Nonces should differ (counter incremented)
        assert_ne!(&pkt1[..8], &pkt2[..8]);
        // Session ID should be the same
        assert_eq!(&pkt1[8..16], &pkt2[8..16]);
    }

    #[test]
    fn should_cpu_block_thresholds() {
        // null/none: only offload large batches
        assert!(!should_cpu_block_encrypt(false, false, 7, 100_000));
        assert!(should_cpu_block_encrypt(false, false, 8, 1));
        // encrypt: packet count or bytes
        assert!(!should_cpu_block_encrypt(true, false, 3, 100));
        assert!(should_cpu_block_encrypt(true, false, 4, 100));
        assert!(should_cpu_block_encrypt(true, false, 1, 4096));
        // aead same as encrypt
        assert!(should_cpu_block_encrypt(false, true, 4, 0));
        // snappy compress threshold
        assert!(!should_cpu_block_compress(4095));
        assert!(should_cpu_block_compress(4096));
    }

    #[test]
    fn encrypt_batch_null_and_cfb() {
        let packets: Vec<Bytes> = vec![Bytes::from(&b"aaa"[..]), Bytes::from(&b"bbbb"[..])];
        let (crypt, _) = select_block_crypt("null", b"key");
        let cb = parking_lot::Mutex::new(CryptoBuf::new(1));
        let out = encrypt_batch(packets, crypt.as_ref(), &cb, None, false, true);
        assert_eq!(out.len(), 2);
        assert_eq!(&out[0][..], b"aaa");
        assert_eq!(&out[1][..], b"bbbb");

        let (crypt, _) = select_block_crypt("aes-128", b"test-key-12345678");
        let packets: Vec<Bytes> = vec![Bytes::from(&b"hello wire"[..])];
        let mut cb = CryptoBuf::new(2);
        let cb_mu = parking_lot::Mutex::new(CryptoBuf::new(2));
        let out = encrypt_batch(packets, crypt.as_ref(), &cb_mu, None, true, true);
        assert_eq!(out.len(), 1);
        assert!(out[0].len() > 10);
        let mut enc = out[0].to_vec();
        let dec = cb.decrypt_cfb(&mut enc, crypt.as_ref()).unwrap();
        assert_eq!(&dec[..], b"hello wire");
    }

    #[test]
    fn encrypt_batch_allow_parallel_false_still_correct() {
        let (crypt, _) = select_block_crypt("aes-128", b"test-key-12345678");
        // ≥4 packets so the parallel branch would normally engage.
        let packets: Vec<Bytes> = (0..8).map(|i| Bytes::from(vec![i as u8; 64])).collect();
        let cb_mu = parking_lot::Mutex::new(CryptoBuf::new(99));
        let out = encrypt_batch(
            packets.clone(),
            crypt.as_ref(),
            &cb_mu,
            None,
            true,
            false, // force serial (cpu_block worker path)
        );
        assert_eq!(out.len(), 8);
        let mut cb = CryptoBuf::new(99);
        for (i, pkt) in out.iter().enumerate() {
            let mut enc = pkt.to_vec();
            let dec = cb.decrypt_cfb(&mut enc, crypt.as_ref()).unwrap();
            assert_eq!(&dec[..], &vec![i as u8; 64][..]);
        }
    }

    #[test]
    fn test_crc_mismatch_returns_none() {
        let (crypt, _) = select_block_crypt("aes-128", b"test-key-12345678");
        let mut cb = CryptoBuf::new(0xDEAD);

        let plaintext = b"hello";
        let mut encrypted = cb.encrypt_cfb(plaintext, crypt.as_ref()).to_vec();

        // Corrupt the CRC field (bytes 16..20)
        encrypted[17] ^= 0xFF;

        let result = cb.decrypt_cfb(&mut encrypted, crypt.as_ref());
        assert!(result.is_none());
    }

    #[test]
    fn test_short_data_returns_none() {
        let (crypt, _) = select_block_crypt("aes-128", b"test-key-12345678");
        let mut cb = CryptoBuf::new(0);

        let mut short = [0u8; 10]; // < CRYPT_HDR (20)
        let result = cb.decrypt_cfb(&mut short, crypt.as_ref());
        assert!(result.is_none());
    }

    #[test]
    fn test_buffer_reuse_no_reallocation() {
        let (crypt, _) = select_block_crypt("aes-128", b"test-key-12345678");
        let mut cb = CryptoBuf::new(0);

        // Encrypt many packets of varying sizes
        for i in 0..100 {
            let data = vec![i as u8; 100 + i * 10];
            let encrypted = cb.encrypt_cfb(&data, crypt.as_ref());
            assert_eq!(encrypted.len(), CRYPT_HDR + data.len());

            // Verify roundtrip
            let mut enc_copy = encrypted.to_vec();
            let decrypted = cb.decrypt_cfb(&mut enc_copy, crypt.as_ref());
            assert!(decrypted.is_some());
            assert_eq!(&decrypted.unwrap()[..], &data[..]);
        }
    }

    #[test]
    fn test_none_crypt() {
        let (crypt, _) = select_block_crypt("none", b"");
        let mut cb = CryptoBuf::new(0);

        let plaintext = b"test none cipher";
        let encrypted = cb.encrypt_cfb(plaintext, crypt.as_ref());
        // With none cipher, nonce and CRC are still written, data is not encrypted
        assert_eq!(&encrypted[CRYPT_HDR..], plaintext);
    }

    #[test]
    fn decrypt_cfb_in_place_roundtrip() {
        let (crypt, _) = select_block_crypt("aes-128", b"test-key-12345678");
        let mut cb = CryptoBuf::new(0xBEEF);
        let plaintext = b"in-place body payload xyz";
        let encrypted = cb.encrypt_cfb(plaintext, crypt.as_ref());
        let mut buf = encrypted.to_vec();
        let body = decrypt_cfb_in_place(&mut buf, crypt.as_ref(), false).unwrap();
        assert_eq!(body, plaintext);
        // body is a subslice of buf (after header)
        assert_eq!(body.as_ptr(), buf[CRYPT_HDR..].as_ptr());
    }

    #[test]
    fn decrypt_cfb_in_place_crc_and_short() {
        let (crypt, _) = select_block_crypt("aes-128", b"test-key-12345678");
        let mut cb = CryptoBuf::new(1);
        let mut enc = cb.encrypt_cfb(b"hello", crypt.as_ref()).to_vec();
        enc[17] ^= 0xFF;
        assert_eq!(
            decrypt_cfb_in_place(&mut enc, crypt.as_ref(), false),
            Err(InboundCryptError::CrcMismatch)
        );
        let mut short = [0u8; 10];
        assert_eq!(
            decrypt_cfb_in_place(&mut short, crypt.as_ref(), false),
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
