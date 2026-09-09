# Hotspot notes

## ✅ 2026-08-27: kcp-rs partial batch-send completion under 64 KiB concurrency

Workload: release `multi_conn_latency`, 32 connections × 4 closed-loop requests,
64 KiB payload, 2s warmup + 10s measurement on macOS loopback. Exact command:

```bash
/usr/bin/time -l ./multi_conn_latency --connections 32 --concurrency 4 \
  --size 65536 --warmup 2 --duration 10 --rt multi
```

Root cause: both synchronous and async KCP TX paths treated any
`try_send_batch` result greater than zero as if the whole batch had been sent.
When the socket accepted only a prefix, the unsent suffix was dropped and KCP
had to recover it via retransmission/RTO. The fix sends only the remaining
suffix while retaining the single-sender token; FEC retries retain the original
expanded wire batch so sequence numbers and FIFO order do not change. Sharded
route drops now also return their receive buffers to the pool.

Three-run release results (median unless noted):

| Metric | Before | After | Change |
|--------|--------|-------|--------|
| Actual throughput | 335.3 req/s (mean 325.6) | **631.8 req/s** (mean 599.9) | **+88.4% median; +84.2% mean** |
| P50 | 231.4 ms | **140.5 ms** | **-39.3%** |
| P90 | 852.4 ms | **400.1 ms** | **-53.1%** |
| P99 | 2324.7 ms | **891.7 ms** | **-61.6%** |
| P999 | 3934.0 ms | **2583.2 ms** | **-34.3%** |
| Aggregate CPU/request | 2.226 ms | **2.089 ms** | **-6.2%** |
| Max RSS | 55.9 MiB | 61.9 MiB | +10.7% |

All six samples were valid with zero shed, incomplete requests, or task
failures. Higher RSS reflects nearly twice as much completed traffic and more
in-flight payload; this workload reported no worker-channel drops, so it does
not exercise the overload-only buffer-recycle path.

## ✅ 2026-08-14: goroutine raw-KCP closed-loop UDP full-duplex fix

Workload: `latency_p99`, 8 request workers, concurrency 32, 26,624-byte
payload, 2s warmup + 10s measurement on macOS loopback.

Before the change, `sample` attributed 1,191 of the input loop's 2,529 samples
to `cthread_yield`/`swtch_pri`: goroutine UDP send and receive shared the same
`SpinMutex<mio::UdpSocket>`. The goroutine kio constructor also bypassed the
shared socket2 setup, so it used the OS-default buffers and only emulated UDP
`connect` in user space. SNMP showed zero retransmits, ruling out KCP loss/ACK
inflation as the throughput gap.

Changes:

- duplicate the UDP kernel handle for sends, allowing send and receive to run
  concurrently while the mio handle remains reactor-owned;
- construct goroutine kio UDP sockets through the same 4 MB-buffered,
  SO_REUSEADDR, kernel-connected `raw_udp` path as tokio and smol.

Release closed-loop results:

| Metric | Before (2 runs) | After (3 runs) | Change |
|--------|-----------------|----------------|--------|
| Throughput | mean 2,477 req/s (2,464–2,530) | mean **3,879 req/s** (3,777–3,948) | **+56.6%** |
| P99 | 18.11 ms representative | 12.12–12.71 ms | about **-32%** |

Relative to the original common-matrix goroutine result (2,424 req/s), the
new three-run mean is +60.0%. The subsequent full 14-stage matrix reproduced
the gain at 3,843 req/s (+58.5% versus 2,424; +21.7% versus Go). Fixed-rate
500 req/s P99 remained stable (median 586 us across three focused runs; 618 us
in the full matrix). A post-change profile no longer contains the socket-lock
yield hotspot; dominant frames are `sendto`, `recvfrom`, `kevent`, and semaphore
wait. No actionable scheduler/runtime leaf remains at >=5%.

## ✅ 2026-08-14: goroutine Go-style lazy M lifecycle

`GORUNTIME_WORKER_THREADS=4 ... profile_rust_go_pprof.sh client 10`
(`rust-client-aes-20260814-060704.pb`, 600MB AES/nocomp):

| Frame | Flat | Interpretation |
|-------|------|----------------|
| UDP `recv_from` | 21.92% | macOS UDP receive syscall |
| goroutine UDP `send_to` | 20.88% | per-datagram send path |
| reactor `poll_once` | 11.76% | per-P mio polling |
| UDP socket `SpinMutex::lock` | 7.62% | shared socket serialization |
| reactor wake | 7.15% | owner-P I/O wakeup |
| `WorkSignal::notify_one` | 0.77% | M park/unpark signal |

