---
name: superpowers-sync
description: When finishing a feature/fix, write (or update) the plan to docs/superpowers/plans/ and the implementation record to docs/superpowers/specs/. Use when closing a task, making a commit after a fix, or when the user asks to "同步" or "sync" docs or commits. No — do not skip for small tasks; the doc file is lightweight.
---

# superpowers-sync — sync plans & specs to `docs/superpowers/`

## When to use

- After implementing a plan (put plan into `plans/` **before** coding, or after)
- When closing/finishing a task — write implementation record to `specs/`
- When the user explicitly asks "同步方案" / "sync plan" / "写文档"
- After a bug-fix that has a `BUGREPORT*.md` under `bugs/` — spec should reference it

## Output file structure

```
docs/superpowers/
├── plans/
│   └── YYYY-MM-DD-<short-kebab>.md
└── specs/
    └── YYYY-MM-DD-<short-kebab>.md
```

### Plan file (`plans/`)

Frontmatter table (YAML-like, Git-flavored table):

```markdown
# Plan: <title>

> **Canonical path (git):** `docs/superpowers/plans/<filename>`

| Field | Value |
|-------|-------|
| Status | draft **|** implemented **|** superseded |
| Created | YYYY-MM-DD |
| Scope | What this plan covers / doesn't cover |
| Out of scope | Explicit non-goals |
| Related | References to AGENTS.md / bugs/ / source files |
```

Field notes:
- **Status**: `draft` = not yet coded; `implemented` = coded; `superseded` = replaced by later plan
- **Scope**: be brief but specific
- **Related**: file paths, commit references, BUGREPORT links

Body sections (flexible — use judgment):
1. **Problem** — what's broken / what's the goal
2. **Root cause** analysis (for bug-fixes)
3. **方案（Solution）** — the approach, alternatives considered
4. **实施顺序（Implementation order）** — dependency-aware steps
5. **验收（Acceptance）** — how to verify done

### Spec file (`specs/`)

Frontmatter same style:

```markdown
# Spec: <title> — implementation record

> **Canonical path (git):** `docs/superpowers/specs/<filename>`

| Field | Value |
|-------|-------|
| Implemented | YYYY-MM-DD |
| All commits | <commit range or "single session"> |
| Bug report | `bugs/BUGREPORT_*.md` (if applicable) |
```

Body:
1. **改动的文件列表（Files changed）** — table per file with what changed and why
2. **修复的故障路径（Fixed failure paths）** — enumerated list
3. **测试结果（Test results）** — `cargo test`, `cargo clippy`, `cargo fmt`
4. **修订记录（Revision history）** — table

## Existing doc precedents

For reference format consistency, see:

- `docs/superpowers/plans/2026-07-22-smux-stream-leak-fix.md` — bug-fix plan with root cause + steps
- `docs/superpowers/specs/2026-07-22-smux-stream-leak-fix.md` — implementation record (files, paths, tests)
- `docs/superpowers/plans/inbound-cfb-null-less-copy.md` — perf plan with scope/litmus/related

## When NOT to sync

- Pure refactor with no behavioral or structural change (rename local variable, comment fix)
- Build system / CI config changes
- The plan already existed in `bugs/` (but still write a spec referencing it)
- Project maintainer says "skip" — then state "skipped superpowers-sync" and why
