//! Packet nonce generation for the kcptun wire format.
//!
//! The 16-byte nonce that prefixes every CFB packet (and the 12-byte AEAD
//! nonce) must be **unpredictable and non-repeating under a fixed key**:
//!
//! - `salsa20` derives its 8-byte stream nonce directly from the first eight
//!   nonce bytes, so a repeated value is a repeated keystream — a two-time
//!   pad, and (since integrity is only a CRC-32 over the plaintext) a forgery
//!   oracle once one keystream block is known.
//! - CFB chains the nonce into the keystream: the first plaintext block *is*
//!   the nonce, so `C₁ = nonce ⊕ E(IV)` and `K₂ = E(C₁)`. A nonce that
//!   repeats makes the ciphertext deterministic, leaking `P ⊕ P'` for packets
//!   that share an index and giving a passive observer a stable fingerprint.
//! - GCM nonce reuse under one key leaks the plaintext XOR *and* the
//!   authentication key relationship needed to forge tags.
//!
//! A per-session counter does not satisfy this: both ends of a session derive
//! the same key from `--key`, both counters start at zero, and every restart
//! and every `--conn` channel starts over.
//!
//! Go's kcp-go solves it with `nonceAES128` (`entropy.go`): AES-128 in
//! counter mode, keyed from the OS CSPRNG at startup, producing 16 fresh
//! bytes per packet. [`NonceGen`] is that construction — one AES block per
//! packet (a few nanoseconds with AES-NI), no syscall on the datapath, and
//! nonces that neither repeat within a session nor correlate across sessions.

use std::sync::atomic::{AtomicU64, Ordering};

use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockEncrypt, KeyInit};
use aes::Aes128;

/// Nonce generator: `AES-128(random key)` applied to a counter block.
///
/// The output is a PRF over a never-repeating input, so within one instance
/// nonces cannot repeat before 2⁶⁴ packets, and two instances (the two ends
/// of a session, two `--conn` channels, a process restart) hold independent
/// random keys and therefore produce unrelated streams.
pub(crate) struct NonceGen {
    cipher: Aes128,
    counter: AtomicU64,
    /// Mixed into the counter block; keeps the data and ACK paths of one
    /// session in different PRF domains even before the random key.
    domain: u64,
}

impl NonceGen {
    pub(crate) fn new(domain: u64) -> Self {
        let mut key = [0u8; 16];
        // A failure here means the OS entropy source is unavailable; there is
        // no safe way to encrypt without it.
        getrandom::getrandom(&mut key).expect("OS CSPRNG unavailable for nonce key");
        NonceGen {
            cipher: Aes128::new(GenericArray::from_slice(&key)),
            counter: AtomicU64::new(0),
            domain,
        }
    }

    /// Next 16-byte nonce.
    #[inline]
    pub(crate) fn next(&self) -> [u8; 16] {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        let mut block = [0u8; 16];
        block[..8].copy_from_slice(&n.to_le_bytes());
        block[8..].copy_from_slice(&self.domain.to_le_bytes());
        let mut b = GenericArray::clone_from_slice(&block);
        self.cipher.encrypt_block(&mut b);
        let mut out = [0u8; 16];
        out.copy_from_slice(&b);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn nonces_do_not_repeat_within_an_instance() {
        let gen = NonceGen::new(0xDEAD_BEEF);
        let seen: HashSet<[u8; 16]> = (0..4096).map(|_| gen.next()).collect();
        assert_eq!(seen.len(), 4096);
    }

    #[test]
    fn instances_with_the_same_domain_do_not_share_a_nonce_stream() {
        // Two ends of a session, or a restarted process: same key material,
        // same domain, and yet their nonce streams must not line up.
        let a = NonceGen::new(7);
        let b = NonceGen::new(7);
        let first_a: Vec<[u8; 16]> = (0..64).map(|_| a.next()).collect();
        let first_b: Vec<[u8; 16]> = (0..64).map(|_| b.next()).collect();
        assert_ne!(first_a, first_b);
        // Not merely offset: no value from one stream appears in the other.
        let set_a: HashSet<_> = first_a.iter().collect();
        assert!(first_b.iter().all(|n| !set_a.contains(n)));
    }

    #[test]
    fn first_nonce_is_not_a_constant() {
        // The old counter-based nonce made the first packet of every session
        // byte-identical, which is both a keystream reuse and a fingerprint.
        let first = NonceGen::new(0).next();
        assert_ne!(first, NonceGen::new(0).next());
        assert_ne!(first, [0u8; 16]);
    }
}
