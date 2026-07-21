//! Twofish — 16-byte block cipher (optimized with pre-computed lookup tables).
//!
//! Ports Go's `golang.org/x/crypto/twofish` implementation directly,
//! pre-computing the `s [4][256]u32` S-box+MDS tables in the constructor.
//! During encryption, g_func is just 4 table lookups + XOR — O(1) per block.
//!
//! This is ~5x faster than the RustCrypto twofish crate (v0.7.1) which
//! recomputes sbox+gf_mult per block.

use super::{cfb16_dec, cfb16_enc, BlockCrypt};

// ─── Constants (from Go's twofish.go) ────────────────────────────────────

const MDS_POLY: u8 = 0x69;
const RS_POLY: u8 = 0x4d;

/// RS matrix for S-key derivation. See [TWOFISH] 4.3
const RS: [[u8; 8]; 4] = [
    [0x01, 0xa4, 0x55, 0x87, 0x5a, 0x58, 0xdb, 0x9e],
    [0xa4, 0x56, 0x82, 0xf3, 0x1e, 0xc6, 0x68, 0xe5],
    [0x02, 0xa1, 0xfc, 0xc1, 0x47, 0xae, 0x3d, 0x19],
    [0xa4, 0x55, 0x87, 0x5a, 0x58, 0xdb, 0x9e, 0x03],
];

