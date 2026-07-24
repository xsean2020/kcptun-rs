//! CAST5 (CAST-128) — 8-byte block cipher.
//!
//! Uses our own CAST5 implementation ported from Go's
//! `golang.org/x/crypto/cast5` (see [`crate::cast5`]). Wire-compatible
//! with Go's cast5 cipher. Operates in CFB-8 mode.

use super::{BlockCrypt, BlockCipher8, cfb8_decrypt, cfb8_encrypt};

#[derive(Debug)]
pub struct Cast5Crypt {
    cipher: crate::cast5::Cast5Cipher,
}

impl Cast5Crypt {
    pub fn new(key: &[u8]) -> Self {
        let cipher = crate::cast5::Cast5Cipher::new(key)
            .unwrap_or_else(|_| crate::cast5::Cast5Cipher::new(&[0u8; 16]).unwrap());
        Cast5Crypt { cipher }
    }
}

impl BlockCipher8 for Cast5Crypt {
    #[inline]
    fn encrypt_block(&self, out: &mut [u8; 8], inp: &[u8; 8]) {
        self.cipher.encrypt_block(out, inp);
    }
}

impl BlockCrypt for Cast5Crypt {
    fn encrypt(&self, data: &mut [u8]) {
        cfb8_encrypt(data, self);
    }
    fn decrypt(&self, data: &mut [u8]) {
        cfb8_decrypt(data, self);
    }
    fn name(&self) -> &'static str {
        "cast5"
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
    fn c5() {
        rt(
            &Cast5Crypt::new(b"test-key-12345"),
            &mut b"hello cast5".to_vec(),
        );
    }
}
