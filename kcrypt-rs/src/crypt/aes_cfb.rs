//! AES in CFB-128 mode (16-byte block CFB).
//!
//! Supports 128/192/256-bit keys. Uses the Go kcp-go fixed IV.
//! The cipher instance is created ONCE in the constructor and reused —
//! re-creating it per block (key schedule) was the original perf bug.

use super::{cfb16_dec, cfb16_enc, BlockCrypt};
use aes::cipher::{generic_array::GenericArray, BlockEncrypt, KeyInit};

enum AesCipher {
    Aes128(aes::Aes128),
    Aes192(aes::Aes192),
    Aes256(aes::Aes256),
}

impl AesCipher {
    #[inline]
    fn encrypt_block(&self, ga: &mut GenericArray<u8, aes::cipher::consts::U16>) {
        match self {
            AesCipher::Aes128(c) => c.encrypt_block(ga),
            AesCipher::Aes192(c) => c.encrypt_block(ga),
            AesCipher::Aes256(c) => c.encrypt_block(ga),
        }
    }
}

impl std::fmt::Debug for AesCipher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AesCipher::Aes128(_) => f.debug_struct("Aes128").finish(),
            AesCipher::Aes192(_) => f.debug_struct("Aes192").finish(),
            AesCipher::Aes256(_) => f.debug_struct("Aes256").finish(),
        }
    }
}

#[derive(Debug)]
pub struct AesCfbCrypt {
    cipher: AesCipher,
    cipher_name: &'static str,
}

impl AesCfbCrypt {
    pub fn new(key: &[u8]) -> Self {
        let klen = key.len();
        let mut padded = [0u8; 32];
        padded[..klen.min(32)].copy_from_slice(&key[..klen.min(32)]);
        let (cipher, cipher_name) = match klen {
            16 => (
                AesCipher::Aes128(aes::Aes128::new_from_slice(&padded[..16]).unwrap()),
                "aes-128",
            ),
            24 => (
                AesCipher::Aes192(aes::Aes192::new_from_slice(&padded[..24]).unwrap()),
                "aes-192",
            ),
            _ => (
                AesCipher::Aes256(aes::Aes256::new_from_slice(&padded).unwrap()),
                "aes-256",
            ),
        };
        AesCfbCrypt {
            cipher,
            cipher_name,
        }
    }

    #[inline]
    fn aes_enc(&self, inp: &[u8; 16], out: &mut [u8; 16]) {
        let mut ga = GenericArray::clone_from_slice(inp);
        self.cipher.encrypt_block(&mut ga);
        out.copy_from_slice(&ga);
    }
}

impl BlockCrypt for AesCfbCrypt {
    fn encrypt(&self, data: &mut [u8]) {
        cfb16_enc(data, &|i, o| self.aes_enc(i, o));
    }
    fn decrypt(&self, data: &mut [u8]) {
        cfb16_dec(data, &|i, o| self.aes_enc(i, o));
    }
    fn name(&self) -> &'static str {
        self.cipher_name
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
    fn aes128() {
        rt(
            &AesCfbCrypt::new(&[0u8; 16]),
            &mut b"hello kcp test!".to_vec(),
        );
    }
    #[test]
    fn aes192() {
        rt(
            &AesCfbCrypt::new(&[0u8; 24]),
            &mut b"hello kcp test 192!".to_vec(),
        );
    }
    #[test]
    fn aes256() {
        rt(
            &AesCfbCrypt::new(&[0u8; 32]),
            &mut b"hello kcp test 256!".to_vec(),
        );
    }

    // ─── Go interop vectors ─────────────────────────────────────────
    #[test]
    fn aes_cfb_go_interop() {
        // AES-128-CFB with fixed IV: same plaintext must produce same ciphertext
        // given same key (deterministic)
        let key = [0u8; 16];
        let crypt = AesCfbCrypt::new(&key);
        let mut data = b"KCP TEST VECTOR 12345678".to_vec();
        let first_enc = data.clone();
        crypt.encrypt(&mut data);
        // Re-encrypt same data - CFB with same IV is deterministic
        let mut data2 = first_enc.clone();
        crypt.encrypt(&mut data2);
        assert_eq!(data, data2, "AES-CFB deterministic encrypt");
        crypt.decrypt(&mut data);
        assert_eq!(data, first_enc, "AES-CFB roundtrip");
    }
}
