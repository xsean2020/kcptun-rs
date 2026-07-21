# Design: Flamegraph-Driven Performance Loop + Project Skill

**Date:** 2026-07-21  
**Status:** Approved for implementation planning  
**Repo:** kcptun-rs  
**Hard constraints:** Go kcptun / kcp-go v5 wire compatibility; `cargo clippy -D warnings`; e2e + stress green; no congestion-window “cheats”.

---

## 1. Problem

The project already has:

- A mature performance plan (`PERF_OPTIMIZATION_PLAN.md`) with P0/P1 done and residual R-items
- Throughput benches (`make bench`, `bench/run_bench.sh`, `bench_results.json`)
- Placeholder `--pprof` endpoints that do **not** produce real profiles

Missing pieces called out in the PERF plan:

- Fixed flamegraph workflow (`bench/README` / one-shot script)
- Evidence-backed hotspot ranking before further micro-optimizations
- A reusable, project-local skill so the full loop (profile → fix → verify → document) is repeatable

Host platform for this work: **macOS arm64** (no Linux `perf` by default). Tooling must be chosen accordingly.

---

## 2. Goals & Success Criteria

| Goal | Success criterion |
|------|-------------------|
| Multi-scenario profiles | Capture **L1–L4** (see matrix) with release binaries; store command + git SHA + artifact path notes |
| Hotspot interpretation | Written ranking of top frames for each scenario (project symbols: `encrypt_batch`, `KCP::flush`, SMUX drain, snappy, `send_batch`, cipher inner loops) |
| Optimization | **≥1** surgical code change **supported by flamegraph evidence**, with before/after bench or CPU-share comparison |
| Correctness | No wire break; relevant unit tests + stress (and e2e if protocol/crypto touched) stay green |
| Skill | Project skill under `.claude/skills/flamegraph-perf/` that another agent/session can follow end-to-end |
| Docs | README (+ ZH if needed), `CLAUDE.md`, and PERF checklist updated with profiling entry points |
| Scope honesty | If no hotspot ≥ ~5% is safely actionable, ship tooling + skill + “why we did not change X” notes instead of speculative rewrites |

### Non-goals

- io_uring / GSO / DPDK / default congestion changes
- Making `--pprof` a full Go-compatible pprof server (optional later; not required)
- Large refactors unrelated to measured hotspots
- Guaranteeing further multi-× speedups (bulk already ~1.2–1.43× Go on some paths)

---

## 3. Approach (selected)

**macOS-native sampling + scripted runbook + evidence-gated optimization + project skill.**

Rationale:

1. Aligns with open PERF checklist items (flamegraph script / fixed flow).
2. macOS arm64 needs an explicit tool path (`samply` first; dtrace/cargo-flamegraph fallback).
3. Full scenario coverage requires automation for reproducibility.
4. Skill should encode **this repo’s** ports, flags, ciphers, and gotchas—not generic profiling theory only.

Rejected alternatives:

- **Manual one-off cargo-flamegraph only** — fragile on macOS, weak skill value.
- **Docs-only without real profiles** — conflicts with closed-loop delivery.

---

## 4. Profiling scenarios (full coverage)

| ID | Load | Typical flags (illustrative) | Purpose |
|----|------|------------------------------|---------|
| L1 | Bulk, null, nocomp | `--crypt null --nocomp`, large `BENCH_DATA_MB` | Data plane, copies, scheduling, UDP batch |
| L2 | Bulk, AES | `--crypt aes` (or aes-128), bulk | Encrypt batch + CFB path |
| L3 | Bulk, heavy cipher | `--crypt 3des` and/or `tea` | Whether cipher still dominates vs Go |
| L4 | Multi-connection stress | existing stress harness | Locks, session map, scheduler fairness |

Fixed environment per capture:

- `cargo build --release` (tokio default unless smol matrix is explicitly added later)
- Fixed key, mode (e.g. `fast` / plan-default), MTU/windows as in bench scripts
- Fixed local ports; loopback client↔server
- Record: date, git SHA, command lines, tool version, output paths

**Optimization priority when only one code-change wave is affordable:**  
**L1 data plane first**, then L3 if profiles show heavy-cipher dominance that maps to a safe R-item. All L1–L4 are still **captured**.

---

## 5. Tooling

| Role | Primary | Fallback |
|------|---------|----------|
| CPU flame / stack sampling | **samply** → Speedscope JSON | `cargo-flamegraph` + dtrace if samply unavailable |
| Throughput | `bench/run_bench.sh`, `bench_rust_vs_go.py` | — |
| Stress | `make stress` / package stress tests | — |
| Alloc (optional) | Instruments Allocations / dhat if null path suspects alloc | Only if L1 points at allocator frames |

Install notes live in the skill and `bench` runbook; prefer project-local scripts over ad-hoc shell history.

---

## 6. Artifacts & layout

```text
bench/
  profile_flamegraph.sh     # start tunnel + workload + samply (or fallback)
  PROFILE_RUNBOOK.md        # human-readable steps; skill may mirror/link
  profiles/                 # large artifacts; gitignore contents except .gitkeep + README
  profiles/README.md        # how to regenerate; what each L* file means
.claude/skills/flamegraph-perf/
  SKILL.md                  # project skill: triggers, steps, matrix, decision tree, verify
docs/superpowers/specs/
  2026-07-21-flamegraph-perf-design.md   # this document
```

Also update:

- `README.md` — short “Performance profiling” section → script + skill
- `README.zh.md` — matching section if EN section is user-facing
- `CLAUDE.md` — agent-facing pointer: orient via skill/runbook before guessing hotspots
- `PERF_OPTIMIZATION_PLAN.md` — check flamegraph script / fixed flow items when done
- `.gitignore` — ignore `bench/profiles/*` binaries/json/svg as appropriate; keep docs