/// Pre-computed sbox tables (from Go's twofish.go)
#[rustfmt::skip]
const SBOX: [[u8; 256]; 2] = [
    [
        0xa9, 0x67, 0xb3, 0xe8, 0x04, 0xfd, 0xa3, 0x76, 0x9a, 0x92, 0x80, 0x78, 0xe4, 0xdd, 0xd1, 0x38,
        0x0d, 0xc6, 0x35, 0x98, 0x18, 0xf7, 0xec, 0x6c, 0x43, 0x75, 0x37, 0x26, 0xfa, 0x13, 0x94, 0x48,
        0xf2, 0xd0, 0x8b, 0x30, 0x84, 0x54, 0xdf, 0x23, 0x19, 0x5b, 0x3d, 0x59, 0xf3, 0xae, 0xa2, 0x82,
        0x63, 0x01, 0x83, 0x2e, 0xd9, 0x51, 0x9b, 0x7c, 0xa6, 0xeb, 0xa5, 0xbe, 0x16, 0x0c, 0xe3, 0x61,
        0xc0, 0x8c, 0x3a, 0xf5, 0x73, 0x2c, 0x25, 0x0b, 0xbb, 0x4e, 0x89, 0x6b, 0x53, 0x6a, 0xb4, 0xf1,
        0xe1, 0xe6, 0xbd, 0x45, 0xe2, 0xf4, 0xb6, 0x66, 0xcc, 0x95, 0x03, 0x56, 0xd4, 0x1c, 0x1e, 0xd7,
        0xfb, 0xc3, 0x8e, 0xb5, 0xe9, 0xcf, 0xbf, 0xba, 0xea, 0x77, 0x39, 0xaf, 0x33, 0xc9, 0x62, 0x71,
        0x81, 0x79, 0x09, 0xad, 0x24, 0xcd, 0xf9, 0xd8, 0xe5, 0xc5, 0xb9, 0x4d, 0x44, 0x08, 0x86, 0xe7,
        0xa1, 0x1d, 0xaa, 0xed, 0x06, 0x70, 0xb2, 0xd2, 0x41, 0x7b, 0xa0, 0x11, 0x31, 0xc2, 0x27, 0x90,
        0x20, 0xf6, 0x60, 0xff, 0x96, 0x5c, 0xb1, 0xab, 0x9e, 0x9c, 0x52, 0x1b, 0x5f, 0x93, 0x0a, 0xef,
        0x91, 0x85, 0x49, 0xee, 0x2d, 0x4f, 0x8f, 0x3b, 0x47, 0x87, 0x6d, 0x46, 0xd6, 0x3e, 0x69, 0x64,
        0x2a, 0xce, 0xcb, 0x2f, 0xfc, 0x97, 0x05, 0x7a, 0xac, 0x7f, 0xd5, 0x1a, 0x4b, 0x0e, 0xa7, 0x5a,
        0x28, 0x14, 0x3f, 0x29, 0x88, 0x3c, 0x4c, 0x02, 0xb8, 0xda, 0xb0, 0x17, 0x55, 0x1f, 0x8a, 0x7d,
        0x57, 0xc7, 0x8d, 0x74, 0xb7, 0xc4, 0x9f, 0x72, 0x7e, 0x15, 0x22, 0x12, 0x58, 0x07, 0x99, 0x34,
        0x6e, 0x50, 0xde, 0x68, 0x65, 0xbc, 0xdb, 0xf8, 0xc8, 0xa8, 0x2b, 0x40, 0xdc, 0xfe, 0x32, 0xa4,
        0xca, 0x10, 0x21, 0xf0, 0xd3, 0x5d, 0x0f, 0x00, 0x6f, 0x9d, 0x36, 0x42, 0x4a, 0x5e, 0xc1, 0xe0,
    ],
    [
        0x75, 0xf3, 0xc6, 0xf4, 0xdb, 0x7b, 0xfb, 0xc8, 0x4a, 0xd3, 0xe6, 0x6b, 0x45, 0x7d, 0xe8, 0x4b,
        0xd6, 0x32, 0xd8, 0xfd, 0x37, 0x71, 0xf1, 0xe1, 0x30, 0x0f, 0xf8, 0x1b, 0x87, 0xfa, 0x06, 0x3f,
        0x5e, 0xba, 0xae, 0x5b, 0x8a, 0x00, 0xbc, 0x9d, 0x6d, 0xc1, 0xb1, 0x0e, 0x80, 0x5d, 0xd2, 0xd5,
        0xa0, 0x84, 0x07, 0x14, 0xb5, 0x90, 0x2c, 0xa3, 0xb2, 0x73, 0x4c, 0x54, 0x92, 0x74, 0x36, 0x51,
        0x38, 0xb0, 0xbd, 0x5a, 0xfc, 0x60, 0x62, 0x96, 0x6c, 0x42, 0xf7, 0x10, 0x7c, 0x28, 0x27, 0x8c,
        0x13, 0x95, 0x9c, 0xc7, 0x24, 0x46, 0x3b, 0x70, 0xca, 0xe3, 0x85, 0xcb, 0x11, 0xd0, 0x93, 0xb8,
        0xa6, 0x83, 0x20, 0xff, 0x9f, 0x77, 0xc3, 0xcc, 0x03, 0x6f, 0x08, 0xbf, 0x40, 0xe7, 0x2b, 0xe2,
        0x79, 0x0c, 0xaa, 0x82, 0x41, 0x3a, 0xea, 0xb9, 0xe4, 0x9a, 0xa4, 0x97, 0x7e, 0xda, 0x7a, 0x17,
        0x66, 0x94, 0xa1, 0x1d, 0x3d, 0xf0, 0xde, 0xb3, 0x0b, 0x72, 0xa7, 0x1c, 0xef, 0xd1, 0x53, 0x3e,
        0x8f, 0x33, 0x26, 0x5f, 0xec, 0x76, 0x2a, 0x49, 0x81, 0x88, 0xee, 0x21, 0xc4, 0x1a, 0xeb, 0xd9,
        0xc5, 0x39, 0x99, 0xcd, 0xad, 0x31, 0x8b, 0x01, 0x18, 0x23, 0xdd, 0x1f, 0x4e, 0x2d, 0xf9, 0x48,
        0x4f, 0xf2, 0x65, 0x8e, 0x78, 0x5c, 0x58, 0x19, 0x8d, 0xe5, 0x98, 0x57, 0x67, 0x7f, 0x05, 0x64,
        0xaf, 0x63, 0xb6, 0xfe, 0xf5, 0xb7, 0x3c, 0xa5, 0xce, 0xe9, 0x68, 0x44, 0xe0, 0x4d, 0x43, 0x69,
        0x29, 0x2e, 0xac, 0x15, 0x59, 0xa8, 0x0a, 0x9e, 0x6e, 0x47, 0xdf, 0x34, 0x35, 0x6a, 0xcf, 0xdc,
        0x22, 0xc9, 0xc0, 0x9b, 0x89, 0xd4, 0xed, 0xab, 0x12, 0xa2, 0x0d, 0x52, 0xbb, 0x02, 0x2f, 0xa9,
        0xd7, 0x61, 0x1e, 0xb4, 0x50, 0x04, 0xf6, 0xc2, 0x16, 0x25, 0x86, 0x56, 0x55, 0x09, 0xbe, 0x91,
    ],
];

