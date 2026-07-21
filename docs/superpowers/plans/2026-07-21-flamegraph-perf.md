# Flamegraph-Driven Perf Loop + Project Skill Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Install a reproducible flamegraph workflow on macOS arm64, capture L1–L4 profiles, land at least one evidence-backed optimization (L1 priority), and codify the whole loop as a project skill plus README/CLAUDE/PERF entry points.

**Architecture:** Scripted tunnel + bulk/stress load under `samply` (Speedscope) with dtrace/`cargo-flamegraph` fallback; hotspot notes map to `PERF_OPTIMIZATION_PLAN.md` residual items; surgical code change only when profiles show ≥~5% actionable cost; skill under `.claude/skills/flamegraph-perf/` records real commands and outcomes.

**Tech Stack:** Rust release binaries (tokio default), `samply`, `bench/throughput.py`, existing stress tests, bash 3.2-compatible scripts, Speedscope JSON artifacts (gitignored).

**Spec:** `docs/superpowers/specs/2026-07-21-flamegraph-perf-design.md`

---

## File map

| Path | Responsibility |
|------|----------------|
| `bench/profile_flamegraph.sh` | One-shot: release check, echo+server+client, samply attach, bulk load, optional stress |
| `bench/PROFILE_RUNBOOK.md` | Human runbook: tools, matrix, reading stacks, verify loop |
| `bench/profiles/.gitkeep` | Keep empty profiles dir |
| `bench/profiles/README.md` | How to regenerate L1–L4 artifacts |
| `bench/profiles/HOTSPOTS.md` | Written ranking after real captures (committed text, not binary profiles) |
| `.gitignore` | Ignore `bench/profiles/*.{json,svg,speedscope,gz,log}` etc. |
| `.claude/skills/flamegraph-perf/SKILL.md` | Project skill for agents |
| `README.md` / `README.zh.md` | User-facing profiling section |
| `CLAUDE.md` | Agent rule: profile before speculative perf edits |
| `PERF_OPTIMIZATION_PLAN.md` | Check flamegraph script items; note outcomes |
| `CHANGELOG.md` | Perf/tooling notes if code or measurable deltas land |
| `Makefile` | Optional `make profile` → script |
| Hot-path crates (only if Task 5 finds evidence) | Likely `kcp-rs/`, `kcrypt-rs/`, `smux-rs/`, `kio-rs/`, or binary flush loops |

---

### Task 1: Gitignore + profiles directory scaffolding

**Files:**
- Create: `bench/profiles/.gitkeep`
- Create: `bench/profiles/README.md`
- Modify: `.gitignore`

- [ ] **Step 1: Create profiles README**

```markdown
# bench/profiles

Generated flamegraph / samply artifacts live here. **Do not commit large binary or JSON profiles.**

## Regenerate

```bash
make release
bash bench/profile_flamegraph.sh all
```

Artifacts:

| File pattern | Scenario |
|--------------|----------|
| `L1-null-nocomp-*.json` | Bulk null + nocomp |
| `L2-aes-nocomp-*.json` | Bulk AES + nocomp |
| `L3-3des-nocomp-*.json` | Bulk 3des + nocomp |
| `L4-stress-*.json` | Multi-conn stress under sampler |

Interpretation notes (committed): `HOTSPOTS.md` (created after first real capture).
```

- [ ] **Step 2: Add .gitkeep**

```bash
touch bench/profiles/.gitkeep
```

- [ ] **Step 3: Update `.gitignore`**

Append:

```gitignore
# Flamegraph / sampling artifacts (regenerate via bench/profile_flamegraph.sh)
/bench/profiles/**
!/bench/profiles/.gitkeep
!/bench/profiles/README.md
!/bench/profiles/HOTSPOTS.md
```

- [ ] **Step 4: Commit**

```bash
git add .gitignore bench/profiles/.gitkeep bench/profiles/README.md
git commit -m "chore(bench): scaffold flamegraph profiles dir and gitignore"
```

