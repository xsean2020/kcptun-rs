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
//! - Slow path (sample hit): capture `backtrace::Backtrace`, record in global map.
//! - Zero-cost when `sample_rate == 0` (profiling disabled).

use std::alloc::{GlobalAlloc, Layout};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::time::SystemTime;

use pprof::protos::{self as protos, Message};
use parking_lot::Mutex;

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

// ─── Allocation sample records ───────────────────────────────────────────────

/// A single allocation sample (one unique call stack).
#[derive(Clone)]
struct AllocSample {
    /// Rich frames (demangled name, filename, line) captured at sample time.
    frames: Vec<Frame>,
    /// Total bytes allocated at this stack.
    alloc_bytes: u64,
    /// Total allocation count at this stack.
    alloc_count: u64,
    /// Total bytes freed at this stack (for inuse = alloc - free).
    free_bytes: u64,
    free_count: u64,
}

/// Structured frame info for better Go pprof compatibility (filename + line).
#[derive(Clone, Debug)]
struct Frame {
    name: String,
    sys_name: String,
    filename: String,
    lineno: u32,
}

/// Global sample map: stack_hash → sample.
static SAMPLES: Mutex<Option<HashMap<u64, AllocSample>>> = Mutex::new(None);

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

    // Atomically increment counter and check if we should sample.
    let prev = ALLOC_COUNTER.fetch_add(size, Ordering::Relaxed);
    let curr = prev.wrapping_add(size);

    // Sample when we cross a rate boundary.
    if curr / rate == prev / rate {
        return; // Same bucket — skip.
    }

    // Ensure we have a start time for time_nanos/duration_nanos.
    ensure_profile_start();

    // Capture backtrace on the slow path.
    let bt = backtrace::Backtrace::new();
    let frames: Vec<Frame> = bt
        .frames()
        .iter()
        .flat_map(|f| f.symbols())
        .map(|s| {
            let name = s
                .name()
                .map(|n| n.to_string())
                .unwrap_or_else(|| "<unknown>".to_string());
            let filename = s
                .filename()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            let lineno = s.lineno().unwrap_or(0);
            Frame {
                name: name.clone(),
                sys_name: name, // backtrace gives demangled; system_name can be same
                filename,
                lineno,
            }
        })
        .collect();

    // Hash on the structured frames (name + file + line) for stability.
    let hash: u64 = {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for fr in &frames {
            fr.name.hash(&mut hasher);
            fr.filename.hash(&mut hasher);
            fr.lineno.hash(&mut hasher);
        }
        hasher.finish()
    };

    let mut guard = SAMPLES.lock();
    if guard.is_none() {
        *guard = Some(HashMap::new());
    }
    let map = guard.as_mut().unwrap();
    let sample = map.entry(hash).or_insert_with(|| AllocSample {
        frames,
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

// ─── pprof protobuf generation ───────────────────────────────────────────────

/// Build a Go pprof protobuf for heap (inuse_space + alloc_space).
pub fn build_heap_profile() -> Vec<u8> {
    build_profile(true)
}

/// Build a Go pprof protobuf for allocs (total alloc_space + alloc_objects).
pub fn build_allocs_profile() -> Vec<u8> {
    build_profile(false)
}

fn build_profile(heap: bool) -> Vec<u8> {
    let samples = ensure_samples();

    // Build a Go-compatible pprof protobuf using the same protos types as the
    // pprof crate's CPU profiler (via protobuf-codec). This ensures full
    // compatibility with `go tool pprof`. We always emit a valid Profile
    // (with 0 or more samples) so that /debug/pprof/heap is always usable,
    // matching Go's net/http/pprof behavior.

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

    for s in samples.values() {
        for fr in &s.frames {
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

    for sample in samples.values() {
        let mut loc_ids: Vec<u64> = Vec::new();

        for fr in &sample.frames {
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

#[cfg(test)]
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

    fn seed(frames: Vec<Frame>, a_bytes: u64, a_cnt: u64, f_bytes: u64, f_cnt: u64) {
        let hash: u64 = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            for fr in &frames {
                fr.name.hash(&mut h);
                fr.filename.hash(&mut h);
                fr.lineno.hash(&mut h);
            }
            h.finish()
        };
        let mut g = SAMPLES.lock();
        if g.is_none() {
            *g = Some(HashMap::new());
        }
        let m = g.as_mut().unwrap();
        let e = m.entry(hash).or_insert_with(|| AllocSample {
            frames,
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
        assert!(!heap_bytes.is_empty(), "heap profile should be a valid (possibly empty) protobuf");

        let allocs_bytes = build_allocs_profile();
        assert!(!allocs_bytes.is_empty(), "allocs profile should be a valid (possibly empty) protobuf");

        // Parse and validate structure for heap
        let prof = protos::Profile::parse_from_bytes(&heap_bytes).expect("parse heap profile");
        assert_eq!(prof.string_table.first(), Some(&"".to_string()));
        assert_eq!(prof.sample_type.len(), 2);
        assert!(prof.sample.is_empty());
        // We include a minimal mapping for tool compatibility
        assert!(prof.mapping.len() >= 1);
    }

    #[test]
    fn heap_profile_roundtrips_via_protos() {
        let _g = TEST_SERIAL.lock();
        reset_state(1);
        let fr = Frame {
            name: "my::func".into(),
            sys_name: "my::func".into(),
            filename: "src/x.rs".into(),
            lineno: 123,
        };
        seed(vec![fr], 8192, 4, 2048, 2);

        let bytes = build_heap_profile();
        assert!(!bytes.is_empty());

        let prof = protos::Profile::parse_from_bytes(&bytes).expect("parse heap profile");
        assert_eq!(prof.string_table.first(), Some(&"".to_string()));
        assert_eq!(prof.sample_type.len(), 2);

        let names: Vec<_> = prof
            .sample_type
            .iter()
            .map(|vt| prof.string_table.get(vt.ty as usize).cloned().unwrap_or_default())
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
        let fr = Frame {
            name: "alloc_site".into(),
            sys_name: "alloc_site".into(),
            filename: "src/a.rs".into(),
            lineno: 1,
        };
        seed(vec![fr], 1024, 1, 0, 0);

        let bytes = build_allocs_profile();
        let prof = protos::Profile::parse_from_bytes(&bytes).unwrap();
        let names: Vec<_> = prof
            .sample_type
            .iter()
            .map(|vt| prof.string_table.get(vt.ty as usize).cloned().unwrap_or_default())
            .collect();
        assert!(names.contains(&"alloc_space".to_string()));
        assert!(names.contains(&"alloc_objects".to_string()));
    }

    #[test]
    fn values_match_sample_type_count() {
        let _g = TEST_SERIAL.lock();
        reset_state(1);
        let fr = Frame {
            name: "v".into(),
            sys_name: "v".into(),
            filename: "f.rs".into(),
            lineno: 9,
        };
        seed(vec![fr], 555, 3, 111, 1);

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
