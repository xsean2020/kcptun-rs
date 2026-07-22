<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-07-22 | Updated: 2026-07-22 -->

# qpp-rs

## Purpose

Quantum Permutation Pad (QPP) stream obfuscation — port of Go `xtaci/qpp` with **algorithmic compatibility** (same ciphertext for same key/data/pad config). Optional upper layer on SMUX streams in client/server.

## Key Files

| File | Description |
|------|-------------|
| `Cargo.toml` | `sha1`, `sha2`, `hmac`, `aes`, `pbkdf2` |
| `src/lib.rs` | Entire crate: `QuantumPermutationPad`, `Rand` (xoshiro256**), pad encrypt/decrypt helpers |

## Subdirectories

None.

## For AI Agents

### Working In This Directory

- Must match Go: xoshiro256** (xoshiro256ss), PBKDF2(SHA1, 128 rounds), `PAD_SWITCH=8`, AES-256 Fisher-Yates pad construction.
- Constants: `QPP_MIN_SEED_LENGTH=32`, `QPP_POWER=8`, `QPP_PAD_SIZE=256`, `QPP_MINIMUM_PADS=3`.
- Used from binaries as optional `QPPPort` wrapper around SMUX streams — not on the UDP/KCP path.
- Do not "modernize" PRNG or KDF without breaking Go interop tests.

### Testing Requirements

- In-file unit tests in `lib.rs`
- e2e with QPP enabled if Go matrix covers it

### Common Patterns

```rust
use qpp_rs::{QuantumPermutationPad, create_qpp_prng};
```

## Dependencies

### Internal

None (leaf).

### External

- `sha1`, `sha2`, `hmac`, `aes`, `pbkdf2`

<!-- MANUAL: -->
