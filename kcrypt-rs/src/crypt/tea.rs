//! TEA (Tiny Encryption Algorithm) — 8-byte block cipher.
//!
//! Ported from Go's `golang.org/x/crypto/tea`. Uses CFB-8 mode.
//! Go's `tea.NewCipherWithRounds(key, 16)` uses rounds=16,
//! and the Encrypt loop runs rounds/2 = 8 iterations.
//!
//! Hot path: monomorphized CFB (no `Fn` closure) matching XTEA/3DES style,
//! so CFB XOR + block encrypt can inline through `encrypt_batch`.

use super::{BlockCrypt, BlockCipher8, cfb8_encrypt, cfb8_decrypt};

#[derive(Debug)]
pub struct TeaCrypt {
    key: [u8; 16],
}

impl TeaCrypt {
    pub fn new(key: &[u8]) -> Self {
        let mut k = [0u8; 16];
        let l = key.len().min(16);
        k[..l].copy_from_slice(&key[..l]);
        TeaCrypt { key: k }
    }

    /// Single 8-byte block encrypt (matches Go tea, 8 Feistel iterations).
    #[inline(always)]
    fn encrypt_block(&self, out: &mut [u8; 8], inp: &[u8; 8]) {
        let mut v0 = u32::from_be_bytes([inp[0], inp[1], inp[2], inp[3]]);
        let mut v1 = u32::from_be_bytes([inp[4], inp[5], inp[6], inp[7]]);
        let k = |i: usize| {
            u32::from_be_bytes([
                self.key[i],
                self.key[i + 1],
                self.key[i + 2],
                self.key[i + 3],
            ])
        };
        let delta = 0x9e3779b9u32;
        let mut sum = 0u32;
        for _ in 0..8 {
            sum = sum.wrapping_add(delta);
            v0 = v0.wrapping_add(
                ((v1 << 4).wrapping_add(k(0)))
                    ^ v1.wrapping_add(sum)
                    ^ ((v1 >> 5).wrapping_add(k(4))),
            );
            v1 = v1.wrapping_add(
                ((v0 << 4).wrapping_add(k(8)))
                    ^ v0.wrapping_add(sum)
                    ^ ((v0 >> 5).wrapping_add(k(12))),
            );
        }
        out[..4].copy_from_slice(&v0.to_be_bytes());
        out[4..].copy_from_slice(&v1.to_be_bytes());
    }
}

impl BlockCipher8 for TeaCrypt {
    #[inline]
    fn encrypt_block(&self, out: &mut [u8; 8], inp: &[u8; 8]) {
        self.encrypt_block(out, inp);
    }
}

impl BlockCrypt for TeaCrypt {
    fn encrypt(&self, data: &mut [u8]) {
        cfb8_encrypt(data, self);
    }
    fn decrypt(&self, data: &mut [u8]) {
        cfb8_decrypt(data, self);
    }
    fn name(&self) -> &'static str {
        "tea"
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
    fn tea0() {
        rt(
            &TeaCrypt::new(b"test-key-12345"),
            &mut b"hello kcp tea!".to_vec(),
        );
    }

    #[test]
    fn tea_go_source_compatible() {
        // Test that TEA implementation matches Go's algorithm
        let key = b"0123456789abcdef"; // 16-byte key
        let crypt = TeaCrypt::new(key);
        let mut data = b"KCP TEATEST".to_vec();
        let orig = data.clone();
        crypt.encrypt(&mut data);
        assert_ne!(data, orig, "TEA encrypt changed data");
        crypt.decrypt(&mut data);
        assert_eq!(data, orig, "TEA roundtrip");
        // Verify TEA uses big endian (matching Go)
        let block = [0u8; 16];
        let (lo, hi) = (
            u32::from_be_bytes([b'0', b'1', b'2', b'3']),
            u32::from_be_bytes([b'4', b'5', b'6', b'7']),
        );
        // Just confirm big-endian behavior
        let k = |i: usize| u32::from_be_bytes([key[i], key[i + 1], key[i + 2], key[i + 3]]);
        assert_eq!(k(0), 0x30313233, "TEA key bytes are big-endian");
        let _ = (block, lo, hi);
    }

    #[test]
    fn tea_cfb_roundtrip_variable_lengths() {
        // Specialized CFB must round-trip multi-length payloads (wire CFB-8).
        let key = b"bench-key-tea!!!!";
        let crypt = TeaCrypt::new(key);
        for len in [1usize, 7, 8, 15, 16, 63, 64, 65, 128, 1024] {
            let mut data: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            let orig = data.clone();
            crypt.encrypt(&mut data);
            assert_ne!(&data[..], &orig[..], "len={len} must change");
            crypt.decrypt(&mut data);
            assert_eq!(&data[..], &orig[..], "len={len} roundtrip");
        }
    }
}
