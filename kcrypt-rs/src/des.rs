//! DES / TripleDES (3DES) — 8-byte block cipher.
//!
//! Ported from Go's `crypto/des` package to use precomputed **Feistel boxes**
//! (combining S-box + P permutation into a single lookup), matching Go's
//! `feistelBox[8][64]u32` approach. This eliminates the expensive
//! `wrapping_mul` bit-permutation operations used by the RustCrypto `des` crate,
//! giving ~2× faster block encryption.
//!
//! Wire-compatible with Go's `crypto/des` and RustCrypto `des::TdesEde3`.

// ─── DES constants (from Go crypto/des/const.go) ─────────────────────────

/// 8 S-boxes, each 4 rows × 16 columns.
const S_BOXES: [[[u8; 16]; 4]; 8] = [
    // S-box 1
    [
        [14, 4, 13, 1, 2, 15, 11, 8, 3, 10, 6, 12, 5, 9, 0, 7],
        [0, 15, 7, 4, 14, 2, 13, 1, 10, 6, 12, 11, 9, 5, 3, 8],
        [4, 1, 14, 8, 13, 6, 2, 11, 15, 12, 9, 7, 3, 10, 5, 0],
        [15, 12, 8, 2, 4, 9, 1, 7, 5, 11, 3, 14, 10, 0, 6, 13],
    ],
    // S-box 2
    [
        [15, 1, 8, 14, 6, 11, 3, 4, 9, 7, 2, 13, 12, 0, 5, 10],
        [3, 13, 4, 7, 15, 2, 8, 14, 12, 0, 1, 10, 6, 9, 11, 5],
        [0, 14, 7, 11, 10, 4, 13, 1, 5, 8, 12, 6, 9, 3, 2, 15],
        [13, 8, 10, 1, 3, 15, 4, 2, 11, 6, 7, 12, 0, 5, 14, 9],
    ],
    // S-box 3
    [
        [10, 0, 9, 14, 6, 3, 15, 5, 1, 13, 12, 7, 11, 4, 2, 8],
        [13, 7, 0, 9, 3, 4, 6, 10, 2, 8, 5, 14, 12, 11, 15, 1],
        [13, 6, 4, 9, 8, 15, 3, 0, 11, 1, 2, 12, 5, 10, 14, 7],
        [1, 10, 13, 0, 6, 9, 8, 7, 4, 15, 14, 3, 11, 5, 2, 12],
    ],
    // S-box 4
    [
        [7, 13, 14, 3, 0, 6, 9, 10, 1, 2, 8, 5, 11, 12, 4, 15],
        [13, 8, 11, 5, 6, 15, 0, 3, 4, 7, 2, 12, 1, 10, 14, 9],
        [10, 6, 9, 0, 12, 11, 7, 13, 15, 1, 3, 14, 5, 2, 8, 4],
        [3, 15, 0, 6, 10, 1, 13, 8, 9, 4, 5, 11, 12, 7, 2, 14],
    ],
    // S-box 5
    [
        [2, 12, 4, 1, 7, 10, 11, 6, 8, 5, 3, 15, 13, 0, 14, 9],
        [14, 11, 2, 12, 4, 7, 13, 1, 5, 0, 15, 10, 3, 9, 8, 6],
        [4, 2, 1, 11, 10, 13, 7, 8, 15, 9, 12, 5, 6, 3, 0, 14],
        [11, 8, 12, 7, 1, 14, 2, 13, 6, 15, 0, 9, 10, 4, 5, 3],
    ],
    // S-box 6
    [
        [12, 1, 10, 15, 9, 2, 6, 8, 0, 13, 3, 4, 14, 7, 5, 11],
        [10, 15, 4, 2, 7, 12, 9, 5, 6, 1, 13, 14, 0, 11, 3, 8],
        [9, 14, 15, 5, 2, 8, 12, 3, 7, 0, 4, 10, 1, 13, 11, 6],
        [4, 3, 2, 12, 9, 5, 15, 10, 11, 14, 1, 7, 6, 0, 8, 13],
    ],
    // S-box 7
    [
        [4, 11, 2, 14, 15, 0, 8, 13, 3, 12, 9, 7, 5, 10, 6, 1],
        [13, 0, 11, 7, 4, 9, 1, 10, 14, 3, 5, 12, 2, 15, 8, 6],
        [1, 4, 11, 13, 12, 3, 7, 14, 10, 15, 6, 8, 0, 5, 9, 2],
        [6, 11, 13, 8, 1, 4, 10, 7, 9, 5, 0, 15, 14, 2, 3, 12],
    ],
    // S-box 8
    [
        [13, 2, 8, 4, 6, 15, 11, 1, 10, 9, 3, 14, 5, 0, 12, 7],
        [1, 15, 13, 8, 10, 3, 7, 4, 12, 5, 6, 11, 0, 14, 9, 2],
        [7, 11, 4, 1, 9, 12, 14, 2, 0, 6, 10, 13, 15, 3, 5, 8],
        [2, 1, 14, 7, 4, 10, 8, 13, 15, 12, 9, 0, 3, 5, 6, 11],
    ],
];

