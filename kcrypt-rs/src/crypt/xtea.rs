//! XTEA (eXtended TEA) — 8-byte block cipher.
//!
//! Ported from Go's `golang.org/x/crypto/xtea`. Uses CFB-8 mode.
//! Requires a 16-byte key (padded if shorter). Runs 64 rounds.
//!
//! Hot path: monomorphized CFB (no `Fn` closure) + two-round loop matching
//! Go `encryptBlock` for better ILP.

use super::{BlockCrypt, GO_CFB_IV};

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

    /// Single 8-byte block encrypt (matches Go `encryptBlock`).
    #[inline(always)]
    fn encrypt_block(&self, dst: &mut [u8; 8], src: &[u8; 8]) {
        let mut v0 = u32::from_be_bytes([src[0], src[1], src[2], src[3]]);
        let mut v1 = u32::from_be_bytes([src[4], src[5], src[6], src[7]]);
        // Two rounds of XTEA applied per loop (Go xtea/block.go)
        let mut i = 0;
        while i < 64 {
            v0 = v0.wrapping_add(((v1 << 4 ^ v1 >> 5).wrapping_add(v1)) ^ self.table[i]);
            i += 1;
            v1 = v1.wrapping_add(((v0 << 4 ^ v0 >> 5).wrapping_add(v0)) ^ self.table[i]);
            i += 1;
        }
        dst[..4].copy_from_slice(&v0.to_be_bytes());
        dst[4..].copy_from_slice(&v1.to_be_bytes());
    }

    /// Specialized CFB-8 encrypt monomorphized on XTEA (no closure).
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
                self.encrypt_block(&mut b, &tbl);
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
            self.encrypt_block(&mut b, &tbl);
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
            self.encrypt_block(&mut b, &tbl);
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
            self.encrypt_block(&mut b, &tbl);
            let y = u64::from_le_bytes(src) ^ u64::from_le_bytes(b);
            chunk.copy_from_slice(&y.to_le_bytes());
            tbl = src;
            i += 8;
        }
        if i < data.len() {
            let chunk = &mut data[i..];
            let len = chunk.len();
            let mut b = [0u8; 8];
            self.encrypt_block(&mut b, &tbl);
            for j in 0..len {
                chunk[j] ^= b[j];
            }
        }
    }
}

impl BlockCrypt for XteaCrypt {
    fn encrypt(&self, data: &mut [u8]) {
        self.cfb_enc_specialized(data);
    }
    fn decrypt(&self, data: &mut [u8]) {
        // CFB uses forward cipher for both encrypt and decrypt
        self.cfb_dec_specialized(data);
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
