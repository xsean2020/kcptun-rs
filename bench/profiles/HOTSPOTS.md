# Hotspot notes

- Date: 2026-07-21
- Git (worktree at capture): `9aa4b3a` (+ uncommitted tooling then committed as feat(bench) profile script)
- Host: macOS arm64 (Darwin 25.3)
- Tool: samply 0.13.1
- Binaries: `CARGO_PROFILE_RELEASE_{STRIP=false,DEBUG=true,LTO=false}` (required for usable names)
- Workload: `bench/kcptun_prof_wl` (native root; Python echo; concurrent bulk)

## Capture artifacts (regenerate locally; gitignored)

| Scenario | Example artifact | Measured thr (this host) |
|----------|------------------|---------------------------|
| L1 null/nocomp bulk 30MB | `L1-null-nocomp-*-103600.json.gz` | ~77 MB/s |
| L2 aes/nocomp bulk 20MB | `L2-aes-nocomp-*-103602.json.gz` | ~12.4 MB/s |
| L3 3des/nocomp bulk 10MB | `L3-3des-nocomp-*-103606.json.gz` | ~13.8 MB/s |
| L4 stress 10 conn | `L4-stress-*-102923.json.gz` | test OK |

Open: `samply load bench/profiles/<file>.json.gz`

## L1 null/nocomp

| Rank | Frame (resolved) | ~self % | Notes |
|------|------------------|---------|-------|
| 1 | `kcptun_server::async_main::{closure}` (multiple PCs) | ~65% + fragments | Tokio main/session loop collapsed under one async mangled root |
| 2 | iterator / runtime glue | <1% | not dominant |
| 3 | mimalloc `mi_page_free_list_extend` | ≪1% | allocator not hot on null path |

**Interpretation:** With null/nocomp, CPU is spread across the async data plane (KCP/SMUX/UDP) rather than a single cipher. No single ≥5% leaf like `encrypt_batch` or `memcpy` stood out after symbolication; flame is “flat async main”.

## L2 aes/nocomp

| Rank | Frame | ~self % | Notes |
|------|-------|---------|-------|
| 1 | `async_main` closure PCs | ~60–65% | same async aggregation |
| 2 | `aes::soft::fixslice::sub_bytes` | ~0.1%+ related AES soft frames | **AES-NI soft path present** |
| 3 | `aes::soft::fixslice::aes256_encrypt` | small | soft AES |
| 4 | `kcrypt_rs::crypt::CryptEngine::select` | small | dispatch overhead visible |

**Throughput evidence:** L2 ~12 MB/s vs L1 ~77 MB/s on same harness → **encryption/CFB path dominates wall-clock** even when leaf % is under-counted due to inlining into `async_main`.

## L3 3des/nocomp

| Rank | Frame | ~self % | Notes |
|------|-------|---------|-------|
| 1 | `async_main` closure PCs | ~65% | aggregation |
| 2 | `kcrypt_rs::des::TripleDesCipher::encrypt_block` | ~0.2–0.4% self (multiple PCs) | **3DES block encrypt** |
| 3 | `kcp_rs::segment::SegmentPool::release` | ~0.3–0.8% across PCs | segment pool churn |

**Throughput:** ~12–14 MB/s bulk — cipher-bound class, consistent with PERF residual heavy-cipher work.

## L4 stress (10 connections)

| Rank | Frame | Notes |
|------|-------|-------|
| 1 | `async_main` / test harness | Short test; high idle/wait fraction (`0x44f8`-class) |
| 2 | No clear lock convoying leaf | Not enough signal for mutex redesign this pass |

## Ranked optimization candidates

1. **AES soft vs hardware:** L2 shows `aes::soft` frames — ensure AES-128/256 CFB uses AES-NI / hardware backend when available (or document why soft). Highest expected win for L2 wall-clock.
2. **3DES `encrypt_block`:** already partially optimized (feistel box history); further micro-opts only if still &lt;0.9× Go on bench matrix.
3. **`SegmentPool::release` on L3:** secondary; investigate retain/reuse under heavy encrypt batching.
4. **Null path:** no ≥5% surgical leaf; leave alone unless new profiles with better async frame recovery show copies/locks.

## Non-actionable / leave alone

- Guessing inside undifferentiated `async_main` without better DWARF/async symbols.
- Congestion-window cheats.
- io_uring / DPDK (out of scope).
- Default release `strip=true` + LTO profiles for analysis (use profiling rebuild flags).

## Attempted fix

### Enable RustCrypto `aes_armv8` on aarch64 (landed)

**Evidence:** L2 profiles showed `aes::soft::fixslice::*`; `nm` confirmed soft AES linked despite Apple Silicon FEAT_AES.

**Change:** `.cargo/config.toml` sets `--cfg aes_armv8` for `aarch64-apple-darwin` and `aarch64-unknown-linux-gnu` (RustCrypto `aes` 0.8 requires this cfg; x86 uses AES-NI by default).

**Before (soft, this host, `kcptun_prof_wl` bulk 20MB aes):** ~12–14 MB/s  
**After (armv8):** ~66–85 MB/s aes (~**5–6×**), null still ~120+ MB/s.

**nm after:** `aes::armv8::Aes*Enc` present; soft fixslice no longer required on hot path.

Wire format unchanged (same CFB + fixed IV). Verified with `cargo test -p kcrypt-rs`.

## Tooling lessons (skill)

- Root process must be **locally built** on macOS (not `/bin/bash`).
- Use `bench/kcptun_prof_wl` + `profile_flamegraph.sh`.
- Rebuild with `STRIP=false DEBUG=true LTO=false` for naming; post-process offsets with `nm` + `rustfilt` if Speedscope shows `0x…`.