---

### Task 2: Install / verify sampling tools

**Files:** none (environment)

- [ ] **Step 1: Check existing tools**

```bash
which samply || true
cargo install --list | rg -i 'samply|flamegraph' || true
uname -m
```

Expected on this machine: `arm64` macOS; `samply` may be missing.

- [ ] **Step 2: Install samply (primary)**

```bash
cargo install samply --locked
samply --version
```

Expected: version prints without error.

If install fails (network/sandbox): document failure in runbook and try fallback:

```bash
cargo install flamegraph --locked
```

Note: `cargo-flamegraph` on macOS uses dtrace and often needs elevated privileges / SIP considerations. Prefer samply.

- [ ] **Step 3: Confirm release binaries exist**

```bash
make release
test -x target/release/kcptun-server && test -x target/release/kcptun-client && echo OK
```

Expected: `OK`

- [ ] **Step 4: Commit nothing** (env only). If you added a `tools` note file, do not invent one—notes go into Task 3 runbook.

---

### Task 3: `bench/profile_flamegraph.sh` + runbook

**Files:**
- Create: `bench/profile_flamegraph.sh` (executable)
- Create: `bench/PROFILE_RUNBOOK.md`
- Modify: `Makefile` (add `profile` target)

- [ ] **Step 1: Write the profiling script**

Create `bench/profile_flamegraph.sh` with bash 3.2 compatibility (macOS default). Required behavior:

1. `cd` to repo root (same pattern as `bench/run_bench.sh`).
2. Env vars:
   - `KEY=bench-key`
   - `DATA_MB=${BENCH_DATA_MB:-50}`
   - `CHUNK_KB=${BENCH_KB:-128}`
   - `CRYPT` overridden per scenario
   - `PROFILE_BIN` default `target/release/kcptun-server` (sample **server** by default; allow `PROFILE_SIDE=client|server|both`)
   - `OUT_DIR=bench/profiles`
3. Reuse helpers from `run_bench.sh` style: `cleanup`, `next_ports`, `wait_for_port`, `start_echo`, `start_server`, `start_client`.
4. Scenarios via first arg: `l1|l2|l3|l4|all` (default `all`).
5. For L1–L3:
   - Set `SERVER_ARGS` / `CLIENT_ARGS` to `--crypt <c> --nocomp --mode fast`
   - L1: `null`, L2: `aes`, L3: `3des`
   - Start echo + server + client
   - Run sampler on the chosen binary PID while `python3 bench/throughput.py "$CLIENT_PORT" --data-mb "$DATA_MB" --chunk-kb "$CHUNK_KB" --latency-iterations 10`
   - Save under `bench/profiles/L{n}-<crypt>-nocomp-<timestamp>.json` (or samply default + `mv`)
6. For L4:
   - Prefer: run `samply record` around  
     `cargo test --release -p kcptun-server --test stress_test test_multithread_10_connections -- --nocapture --test-threads=1`  
   - If that is too noisy, fall back to longer bulk with `--conn 4` if CLI supports multi-conn; otherwise document stress-only path.
7. Detect sampler:

```bash
if command -v samply >/dev/null 2>&1; then
  SAMPLER=samply
elif command -v cargo-flamegraph >/dev/null 2>&1 || command -v flamegraph >/dev/null 2>&1; then
  SAMPLER=flamegraph
else
  echo "No samply/flamegraph; install: cargo install samply"
  exit 1
fi
```

Samply attach pattern (adjust to installed CLI — verify with `samply --help`):

```bash
# Preferred: record by spawning workload under samply when possible.
# If attaching to running PID:
samply record --save-only -o "$OUT_FILE" --pid "$SERVER_PID" &
SAMP_PID=$!
# run throughput
python3 bench/throughput.py ...
kill -INT "$SAMP_PID" 2>/dev/null || true
wait "$SAMP_PID" 2>/dev/null || true
```

