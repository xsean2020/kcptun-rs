//! FEC loss-injection harness (evidence gate for FEC pooling, plan Phase 6.2).
//!
//! Builds a pair of raw `KcpStream`s over localhost UDP with Reed-Solomon FEC
//! and injects configurable datagram loss on the data-receiver side via a
//! `LossyTransport` wrapper (ACKs flow back losslessly). Reports throughput,
//! KCP SNMP counters, and FEC recovery stats so a sustained run can be
//! profiled with the C5 (high-loss / high-reorder) workload that the canonical
//! tail-latency plan deferred.
//!
//! ```text
//! # 5% loss, 256 MiB transfer, FEC 10/3
//! cargo run -p kcp-rs --features async --release --example fec_loss -- \
//!     --loss 0.05 --size 256 --datashard 10 --parityshard 3
//! # 15% loss (C5-like), recover-heavy profile target
//! cargo run -p kcp-rs --features async --release --example fec_loss -- \
//!     --loss 0.15 --size 512 --datashard 10 --parityshard 3
//! ```
//!
//! Emits a machine-readable `RESULT` line plus a human table.

#[cfg(feature = "async")]
mod lossy {
    use std::io;
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;
    use std::time::Instant;

    use bytes::Bytes;
    use kcp_rs::{KcpConfig, KcpMode, KcpStream, PacketTransport, DEFAULT_SNMP};

    /// Bernoulli-loss wrapper around an inner transport. Drops datagrams on
    /// the **receive** side with probability `loss` and forwards the rest.
    ///
    /// Drop-on-arrival (rather than drop-on-send) preserves the transport's
    /// send-count contract: `try_send_batch` callers re-queue `packets[sent..]`
    /// against the original array, so silently filtering the send side would
    /// misalign counts and cause RTO-scale stalls. Wrap the *data receiver*
    /// with this so the ACK return path (the sender's recv) stays lossless.
    /// Lock-free (AtomicU64 splitmix64 PRNG), no per-packet alloc.
    pub struct LossyTransport {
        inner: Arc<dyn PacketTransport>,
        loss: f64,
        state: AtomicU64,
    }

    impl LossyTransport {
        pub fn new(inner: Arc<dyn PacketTransport>, loss: f64) -> Arc<Self> {
            assert!((0.0..=0.9).contains(&loss), "loss must be in [0, 0.9]");
            Arc::new(Self {
                inner,
                loss,
                state: AtomicU64::new(0x853c_49e6_748f_ea9b),
            })
        }

        fn next_u64(&self) -> u64 {
            // splitmix64-style: state is monotonically incremented, mixing
            // happens per call, so no atomic CAS loop is needed.
            let x = self
                .state
                .fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed);
            let mut z = x ^ (x >> 30);
            z = z.wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z ^= z >> 27;
            z = z.wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }

