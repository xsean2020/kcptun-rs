<!-- Parent: ../AGENTS.md -->
<!-- Generated: 2026-07-20 | Updated: 2026-07-20 -->

# kcptun-go

## Purpose

Staging area for **reference Go kcptun binaries** used by root `test_e2e.sh` (Go↔Rust matrix). Not a Rust crate.

## Key Files

| File | Description |
|------|-------------|
| `README.md` | Notes Go source location (`/Users/sean/Documents/kcptun`) |
| `go.mod` | Minimal module stub if present |
| `client` / `server` | Built binaries (**gitignored**, rebuild as needed) |

## For AI Agents

### Working In This Directory

- Rebuild from Go kcptun source when binaries missing or outdated.
- Do not commit large binaries.
- e2e script expects paths: `./tests/kcptun-go/server` and `./tests/kcptun-go/client`.

### Testing Requirements

```bash
# from repo root after Go build + Rust release
bash test_e2e.sh
```

<!-- MANUAL: -->
