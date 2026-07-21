use crate::*;

#[cfg(feature = "tokio")]
#[tokio::test]
async fn test_sleep_ms() {
    let start = std::time::Instant::now();
    sleep_ms(50).await;
    assert!(start.elapsed() >= std::time::Duration::from_millis(40));
}

#[cfg(feature = "smol")]
#[test]
fn test_sleep_ms() {
    let start = std::time::Instant::now();
    block_on(sleep_ms(50));
    assert!(start.elapsed() >= std::time::Duration::from_millis(40));
}

#[cfg(feature = "tokio")]
#[tokio::test]
async fn test_timeout_ok() {
    let result = timeout(std::time::Duration::from_secs(1), async { 42 })
        .await
        .unwrap();
    assert_eq!(result, 42);
}

#[cfg(feature = "tokio")]
#[tokio::test]
async fn test_timeout_elapsed() {
    let result = timeout(std::time::Duration::from_millis(10), sleep_ms(500)).await;
    assert!(result.is_err());
}

#[cfg(feature = "smol")]
#[test]
fn test_timeout_ok() {
    let result = block_on(timeout(std::time::Duration::from_secs(1), async { 42 })).unwrap();
    assert_eq!(result, 42);
}

#[cfg(feature = "smol")]
#[test]
fn test_timeout_elapsed() {
    let result = block_on(timeout(std::time::Duration::from_millis(10), sleep_ms(500)));
    assert!(result.is_err());
}

#[cfg(feature = "tokio")]
#[tokio::test]
async fn test_notify() {
    let n = std::sync::Arc::new(Notify::new());
    let n2 = n.clone();
    tokio::spawn(async move {
        sleep_ms(20).await;
        n2.notify_waiters();
    });
    n.notified().await;
}

#[cfg(feature = "smol")]
#[test]
fn test_notify() {
    let n = std::sync::Arc::new(Notify::new());
    let n2 = n.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(20));
        n2.notify_waiters();
    });
    block_on(n.notified());
}

#[cfg(feature = "tokio")]
#[tokio::test]
async fn test_read_to_string() {
    let path = std::env::temp_dir().join("kio_test_file.txt");
    std::fs::write(&path, "hello world").unwrap();
    let content = read_to_string(path.clone()).await.unwrap();
    assert_eq!(content, "hello world");
    let _ = std::fs::remove_file(&path);
}

#[cfg(feature = "smol")]
#[test]
fn test_read_to_string() {
    let path = std::env::temp_dir().join("kio_test_file_smol.txt");
    std::fs::write(&path, "hello world").unwrap();
    let content = block_on(read_to_string(path.clone())).unwrap();
    assert_eq!(content, "hello world");
    let _ = std::fs::remove_file(&path);
}

// ─── Multi-threaded block_on smoke test (smol only) ──────────────────────────
// Ensures block_on cleanly starts/stops worker threads without hanging.
#[cfg(feature = "smol")]
#[test]
fn test_block_on_multithread_smoke() {
    let result = block_on(async {
        // Spawn a task on the global executor and await it — this exercises
        // the multi-threaded scheduling path.
        let h = spawn_task(async { 100u32 });
        h.await + 1
    });
    assert_eq!(result, 101);
}

// ─── spawn_task + JoinHandle await (both backends) ────────────────────────────
#[cfg(feature = "tokio")]
#[tokio::test]
async fn test_spawn_and_await() {
    let h = spawn_task(async { 42u32 });
    assert_eq!(h.await.unwrap(), 42);
}

#[cfg(feature = "smol")]
#[test]
fn test_spawn_and_await() {
    let result = block_on(async {
        let h = spawn_task(async { 42u32 });
        h.await
    });
    assert_eq!(result, 42);
}

#[cfg(feature = "tokio")]
#[tokio::test]
async fn test_cpu_block() {
    let r = cpu_block(|| (0..1000u64).sum::<u64>()).await;
    assert_eq!(r, 499500);
}

#[cfg(feature = "smol")]
#[test]
fn test_cpu_block() {
    let r = block_on(cpu_block(|| (0..1000u64).sum::<u64>()));
    assert_eq!(r, 499500);
}

// ─── spawn_task throughput micro-benchmark ──────────────────────────────────
// Measures fire-and-forget `spawn_task` overhead. The `#[inline(always)]`
// attribute guarantees the thin delegation is erased at compile time, so this
// isolates the underlying runtime's spawn cost (tokio::spawn or Executor::spawn).
#[test]
fn bench_spawn_task_throughput() {
    const N: u32 = 50_000;

    block_on(async {
        // Warmup
        for _ in 0..1000 {
            let _ = spawn_task(async {});
        }

        let start = std::time::Instant::now();
        for _ in 0..N {
            let _ = spawn_task(async {});
        }
        let elapsed = start.elapsed();

        let ns_per_call = elapsed.as_nanos() as f64 / N as f64;
        println!(
            "spawn_task: {N} calls in {elapsed:?} = {ns_per_call:.1} ns/call ({:.0}M calls/s)",
            1e9 / ns_per_call / 1e6
        );
    });
}
