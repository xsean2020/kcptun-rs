//! Table-based (T-table) software AES — encryption direction only.
//!
//! Purpose: the CFB-128 mode chains block-to-block (`ksᵢ = E(ksᵢ₋₁)`), so the
//! batch-of-8 fixslice backend behind the `aes` crate cannot be amortized and
//! a per-block `encrypt_block` call measures ~3× slower than a table
//! implementation on CPUs without AES-NI (2026-09-04 VM A/B: aes-CFB
//! rust-rust 4.4 vs go-go 16.9 MB/s, while aes-gcm — which batches — is at
//! parity). Go's `crypto/aes` uses exactly this T-table fallback
//! (`encryptBlockGo`) on such hosts, so this restores parity with the
//! wire-compat reference. The fixslice path remains for hardware-accelerated
//! builds (`aes_cfb.rs` picks at runtime).
//!
//! Security note: like Go's fallback, T-tables are not constant-time
//! (cache-timing side channel). This backend is selected only when the CPU
//! lacks hardware AES; on AES-NI/ARMv8 hosts the `aes` crate path is used.
//!
//! Only the encryption direction is implemented: CFB-128 mode (both encrypt
//! and decrypt, see `cfb16_encrypt`/`cfb16_decrypt`) uses `E_k` exclusively.

/// Round constants for key expansion.
const RCON: [u8; 10] = [0x01, 0x02, 0x04, 0x08, 0x10, 0x20, 0x40, 0x80, 0x1b, 0x36];

/// Forward S-box.
const SBOX: [u8; 256] = [
    0x63, 0x7c, 0x77, 0x7b, 0xf2, 0x6b, 0x6f, 0xc5, 0x30, 0x01, 0x67, 0x2b, 0xfe, 0xd7, 0xab, 0x76,
    0xca, 0x82, 0xc9, 0x7d, 0xfa, 0x59, 0x47, 0xf0, 0xad, 0xd4, 0xa2, 0xaf, 0x9c, 0xa4, 0x72, 0xc0,
    0xb7, 0xfd, 0x93, 0x26, 0x36, 0x3f, 0xf7, 0xcc, 0x34, 0xa5, 0xe5, 0xf1, 0x71, 0xd8, 0x31, 0x15,
    0x04, 0xc7, 0x23, 0xc3, 0x18, 0x96, 0x05, 0x9a, 0x07, 0x12, 0x80, 0xe2, 0xeb, 0x27, 0xb2, 0x75,
    0x09, 0x83, 0x2c, 0x1a, 0x1b, 0x6e, 0x5a, 0xa0, 0x52, 0x3b, 0xd6, 0xb3, 0x29, 0xe3, 0x2f, 0x84,
    0x53, 0xd1, 0x00, 0xed, 0x20, 0xfc, 0xb1, 0x5b, 0x6a, 0xcb, 0xbe, 0x39, 0x4a, 0x4c, 0x58, 0xcf,
    0xd0, 0xef, 0xaa, 0xfb, 0x43, 0x4d, 0x33, 0x85, 0x45, 0xf9, 0x02, 0x7f, 0x50, 0x3c, 0x9f, 0xa8,
    0x51, 0xa3, 0x40, 0x8f, 0x92, 0x9d, 0x38, 0xf5, 0xbc, 0xb6, 0xda, 0x21, 0x10, 0xff, 0xf3, 0xd2,
    0xcd, 0x0c, 0x13, 0xec, 0x5f, 0x97, 0x44, 0x17, 0xc4, 0xa7, 0x7e, 0x3d, 0x64, 0x5d, 0x19, 0x73,
    0x60, 0x81, 0x4f, 0xdc, 0x22, 0x2a, 0x90, 0x88, 0x46, 0xee, 0xb8, 0x14, 0xde, 0x5e, 0x0b, 0xdb,
    0xe0, 0x32, 0x3a, 0x0a, 0x49, 0x06, 0x24, 0x5c, 0xc2, 0xd3, 0xac, 0x62, 0x91, 0x95, 0xe4, 0x79,
    0xe7, 0xc8, 0x37, 0x6d, 0x8d, 0xd5, 0x4e, 0xa9, 0x6c, 0x56, 0xf4, 0xea, 0x65, 0x7a, 0xae, 0x08,
    0xba, 0x78, 0x25, 0x2e, 0x1c, 0xa6, 0xb4, 0xc6, 0xe8, 0xdd, 0x74, 0x1f, 0x4b, 0xbd, 0x8b, 0x8a,
    0x70, 0x3e, 0xb5, 0x66, 0x48, 0x03, 0xf6, 0x0e, 0x61, 0x35, 0x57, 0xb9, 0x86, 0xc1, 0x1d, 0x9e,
    0xe1, 0xf8, 0x98, 0x11, 0x69, 0xd9, 0x8e, 0x94, 0x9b, 0x1e, 0x87, 0xe9, 0xce, 0x55, 0x28, 0xdf,
    0x8c, 0xa1, 0x89, 0x0d, 0xbf, 0xe6, 0x42, 0x68, 0x41, 0x99, 0x2d, 0x0f, 0xb0, 0x54, 0xbb, 0x16,
];

