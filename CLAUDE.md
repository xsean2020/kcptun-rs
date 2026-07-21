# CLAUDE.md

Behavioral guidelines to reduce common LLM coding mistakes. Merge with project-specific instructions as needed.

**Tradeoff:** These guidelines bias toward caution over speed. For trivial tasks, use judgment.

## 1. Think Before Coding

**Don't assume. Don't hide confusion. Surface tradeoffs.**

Before implementing:
- State your assumptions explicitly. If uncertain, ask.
- If multiple interpretations exist, present them - don't pick silently.
- If a simpler approach exists, say so. Push back when warranted.
- If something is unclear, stop. Name what's confusing. Ask.

## 2. Simplicity First

**Minimum code that solves the problem. Nothing speculative.**

- No features beyond what was asked.
- No abstractions for single-use code.
- No "flexibility" or "configurability" that wasn't requested.
- No error handling for impossible scenarios.
- If you write 200 lines and it could be 50, rewrite it.

Ask yourself: "Would a senior engineer say this is overcomplicated?" If yes, simplify.

## 3. Surgical Changes

**Touch only what you must. Clean up only your own mess.**

When editing existing code:
- Don't "improve" adjacent code, comments, or formatting.
- Don't refactor things that aren't broken.
- Match existing style, even if you'd do it differently.
- If you notice unrelated dead code, mention it - don't delete it.

When your changes create orphans:
- Remove imports/variables/functions that YOUR changes made unused.
- Don't remove pre-existing dead code unless asked.

The test: Every changed line should trace directly to the user's request.

## 4. Goal-Driven Execution

**Define success criteria. Loop until verified.**

Transform tasks into verifiable goals:
- "Add validation" → "Write tests for invalid inputs, then make them pass"
- "Fix the bug" → "Write a test that reproduces it, then make it pass"
- "Refactor X" → "Ensure tests pass before and after"

For multi-step tasks, state a brief plan:
```
1. [Step] → verify: [check]
2. [Step] → verify: [check]
3. [Step] → verify: [check]
```

Strong success criteria let you loop independently. Weak criteria ("make it work") require constant clarification.

## 5. Zero Warnings, Zero Errors

**Compilation must be flawless. Warnings are bugs in disguise.**

The code must compile perfectly on the first try without any warnings, errors, or linter complaints.

- **Treat Warnings as Errors:** Never ignore compiler warnings (e.g., unused variables, dead code, implicit type conversions, or style lints). If a warning appears, fix the root cause immediately—do not suppress it with annotations or macros unless explicitly requested.
- **Strict Lint Compliance:** Adhere strictly to the project's default linter and formatter rules (e.g., `golangci-lint` for Go, `cargo clippy` for Rust).
- **No Residual Drafts:** Ensure all experimental, commented-out code, temporary debug logs, or unused imports are completely removed before declaring completion.

Ask yourself: "Will the CI/CD pipeline fail or throw a warning because of this change?" If yes, it is not ready.

---

**These guidelines are working if:** fewer unnecessary changes in diffs, fewer rewrites due to overcomplication, and clarifying questions come before implementation rather than after mistakes.

---

## Project: kcptun-rs

### AGENTS.md — AI orientation (read before scanning)

Hierarchical project maps live in `AGENTS.md` files. **Prefer these over full-repo scans** for orientation, architecture, wire formats, crate purpose, and local gotchas.

| When | Read |
|------|------|
| Session start / unfamiliar task | Root `AGENTS.md` first |
| Working in a crate or subdirectory | That directory's `AGENTS.md` (and parent if linked) |
| Nested modules (e.g. `kcrypt-rs/src/crypt`, `kio-rs/src/net`) | Nested `AGENTS.md` under that path |
| Structure or public API changed substantially | Update the nearest `AGENTS.md` (and root if workspace-level); keep `<!-- MANUAL: -->` sections |

Rules for agents:
1. **Orient with AGENTS.md, then open source files only for the code you will change.** Do not re-map the whole workspace by default.
2. Parent tags (`<!-- Parent: ../AGENTS.md -->`) form the hierarchy — follow them upward for broader context.
3. `CLAUDE.md` = behavioral rules + critical gotchas; `AGENTS.md` = structure, ownership, commands, per-area guidance. Both apply; on conflict for *how to work*, this file wins; for *what lives where*, AGENTS.md wins.
4. Skip inventing architecture from scratch when `AGENTS.md` already documents it.
5. Refresh docs after intentional structural changes; do not churn AGENTS.md for pure bugfix lines.

Tree (entry points):

```
AGENTS.md
├── kcp-rs/  kcrypt-rs/  smux-rs/  qpp-rs/  kio-rs/
├── kcptun-client/  kcptun-server/
├── bench/  tests/  .cargo/
└── (nested) kcrypt-rs/src/crypt, kio-rs/src/{net,sync,task,time}, …
```

### Commands

