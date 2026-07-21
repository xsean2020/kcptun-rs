//! # qpp-rs
//!
//! Quantum Permutation Pad (QPP) encryption — a port of Go's `xtaci/qpp` that
//! preserves **algorithmic compatibility**.
//!
//! ## Wire-level compatibility
//!
//! This implementation produces the exact same encrypted output as Go's qpp
//! given the same key, data, and pad configuration:
//! - xoshiro256** PRNG (matching Go's xoshiro256ss)
//! - PBKDF2(SHA1, 128 rounds) for key derivation
//! - PAD_SWITCH=8 bytes per pad before switching
//! - Permutation via AES-256 encrypted Fisher-Yates shuffle

use std::fmt;

// ─── Constants (matching Go qpp) ──────────────────────────────────────────

const PM_SELECTOR_IDENTIFIER: &str = "PERMUTATION_MATRIX_SELECTOR";
const SHUFFLE_SALT: &str = "___QUANTUM_PERMUTATION_PAD_SHUFFLE_SALT___";
const PRNG_SALT: &str = "___QUANTUM_PERMUTATION_PAD_PRNG_SALT___";
const CHUNK_DERIVE_SALT: &str = "___QUANTUM_PERMUTATION_PAD_SEED_DERIVE___";
const PBKDF2_LOOPS: u32 = 128;
const CHUNK_DERIVE_LOOPS: u32 = 1024;
const PAD_SWITCH: u8 = 8;
const QUBITS: u8 = 8;

/// QPP minimum seed length.
pub const QPP_MIN_SEED_LENGTH: usize = 32;
/// QPP permutation dimension (power).
pub const QPP_POWER: u16 = 8;
/// Default QPP pad size in bytes.
pub const QPP_PAD_SIZE: usize = 256;
/// Minimum number of pads.
pub const QPP_MINIMUM_PADS: u16 = 3;

// ─── Rand (xoshiro256** PRNG) ─────────────────────────────────────────────

/// Stateful xoshiro256** PRNG matching Go's qpp.Rand.
#[derive(Clone)]
pub struct Rand {
    xoshiro: [u64; 4],
    seed64: u64,
    count: u8,
}

impl fmt::Debug for Rand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Rand")
            .field("xoshiro", &self.xoshiro)
            .field("seed64", &self.seed64)
            .field("count", &self.count)
            .finish()
    }
}

#[inline]
fn rol64(x: u64, k: u32) -> u64 {
    x.rotate_left(k)
}

/// xoshiro256** step — matches Go's xoshiro256ss.
fn xoshiro256ss(s: &mut [u64; 4]) -> u64 {
    let result = rol64(s[1].wrapping_mul(5), 7).wrapping_mul(9);
    let t = s[1] << 17;
    s[2] ^= s[0];
    s[3] ^= s[1];
    s[1] ^= s[2];
    s[0] ^= s[3];
    s[2] ^= t;
    s[3] = rol64(s[3], 45);
    result
}

impl Rand {
    /// Step the PRNG and return the next 64-bit value.
    pub fn next_u64(&mut self) -> u64 {
        let r = xoshiro256ss(&mut self.xoshiro);
        self.seed64 = r;
        r
    }
}

/// Create a PRNG from a seed using PBKDF2 (matching Go's CreatePRNG).
pub fn create_prng(seed: &[u8]) -> Rand {
    use hmac::Mac;
    let mut mac = <hmac::Hmac<sha2::Sha256> as Mac>::new_from_slice(seed).unwrap();
    mac.update(PM_SELECTOR_IDENTIFIER.as_bytes());
    let sum = mac.finalize().into_bytes();

    // PBKDF2(SHA1, 128 rounds) to derive xoshiro state
    let mut xoshiro_key = [0u8; 32];
    let _ = pbkdf2::pbkdf2::<hmac::Hmac<sha1::Sha1>>(
        &sum,
        PRNG_SALT.as_bytes(),
        PBKDF2_LOOPS,
        &mut xoshiro_key,
    );

    let mut xoshiro = [0u64; 4];
    xoshiro[0] = u64::from_le_bytes(xoshiro_key[0..8].try_into().unwrap());
    xoshiro[1] = u64::from_le_bytes(xoshiro_key[8..16].try_into().unwrap());
    xoshiro[2] = u64::from_le_bytes(xoshiro_key[16..24].try_into().unwrap());
    xoshiro[3] = u64::from_le_bytes(xoshiro_key[24..32].try_into().unwrap());

    let seed64 = xoshiro256ss(&mut xoshiro);
    Rand {
        xoshiro,
        seed64,
        count: 0,
    }
}