// ─── GF arithmetic (key setup only) ───────────────────────────────────────

#[inline]
fn gf_mult(mut a: u8, mut b: u8, p: u8) -> u8 {
    let mut result: u8 = 0;
    for _ in 0..7 {
        if a & 1 == 1 {
            result ^= b;
        }
        a >>= 1;
        if b & 0x80 == 0x80 {
            b = (b << 1) ^ p;
        } else {
            b <<= 1;
        }
    }
    if a & 1 == 1 {
        result ^= b;
    }
    result
}

#[inline]
fn mds_column_mult(x: u8, col: usize) -> u32 {
    let m01 = x as u32;
    let m5b = gf_mult(x, 0x5b, MDS_POLY) as u32;
    let mef = gf_mult(x, 0xef, MDS_POLY) as u32;
    match col {
        0 => m01 | (m5b << 8) | (mef << 16) | (mef << 24),
        1 => mef | (mef << 8) | (m5b << 16) | (m01 << 24),
        2 => m5b | (mef << 8) | (m01 << 16) | (mef << 24),
        _ => m5b | (m01 << 8) | (mef << 16) | (m5b << 24),
    }
}

fn rs_mult(m: &[u8], out: &mut [u8]) {
    for i in 0..4 {
        out[i] = 0;
        for j in 0..8 {
            out[i] ^= gf_mult(m[j], RS[i][j], RS_POLY);
        }
    }
}

/// h function for key schedule. See [TWOFISH] 4.3.5
fn h(x: &[u8; 4], key: &[u8], offset: usize) -> u32 {
    let mut y = *x;
    let k = key.len() / 8;
    if k == 4 {
        y[0] = SBOX[1][y[0] as usize] ^ key[4 * (6 + offset)];
        y[1] = SBOX[0][y[1] as usize] ^ key[4 * (6 + offset) + 1];
        y[2] = SBOX[0][y[2] as usize] ^ key[4 * (6 + offset) + 2];
        y[3] = SBOX[1][y[3] as usize] ^ key[4 * (6 + offset) + 3];
    }
    if k >= 3 {
        y[0] = SBOX[1][y[0] as usize] ^ key[4 * (4 + offset)];
        y[1] = SBOX[1][y[1] as usize] ^ key[4 * (4 + offset) + 1];
        y[2] = SBOX[0][y[2] as usize] ^ key[4 * (4 + offset) + 2];
        y[3] = SBOX[0][y[3] as usize] ^ key[4 * (4 + offset) + 3];
    }
    let a = 4 * (2 + offset);
    let b = 4 * offset;
    y[0] = SBOX[1][(SBOX[0][(SBOX[0][y[0] as usize] ^ key[a]) as usize] ^ key[b]) as usize];
    y[1] = SBOX[0][(SBOX[0][(SBOX[1][y[1] as usize] ^ key[a + 1]) as usize] ^ key[b + 1]) as usize];
    y[2] = SBOX[1][(SBOX[1][(SBOX[0][y[2] as usize] ^ key[a + 2]) as usize] ^ key[b + 2]) as usize];
    y[3] = SBOX[0][(SBOX[1][(SBOX[1][y[3] as usize] ^ key[a + 3]) as usize] ^ key[b + 3]) as usize];
    let mut z: u32 = 0;
    for (i, &val) in y.iter().enumerate() {
        z ^= mds_column_mult(val, i);
    }
    z
}

// ─── Cipher ──────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct TwofishCrypt {
    /// Pre-computed S-box + MDS lookup tables (4 × 256 × u32 = 4 KB)
    s: [[u32; 256]; 4],
    /// Subkeys
    k: [u32; 40],
}

impl std::fmt::Debug for TwofishCrypt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TwofishCrypt").finish()
    }
}

