//! Simple XOR cipher with PBKDF2 key expansion.
//!
//! The password is expanded to a 1500-byte key via PBKDF2-HMAC-SHA1
//! (32 iterations, fixed salt). Encryption/decryption is a simple
//! repeating-key XOR — symmetric and very fast but cryptographically weak.
//!
//! Uses 64-bit (u64) XOR for 8× throughput, matching Go's
//! `subtle.XORBytes` word-size approach.

use super::{BlockCrypt, SALT_XOR};

#[derive(Debug)]
pub struct SimpleXORCrypt {
    key: Vec<u8>,
}

impl SimpleXORCrypt {
    pub fn new(key: &[u8]) -> Self {
        let mut k = vec![0u8; 1500];
        let _ = pbkdf2::pbkdf2::<hmac::Hmac<sha1::Sha1>>(key, SALT_XOR.as_bytes(), 32, &mut k);
        SimpleXORCrypt { key: k }
    }

    /// XOR data with key in-place. Uses u64 chunks for speed.
    #[inline]
    fn xor_inplace(d: &mut [u8], key: &[u8]) {
        let n = d.len().min(key.len());
        let mut i = 0;
        // Fast path: 8-byte u64 XOR (matching Go's subtle.XORBytes word-size)
        while i + 8 <= n {
            let k = u64::from_le_bytes(key[i..i + 8].try_into().unwrap());
            let v = u64::from_le_bytes(d[i..i + 8].try_into().unwrap());
            d[i..i + 8].copy_from_slice(&(v ^ k).to_le_bytes());
            i += 8;
        }
        // Tail bytes
        while i < n {
            d[i] ^= key[i];
            i += 1;
        }
        // Wrap if data longer than key (shouldn't happen: key=1500, MTU=1350)
        while i < d.len() {
            d[i] ^= key[i % key.len()];
            i += 1;
        }
    }
}

impl BlockCrypt for SimpleXORCrypt {
    fn encrypt(&self, d: &mut [u8]) {
        Self::xor_inplace(d, &self.key);
    }
    fn decrypt(&self, d: &mut [u8]) {
        // XOR is symmetric
        Self::xor_inplace(d, &self.key);
    }
    fn name(&self) -> &'static str {
        "xor"
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
    fn xor0() {
        rt(&SimpleXORCrypt::new(b"key"), &mut b"hello kcp".to_vec());
    }
    #[test]
    fn xor_large() {
        // Test with data larger than 8 bytes to exercise u64 path
        let data: Vec<u8> = (0..200u8).collect();
        rt(&SimpleXORCrypt::new(b"key"), &mut data.clone());
    }
    #[test]
    fn xor_wrap() {
        // Test with data larger than key (1500 bytes) to exercise wrap path
        let data = vec![0xABu8; 2000];
        rt(&SimpleXORCrypt::new(b"key"), &mut data.clone());
    }
}
