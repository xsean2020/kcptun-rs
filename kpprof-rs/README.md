# kpprof-rs

Go-compatible pprof HTTP server for Rust.

Provides a `net/http/pprof`-compatible HTTP server that serves CPU profiles, heap/allocs profiles, and thread dumps as **Go pprof protobuf** — analyzable directly with the official `go tool pprof` toolchain.

This means you can profile a **Rust** binary and analyze it with the **Go** toolchain's flame graph, top, and source-listing views.

## Table of Contents

- [Overview](#overview)
- [Feature Flags](#feature-flags)
- [Integration](#integration)
- [Build](#build)
- [Quick Start](#quick-start)
- [HTTP Endpoints](#http-endpoints)
- [CPU Profiling](#cpu-profiling)
- [Heap & Allocation Profiling](#heap--allocation-profiling)
- [Thread Dump (Goroutine Equivalent)](#thread-dump-goroutine-equivalent)
- [Deadlock Detection](#deadlock-detection)
- [Symbol Lookup](#symbol-lookup)
- [Go Compatibility Notes](#go-compatibility-notes)
- [Architecture](#architecture)
- [Troubleshooting](#troubleshooting)

---

## Overview

kpprof-rs is a standalone Rust library that:

1. Provides a `ProfilingAllocator` that wraps `mimalloc` and samples heap allocations.
2. Spawns a lightweight HTTP server on a user-specified address, serving `/debug/pprof/*` endpoints.
3. Emits profiles in Go's pprof protobuf format (application-level gzipped), so `go tool pprof` can consume them without any conversion.

The crate depends on `knet-rs` for tokio-based async I/O, but has no coupling to any specific application protocol — it can be dropped into any Rust binary.

---

## Feature Flags

| Feature | Default | Description |
|---------|---------|-------------|
| `tokio` | ✅ | Use tokio runtime (via `knet-rs`) |
| `deadlock` | ❌ | Enable deadlock detection via `parking_lot::deadlock_detection` (adds runtime overhead) |

The consuming binary typically wraps these behind its own feature gates. For example, a binary might expose `pprof` (enables the dependency) and `pprof-deadlock` (enables `pprof` + `deadlock`).

---

## Integration

### 1. Add dependency

```toml
[dependencies]
kpprof-rs = { path = "...", optional = true, default-features = false }

[features]
pprof = ["dep:kpprof-rs"]
pprof-deadlock = ["pprof", "kpprof-rs/deadlock"]
```

### 2. Use the profiling allocator

```rust
#[cfg(feature = "pprof")]
#[global_allocator]
static GLOBAL: kpprof::ProfilingAllocator = kpprof::ProfilingAllocator::new();

#[cfg(not(feature = "pprof"))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;
```

### 3. Start the server

```rust
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[cfg(feature = "pprof")]
if let Some(ref addr) = pprof_addr {
    // Optional: background deadlock detector (requires "deadlock" feature)
    #[cfg(feature = "deadlock")]
    kpprof::start_deadlock_detector();

    let stop = Arc::new(AtomicBool::new(false));
    kio::spawn_task(async move {
        let _ = kpprof::run_pprof(&addr, stop).await;
    });
}
```

`run_pprof()` takes an address string (e.g. `"127.0.0.1:6060"`) and an `Arc<AtomicBool>` stop flag. It returns when the stop flag is set to `true`.

---

## Build

### Profiling build (recommended)

For human-readable stack traces, build with frame pointers and debug info:

```bash
RUSTFLAGS="-C force-frame-pointers=yes" \
  cargo build --profile profiling -p my-binary
```

A suitable `profiling` profile in `Cargo.toml`:

```toml
[profile.profiling]
inherits = "release"
lto = false          # no LTO → readable stacks
codegen-units = 16
strip = false        # keep symbols
debug = 2            # full debug info
```

### With deadlock detection

```bash
cargo build --profile profiling --features pprof-deadlock -p my-binary
```

### Smol runtime

```bash
cargo build --profile profiling --no-default-features \
  --features pprof -p my-binary
```

### Release build (symbols stripped)

```bash
# pprof still works, but stack traces will lack function names
cargo build --release --features pprof -p my-binary
```

> **Tip:** Always prefer the `profiling` profile for profiling work. The `release` profile strips symbols and enables LTO, making flame graphs unreadable.

---

## Quick Start

### 1. Start your binary with pprof

```bash
./my-binary --pprof 127.0.0.1:6060
```

(How `--pprof` is wired up depends on your binary's CLI. See [Integration](#integration).)

### 2. Browse available profiles

Open in a browser:

```
http://127.0.0.1:6060/debug/pprof/
```

### 3. Capture a CPU profile

```bash
# Direct — go tool pprof handles gzip transparently
go tool pprof -http=:0 http://127.0.0.1:6060/debug/pprof/profile?seconds=30
```

### 4. Capture a heap profile

```bash
curl -o heap.pb http://127.0.0.1:6060/debug/pprof/heap
go tool pprof -http=:0 heap.pb
```

### 5. View a thread dump

```bash
curl 'http://127.0.0.1:6060/debug/pprof/goroutine?debug=2'
```

---

## HTTP Endpoints

All endpoints are under `/debug/pprof/`. Matching Go's `net/http/pprof`:

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/debug/pprof/` | GET / HEAD | HTML index listing all profile types |
| `/debug/pprof/profile?seconds=N` | GET / HEAD | CPU profile as gzip protobuf (default 30s, clamped 1–300) |
| `/debug/pprof/cmdline` | GET / HEAD | Command line args (`\0`-separated) |
| `/debug/pprof/symbol` | GET / POST | Symbol lookup (`num_symbols` format) |
| `/debug/pprof/heap?gc=1&debug=N` | GET / HEAD | Heap allocation profile (gzip protobuf, or text summary if debug>0) |
| `/debug/pprof/allocs?debug=N` | GET / HEAD | Cumulative allocation profile (gzip protobuf) |
| `/debug/pprof/goroutine?debug=N` | GET / HEAD | Thread dump (debug>0 = text, debug=0 = empty protobuf) |
| `/debug/pprof/block` | GET / HEAD | Empty block profile (no block profiling in Rust) |
| `/debug/pprof/mutex` | GET / HEAD | Empty mutex profile |
| `/debug/pprof/threadcreate` | GET / HEAD | Empty threadcreate profile |
| `/debug/pprof/trace` | GET | **400** — trace not supported in Rust runtime |
| `/debug/pprof/deadlock` | GET / HEAD | Deadlock check (requires `deadlock` feature) |

All non-GET/HEAD requests (except `symbol` which allows POST) return `405 Method Not Allowed`.

All responses include `X-Content-Type-Options: nosniff` and `Connection: close`.

---

## CPU Profiling

CPU profiling uses the [`pprof`](https://crates.io/crates/pprof) crate's `ProfilerGuardBuilder` at **997 Hz** sampling frequency. The profiling work runs on a blocking thread pool via `kio::cpu_block` to avoid blocking the async runtime.

### Capture

```bash
# Option A: Direct HTTP fetch — go tool pprof handles gzip transparently
go tool pprof -http=:0 http://127.0.0.1:6060/debug/pprof/profile?seconds=30

# Option B: Save to file, then analyze
curl -o cpu.pb http://127.0.0.1:6060/debug/pprof/profile?seconds=30
go tool pprof -http=:0 cpu.pb
```

### Analyze

```bash
# Interactive web UI (flame graph, top, source listing)
go tool pprof -http=127.0.0.1:0 cpu.pb

# Top functions (CLI)
go tool pprof -top cpu.pb

# Source-level annotation
go tool pprof -list=my_function cpu.pb

# Compare two profiles
go tool pprof -base cpu-before.pb cpu-after.pb
```

### Query parameters

| Parameter | Default | Range | Description |
|-----------|---------|-------|-------------|
| `seconds` | 30 | 1–300 | CPU sampling duration |

---

## Heap & Allocation Profiling

Heap profiling is provided by `ProfilingAllocator` — a wrapper around `mimalloc::MiMalloc` that samples allocations at a configurable rate.

### How it works

| Aspect | Detail |
|--------|--------|
| Default sample rate | 1 sample per **512 KB** allocated (`DEFAULT_SAMPLE_RATE = 524_288`), matching Go's `runtime.MemProfileRate` |
| Fast path | Single atomic counter increment — effectively zero-cost |
| Slow path (sample hit) | Captures raw stack addresses via `backtrace::trace()` (no symbolization in allocator path) |
| Symbolization | Deferred to profile build time via `backtrace::resolve()` |
| Re-entrance guard | Thread-local `Cell<bool>` prevents unbounded recursion when `backtrace::trace()` itself allocates |
| Zero-cost mode | When `sample_rate == 0`, profiling is disabled (fast path only) |

### Two profile types

| Endpoint | Sample types | Description |
|----------|--------------|-------------|
| `/debug/pprof/heap` | `inuse_space` (bytes) + `inuse_objects` (count) | Live (in-use) memory snapshot |
| `/debug/pprof/allocs` | `alloc_space` (bytes) + `alloc_objects` (count) | Cumulative allocations since start |

### Capture

```bash
# Heap (in-use memory)
curl -o heap.pb http://127.0.0.1:6060/debug/pprof/heap
go tool pprof -http=:0 heap.pb

# Allocs (cumulative)
curl -o allocs.pb http://127.0.0.1:6060/debug/pprof/allocs
go tool pprof -http=:0 allocs.pb
```

### Query parameters

| Parameter | Description |
|-----------|-------------|
| `gc=1` | Hint to run GC before sampling (mimalloc auto-manages internally; logged but no-op) |
| `debug=N` | If > 0, returns text summary instead of gzip protobuf |

---

## Thread Dump (Goroutine Equivalent)

Since Rust doesn't have goroutines, `/debug/pprof/goroutine` provides a **thread dump** equivalent:

| `debug` value | Output |
|---------------|--------|
| 0 | Empty valid pprof protobuf (gzipped) — `go tool pprof` won't error |
| 1 (default) | Thread list with names |
| 2 | Full thread dump: kernel stack, syscall info, thread status (Linux only) |

### Linux

Reads from `/proc/self/task/{tid}/`:
- `comm` — thread name
- `stack` — kernel stack (requires `CAP_SYS_PTRACE`)
- `syscall` — current syscall info
- `status` — thread state

### Non-Linux

Falls back to `parking_lot::deadlock::check_deadlock()` if the `deadlock` feature is enabled, otherwise returns a notice that thread dumps require Linux.

### Capture

```bash
# Full thread dump (same format as an unrecovered panic)
curl 'http://127.0.0.1:6060/debug/pprof/goroutine?debug=2'
```

---

## Deadlock Detection

> **Requires** the `deadlock` feature at build time.

Deadlock detection uses `parking_lot::deadlock_detection`, which tracks all `parking_lot` lock acquisitions and can detect cycles.

### Background detector

When enabled, `start_deadlock_detector()` spawns a background thread that checks for deadlocks every **5 seconds** and logs them at `error` level:

```
=== DEADLOCK DETECTED (1 cycles) ===
  Deadlock cycle #0 (2 threads):
    Thread Id 12345
    ...backtrace...
```

### On-demand check

```bash
curl 'http://127.0.0.1:6060/debug/pprof/deadlock'
```

Returns `"no deadlocks detected"` when healthy, or a full deadlock cycle report.

### Overhead

`parking_lot::deadlock_detection` adds overhead to every lock acquisition. Use only for debugging, not production.

---

## Symbol Lookup

`/debug/pprof/symbol` maps program counters to function names, matching Go's format exactly.

### GET (raw query)

```
GET /debug/pprof/symbol?0x1234567+0x7654321
```

Addresses are separated by `+` in the raw query string.

### POST (body)

```
POST /debug/pprof/symbol
Content-Type: application/x-protobuf

0x1234567+0x7654321
```

### Response format

```
num_symbols: 1
0x1234567 kpprof::run_pprof
0x7654321 kio::spawn_task
```

- First line is always `num_symbols: 1\n` (Go writes this unconditionally; pprof only checks > 0).
- Each subsequent line: `0x{address} {function_name}` (or `?` if unresolved).
- Also supports `address=0x123&address=0x456` format for backward compatibility.

Symbolization uses `backtrace::resolve()`, which works with debug info in the binary (requires `debug = 2` in the build profile).

---

## Go Compatibility Notes

| Aspect | Go behavior | kpprof-rs behavior |
|--------|-------------|--------------------|
| Gzip | Application-level (inside `runtime/pprof` encoder) | Application-level (via `flate2`, inside `gzip_bytes()`) — **no** `Content-Encoding` header |
| `X-Content-Type-Options` | `nosniff` on all responses | ✅ Same |
| `Content-Disposition` | `attachment; filename="profile"` on profiles | ✅ Same |
| Method enforcement | GET-only (Go 1.22+), except symbol allows POST | ✅ Same |
| Symbol format | `num_symbols: 1\n` + `0xADDR name\n` | ✅ Same |
| CPU sample rate | 100 Hz (default) | 997 Hz (pprof crate default) |
| Heap sample rate | `runtime.MemProfileRate` (default 512 KB) | ✅ Same (`DEFAULT_SAMPLE_RATE = 524_288`) |
| Empty profiles | Valid protobuf with 0 samples for `block`/`mutex`/`threadcreate` | ✅ Same (`empty_profile()`) |
| `trace` endpoint | Go execution tracer | **400** — not supported in Rust |
| `goroutine` | Real goroutine stacks | Thread dump (Linux `/proc`) or empty protobuf |

---

## Architecture

### Module structure

```
kpprof-rs/
├── Cargo.toml          — features: tokio/deadlock
└── src/
    ├── lib.rs           — run_pprof(), HTTP server, all route handlers
    ├── heap.rs          — ProfilingAllocator, build_heap_profile(), build_allocs_profile()
    └── deadlock.rs      — start_deadlock_detector(), dump_deadlocks() (optional)
```

### Crate dependency graph

```
kpprof-rs ──► knet-rs  (feature: tokio)
          ──► pprof (CPU profiling, protobuf-codec)
          ──► backtrace (stack capture + symbol resolution)
          ──► flate2 (gzip compression)
          ──► mimalloc (underlying allocator)
          ──► parking_lot (mutex + optional deadlock detection)
```

### ProfilingAllocator design

```
alloc(layout)
  │
  ├── mimalloc::MiMalloc.alloc(layout)    ← real allocation
  │
  └── record_sample(true, size)
        │
        ├── fast path: ALLOC_COUNTER.fetch_add(size)  ← single atomic
        │      └── if same rate bucket → return (no backtrace)
        │
        └── slow path (sample hit):
              ├── IN_SAMPLE guard set (thread-local)
              ├── backtrace::trace() → raw addresses (NO symbolization)
              ├── hash addresses → dedup in SAMPLES map
              ├── update alloc_bytes / alloc_count
              └── IN_SAMPLE guard cleared

dealloc(ptr, layout)
  │
  ├── record_sample(false, size)  ← same fast/slow path, updates free_bytes
  └── mimalloc::MiMalloc.dealloc(ptr, layout)
```

Symbolization happens only in `build_profile()`:
```
build_heap_profile() / build_allocs_profile()
  │
  ├── clone SAMPLES map
  ├── for each sample: resolve_address(addr) → Frame { name, filename, lineno }
  ├── build pprof protobuf (Profile, Sample, Function, Location, Mapping)
  └── serialize → Vec<u8>
```

### HTTP server loop

```
run_pprof(addr, stop)
  │
  ├── kio::TcpListener::bind(addr)
  │
  └── loop:
        ├── check stop flag (every 500ms via accept timeout)
        ├── accept connection
        ├── read HTTP request headers (up to 8192 bytes, 2s timeout)
        ├── parse method / path / query / Content-Length
        ├── read POST body if symbol endpoint
        ├── dispatch route:
        │     ├── /debug/pprof/           → HTML index
        │     ├── /debug/pprof/profile    → CPU profile (kio::cpu_block + pprof crate)
        │     ├── /debug/pprof/cmdline    → std::env::args()
        │     ├── /debug/pprof/symbol     → backtrace::resolve()
        │     ├── /debug/pprof/heap       → build_heap_profile() + gzip
        │     ├── /debug/pprof/allocs     → build_allocs_profile() + gzip
        │     ├── /debug/pprof/goroutine  → dump_threads() or empty_profile()
        │     ├── /debug/pprof/block      → empty_profile()
        │     ├── /debug/pprof/mutex      → empty_profile()
        │     ├── /debug/pprof/threadcreate → empty_profile()
        │     ├── /debug/pprof/trace      → 400 (not supported)
        │     ├── /debug/pprof/deadlock   → dump_deadlocks() [if feature]
        │     └── *                       → 404
        └── respond() → write_all + flush + close
```

---

## Troubleshooting

### Heap profile returns "not enabled"

If the `ProfilingAllocator` is not active (binary built without the `pprof` feature), heap/allocs endpoints return empty. Ensure the feature is enabled and the allocator is registered as `#[global_allocator]`.

### Flame graph shows `<unknown>` for all functions

Symbols are stripped. Build with the `profiling` profile and frame pointers:

```bash
RUSTFLAGS="-C force-frame-pointers=yes" \
  cargo build --profile profiling --features pprof -p my-binary
```

The `profiling` profile should keep `debug = 2`, `strip = false`, `lto = false`.

### `go tool pprof` errors on fetch

The profile file might be empty or the server not ready. Verify:

```bash
# Check the server is up
curl -s http://127.0.0.1:6060/debug/pprof/ | head

# Check CPU profile works (short duration)
curl -o /tmp/test.pb 'http://127.0.0.1:6060/debug/pprof/profile?seconds=3'
file /tmp/test.pb   # should show: gzip compressed data
```

### Thread dump shows "requires Linux"

Full thread dumps (`goroutine?debug=2`) require `/proc/self/task/`, which is Linux-only. On macOS, use `samply` or `Instruments` instead.

### Deadlock endpoint returns 404

Deadlock detection requires the `deadlock` feature (not just the base `pprof`). Rebuild:

```bash
cargo build --profile profiling --features pprof-deadlock -p my-binary
```

### Reentrant allocation panic in profiling builds

This was a known issue where `backtrace::trace()` in the allocator path could trigger `addr2line` `OnceCell` reentrant initialization. It is fixed by deferring symbolization to `build_profile()`. If you see this, ensure you're using the latest code.