```bash
# Build (debug)
cargo build --workspace

# Release build (opt-level=3, LTO, stripped, panic=abort)
cargo build --release

# Run all unit tests
cargo test --workspace

# Run stress tests (data integrity + concurrency, requires release build first)
cargo test --release --package kcptun-server --test stress_test -- --nocapture --test-threads=1

# Specific stress test
cargo test --release --package kcptun-server --test stress_test -- test_multithread_100_connections -- --nocapture

# Go↔Rust e2e interop test (tokio + smol; requires Go kcptun binaries)
make e2e                # auto-builds release + release-smol, then runs test_e2e.sh
bash test_e2e.sh        # same, without auto-build

# Lint (warnings = errors)
cargo clippy --workspace -- -D warnings

# Format
cargo fmt --all

# Snappy Go-Rust interop test
cargo test test_snappy_go_rust_interop -- --nocapture

# Makefile shortcuts: make build/test/release/stress/clippy/fmt
```

### Workspace Architecture

7 crates in a Cargo workspace, wire-compatible with Go kcptun (xtaci/kcp-go v5):

```
kcptun-rs/
├── kcp-rs/          — KCP ARQ protocol state machine (reliable UDP)
├── kcrypt-rs/       — 13 block ciphers + AES-128-GCM (extracted from kcp-rs)
├── smux-rs/         — SMUX stream multiplexer (v1/v2)
├── qpp-rs/          — Quantum Permutation Pad obfuscation
├── kio-rs/          — Async runtime + network I/O abstraction (tokio / smol)
├── kcptun-client/   — Client binary (tokio / smol async)
└── kcptun-server/   — Server binary (tokio / smol async)
```

Protocol stack (bottom→top): `UDP → BlockCrypt/FEC → KCP → Snappy → SMUX Session → SMUX Stream → TCP`

### Key Design Decisions

- **Vendored deps**: `.cargo/config.toml` points to `vendor/` directory. Run `make vendor` to refresh.
- **Global allocator**: Both binaries use `mimalloc::MiMalloc`.
- **kcp-rs clippy lints suppressed**: Matches Go kcp-go v5 control flow exactly — many clippy rules are allowed at the crate level to preserve that correspondence. Don't "fix" them.
- **KCP lock contention**: Flush loop is split into 4 phases — phases 1-3 run outside the KCP mutex; only phase 4 (`kcp.send()/update()/flush()`) holds it briefly.
- **Snappy compression at SMUX session level** (not per-stream), matching Go's `github.com/golang/snappy`.
- **PBKDF2 key derivation**: `SALT = b"kcp-go"`, matching Go.
- **Crypto wire format**: `[nonce 16B][CRC32 4B][payload]` for CFB ciphers; `[nonce 12B][ciphertext+tag 16B]` for AES-GCM.
- **KCP segment header**: 24 bytes, little-endian: `conv(4)|cmd(1)|frg(1)|wnd(2)|ts(4)|sn(4)|una(4)|len(4)`.
- **SMUX frame header**: 8 bytes: `ver(1)|cmd(1)|length(2) LE|stream_id(4) LE`.
- **CFB uses fixed IV** (`GO_CFB_IV` in `kcrypt-rs/src/crypt.rs`), matching Go kcp-go.
- **Nonce generation**: `CryptoBuf` uses an `AtomicU64` counter + session ID (no PRNG per packet).

### Important Gotchas

- `--key`, `--crypt`, `--mode`, and `--nocomp` must match between client and server.
- Compression is **enabled by default** (`--nocomp` = false), matching Go behavior.
- The `null` cipher has no crypto header at all (different from `none` which has the header but no encryption).
- TEA cipher uses 8 rounds (Go rounds/2) for compatibility.
- SM4 uses tjfoc/gmsm S-box with specific CK fix.
- Go requires segment ACKs on every received Push segment — `kcp_rs::KCP::input()` queues ACKs for all received push segments.
- `snd_buf` cleanup: ACKed segments are removed front-of-buffer in `flush()`, matching Go's `k.snd_buf = k.snd_buf[1:]`.
- Without `nocwnd=1`, the default congestion window is capped at 32 — bump with `--sndwnd` and/or `--nc`.

### Performance profiling (agents)

- Before speculative perf edits, run/read the project skill
  `.claude/skills/flamegraph-perf/SKILL.md` and `bench/PROFILE_RUNBOOK.md`.
- Prefer evidence from `bench/profiles/HOTSPOTS.md` + re-bench over guesswork.
- One optimization class per change; keep wire compatibility; use shared
  `encrypt_batch` paths to avoid client/server drift.
- On aarch64, ensure `.cargo/config.toml` keeps `--cfg aes_armv8` (regenerated by
  `make vendor`) so AES does not fall back to soft fixslice.

### Go-compatible pprof from Rust

Prefer `--pprof ADDR` + `go tool pprof` for human-readable stacks:

```bash
bash bench/profile_rust_go_pprof.sh server 20
go tool pprof -http=127.0.0.1:0 bench/profiles/rust-server-*.pb
```

samply/Speedscope remains available via `bench/profile_flamegraph.sh` (post-processed `*.named.json.gz`).

