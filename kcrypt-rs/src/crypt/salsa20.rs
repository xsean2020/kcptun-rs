//! Salsa20 stream cipher (32-byte key).
//!
//! Ported from Go's `golang.org/x/crypto/salsa20`. The first 8 bytes of
//! the plaintext are used as the nonce (and left unchanged); the keystream
//! starts at offset 8 and XORs the remaining bytes.

use super::BlockCrypt;

#[derive(Debug)]
pub struct Salsa20Crypt {
    key: [u8; 32],
}

impl Salsa20Crypt {
    pub fn new(key: &[u8]) -> Self {
        let mut k = [0u8; 32];
        let l = key.len().min(32);
        k[..l].copy_from_slice(&key[..l]);
        Salsa20Crypt { key: k }
    }
}

#[inline]
fn sqr(a: &mut u32, b: &mut u32, c: &mut u32, d: &mut u32) {
    *b ^= a.wrapping_add(*d).rotate_left(7);
    *c ^= b.wrapping_add(*a).rotate_left(9);
    *d ^= c.wrapping_add(*b).rotate_left(13);
    *a ^= d.wrapping_add(*c).rotate_left(18);
}

#[inline]
fn u32from(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}

fn saltwenty(key: &[u8; 32], nonce: &[u8; 8], ctr: u64, out: &mut [u8; 64]) {
    // State matrix layout matching Go's golang.org/x/crypto/salsa20/salsa/salsa20_ref.go
    // Go uses: j0=c0, j1=k0, j2=k1, j3=k2, j4=k3, j5=c1,
    //          j6=n0, j7=n1, j8=ctr_lo, j9=ctr_hi, j10=c2,
    //          j11=k4, j12=k5, j13=k6, j14=k7, j15=c3
    let s = 0x61707865u32;
    let (
        mut x0,
        mut x1,
        mut x2,
        mut x3,
        mut x4,
        mut x5,
        mut x6,
        mut x7,
        mut x8,
        mut x9,
        mut x10,
        mut x11,
        mut x12,
        mut x13,
        mut x14,
        mut x15,
    ) = (
        s,
        u32from(key, 0),
        u32from(key, 4),
        u32from(key, 8),
        u32from(key, 12),
        0x3320646e,
        u32from(nonce, 0),
        u32from(nonce, 4),
        (ctr & 0xFFFFFFFF) as u32,
        (ctr >> 32) as u32,
        0x79622d32,
        u32from(key, 16),
        u32from(key, 20),
        u32from(key, 24),
        u32from(key, 28),
        0x6b206574,
    );
    let (i0, i1, i2, i3, i4, i5, i6, i7, i8, i9, i10, i11, i12, i13, i14, i15) = (
        x0, x1, x2, x3, x4, x5, x6, x7, x8, x9, x10, x11, x12, x13, x14, x15,
    );
    for _ in 0..10 {
        sqr(&mut x0, &mut x4, &mut x8, &mut x12);
        sqr(&mut x5, &mut x9, &mut x13, &mut x1);
        sqr(&mut x10, &mut x14, &mut x2, &mut x6);
        sqr(&mut x15, &mut x3, &mut x7, &mut x11);
        sqr(&mut x0, &mut x1, &mut x2, &mut x3);
        sqr(&mut x5, &mut x6, &mut x7, &mut x4);
        sqr(&mut x10, &mut x11, &mut x8, &mut x9);
        sqr(&mut x15, &mut x12, &mut x13, &mut x14);
    }
    let v = [
        x0.wrapping_add(i0),
        x1.wrapping_add(i1),
        x2.wrapping_add(i2),
        x3.wrapping_add(i3),
        x4.wrapping_add(i4),
        x5.wrapping_add(i5),
        x6.wrapping_add(i6),
        x7.wrapping_add(i7),
        x8.wrapping_add(i8),
        x9.wrapping_add(i9),
        x10.wrapping_add(i10),
        x11.wrapping_add(i11),
        x12.wrapping_add(i12),
        x13.wrapping_add(i13),
        x14.wrapping_add(i14),
        x15.wrapping_add(i15),
    ];
    for i in 0..16 {
        out[i * 4..(i + 1) * 4].copy_from_slice(&v[i].to_le_bytes());
    }
}

impl BlockCrypt for Salsa20Crypt {
    fn encrypt(&self, data: &mut [u8]) {
        if data.is_empty() {
            return;
        }
        let mut nonce = [0u8; 8];
        let nlen = data.len().min(8);
        nonce[..nlen].copy_from_slice(&data[..nlen]);
        let mut ctr = 0u64;
        let mut ks = [0u8; 64];
        let mut off = 8usize;
        while off < data.len() {
            saltwenty(&self.key, &nonce, ctr, &mut ks);
            let end = (off + 64).min(data.len());
            for j in off..end {
                data[j] ^= ks[j - off];
            }
            ctr += 1;
            off += 64;
        }
    }
    fn decrypt(&self, data: &mut [u8]) {
        // Salsa20 is symmetric
        self.encrypt(data);
    }
    fn name(&self) -> &'static str {
        "salsa20"
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
    fn salsa() {
        let msg = b"hello kcp salsa test!";
        let mut d = vec![0u8; 8 + msg.len()];
        d[8..].copy_from_slice(msg);
        rt(&Salsa20Crypt::new(b"test-key-12345-test-key-67890"), &mut d);
    }

    #[test]
    fn salsa20_go_source_compatible() {
        // Test Salsa20 matches Go's algorithm
        let key = b"test-key-12345-test-key-67890";
        let crypt = Salsa20Crypt::new(key);
        let msg = b"TEST VECTOR FOR SALSA20!!";
        let mut data = vec![0u8; 8 + msg.len()];
        data[8..].copy_from_slice(msg);
        let orig = data.clone();
        crypt.encrypt(&mut data);
        assert_eq!(data[..8], orig[..8], "Salsa20 nonce unchanged");
        crypt.decrypt(&mut data);
        assert_eq!(data, orig, "Salsa20 roundtrip");
    }
}
