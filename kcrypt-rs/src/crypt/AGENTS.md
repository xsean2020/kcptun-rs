<!-- Parent: ../../AGENTS.md -->
<!-- Generated: 2026-07-20 | Updated: 2026-07-20 -->

# crypt

## Purpose

Per-cipher implementations of `BlockCrypt` / `AeadCrypt`. Each file is a self-contained cipher matching Go kcp-go naming and wire behavior. Parent `crypt.rs` owns traits, CFB helpers, `GO_CFB_IV`, and `select_*` factories.

## Key Files

| File | Description |
|------|-------------|
| `none.rs` | No-op cipher (`none` / `null` payload transform) |
| `xor.rs` | Simple XOR with PBKDF2-expanded key (`SALT_XOR`) |
| `aes_cfb.rs` | AES-128/192/256 CFB |
| `aes_gcm.rs` | AES-128-GCM AEAD (`AeadCrypt`) |
| `sm4.rs` | SM4 CFB (tjfoc/gmsm S-box + CK fix) |
| `tea.rs` | TEA CFB, **8 rounds** |
| `xtea.rs` | XTEA CFB, 64 rounds |
| `salsa20.rs` | Salsa20 stream |
| `blowfish.rs` | Blowfish CFB |
| `twofish.rs` | Twofish CFB |
| `cast5_crypt.rs` | CAST5/CAST-128 CFB wrapping `cast5.rs` |
| `triple_des.rs` | 3DES CFB |

## For AI Agents

### Working In This Directory

- Implement only `encrypt`/`decrypt` (or AEAD seal/open). Packet framing is outside this module.
- Use parent CFB helpers (`cfb8_enc/dec`, `cfb16_enc/dec`) with monomorphized closures for speed.
- Never change round counts / S-boxes without Go interop proof.
- `name()` return values must match CLI crypt strings (`"aes-128"`, `"3des"`, etc.).

### Testing Requirements

Change one cipher → e2e that crypt method; full matrix if CFB helpers change.

### Common Patterns

Construct from 16/24/32-byte key material; store expanded round keys; process in place.

## Dependencies

### Internal
- Parent `crypt` module helpers; `crate::cast5` for CAST5

### External
Cipher crates re-exported via parent Cargo.toml

<!-- MANUAL: -->
