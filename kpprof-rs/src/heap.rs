//! Heap allocation profiling via a sampling allocator wrapper.
//!
//! Wraps `mimalloc::MiMalloc` and samples allocations at a configurable rate.
//! Captures call stacks for sampled allocations and exposes them as Go pprof
//! protobuf via `/debug/pprof/heap` and `/debug/pprof/allocs`.
//!
//! ## Design
//!
//! - Sampling rate: 1 allocation per `sample_rate` bytes (default 524288 = 512KB,
//!   matching Go `runtime.MemProfileRate`).
//! - Fast path: atomic counter increment (no backtrace).
//! - Slow path (sample hit): capture raw stack addresses via `backtrace::trace()`
//!   (NO symbolization — avoids `addr2line` `OnceCell` reentrant init panics
//!   when the `pprof` crate's CPU profiler or its `trigger_lazy()` is active).
//! - Symbolization is deferred to `build_profile()` via `backtrace::resolve()`.
//! - Zero-cost when `sample_rate == 0` (profiling disabled).

use std::alloc::{GlobalAlloc, Layout};
use std::cell::Cell;
use std::collections::HashMap;
#[cfg(unix)]
use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::time::SystemTime;

// ─── Re-entrance guard ───────────────────────────────────────────────────────
//
// `record_sample` calls `backtrace::trace()` on the slow path, which itself
// allocates memory. Without a guard, those re-entrant allocations would call
// `record_sample` again, potentially capturing another backtrace, which
// allocates again, etc. This creates a deep recursion that overflows the stack
// in profiling builds (`debug = 2`, `lto = false` — no inlining means each
// frame is large).
//
// The guard is a thread-local `Cell<bool>` with `const` initialization, so it
// never allocates and adds only a single pointer dereference to the fast path.
thread_local! {
    static IN_SAMPLE: Cell<bool> = const { Cell::new(false) };
}

use parking_lot::Mutex;

#[cfg(unix)]
use pprof::protos::{self as protos, Message};

// ─── Global sampling state ───────────────────────────────────────────────────

/// Default sampling rate: one sample per 512KB allocated (Go-compatible).
pub(crate) const DEFAULT_SAMPLE_RATE: usize = 524_288;

/// Global allocation counter — incremented on every alloc/dealloc.
static ALLOC_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// Total bytes allocated (sampled, for inuse calculation).
static TOTAL_ALLOC_BYTES: AtomicU64 = AtomicU64::new(0);
static TOTAL_FREE_BYTES: AtomicU64 = AtomicU64::new(0);

/// Sample rate (0 = disabled). Set by the binary on startup.
static SAMPLE_RATE: AtomicUsize = AtomicUsize::new(DEFAULT_SAMPLE_RATE);

/// Start time of heap profiling (first sample). Used for time_nanos / duration_nanos.
static PROFILE_START: OnceLock<SystemTime> = OnceLock::new();

fn ensure_profile_start() {
    if PROFILE_START.get().is_none() {
        let _ = PROFILE_START.set(SystemTime::now());
    }
}