        fn should_drop(&self) -> bool {
            let thr = (self.loss * 65_536.0) as u32;
            (self.next_u64() as u32 & 0xFFFF) < thr
        }
    }

    #[async_trait::async_trait]
    impl PacketTransport for LossyTransport {
        fn local_addr(&self) -> io::Result<SocketAddr> {
            self.inner.local_addr()
        }

        async fn recv(&self, buf: &mut [u8]) -> io::Result<usize> {
            loop {
                let n = self.inner.recv(buf).await?;
                if !self.should_drop() {
                    return Ok(n);
                }
            }
        }
        fn try_recv(&self, buf: &mut [u8]) -> io::Result<usize> {
            loop {
                match self.inner.try_recv(buf) {
                    Err(e) => return Err(e),
                    Ok(n) if self.should_drop() => continue,
                    Ok(n) => return Ok(n),
                }
            }
        }
        async fn recv_vec(&self, buf: &mut Vec<u8>) -> io::Result<usize> {
            loop {
                let n = self.inner.recv_vec(buf).await?;
                if !self.should_drop() {
                    return Ok(n);
                }
            }
        }
        fn try_recv_vec(&self, buf: &mut Vec<u8>) -> io::Result<usize> {
            loop {
                match self.inner.try_recv_vec(buf) {
                    Err(e) => return Err(e),
                    Ok(n) if self.should_drop() => continue,
                    Ok(n) => return Ok(n),
                }
            }
        }

        async fn send_batch(&self, packets: &[Bytes]) -> io::Result<()> {
            self.inner.send_batch(packets).await
        }
        async fn send_batch_to(&self, packets: &[Bytes], target: SocketAddr) -> io::Result<()> {
            self.inner.send_batch_to(packets, target).await
        }
        async fn send_urgent(&self, packets: &[Bytes]) -> io::Result<()> {
            self.inner.send_urgent(packets).await
        }
        async fn send_urgent_to(&self, packets: &[Bytes], target: SocketAddr) -> io::Result<()> {
            self.inner.send_urgent_to(packets, target).await
        }
        fn try_send_batch(&self, packets: &[Bytes]) -> io::Result<usize> {
            self.inner.try_send_batch(packets)
        }
        fn try_send_batch_to(&self, packets: &[Bytes], target: SocketAddr) -> io::Result<usize> {
            self.inner.try_send_batch_to(packets, target)
        }
    }

    /// One connected UDP pair. `a` sends through `LossyTransport` (lossy),
    /// `b` is plain. Returns the two KcpStreams after FEC/conv config is applied.
    #[allow(clippy::too_many_arguments)]
    async fn build_pair(
        loss: f64,
        datashard: u32,
        parityshard: u32,
        mtu: u32,
        conv: u32,
        mode: KcpMode,
        sndwnd: u32,
        rcvwnd: u32,
    ) -> (KcpStream, KcpStream) {
        // Probe two ephemeral localhost ports, then drop the probes so the
        // connected sockets can bind them (knet::UdpSocket::connect binds its
        // own local address).
        let probe_a = knet::UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        let probe_b = knet::UdpSocket::bind(SocketAddr::from(([127, 0, 0, 1], 0))).unwrap();
        let addr_a = probe_a.local_addr().unwrap();
        let addr_b = probe_b.local_addr().unwrap();
        drop(probe_a);
        drop(probe_b);

        let conn_a = knet::UdpSocket::connect(addr_a, addr_b).unwrap();
        let conn_b = knet::UdpSocket::connect(addr_b, addr_a).unwrap();

        let inner_a: Arc<dyn PacketTransport> =
            Arc::new(knet::DatagramSocket::Udp(conn_a));
        let inner_b: Arc<dyn PacketTransport> =
            Arc::new(knet::DatagramSocket::Udp(conn_b));

        // Lossy wrapper goes on the data receiver (B): A→B data loses, while
        // A's recv (B→A ACKs) stays lossless, so recovery is measurable.
        let transport_b = if loss > 0.0 {
            LossyTransport::new(inner_b.clone(), loss) as Arc<dyn PacketTransport>
        } else {
            inner_b.clone()
        };

        let cfg_a = KcpConfig {
            conv,
            mode,
            mtu,
            sndwnd,
            rcvwnd,
            datashard,
            parityshard,
            acknodelay: true,
            ..KcpConfig::default()
        };

        let cfg_b = KcpConfig {
            conv,
            mode,
            mtu,
            sndwnd,
            rcvwnd,
            datashard,
            parityshard,
            acknodelay: true,
            ..KcpConfig::default()
        };

        let a = KcpStream::with_transport(inner_a, addr_b)
            .connected(true)
            .config(cfg_a)
            .build()
            .await
            .unwrap();
        let b = KcpStream::with_transport(transport_b, addr_a)
            .connected(true)
            .config(cfg_b)
            .build()
            .await
            .unwrap();
        (a, b)
    }

    pub fn run(args: Args) {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async move {
            let mode = match args.mode.as_str() {
                "fast" => KcpMode::Fast,
                "fast2" => KcpMode::Fast2,
                "fast3" => KcpMode::Fast3,
                "normal" => KcpMode::Normal,
                _ => KcpMode::Fast3,
            };
            let (a, b) = build_pair(
                args.loss,
                args.datashard,
                args.parityshard,
                args.mtu,
                args.conv,
                mode,
                args.sndwnd,
                args.rcvwnd,
            )
            .await;

            let total = args.size * 1024 * 1024;
            let chunk = 64 * 1024;
            let send_task = knet::spawn_task(async move {
                let mut sent = 0usize;
                let data = vec![0xABu8; chunk];
                while sent < total {
                    let n = chunk.min(total - sent);
                    a.write_all(&data[..n]).await.unwrap();
                    sent += n;
                }
                a.close();
            });

            let start = Instant::now();
            let mut got = 0usize;
            let mut buf = vec![0u8; chunk];
            let mut last_print = 0usize;
            while got < total {
                let n = b.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }
                got += n;
                if got - last_print >= 4 * 1024 * 1024 {
                    last_print = got;
                    eprintln!(
                        "progress: {:.1}/{:.1} MiB in {:.1}s",
                        got as f64 / 1048576.0,
                        total as f64 / 1048576.0,
                        start.elapsed().as_secs_f64()
                    );
                }
            }
            let elapsed = start.elapsed();

            let _ = send_task.await;

            let snmp = &DEFAULT_SNMP;
            let g = |c: &std::sync::atomic::AtomicU64| c.load(Ordering::Relaxed);
            let mbps = got as f64 / 1024.0 / 1024.0 / elapsed.as_secs_f64();
            println!(
                "RESULT loss={:.2} datashard={} parityshard={} size_mib={} elapsed_ms={:.1} \
                 mbps={:.1} bytes_ok={} fec_recovered={} fec_parity={} fec_shard_set={} \
                 fec_full_shards={} fec_errs={}",
                args.loss,
                args.datashard,
                args.parityshard,
                args.size,
                elapsed.as_millis(),
                mbps,
                got,
                g(&snmp.fec_recovered),
                g(&snmp.fec_parity_shards),
                g(&snmp.fec_shard_set),
                g(&snmp.fec_full_shards),
                g(&snmp.fec_errs),
            );
            assert_eq!(got, total, "transfer incomplete: got {} want {}", got, total);
        });
    }

    pub struct Args {
        pub loss: f64,
        pub size: usize,
        pub datashard: u32,
        pub parityshard: u32,
        pub mtu: u32,
        pub conv: u32,
        pub mode: String,
        pub sndwnd: u32,
        pub rcvwnd: u32,
    }
}