/// Te tables: combined SubBytes + ShiftRows + MixColumns.
///
/// `TE[i][x] = [2·S, S, S, 3·S]` rotated left by i bytes, where S = SBOX[x].
/// Built from the S-box at first use (one-time ~1µs; keeps the binary free of
/// 4 KB of constants and avoids any table-generation drift).
struct Tables {
    te: [[u32; 256]; 4],
}

impl Tables {
    fn new() -> Self {
        let mut te = [[0u32; 256]; 4];
        for (x, &s) in SBOX.iter().enumerate() {
            let s = s as u32;
            let s2 = gf2mul(s, 2);
            let s3 = gf2mul(s, 3);
            // MixColumns row-0 coefficients applied to an input byte x:
            // [2,1,1,3] → BE word [2·s, s, s, 3·s]. Rows 1-3 of the matrix are
            // byte-rotations of row 0, so the other tables are the same word
            // rotated right (moving each coefficient down one state row).
            let w0 = (s2 << 24) | (s << 16) | (s << 8) | s3;
            te[0][x] = w0;
            te[1][x] = w0.rotate_right(8);
            te[2][x] = w0.rotate_right(16);
            te[3][x] = w0.rotate_right(24);
        }
        Tables { te }
    }
}

/// GF(2^8) multiply with the AES modulus x^8+x^4+x^3+x+1 (0x11b).
#[inline(always)]
fn gf2mul(mut a: u32, mut b: u32) -> u32 {
    let mut p = 0u32;
    let mut i = 0;
    while i < 8 {
        if b & 1 != 0 {
            p ^= a;
        }
        let hi = a & 0x80;
        a = (a << 1) & 0xff;
        if hi != 0 {
            a ^= 0x1b;
        }
        b >>= 1;
        i += 1;
    }
    p
}

/// Static tables, initialized once. `OnceLock` keeps the hot path lock-free
/// (tables are read-only after init).
static TABLES: std::sync::OnceLock<Tables> = std::sync::OnceLock::new();

fn tables() -> &'static Tables {
    TABLES.get_or_init(Tables::new)
}

/// Encryption round-key schedule; `w[0..4]` is the initial round key.
pub struct SoftAesEnc {
    /// 44/52/60 words for AES-128/192/256.
    w: Vec<u32>,
    nr: usize, // rounds: 10/12/14
}

impl SoftAesEnc {
    pub fn new(key: &[u8]) -> Self {
        match key.len() {
            16 => Self::expand(key, 4, 10),
            24 => Self::expand(key, 6, 12),
            32 => Self::expand(key, 8, 14),
            _ => panic!("soft aes: unsupported key length {}", key.len()),
        }
    }