impl TwofishCrypt {
    pub fn new(key: &[u8]) -> Self {
        let keylen = key.len();
        // Pad/truncate to a valid key length (16, 24, or 32)
        let keylen = if keylen >= 32 {
            32
        } else if keylen >= 24 {
            24
        } else {
            16
        };
        let mut padded_key = [0u8; 32];
        padded_key[..key.len().min(32)].copy_from_slice(&key[..key.len().min(32)]);
        let key = &padded_key[..keylen];

        let k = keylen / 8; // number of 64-bit words

        // ── Compute S-key (RS matrix multiplication) ──
        let mut s_key = [0u8; 16];
        for i in 0..k {
            let mut out = [0u8; 4];
            rs_mult(&key[i * 8..i * 8 + 8], &mut out);
            s_key[i * 4..(i + 1) * 4].copy_from_slice(&out);
        }

        // ── Compute subkeys ──
        let mut subkeys = [0u32; 40];
        let rho: u32 = 0x1010101;
        for x in 0..20u32 {
            let mut tmp = [0u8; 4];
            let val = (rho.wrapping_mul(2 * x)).to_le_bytes();
            tmp.copy_from_slice(&val);
            let a = h(&tmp, key, 0);

            let val2 = (rho.wrapping_mul(2 * x + 1)).to_le_bytes();
            tmp.copy_from_slice(&val2);
            let b = h(&tmp, key, 1).rotate_left(8);

            subkeys[2 * x as usize] = a.wrapping_add(b);
            subkeys[2 * x as usize + 1] = (a.wrapping_add(b.wrapping_mul(2))).rotate_left(9);
        }

        // ── Pre-compute S-box + MDS lookup tables ──
        // This is the key optimization: each table entry encodes the full
        // S-box chain + key mixing + MDS multiplication for one byte value.
        let mut s = [[0u32; 256]; 4];
        match k {
            2 => {
                for i in 0..256 {
                    s[0][i] = mds_column_mult(
                        SBOX[1][SBOX[0][SBOX[0][i] as usize ^ s_key[0] as usize] as usize
                            ^ s_key[4] as usize],
                        0,
                    );
                    s[1][i] = mds_column_mult(
                        SBOX[0][SBOX[0][SBOX[1][i] as usize ^ s_key[1] as usize] as usize
                            ^ s_key[5] as usize],
                        1,
                    );
                    s[2][i] = mds_column_mult(
                        SBOX[1][SBOX[1][SBOX[0][i] as usize ^ s_key[2] as usize] as usize
                            ^ s_key[6] as usize],
                        2,
                    );
                    s[3][i] = mds_column_mult(
                        SBOX[0][SBOX[1][SBOX[1][i] as usize ^ s_key[3] as usize] as usize
                            ^ s_key[7] as usize],
                        3,
                    );
                }
            }
            3 => {
                for i in 0..256 {
                    s[0][i] = mds_column_mult(
                        SBOX[1][SBOX[0][SBOX[0][SBOX[1][i] as usize ^ s_key[0] as usize] as usize
                            ^ s_key[4] as usize] as usize
                            ^ s_key[8] as usize],
                        0,
                    );
                    s[1][i] = mds_column_mult(
                        SBOX[0][SBOX[0][SBOX[1][SBOX[1][i] as usize ^ s_key[1] as usize] as usize
                            ^ s_key[5] as usize] as usize
                            ^ s_key[9] as usize],
                        1,
                    );
                    s[2][i] = mds_column_mult(
                        SBOX[1][SBOX[1][SBOX[0][SBOX[0][i] as usize ^ s_key[2] as usize] as usize
                            ^ s_key[6] as usize] as usize
                            ^ s_key[10] as usize],
                        2,
                    );
                    s[3][i] = mds_column_mult(
                        SBOX[0][SBOX[1][SBOX[1][SBOX[0][i] as usize ^ s_key[3] as usize] as usize
                            ^ s_key[7] as usize] as usize
                            ^ s_key[11] as usize],
                        3,
                    );
                }
            }
            _ => {
                // k=4 (256-bit key) — the common case for kcptun.
                // Go (k=4 default case): 5 sbox layers + ^S[12..15].
                //   s[0]: sbox[1][sbox[0][sbox[0][sbox[1][sbox[1][i]^S[0]]^S[4]]^S[8]]^S[12]]
                // The previous Rust code incorrectly reused the k=3 structure
                // (only 4 sbox layers, missing ^S[12..15] and the innermost sbox),
                // which broke Go↔Rust interop for 256-bit keys.
                for i in 0..256 {
                    s[0][i] = mds_column_mult(SBOX[1][SBOX[0][SBOX[0][SBOX[1][SBOX[1][i] as usize ^ s_key[0] as usize] as usize ^ s_key[4] as usize] as usize ^ s_key[8] as usize] as usize ^ s_key[12] as usize], 0);
                    s[1][i] = mds_column_mult(SBOX[0][SBOX[0][SBOX[1][SBOX[1][SBOX[0][i] as usize ^ s_key[1] as usize] as usize ^ s_key[5] as usize] as usize ^ s_key[9] as usize] as usize ^ s_key[13] as usize], 1);
                    s[2][i] = mds_column_mult(SBOX[1][SBOX[1][SBOX[0][SBOX[0][SBOX[0][i] as usize ^ s_key[2] as usize] as usize ^ s_key[6] as usize] as usize ^ s_key[10] as usize] as usize ^ s_key[14] as usize], 2);
                    s[3][i] = mds_column_mult(SBOX[0][SBOX[1][SBOX[1][SBOX[0][SBOX[1][i] as usize ^ s_key[3] as usize] as usize ^ s_key[7] as usize] as usize ^ s_key[11] as usize] as usize ^ s_key[15] as usize], 3);
                }
            }
        }

        TwofishCrypt { s, k: subkeys }
    }

