<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-07-20 | Updated: 2026-07-20 -->

# .cargo

## Purpose

Cargo configuration for **vendored** third-party dependencies. Generated/maintained by `make vendor`.

## Key Files

| File | Description |
|------|-------------|
| `config.toml` | Redirects crates-io to `../vendor`; also sets `aes_armv8` rustflags for aarch64 |

## For AI Agents

### Working In This Directory

- Do not hand-edit unless you know you need offline registry overrides.
- After dependency changes: `make vendor` (rewrites this file and `vendor/`).
- **Preserve `aes_armv8` rustflags** for `aarch64-apple-darwin` / `aarch64-unknown-linux-gnu` — without them RustCrypto `aes` 0.8 uses soft AES on Apple Silicon (large CFB throughput regression). `make vendor` regenerates these flags.

### Testing Requirements

`cargo build --workspace` offline should still resolve via vendor.

<!-- MANUAL: -->
