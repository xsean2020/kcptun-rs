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

// ─── JoinHandle detach (starvation / fire-and-forget guard) ───────────────────
// Dropping JoinHandle must NOT cancel the task on either backend. Without this,
// smol would kill flush loops / stream handlers when the spawn handle is dropped.
#[cfg(feature = "tokio")]
#[tokio::test]
async fn test_join_handle_detach_on_drop() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let done = Arc::new(AtomicBool::new(false));
    {
        let done = done.clone();
        let _h = spawn_task(async move {
            sleep_ms(30).await;
            done.store(true, Ordering::SeqCst);
        });
        // Drop handle immediately (fire-and-forget).
    }
    for _ in 0..40 {
        if done.load(Ordering::SeqCst) {
            return;
        }
        sleep_ms(10).await;
    }
    panic!("detached task did not complete after JoinHandle drop");
}

#[cfg(feature = "smol")]
#[test]
fn test_join_handle_detach_on_drop() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let done = Arc::new(AtomicBool::new(false));
    block_on(async {
        {
            let done = done.clone();
            let _h = spawn_task(async move {
                sleep_ms(30).await;
                done.store(true, Ordering::SeqCst);
            });
            // Drop handle immediately (fire-and-forget).
        }
        for _ in 0..40 {
            if done.load(Ordering::SeqCst) {
                return;
            }
            sleep_ms(10).await;
        }
        panic!("detached task did not complete after JoinHandle drop");
    });
}

// ─── copy_bidirectional_idle: true idle (reset on data), not total timeout ────
// A busy pipe that keeps transferring past `idle_secs` wall time must complete.
// The old smol path wrapped the whole copy in `timeout(idle_secs)`, which would
// kill long-lived sessions (Go closeWait is idle-based).
//
// Topology: paced writer ──► A ◄copy► B ──► drain reader
// (A/B are two ends of a second loopback TCP pair; writer feeds A via first pair.)
#[cfg(feature = "tokio")]
#[tokio::test]
async fn test_copy_bidirectional_idle_resets_on_data() {
    use crate::net::{TcpListener, TcpStream};
    use std::net::SocketAddr;
    use std::time::Instant;

    // Pair 1: paced client → accepted as left side of the copy (via bridge).
    let l1 = TcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap())
        .await
        .unwrap();
    let a1 = l1.local_addr().unwrap();
    // Pair 2: right side of the copy → drain.
    let l2 = TcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap())
        .await
        .unwrap();
    let a2 = l2.local_addr().unwrap();

    let writer = spawn_task(async move {
        let mut s = TcpStream::connect(a1.to_string()).await.unwrap();
        for i in 0u8..4 {
            sleep_ms(400).await;
            use crate::AsyncWriteExt;
            s.write_all(&[b'x' + i]).await.unwrap();
        }
        use crate::AsyncWriteExt;
        let _ = s.shutdown().await;
    });

    let (mut from_writer, _) = l1.accept().await.unwrap();
    let mut to_drain = TcpStream::connect(a2.to_string()).await.unwrap();
    let (mut from_copy, _) = l2.accept().await.unwrap();

    let drain = spawn_task(async move {
        use crate::AsyncReadExt;
        let mut buf = [0u8; 64];
        let mut n = 0u64;
        loop {
            match from_copy.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(k) => n += k as u64,
            }
        }
        n
    });

    let start = Instant::now();
    // copy from_writer → to_drain; reverse direction idle (no data).
    let (ab, _ba) = copy_bidirectional_idle(&mut from_writer, &mut to_drain, 1)
        .await
        .unwrap();
    let elapsed = start.elapsed();
    let drained = drain.await.unwrap();
    let _ = writer.await;

    assert_eq!(ab, 4);
    assert_eq!(drained, 4);
    assert!(
        elapsed >= std::time::Duration::from_millis(1200),
        "pipe must live longer than idle_secs wall clock when data keeps flowing; got {elapsed:?}"
    );
}

