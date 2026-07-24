//! Block cipher and AEAD implementations for KCP packet encryption.
//!
//! Port of Go's `kcp-go/v5/crypt.go`. Wire-level compatible with all 13 ciphers.
//!
//! Each cipher lives in its own submodule under [`crypt/`](self):
//!
//! | Module          | Cipher         | Block size |
//! |-----------------|----------------|------------|
//! | [`none`]        | `NoneCrypt`    | n/a        |
//! | [`xor`]         | `SimpleXORCrypt`| stream    |
//! | [`aes_cfb`]     | `AesCfbCrypt`  | 16 B       |
//! | [`sm4`]         | `Sm4Crypt`     | 16 B       |
//! | [`tea`]         | `TeaCrypt`     | 8 B        |
//! | [`xtea`]        | `XteaCrypt`    | 8 B        |
//! | [`salsa20`]     | `Salsa20Crypt` | stream     |
//! | [`blowfish`]    | `BlowfishCrypt`| 8 B        |
//! | [`twofish`]     | `TwofishCrypt` | 16 B       |
//! | [`cast5_crypt`] | `Cast5Crypt`   | 8 B        |
//! | [`triple_des`]  | `TripleDesCrypt`| 8 B       |
//! | [`aes_gcm`]     | `Aes128GcmCrypt`| 16 B (AEAD)|

use std::fmt;

// ─── Submodules ────────────────────────────────────────────────────────
mod aes_cfb;
mod aes_gcm;
mod blowfish;
mod cast5_crypt;
mod none;
mod salsa20;
mod sm4;
mod tea;
mod triple_des;
mod twofish;
mod xor;
mod xtea;

// Re-export all cipher types at the module root.
pub use aes_cfb::AesCfbCrypt;
pub use aes_gcm::Aes128GcmCrypt;
pub use blowfish::BlowfishCrypt;
pub use cast5_crypt::Cast5Crypt;
pub use none::NoneCrypt;
pub use salsa20::Salsa20Crypt;
pub use sm4::Sm4Crypt;
pub use tea::TeaCrypt;
pub use triple_des::TripleDesCrypt;
pub use twofish::TwofishCrypt;
pub use xor::SimpleXORCrypt;
pub use xtea::XteaCrypt;

// ─── Shared constants ──────────────────────────────────────────────────

/// Fixed IV used by Go's kcp-go CFB implementation.
pub(crate) const GO_CFB_IV: [u8; 16] = [
    167, 115, 79, 156, 18, 172, 27, 1, 164, 21, 242, 193, 252, 120, 230, 107,
];

/// PBKDF2 salt for the XOR cipher key expansion.
const SALT_XOR: &str = "sH3CIVoF#rWLtJo6";

// ─── Traits ────────────────────────────────────────────────────────────

/// In-place block cipher encryption/decryption trait.
///
/// Implementations are stateless after construction — the same key produces
/// deterministic results because CFB uses a fixed IV (`GO_CFB_IV`).
pub trait BlockCrypt: Send + Sync + fmt::Debug {
    /// Encrypt `data` in place.
    fn encrypt(&self, data: &mut [u8]);
    /// Decrypt `data` in place.
    fn decrypt(&self, data: &mut [u8]);
    /// Return the cipher's canonical name (e.g. `"aes-128"`, `"tea"`).
    fn name(&self) -> &'static str;
}

/// AEAD trait for nonce-based authenticated encryption (AES-GCM).
///
/// This is a separate trait from `BlockCrypt` because AEAD uses a different
/// packet layout: `[nonce 12B][ciphertext + tag 16B]` with no CRC32.
pub trait AeadCrypt: Send + Sync + fmt::Debug {
    /// Size of the nonce in bytes (12 for AES-GCM).
    fn nonce_size(&self) -> usize;
    /// Encrypt plaintext, returning `[nonce][ciphertext+tag]`.
    fn seal(&self, plaintext: &[u8]) -> Vec<u8>;
    /// Encrypt into a reusable buffer; returns a frozen `Bytes` slice of the packet.
    ///
    /// Default: allocate via `seal` then copy into `out`. Implementations should
    /// override for zero-extra-alloc paths.
    fn seal_into(&self, plaintext: &[u8], out: &mut bytes::BytesMut) -> bytes::Bytes {
        let sealed = self.seal(plaintext);
        out.clear();
        out.extend_from_slice(&sealed);
        out.split_to(sealed.len()).freeze()
    }
    /// Decrypt `[nonce][ciphertext+tag]`, returning plaintext or error.
    fn open(&self, data: &[u8]) -> Result<Vec<u8>, String>;
    fn name(&self) -> &'static str;
}