    /// g function: 4 table lookups + XOR (O(1) — pre-computed tables)
    #[inline(always)]
    fn g(&self, x: u32) -> u32 {
        self.s[0][(x & 0xFF) as usize]
            ^ self.s[1][((x >> 8) & 0xFF) as usize]
            ^ self.s[2][((x >> 16) & 0xFF) as usize]
            ^ self.s[3][((x >> 24) & 0xFF) as usize]
    }

    /// Encrypt a single 16-byte block.
    #[inline]
    fn encrypt_block(&self, inp: &[u8; 16], out: &mut [u8; 16]) {
        let mut p = [
            u32::from_le_bytes(inp[0..4].try_into().unwrap()),
            u32::from_le_bytes(inp[4..8].try_into().unwrap()),
            u32::from_le_bytes(inp[8..12].try_into().unwrap()),
            u32::from_le_bytes(inp[12..16].try_into().unwrap()),
        ];

        // Input whitening
        p[0] ^= self.k[0];
        p[1] ^= self.k[1];
        p[2] ^= self.k[2];
        p[3] ^= self.k[3];

        for r in 0..8 {
            let k = 4 * r + 8;
            let t1 = self.g(p[1].rotate_left(8));
            let t0 = self.g(p[0]).wrapping_add(t1);
            p[2] = (p[2] ^ (t0.wrapping_add(self.k[k]))).rotate_right(1);
            let t2 = t1.wrapping_add(t0).wrapping_add(self.k[k + 1]);
            p[3] = p[3].rotate_left(1) ^ t2;

            let t1 = self.g(p[3].rotate_left(8));
            let t0 = self.g(p[2]).wrapping_add(t1);
            p[0] = (p[0] ^ (t0.wrapping_add(self.k[k + 2]))).rotate_right(1);
            let t2 = t1.wrapping_add(t0).wrapping_add(self.k[k + 3]);
            p[1] = (p[1].rotate_left(1)) ^ t2;
        }

        // Undo last swap + output whitening
        out[0..4].copy_from_slice(&(p[2] ^ self.k[4]).to_le_bytes());
        out[4..8].copy_from_slice(&(p[3] ^ self.k[5]).to_le_bytes());
        out[8..12].copy_from_slice(&(p[0] ^ self.k[6]).to_le_bytes());
        out[12..16].copy_from_slice(&(p[1] ^ self.k[7]).to_le_bytes());
    }
}

impl BlockCrypt for TwofishCrypt {
    fn encrypt(&self, data: &mut [u8]) {
        cfb16_enc(data, &|i, o| self.encrypt_block(i, o));
    }
    fn decrypt(&self, data: &mut [u8]) {
        cfb16_dec(data, &|i, o| self.encrypt_block(i, o));
    }
    fn name(&self) -> &'static str {
        "twofish"
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
    fn tfish() {
        rt(
            &TwofishCrypt::new(&[0u8; 32]),
            &mut b"hello kcp tf test!".to_vec(),
        );
    }
    #[test]
    fn tfish_128() {
        rt(
            &TwofishCrypt::new(&[0u8; 16]),
            &mut b"hello kcp 128key!".to_vec(),
        );
    }
    #[test]
    fn tfish_192() {
        rt(
            &TwofishCrypt::new(&[0u8; 24]),
            &mut b"hello kcp 192key!".to_vec(),
        );
    }
}