#[cfg(feature = "smol")]
#[test]
fn test_copy_bidirectional_idle_resets_on_data() {
    use crate::net::{TcpListener, TcpStream};
    use std::net::SocketAddr;
    use std::time::Instant;

    // smol poll_fn copy state is large in debug; use a roomy stack for the test thread.
    std::thread::Builder::new()
        .stack_size(8 * 1024 * 1024)
        .spawn(|| block_on(async {
        let l1 = TcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap())
            .await
            .unwrap();
        let a1 = l1.local_addr().unwrap();
        let l2 = TcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap())
            .await
            .unwrap();
        let a2 = l2.local_addr().unwrap();

        let writer = spawn_task(async move {
            let mut s = TcpStream::connect(a1.to_string()).await.unwrap();
            for i in 0u8..4 {
                sleep_ms(400).await;
                use crate::AsyncWriteExt;
                s.write_all(&[b'x' + i]).await.unwrap();
            }
            use crate::AsyncWriteExt;
            let _ = s.close().await;
        });

        let (mut from_writer, _) = l1.accept().await.unwrap();
        let mut to_drain = TcpStream::connect(a2.to_string()).await.unwrap();
        let (mut from_copy, _) = l2.accept().await.unwrap();

        let drain = spawn_task(async move {
            use crate::AsyncReadExt;
            let mut buf = [0u8; 64];
            let mut n = 0u64;
            loop {
                match from_copy.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(k) => n += k as u64,
                }
            }
            n
        });

        let start = Instant::now();
        let (ab, _ba) = copy_bidirectional_idle(&mut from_writer, &mut to_drain, 1)
            .await
            .unwrap();
        let elapsed = start.elapsed();
        let drained = drain.await;
        let _ = writer.await;

        assert_eq!(ab, 4);
        assert_eq!(drained, 4);
        assert!(
            elapsed >= std::time::Duration::from_millis(1200),
            "pipe must live longer than idle_secs wall clock when data keeps flowing; got {elapsed:?}"
        );
    }))
    .unwrap()
    .join()
    .unwrap();
}

/// Idle timer must fire when no data flows for `idle_secs`.
#[cfg(feature = "tokio")]
#[tokio::test]
async fn test_copy_bidirectional_idle_fires_when_quiet() {
    use crate::net::{TcpListener, TcpStream};
    use std::net::SocketAddr;
    use std::time::Instant;

    let listener = TcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap())
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let mut client = TcpStream::connect(addr.to_string()).await.unwrap();
    let (mut server, _) = listener.accept().await.unwrap();

    let start = Instant::now();
    let (ab, ba) = copy_bidirectional_idle(&mut client, &mut server, 1)
        .await
        .unwrap();
    let elapsed = start.elapsed();
    assert_eq!((ab, ba), (0, 0));
    assert!(
        elapsed >= std::time::Duration::from_millis(800)
            && elapsed < std::time::Duration::from_secs(3),
        "idle exit expected ~1s, got {elapsed:?}"
    );
}

#[cfg(feature = "smol")]
#[test]
fn test_copy_bidirectional_idle_fires_when_quiet() {
    use crate::net::{TcpListener, TcpStream};
    use std::net::SocketAddr;
    use std::time::Instant;

    std::thread::Builder::new()
        .stack_size(8 * 1024 * 1024)
        .spawn(|| {
            block_on(async {
                let listener = TcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap())
                    .await
                    .unwrap();
                let addr = listener.local_addr().unwrap();
                let mut client = TcpStream::connect(addr.to_string()).await.unwrap();
                let (mut server, _) = listener.accept().await.unwrap();

                let start = Instant::now();
                let (ab, ba) = copy_bidirectional_idle(&mut client, &mut server, 1)
                    .await
                    .unwrap();
                let elapsed = start.elapsed();
                assert_eq!((ab, ba), (0, 0));
                assert!(
                    elapsed >= std::time::Duration::from_millis(800)
                        && elapsed < std::time::Duration::from_secs(3),
                    "idle exit expected ~1s, got {elapsed:?}"
                );
            })
        })
        .unwrap()
        .join()
        .unwrap();
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
