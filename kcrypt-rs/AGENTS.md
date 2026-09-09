<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-07-22 | Updated: 2026-08-03 (legacy selectors deprecated; thiserror errors) -->

# kcrypt-rs

## Purpose

Shared block-cipher and AEAD library for kcptun-rs. Port of Go `kcp-go/v5/crypt.go` with full wire compatibility for 13 methods. Extracted from `kcp-rs` so crypto can evolve independently. `kcp-rs` has **no** dependency on this crate — consume `kcrypt-rs` directly. Hosts **wire packing** (`CryptoBuf`, `encrypt_batch`, offload heuristics) in `wire.rs` since B2 (2026-07-31).

## Key Files

| File | Description |
|------|-------------|
| `Cargo.toml` | `aes`, `aes-gcm`, `twofish`, `blowfish`, `des`, `pbkdf2`, `hmac`, `sha1`, `bytes`, `crc32fast`, `parking_lot` |
| `src/lib.rs` | Preferred public API: `CryptEngine::select`; deprecated compatibility selectors, traits, and `wire::*` remain exported |
| `src/crypt.rs` | Traits `BlockCrypt` / `AeadCrypt`; CFB helpers; `GO_CFB_IV`; factory; re-exports ciphers |
| `src/wire.rs` | **Wire packing** (B2): `CryptoBuf`, `encrypt_batch(_into)`, `decrypt_cfb_in_place`, `strip_cfb_header_if_present`, `inbound_null`, `should_cpu_block_*`, `OffloadProfile`, `CRYPT_HDR`/`NONCE_SZ` |
| `src/cast5.rs` | Full CAST-128 (RFC 2144) block implementation (Go-compatible) |
| `src/des.rs` | Go-style DES/3DES Feistel boxes (~2× vs soft RustCrypto path) |

## Subdirectories

| Directory | Purpose |
|-----------|---------|
| `src/crypt/` | One module per cipher (see `src/crypt/AGENTS.md`) |

## For AI Agents

### Working In This Directory

- **CFB uses fixed IV** `GO_CFB_IV` (16 bytes hardcoded to match Go). Never randomize per-packet IV.
- `BlockCrypt::{encrypt,decrypt}` take `&self` — ciphers are **stateless after construction**.
- **Hot path:** prefer `Arc<CryptEngine>` (enum match) over `Arc<dyn BlockCrypt>`. Helpers: `is_aead()`, `as_aead()`, `uses_cfb_header(method)`.
- `select_block_crypt` / `select_aead_crypt` remain for compatibility but are deprecated; new code uses `CryptEngine::select`.
- CFB helpers are generic `<F: Fn>` for monomorphization/inlining — keep them generic, not `&dyn Fn`.
- Key selection: `CryptEngine::select` / `select_block_crypt` / `select_aead_crypt` — password typically already PBKDF2-derived 32B key from binaries (`SALT = b"kcp-go"`).
- TEA: **8 rounds** (Go uses rounds/2). SM4: tjfoc/gmsm S-box + CK fix. Do not "upgrade" defaults that break interop.
- `null`/`none` both map to no-op encrypt; packet **header** difference is handled in binaries / `CryptoBuf` via `has_encryption` / `uses_cfb_header`.
- Hot CFB paths (AES, 3DES, XTEA, Blowfish, …) are monomorphized — prefer that pattern for new ciphers.
- On aarch64, `.cargo/config.toml` sets `--cfg aes_armv8` so AES is not soft fixslice.

### Testing Requirements

- Cipher unit tests in modules / `crypt.rs`
- Any algorithm change → `bash test_e2e.sh` across crypt matrix
- Perf-sensitive CFB changes → `make bench` / flamegraph skill

### Common Patterns

```rust
// Preferred on session hot path:
let (engine, name) = CryptEngine::select("aes-128", &key);
let crypt = Arc::new(engine);
crypt.encrypt(&mut data);

// Legacy / tests:
let (cipher, name) = select_block_crypt("aes-128", &key);
cipher.encrypt(&mut data);
```

Wire packing (CFB nonce+CRC, AEAD, offload heuristics) lives here in `wire.rs` — `CryptoBuf`, `encrypt_batch`, `decrypt_cfb_in_place`, `should_cpu_block_*`, `OffloadProfile`. Moved from `kcp-rs` (B2, 2026-07-31).
`encrypt_batch` takes `&CryptEngine` (AEAD via `crypt.as_aead()`).
- `null` vs `none` header policy: `uses_cfb_header(method)` / caller `has_encryption` flag — see `wire::encrypt_batch`.
- Packet nonces come from `nonce::NonceGen` (private module): AES-128 over a counter under a per-instance random key, i.e. Go's `nonceAES128`. Nonces must be unpredictable, not just unique — `salsa20` uses the first 8 bytes as its stream nonce and CFB chains the nonce block into the keystream. Do not replace it with a plain counter: both ends of a session hold the same derived key, so their counters would produce identical keystreams. `CryptoBuf::new(session_id)` passes `session_id` as a PRF domain separator only.

## Dependencies

### Internal

None (leaf crypto crate).

### External

- `aes`, `aes-gcm`, `twofish`, `blowfish`, `des`, `pbkdf2`, `hmac`, `sha1`, `bytes`, `crc32fast`, `parking_lot`

<!-- MANUAL: -->
