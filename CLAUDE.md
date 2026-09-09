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

## 6. Gate Checks (Mandatory Before Completion)

**Every change MUST pass all three gates before being declared complete. No exceptions.**

```bash
make gate
```

This runs, in order:
1. `cargo fmt --all -- --check` — formatting must be clean (zero diff)
2. `cargo test --workspace` — all tests must pass (zero failures)
3. `cargo clippy --workspace -- -D warnings` — lint must be clean (zero warnings)

If any gate fails:
- **fmt --check fails** → run `cargo fmt --all` and re-check
- **test fails** → fix the test or the code; never skip or `#[ignore]` without explicit request
- **clippy fails** → fix the root cause; never suppress with `#[allow(...)]` unless explicitly requested

These gates run in CI. A change that doesn't pass all three is not ready to commit.

## 7. Output Discipline — Failures Only

**Unit / perf / gate-check runs: care only about failures.** Never dump full command output into context — it bloats context and wastes tokens.

- Report: pass/fail totals, the list of failed test names, and the failure output itself. Suppress the passing noise (`test result: ok`, per-test `ok` lines, benchmark tables).
- Filter command output before showing it, e.g.:
  `cargo test 2>&1 | grep -E "FAILED|failures:|panicked|error:"`
  `make gate 2>&1 | grep -E "FAILED|failures:|error"`
  (For each failure, also include the failing test's body so it can be fixed.)
- Same for build / lint / benchmark / e2e output: output on demand. Show only what the current task needs — errors, failures, and the minimal summary. If a full log is genuinely required, save it to a file and read only the relevant slice.

**These guidelines are working if:** fewer unnecessary changes in diffs, fewer rewrites due to overcomplication, and clarifying questions come before implementation rather than after mistakes.

---

## Project: kcptun-rs

### AGENTS.md — AI orientation (read before scanning)

Hierarchical project maps live in `AGENTS.md` files. **Prefer these over full-repo scans** for orientation, architecture, wire formats, crate purpose, and local gotchas.

**Skill (mandatory checklist):** `.claude/skills/agents-md-orient/` — use for 分析项目 / 对比本项目 / architecture mapping, and after structural or public-API changes to sync the nearest `AGENTS.md`.

| When | Read |
|------|------|
| Session start / unfamiliar task | Root `AGENTS.md` first |
| Working in a crate or subdirectory | That directory's `AGENTS.md` (and parent if linked) |
| Nested modules (e.g. `kcrypt-rs/src/crypt`, `knet-rs/src/net`) | Nested `AGENTS.md` under that path |
| Structure or public API changed substantially | Update the nearest `AGENTS.md` (and root if workspace-level); keep `<!-- MANUAL: -->` sections |

Rules for agents:
1. **Orient with AGENTS.md, then open source files only for the code you will change.** Do not re-map the whole workspace by default.
2. Parent tags (`<!-- Parent: ../AGENTS.md -->`) form the hierarchy — follow them upward for broader context.
3. `CLAUDE.md` = behavioral rules + critical gotchas; `AGENTS.md` = structure, ownership, commands, per-area guidance. Both apply; on conflict for *how to work*, this file wins; for *what lives where*, AGENTS.md wins.
4. Skip inventing architecture from scratch when `AGENTS.md` already documents it.
5. Refresh docs after intentional structural changes; do not churn AGENTS.md for pure bugfix lines.
6. Before calling structural work done: either update nearest AGENTS or state "no AGENTS sync needed" with a one-line reason.

Tree (entry points):

```
AGENTS.md
├── kcp-rs/  kcrypt-rs/  smux-rs/  qpp-rs/  knet-rs/
├── kcptun-client/  kcptun-server/
├── bench/  .cargo/
├── bugs/   — bug reports & postmortems (not under repo root)
├── tests/  — Go e2e reference bins only (no AGENTS.md)
└── (nested) kcrypt-rs/src/crypt, knet-rs/src/{net,sync,task,time}, …
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

# Go↔Rust e2e interop test (requires Go kcptun binaries)
make e2e                # auto-builds release, then runs test_e2e.sh
bash test_e2e.sh        # same, without auto-build

# Lint (warnings = errors)
cargo clippy --workspace -- -D warnings

# Format
cargo fmt --all

# Snappy Go-Rust interop test
cargo test test_snappy_go_rust_interop -- --nocapture

# Benchmark (comprehensive cipher × compression matrix)
python3 bench_rust_vs_go.py --quick --rust-only --conn 4 --size 65536 --runs 3

# Rate limit test (per-connection token bucket)
target/release/kcptun-server -l :29900 -t 127.0.0.1:8080 --key test --crypt null --ratelimit 1048576

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
├── knet-rs/          — Async runtime + network I/O abstraction (tokio)
├── kcptun-client/   — Client binary (tokio async)
└── kcptun-server/   — Server binary (tokio async)
```

Protocol stack (bottom→top): `UDP → BlockCrypt/FEC → KCP → Snappy → SMUX Session → SMUX Stream → TCP`

### Key Design Decisions

- **Deps from crates.io**: not vendored; `.cargo/config.toml` only sets aarch64 `aes_armv8` rustflags. Cargo fetches per-platform libs at build time (network / crates.io mirror required for fresh builds).
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
- **All CFB ciphers use the same 20B wire header**: `[nonce 16B][CRC32 4B][ciphertext]`.
  Go's `postProcess()` applies this uniformly to every non-AEAD cipher including `xor` and `salsa20`.
  Per-cipher internals (e.g. salsa20 using first 8B of nonce for keystream) do NOT change the wire layout.
  Never add per-cipher wire format dispatch — all CFB ciphers go through `encrypt_cfb` / `decrypt_cfb_in_place`.
- TEA cipher uses 8 rounds (Go rounds/2) for compatibility.
- SM4 uses tjfoc/gmsm S-box with specific CK fix.
- Go requires segment ACKs on every received Push segment — `kcp_rs::KCP::input()` queues ACKs for all received push segments.
- `snd_buf` cleanup: ACKed segments are removed front-of-buffer in `flush()`, matching Go's `k.snd_buf = k.snd_buf[1:]`.
- Without `nocwnd=1`, the default congestion window is capped at 32 — bump with `--sndwnd` and/or `--nc`.
- **FEC defaults to 10/3** (Go-compatible). Send path uses session-layer `FecEncoder`;
  set `--datashard 0 --parityshard 0` to disable. Crypto wraps the whole FEC frame
  (`FecEncoder` offset=0).


### Bug reports (agents)

- **All new / ongoing bug write-ups go under `bugs/`** (never dump new
  `BUGREPORT*.md` at repo root). Prefer one file per issue:
  `bugs/BUGREPORT_<SHORT_SLUG>.md`.
- Keep historical notes in place under `bugs/`; update status in-file when fixed.
- When summarizing investigation, link the file from `AGENTS.md` Key Files if it
  is an open, multi-session item.
- Do not commit `.claude/` worktrees or session caches.

### Performance profiling (agents)

- Before speculative perf edits, run/read the project skill
  `.claude/skills/flamegraph-perf/SKILL.md` and `bench/PROFILE_RUNBOOK.md`.
- Prefer evidence from `bench/profiles/HOTSPOTS.md` + re-bench over guesswork.
- One optimization class per change; keep wire compatibility; use shared
  `encrypt_batch` paths to avoid client/server drift.
- On aarch64, ensure `.cargo/config.toml` keeps `--cfg aes_armv8` (hand-maintained)
  so AES does not fall back to soft fixslice.


### Recent unpushed work (session summary — 2026-07-21 → 2026-07-22)

Commits on `master` ahead of `origin` (newest last):

1. `196408e` true-idle + MPMC `cpu_block` + no nested encrypt parallel  
2. `dc419b6` L3 3des flamegraph notes  
3. `c3945e0` snmp_logger reads `DEFAULT_SNMP` (was always zeros)  
4. `7420f3a` Go-compatible SNMP fields + **opt-in** collection (`snmp_enable` only if `--snmplog` + period>0)  
5. `39e85df` Snappy large-flush → `cpu_block` when null/none crypto  
6. `42bee6b` 3DES CFB monomorphize; skip snappy offload under heavy crypto  
7. `589b06c` proxy SMUX stream leak memory growth bug report  
8. `b5ac159` XTEA / Blowfish CFB monomorphize  

Also staged: move `BUGREPORT*.md` → `bugs/`.

**Open bugs:** see `bugs/` (esp. `bugs/BUGREPORT_PROXY_MEMORY_GROWTH.md`).

### Go-compatible pprof from Rust

Prefer `--pprof` + `go tool pprof` for human-readable stacks:

```bash
bash bench/profile_rust_go_pprof.sh server 20
go tool pprof -http=127.0.0.1:0 bench/profiles/rust-server-*.pb
```

Standard binaries enable the Go-compatible pprof HTTP server feature by default; pass `--pprof` to listen on `:6060`. Use `--features pprof-deadlock` for deadlock detection.

<!-- gitnexus:start -->
# GitNexus — Code Intelligence

This project is indexed by GitNexus as **kcptun-rs** (6212 symbols, 13722 relationships, 300 execution flows). Use the GitNexus MCP tools to understand code, assess impact, and navigate safely.

> Index stale? Run `node .gitnexus/run.cjs analyze` from the project root — it auto-selects an available runner. No `.gitnexus/run.cjs` yet? `npx gitnexus analyze` (npm 11 crash → `npm i -g gitnexus`; #1939).

## Always Do

- **MUST run impact analysis before editing any symbol.** Before modifying a function, class, or method, run `impact({target: "symbolName", direction: "upstream"})` and report the blast radius (direct callers, affected processes, risk level) to the user.
- **MUST run `detect_changes()` before committing** to verify your changes only affect expected symbols and execution flows. For regression review, compare against the default branch: `detect_changes({scope: "compare", base_ref: "master"})`.
- **MUST warn the user** if impact analysis returns HIGH or CRITICAL risk before proceeding with edits.
- When exploring unfamiliar code, use `query({query: "concept"})` to find execution flows instead of grepping. It returns process-grouped results ranked by relevance.
- When you need full context on a specific symbol — callers, callees, which execution flows it participates in — use `context({name: "symbolName"})`.

## Never Do

- NEVER edit a function, class, or method without first running `impact` on it.
- NEVER ignore HIGH or CRITICAL risk warnings from impact analysis.
- NEVER rename symbols with find-and-replace — use `rename` which understands the call graph.
- NEVER commit changes without running `detect_changes()` to check affected scope.

## Resources

| Resource | Use for |
|----------|---------|
| `gitnexus://repo/kcptun-rs/context` | Codebase overview, check index freshness |
| `gitnexus://repo/kcptun-rs/clusters` | All functional areas |
| `gitnexus://repo/kcptun-rs/processes` | All execution flows |
| `gitnexus://repo/kcptun-rs/process/{name}` | Step-by-step execution trace |

## CLI

| Task | Read this skill file |
|------|---------------------|
| Understand architecture / "How does X work?" | `.claude/skills/gitnexus/gitnexus-exploring/SKILL.md` |
| Blast radius / "What breaks if I change X?" | `.claude/skills/gitnexus/gitnexus-impact-analysis/SKILL.md` |
| Trace bugs / "Why is X failing?" | `.claude/skills/gitnexus/gitnexus-debugging/SKILL.md` |
| Rename / extract / split / refactor | `.claude/skills/gitnexus/gitnexus-refactoring/SKILL.md` |
| Tools, resources, schema reference | `.claude/skills/gitnexus/gitnexus-guide/SKILL.md` |
| Index, status, clean, wiki CLI commands | `.claude/skills/gitnexus/gitnexus-cli/SKILL.md` |

<!-- gitnexus:end -->