// ─── Block cipher traits for CFB (R1: deduplicate ~400 LOC) ───────────────

/// Trait for 8-byte block ciphers (TEA, XTEA, Blowfish, 3DES, CAST5).
///
/// Used to provide a uniform interface for the shared CFB-8 implementation.
pub trait BlockCipher8: Send + Sync {
    /// Encrypt one 8-byte block: `out = E(inp)`.
    fn encrypt_block(&self, out: &mut [u8; 8], inp: &[u8; 8]);
}

/// Trait for 16-byte block ciphers (AES, Twofish, SM4).
///
/// Used to provide a uniform interface for the shared CFB-128 implementation.
pub trait BlockCipher16: Send + Sync {
    /// Encrypt one 16-byte block: `out = E(inp)`.
    fn encrypt_block(&self, out: &mut [u8; 16], inp: &[u8; 16]);

    /// Encrypt multiple 16-byte blocks in place (for hardware ILP / AES-NI pipelining).
    ///
    /// Default: fall back to calling `encrypt_block` in a loop.
    /// Implementations (e.g. AES) should override to use `encrypt_blocks` when available.
    #[inline]
    fn encrypt_blocks(&self, blocks: &mut [u8]) {
        // Process full 16-byte blocks
        let mut i = 0;
        while i + 16 <= blocks.len() {
            let mut b = [0u8; 16];
            b.copy_from_slice(&blocks[i..i + 16]);
            let mut out = [0u8; 16];
            self.encrypt_block(&mut out, &b);
            blocks[i..i + 16].copy_from_slice(&out);
            i += 16;
        }
        // Tail < 16B left as-is (caller handles padding semantics)
    }
}

// ─── Shared CFB implementations using the traits ─────────────────────────

/// CFB-8 encrypt using a `BlockCipher8`.
///
/// Matches the existing `cfb8_enc` semantics and Go wire format (fixed IV).
#[inline]
pub fn cfb8_encrypt<C: BlockCipher8>(data: &mut [u8], c: &C) {
    if data.is_empty() {
        return;
    }
    let mut tbl = [
        GO_CFB_IV[0], GO_CFB_IV[1], GO_CFB_IV[2], GO_CFB_IV[3],
        GO_CFB_IV[4], GO_CFB_IV[5], GO_CFB_IV[6], GO_CFB_IV[7],
    ];
    let mut i = 0;
    while i + 64 <= data.len() {
        for _ in 0..8 {
            let chunk = &mut data[i..i + 8];
            let mut b = [0u8; 8];
            c.encrypt_block(&mut b, &tbl);
            let y = u64::from_le_bytes([
                chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], chunk[6], chunk[7],
            ]) ^ u64::from_le_bytes(b);
            let out = y.to_le_bytes();
            chunk.copy_from_slice(&out);
            tbl = out;
            i += 8;
        }
    }
    while i + 8 <= data.len() {
        let chunk = &mut data[i..i + 8];
        let mut b = [0u8; 8];
        c.encrypt_block(&mut b, &tbl);
        let y = u64::from_le_bytes([
            chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], chunk[6], chunk[7],
        ]) ^ u64::from_le_bytes(b);
        let out = y.to_le_bytes();
        chunk.copy_from_slice(&out);
        tbl = out;
        i += 8;
    }
    if i < data.len() {
        let chunk = &mut data[i..];
        let len = chunk.len();
        let mut b = [0u8; 8];
        c.encrypt_block(&mut b, &tbl);
        for j in 0..len {
            chunk[j] ^= b[j];
        }
    }
}