/// P permutation table (permutationFunction in Go)
const PERMUTATION_FUNCTION: [u8; 32] = [
    16, 25, 12, 11, 3, 20, 4, 15, 31, 17, 9, 6, 27, 14, 1, 22, 30, 24, 8, 18, 0, 5, 29, 23, 13, 19,
    2, 26, 10, 21, 28, 7,
];

/// PC-1 table (permutedChoice1 in Go)
const PERMUTED_CHOICE_1: [u8; 56] = [
    7, 15, 23, 31, 39, 47, 55, 63, 6, 14, 22, 30, 38, 46, 54, 62, 5, 13, 21, 29, 37, 45, 53, 61, 4,
    12, 20, 28, 1, 9, 17, 25, 33, 41, 49, 57, 2, 10, 18, 26, 34, 42, 50, 58, 3, 11, 19, 27, 35, 43,
    51, 59, 36, 44, 52, 60,
];

/// PC-2 table (permutedChoice2 in Go)
const PERMUTED_CHOICE_2: [u8; 48] = [
    42, 39, 45, 32, 55, 51, 53, 28, 41, 50, 35, 46, 33, 37, 44, 52, 30, 48, 40, 49, 29, 36, 43, 54,
    15, 4, 25, 19, 9, 1, 26, 16, 5, 11, 23, 8, 12, 7, 17, 0, 22, 3, 10, 14, 6, 20, 27, 24,
];

/// Key schedule left rotations per round
const KS_ROTATIONS: [u8; 16] = [1, 1, 2, 2, 2, 2, 2, 2, 1, 2, 2, 2, 2, 2, 2, 1];

// ─── Bit permutation helpers (from Go crypto/des/block.go) ──────────────

/// General-purpose bit permutation (permuteBlock in Go).
/// Must be `const fn` so it can be used in the `FEISTEL_BOX` static initializer.
const fn permute_block(src: u64, permutation: &[u8]) -> u64 {
    let mut block: u64 = 0;
    let mut i = 0;
    let len = permutation.len();
    while i < len {
        let n = permutation[i];
        let bit = (src >> n) & 1;
        block |= bit << ((len - 1) - i);
        i += 1;
    }
    block
}

/// Initial Permutation (permuteInitialBlock in Go)
fn permute_initial_block(mut block: u64) -> u64 {
    let b1 = block >> 48;
    let b2 = block << 48;
    block ^= b1 ^ b2 ^ (b1 << 48) ^ (b2 >> 48);

    let b1 = (block >> 32) & 0xff00ff;
    let b2 = block & 0xff00ff00;
    block ^= (b1 << 32) ^ b2 ^ (b1 << 8) ^ (b2 << 24);

    let b1 = block & 0x0f0f00000f0f0000;
    let b2 = block & 0x0000f0f00000f0f0;
    block ^= b1 ^ b2 ^ (b1 >> 12) ^ (b2 << 12);

    let b1 = block & 0x3300330033003300;
    let b2 = block & 0x00cc00cc00cc00cc;
    block ^= b1 ^ b2 ^ (b1 >> 6) ^ (b2 << 6);

    let b1 = block & 0xaaaaaaaa55555555;
    block ^= b1 ^ (b1 >> 33) ^ (b1 << 33);

    block
}

