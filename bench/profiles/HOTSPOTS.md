# Hotspot notes

## 3des+comp investigation (2026-07-21, git post-snappy-offload)

### Algorithm vs Go
Rust `TripleDesCipher` already ports Go `crypto/des`: precomputed Feistel boxes,
single IP/FP for 48 EDE rounds (better than RustCrypto's 3× IP/FP). CFB-8 matches
kcp-go fixed IV + 64-bit XOR.

### Regression found
Snappy unconditional `cpu_block` at ≥4KiB **stacked** with 3des `encrypt_batch`
`cpu_block` → smol thr collapsed (~4 MB/s). Fix: offload Snappy only for null/none.

### Post-fix median thr (2MB×4)
| impl | +comp | nocomp |
|------|------:|-------:|
| Go | ~9.0 | ~10.7 |
| Rust-tokio | ~15.2 | ~12.2 |
| Rust-smol | ~10.7 | ~15.4 |



## L3 re-capture (post 196408e) — 2026-07-21 15:46

- Git: `196408e` (smol true-idle + MPMC cpu_block + allow_parallel)
- Host: macOS arm64, samply 0.13.1
- Build: `cargo build --profile profiling` (debug=2, strip=false, lto=false, frame pointers, `aes_armv8`)
- Workload: `bash bench/profile_flamegraph.sh l3` with `BENCH_DATA_MB=20`
- Artifact: `bench/profiles/L3-3des-nocomp-20260721-154613.named.json.gz`
- Measured thr (profiling bins, not release LTO): **~7.48 MB/s** (expect lower than release)

### Leaf ranking (Gecko samples, all threads, ~15.9k samples)

| Rank | Frame | ~leaf % | Notes |
|------|-------|---------|-------|
| 1 | `tokio::…Harness::complete` | ~33% | runtime bookkeeping / task complete |
| 2 | `KcpServerSession::feed_data` | ~27% | inbound decrypt + FEC + KCP input (inlined body) |
| 3 | clap `Parser::parse` | ~13% | startup noise in short capture |
| 4 | `slice::sort::…small_sort` | ~12% | mostly **client** process; not 3DES core |
| 5 | **`TripleDesCipher::encrypt_block`** | **~11%** | **real 3DES Feistel work (server)** |
| — | `encrypt_batch` (any-in-stack) | ~8% | outbound batch encrypt |
| — | CFB / `des::` (any-in-stack) | ~11% | CFB-8 + DES path |

Keyword any-in-stack: `TripleDes`/`encrypt_block` ~11%; `encrypt_batch` ~8%; snappy ≈0% (`--nocomp`).

### Interpretation

1. **Cipher-bound, as expected for L3.** `encrypt_block` is the only large *named* crypto leaf; Feistel already uses Go-style precomputed boxes + single IP/FP for 48 rounds (faster than RustCrypto DES).
2. **Release matrix already ≥ Go on 3des nocomp** (post-bench: smol ~14.3 vs Go ~13.1 MB/s ≈ **1.09×**). Skill gate: further 3DES micro-opts only if thr **&lt;0.9× Go** — **not met**.
3. **`feed_data` high leaf %** is mostly **inbound CFB decrypt** collapsed into one mangled frame (profile attributes both client/server samples oddly for that symbol — treat as “data-plane decrypt/input”, not a separate API tax).
4. **Remaining product gap is not L3-nocomp cipher**: short-connection matrix **3des+comp** still weak on smol vs tokio (~0.81×). That points at **compression + scheduling**, not more Feistel unrolling. Next evidence: L3-with-comp or dedicated 3des/comp profile — **not** more IP/FP tricks.

### Decision (this pass)

- **No 3DES algorithm change.** Wire-risk high, KPI already above Go on nocomp.
- Prefer next: snappy conditional `cpu_block` / smol pool under **comp=on**, or profile `3des+comp` specifically.
- Optional hygiene: ignore clap/startup frames; re-run longer capture if ranking startup noise.

---


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