If `samply record` only supports wrapping a command, use:

```bash
samply record --save-only -o "$OUT_FILE" -- \
  "$SERVER_BIN" -l "0.0.0.0:$SERVER_PORT" -t "127.0.0.1:$ECHO_PORT" --key "$KEY" $SERVER_ARGS &
```

…and start client separately; stop after throughput completes.

8. Print absolute path of each artifact and git SHA:

```bash
echo "git=$(git rev-parse --short HEAD)"
echo "artifact=$OUT_FILE"
```

9. `trap cleanup EXIT` must kill client/server/echo and any leftover sampler.

Minimal CLI usage header in script:

```bash
# Usage:
#   bash bench/profile_flamegraph.sh           # all scenarios
#   bash bench/profile_flamegraph.sh l1
#   BENCH_DATA_MB=100 bash bench/profile_flamegraph.sh l2
#   PROFILE_SIDE=client bash bench/profile_flamegraph.sh l1
```

- [ ] **Step 2: Make executable and dry-run help**

```bash
chmod +x bench/profile_flamegraph.sh
bash bench/profile_flamegraph.sh 2>&1 | head -5 || true
# If no args runs all, skip head test — instead:
bash -n bench/profile_flamegraph.sh
```

Expected: `bash -n` exits 0.

- [ ] **Step 3: Write `bench/PROFILE_RUNBOOK.md`**

Must include:

