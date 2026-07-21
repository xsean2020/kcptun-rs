<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-07-20 | Updated: 2026-07-20 -->

# tests

## Purpose

External test assets for end-to-end interoperability. Holds prebuilt **Go kcptun** client/server used by `test_e2e.sh` at the repo root.

## Key Files

None at this level.

## Subdirectories

| Directory | Purpose |
|-----------|---------|
| `kcptun-go/` | Go reference binaries + notes (see `kcptun-go/AGENTS.md`) |

## For AI Agents

### Working In This Directory

- Binaries are **gitignored** (`tests/kcptun-go/client`, `server`). Rebuild from Go source if missing.
- Go source path (local): `/Users/sean/Documents/kcptun` (documented in `kcptun-go/README.md`).
- Do not check large binaries into git.

### Testing Requirements

```bash
bash test_e2e.sh   # from repo root
```

<!-- MANUAL: -->
