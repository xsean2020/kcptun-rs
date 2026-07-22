<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-07-22 | Updated: 2026-07-22 -->

# kcrypt-rs

## Purpose

Shared block-cipher and AEAD library for kcptun-rs. Port of Go `kcp-go/v5/crypt.go` with full wire compatibility for 13 methods. Extracted from `kcp-rs` so crypto can evolve independently; `kcp-rs` re-exports this crate.

## Key Files

| File | Description |
|------|-------------|
| `Cargo.toml` | `aes`, `aes-gcm`, `twofish`, `blowfish`, `des`, `pbkdf2`, `hmac`, `sha1`, `bytes` |
| `src/lib.rs` | Public API: `select_block_crypt`, `select_aead_crypt`, traits, `CryptEngine` |
| `src/crypt.rs` | Traits `BlockCrypt` / `AeadCrypt`; CFB helpers; `GO_CFB_IV`; factory; re-exports ciphers |
| `src/cast5.rs` | Full CAST-128 (RFC 2144) block implementation (Go-compatible) |
| `src/des.rs` | Go-style DES/3DES Feistel boxes (~2× vs soft RustCrypto path) |

## Subdirectories

| Directory | Purpose |
|-----------|---------|
| `src/crypt/` | One module per cipher (see `src/crypt/AGENTS.md`) |

## For AI Agents

### Working In This Directory

- **CFB uses fixed IV** `GO_CFB_IV` (16 bytes hardcoded to match Go). Never randomize per-packet IV.
- `BlockCrypt::{encrypt,decrypt}` take `&self` — ciphers are **stateless after construction**. Callers store `Arc<dyn BlockCrypt>` without Mutex.
- CFB helpers are generic `<F: Fn>` for monomorphization/inlining — keep them generic, not `&dyn Fn`.
- Key selection: `select_block_crypt(name, password)` / `select_aead_crypt` — password typically already PBKDF2-derived 32B key from binaries (`SALT = b"kcp-go"`).
- TEA: **8 rounds** (Go uses rounds/2). SM4: tjfoc/gmsm S-box + CK fix. Do not "upgrade" defaults that break interop.
- `null`/`none` both map to no-op encrypt; packet **header** difference is handled in binaries / `CryptoBuf`.
- Hot CFB paths (AES, 3DES, XTEA, Blowfish, …) are monomorphized — prefer that pattern for new ciphers.
- On aarch64, `.cargo/config.toml` sets `--cfg aes_armv8` so AES is not soft fixslice.

### Testing Requirements

- Cipher unit tests in modules / `crypt.rs`
- Any algorithm change → `bash test_e2e.sh` across crypt matrix
- Perf-sensitive CFB changes → `make bench` / flamegraph skill

### Common Patterns

```rust
let (cipher, name) = select_block_crypt("aes-128", &key);
cipher.encrypt(&mut data);
cipher.decrypt(&mut data);
```

Wire packing (CFB nonce+CRC) is done by `kcp_rs::CryptoBuf`, not this crate.

## Dependencies

### Internal

None (leaf crypto crate).

### External

- `aes`, `aes-gcm`, `twofish`, `blowfish`, `des`, `pbkdf2`, `hmac`, `sha1`, `bytes`

<!-- MANUAL: -->