/// Final Permutation (permuteFinalBlock in Go) — reverse of IP
fn permute_final_block(mut block: u64) -> u64 {
    let b1 = block & 0xaaaaaaaa55555555;
    block ^= b1 ^ (b1 >> 33) ^ (b1 << 33);

    let b1 = block & 0x3300330033003300;
    let b2 = block & 0x00cc00cc00cc00cc;
    block ^= b1 ^ b2 ^ (b1 >> 6) ^ (b2 << 6);

    let b1 = block & 0x0f0f00000f0f0000;
    let b2 = block & 0x0000f0f00000f0f0;
    block ^= b1 ^ b2 ^ (b1 >> 12) ^ (b2 << 12);

    let b1 = (block >> 32) & 0xff00ff;
    let b2 = block & 0xff00ff00;
    block ^= (b1 << 32) ^ b2 ^ (b1 << 8) ^ (b2 << 24);

    let b1 = block >> 48;
    let b2 = block << 48;
    block ^= b1 ^ b2 ^ (b1 << 48) ^ (b2 >> 48);

    block
}

/// Expand 48-bit input to 64-bit, with each 6-bit block padded by two extra
/// bits at the top (unpack in Go).
fn unpack(x: u64) -> u64 {
    ((x >> 6) & 0xff)
        | ((x >> 18) & 0xff) << 8
        | ((x >> 30) & 0xff) << 16
        | ((x >> 42) & 0xff) << 24
        | (x & 0xff) << 32
        | ((x >> 12) & 0xff) << 40
        | ((x >> 24) & 0xff) << 48
        | ((x >> 36) & 0xff) << 56
}

/// 28-bit circular left shift (ksRotate in Go)
fn ks_rotate(input: u32) -> [u32; 16] {
    let mut out = [0u32; 16];
    let mut last = input;
    for i in 0..16 {
        let left = (last << (4 + KS_ROTATIONS[i])) >> 4;
        let right = (last << 4) >> (32 - KS_ROTATIONS[i]);
        out[i] = left | right;
        last = out[i];
    }
    out
}

// ─── Feistel box precomputation ──────────────────────────────────────────

/// Precomputed Feistel box: combines S-box output + P permutation into a
/// single lookup table, matching Go's `feistelBox[8][64]u32`.
static FEISTEL_BOX: [[u32; 64]; 8] = {
    let mut box_val = [[0u32; 64]; 8];
    let mut s = 0;
    while s < 8 {
        let mut i = 0;
        while i < 4 {
            let mut j = 0;
            while j < 16 {
                let f = (S_BOXES[s][i][j] as u64) << (4 * (7 - s as u64));
                let f = permute_block(f, &PERMUTATION_FUNCTION);

                // Row is determined by the 1st and 6th bit.
                // Column is the middle four bits.
                let row = ((i & 2) << 4) | (i & 1);
                let col = j << 1;
                let t = row | col;

                // The rotation was performed in the feistel rounds, being factored out
                let f = (f as u32).rotate_left(1);
                box_val[s][t] = f;

                j += 1;
            }
            i += 1;
        }
        s += 1;
    }
    box_val
};

// ─── Feistel function (2 rounds per call) ────────────────────────────────

