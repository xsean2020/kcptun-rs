//! Deadlock detection via `parking_lot::deadlock_detection`.
//!
//! When the `deadlock` feature is enabled, `parking_lot` tracks all lock
//! acquisitions and can detect cycles. This module provides:
//!
//! 1. A background thread that periodically checks for deadlocks and logs them.
//! 2. An HTTP endpoint `/debug/pprof/deadlock` for on-demand checks.
//! 3. Thread dump integration for `/debug/pprof/goroutine`.

use std::time::Duration;

/// Start a background thread that checks for deadlocks every 5 seconds.
///
/// When a deadlock is detected, logs the backtrace of all deadlocked threads
/// at `error` level.
pub fn start_deadlock_detector() {
    std::thread::Builder::new()
        .name("pprof-deadlock".into())
        .spawn(move || {
            loop {
                let deadlocks = parking_lot::deadlock::check_deadlock();
                if !deadlocks.is_empty() {
                    log::error!(
                        "=== DEADLOCK DETECTED ({} cycles) ===",
                        deadlocks.len()
                    );
                    for (i, threads) in deadlocks.iter().enumerate() {
                        log::error!("  Deadlock cycle #{} ({} threads):", i, threads.len());
                        for t in threads {
                            log::error!("    Thread Id {:#?}", t.thread_id());
                            log::error!("    {:#?}", t.backtrace());
                        }
                    }
                }
                std::thread::sleep(Duration::from_secs(5));
            }
        })
        .expect("failed to spawn deadlock detector thread");

    log::info!("deadlock detector enabled (5s interval)");
}

/// Check for deadlocks and return a human-readable report.
///
/// Returns "no deadlocks detected" when the system is healthy.
pub fn dump_deadlocks() -> String {
    let deadlocks = parking_lot::deadlock::check_deadlock();
    if deadlocks.is_empty() {
        return "no deadlocks detected\n".to_string();
    }

    let mut out = format!("=== {} DEADLOCK CYCLES ===\n\n", deadlocks.len());
    for (i, threads) in deadlocks.iter().enumerate() {
        out.push_str(&format!("Deadlock cycle #{} ({} threads):\n", i, threads.len()));
        for t in threads {
            out.push_str(&format!("  Thread Id {:#?}\n", t.thread_id()));
            out.push_str(&format!("  {:#?}\n\n", t.backtrace()));
        }
    }
    out
}