/// FNV-1a hash for stack deduplication. Zero-allocation (unlike DefaultHasher).
fn fnv1a_hash(data: &[usize]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &addr in data {
        hash ^= addr as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

// ─── Allocation sample records ───────────────────────────────────────────────

/// Maximum stack depth captured per sample. 64 frames covers virtually all
/// real-world async/tokio stacks without heap allocation.
const MAX_STACK_DEPTH: usize = 64;

/// A single allocation sample (one unique call stack).
#[derive(Clone)]
struct AllocSample {
    /// Raw stack addresses captured at sample time via `backtrace::trace()`.
    /// Symbolization is deferred to `build_profile()` to avoid `addr2line`
    /// `OnceCell` reentrant init panics in the allocator hot path.
    #[cfg(unix)]
    addresses: Vec<usize>,
    /// Total bytes allocated at this stack.
    alloc_bytes: u64,
    /// Total allocation count at this stack.
    alloc_count: u64,
    /// Total bytes freed at this stack (for inuse = alloc - free).
    free_bytes: u64,
    free_count: u64,
}

/// Structured frame info for Go pprof compatibility (filename + line).
/// Resolved from raw addresses during `build_profile()`.
#[cfg(unix)]
#[derive(Clone, Debug)]
struct Frame {
    name: String,
    sys_name: String,
    filename: String,
    lineno: u32,
}

/// Global sample map: stack_hash → sample.
static SAMPLES: Mutex<Option<HashMap<u64, AllocSample>>> = Mutex::new(None);

#[cfg(unix)]
fn ensure_samples() -> HashMap<u64, AllocSample> {
    let mut guard = SAMPLES.lock();
    if guard.is_none() {
        *guard = Some(HashMap::new());
    }
    // Can't move out of the guard; clone the map for read-only access.
    // This is called infrequently (only when building profiles).
    guard.as_ref().unwrap().clone()
}

fn record_sample(is_alloc: bool, size: usize) {
    let rate = SAMPLE_RATE.load(Ordering::Relaxed);
    if rate == 0 {
        return;
    }

    // Prevent re-entrant sampling: if we're already capturing a backtrace (which
    // allocates internally), skip sampling for those allocations to avoid
    // unbounded recursion that would overflow the stack.
    if IN_SAMPLE.with(|f| f.get()) {
        return;
    }

    // Atomically increment counter and check if we should sample.
    let prev = ALLOC_COUNTER.fetch_add(size, Ordering::Relaxed);
    let curr = prev.wrapping_add(size);

    // Sample when we cross a rate boundary.
    if curr / rate == prev / rate {
        return; // Same bucket — skip.
    }

    // Set the re-entrance guard BEFORE any operation that might trigger a
    // re-entrant allocation (ensure_profile_start, backtrace::trace, etc).
    IN_SAMPLE.with(|f| f.set(true));

    // Ensure we have a start time for time_nanos/duration_nanos.
    ensure_profile_start();

    // Capture raw stack addresses via backtrace::trace().
    // This does NOT trigger addr2line symbolization, avoiding OnceCell
    // reentrant init panics when the pprof CPU profiler's signal handler
    // or trigger_lazy() is active.
    //
    // Use a stack-allocated array to avoid heap allocation in the slow path
    // (which would trigger madvise/mimalloc overhead and distort the profile).
    let mut addresses: [usize; MAX_STACK_DEPTH] = [0; MAX_STACK_DEPTH];
    let mut depth: usize = 0;
    backtrace::trace(|frame| {
        if depth < MAX_STACK_DEPTH {
            addresses[depth] = frame.ip() as usize;
            depth += 1;
            true
        } else {
            false // stop — stack too deep
        }
    });
    let addr_slice = &addresses[..depth];

    // Hash on the raw addresses for stack deduplication.
    // Use FNV-1a (no allocation, unlike DefaultHasher which allocates).
    let hash: u64 = fnv1a_hash(addr_slice);

    let mut guard = SAMPLES.lock();
    if guard.is_none() {
        *guard = Some(HashMap::new());
    }
    let map = guard.as_mut().unwrap();
    let sample = map.entry(hash).or_insert_with(|| AllocSample {
        #[cfg(unix)]
        addresses: addr_slice.to_vec(),
        alloc_bytes: 0,
        alloc_count: 0,
        free_bytes: 0,
        free_count: 0,
    });

    if is_alloc {
        sample.alloc_bytes += size as u64;
        sample.alloc_count += 1;
        TOTAL_ALLOC_BYTES.fetch_add(size as u64, Ordering::Relaxed);
    } else {
        sample.free_bytes += size as u64;
        sample.free_count += 1;
        TOTAL_FREE_BYTES.fetch_add(size as u64, Ordering::Relaxed);
    }

    // Clear the re-entrance guard.
    IN_SAMPLE.with(|f| f.set(false));
}

// ─── Profiling allocator ─────────────────────────────────────────────────────

/// A global allocator that wraps `mimalloc::MiMalloc` and samples allocations.
///
/// Use this as `#[global_allocator]` when the `pprof` feature is enabled.
/// When `sample_rate == 0`, the fast path is a single atomic add — effectively
/// zero-cost.
pub struct ProfilingAllocator;

impl Default for ProfilingAllocator {
    fn default() -> Self {
        Self::new()
    }
}

impl ProfilingAllocator {
    /// Create a new profiling allocator with the default sample rate.
    pub const fn new() -> Self {
        ProfilingAllocator
    }
}

unsafe impl GlobalAlloc for ProfilingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = mimalloc::MiMalloc.alloc(layout);
        if !ptr.is_null() {
            record_sample(true, layout.size());
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = mimalloc::MiMalloc.alloc_zeroed(layout);
        if !ptr.is_null() {
            record_sample(true, layout.size());
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        record_sample(false, layout.size());
        mimalloc::MiMalloc.dealloc(ptr, layout);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // Record dealloc of old, alloc of new.
        record_sample(false, layout.size());
        let new_ptr = mimalloc::MiMalloc.realloc(ptr, layout, new_size);
        if !new_ptr.is_null() {
            record_sample(true, new_size);
        }
        new_ptr
    }
}

// ─── Symbolization (deferred from record_sample) ─────────────────────────────

/// Resolve a raw address to a `Frame` using `backtrace::resolve()`.
///
/// This is called during `build_profile()`, NOT during `record_sample()`, to
/// avoid `addr2line` `OnceCell` reentrant init panics in the allocator path.
#[cfg(unix)]
fn resolve_address(addr: usize) -> Frame {
    let mut name = "<unknown>".to_string();
    let mut filename = String::new();
    let mut lineno = 0u32;

    backtrace::resolve(addr as *mut std::ffi::c_void, |sym| {
        if let Some(n) = sym.name() {
            name = n.to_string();
        }
        if let Some(f) = sym.filename() {
            filename = f.to_string_lossy().into_owned();
        }
        if let Some(l) = sym.lineno() {
            lineno = l;
        }
    });

    Frame {
        sys_name: name.clone(),
        name,
        filename,
        lineno,
    }
}

/// Check if a symbolized frame belongs to the profiling infrastructure.
///
/// These frames are captured by `backtrace::trace()` inside `record_sample()`
/// and must be stripped so that the leaf frame is the actual allocation site.
#[cfg(unix)]
fn is_profiling_frame(name: &str) -> bool {
    // Match on substrings that cover both debug and release (mangled) names.
    // `record_sample` — the sampling function itself
    // `ProfilingAllocator` — alloc/dealloc/alloc_zeroed/realloc wrappers
    // `backtrace::trace` — the backtrace crate's trace entry point
    // `backtrace::backtrace` — internal backtrace impl
    name.contains("record_sample")
        || name.contains("ProfilingAllocator")
        || name.contains("backtrace::trace")
        || name.contains("backtrace::backtrace")
        || name.contains("kpprof::heap")
}

/// Remove leading (leaf-side) frames that belong to the profiling allocator.
///
/// `backtrace::trace()` captures frames from leaf (innermost) to root. The
/// first few frames are always `record_sample` → `ProfilingAllocator::alloc`
/// → … They must be stripped so `go tool pprof` attributes `flat` to the real
/// caller, not to the profiler.
#[cfg(unix)]
fn skip_profiling_frames(frames: Vec<Frame>) -> Vec<Frame> {
    let mut iter = frames.into_iter();
    let mut skipped = Vec::new();

    // Skip all leading profiling-internal frames.
    for fr in iter.by_ref() {
        if is_profiling_frame(&fr.name) {
            skipped.push(fr);
        } else {
            // First non-profiling frame — keep it as the new leaf.
            let mut result = vec![fr];
            result.extend(iter);
            return result;
        }
    }

    // All frames were profiling-internal (shouldn't happen, but guard anyway).
    skipped
}

// ─── pprof protobuf generation ───────────────────────────────────────────────

/// Build a Go pprof protobuf for heap (inuse_space + alloc_space).
#[cfg(unix)]
pub fn build_heap_profile() -> Vec<u8> {
    build_profile(true)
}

/// Build a Go pprof protobuf for allocs (total alloc_space + alloc_objects).
#[cfg(unix)]
pub fn build_allocs_profile() -> Vec<u8> {
    build_profile(false)
}

/// Non-Unix stub: pprof-rs requires POSIX signals, so heap profiling is
/// unavailable on Windows. Returns empty so the HTTP endpoint degrades
/// gracefully.
#[cfg(not(unix))]
pub fn build_heap_profile() -> Vec<u8> {
    Vec::new()
}

#[cfg(not(unix))]
pub fn build_allocs_profile() -> Vec<u8> {
    Vec::new()
}

#[cfg(unix)]
fn build_profile(heap: bool) -> Vec<u8> {
    let samples = ensure_samples();

    // Build a Go-compatible pprof protobuf using the same protos types as the
    // pprof crate's CPU profiler (via protobuf-codec). This ensures full
    // compatibility with `go tool pprof`. We always emit a valid Profile
    // (with 0 or more samples) so that /debug/pprof/heap is always usable,
    // matching Go's net/http/pprof behavior.

    // 0) Symbolize raw addresses → Frame structs.
    //    This is the ONLY place symbolization happens, safely outside the
    //    allocator hot path and any signal handler context.
    //
    //    Skip leading (leaf) frames that belong to the profiling infrastructure
    //    itself (record_sample, ProfilingAllocator::alloc/dealloc/realloc,
    //    backtrace::trace). Without this, `record_sample` becomes the leaf and
    //    gets 100% flat attribution, masking the real allocation caller.
    let mut symbolized: HashMap<u64, (Vec<Frame>, &AllocSample)> = HashMap::new();
    for (hash, sample) in &samples {
        let all_frames: Vec<Frame> = sample
            .addresses
            .iter()
            .map(|&a| resolve_address(a))
            .collect();
        let frames: Vec<Frame> = skip_profiling_frames(all_frames);
        symbolized.insert(*hash, (frames, sample));
    }

    // 1) Collect unique strings. string_table[0] must be "".
    let mut dedup: HashSet<String> = HashSet::new();
    dedup.insert("".to_string());

    // Common units and labels
    dedup.insert("bytes".to_string());
    dedup.insert("count".to_string());
    dedup.insert("space".to_string());

    if heap {
        dedup.insert("inuse_space".to_string());
        dedup.insert("inuse_objects".to_string());
    } else {
        dedup.insert("alloc_space".to_string());
        dedup.insert("alloc_objects".to_string());
    }

    // Include current executable name for a minimal mapping (best-effort)
    let exe_name: Option<String> = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()));
    if let Some(ref name) = exe_name {
        if !name.is_empty() {
            dedup.insert(name.clone());
        }
    }

    for (frames, _) in symbolized.values() {
        for fr in frames {
            dedup.insert(fr.name.clone());
            dedup.insert(fr.sys_name.clone());
            dedup.insert(fr.filename.clone());
        }
    }

    let mut str_tbl: Vec<String> = dedup.into_iter().collect();
    // Ensure index 0 is ""
    if let Some(pos) = str_tbl.iter().position(|s: &String| s.is_empty()) {
        str_tbl.swap(0, pos);
    } else {
        str_tbl.insert(0, "".to_string());
    }

    let mut str_index: HashMap<&str, i64> = HashMap::new();
    for (i, s) in str_tbl.iter().enumerate() {
        str_index.insert(s.as_str(), i as i64);
    }

    // 2) Build a minimal mapping (id=1). Tools like go tool pprof like to see
    // at least one mapping with has_functions when symbolic info is present.
    let mut map_tbl: Vec<protos::Mapping> = Vec::new();
    let mapping_id: u64 = 1;
    let mut mapping = protos::Mapping {
        id: mapping_id,
        has_functions: true,
        has_filenames: true,
        ..Default::default()
    };
    if let Some(ref name) = exe_name {
        if let Some(&idx) = str_index.get(name.as_str()) {
            mapping.filename = idx;
        }
    }
    map_tbl.push(mapping);

    // Determine preferred sample type string index for default_sample_type.
    // Per google/pprof Profile proto, this is an index into string_table
    // naming the preferred sample value type (e.g. "inuse_space").
    let preferred_type = if heap { "inuse_space" } else { "alloc_space" };
    let default_sample_type = *str_index.get(preferred_type).unwrap_or(&0);

    // 3) Build functions / locations / samples
    let mut fn_tbl: Vec<protos::Function> = Vec::new();
    let mut loc_tbl: Vec<protos::Location> = Vec::new();
    // Dedup key: (name, filename, lineno) -> function id
    let mut func_map: HashMap<(String, String, u32), u64> = HashMap::new();

    let mut pb_samples: Vec<protos::Sample> = Vec::new();

    for (frames, sample) in symbolized.values() {
        let mut loc_ids: Vec<u64> = Vec::new();

        for fr in frames {
            let key = (fr.name.clone(), fr.filename.clone(), fr.lineno);
            let func_id = *func_map.entry(key.clone()).or_insert_with(|| {
                let id = (fn_tbl.len() as u64) + 1;

                let function = protos::Function {
                    id,
                    name: *str_index.get(fr.name.as_str()).unwrap_or(&0),
                    system_name: *str_index.get(fr.sys_name.as_str()).unwrap_or(&0),
                    filename: *str_index.get(fr.filename.as_str()).unwrap_or(&0),
                    ..Default::default()
                };
                fn_tbl.push(function);

                let line = protos::Line {
                    function_id: id,
                    line: fr.lineno as i64,
                    ..Default::default()
                };
                let loc = protos::Location {
                    id,
                    mapping_id,
                    line: vec![line],
                    ..Default::default()
                };
                loc_tbl.push(loc);

                id
            });

            loc_ids.push(func_id);
        }

        let inuse_bytes = sample.alloc_bytes.saturating_sub(sample.free_bytes);
        let inuse_count = sample.alloc_count.saturating_sub(sample.free_count);

        let values = if heap {
            vec![inuse_bytes as i64, inuse_count as i64]
        } else {
            vec![sample.alloc_bytes as i64, sample.alloc_count as i64]
        };

        let s = protos::Sample {
            location_id: loc_ids,
            value: values,
            ..Default::default()
        };
        pb_samples.push(s);
    }

    // 4) sample_type
    // Order matches Go runtime/pprof convention for memory profiles:
    //   heap (inuse view): [inuse_space/bytes, inuse_objects/count]
    //   allocs (cumulative): [alloc_space/bytes, alloc_objects/count]
    // Bytes first is conventional so the default view shows space.
    let (ty0, unit0, ty1, unit1) = if heap {
        ("inuse_space", "bytes", "inuse_objects", "count")
    } else {
        ("alloc_space", "bytes", "alloc_objects", "count")
    };

    let sample_type = vec![
        protos::ValueType {
            ty: *str_index.get(ty0).unwrap_or(&0),
            unit: *str_index.get(unit0).unwrap_or(&0),
            ..Default::default()
        },
        protos::ValueType {
            ty: *str_index.get(ty1).unwrap_or(&0),
            unit: *str_index.get(unit1).unwrap_or(&0),
            ..Default::default()
        },
    ];

    // 5) period_type / period (Go MemProfileRate style)
    let period_type = Some(protos::ValueType {
        ty: *str_index.get("space").unwrap_or(&0),
        unit: *str_index.get("bytes").unwrap_or(&0),
        ..Default::default()
    });
    let period = SAMPLE_RATE.load(Ordering::Relaxed) as i64;

    // 6) time/duration
    let (time_nanos, duration_nanos) = if let Some(start) = PROFILE_START.get() {
        let now = SystemTime::now();
        let t = start
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as i64;
        let d = now.duration_since(*start).unwrap_or_default().as_nanos() as i64;
        (t, d)
    } else {
        (0, 0)
    };

    // 7) Assemble Profile using the same protos the pprof crate uses
    let profile = protos::Profile {
        sample_type,
        sample: pb_samples,
        mapping: map_tbl,
        location: loc_tbl,
        function: fn_tbl,
        string_table: str_tbl,
        time_nanos,
        duration_nanos,
        period_type: period_type.into(),
        period,
        default_sample_type,
        ..Default::default()
    };

    let mut content = Vec::new();
    if let Err(e) = profile.write_to_vec(&mut content) {
        log::error!("failed to serialize heap/allocs profile: {}", e);
        return Vec::new();
    }
    content
}