Optional: small sample screenshot or textual top-N dump in runbook (not mandatory large binaries in git).

---

## 7. Closed-loop workflow

```text
1. Install/verify tools (samply, release toolchain)
2. Release build
3. Capture L1–L4 profiles via script
4. Rank hotspots (symbol + % + stack context)
5. Map top items to PERF residual IDs (R3–R7, cipher opts, etc.) or new findings
6. Implement ONE class of change (surgical)
7. Re-profile affected scenarios + bench/stress
8. Accept or revert
9. Write skill + README/CLAUDE/PERF updates reflecting real commands and outcomes
```

### Optimization discipline

- Evidence first (profile % or clear bench delta)
- One optimization class per iteration
- Surgical diffs only (CLAUDE.md rules)
- Wire compatibility preserved
- `cargo clippy --workspace -- -D warnings` clean for touched crates
- Prefer shared paths (`encrypt_batch`, kio batch APIs) over client/server drift

### Decision tree (high level)

1. **Cipher inner loop dominates (L2/L3)** → monomorphization / algorithm micro-opts already in plan; verify not dyn/dispatch leftover
2. **Copy / Bytes / Vec churn (L1)** → ownership pipeline, avoid `to_vec`, null move paths
3. **Lock / mutex (L1/L4)** → shorten critical sections; never hold KCP lock across encrypt/snappy
4. **Syscall / send (L1)** → batch send paths; platform sendmmsg only if Linux and justified
5. **Scheduler / cpu_block (L1/L2)** → thresholds already exist; only retune with evidence
6. **No actionable hotspot** → stop coding; complete skill/docs

---

## 8. Skill design (`.claude/skills/flamegraph-perf/SKILL.md`)

Frontmatter + body must include:

1. **Name / description / triggers** (e.g. “flamegraph”, “性能瓶颈”, “profile kcptun”)
2. **When to use / when not to use**
3. **Prerequisites** (macOS vs Linux notes, samply, release build)
4. **Scenario matrix L1–L4** with exact flags once scripts stabilize
5. **Capture commands** (script first, manual fallback)
6. **How to read results** for this stack’s symbols
7. **Optimization decision tree** linked to `PERF_OPTIMIZATION_PLAN.md`
8. **Verify loop** (`make test`, stress, bench, e2e when needed)
9. **Doc update checklist**
10. **Gotchas** (pprof placeholder, null vs none headers, nocomp default, dual runtime, wire rules)

Skill records **the real process executed in this effort**, not a fictional ideal.

---

## 9. Documentation touch points

| File | Change |
|------|--------|
| `README.md` | Profiling section: install, `bench/profile_flamegraph.sh`, link skill |
| `README.zh.md` | Same in Chinese |
| `CLAUDE.md` | Agent rule: use flamegraph skill/runbook before speculative perf edits |
| `PERF_OPTIMIZATION_PLAN.md` | Mark flamegraph script / fixed flow; note outcomes |
| `bench/PROFILE_RUNBOOK.md` | Full human procedure |
| `CHANGELOG.md` | If code or measurable bench changes land |

---

## 10. Verification plan

| Stage | Command / check |
|-------|-----------------|
| Build | `cargo build --release` |
| Unit | `cargo test --workspace` (or targeted crates after small changes) |
| Lint | `cargo clippy --workspace -- -D warnings` |
| Stress | `make stress` when data-plane or concurrency touched |
| E2E | `bash test_e2e.sh` if crypto/protocol/wire path changed |
| Bench | Before/after bulk for affected cipher configs |
| Profile | Re-capture hottest scenario; hotspot % should move as expected or explain why not |

---

## 11. Risks & mitigations

| Risk | Mitigation |
|------|------------|
| samply install fails | Document fallback; still ship script structure + runbook |
| Loopback noise / turbo variance | Fixed data size, repeat runs, report ranges not single points |
| Client/server drift after fix | Prefer shared helpers; dual-check both binaries in profile if needed |
| Over-optimizing already-won null path | Require evidence; prefer L3 or L4 if L1 is flat |
| Large profile files in git | gitignore; keep regenerate instructions |

---

## 12. Implementation phases (for planning skill)

1. **Tooling & script** — install path, `profile_flamegraph.sh`, profiles dir + gitignore  
2. **Baseline capture** — L1–L4 + written hotspot notes  
3. **Optimize** — one evidence-backed change wave (L1 priority)  
4. **Re-verify** — tests/bench/profile  
5. **Skill + docs** — `.claude/skills/flamegraph-perf`, README, CLAUDE, PERF, CHANGELOG as needed  

Phase order is strict for capture/optimize; skill writing may draft skeleton early but **must be updated with real results** at the end.

---

## 13. Open defaults (resolved)

| Question | Resolution |
|----------|------------|
| Delivery scope | Full closed loop |
| Scenarios | All of L1–L4 |
| Skill location | Project-local `.claude/skills/flamegraph-perf/` |
| Code-change priority | L1 data plane first |
| Host | macOS arm64; samply primary |

---

## 14. Spec self-review

- No TBD placeholders left for required decisions  
- Consistent with PERF plan residual “flamegraph script / fixed flow”  
- Scope is one implementation plan (tooling + profile + limited optimize + skill/docs)  
- “Full coverage” means **profile** all scenarios; **code change** prioritizes L1 unless profiles redirect  

---

## Approval

Design approved by user (2026-07-21) for writing the implementation plan next.