/// DES Feistel function — processes 2 rounds per call (feistel in Go).
/// Returns updated (left, right).
#[inline(always)]
fn feistel(l: u32, r: u32, k0: u64, k1: u64) -> (u32, u32) {
    let mut left = l;
    let mut right = r;

    // Round 1 (with k0)
    let mut t = right ^ (k0 >> 32) as u32;
    left ^= FEISTEL_BOX[7][(t & 0x3f) as usize]
        ^ FEISTEL_BOX[5][((t >> 8) & 0x3f) as usize]
        ^ FEISTEL_BOX[3][((t >> 16) & 0x3f) as usize]
        ^ FEISTEL_BOX[1][((t >> 24) & 0x3f) as usize];

    t = right.rotate_left(28) ^ k0 as u32;
    left ^= FEISTEL_BOX[6][(t & 0x3f) as usize]
        ^ FEISTEL_BOX[4][((t >> 8) & 0x3f) as usize]
        ^ FEISTEL_BOX[2][((t >> 16) & 0x3f) as usize]
        ^ FEISTEL_BOX[0][((t >> 24) & 0x3f) as usize];

    // Round 2 (with k1)
    t = left ^ (k1 >> 32) as u32;
    right ^= FEISTEL_BOX[7][(t & 0x3f) as usize]
        ^ FEISTEL_BOX[5][((t >> 8) & 0x3f) as usize]
        ^ FEISTEL_BOX[3][((t >> 16) & 0x3f) as usize]
        ^ FEISTEL_BOX[1][((t >> 24) & 0x3f) as usize];

    t = left.rotate_left(28) ^ k1 as u32;
    right ^= FEISTEL_BOX[6][(t & 0x3f) as usize]
        ^ FEISTEL_BOX[4][((t >> 8) & 0x3f) as usize]
        ^ FEISTEL_BOX[2][((t >> 16) & 0x3f) as usize]
        ^ FEISTEL_BOX[0][((t >> 24) & 0x3f) as usize];

    (left, right)
}

// ─── Single DES ───────────────────────────────────────────────────────────

/// Single DES cipher with 16 precomputed subkeys.
#[derive(Clone)]
pub struct DesCipher {
    subkeys: [u64; 16],
}

impl std::fmt::Debug for DesCipher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DesCipher { ... }")
    }
}

impl DesCipher {
    /// Create a new DES cipher from an 8-byte key.
    pub fn new(key: &[u8]) -> Self {
        assert!(key.len() >= 8, "DES key must be at least 8 bytes");
        let key_u64 = u64::from_be_bytes(key[..8].try_into().unwrap());
        Self::from_u64(key_u64)
    }

    /// Create from a big-endian u64 key.
    fn from_u64(key: u64) -> Self {
        let permuted_key = permute_block(key, &PERMUTED_CHOICE_1);

        let left_rotations = ks_rotate((permuted_key >> 28) as u32);
        let right_rotations = ks_rotate(((permuted_key << 4) >> 4) as u32);

        let mut subkeys = [0u64; 16];
        for i in 0..16 {
            let pc2_input = (left_rotations[i] as u64) << 28 | right_rotations[i] as u64;
            subkeys[i] = unpack(permute_block(pc2_input, &PERMUTED_CHOICE_2));
        }
        DesCipher { subkeys }
    }

    /// Encrypt a single 8-byte block (src → dst).
    #[inline]
    pub fn encrypt_block(&self, dst: &mut [u8; 8], src: &[u8; 8]) {
        self.crypt_block(dst, src, false);
    }

    /// Decrypt a single 8-byte block (src → dst).
    #[inline]
    pub fn decrypt_block(&self, dst: &mut [u8; 8], src: &[u8; 8]) {
        self.crypt_block(dst, src, true);
    }

    fn crypt_block(&self, dst: &mut [u8; 8], src: &[u8; 8], decrypt: bool) {
        let b = u64::from_be_bytes(*src);
        let b = permute_initial_block(b);
        let mut left = (b >> 32) as u32;
        let mut right = b as u32;

        left = left.rotate_left(1);
        right = right.rotate_left(1);

        if decrypt {
            for i in 0..8 {
                (left, right) = feistel(
                    left,
                    right,
                    self.subkeys[15 - 2 * i],
                    self.subkeys[15 - (2 * i + 1)],
                );
            }
        } else {
            for i in 0..8 {
                (left, right) = feistel(left, right, self.subkeys[2 * i], self.subkeys[2 * i + 1]);
            }
        }

        left = left.rotate_right(1);
        right = right.rotate_right(1);

        let pre_output = (right as u64) << 32 | left as u64;
        *dst = permute_final_block(pre_output).to_be_bytes();
    }
}

