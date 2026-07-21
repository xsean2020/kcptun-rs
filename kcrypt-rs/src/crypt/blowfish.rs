//! Blowfish — 8-byte block cipher.
//!
//! Uses RustCrypto `blowfish` crate. Operates in CFB-8 mode.
//! The cipher instance is created ONCE in the constructor and reused for
//! every block — re-creating it per block (key schedule) was the original
//! performance bug that made Blowfish ~100x slower than Go.

use super::{cfb8_dec, cfb8_enc, BlockCrypt};
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

    #[inline]
    fn bf_enc(&self, inp: &[u8; 8], out: &mut [u8; 8]) {
        let mut ga = GenericArray::clone_from_slice(inp);
        self.cipher.encrypt_block(&mut ga);
        out.copy_from_slice(&ga);
    }
}

impl BlockCrypt for BlowfishCrypt {
    fn encrypt(&self, data: &mut [u8]) {
        cfb8_enc(data, &|i, o| self.bf_enc(i, o));
    }
    fn decrypt(&self, data: &mut [u8]) {
        cfb8_dec(data, &|i, o| self.bf_enc(i, o));
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