/// CFB-8 decrypt using a `BlockCipher8`.
#[inline]
pub fn cfb8_decrypt<C: BlockCipher8>(data: &mut [u8], c: &C) {
    if data.is_empty() {
        return;
    }
    let mut tbl = [
        GO_CFB_IV[0], GO_CFB_IV[1], GO_CFB_IV[2], GO_CFB_IV[3],
        GO_CFB_IV[4], GO_CFB_IV[5], GO_CFB_IV[6], GO_CFB_IV[7],
    ];
    let mut i = 0;
    while i + 8 <= data.len() {
        let chunk = &mut data[i..i + 8];
        let src = [
            chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], chunk[6], chunk[7],
        ];
        let mut b = [0u8; 8];
        c.encrypt_block(&mut b, &tbl);
        let y = u64::from_le_bytes(src) ^ u64::from_le_bytes(b);
        chunk.copy_from_slice(&y.to_le_bytes());
        tbl = src;
        i += 8;
    }
    if i < data.len() {
        let chunk = &mut data[i..];
        let len = chunk.len();
        let mut b = [0u8; 8];
        c.encrypt_block(&mut b, &tbl);
        for j in 0..len {
            chunk[j] ^= b[j];
        }
    }
}

/// CFB-128 encrypt using a `BlockCipher16`.
///
/// Uses direct byte indexing (matching `cfb8_encrypt`) instead of
/// `try_into().unwrap()` to avoid runtime bounds checks in the hot path.
#[inline]
pub fn cfb16_encrypt<C: BlockCipher16>(data: &mut [u8], c: &C) {
    if data.is_empty() {
        return;
    }
    let mut tbl = GO_CFB_IV;
    let mut i = 0;
    while i + 128 <= data.len() {
        for _ in 0..8 {
            let chunk = &mut data[i..i + 16];
            let mut b = [0u8; 16];
            c.encrypt_block(&mut b, &tbl);
            let b0 = u64::from_le_bytes([
                b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
            ]);
            let b1 = u64::from_le_bytes([
                b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15],
            ]);
            let s0 = u64::from_le_bytes([
                chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], chunk[6], chunk[7],
            ]);
            let s1 = u64::from_le_bytes([
                chunk[8], chunk[9], chunk[10], chunk[11], chunk[12], chunk[13], chunk[14], chunk[15],
            ]);
            let out0 = (s0 ^ b0).to_le_bytes();
            let out1 = (s1 ^ b1).to_le_bytes();
            chunk[0..8].copy_from_slice(&out0);
            chunk[8..16].copy_from_slice(&out1);
            tbl.copy_from_slice(chunk);
            i += 16;
        }
    }
    while i + 16 <= data.len() {
        let chunk = &mut data[i..i + 16];
        let mut b = [0u8; 16];
        c.encrypt_block(&mut b, &tbl);
        let b0 = u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]);
        let b1 = u64::from_le_bytes([
            b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15],
        ]);
        let s0 = u64::from_le_bytes([
            chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], chunk[6], chunk[7],
        ]);
        let s1 = u64::from_le_bytes([
            chunk[8], chunk[9], chunk[10], chunk[11], chunk[12], chunk[13], chunk[14], chunk[15],
        ]);
        let out0 = (s0 ^ b0).to_le_bytes();
        let out1 = (s1 ^ b1).to_le_bytes();
        chunk[0..8].copy_from_slice(&out0);
        chunk[8..16].copy_from_slice(&out1);
        tbl.copy_from_slice(chunk);
        i += 16;
    }
    if i < data.len() {
        let chunk = &mut data[i..];
        let len = chunk.len();
        let mut b = [0u8; 16];
        c.encrypt_block(&mut b, &tbl);
        for j in 0..len {
            chunk[j] ^= b[j];
        }
    }
}

