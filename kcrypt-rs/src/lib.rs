//! # kcrypt-rs
//!
//! Shared block-cipher and AEAD encryption library for the kcptun-rs project.
//!
//! Ported from Go's `kcp-go/v5/crypt.go`. Wire-level compatible with all 13
//! ciphers supported by the upstream Go implementation.
//!
//! ## Ciphers
//!
//! | Method       | Trait       | Block size | Notes                          |
//! |--------------|-------------|------------|--------------------------------|
//! | `none`/`null`| `BlockCrypt`| n/a        | No-op                          |
//! | `xor`        | `BlockCrypt`| stream     | PBKDF2-expanded XOR key        |
//! | `aes-128`    | `BlockCrypt`| 16 B       | AES-CFB                        |
//! | `aes-192`    | `BlockCrypt`| 16 B       | AES-CFB                        |
//! | `aes`/`aes-256`| `BlockCrypt`| 16 B     | AES-CFB                        |
//! | `sm4`        | `BlockCrypt`| 16 B       | tjfoc/gmsm S-box               |
//! | `tea`        | `BlockCrypt`| 8 B        | TEA (8 rounds)                 |
//! | `xtea`       | `BlockCrypt`| 8 B        | XTEA (64 rounds)               |
//! | `salsa20`    | `BlockCrypt`| stream     | Salsa20 stream cipher          |
//! | `blowfish`   | `BlockCrypt`| 8 B        | Blowfish-CFB                   |
//! | `twofish`    | `BlockCrypt`| 16 B       | Twofish-CFB                    |
//! | `cast5`      | `BlockCrypt`| 8 B        | CAST-128 (RFC 2144)            |
//! | `3des`       | `BlockCrypt`| 8 B        | Triple-DES-CFB                 |
//! | `aes-128-gcm`| `AeadCrypt` | 16 B       | AES-128-GCM (nonce + tag)      |
//!
//! ## Wire packing
//!
//! CFB/AEAD packet framing (`CryptoBuf`, `encrypt_batch`, offload heuristics)
//! lives in [`wire`]. Prefer `kcrypt_rs::wire` (or the crate-root re-exports)
//! over the old `kcp_rs::crypto_buf` path, which has been removed.
//!
//! ## Usage
//!
//! ```no_run
//! use kcrypt_rs::{select_block_crypt, BlockCrypt};
//!
//! let (cipher, name) = select_block_crypt("aes-128", b"my-password");
//! let mut data = b"hello world".to_vec();
//! cipher.encrypt(&mut data);
//! cipher.decrypt(&mut data);
//! ```

pub mod cast5;
pub mod crypt;
pub mod des;
pub mod wire;

// Re-export the primary public API at the crate root for convenience.
// Keep the deprecated compatibility functions reachable for downstream users;
// new code should call `CryptEngine::select`.
#[allow(deprecated)]
pub use crypt::{select_aead_crypt, select_block_crypt, AeadCrypt, BlockCrypt, CryptEngine};
pub use wire::{
    decrypt_cfb_in_place, encrypt_batch, encrypt_batch_into, encrypt_batch_ref_into, inbound_null,
    should_cpu_block_compress, should_cpu_block_decrypt, should_cpu_block_encrypt,
    strip_cfb_header_if_present, CryptoBuf, InboundCryptError, OffloadProfile, CRYPTO_HEADER_SIZE,
    NONCE_SIZE,
};
