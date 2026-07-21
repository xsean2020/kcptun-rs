<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-07-20 | Updated: 2026-07-20 -->

# qpp-rs

## Purpose

Quantum Permutation Pad (QPP) stream obfuscation — algorithmic port of Go `xtaci/qpp`. Optional per-stream layer above SMUX (enabled via `--QPP` / `--QPPCount`). Produces **byte-identical** ciphertext to Go given the same key, data, and pad count.

## Key Files

| File | Description |
|------|-------------|
| `Cargo.toml` | `sha1`, `sha2`, `hmac`, `aes`, `pbkdf2` |
| `src/lib.rs` | Entire implementation (~500 LOC): xoshiro256**, pads, encrypt/decrypt |

## Subdirectories

None.

## For AI Agents

### Working In This Directory

- Preserve exact Go constants: salts, `PBKDF2_LOOPS=128`, `CHUNK_DERIVE_LOOPS=1024`, `PAD_SWITCH=8`, `QUBITS=8`, pad size 256.
- PRNG is **xoshiro256\*\*** matching Go `xoshiro256ss` — do not swap for a Rust crate RNG.
- Key derivation: HMAC-SHA256 selector + PBKDF2-HMAC-SHA1; Fisher-Yates shuffle via AES-256.
- Pad count should be prime (CLI documents this); min pads = 3; min seed length = 32.
- Runtime-agnostic pure crypto — no async.

### Testing Requirements

- Any PRNG/pad change requires Go interop vectors (e2e with `--QPP`)
- Prefer golden tests against Go outputs if available

### Common Patterns

Binaries wrap streams: `QPPPort<T>` applies QPP on read/write over an inner `AsyncRead+AsyncWrite`.

## Dependencies

### Internal
None.

### External
`sha1`, `sha2`, `hmac`, `aes`, `pbkdf2`

<!-- MANUAL: -->