    /// Standard Rijndael key expansion (FIPS-197), word-wise.
    fn expand(key: &[u8], nk: usize, nr: usize) -> Self {
        let mut w = vec![0u32; 4 * (nr + 1)];
        for (i, word) in w.iter_mut().enumerate().take(nk) {
            *word =
                u32::from_be_bytes([key[4 * i], key[4 * i + 1], key[4 * i + 2], key[4 * i + 3]]);
        }
        let mut rcon_i = 0;
        let mut i = nk;
        while i < 4 * (nr + 1) {
            let mut temp = w[i - 1];
            if i.is_multiple_of(nk) {
                // RotWord + SubWord + Rcon
                temp = temp.rotate_left(8);
                temp = u32::from_be_bytes([
                    SBOX[(temp >> 24) as usize],
                    SBOX[((temp >> 16) & 0xff) as usize],
                    SBOX[((temp >> 8) & 0xff) as usize],
                    SBOX[(temp & 0xff) as usize],
                ]);
                temp ^= (RCON[rcon_i] as u32) << 24;
                rcon_i += 1;
            } else if nk > 6 && i % nk == 4 {
                // AES-256 only: SubWord
                temp = u32::from_be_bytes([
                    SBOX[(temp >> 24) as usize],
                    SBOX[((temp >> 16) & 0xff) as usize],
                    SBOX[((temp >> 8) & 0xff) as usize],
                    SBOX[(temp & 0xff) as usize],
                ]);
            }
            w[i] = w[i - nk] ^ temp;
            i += 1;
        }
        SoftAesEnc { w, nr }
    }

    /// Encrypt one 16-byte block: `out = E(inp)`.
    #[inline]
    pub fn encrypt_block(&self, out: &mut [u8; 16], inp: &[u8; 16]) {
        let te = &tables().te;
        // State as four big-endian 32-bit columns, whitened by the first key.
        let mut s0 = u32::from_be_bytes([inp[0], inp[1], inp[2], inp[3]]) ^ self.w[0];
        let mut s1 = u32::from_be_bytes([inp[4], inp[5], inp[6], inp[7]]) ^ self.w[1];
        let mut s2 = u32::from_be_bytes([inp[8], inp[9], inp[10], inp[11]]) ^ self.w[2];
        let mut s3 = u32::from_be_bytes([inp[12], inp[13], inp[14], inp[15]]) ^ self.w[3];

        // Rounds 1..nr: SubBytes+ShiftRows+MixColumns via T-tables. ShiftRows
        // is folded into the table-row selection: column c takes s[(c+r) % 4].
        for r in 1..self.nr {
            let k = 4 * r;
            let n0 = te[0][(s0 >> 24) as usize]
                ^ te[1][((s1 >> 16) & 0xff) as usize]
                ^ te[2][((s2 >> 8) & 0xff) as usize]
                ^ te[3][(s3 & 0xff) as usize]
                ^ self.w[k];
            let n1 = te[0][(s1 >> 24) as usize]
                ^ te[1][((s2 >> 16) & 0xff) as usize]
                ^ te[2][((s3 >> 8) & 0xff) as usize]
                ^ te[3][(s0 & 0xff) as usize]
                ^ self.w[k + 1];
            let n2 = te[0][(s2 >> 24) as usize]
                ^ te[1][((s3 >> 16) & 0xff) as usize]
                ^ te[2][((s0 >> 8) & 0xff) as usize]
                ^ te[3][(s1 & 0xff) as usize]
                ^ self.w[k + 2];
            let n3 = te[0][(s3 >> 24) as usize]
                ^ te[1][((s0 >> 16) & 0xff) as usize]
                ^ te[2][((s1 >> 8) & 0xff) as usize]
                ^ te[3][(s2 & 0xff) as usize]
                ^ self.w[k + 3];
            s0 = n0;
            s1 = n1;
            s2 = n2;
            s3 = n3;
        }

        // Final round: SubBytes + ShiftRows + AddRoundKey, no MixColumns.
        // Byte j of output column c = SBOX of byte j of state column (c+j)%4.
        let k = 4 * self.nr;
        #[inline(always)]
        fn sub(x: u32) -> u32 {
            SBOX[x as usize] as u32
        }
        let n0 = (sub(s0 >> 24) << 24)
            ^ (sub((s1 >> 16) & 0xff) << 16)
            ^ (sub((s2 >> 8) & 0xff) << 8)
            ^ sub(s3 & 0xff)
            ^ self.w[k];
        let n1 = (sub(s1 >> 24) << 24)
            ^ (sub((s2 >> 16) & 0xff) << 16)
            ^ (sub((s3 >> 8) & 0xff) << 8)
            ^ sub(s0 & 0xff)
            ^ self.w[k + 1];
        let n2 = (sub(s2 >> 24) << 24)
            ^ (sub((s3 >> 16) & 0xff) << 16)
            ^ (sub((s0 >> 8) & 0xff) << 8)
            ^ sub(s1 & 0xff)
            ^ self.w[k + 2];
        let n3 = (sub(s3 >> 24) << 24)
            ^ (sub((s0 >> 16) & 0xff) << 16)
            ^ (sub((s1 >> 8) & 0xff) << 8)
            ^ sub(s2 & 0xff)
            ^ self.w[k + 3];

        out[0..4].copy_from_slice(&n0.to_be_bytes());
        out[4..8].copy_from_slice(&n1.to_be_bytes());
        out[8..12].copy_from_slice(&n2.to_be_bytes());
        out[12..16].copy_from_slice(&n3.to_be_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FIPS-197 Appendix C.1: AES-128 known-answer test.
    #[test]
    fn fips197_aes128() {
        let key = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f,
        ];
        let pt = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ];
        let want = [
            0x69, 0xc4, 0xe0, 0xd8, 0x6a, 0x7b, 0x04, 0x30, 0xd8, 0xcd, 0xb7, 0x80, 0x70, 0xb4,
            0xc5, 0x5a,
        ];
        let mut out = [0u8; 16];
        SoftAesEnc::new(&key).encrypt_block(&mut out, &pt);
        assert_eq!(out, want);
    }

