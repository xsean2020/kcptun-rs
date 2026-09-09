#![cfg(feature = "async")]

use kcp_rs::{KcpConfig, KcpMode, KcpStream, KcpTcpListener};
use knet::AsyncWriteExt;

fn root_test() -> bool {
    std::env::var("KCPTCP_ROOT_TEST").is_ok()
        && cfg!(target_os = "linux")
        && unsafe { libc::geteuid() } == 0
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_tcp_bidirectional() {
    if !root_test() {
        eprintln!("skipped");
        return;
    }
    let listener = KcpTcpListener::bind("127.0.0.1:0").build().unwrap();
    let addr = listener.local_addr().unwrap();
    let mut client = KcpStream::connect_tcp(addr)
        .mode(KcpMode::Fast3)
        .build()
        .await
        .unwrap();
    let (server, _) = listener.accept().await.unwrap();
    client.write_all(b"tcp-hello").await.unwrap();
    client.flush().await.unwrap();
    let mut buf = [0u8; 16];
    let mut filled = 0;
    while filled < 9 {
        filled += server.read(&mut buf[filled..]).await.unwrap();
    }
    assert_eq!(&buf[..filled], b"tcp-hello");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connect_tcp_bidirectional_fec_10_3() {
    if !root_test() {
        eprintln!("skipped");
        return;
    }
    let listener = KcpTcpListener::bind("127.0.0.1:0")
        .config(KcpConfig {
            datashard: 10,
            parityshard: 3,
            ..KcpConfig::default()
        })
        .build()
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let mut client = KcpStream::connect_tcp(addr)
        .mode(KcpMode::Fast3)
        .fec(10, 3)
        .build()
        .await
        .unwrap();
    let (server, _) = listener.accept().await.unwrap();
    // 32 KiB payload spans multiple FEC groups (10 data + 3 parity), so FEC
    // encode/decode is genuinely exercised on both sides.
    let payload = vec![0xABu8; 32768];
    client.write_all(&payload).await.unwrap();
    client.flush().await.unwrap();
    let mut buf = vec![0u8; payload.len()];
    let mut filled = 0;
    while filled < payload.len() {
        filled += server.read(&mut buf[filled..]).await.unwrap();
    }
    assert_eq!(&buf[..], &payload[..]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconnect_after_drop_gets_fresh_session() {
    if !root_test() {
        eprintln!("skipped");
        return;
    }
    let listener = KcpTcpListener::bind("127.0.0.1:0").build().unwrap();
    let addr = listener.local_addr().unwrap();

    // First connection
    let mut client1 = KcpStream::connect_tcp(addr)
        .mode(KcpMode::Fast3)
        .build()
        .await
        .unwrap();
    let (server1, _) = listener.accept().await.unwrap();
    client1.write_all(b"hello1").await.unwrap();
    client1.flush().await.unwrap();
    let mut buf = [0u8; 16];
    let n = server1.read(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], b"hello1");
    drop(client1);
    drop(server1);

    // Second connection (fresh TCP 3-way handshake)
    let mut client2 = KcpStream::connect_tcp(addr)
        .mode(KcpMode::Fast3)
        .build()
        .await
        .unwrap();
    let (server2, _) = listener.accept().await.unwrap();
    client2.write_all(b"hello2").await.unwrap();
    client2.flush().await.unwrap();
    let mut buf2 = [0u8; 16];
    let n2 = server2.read(&mut buf2).await.unwrap();
    assert_eq!(&buf2[..n2], b"hello2");
}