/// CFB-128 decrypt using a `BlockCipher16`.
///
/// Uses direct byte indexing (matching `cfb8_decrypt`) instead of
/// `try_into().unwrap()` to avoid runtime bounds checks in the hot path.
#[inline]
pub fn cfb16_decrypt<C: BlockCipher16>(data: &mut [u8], c: &C) {
    if data.is_empty() {
        return;
    }
    let mut tbl = GO_CFB_IV;
    let mut i = 0;
    while i + 16 <= data.len() {
        let chunk = &mut data[i..i + 16];
        let src = [
            chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], chunk[6], chunk[7],
            chunk[8], chunk[9], chunk[10], chunk[11], chunk[12], chunk[13], chunk[14], chunk[15],
        ];
        let mut b = [0u8; 16];
        c.encrypt_block(&mut b, &tbl);
        let b0 = u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]);
        let b1 = u64::from_le_bytes([
            b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15],
        ]);
        let s0 = u64::from_le_bytes([
            src[0], src[1], src[2], src[3], src[4], src[5], src[6], src[7],
        ]);
        let s1 = u64::from_le_bytes([
            src[8], src[9], src[10], src[11], src[12], src[13], src[14], src[15],
        ]);
        let out0 = (s0 ^ b0).to_le_bytes();
        let out1 = (s1 ^ b1).to_le_bytes();
        chunk[0..8].copy_from_slice(&out0);
        chunk[8..16].copy_from_slice(&out1);
        tbl = src;
        i += 16;
    }
    if i < data.len() {
        let chunk = &mut data[i..];
        let len = chunk.len();
        let mut b = [0u8; 16];
        c.encrypt_block(&mut b, &tbl);
        for j in 0..len {
            chunk[j] ^= b[j];
        }
    }
}

// ─── Cipher Selection ──────────────────────────────────────────────────

/// Pad (or truncate) `k` to exactly `s` bytes.
fn pad(k: &[u8], s: usize) -> Vec<u8> {
    if k.len() < s {
        let mut v = vec![0u8; s];
        v[..k.len()].copy_from_slice(k);
        v
    } else {
        k[..s].to_vec()
    }
}

/// Select a [`BlockCrypt`] implementation by method name.
///
/// Returns a boxed cipher and the canonical method name string.
/// Unknown methods default to AES-256-CFB (`"aes"`).
pub fn select_block_crypt(method: &str, pass: &[u8]) -> (Box<dyn BlockCrypt>, String) {
    match method {
        // Go's "null" means no cipher at all (nil BlockCrypt = no crypto header).
        // "none" means NoneBlockCrypt (copies data, but still has crypto header).
        "null" => (Box::new(NoneCrypt), "null".to_string()),
        "none" => (Box::new(NoneCrypt), "none".to_string()),
        "xor" => (Box::new(SimpleXORCrypt::new(pass)), method.to_string()),
        "aes-128" => (
            Box::new(AesCfbCrypt::new(&pad(pass, 16))),
            method.to_string(),
        ),
        "aes-192" => (
            Box::new(AesCfbCrypt::new(&pad(pass, 24))),
            method.to_string(),
        ),
        "aes" | "aes-256" => (Box::new(AesCfbCrypt::new(pass)), "aes".to_string()),
        "aes-128-gcm" => (
            Box::new(Aes128GcmCrypt::new(pass)),
            "aes-128-gcm".to_string(),
        ),
        "sm4" => (Box::new(Sm4Crypt::new(&pad(pass, 16))), method.to_string()),
        "tea" => (Box::new(TeaCrypt::new(pass)), method.to_string()),
        "xtea" => (Box::new(XteaCrypt::new(pass)), method.to_string()),
        "salsa20" | "salsa" => (Box::new(Salsa20Crypt::new(pass)), "salsa20".to_string()),
        "blowfish" => (Box::new(BlowfishCrypt::new(pass)), method.to_string()),
        "twofish" => (
            Box::new(TwofishCrypt::new(&pad(pass, 32))),
            method.to_string(),
        ),
        "cast5" => (
            Box::new(Cast5Crypt::new(&pad(pass, 16))),
            method.to_string(),
        ),
        "3des" | "tripledes" => (
            Box::new(TripleDesCrypt::new(&pad(pass, 24))),
            "3des".to_string(),
        ),
        _ => (Box::new(AesCfbCrypt::new(pass)), "aes".to_string()),
    }
}