`wakep`/idle-P selection/worker creation do not appear as actionable leaves.
The fixed-P, lazy-M lifecycle therefore adds no ≥5% scheduler hotspot; the
remaining actionable scheduler-adjacent costs are existing per-packet reactor
wake and UDP socket locking. Throughput during capture was 57.66 MB/s.

## ✅ 2026-08-05: kcp-rs 尾延迟优化后重新验证（当前 master，commits ae765aa7…4287a9bf）

`CRYPT=null bash bench/profile_rust_go_pprof.sh server 20`（`rust-server-null-20260805-233001.pb`）：

| 项 | 基线 (073115.pb) | 当前 (233001.pb) | 说明 |
|----|------------------|------------------|------|
| `UdpSocket::send_to` | 39.0% | **43.3%** | macOS UDP send syscall（无 sendmmsg，逐包） |
| `Socket::recv_from` | 26.6% | **28.4%** | UDP recv syscall |
| tokio io `Driver::turn` | 9.5% | 9.9% | tokio reactor |
| mimalloc（分配） | ~10% | **~3%** | raw 双缓冲 + drop 路径回收生效 |
| `Timespec::now` | 2.25% | **1.73%** | `current_ms` OnceLock 缓存生效 |
| kcp-rs `spawn_input_loop` | 7.6% cum | 不在 top | 批量接收/锁优化出热点 |
| listener 路由开销 | 逐包 | **~0.2% cum** | 一次 sessions 锁/burst + 零分配路由 |

**结论**：kcp-rs 内部工作已降到接近零（listener 路由 cum ~0.2%，input loop 出 top）。
服务器 72% CPU 是 macOS UDP syscall（send_to 43% + recv_from 28%），无 sendmmsg 不可削减；
其余为 tokio reactor（~17%）。达到 tail-latency 计划 §17 停止条件：无可行动 ≥5% kcp-rs leaf。

**剩余杠杆（证据门控 / Linux 限定）**：
- Linux `sendmmsg`/`recvmmsg`（kio 已实现，待 Linux 容器验证）— 唯一能砍 syscall 数的改动
- tokio `current-thread` runtime（`--event-loop current-thread`）— 降跨线程唤醒 P99（部署选择，已支持）
- Phase 7 SO_REUSEPORT shard ownership — 需 Linux 实测

## 🔄 2026-08-04 重新验证（commit 25b6087f 后，重建二进制，`--quick --conn 4 --size 1M --runs 3` 3-way）

| Config | Tokio | Smol | Go | T/S | T/Go |
|--------|-------|------|-----|-----|------|
| null/nocomp | 31.5 | 43.3 | 34.6 | 0.73× | 0.91× |
| null/comp | 30.8 | 45.0 | 35.0 | 0.68× | 0.88× |
| aes-128/nocomp | 40.4 | 40.5 | 33.8 | 1.00× | 1.19× |
| aes-128-gcm/comp | 39.4 | 46.9 | 35.2 | 0.84× | 1.12× |
| salsa20/comp | 41.6 | 43.5 | 32.6 | 0.96× | 1.27× |
| blowfish/nocomp | 36.6 | 42.8 | 30.0 | 0.86× | 1.22× |
| sm4/nocomp | 34.3 | 35.5 | 3.4 | 0.97× | 10.1× |
| 3des/nocomp | 30.3 | 22.3 | 13.8 | **1.36×** | 2.19× |

> 注：本跑 tokio null/nocomp 31.5 MB/s，低于 commit 声称的 41.2（其用 50 轮单场景）；
> 同配置 3-way 下 tokio 轻加密仍落后 smol 0.68–1.00×（ACK 膨胀已修，剩余为多线程调度/逐包 send 开销），
> 3des/sm4 重加密 tokio 领先（T/S 1.29–1.36× / 0.97×）。两端均领先 Go（除 null 持平）。

## ✅ 2026-08-04: tokio 服务端 ACK 膨胀 → reader 批量 notify（已修复）

`bench_rust_vs_go.py` 3-way（tokio/smol/Go, --conn 4 --size 1M --runs 3）发现
smol 在轻量加密（null/aes/gcm/blowfish）领先 tokio 1.1–1.5x（T/S 0.67–0.89），
重型加密（3des/sm4）tokio 领先（T/S 1.09–1.50）。

pprof（4 并发负载）：
- tokio server 3.35 核 vs smol 1.48 核；`UdpSocket::send_to` 51.6%（1.73 核）
- SNMP（50 轮，数据段完全相同 161306 vs 161184）：tokio ACK datagram **70473** vs smol **5033**（14x）