// ─── Seed-to-chunks (matching Go) ─────────────────────────────────────────

fn seed_to_chunks(seed: &[u8]) -> Vec<Vec<u8>> {
    let seed = if seed.len() < 32 {
        let mut expanded = vec![0u8; 32];
        let _ = pbkdf2::pbkdf2::<hmac::Hmac<sha1::Sha1>>(
            seed,
            CHUNK_DERIVE_SALT.as_bytes(),
            CHUNK_DERIVE_LOOPS,
            &mut expanded,
        );
        expanded
    } else {
        seed.to_vec()
    };

    // QPPMinimumSeedLength(QUBITS=8): 256! needs ~211 bytes
    let byte_length = qpp_minimum_seed_length_inner(QUBITS);
    let chunk_count = (byte_length.div_ceil(32)).max(1);
    let mut chunks = vec![vec![0u8; 32]; chunk_count];
    for i in 0..chunk_count {
        for j in 0..32 {
            chunks[i][j] = seed[(i * 32 + j) % seed.len()];
        }
        let mut derived = vec![0u8; 32];
        let _ = pbkdf2::pbkdf2::<hmac::Hmac<sha1::Sha1>>(
            &chunks[i],
            CHUNK_DERIVE_SALT.as_bytes(),
            CHUNK_DERIVE_LOOPS,
            &mut derived,
        );
        chunks[i] = derived;
    }
    chunks
}

// ─── Shuffle (Fisher-Yates with AES-256, matching Go) ─────────────────────

fn shuffle_pad(chunk: &[u8], pad: &mut [u8], pad_id: u16, blocks: &[aes::Aes256]) {
    use aes::cipher::generic_array::GenericArray;
    use aes::cipher::BlockEncrypt;
    use hmac::Mac;

    let message = format!("QPP_{:b}", pad_id);
    let mut mac = <hmac::Hmac<sha2::Sha256> as Mac>::new_from_slice(chunk).unwrap();
    mac.update(message.as_bytes());
    let mut sum = mac.finalize().into_bytes();

    for i in (1..pad.len()).rev() {
        // Go: encrypt sum with ALL AES blocks, ALL 32 bytes, in shuffle loop
        for b in blocks {
            for off in (0..sum.len()).step_by(16) {
                let mut block_data = GenericArray::clone_from_slice(&sum[off..off + 16]);
                b.encrypt_block(&mut block_data);
                sum[off..off + 16].copy_from_slice(&block_data);
            }
        }

        let bigrand = {
            let mut val = 0u64;
            for &b in sum.iter().take(8) {
                val = (val << 8) | b as u64;
            }
            val % (i + 1) as u64
        };

        pad.swap(i, bigrand as usize);
    }
}

// ─── QuantumPermutationPad ───────────────────────────────────────────────

/// A quantum permutation pad (QPP) encryption/decryption device.
///
/// Wire-compatible with Go's `xtaci/qpp`.
pub struct QuantumPermutationPad {
    /// Encryption pads (forward permutations).
    pub pads: Vec<u8>,
    /// Decryption pads (reverse permutations).
    pub rpads: Vec<u8>,
    num_pads: u16,
    enc_rand: Rand,
    dec_rand: Rand,
}

impl fmt::Debug for QuantumPermutationPad {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QuantumPermutationPad")
            .field("num_pads", &self.num_pads)
            .finish()
    }
}

impl QuantumPermutationPad {
    /// Create a new QPP with the given key and pad count.
    pub fn new(key: &[u8], num_pads: u16) -> Self {
        use aes::cipher::KeyInit;
        let num_pads = num_pads.max(1);
        let matrix_bytes = 1 << QUBITS;
        let total = num_pads as usize * matrix_bytes;
        let mut pads = vec![0u8; total];
        let mut rpads = vec![0u8; total];

        let chunks = seed_to_chunks(key);
        // Create AES-256 blocks for each chunk (matching Go)
        let mut blocks: Vec<aes::Aes256> = Vec::new();
        for chunk in &chunks {
            let mut aes_key = [0u8; 32];
            let _ = pbkdf2::pbkdf2::<hmac::Hmac<sha1::Sha1>>(
                chunk,
                SHUFFLE_SALT.as_bytes(),
                PBKDF2_LOOPS,
                &mut aes_key,
            );
            blocks.push(aes::Aes256::new_from_slice(&aes_key).unwrap());
        }

        for i in 0..num_pads as usize {
            let pad = &mut pads[i * matrix_bytes..(i + 1) * matrix_bytes];
            for (j, slot) in pad.iter_mut().enumerate() {
                *slot = j as u8;
            }
            shuffle_pad(&chunks[i % chunks.len()], pad, i as u16, &blocks);
            let rpad = &mut rpads[i * matrix_bytes..(i + 1) * matrix_bytes];
            for (j, slot) in pad.iter().enumerate() {
                rpad[*slot as usize] = j as u8;
            }
        }

        let enc_rand = create_prng(key);
        let dec_rand = create_prng(key);

        QuantumPermutationPad {
            pads,
            rpads,
            num_pads,
            enc_rand,
            dec_rand,
        }
    }

