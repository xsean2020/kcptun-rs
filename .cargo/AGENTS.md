<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-07-22 | Updated: 2026-07-22 -->

# .cargo

## Purpose

Cargo configuration for vendored dependencies and target-specific rustflags. Regenerated/rewritten in part by `make vendor`.

## Key Files

| File | Description |
|------|-------------|
| `config.toml` | `crates-io` → `vendor/`; aarch64 `rustflags = ["--cfg", "aes_armv8"]` for Apple Darwin + Linux GNU |

## Subdirectories

None.

## For AI Agents

### Working In This Directory

- Do not hand-edit the source replacement block without also running/understanding `make vendor`.
- **Keep** aarch64 `aes_armv8` cfg — without it AES falls back to soft fixslice (major perf cliff).
- Vendor dir is repo-root `vendor/` (not under `.cargo/`).

### Testing Requirements

- After vendor refresh: `cargo build --workspace` / `make build`

### Common Patterns

- Offline builds rely on `vendor/` + this config

## Dependencies

### Internal

- `Makefile` `vendor` / `vendor-force` targets

### External

- Cargo / rustc

<!-- MANUAL: -->