    /// FIPS-197 Appendix C.2: AES-192 known-answer test.
    #[test]
    fn fips197_aes192() {
        let key = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17,
        ];
        let pt = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ];
        let want = [
            0xdd, 0xa9, 0x7c, 0xa4, 0x86, 0x4c, 0xdf, 0xe0, 0x6e, 0xaf, 0x70, 0xa0, 0xec, 0x0d,
            0x71, 0x91,
        ];
        let mut out = [0u8; 16];
        SoftAesEnc::new(&key).encrypt_block(&mut out, &pt);
        assert_eq!(out, want);
    }

    /// FIPS-197 Appendix C.3: AES-256 known-answer test.
    #[test]
    fn fips197_aes256() {
        let key = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b,
            0x1c, 0x1d, 0x1e, 0x1f,
        ];
        let pt = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ];
        let want = [
            0x8e, 0xa2, 0xb7, 0xca, 0x51, 0x67, 0x45, 0xbf, 0xea, 0xfc, 0x49, 0x90, 0x4b, 0x49,
            0x60, 0x89,
        ];
        let mut out = [0u8; 16];
        SoftAesEnc::new(&key).encrypt_block(&mut out, &pt);
        assert_eq!(out, want);
    }

    /// Cross-check against the `aes` crate on random inputs (the two backends
    /// must be bit-identical — same algorithm, different cost model).
    #[test]
    fn matches_aes_crate_random() {
        use aes::cipher::generic_array::GenericArray;
        use aes::cipher::KeyInit;
        let mut out_soft = [0u8; 16];
        let mut out_ref = [0u8; 16];
        let mut state: u64 = 0x243f6a8885a308d3;
        fn next(state: &mut u64) -> u64 {
            // xorshift64* — enough randomness for a smoke cross-check.
            *state ^= *state >> 12;
            *state ^= *state << 25;
            *state ^= *state >> 27;
            state.wrapping_mul(0x2545F4914F6CDD1D)
        }
        for klen in [16usize, 24, 32] {
            for _ in 0..64 {
                let mut key = [0u8; 32];
                for b in key.iter_mut() {
                    *b = (next(&mut state) & 0xff) as u8;
                }
                let mut block = [0u8; 16];
                for b in block.iter_mut() {
                    *b = (next(&mut state) & 0xff) as u8;
                }
                SoftAesEnc::new(&key[..klen]).encrypt_block(&mut out_soft, &block);
                let mut ga = GenericArray::clone_from_slice(&block);
                match klen {
                    16 => {
                        let c = aes::Aes128::new_from_slice(&key[..16]).unwrap();
                        aes::cipher::BlockEncrypt::encrypt_block(&c, &mut ga);
                    }
                    24 => {
                        let c = aes::Aes192::new_from_slice(&key[..24]).unwrap();
                        aes::cipher::BlockEncrypt::encrypt_block(&c, &mut ga);
                    }
                    _ => {
                        let c = aes::Aes256::new_from_slice(&key).unwrap();
                        aes::cipher::BlockEncrypt::encrypt_block(&c, &mut ga);
                    }
                }
                out_ref.copy_from_slice(&ga);
                assert_eq!(out_soft, out_ref, "klen={klen} block={block:?}");
            }
        }
    }
}
