<!-- Parent: ../../AGENTS.md -->
<!-- Generated: 2026-07-22 | Updated: 2026-07-22 -->

# crypt (cipher modules)

## Purpose

One implementation module per BlockCrypt/AEAD cipher used by `kcrypt-rs`. Wired through parent `src/crypt.rs` factories and re-exports.

## Key Files

| File | Description |
|------|-------------|
| `aes_cfb.rs` | AES-128/192/256 CFB (`AesCfbCrypt`) |
| `aes_gcm.rs` | AES-128-GCM AEAD (`Aes128GcmCrypt`) |
| `sm4.rs` | SM4-CFB (tjfoc/gmsm S-box) |
| `tea.rs` | TEA-CFB (8 rounds for Go compat) |
| `xtea.rs` | XTEA-CFB |
| `salsa20.rs` | Salsa20 stream |
| `blowfish.rs` | Blowfish-CFB |
| `twofish.rs` | Twofish-CFB |
| `cast5_crypt.rs` | CAST-128 CFB wrapper over `crate::cast5` |
| `triple_des.rs` | 3DES-CFB (uses Go-style DES boxes in `crate::des`) |
| `xor.rs` | Simple XOR stream (`SimpleXORCrypt`) |
| `none.rs` | No-op crypt (`NoneCrypt`) for `none`/`null` methods |

## Subdirectories

None.

## For AI Agents

### Working In This Directory

- Prefer monomorphized CFB hot paths consistent with recent AES/3DES/XTEA/Blowfish work.
- Do not change round counts, S-boxes, or IV usage without e2e crypt matrix.
- `none` vs `null` header difference is outside these modules (CryptoBuf / binaries).
- Keep `BlockCrypt`/`AeadCrypt` trait impls `Send + Sync` and encrypt/decrypt on `&self`.

### Testing Requirements

- Module tests + parent `crypt.rs` tests
- `bash test_e2e.sh` crypt matrix after algorithm edits

### Common Patterns

- Construct from key bytes in factory; no per-packet IV random (CFB fixed IV in parent)

## Dependencies

### Internal

- Parent `kcrypt-rs` (`cast5`, `des`, trait defs in `crypt.rs`)

### External

- Cipher crates re-exported via parent `Cargo.toml`

<!-- MANUAL: -->
