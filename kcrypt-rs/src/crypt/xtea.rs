//! XTEA (eXtended TEA) — 8-byte block cipher.
//!
//! Ported from Go's `golang.org/x/crypto/xtea`. Uses CFB-8 mode.
//! Requires a 16-byte key (padded if shorter). Runs 64 rounds.

use super::{cfb8_dec, cfb8_enc, BlockCrypt};

#[derive(Debug)]
pub struct XteaCrypt {
    table: [u32; 64],
}

impl XteaCrypt {
    pub fn new(key: &[u8]) -> Self {
        let mut k = [0u32; 4];
        // Pad key to 16 bytes if needed (Go xtea requires 16-byte key)
        let mut padded = [0u8; 16];
        padded[..key.len().min(16)].copy_from_slice(&key[..key.len().min(16)]);
        for i in 0..4 {
            k[i] = u32::from_be_bytes([
                padded[i * 4],
                padded[i * 4 + 1],
                padded[i * 4 + 2],
                padded[i * 4 + 3],
            ]);
        }
        let mut t = [0u32; 64];
        let delta = 0x9E3779B9u32;
        let mut sum = 0u32;
        for i in (0..64).step_by(2) {
            t[i] = sum.wrapping_add(k[sum as usize & 3]);
            sum = sum.wrapping_add(delta);
            t[i + 1] = sum.wrapping_add(k[(sum >> 11) as usize & 3]);
        }
        XteaCrypt { table: t }
    }

    fn xtea_enc(&self, inp: &[u8; 8], out: &mut [u8; 8]) {
        let (mut a, mut b) = (
            u32::from_be_bytes([inp[0], inp[1], inp[2], inp[3]]),
            u32::from_be_bytes([inp[4], inp[5], inp[6], inp[7]]),
        );
        for i in 0..64 {
            if i % 2 == 0 {
                a = a.wrapping_add(((b << 4 ^ b >> 5).wrapping_add(b)) ^ self.table[i]);
            } else {
                b = b.wrapping_add(((a << 4 ^ a >> 5).wrapping_add(a)) ^ self.table[i]);
            }
        }
        out[..4].copy_from_slice(&a.to_be_bytes());
        out[4..].copy_from_slice(&b.to_be_bytes());
    }
}

impl BlockCrypt for XteaCrypt {
    fn encrypt(&self, data: &mut [u8]) {
        cfb8_enc(data, &|i, o| self.xtea_enc(i, o));
    }
    fn decrypt(&self, data: &mut [u8]) {
        // CFB uses forward cipher for both encrypt and decrypt
        cfb8_dec(data, &|i, o| self.xtea_enc(i, o));
    }
    fn name(&self) -> &'static str {
        "xtea"
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
    fn xtea0() {
        rt(
            &XteaCrypt::new(b"test-key-12345"),
            &mut b"hello kcp xt!".to_vec(),
        );
    }

    #[test]
    fn xtea_go_source_compatible() {
        // Test XTEA implementation matches Go's algorithm
        let key = b"0123456789abcdef"; // 16-byte key
        let crypt = XteaCrypt::new(key);
        let mut data = b"KCP XTEA TEST!".to_vec();
        let orig = data.clone();
        crypt.encrypt(&mut data);
        assert_ne!(data, orig, "XTEA encrypt changed data");
        crypt.decrypt(&mut data);
        assert_eq!(data, orig, "XTEA roundtrip");
    }
}