/// Select an [`AeadCrypt`] if the method is an AEAD variant.
///
/// Returns `None` for non-AEAD methods.
pub fn select_aead_crypt(method: &str, pass: &[u8]) -> Option<Box<dyn AeadCrypt>> {
    match method {
        "aes-128-gcm" => Some(Box::new(Aes128GcmCrypt::new(pass))),
        _ => None,
    }
}

// ─── Monomorphized cipher enum (P3 optional hot-path dispatch) ─────────

/// Concrete cipher set with static dispatch via `match`.
///
/// Prefer this over `dyn BlockCrypt` on the encrypt hot path when the
/// method is known at session start — eliminates vtable calls while keeping
/// object-safe [`BlockCrypt`] for existing APIs.
#[derive(Debug)]
pub enum CryptEngine {
    None(NoneCrypt),
    Xor(SimpleXORCrypt),
    AesCfb(AesCfbCrypt),
    Aes128Gcm(Aes128GcmCrypt),
    Sm4(Sm4Crypt),
    Tea(TeaCrypt),
    Xtea(XteaCrypt),
    Salsa20(Salsa20Crypt),
    Blowfish(BlowfishCrypt),
    Twofish(TwofishCrypt),
    Cast5(Cast5Crypt),
    TripleDes(TripleDesCrypt),
}

impl CryptEngine {
    /// Build a [`CryptEngine`] from a method name (same selection as
    /// [`select_block_crypt`]).
    pub fn select(method: &str, pass: &[u8]) -> (Self, String) {
        match method {
            "null" | "none" => (CryptEngine::None(NoneCrypt), method.to_string()),
            "xor" => (
                CryptEngine::Xor(SimpleXORCrypt::new(pass)),
                method.to_string(),
            ),
            "aes-128" => (
                CryptEngine::AesCfb(AesCfbCrypt::new(&pad(pass, 16))),
                method.to_string(),
            ),
            "aes-192" => (
                CryptEngine::AesCfb(AesCfbCrypt::new(&pad(pass, 24))),
                method.to_string(),
            ),
            "aes" | "aes-256" => (
                CryptEngine::AesCfb(AesCfbCrypt::new(pass)),
                "aes".to_string(),
            ),
            "aes-128-gcm" => (
                CryptEngine::Aes128Gcm(Aes128GcmCrypt::new(pass)),
                "aes-128-gcm".to_string(),
            ),
            "sm4" => (
                CryptEngine::Sm4(Sm4Crypt::new(&pad(pass, 16))),
                method.to_string(),
            ),
            "tea" => (CryptEngine::Tea(TeaCrypt::new(pass)), method.to_string()),
            "xtea" => (CryptEngine::Xtea(XteaCrypt::new(pass)), method.to_string()),
            "salsa20" | "salsa" => (
                CryptEngine::Salsa20(Salsa20Crypt::new(pass)),
                "salsa20".to_string(),
            ),
            "blowfish" => (
                CryptEngine::Blowfish(BlowfishCrypt::new(pass)),
                method.to_string(),
            ),
            "twofish" => (
                CryptEngine::Twofish(TwofishCrypt::new(&pad(pass, 32))),
                method.to_string(),
            ),
            "cast5" => (
                CryptEngine::Cast5(Cast5Crypt::new(&pad(pass, 16))),
                method.to_string(),
            ),
            "3des" | "tripledes" => (
                CryptEngine::TripleDes(TripleDesCrypt::new(&pad(pass, 24))),
                "3des".to_string(),
            ),
            _ => (
                CryptEngine::AesCfb(AesCfbCrypt::new(pass)),
                "aes".to_string(),
            ),
        }
    }

