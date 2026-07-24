//! Triple-DES (3DES) — 8-byte block cipher.
//!
//! Uses our custom `des::TripleDesCipher` (ported from Go's `crypto/des`)
//! which precomputes Feistel boxes — combining S-box + P permutation into
//! a single lookup table. This is ~2× faster than the RustCrypto `des` crate
//! which does S-box lookups and P permutation separately (including expensive
//! `wrapping_mul` bit operations).
//!
//! The custom implementation also applies IP/FP only once for all 48 rounds
//! (3×16) of TripleDES, matching Go's `tripleDESCipher` — the RustCrypto
//! crate does IP/FP 3 times (once per DES stage).
//!
//! Operates in CFB-8 mode. Wire-compatible with Go's `crypto/des`.

use super::{BlockCrypt, BlockCipher8, cfb8_encrypt, cfb8_decrypt};
use crate::des::TripleDesCipher;

#[derive(Debug)]
pub struct TripleDesCrypt {
    cipher: TripleDesCipher,
}

impl TripleDesCrypt {
    pub fn new(key: &[u8]) -> Self {
        TripleDesCrypt {
            cipher: TripleDesCipher::new(key),
        }
    }

    /// Encrypt one 8-byte block: out = E(inp).
    #[inline(always)]
    fn encrypt_block(&self, out: &mut [u8; 8], inp: &[u8; 8]) {
        let mut b = [0u8; 8];
        self.cipher.encrypt_block(&mut b, inp);
        out.copy_from_slice(&b);
    }
}

impl BlockCipher8 for TripleDesCrypt {
    #[inline]
    fn encrypt_block(&self, out: &mut [u8; 8], inp: &[u8; 8]) {
        self.encrypt_block(out, inp);
    }
}

impl BlockCrypt for TripleDesCrypt {
    fn encrypt(&self, data: &mut [u8]) {
        cfb8_encrypt(data, self);
    }
    fn decrypt(&self, data: &mut [u8]) {
        cfb8_decrypt(data, self);
    }
    fn name(&self) -> &'static str {
        "3des"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn rt(c: &dyn BlockCrypt, d: &mut [u8]) {
        let o = d.to_vec();
        c.encrypt(d);
        c.decrypt(d);
        assert_eq!(d, &o, "{} roundtrip", c.name());
    }
    #[test]
    fn td3() {
        rt(
            &TripleDesCrypt::new(&[0u8; 24]),
            &mut b"hello 3des".to_vec(),
        );
    }
}
