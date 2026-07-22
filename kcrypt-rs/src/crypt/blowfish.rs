//! Blowfish — 8-byte block cipher.
//!
//! Uses RustCrypto `blowfish` crate. Operates in CFB-8 mode.
//! The cipher instance is created ONCE in the constructor and reused for
//! every block — re-creating it per block (key schedule) was the original
//! performance bug that made Blowfish ~100x slower than Go.
//!
//! Hot path: monomorphized CFB (no `Fn` closure) + reuse of a single
//! `GenericArray` buffer for block encrypt.

use super::{BlockCrypt, GO_CFB_IV};
use blowfish::cipher::{generic_array::GenericArray, BlockEncrypt, KeyInit};
use blowfish::Blowfish;

#[derive(Debug)]
pub struct BlowfishCrypt {
    cipher: Blowfish,
}

impl BlowfishCrypt {
    pub fn new(key: &[u8]) -> Self {
        BlowfishCrypt {
            cipher: <Blowfish as KeyInit>::new_from_slice(key).expect("invalid blowfish key"),
        }
    }

    /// Encrypt one 8-byte block in place (CFB register / ciphertext feedback).
    #[inline(always)]
    fn encrypt_block_inplace(&self, block: &mut [u8; 8]) {
        let ga = GenericArray::from_mut_slice(block);
        self.cipher.encrypt_block(ga);
    }

    /// Specialized CFB-8 encrypt monomorphized on Blowfish.
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
                let mut b = tbl;
                self.encrypt_block_inplace(&mut b);
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
            let mut b = tbl;
            self.encrypt_block_inplace(&mut b);
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
            let mut b = tbl;
            self.encrypt_block_inplace(&mut b);
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
            let mut b = tbl;
            self.encrypt_block_inplace(&mut b);
            let y = u64::from_le_bytes(src) ^ u64::from_le_bytes(b);
            chunk.copy_from_slice(&y.to_le_bytes());
            tbl = src;
            i += 8;
        }
        if i < data.len() {
            let chunk = &mut data[i..];
            let len = chunk.len();
            let mut b = tbl;
            self.encrypt_block_inplace(&mut b);
            for j in 0..len {
                chunk[j] ^= b[j];
            }
        }
    }
}

impl BlockCrypt for BlowfishCrypt {
    fn encrypt(&self, data: &mut [u8]) {
        self.cfb_enc_specialized(data);
    }
    fn decrypt(&self, data: &mut [u8]) {
        self.cfb_dec_specialized(data);
    }
    fn name(&self) -> &'static str {
        "blowfish"
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
    fn bfish() {
        rt(
            &BlowfishCrypt::new(b"test-key"),
            &mut b"hello kcp bf!".to_vec(),
        );
    }
}