根因：`kcp-rs::spawn_listener_reader` 每 datagram `push_and_reuse`（内含 `notify_one`）。
tokio 多线程下 peer input loop 被逐包跨线程唤醒 → 1 包 burst → `flush_input_batch`
每数据段发一个 ACK。客户端（直连 OS socket drain）不膨胀（1.19x），确认是 PeerQueue 逐包 notify。

修复：reader 改为批量 drain socket → 路由各 datagram → 每 affected peer queue 只 `notify_one`
一次（`push_and_reuse` 移除内部 notify；input loop 一次 drain 整批、批量 ACK；spare 缓冲池避免逐包分配）。

验证（release，50 轮 4 并发）：
- tokio：25.3 → **41.2 MB/s**（+63%），ACK 70473 → **959**（73x）
- smol：38.4 → 43.4 MB/s（无回归，批量 notify 也小幅受益）
- 稳态差距 0.66x → 0.95x（打平）；一次性 4x1MB：22.6 → 27.6 MB/s
- `make gate` / stress（8/8）/ e2e 全过

## ⚠️ 2026-08-03: 首次建立期回归（未修复）

`bench_rust_vs_go.py` 同配置 A/B 实测（--conn 4 --size 1M --runs 3）：
当前 master vs 00e5e3df，**当前版慢 2–3 倍**（null/no-comp 14.6 vs 43.4 MB/s）。
根因是**新鲜隧道首次并发突发**的建立期延迟（current 280–550ms vs old 83ms），
非稳态吞吐（稳态两者 ~65–75ms 持平）。详见 `docs/PERF_GAP_ANALYSIS_00e5e3df_vs_master.md`。
热点：客户端 accept 第 2 次 accept 阻塞 ~90ms；服务端 KcpListener peer 建立间隔 ~100–200ms。

## Bench matrix: Rust-tokio vs Go (from bench_results.json, 2026-07-27)

| Scenario | Rust-tokio (MB/s) | Go (MB/s) | Gap | Status |
|----------|-------------------|-----------|------|--------|
| 3des/comp | 15.5 | 13.4 | +16% | ✅ P0 done |
| none/no-comp | 65.7 | 46.2 | +42% | ✅ P1 done |
| aes-128-gcm/no-comp | 55.2 | 43.2 | +28% | ✅ P1 done |
| aes-128/no-comp | 35.4 | 39.8 | -11% | P3 |
| salsa20/comp | 33.0 | 36.9 | -11% | P3 |

Note: Rust-smol 3des/comp = 16.5 MB/s (also faster than Go).

---

## pprof 3des/comp analysis (2026-07-28, post ITIMER_REAL + frame-pointer)

- Build: `make profiling-bins` (force-frame-pointers, ITIMER_REAL, frame-pointer unwinding)
- Duration: 10s, Total samples = 8.89s (88.83% coverage)

### Top frames (wall-clock)

| Rank | Frame | flat % | cum % | Notes |
|------|-------|--------|-------|-------|
| 1 | `tokio::runtime::park::Inner::park` | 99.20% | 99.20% | I/O wait (correct for I/O-bound server) |
| 2 | `Inner::park_condvar` | 0.78% | 0.78% | tokio parking |
| 3 | `KcpServerSession::kcp_input_and_smux` | 0.01% | 0.02% | real work (filtered from park) |

### Interpretation

With ITIMER_REAL (wall-clock), I/O-bound servers show 99% in `Inner::park`.
This is **correct** — the server spends most wall time waiting for I/O.
To find CPU hotspots, filter: `go tool pprof -top -ignore="Inner::park" profile.pb.gz`

### Previous pprof issues (fixed)

- ~~`unix_madvise` 13% flat~~ — Fixed: ProfilingAllocator slow path now uses stack-allocated arrays
- ~~`__pthread_atfork_parent_handlers` 20% flat~~ — Fixed: frame-pointer unwinding stops at libc boundaries
- ~~`record_sample` 100% flat in heap profile~~ — Fixed: skip_profiling_frames() strips profiling internal frames
- ~~0.7% sample coverage~~ — Fixed: ITIMER_REAL instead of ITIMER_PROF gives ~90% coverage

---

## Previous pprof 3des/comp analysis (2026-07-27, pre-fixes)

