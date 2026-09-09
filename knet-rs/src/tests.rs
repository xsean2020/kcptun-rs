use crate::*;

#[tokio::test]
async fn test_sleep_ms() {
    let start = std::time::Instant::now();
    sleep_ms(50).await;
    assert!(start.elapsed() >= std::time::Duration::from_millis(40));
}

#[tokio::test]
async fn test_timeout_ok() {
    let result = timeout(std::time::Duration::from_secs(1), async { 42 })
        .await
        .unwrap();
    assert_eq!(result, 42);
}

#[tokio::test]
async fn test_timeout_elapsed() {
    let result = timeout(std::time::Duration::from_millis(10), sleep_ms(500)).await;
    assert!(result.is_err());
}

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

#[tokio::test]
async fn test_read_to_string() {
    let path = std::env::temp_dir().join("knet_test_file.txt");
    std::fs::write(&path, "hello world").unwrap();
    let content = read_to_string(path.clone()).await.unwrap();
    assert_eq!(content, "hello world");
    let _ = std::fs::remove_file(&path);
}

// ─── spawn_task + JoinHandle await ────────────────────────────────────────────
#[tokio::test]
async fn test_spawn_and_await() {
    let h = spawn_task(async { 42u32 });
    assert_eq!(h.await.unwrap(), 42);
}

#[tokio::test]
async fn test_cpu_block() {
    let r = cpu_block(|| (0..1000u64).sum::<u64>()).await;
    assert_eq!(r, 499500);
}

// ─── JoinHandle detach (fire-and-forget guard) ───────────────────────────────
// Dropping JoinHandle must NOT cancel the task.
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

// ─── copy_bidirectional_idle: true idle (reset on data), not total timeout ────
// A busy pipe that keeps transferring past `idle_secs` wall time must complete.
#[tokio::test]
async fn test_copy_bidirectional_idle_resets_on_data() {
    use crate::net::{TcpListener, TcpStream};
    use std::net::SocketAddr;
    use std::time::Instant;

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

/// Idle timer must fire when no data flows for `idle_secs`.
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

// ─── copy_bidirectional_postwait: Go closeWait semantics ─────────────────────

/// postwait=0: copy completes, returns immediately (no delay).
#[tokio::test]
async fn test_copy_bidirectional_postwait_immediate() {
    use crate::net::{TcpListener, TcpStream};
    use std::net::SocketAddr;
    use std::time::Instant;

    let listener = TcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap())
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let mut client = TcpStream::connect(addr.to_string()).await.unwrap();
    let (mut server, _) = listener.accept().await.unwrap();

    use crate::AsyncWriteExt;
    let _ = client.shutdown().await;
    let _ = server.shutdown().await;

    let start = Instant::now();
    let (ab, ba) = copy_bidirectional_postwait(&mut client, &mut server, 0)
        .await
        .unwrap();
    let elapsed = start.elapsed();
    assert_eq!((ab, ba), (0, 0));
    assert!(
        elapsed < std::time::Duration::from_millis(500),
        "postwait=0 must return immediately; got {elapsed:?}"
    );
}

/// postwait=1: copy completes (both sides EOF), then waits ~1s.
#[tokio::test]
async fn test_copy_bidirectional_postwait_delays() {
    use crate::net::{TcpListener, TcpStream};
    use std::net::SocketAddr;
    use std::time::Instant;

    let listener = TcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap())
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let mut client = TcpStream::connect(addr.to_string()).await.unwrap();
    let (mut server, _) = listener.accept().await.unwrap();

    use crate::AsyncWriteExt;
    let _ = client.shutdown().await;
    let _ = server.shutdown().await;

    let start = Instant::now();
    let (ab, ba) = copy_bidirectional_postwait(&mut client, &mut server, 1)
        .await
        .unwrap();
    let elapsed = start.elapsed();
    assert_eq!((ab, ba), (0, 0));
    assert!(
        elapsed >= std::time::Duration::from_millis(900),
        "postwait=1 must delay >=900ms; got {elapsed:?}"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(3),
        "postwait=1 delay too long; got {elapsed:?}"
    );
}

/// One direction reaches EOF while the reverse direction stays open. Go waits
/// one grace period, closes both endpoints, then waits the second copy's grace.
#[tokio::test]
async fn test_copy_bidirectional_postwait_half_close_finishes() {
    use crate::net::{TcpListener, TcpStream};
    use std::net::SocketAddr;
    use std::time::Instant;

    let listener = TcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap())
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let mut client = TcpStream::connect(addr.to_string()).await.unwrap();
    let (mut server, _) = listener.accept().await.unwrap();

    use crate::AsyncWriteExt;
    client.shutdown().await.unwrap();

    let start = Instant::now();
    let result = copy_bidirectional_postwait(&mut client, &mut server, 1).await;
    let elapsed = start.elapsed();
    assert!(result.is_ok());
    assert!(
        elapsed >= std::time::Duration::from_millis(1800),
        "two closeWait grace periods should elapse; got {elapsed:?}"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(4),
        "half-closed pipe must not hang; got {elapsed:?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn test_unix_listener_and_stream_connect() {
    use crate::{AsyncReadExt, AsyncWriteExt};

    let path = std::env::temp_dir().join(format!(
        "knet-unix-{}-{}.sock",
        std::process::id(),
        mono_ms()
    ));
    let listener = UnixListener::bind(&path).await.unwrap();
    let mut client = TcpStream::connect(path.to_string_lossy()).await.unwrap();
    let (mut server, _) = listener.accept().await.unwrap();

    client.write_all(b"unix").await.unwrap();
    let mut received = [0u8; 4];
    server.read_exact(&mut received).await.unwrap();
    assert_eq!(&received, b"unix");

    drop(listener);
    assert!(!path.exists(), "listener must unlink its socket on drop");
}

// ─── spawn_task throughput micro-benchmark ──────────────────────────────────
#[test]
fn bench_spawn_task_throughput() {
    const N: u32 = 50_000;

    block_on(async {
        // Warmup
        for _ in 0..1000 {
            drop(spawn_task(async {}));
        }

        let start = std::time::Instant::now();
        for _ in 0..N {
            drop(spawn_task(async {}));
        }
        let elapsed = start.elapsed();

        let ns_per_call = elapsed.as_nanos() as f64 / N as f64;
        println!(
            "spawn_task: {N} calls in {elapsed:?} = {ns_per_call:.1} ns/call ({:.0}M calls/s)",
            1e9 / ns_per_call / 1e6
        );
    });
}