- Prerequisites (`make release`, `cargo install samply`)
- Scenario matrix L1–L4 with exact crypt flags
- How to open Speedscope (`samply load file.json` or https://www.speedscope.app)
- Symbol map for this repo:

| Symbol / pattern | Layer |
|------------------|--------|
| `encrypt_batch` / `should_cpu_block_encrypt` | crypto batch |
| `CryptEngine` / cipher `encrypt` | block crypt |
| `KCP::flush` / `KCP::input` / `KCP::send` | ARQ |
| `encode_header_into` / SMUX flush | mux |
| snappy compress/decompress | compression |
| `send_batch` / `recv` | UDP I/O |
| `cpu_block` / tokio runtime | scheduling |

- Optimization decision tree (from design §7)
- Verify commands after a code change
- macOS gotchas (no Linux perf; pprof flag is placeholder)

- [ ] **Step 4: Makefile target**

Near `bench:` target, add:

```makefile
profile: release
	bash bench/profile_flamegraph.sh all
```

Also add `profile` to `.PHONY` if present.

- [ ] **Step 5: Smoke one scenario (short)**

```bash
BENCH_DATA_MB=8 bash bench/profile_flamegraph.sh l1
ls -la bench/profiles/ | head
```

Expected: at least one new profile artifact; tunnel teardown clean (no orphan processes).

- [ ] **Step 6: Commit**

```bash
git add bench/profile_flamegraph.sh bench/PROFILE_RUNBOOK.md Makefile
git commit -m "feat(bench): add flamegraph profiling script and runbook"
```

---

### Task 4: Capture full L1–L4 matrix + HOTSPOTS.md

**Files:**
- Create: `bench/profiles/HOTSPOTS.md` (committed analysis text)

- [ ] **Step 1: Capture all scenarios**

```bash
make release
BENCH_DATA_MB=50 bash bench/profile_flamegraph.sh all
ls -la bench/profiles/
```

Expected: L1–L4 artifacts present (or clear log lines if L4 uses different naming).

- [ ] **Step 2: Analyze each profile**

For each scenario, open with Speedscope / `samply load` and record top frames (name + approximate %).

Write `bench/profiles/HOTSPOTS.md`:

```markdown
# Hotspot notes

- Date: YYYY-MM-DD
- Git: <short sha>
- Host: macOS arm64
- Tool: samply <version>
- DATA_MB: 50

## L1 null/nocomp
| Rank | Frame | ~% | Notes |
|------|-------|----|-------|
| 1 | ... | | |

## L2 aes/nocomp
...

## L3 3des/nocomp
...

## L4 stress
...

## Ranked optimization candidates
1. ... (maps to PERF R? / new)
2. ...
3. ...

## Non-actionable / leave alone
- ...
```

Fill with **real** data from the captures—no placeholders.

- [ ] **Step 3: Commit analysis only**

```bash
git add bench/profiles/HOTSPOTS.md
git commit -m "docs(bench): record L1–L4 flamegraph hotspot analysis"
```

---

### Task 5: Evidence-backed optimization (L1 priority)

**Files:** depend on `HOTSPOTS.md`. Prefer shared helpers over client/server drift.

**Hard rules:** surgical change only; wire-compatible; one optimization class; if no hotspot ≥~5% safely fixable, skip code change and document why in HOTSPOTS.md + skill (still complete Tasks 6–7).

- [ ] **Step 1: Select candidate**

From HOTSPOTS.md, pick top L1-related item (or L3 if L1 is flat and L3 clearly cipher-bound). Write the hypothesis in HOTSPOTS.md under `## Attempted fix`.

- [ ] **Step 2: Baseline measure (before)**

```bash
# Bulk for the affected crypt
# Prefer existing harness; example for aes bulk via run_bench defaults:
BENCH_DATA_MB=50 bash bench/run_bench.sh 2>&1 | tee /tmp/bench-before.txt
```

Or for null, temporarily document a one-off invocation matching profile flags (same ports pattern as script). Record MB/s.

- [ ] **Step 3: Implement minimal fix**

Examples of **allowed** directions only if profiles support them:

- Remove residual copy on hot path (Bytes ownership)
- Avoid unnecessary `cpu_block` for small batches (threshold already exists—only retune with evidence)
- Cipher micro-opt already patterned in `kcrypt-rs` (feistel/XOR style)—only if L3 shows inner loop dominance
- Shorten lock scope if mutex frames appear

**Forbidden without new design approval:** sendmmsg-only Linux rewrites as default, congestion cheats, protocol changes.

Show the actual patch in the worker’s commit; do not leave “similar to R5” without code.

- [ ] **Step 4: Tests**

```bash
cargo test --workspace
cargo clippy --workspace -- -D warnings
```

If crypto/protocol touched:

```bash
bash test_e2e.sh
```

If data-plane / concurrency:

```bash
make stress
```

Expected: all green.

- [ ] **Step 5: After measure + re-profile**

```bash
BENCH_DATA_MB=50 bash bench/run_bench.sh 2>&1 | tee /tmp/bench-after.txt
BENCH_DATA_MB=50 bash bench/profile_flamegraph.sh l1   # or affected scenario
```

Update HOTSPOTS.md with before/after numbers and whether hotspot % moved.

- [ ] **Step 6: Commit**

```bash
git add -A
git commit -m "perf: <one-line description of evidence-backed change>"
```

If no code change:

```bash
git add bench/profiles/HOTSPOTS.md
git commit -m "docs(bench): conclude no safe ≥5% hotspot for this pass"
```

---

### Task 6: Project skill `.claude/skills/flamegraph-perf`

**Files:**
- Create: `.claude/skills/flamegraph-perf/SKILL.md`

- [ ] **Step 1: Write SKILL.md**

Use real commands from Tasks 2–5 (not speculative). Structure:

```markdown
---
name: flamegraph-perf
description: Profile kcptun-rs with flamegraphs (samply), rank hotspots, evidence-gated optimize, verify. Use when user asks for flamegraph, 火焰图, performance bottleneck, or CPU profiling of kcptun client/server.
---

# Flamegraph-driven performance loop (kcptun-rs)

## When to use
- Finding CPU bottlenecks on client/server data plane
- Before speculative perf refactors
- After large perf PRs to confirm hotspot movement

## When not to use
- Pure correctness bugs (use systematic debugging)
- Protocol design without a load hypothesis

## Prerequisites
- macOS arm64 or Linux; primary sampler: samply (`cargo install samply`)
- `make release`
- Read: `bench/PROFILE_RUNBOOK.md`, `PERF_OPTIMIZATION_PLAN.md`

## Scenario matrix
| ID | Command | Purpose |
|----|---------|---------|
| L1 | `bash bench/profile_flamegraph.sh l1` | null/nocomp bulk |
| L2 | `bash bench/profile_flamegraph.sh l2` | aes bulk |
| L3 | `bash bench/profile_flamegraph.sh l3` | 3des bulk |
| L4 | `bash bench/profile_flamegraph.sh l4` | stress |
| All | `make profile` or `bash bench/profile_flamegraph.sh all` | full matrix |

## Closed loop
1. Capture profiles
2. Update/read `bench/profiles/HOTSPOTS.md`
3. One surgical fix if hotspot actionable
4. `cargo test --workspace` + clippy `-D warnings`
5. stress/e2e as needed
6. Re-bench + re-profile
7. CHANGELOG if measurable

## Symbol map
(include table from runbook)

## Decision tree
(include design tree)

## Gotchas
- `--pprof` is a placeholder HTTP banner, not real pprof
- `null` has no crypto header; `none` has header without encryption
- Compression default is ON; benches use `--nocomp` for raw path
- Keep client/server encrypt path symmetric via `kcp_rs::encrypt_batch`
- Wire compatibility > micro-opts

## Last run summary
- Date / git SHA / tool
- Top findings
- Change landed or reason skipped
```

Fill **Last run summary** from Task 5 results.

- [ ] **Step 2: Commit**

```bash
git add .claude/skills/flamegraph-perf/SKILL.md
git commit -m "feat(skills): add project flamegraph-perf skill"
```

---

### Task 7: README, CLAUDE, PERF plan, CHANGELOG

**Files:**
- Modify: `README.md`
- Modify: `README.zh.md`
- Modify: `CLAUDE.md`
- Modify: `PERF_OPTIMIZATION_PLAN.md`
- Modify: `CHANGELOG.md`
- Modify: `AGENTS.md` (one-line pointer under Key Files or Commands)

- [ ] **Step 1: README.md section**

Add after Features or near Benchmark mentions (find existing bench mention; place nearby):

```markdown
## Performance profiling

CPU flamegraphs for the data plane (macOS arm64: **samply** → Speedscope):

```bash
cargo install samply --locked
make release
bash bench/profile_flamegraph.sh all    # or: make profile
```

- Runbook: [`bench/PROFILE_RUNBOOK.md`](bench/PROFILE_RUNBOOK.md)
- Hotspot notes: [`bench/profiles/HOTSPOTS.md`](bench/profiles/HOTSPOTS.md)
- Agent skill: [`.claude/skills/flamegraph-perf/SKILL.md`](.claude/skills/flamegraph-perf/SKILL.md)

See also [`PERF_OPTIMIZATION_PLAN.md`](PERF_OPTIMIZATION_PLAN.md) for residual optimization items and KPI gates.
```

- [ ] **Step 2: README.zh.md**

Matching Chinese section:

```markdown
## 性能剖析（火焰图）

数据面 CPU 火焰图（macOS arm64 优先使用 **samply** → Speedscope）：

```bash
cargo install samply --locked
make release
bash bench/profile_flamegraph.sh all    # 或: make profile
```

- 操作手册：[`bench/PROFILE_RUNBOOK.md`](bench/PROFILE_RUNBOOK.md)
- 热点记录：[`bench/profiles/HOTSPOTS.md`](bench/profiles/HOTSPOTS.md)
- Agent skill：[`.claude/skills/flamegraph-perf/SKILL.md`](.claude/skills/flamegraph-perf/SKILL.md)

剩余优化项与 KPI 见 [`PERF_OPTIMIZATION_PLAN.md`](PERF_OPTIMIZATION_PLAN.md)。
```

- [ ] **Step 3: CLAUDE.md**

Under Important Gotchas or Commands, add:

```markdown
### Performance profiling (agents)

- Before speculative perf edits, run/read the project skill
  `.claude/skills/flamegraph-perf/SKILL.md` and `bench/PROFILE_RUNBOOK.md`.
- Prefer evidence from `bench/profiles/HOTSPOTS.md` + re-bench over guesswork.
- One optimization class per change; keep wire compatibility; use shared
  `encrypt_batch` paths to avoid client/server drift.
```

- [ ] **Step 4: PERF_OPTIMIZATION_PLAN.md**

Mark completed:

- `[x] --pprof 已有则固定 flamegraph 流程写入 bench/README` → note actual path is `PROFILE_RUNBOOK.md` + script
- `[x] flamegraph 一键脚本（可选）` → `bench/profile_flamegraph.sh` / `make profile`

Add short note under 剖析 table: samply primary on macOS.

- [ ] **Step 5: CHANGELOG.md Unreleased**

```markdown
### Added
- Flamegraph profiling script `bench/profile_flamegraph.sh`, runbook, and project skill `flamegraph-perf`.

### Changed
- (if Task 5 landed code) <perf bullet with measured delta>
```

- [ ] **Step 6: AGENTS.md one-liner**

In Key Files table add:

```markdown
| `bench/profile_flamegraph.sh` | samply/flamegraph capture for L1–L4 loads |
| `.claude/skills/flamegraph-perf/` | Agent skill: profile → optimize → verify |
```

- [ ] **Step 7: Commit**

```bash
git add README.md README.zh.md CLAUDE.md PERF_OPTIMIZATION_PLAN.md CHANGELOG.md AGENTS.md
git commit -m "docs: wire flamegraph skill and profiling entry points"
```

---

### Task 8: Final verification gate

**Files:** none

- [ ] **Step 1: Script still works**

```bash
bash -n bench/profile_flamegraph.sh
BENCH_DATA_MB=4 bash bench/profile_flamegraph.sh l1
```

Expected: exit 0; artifact created.

- [ ] **Step 2: Workspace health**

```bash
cargo test --workspace
cargo clippy --workspace -- -D warnings
```

Expected: pass / zero warnings.

- [ ] **Step 3: Doc presence checklist**

```bash
test -f .claude/skills/flamegraph-perf/SKILL.md
test -f bench/PROFILE_RUNBOOK.md
test -f bench/profiles/HOTSPOTS.md
rg -n "profile_flamegraph|flamegraph-perf" README.md CLAUDE.md PERF_OPTIMIZATION_PLAN.md
```

Expected: all files exist; ripgrep hits in all three docs.

- [ ] **Step 4: Final commit only if fixes needed**; otherwise done.

---

## Spec coverage self-check

| Spec requirement | Task |
|------------------|------|
| L1–L4 capture | 3, 4 |
| Hotspot write-up | 4 |
| ≥1 evidence-backed opt or honest skip | 5 |
| Project skill | 6 |
| README / CLAUDE / PERF | 7 |
| samply primary, macOS | 2, 3 |
| gitignore large profiles | 1 |
| Verify loop | 5, 8 |
| No io_uring/DPDK / no congestion cheat | 5 hard rules |

## Placeholder scan

No TBD steps; Task 5 code is intentionally data-dependent but requires real patch or explicit no-op commit with written reason.

## Execution handoff

Plan complete and saved to `docs/superpowers/plans/2026-07-21-flamegraph-perf.md`.

**Two execution options:**

1. **Subagent-Driven (recommended)** — fresh subagent per task, review between tasks  
2. **Inline Execution** — this session executes tasks with checkpoints  

Which approach?