    /// Canonical method name for this engine.
    pub fn name(&self) -> &'static str {
        match self {
            CryptEngine::None(_) => "none",
            CryptEngine::Xor(_) => "xor",
            CryptEngine::AesCfb(c) => c.name(),
            CryptEngine::Aes128Gcm(_) => "aes-128-gcm",
            CryptEngine::Sm4(_) => "sm4",
            CryptEngine::Tea(_) => "tea",
            CryptEngine::Xtea(_) => "xtea",
            CryptEngine::Salsa20(_) => "salsa20",
            CryptEngine::Blowfish(_) => "blowfish",
            CryptEngine::Twofish(_) => "twofish",
            CryptEngine::Cast5(_) => "cast5",
            CryptEngine::TripleDes(_) => "3des",
        }
    }
}

impl BlockCrypt for CryptEngine {
    #[inline]
    fn encrypt(&self, data: &mut [u8]) {
        match self {
            CryptEngine::None(c) => c.encrypt(data),
            CryptEngine::Xor(c) => c.encrypt(data),
            CryptEngine::AesCfb(c) => c.encrypt(data),
            CryptEngine::Aes128Gcm(c) => c.encrypt(data),
            CryptEngine::Sm4(c) => c.encrypt(data),
            CryptEngine::Tea(c) => c.encrypt(data),
            CryptEngine::Xtea(c) => c.encrypt(data),
            CryptEngine::Salsa20(c) => c.encrypt(data),
            CryptEngine::Blowfish(c) => c.encrypt(data),
            CryptEngine::Twofish(c) => c.encrypt(data),
            CryptEngine::Cast5(c) => c.encrypt(data),
            CryptEngine::TripleDes(c) => c.encrypt(data),
        }
    }

    #[inline]
    fn decrypt(&self, data: &mut [u8]) {
        match self {
            CryptEngine::None(c) => c.decrypt(data),
            CryptEngine::Xor(c) => c.decrypt(data),
            CryptEngine::AesCfb(c) => c.decrypt(data),
            CryptEngine::Aes128Gcm(c) => c.decrypt(data),
            CryptEngine::Sm4(c) => c.decrypt(data),
            CryptEngine::Tea(c) => c.decrypt(data),
            CryptEngine::Xtea(c) => c.decrypt(data),
            CryptEngine::Salsa20(c) => c.decrypt(data),
            CryptEngine::Blowfish(c) => c.decrypt(data),
            CryptEngine::Twofish(c) => c.decrypt(data),
            CryptEngine::Cast5(c) => c.decrypt(data),
            CryptEngine::TripleDes(c) => c.decrypt(data),
        }
    }

    fn name(&self) -> &'static str {
        CryptEngine::name(self)
    }
}

// ─── Integration tests ─────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn select_all() {
        for m in &[
            "null",
            "none",
            "xor",
            "aes-128",
            "aes-192",
            "aes",
            "aes-128-gcm",
            "sm4",
            "tea",
            "xtea",
            "salsa20",
            "blowfish",
            "twofish",
            "cast5",
            "3des",
        ] {
            let (c, n) = select_block_crypt(m, b"test-key-12345");
            let mut d = b"test data!".to_vec();
            let o = d.clone();
            c.encrypt(&mut d);
            c.decrypt(&mut d);
            assert_eq!(d, o, "{}", n);
        }
    }

    #[test]
    fn crypt_engine_matches_dyn() {
        for m in &["null", "aes-128", "3des", "sm4", "tea"] {
            let (dyn_c, _) = select_block_crypt(m, b"test-key-12345");
            let (eng, _) = CryptEngine::select(m, b"test-key-12345");
            let mut a = b"hello static dispatch!".to_vec();
            let mut b = a.clone();
            dyn_c.encrypt(&mut a);
            eng.encrypt(&mut b);
            assert_eq!(a, b, "encrypt mismatch for {}", m);
            dyn_c.decrypt(&mut a);
            eng.decrypt(&mut b);
            assert_eq!(a, b, "decrypt mismatch for {}", m);
            assert_eq!(&a, b"hello static dispatch!");
        }
    }
}
