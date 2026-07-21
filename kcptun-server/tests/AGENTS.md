<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-07-20 | Updated: 2026-07-20 -->

# tests

## Purpose

Integration / stress tests for `kcptun-server` (and client binary as peer). Primary artifact: multi-connection data integrity stress suite.

## Key Files

| File | Description |
|------|-------------|
| `stress_test.rs` | Spawns release client+server+echo; N threads; payloads 1 B–512 KB; byte-exact verify |

## For AI Agents

### Working In This Directory

- Requires **release** binaries under workspace `target/release/` (see `find_bin`).
- Use `--test-threads=1` for stress to avoid port races.
- `TestEnv` kills ports via `lsof` — macOS/Linux oriented.
- Config helpers: crypt, nocomp, conn count variants.

### Testing Requirements

```bash
make release && make stress
cargo test --release -p kcptun-server --test stress_test -- --nocapture --test-threads=1
```

### Common Patterns

Start env → many TCP clients to client listen port → echo through tunnel → assert.

## Dependencies

### Internal
Built `kcptun-client` / `kcptun-server` binaries

<!-- MANUAL: -->