// ─── Triple DES (EDE3) ───────────────────────────────────────────────────

/// Triple DES (3DES) cipher with EDE3 keying (encrypt-decrypt-encrypt).
///
/// Ports Go's `tripleDESCipher` which applies IP/FP only once for all
/// 48 rounds (3×16), not 3 times like the RustCrypto `des` crate.
#[derive(Clone)]
pub struct TripleDesCipher {
    c1: DesCipher,
    c2: DesCipher,
    c3: DesCipher,
}

impl std::fmt::Debug for TripleDesCipher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TripleDesCipher { ... }")
    }
}

impl TripleDesCipher {
    /// Create a new TripleDES cipher from a 24-byte key.
    pub fn new(key: &[u8]) -> Self {
        assert!(key.len() >= 24, "3DES key must be at least 24 bytes");
        TripleDesCipher {
            c1: DesCipher::new(&key[..8]),
            c2: DesCipher::new(&key[8..16]),
            c3: DesCipher::new(&key[16..24]),
        }
    }

    /// Encrypt a single 8-byte block (src → dst).
    #[inline(always)]
    pub fn encrypt_block(&self, dst: &mut [u8; 8], src: &[u8; 8]) {
        let b = u64::from_be_bytes(*src);
        let b = permute_initial_block(b);
        let mut left = (b >> 32) as u32;
        let mut right = b as u32;

        left = left.rotate_left(1);
        right = right.rotate_left(1);

        // c1: encrypt (forward)
        for i in 0..8 {
            (left, right) = feistel(
                left,
                right,
                self.c1.subkeys[2 * i],
                self.c1.subkeys[2 * i + 1],
            );
        }
        // c2: decrypt (backward, with l/r swap)
        for i in 0..8 {
            (right, left) = feistel(
                right,
                left,
                self.c2.subkeys[15 - 2 * i],
                self.c2.subkeys[15 - (2 * i + 1)],
            );
        }
        // c3: encrypt (forward)
        for i in 0..8 {
            (left, right) = feistel(
                left,
                right,
                self.c3.subkeys[2 * i],
                self.c3.subkeys[2 * i + 1],
            );
        }

        left = left.rotate_right(1);
        right = right.rotate_right(1);

        let pre_output = (right as u64) << 32 | left as u64;
        *dst = permute_final_block(pre_output).to_be_bytes();
    }