| Rank | Frame | flat % | cum % | Notes |
|------|-------|--------|-------|-------|
| 1 | `UdpSocket::send_to` | 12.69% | 12.69% | UDP send syscall |
| 2 | `__pthread_atfork_parent_handlers` | 12.58% | 12.58% | ~~pprof signal artifact~~ (fixed) |
| 3 | `unix_madvise` | 6.40% | 6.40% | ~~ProfilingAllocator overhead~~ (fixed) |
| 4 | `Socket::recv_from_with_flags` | 4.30% | 4.30% | UDP recv syscall |
| 5 | `Selector::wake` | 3.42% | 3.42% | mio/tokio I/O driver |
| 6 | `encrypt_cfb` | 0.63% | 8.55% | 3DES CFB encrypt (cumulative) |

---

## Optimization history

### P0: Offload snappy to cpu_block even when has_encryption — ✅ DONE

Removed the `!has_encryption` guard in `should_cpu_block_compress` logic.

Result: 3des/comp tokio throughput improved from 6.9 → 15.5 MB/s (2.2×), surpassing Go (13.4 MB/s).

### P1: Channel-based blocking pool for tokio — ✅ DONE

Replaced `tokio::task::spawn_blocking` in `kio::cpu_block` with a persistent thread pool
+ `async_channel` job dispatch, mirroring smol's `BlockingPool`.

Result (bench, 10MB×4, macOS arm64, release LTO):

| Scenario | Before P1 (MB/s) | After P1 (MB/s) | Improvement |
|----------|-------------------|------------------|-------------|
| none/no-comp | ~31.6 | ~65.7 | +108% (2.1×) |
| null/no-comp | ~34.7 | ~54.4 | +57% |
| aes-128-gcm/no-comp | ~34.1 | ~55.2 | +62% |
| 3des/comp (post P0) | ~15.5 | ~15.5 | No regression |

### AES hardware acceleration — ✅ DONE

Evidence: L2 profiles showed `aes::soft::fixslice::*`; `nm` confirmed soft AES linked
despite Apple Silicon FEAT_AES.

Change: `.cargo/config.toml` sets `--cfg aes_armv8` for `aarch64-apple-darwin` and
`aarch64-unknown-linux-gnu`.

Before (soft): ~12–14 MB/s → After (armv8): ~66–85 MB/s (~5–6×).

### pprof-rs data quality fixes — ✅ DONE (2026-07-28)

1. **ITIMER_REAL** instead of **ITIMER_PROF**: I/O-bound servers get ~90% sample coverage
   vs <1%. ITIMER_REAL generates SIGALRM (signal handler updated).
2. **frame-pointer feature**: clean Rust-only backtraces, stops at libc/pthread boundaries.
3. **Empty backtrace filtering**: samples fired in libc (no frame pointers) are dropped.
4. **ProfilingAllocator hot path**: stack-allocated `[usize; 64]` + FNV-1a hash (zero alloc).
5. **Heap profile frame skipping**: `skip_profiling_frames()` strips `record_sample` etc.

## Remaining work

### Unified-session Tokio regression — ✅ FIXED (2026-08-03)

After the production cut-over to `KcptunSession`, 300 MB interleaved runs showed
the new Rust-Tokio pair about 4–8% behind `00e5e3dfeae`. The server CPU/alloc
profile identified two migration costs: per-datagram burst copies in
`spawn_input_loop`/`feed_inbound_batch`, and a fixed 2ms wait between 64 KiB
SMUX writer chunks.

Changes:

1. Reuse input burst storage and borrow original FEC data shards.
2. Transfer listener-owned per-peer datagram buffers through a bounded recycle
   pool instead of copying queue entries into `KcpConn` buffers.
3. Preserve a writer notify permit while SMUX stream data remains queued, so a
   common 128 KiB TCP write does not pause between its two 64 KiB chunks.

The follow-up profile reduced flat CPU in `feed_inbound_batch` from 7.35% to
0.19% and `spawn_input_loop` from 9.44% to 0.14%. Their flat allocation-object
share became negligible (`spawn_input_loop`: 4 samples / 0.043%); the bounded
listener buffer refill accounted for 567 objects / 6.04% rather than one copy
allocation per packet.

500 MB ABBA loopback (`aes`, `fast`, FEC 10/3, SMUX v2, no compression):

| Pair | Runs (MB/s) | Mean |
|------|-------------|------|
| unified stack | 22.45, 25.11 | 23.78 |
| `00e5e3dfeae` | 21.34, 23.30 | 22.32 |

The unified stack is +6.5% on the longer paired sample; median RTT remained in
the same 0.48–0.57ms range. Short 200 MB samples varied materially on macOS, so
use interleaved 500 MB or larger runs for commit comparisons.