#[cfg(all(unix, test))]
mod tests {
    use super::*;
    use pprof::protos::Message;

    // Serialize these tests because they mutate process-global sampling state.
    static TEST_SERIAL: parking_lot::Mutex<()> = parking_lot::const_mutex(());

    fn reset_state(rate: usize) {
        let mut g = SAMPLES.lock();
        *g = Some(HashMap::new());
        SAMPLE_RATE.store(rate, Ordering::Relaxed);
        TOTAL_ALLOC_BYTES.store(0, Ordering::Relaxed);
        TOTAL_FREE_BYTES.store(0, Ordering::Relaxed);
        ALLOC_COUNTER.store(0, Ordering::Relaxed);
    }

    /// Seed a sample with raw addresses (for testing protobuf structure).
    fn seed(addresses: Vec<usize>, a_bytes: u64, a_cnt: u64, f_bytes: u64, f_cnt: u64) {
        let hash = fnv1a_hash(&addresses);
        let mut g = SAMPLES.lock();
        if g.is_none() {
            *g = Some(HashMap::new());
        }
        let m = g.as_mut().unwrap();
        let e = m.entry(hash).or_insert_with(|| AllocSample {
            addresses,
            alloc_bytes: 0,
            alloc_count: 0,
            free_bytes: 0,
            free_count: 0,
        });
        e.alloc_bytes += a_bytes;
        e.alloc_count += a_cnt;
        e.free_bytes += f_bytes;
        e.free_count += f_cnt;
    }