    /// Decrypt a single 8-byte block (src → dst).
    #[inline(always)]
    pub fn decrypt_block(&self, dst: &mut [u8; 8], src: &[u8; 8]) {
        let b = u64::from_be_bytes(*src);
        let b = permute_initial_block(b);
        let mut left = (b >> 32) as u32;
        let mut right = b as u32;

        left = left.rotate_left(1);
        right = right.rotate_left(1);

        // c3: decrypt (backward)
        for i in 0..8 {
            (left, right) = feistel(
                left,
                right,
                self.c3.subkeys[15 - 2 * i],
                self.c3.subkeys[15 - (2 * i + 1)],
            );
        }
        // c2: encrypt (forward, with l/r swap)
        for i in 0..8 {
            (right, left) = feistel(
                right,
                left,
                self.c2.subkeys[2 * i],
                self.c2.subkeys[2 * i + 1],
            );
        }
        // c1: decrypt (backward)
        for i in 0..8 {
            (left, right) = feistel(
                left,
                right,
                self.c1.subkeys[15 - 2 * i],
                self.c1.subkeys[15 - (2 * i + 1)],
            );
        }

        left = left.rotate_right(1);
        right = right.rotate_right(1);

        let pre_output = (right as u64) << 32 | left as u64;
        *dst = permute_final_block(pre_output).to_be_bytes();
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify our DES matches RustCrypto's des crate for a known key.
    #[test]
    fn test_des_matches_rustcrypto() {
        // Key: 0x0123456789ABCDEF, Plaintext: 0x4E6F772069732074
        let key = [0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF];
        let plaintext = [0x4E, 0x6F, 0x77, 0x20, 0x69, 0x73, 0x20, 0x74];

        // RustCrypto des crate
        use des::cipher::{generic_array::GenericArray, BlockEncrypt, KeyInit};
        let rc_des = des::Des::new_from_slice(&key).unwrap();
        let mut ga = GenericArray::clone_from_slice(&plaintext);
        rc_des.encrypt_block(&mut ga);
        let rc_ct: [u8; 8] = ga.into();

        // Our implementation
        let cipher = DesCipher::new(&key);
        let mut our_ct = [0u8; 8];
        cipher.encrypt_block(&mut our_ct, &plaintext);

        assert_eq!(our_ct, rc_ct, "DES encrypt mismatch with RustCrypto");

        // Verify decrypt roundtrip
        let mut our_pt = [0u8; 8];
        cipher.decrypt_block(&mut our_pt, &our_ct);
        assert_eq!(our_pt, plaintext, "DES decrypt mismatch");
    }

    /// Verify 3DES encrypt/decrypt roundtrip.
    #[test]
    fn test_3des_roundtrip() {
        let key = [
            0x01u8, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF, 0xFE, 0xDC, 0xBA, 0x98, 0x76, 0x54,
            0x32, 0x10, 0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF,
        ];
        let plaintext = [0x41u8, 0x42, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48];

        let cipher = TripleDesCipher::new(&key);
        let mut ct = [0u8; 8];
        cipher.encrypt_block(&mut ct, &plaintext);

        let mut pt = [0u8; 8];
        cipher.decrypt_block(&mut pt, &ct);
        assert_eq!(pt, plaintext, "3DES roundtrip mismatch");
    }

    /// Verify 3DES matches RustCrypto des crate output for random keys.
    #[test]
    fn test_3des_matches_rustcrypto() {
        // Use a known test vector that both implementations must agree on
        let key = [0u8; 24];
        let plaintext = [0u8; 8];

        // RustCrypto des crate (the one we're replacing)
        use des::cipher::{generic_array::GenericArray, BlockEncrypt, KeyInit};
        let rc_cipher = des::TdesEde3::new_from_slice(&key).unwrap();
        let mut ga = GenericArray::clone_from_slice(&plaintext);
        rc_cipher.encrypt_block(&mut ga);
        let rc_expected: [u8; 8] = ga.into();

        // Our implementation
        let our_cipher = TripleDesCipher::new(&key);
        let mut our_ct = [0u8; 8];
        our_cipher.encrypt_block(&mut our_ct, &plaintext);

        assert_eq!(
            our_ct, rc_expected,
            "3DES encrypt mismatch with RustCrypto des crate"
        );
    }

    /// Verify 3DES with non-zero key matches RustCrypto.
    #[test]
    fn test_3des_nonzero_matches_rustcrypto() {
        let key = [
            0x01u8, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF, 0xFE, 0xDC, 0xBA, 0x98, 0x76, 0x54,
            0x32, 0x10, 0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF,
        ];
        let plaintext = [0x41u8, 0x42, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48];

        // RustCrypto
        use des::cipher::{generic_array::GenericArray, BlockEncrypt, KeyInit};
        let rc_cipher = des::TdesEde3::new_from_slice(&key).unwrap();
        let mut ga = GenericArray::clone_from_slice(&plaintext);
        rc_cipher.encrypt_block(&mut ga);
        let rc_ct: [u8; 8] = ga.into();

        // Our implementation
        let our_cipher = TripleDesCipher::new(&key);
        let mut our_ct = [0u8; 8];
        our_cipher.encrypt_block(&mut our_ct, &plaintext);

        assert_eq!(our_ct, rc_ct, "3DES encrypt mismatch");

        // Verify decrypt
        let mut our_pt = [0u8; 8];
        our_cipher.decrypt_block(&mut our_pt, &our_ct);
        assert_eq!(our_pt, plaintext, "3DES decrypt mismatch");
    }
}
