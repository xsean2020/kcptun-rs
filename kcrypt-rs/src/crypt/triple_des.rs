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

use super::{BlockCrypt, GO_CFB_IV};
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

    /// Specialized CFB-8 encrypt monomorphized on TripleDesCipher (no closure).
    #[inline(always)]
    fn cfb_enc_specialized(&self, data: &mut [u8]) {
        if data.is_empty() {
            return;
        }
        let mut tbl = [
            GO_CFB_IV[0],
            GO_CFB_IV[1],
            GO_CFB_IV[2],
            GO_CFB_IV[3],
            GO_CFB_IV[4],
            GO_CFB_IV[5],
            GO_CFB_IV[6],
            GO_CFB_IV[7],
        ];
        let mut i = 0;
        while i + 64 <= data.len() {
            for _ in 0..8 {
                let chunk = &mut data[i..i + 8];
                let mut b = [0u8; 8];
                self.cipher.encrypt_block(&mut b, &tbl);
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
            self.cipher.encrypt_block(&mut b, &tbl);
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
            self.cipher.encrypt_block(&mut b, &tbl);
            for j in 0..len {
                chunk[j] ^= b[j];
            }
        }
    }

    #[inline(always)]
    fn cfb_dec_specialized(&self, data: &mut [u8]) {
        if data.is_empty() {
            return;
        }
        let mut tbl = [
            GO_CFB_IV[0],
            GO_CFB_IV[1],
            GO_CFB_IV[2],
            GO_CFB_IV[3],
            GO_CFB_IV[4],
            GO_CFB_IV[5],
            GO_CFB_IV[6],
            GO_CFB_IV[7],
        ];
        let mut i = 0;
        while i + 8 <= data.len() {
            let chunk = &mut data[i..i + 8];
            let src = [
                chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], chunk[6], chunk[7],
            ];
            let mut b = [0u8; 8];
            self.cipher.encrypt_block(&mut b, &tbl);
            let y = u64::from_le_bytes(src) ^ u64::from_le_bytes(b);
            chunk.copy_from_slice(&y.to_le_bytes());
            tbl = src;
            i += 8;
        }
        if i < data.len() {
            let chunk = &mut data[i..];
            let len = chunk.len();
            let mut b = [0u8; 8];
            self.cipher.encrypt_block(&mut b, &tbl);
            for j in 0..len {
                chunk[j] ^= b[j];
            }
        }
    }
}

impl BlockCrypt for TripleDesCrypt {
    fn encrypt(&self, data: &mut [u8]) {
        self.cfb_enc_specialized(data);
    }
    fn decrypt(&self, data: &mut [u8]) {
        // CFB decrypt uses the forward (encrypt) cipher, matching Go's decrypt8
        self.cfb_dec_specialized(data);
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
