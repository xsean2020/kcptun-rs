---
name: agents-md-orient
description: Use when analyzing or comparing this project, starting unfamiliar work, mapping architecture, or after structural/public-API changes that may stale AGENTS.md. Triggers: 分析项目, 对比本项目, architecture overview, crate layout, finishing PRs that move modules or Key Files.
---

# AGENTS.md-first orientation & sync (kcptun-rs)

## Overview

Hierarchical `AGENTS.md` is the project map. **Orient from it before scanning source.** After structural changes, **diff reality against the nearest AGENTS.md and update when stale.** Full-repo remaps waste context and drift agent understanding.

`CLAUDE.md` = how to work; `AGENTS.md` = what lives where.

## When to use / not

**Use:** analyze/compare/explain project; unfamiliar area; urge to `find`/`Glob **/*`; after moves, new crates/modules, public API, command renames, Key Files layout.

**Skip:** one-line fix with no layout/public surface change; single file user already named; AGENTS churn without real structural change.

## Iron rules

1. Root `AGENTS.md` → nearest crate/dir `AGENTS.md` → only then open source you will change or cite.
2. No full-repo scan by default. Do not invent architecture when AGENTS already documents it.
3. Before done: compare structural change set to nearest AGENTS (and root if workspace-level).
4. Preserve `<!-- MANUAL: -->` blocks.
5. Follow `<!-- Parent: ../AGENTS.md -->` upward — not sideways full scans.

## Orientation

```text
Need understanding? → root AGENTS → crate/nested AGENTS → Key Files only → stop
```

For 分析项目 / 对比本项目 / architecture: quote stack, crates, wire formats from AGENTS. Open source only for a *specific* gap. If AGENTS ≠ code, **code wins**, then **fix AGENTS same session**.

## Sync after changes

| Change class | Sync target |
|--------------|-------------|
| New/removed crate or top-level dir | Root Subdirectories / Key Files |
| New/moved `pub` module, factory, wire format | Nearest crate AGENTS Key Files + agent notes |
| Nested layout (`src/crypt/…`, kio net/sync/…) | Nested `…/AGENTS.md` |
| Commands / Makefile / e2e entrypoints | Root Commands (+ crate Testing if local) |
| Bug paths under `bugs/` | Root Key Files; never `BUGREPORT*.md` at repo root |
| Pure bugfix / perf line, no layout | **No** AGENTS churn |

1. List created/renamed/deleted/exported paths.  
2. Open nearest AGENTS (walk parents).  
3. Would AGENTS-only agents be wrong about ownership, Key Files, stack, or commands?  
4. If yes → surgical edit; bump `<!-- Updated: YYYY-MM-DD -->`.  
5. If no → state "no AGENTS sync needed" with one-line reason.

## Red flags — STOP

- Workspace Glob/find before any AGENTS.md  
- "Docs stale; trust tree/Cargo first" as *first* move  
- Architecture from memory without root AGENTS this session  
- Module move / new `pub mod` without crate AGENTS check  
- Full AGENTS rewrite for a one-line fix  
- New bug write-ups at repo root  

→ Return to AGENTS.md-first, then continue.

## Rationalizations

| Excuse | Reality |
|--------|---------|
| "Cargo.toml is truth" | Deps yes; orientation → AGENTS first, verify gaps only. |
| "AGENTS might be stale" | Orient *and* fix on drift — don't skip. |
| "Broad scan = thorough" | Thorough = correct AGENTS + files you touch. |
| "Analysis doesn't need docs" | Analysis is what AGENTS is for. |
| "Docs-only move → skip AGENTS" | Check Key File paths; update only if wrong. |
| "Tiny public API" | Still belongs in crate Key Files. |
| "CLAUDE.md already says this" | Skill is the operational checklist under pressure. |

## Success criteria

- [ ] Analyze/compare: root AGENTS before workspace-wide scan  
- [ ] Crate work: crate AGENTS before bulk `src/**`  
- [ ] Structural/public-API work: nearest AGENTS updated **or** explicit no-sync reason  
- [ ] No architecture invention when AGENTS answers  
- [ ] MANUAL sections preserved  

## Related

- `CLAUDE.md` § AGENTS.md — AI orientation  
- `.claude/skills/flamegraph-perf/`  
- Bug reports: `bugs/` only  