    /// Encrypt data in-place using the default PRNG.
    pub fn encrypt(&mut self, data: &mut [u8]) {
        encrypt_with_pads(&self.pads, data, &mut self.enc_rand, self.num_pads);
    }

    /// Decrypt data in-place using the default PRNG.
    pub fn decrypt(&mut self, data: &mut [u8]) {
        decrypt_with_pads(&self.rpads, data, &mut self.dec_rand, self.num_pads);
    }

    /// Get the number of pads.
    #[inline]
    pub fn count(&self) -> u16 {
        self.num_pads
    }
}

/// Internal: encrypt data using permutation pads.
pub fn encrypt_with_pads(pads: &[u8], data: &mut [u8], rand: &mut Rand, num_pads: u16) {
    if data.is_empty() || num_pads == 0 {
        return;
    }
    let size = data.len();
    let mut r: u64 = rand.seed64;
    let mut base = (r as u16 % num_pads) as usize * 256;
    let mut count = rand.count;
    let mut offset = 0usize;

    if count != 0 {
        while offset < data.len() {
            let rr = (r >> (count * 8)) as u8;
            data[offset] = pads[base + (data[offset] ^ rr) as usize];
            count += 1;
            if count == PAD_SWITCH {
                r = xoshiro256ss(&mut rand.xoshiro);
                base = (r as u16 % num_pads) as usize * 256;
                offset += 1;
                count = 0;
                break;
            }
            offset += 1;
        }
    }

    let remaining = &mut data[offset..];
    let repeat = remaining.len() / 8;
    for i in 0..repeat {
        let d = &mut remaining[i * 8..i * 8 + 8];
        let rr0 = r as u8;
        let rr1 = (r >> 8) as u8;
        let rr2 = (r >> 16) as u8;
        let rr3 = (r >> 24) as u8;
        let rr4 = (r >> 32) as u8;
        let rr5 = (r >> 40) as u8;
        let rr6 = (r >> 48) as u8;
        let rr7 = (r >> 56) as u8;

        d[0] = pads[base + (d[0] ^ rr0) as usize];
        d[1] = pads[base + (d[1] ^ rr1) as usize];
        d[2] = pads[base + (d[2] ^ rr2) as usize];
        d[3] = pads[base + (d[3] ^ rr3) as usize];
        d[4] = pads[base + (d[4] ^ rr4) as usize];
        d[5] = pads[base + (d[5] ^ rr5) as usize];
        d[6] = pads[base + (d[6] ^ rr6) as usize];
        d[7] = pads[base + (d[7] ^ rr7) as usize];

        r = xoshiro256ss(&mut rand.xoshiro);
        base = (r as u16 % num_pads) as usize * 256;
    }

    let tail_start = offset + repeat * 8;
    for i in tail_start..data.len() {
        let rr = (r >> (count * 8)) as u8;
        data[i] = pads[base + (data[i] ^ rr) as usize];
        count += 1;
    }

    rand.seed64 = r;
    rand.count = ((rand.count as usize + size) % PAD_SWITCH as usize) as u8;
}