    #[test]
    fn empty_returns_empty() {
        let _g = TEST_SERIAL.lock();
        reset_state(1);

        // Go-compatible behavior: we still emit a valid Profile (0 samples) so
        // /debug/pprof/heap is always usable, matching net/http/pprof semantics.
        let heap_bytes = build_heap_profile();
        assert!(
            !heap_bytes.is_empty(),
            "heap profile should be a valid (possibly empty) protobuf"
        );

        let allocs_bytes = build_allocs_profile();
        assert!(
            !allocs_bytes.is_empty(),
            "allocs profile should be a valid (possibly empty) protobuf"
        );

        // Parse and validate structure for heap
        let prof = protos::Profile::parse_from_bytes(&heap_bytes).expect("parse heap profile");
        assert_eq!(prof.string_table.first(), Some(&"".to_string()));
        assert_eq!(prof.sample_type.len(), 2);
        assert!(prof.sample.is_empty());
        // We include a minimal mapping for tool compatibility
        assert!(!prof.mapping.is_empty());
    }

    #[test]
    fn heap_profile_roundtrips_via_protos() {
        let _g = TEST_SERIAL.lock();
        reset_state(1);
        // Use a dummy address (0x1000) — symbolization will resolve to "<unknown>"
        // but the protobuf structure should still be valid.
        seed(vec![0x1000], 8192, 4, 2048, 2);

        let bytes = build_heap_profile();
        assert!(!bytes.is_empty());

        let prof = protos::Profile::parse_from_bytes(&bytes).expect("parse heap profile");
        assert_eq!(prof.string_table.first(), Some(&"".to_string()));
        assert_eq!(prof.sample_type.len(), 2);

        let names: Vec<_> = prof
            .sample_type
            .iter()
            .map(|vt| {
                prof.string_table
                    .get(vt.ty as usize)
                    .cloned()
                    .unwrap_or_default()
            })
            .collect();
        assert!(names.contains(&"inuse_space".to_string()));
        assert!(names.contains(&"inuse_objects".to_string()));

        assert_eq!(prof.sample.len(), 1);
        assert_eq!(prof.sample[0].value.len(), 2);
        assert!(!prof.function.is_empty());
        assert!(!prof.location.is_empty());
    }

    #[test]
    fn allocs_profile_has_alloc_labels() {
        let _g = TEST_SERIAL.lock();
        reset_state(1);
        seed(vec![0x2000], 1024, 1, 0, 0);

        let bytes = build_allocs_profile();
        let prof = protos::Profile::parse_from_bytes(&bytes).unwrap();
        let names: Vec<_> = prof
            .sample_type
            .iter()
            .map(|vt| {
                prof.string_table
                    .get(vt.ty as usize)
                    .cloned()
                    .unwrap_or_default()
            })
            .collect();
        assert!(names.contains(&"alloc_space".to_string()));
        assert!(names.contains(&"alloc_objects".to_string()));
    }

    #[test]
    fn values_match_sample_type_count() {
        let _g = TEST_SERIAL.lock();
        reset_state(1);
        seed(vec![0x3000], 555, 3, 111, 1);

        for bytes in [build_heap_profile(), build_allocs_profile()] {
            if bytes.is_empty() {
                continue;
            }
            let prof = protos::Profile::parse_from_bytes(&bytes).unwrap();
            let st_len = prof.sample_type.len();
            for s in &prof.sample {
                assert_eq!(s.value.len(), st_len);
            }
        }
    }
}
