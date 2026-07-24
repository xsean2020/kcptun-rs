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

// Re-export the primary public API at the crate root for convenience.
pub use crypt::{
    select_aead_crypt, select_block_crypt, AeadCrypt, BlockCrypt, CryptEngine,
};