fn main() {
    #[cfg(not(feature = "async"))]
    {
        eprintln!("build with --features async");
        std::process::exit(2);
    }
    #[cfg(feature = "async")]
    {
        let mut loss = 0.05f64;
        let mut size = 256usize;
        let mut datashard = 10u32;
        let mut parityshard = 3u32;
        let mut mtu = 1350u32;
        let mut conv = 0x00C0_FFEEu32;
        let mut mode = "fast3".to_string();
        let mut sndwnd = 512u32;
        let mut rcvwnd = 512u32;

        let mut it = std::env::args().skip(1);
        while let Some(arg) = it.next() {
            let mut val = || it.next().unwrap_or_else(|| panic!("missing value for {arg}"));
            match arg.as_str() {
                "--loss" => loss = val().parse().unwrap(),
                "--size" => size = val().parse().unwrap(),
                "--datashard" => datashard = val().parse().unwrap(),
                "--parityshard" => parityshard = val().parse().unwrap(),
                "--mtu" => mtu = val().parse().unwrap(),
                "--conv" => conv = u32::from_str_radix(val().trim_start_matches("0x"), 16).unwrap(),
                "--mode" => mode = val(),
                "--sndwnd" => sndwnd = val().parse().unwrap(),
                "--rcvwnd" => rcvwnd = val().parse().unwrap(),
                other => panic!("unknown arg: {other}"),
            }
        }

        lossy::run(lossy::Args {
            loss,
            size,
            datashard,
            parityshard,
            mtu,
            conv,
            mode,
            sndwnd,
            rcvwnd,
        });
    }
}
