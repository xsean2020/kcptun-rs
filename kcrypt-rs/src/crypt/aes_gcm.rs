//! AES-128-GCM authenticated encryption (AEAD).
//!
//! Packet layout (matching Go's `aeadCrypt`):
//!   `[nonce 12B][ciphertext + tag 16B]`
//!
//! No CRC32 — AEAD provides built-in authentication via the GCM tag.
//!
//! Nonce: 12-byte counter (unique per `Aes128GcmCrypt` instance). Go uses
//! random nonces; uniqueness is the only wire requirement. Counter avoids
//! per-packet PRNG cost on the seal hot path.

use std::sync::atomic::{AtomicU64, Ordering};

use aes_gcm::aead::{generic_array::GenericArray, AeadInPlace, KeyInit};
use aes_gcm::Aes128Gcm;
use bytes::{Bytes, BytesMut};

use super::{AeadCrypt, BlockCrypt};

const NONCE_SZ: usize = 12;
const TAG_SZ: usize = 16;

pub struct Aes128GcmCrypt {
    cipher: Aes128Gcm,
    /// Monotonic counter for unique nonces (replaces per-packet PRNG).
    nonce_ctr: AtomicU64,
}

impl std::fmt::Debug for Aes128GcmCrypt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Aes128GcmCrypt").finish()
    }
}

impl Aes128GcmCrypt {
    pub fn new(key: &[u8]) -> Self {
        let mut k = [0u8; 16];
        let l = key.len().min(16);
        k[..l].copy_from_slice(&key[..l]);
        let cipher = Aes128Gcm::new(GenericArray::from_slice(&k));
        Aes128GcmCrypt {
            cipher,
            nonce_ctr: AtomicU64::new(1),
        }
    }

    #[inline]
    fn next_nonce(&self) -> [u8; NONCE_SZ] {
        let n = self.nonce_ctr.fetch_add(1, Ordering::Relaxed);
        let mut nonce = [0u8; NONCE_SZ];
        // 12-byte nonce: high 4 zero, low 8 = counter LE (unique per instance)
        nonce[4..12].copy_from_slice(&n.to_le_bytes());
        nonce
    }
}

impl AeadCrypt for Aes128GcmCrypt {
    fn nonce_size(&self) -> usize {
        NONCE_SZ
    }

    fn seal(&self, plaintext: &[u8]) -> Vec<u8> {
        let mut out = BytesMut::with_capacity(NONCE_SZ + plaintext.len() + TAG_SZ);
        self.seal_into(plaintext, &mut out).to_vec()
    }

    fn seal_into(&self, plaintext: &[u8], out: &mut BytesMut) -> Bytes {
        let total = NONCE_SZ + plaintext.len() + TAG_SZ;
        // Keep spare capacity so split_to(total) does not empty the allocation
        // (BytesMut moves capacity with a full-length split).
        const SPARE: usize = 2048;
        out.clear();
        out.reserve(total + SPARE);
        // Build [nonce | plaintext | tag_placeholder] via extend — avoids the
        // full-buffer zero-fill that `resize(total, 0)` + `copy_from_slice`
        // would do (two O(n) passes → one O(n) + one O(TAG_SZ)).
        let nonce_bytes = self.next_nonce();
        out.extend_from_slice(&nonce_bytes);
        out.extend_from_slice(plaintext);
        out.extend_from_slice(&[0u8; TAG_SZ]); // 16-byte tag placeholder

        let nonce = GenericArray::from_slice(&nonce_bytes);
        let pt_start = NONCE_SZ;
        let pt_end = NONCE_SZ + plaintext.len();
        let tag = self
            .cipher
            .encrypt_in_place_detached(nonce, b"", &mut out[pt_start..pt_end])
            .expect("AES-GCM encrypt should not fail");
        out[pt_end..].copy_from_slice(tag.as_slice());
        let frozen = out.split_to(total).freeze();
        // Warm the leftover allocation for the next seal_into call.
        if out.capacity() < SPARE {
            out.reserve(SPARE);
        }
        frozen
    }

    fn open(&self, data: &[u8]) -> Result<Vec<u8>, String> {
        if data.len() < NONCE_SZ + TAG_SZ {
            return Err("AEAD data too short".into());
        }
        let nonce = GenericArray::from_slice(&data[..NONCE_SZ]);
        let ct_and_tag = &data[NONCE_SZ..];
        let pt_len = ct_and_tag.len() - TAG_SZ;
        let mut buf = ct_and_tag[..pt_len].to_vec();
        let tag = GenericArray::from_slice(&ct_and_tag[pt_len..]);
        self.cipher
            .decrypt_in_place_detached(nonce, b"", &mut buf, tag)
            .map_err(|e| format!("AEAD decrypt failed: {:?}", e))?;
        Ok(buf)
    }

    fn name(&self) -> &'static str {
        "aes-128-gcm"
    }
}

impl BlockCrypt for Aes128GcmCrypt {
    fn encrypt(&self, _data: &mut [u8]) {
        // AEAD doesn't use the BlockCrypt in-place interface.
        // Use AeadCrypt::seal/open instead.
    }
    fn decrypt(&self, _data: &mut [u8]) {}
    fn name(&self) -> &'static str {
        "aes-128-gcm"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_open_roundtrip() {
        let c = Aes128GcmCrypt::new(b"0123456789abcdef");
        let pt = b"hello aead wire";
        let sealed = c.seal(pt);
        assert_eq!(sealed.len(), 12 + pt.len() + 16);
        let opened = c.open(&sealed).unwrap();
        assert_eq!(&opened[..], pt);
    }

    #[test]
    fn seal_into_reuses_buffer() {
        let c = Aes128GcmCrypt::new(b"0123456789abcdef");
        let mut out = BytesMut::new();
        let a = c.seal_into(b"abc", &mut out);
        let cap_after_first = out.capacity();
        assert!(
            cap_after_first >= 2048,
            "expected warm spare capacity, got {}",
            cap_after_first
        );
        let b = c.seal_into(b"defgh", &mut out);
        assert_eq!(c.open(&a).unwrap(), b"abc");
        assert_eq!(c.open(&b).unwrap(), b"defgh");
        assert!(
            out.capacity() >= 2048,
            "capacity should remain warm across seals, got {}",
            out.capacity()
        );
    }

    #[test]
    fn nonces_differ() {
        let c = Aes128GcmCrypt::new(b"0123456789abcdef");
        let a = c.seal(b"x");
        let b = c.seal(b"x");
        assert_ne!(&a[..12], &b[..12]);
    }
}