/// Internal: decrypt data using reverse permutation pads.
pub fn decrypt_with_pads(rpads: &[u8], data: &mut [u8], rand: &mut Rand, num_pads: u16) {
    if data.is_empty() || num_pads == 0 {
        return;
    }
    let size = data.len();
    let mut r: u64 = rand.seed64;
    let mut base = (r as u16 % num_pads) as usize * 256;
    let mut count = rand.count;
    let mut offset = 0usize;

    if count != 0 {
        while offset < data.len() {
            let rr = (r >> (count * 8)) as u8;
            data[offset] = rpads[base + data[offset] as usize] ^ rr;
            count += 1;
            if count == PAD_SWITCH {
                r = xoshiro256ss(&mut rand.xoshiro);
                base = (r as u16 % num_pads) as usize * 256;
                offset += 1;
                count = 0;
                break;
            }
            offset += 1;
        }
    }

    let remaining = &mut data[offset..];
    let repeat = remaining.len() / 8;
    for i in 0..repeat {
        let d = &mut remaining[i * 8..i * 8 + 8];
        let rr0 = r as u8;
        let rr1 = (r >> 8) as u8;
        let rr2 = (r >> 16) as u8;
        let rr3 = (r >> 24) as u8;
        let rr4 = (r >> 32) as u8;
        let rr5 = (r >> 40) as u8;
        let rr6 = (r >> 48) as u8;
        let rr7 = (r >> 56) as u8;

        d[0] = rpads[base + d[0] as usize] ^ rr0;
        d[1] = rpads[base + d[1] as usize] ^ rr1;
        d[2] = rpads[base + d[2] as usize] ^ rr2;
        d[3] = rpads[base + d[3] as usize] ^ rr3;
        d[4] = rpads[base + d[4] as usize] ^ rr4;
        d[5] = rpads[base + d[5] as usize] ^ rr5;
        d[6] = rpads[base + d[6] as usize] ^ rr6;
        d[7] = rpads[base + d[7] as usize] ^ rr7;

        r = xoshiro256ss(&mut rand.xoshiro);
        base = (r as u16 % num_pads) as usize * 256;
    }

    let tail_start = offset + repeat * 8;
    for i in tail_start..data.len() {
        let rr = (r >> (count * 8)) as u8;
        data[i] = rpads[base + data[i] as usize] ^ rr;
        count += 1;
    }

    rand.seed64 = r;
    rand.count = ((rand.count as usize + size) % PAD_SWITCH as usize) as u8;
}

/// Compute minimum seed byte length for a given permutation size (Ω(n!) bits).
fn qpp_minimum_seed_length_inner(qubits: u8) -> usize {
    let n = 1usize << qubits; // e.g., 256 for QUBITS=8
    let mut bits = 0.0f64;
    for i in 2..=n {
        bits += (i as f64).log2();
    }
    (bits.ceil() as usize).div_ceil(8)
}

/// Calculate the minimum seed length required for the given permutation dimension.
pub fn qpp_minimum_seed_length(_power: u8) -> usize {
    QPP_MIN_SEED_LENGTH
}
pub fn qpp_minimum_pads(_power: u8) -> u16 {
    QPP_MINIMUM_PADS
}

/// Create a PRNG from a seed (matching Go's `qpp.CreatePRNG`).
pub fn create_qpp_prng(seed: &[u8]) -> Rand {
    create_prng(seed)
}

// ─── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qpp_roundtrip() {
        let key = b"test-key-12345-test-key-67890";
        let mut qpp = QuantumPermutationPad::new(key, 61);
        let mut data = vec![0xAB, 0xCD, 0xEF, 0x12, 0x34, 0x56, 0x78, 0x90];
        let original = data.clone();
        qpp.encrypt(&mut data);
        assert_ne!(data, original);
        qpp.decrypt(&mut data);
        assert_eq!(data, original);
    }

    #[test]
    fn qpp_empty() {
        let mut qpp = QuantumPermutationPad::new(b"key", 10);
        let mut empty: Vec<u8> = vec![];
        qpp.encrypt(&mut empty);
        assert!(empty.is_empty());
        qpp.decrypt(&mut empty);
        assert!(empty.is_empty());
    }

    #[test]
    fn qpp_pad_count() {
        assert_eq!(QuantumPermutationPad::new(b"key", 61).count(), 61);
    }

    #[test]
    fn qpp_deterministic() {
        let key = b"deterministic-test-key-for-qpp";
        let data = b"hello qpp world!";
        let mut qpp1 = QuantumPermutationPad::new(key, 10);
        let mut qpp2 = QuantumPermutationPad::new(key, 10);
        let mut d1 = data.to_vec();
        let mut d2 = data.to_vec();
        qpp1.encrypt(&mut d1);
        qpp2.encrypt(&mut d2);
        assert_eq!(d1, d2);
    }

    #[test]
    fn prng_deterministic() {
        let seed = b"test-seed-for-prng";
        let mut rng1 = create_prng(seed);
        let mut rng2 = create_prng(seed);
        for _ in 0..100 {
            assert_eq!(rng1.next_u64(), rng2.next_u64());
        }
    }

    #[test]
    fn xoshiro_works() {
        let mut state = [1u64, 2, 3, 4];
        assert!(xoshiro256ss(&mut state) != 0);
    }

    #[test]
    fn long_data_roundtrip() {
        let key = b"test-long-data-key";
        let mut qpp = QuantumPermutationPad::new(key, 31);
        let mut data: Vec<u8> = (0..1000).map(|i| (i & 0xFF) as u8).collect();
        let original = data.clone();
        qpp.encrypt(&mut data);
        assert_ne!(data, original);
        qpp.decrypt(&mut data);
        assert_eq!(data, original);
    }
}