**P2: Reduce UDP send overhead on macOS**

On macOS, `send_batch_to` falls through to sequential `try_send_to` + `writable().await`.
Consider using a dedicated sender thread or batching smaller packets.

Expected impact: 5-10% improvement on macOS (not applicable on Linux where sendmmsg is used).

**P3: Close remaining throughput gaps**

- aes-128/no-comp: -11% vs Go
- salsa20/comp: -11% vs Go

These are secondary; P0+P1 already moved most scenarios past Go.

## Symbol map (pprof frame patterns)

| Frame pattern | Layer |
|---------------|--------|
| `encrypt_batch` / `should_cpu_block_encrypt` | Crypto batch |
| `CryptEngine` / cipher `encrypt` / CFB | Block crypt |
| `aes::armv8::*` / `aes::ni::*` | Hardware AES |
| `TripleDesCipher::encrypt_block` | 3DES |
| `KCP::flush` / `input` / `send` / `SegmentPool` | ARQ |
| `encode_header_into` / SMUX flush | Mux |
| snappy | Compression (off with `--nocomp`) |
| `send_batch` / `UdpSocket::send_to` | UDP I/O |
| `tokio::runtime::park::Inner::park` | I/O wait (filter with `-ignore`) |
| `cpu_block` | Scheduling / offload |

## Decision tree

1. **Profile shows 99% `Inner::park`** → I/O-bound; filter with `-ignore="Inner::park"`.
2. **Cipher inner loop (L2/L3)** → algorithm micro-opts; verify not residual `dyn`.
3. **Copy / Bytes churn (L1)** → ownership pipeline.
4. **Lock / mutex (L1/L4)** → shorten critical sections; never hold KCP lock across encrypt/snappy.
5. **Syscall / send** → batch send; Linux sendmmsg only if justified.
6. **No actionable ≥~5% leaf** → stop coding; document here.

Hard rules: wire compatibility; no congestion cheats; one class per change; shared `encrypt_batch`.

## Goroutine closed-loop saturation (2026-08-14)

Profiled with identical `workers=8`, `concurrency=32`, `payload=26624` on macOS.
The dominant active frames in both runtimes are UDP syscalls and KCP; goroutine
also paid for fresh-work stealing after every owner-affine I/O completion.

Changes retained after A/B gates:

- Share one fresh task per four spawns before a stackful G becomes P-affine.
- P/M lifecycle remains lazy, while the configured P ceiling defaults to the
  physical CPU count.
- A P that already owns a reactor returns to I/O polling instead of scanning
  every other P's fresh queue; non-I/O searching Ms still steal normally.
- Connected UDP uses `recv`/`send` directly and a duplicated data-plane handle,
  avoiding peer-address materialization and the reactor registration lock.
- Canceled per-P timeout entries are pruned lazily; UDP readiness no longer
  creates a redundant 10ms fallback timer on every drained burst.

Rejected by measurement:

- Darwin `sendmsg_x`: ABI/round-trip tests passed, but typical KCP flush batches
  were too small and metadata setup erased the syscall saving.
- Unconditional round-robin placement of all fresh Gs: violated reactor/fd
  affinity and hung the closed-loop test.
- Suppressing same-P waker notifications: correct in smoke tests but no stable
  throughput improvement.
- Directly migrating a suspended corosensei fiber after `WAITING → RUNNABLE`:
  a minimal cross-thread resume test passed, but the release 64-connection I/O
  benchmark crashed with `SIGSEGV` (exit 139). corosensei requires every value
  retained on the suspended stack to be `Send`; the runtime's internal parking
  path retains a `Yielder` reference, so a `Future: Send` bound alone is not a
  sufficient proof. The experiment was fully reverted.

Final full `run_p99.sh` result (`workers=8`, `concurrency=32`, 10s): goroutine
4529 req/s versus Tokio 4838 (-6.4%), Smol 4475, and Go 3105. At fixed 500 RPS,
goroutine P99 was 0.561ms versus Tokio 1.149ms and Go 0.708ms. At concurrency
64, owner-pinned stackful Gs degrade materially (3699 req/s, P999 82.98ms).
A later rollback-control run at the same 8-P / 64-connection shape completed at
3807 req/s with P99/P999 of 23.65/25.24ms; it is a single short sample, not an
accepted regression or gain. The next architectural gate for a pure stackful
M:N design is a context backend whose suspended state and scheduler hand-off
are explicitly cross-M safe; do not migrate the current corosensei fibers.
