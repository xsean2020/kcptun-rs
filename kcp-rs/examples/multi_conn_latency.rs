//! Multi-connection raw KCP latency benchmark.
//!
//! The benchmark binds one [`KcpListener`], dials `--connections` clients,
//! accepts one echo connection per client, and measures one global latency
//! distribution.  Open-model runs use `--rps` as an aggregate fixed offer;
//! `--concurrency` switches each connection to its own closed loop.
//!
//! ```text
//! cargo run -p kcp-rs --features async --example multi_conn_latency -- \
//!     --connections 8 --rps 4000 --warmup 5 --duration 60
//! cargo run -p kcp-rs --features async --example multi_conn_latency -- \
//!     --connections 8 --concurrency 32 --duration 60
//! ```
//!
//! The sole machine-readable output is a `RESULT` line.  Percentiles are
//! nearest-rank values over all raw samples from all connections (never an
//! average of per-connection percentiles).  The existing `p50_us` through
//! `p999_us` fields are success-only; offered-load percentiles include all
//! planned measurement slots and represent shed slots as `+inf`.

#[cfg(feature = "async")]
mod benchmark {
    use std::collections::VecDeque;
    use std::env;
    use std::io;
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use kcp_rs::{KcpListener, KcpMode, KcpStream};
    use knet::AsyncReadExt;

    // Keep the same explicit profile as latency_p99.rs.  These are benchmark
    // settings, not changes to KcpConfig defaults or to wire behaviour.
    const MTU: u32 = 1350;
    const SNDWND: u32 = 512;
    const RCVWND: u32 = 512;
    const CONV: u32 = 0x00C0_FFEE;
    const SIZE_DEFAULT: usize = 1024;
    const CONNECTIONS_DEFAULT: usize = 1;
    const RPS_DEFAULT: u32 = 500;
    const WARMUP_DEFAULT: u64 = 5;
    const DURATION_DEFAULT: u64 = 60;
    const OP_TIMEOUT: Duration = Duration::from_secs(10);
    const JOIN_TIMEOUT: Duration = Duration::from_secs(12);

    #[derive(Debug)]
    struct Args {
        connections: usize,
        size: usize,
        rps: u32,
        queue_depth: usize,
        concurrency: usize,
        warmup: u64,
        duration: u64,
        rt_single: bool,
    }

    fn parse_args() -> Args {
        let mut args = Args {
            connections: CONNECTIONS_DEFAULT,
            size: SIZE_DEFAULT,
            rps: RPS_DEFAULT,
            queue_depth: 64,
            concurrency: 0,
            warmup: WARMUP_DEFAULT,
            duration: DURATION_DEFAULT,
            rt_single: false,
        };
        let mut it = env::args().skip(1);
        while let Some(key) = it.next() {
            match key.as_str() {
                "--connections" => {
                    args.connections = it
                        .next()
                        .expect("--connections needs n")
                        .parse()
                        .expect("bad connections")
                }
                "--size" => {
                    args.size = it
                        .next()
                        .expect("--size needs n")
                        .parse()
                        .expect("bad size")
                }
                "--rps" => args.rps = it.next().expect("--rps needs n").parse().expect("bad rps"),
                "--queue-depth" => {
                    args.queue_depth = it
                        .next()
                        .expect("--queue-depth needs n")
                        .parse()
                        .expect("bad queue depth")
                }
                "--concurrency" | "-c" => {
                    args.concurrency = it
                        .next()
                        .expect("--concurrency needs n")
                        .parse()
                        .expect("bad concurrency")
                }
                "--warmup" => {
                    args.warmup = it
                        .next()
                        .expect("--warmup needs seconds")
                        .parse()
                        .expect("bad warmup")
                }
                "--duration" => {
                    args.duration = it
                        .next()
                        .expect("--duration needs seconds")
                        .parse()
                        .expect("bad duration")
                }
                "--rt" => match it.next().expect("--rt needs single|multi").as_str() {
                    "single" => args.rt_single = true,
                    "multi" => args.rt_single = false,
                    other => panic!("bad --rt value {other:?}; expected single|multi"),
                },
                other => eprintln!("ignoring unknown arg: {other}"),
            }
        }
        args
    }

    #[derive(Clone, Copy)]
    struct RunWindow {
        start_at: Instant,
        measure_start: Instant,
        measure_end: Instant,
    }

    /// A runtime-agnostic start barrier.  Every worker announces readiness,
    /// then the coordinator releases all workers for one shared clock window.
    struct StartBarrier {
        total: usize,
        ready: AtomicUsize,
        ready_notify: knet::Notify,
        releases: Vec<Arc<knet::Notify>>,
        window: parking_lot::Mutex<Option<RunWindow>>,
    }

    impl StartBarrier {
        fn new(total: usize) -> Arc<Self> {
            Arc::new(Self {
                total,
                ready: AtomicUsize::new(0),
                ready_notify: knet::Notify::new(),
                releases: (0..total).map(|_| Arc::new(knet::Notify::new())).collect(),
                window: parking_lot::Mutex::new(None),
            })
        }

        async fn wait(self: &Arc<Self>, index: usize) -> RunWindow {
            self.ready.fetch_add(1, Ordering::Release);
            self.ready_notify.notify_one();
            self.releases[index].notified().await;
            self.window
                .lock()
                .as_ref()
                .copied()
                .expect("start barrier released without a window")
        }

        async fn release_when_ready(&self, warmup: Duration, duration: Duration) -> RunWindow {
            while self.ready.load(Ordering::Acquire) < self.total {
                self.ready_notify.notified().await;
            }
            // Leave a small scheduling margin after the last worker reaches the
            // barrier so all runtimes begin from one common timestamp.
            let start_at = Instant::now() + Duration::from_millis(100);
            let window = RunWindow {
                start_at,
                measure_start: start_at + warmup,
                measure_end: start_at + warmup + duration,
            };
            *self.window.lock() = Some(window);
            for release in &self.releases {
                // `notify_one` stores a permit when the worker has not polled
                // its waiter yet, so the ready/release race cannot strand it.
                release.notify_one();
            }
            window
        }
    }

    #[derive(Clone)]
    struct Metrics {
        latency_shards: Arc<Vec<Arc<parking_lot::Mutex<Vec<f64>>>>>,
        ok: Arc<AtomicUsize>,
        shed: Arc<AtomicUsize>,
        offered_slots: Arc<AtomicUsize>,
        task_failures: Arc<AtomicUsize>,
    }

    impl Metrics {
        fn new(connections: usize) -> Self {
            Self {
                latency_shards: Arc::new(
                    (0..connections)
                        .map(|_| Arc::new(parking_lot::Mutex::new(Vec::new())))
                        .collect(),
                ),
                ok: Arc::new(AtomicUsize::new(0)),
                shed: Arc::new(AtomicUsize::new(0)),
                offered_slots: Arc::new(AtomicUsize::new(0)),
                task_failures: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn shard(&self, index: usize) -> Arc<parking_lot::Mutex<Vec<f64>>> {
            self.latency_shards[index].clone()
        }
    }

    #[inline]
    fn in_measurement(window: RunWindow, planned_send: Instant) -> bool {
        planned_send >= window.measure_start && planned_send < window.measure_end
    }

    #[inline]
    fn record_shed(metrics: &Metrics, window: RunWindow, planned_send: Instant) {
        if in_measurement(window, planned_send) {
            metrics.shed.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[inline]
    fn record_offered(metrics: &Metrics, window: RunWindow, planned_send: Instant) {
        if in_measurement(window, planned_send) {
            metrics.offered_slots.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[inline]
    fn record_task_failure(metrics: &Metrics) {
        metrics.task_failures.fetch_add(1, Ordering::Relaxed);
    }

    /// Issue one write.  The timestamp intentionally precedes the await so
    /// scheduler queueing and KCP send-window backpressure are included in the
    /// measured operation.
    async fn issue_write(
        conn: &KcpStream,
        payload: &[u8],
        in_flight: &Arc<parking_lot::Mutex<VecDeque<Instant>>>,
        metrics: &Metrics,
        window: RunWindow,
        planned_send: Instant,
    ) -> bool {
        in_flight.lock().push_back(planned_send);
        match knet::timeout(OP_TIMEOUT, conn.write_all(payload)).await {
            Ok(Ok(())) => true,
            Ok(Err(_)) | Err(_) => {
                // Remove the request that failed to enter KCP.  Older requests
                // remain for the reader to match or shed on its timeout.
                let _ = in_flight.lock().pop_back();
                record_shed(metrics, window, planned_send);
                conn.close();
                false
            }
        }
    }

    async fn echo_loop(conn: KcpStream, size: usize, stop: Arc<AtomicBool>) {
        let (mut reader, writer) = conn.into_split();
        let mut buf = vec![0u8; size];
        while !stop.load(Ordering::Acquire) {
            match knet::timeout(OP_TIMEOUT, reader.read_exact(&mut buf)).await {
                Ok(Ok(_)) => {}
                Ok(Err(_)) | Err(_) => break,
            }
            match knet::timeout(OP_TIMEOUT, writer.write_all(&buf)).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) | Err(_) => break,
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_reader(
        conn: KcpStream,
        size: usize,
        window: RunWindow,
        in_flight: Arc<parking_lot::Mutex<VecDeque<Instant>>>,
        latency_shard: Arc<parking_lot::Mutex<Vec<f64>>>,
        metrics: Metrics,
        sender_done: Arc<AtomicBool>,
        ack_tx: Option<knet::Sender<()>>,
    ) {
        let mut reader = conn.clone();
        let mut buf = vec![0u8; size];
        let drain_deadline = window.measure_end + OP_TIMEOUT;
        loop {
            if sender_done.load(Ordering::Acquire) && in_flight.lock().is_empty() {
                break;
            }
            let remaining = drain_deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let timeout = remaining.min(OP_TIMEOUT);
            match knet::timeout(timeout, reader.read_exact(&mut buf)).await {
                Ok(Ok(_)) => {
                    let sent_at = in_flight.lock().pop_front();
                    if let Some(sent_at) = sent_at {
                        if in_measurement(window, sent_at) {
                            latency_shard
                                .lock()
                                .push(sent_at.elapsed().as_secs_f64() * 1e6);
                            metrics.ok.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    if let Some(tx) = &ack_tx {
                        let _ = tx.try_send(());
                    }
                }
                Ok(Err(_)) | Err(_) => break,
            }
        }
        let lost = in_flight.lock().drain(..).collect::<Vec<_>>();
        for planned_send in lost {
            record_shed(&metrics, window, planned_send);
        }
        reader.close();
    }

    async fn run_open_worker(
        conn: KcpStream,
        receiver: knet::Receiver<Instant>,
        size: usize,
        window: RunWindow,
        latency_shard: Arc<parking_lot::Mutex<Vec<f64>>>,
        metrics: Metrics,
    ) {
        let in_flight = Arc::new(parking_lot::Mutex::new(VecDeque::new()));
        let sender_done = Arc::new(AtomicBool::new(false));
        let sender_conn = conn.clone();
        let sender_queue = in_flight.clone();
        let sender_done_c = sender_done.clone();
        let sender_metrics = metrics.clone();
        let sender_metrics_task = sender_metrics.clone();
        let payload = vec![0x5Au8; size];
        let sender = knet::spawn_task(async move {
            let receiver = receiver;
            while let Ok(planned_send) = receiver.recv().await {
                if !issue_write(
                    &sender_conn,
                    &payload,
                    &sender_queue,
                    &sender_metrics_task,
                    window,
                    planned_send,
                )
                .await
                {
                    break;
                }
            }
            // If a write fails, tokens accepted by the scheduler but not
            // consumed by this worker are shed rather than silently
            // disappearing. Normally the closed channel is drained fully,
            // including measurement-window tokens which complete during the
            // bounded post-measurement drain period.
            while let Ok(planned_send) = receiver.try_recv() {
                record_shed(&sender_metrics_task, window, planned_send);
            }
            sender_done_c.store(true, Ordering::Release);
        });

        let reader_metrics = metrics.clone();
        let reader = knet::spawn_task(run_reader(
            conn.clone(),
            size,
            window,
            in_flight,
            latency_shard,
            metrics,
            sender_done,
            None,
        ));
        if !await_task(sender).await {
            record_task_failure(&sender_metrics);
        }
        if !await_task(reader).await {
            record_task_failure(&reader_metrics);
        }
        conn.close();
    }

    async fn run_closed_worker(
        conn: KcpStream,
        size: usize,
        concurrency: usize,
        window: RunWindow,
        latency_shard: Arc<parking_lot::Mutex<Vec<f64>>>,
        metrics: Metrics,
    ) {
        let until_start = window.start_at.saturating_duration_since(Instant::now());
        if !until_start.is_zero() {
            knet::sleep(until_start).await;
        }
        let in_flight = Arc::new(parking_lot::Mutex::new(VecDeque::new()));
        let sender_done = Arc::new(AtomicBool::new(false));
        let (ack_tx, ack_rx) = knet::bounded::<()>(concurrency.max(1));
        let sender_conn = conn.clone();
        let sender_queue = in_flight.clone();
        let sender_done_c = sender_done.clone();
        let sender_metrics = metrics.clone();
        let sender_metrics_task = sender_metrics.clone();
        let payload = vec![0x5Au8; size];
        let sender = knet::spawn_task(async move {
            let ack_rx = ack_rx;
            for _ in 0..concurrency {
                let planned_send = Instant::now();
                record_offered(&sender_metrics_task, window, planned_send);
                if Instant::now() >= window.measure_end
                    || !issue_write(
                        &sender_conn,
                        &payload,
                        &sender_queue,
                        &sender_metrics_task,
                        window,
                        planned_send,
                    )
                    .await
                {
                    sender_done_c.store(true, Ordering::Release);
                    return;
                }
            }
            while Instant::now() < window.measure_end {
                let remaining = window.measure_end.saturating_duration_since(Instant::now());
                match knet::timeout(remaining, ack_rx.recv()).await {
                    Ok(Ok(())) => {
                        let planned_send = Instant::now();
                        record_offered(&sender_metrics_task, window, planned_send);
                        if Instant::now() >= window.measure_end
                            || !issue_write(
                                &sender_conn,
                                &payload,
                                &sender_queue,
                                &sender_metrics_task,
                                window,
                                planned_send,
                            )
                            .await
                        {
                            break;
                        }
                    }
                    Ok(Err(_)) | Err(_) => break,
                }
            }
            sender_done_c.store(true, Ordering::Release);
        });

        let reader_metrics = metrics.clone();
        let reader = knet::spawn_task(run_reader(
            conn.clone(),
            size,
            window,
            in_flight,
            latency_shard,
            metrics,
            sender_done,
            Some(ack_tx),
        ));
        if !await_task(sender).await {
            record_task_failure(&sender_metrics);
        }
        if !await_task(reader).await {
            record_task_failure(&reader_metrics);
        }
        conn.close();
    }

    #[derive(Clone, Copy)]
    struct Stats {
        p50: f64,
        p90: f64,
        p99: f64,
        p999: f64,
        offered_p99: f64,
        offered_p999: f64,
        p99_covered: bool,
        p999_covered: bool,
    }

    #[inline]
    fn nearest_rank(samples_len: usize, q: f64) -> usize {
        ((samples_len as f64 * q).ceil() as usize).max(1)
    }

    fn stats(mut samples: Vec<f64>, offered_slots: usize) -> Stats {
        if samples.is_empty() {
            return Stats {
                p50: 0.0,
                p90: 0.0,
                p99: 0.0,
                p999: 0.0,
                offered_p99: -1.0,
                offered_p999: -1.0,
                p99_covered: false,
                p999_covered: false,
            };
        }
        samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let percentile = |q: f64| samples[nearest_rank(samples.len(), q) - 1];
        let offered_percentile = |q: f64| {
            if offered_slots == 0 {
                return (-1.0, false);
            }
            let rank = nearest_rank(offered_slots, q);
            if rank > samples.len() {
                (-1.0, false)
            } else {
                (samples[rank - 1], true)
            }
        };
        let (offered_p99, p99_covered) = offered_percentile(0.99);
        let (offered_p999, p999_covered) = offered_percentile(0.999);
        Stats {
            p50: percentile(0.50),
            p90: percentile(0.90),
            p99: percentile(0.99),
            p999: percentile(0.999),
            offered_p99,
            offered_p999,
            p99_covered,
            p999_covered,
        }
    }

    #[cfg(feature = "async")]
    async fn await_task<T>(task: knet::JoinHandle<T>) -> bool {
        task.await.is_ok()
    }

    async fn wait_task<T>(task: knet::JoinHandle<T>, timeout: Duration, metrics: &Metrics) {
        match knet::timeout(timeout, await_task(task)).await {
            Ok(true) => {}
            Ok(false) | Err(_) => record_task_failure(metrics),
        }
    }

    /*
     * The scheduler body below intentionally remains fixed-rate. Each
     * measurement slot is counted as offered before the bounded per-connection
     * channel is attempted. If this task is descheduled, overdue slots retain
     * their original timestamps and are emitted immediately; their scheduling
     * delay is measured instead of hidden as coordinated omission. A channel
     * rejection itself increments shed.
     */
    async fn schedule_open(
        senders: Vec<knet::Sender<Instant>>,
        window: RunWindow,
        rps: u32,
        metrics: Metrics,
    ) {
        let interval = Duration::from_secs_f64(1.0 / f64::from(rps));
        let mut next_send = window.start_at;
        let mut sequence = 0usize;
        while next_send < window.measure_end {
            let now = Instant::now();
            if now < next_send {
                knet::sleep(next_send - now).await;
                continue;
            }
            record_offered(&metrics, window, next_send);
            if senders[sequence % senders.len()]
                .try_send(next_send)
                .is_err()
            {
                record_shed(&metrics, window, next_send);
            }
            sequence += 1;
            next_send += interval;
        }
        for sender in senders {
            sender.close();
        }
    }

    async fn run_benchmark(args: Args) -> io::Result<()> {
        if args.connections == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "--connections must be greater than zero",
            ));
        }
        if args.size == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "--size must be greater than zero",
            ));
        }
        if args.concurrency == 0 && args.rps == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "--rps must be greater than zero in open mode",
            ));
        }
        if args.concurrency == 0 && args.queue_depth == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "--queue-depth must be greater than zero in open mode",
            ));
        }

        let listener = KcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .conv(CONV)
            .mode(KcpMode::Fast3)
            .mtu(MTU)
            .sndwnd(SNDWND)
            .rcvwnd(RCVWND)
            .build()
            .await?;
        let address = listener.local_addr()?;

        // A connect timeout turns each dial into a bounded probe, ensuring the
        // listener has seen all N peers before the accept phase proceeds.
        let mut clients = Vec::with_capacity(args.connections);
        for _ in 0..args.connections {
            let client = KcpStream::connect(address)
                .conv(CONV)
                .mode(KcpMode::Fast3)
                .mtu(MTU)
                .sndwnd(SNDWND)
                .rcvwnd(RCVWND)
                .connect_timeout(OP_TIMEOUT)
                .build()
                .await?;
            clients.push(client);
        }

        let stop_echo = Arc::new(AtomicBool::new(false));
        let mut echo_tasks = Vec::with_capacity(args.connections);
        for _ in 0..args.connections {
            let (conn, _) = listener.accept_timeout(OP_TIMEOUT).await?;
            echo_tasks.push(knet::spawn_task(echo_loop(
                conn,
                args.size,
                stop_echo.clone(),
            )));
        }

        let metrics = Metrics::new(args.connections);
        let barrier = StartBarrier::new(args.connections);
        let mut dispatchers = Vec::new();
        let mut workers = Vec::with_capacity(args.connections);
        for (index, conn) in clients.into_iter().enumerate() {
            let barrier_c = barrier.clone();
            let metrics_c = metrics.clone();
            let latency_shard = metrics.shard(index);
            if args.concurrency > 0 {
                workers.push(knet::spawn_task(async move {
                    let window = barrier_c.wait(index).await;
                    run_closed_worker(
                        conn,
                        args.size,
                        args.concurrency,
                        window,
                        latency_shard,
                        metrics_c,
                    )
                    .await;
                }));
            } else {
                let (sender, receiver) = knet::bounded::<Instant>(args.queue_depth);
                dispatchers.push(sender);
                workers.push(knet::spawn_task(async move {
                    let window = barrier_c.wait(index).await;
                    run_open_worker(conn, receiver, args.size, window, latency_shard, metrics_c)
                        .await;
                }));
            }
        }

        let window = barrier
            .release_when_ready(
                Duration::from_secs(args.warmup),
                Duration::from_secs(args.duration),
            )
            .await;
        let run_timeout =
            window.measure_end.saturating_duration_since(Instant::now()) + JOIN_TIMEOUT;
        if args.concurrency == 0 {
            let scheduler = knet::spawn_task(schedule_open(
                dispatchers,
                window,
                args.rps,
                metrics.clone(),
            ));
            wait_task(scheduler, run_timeout, &metrics).await;
        }
        for worker in workers {
            wait_task(worker, run_timeout, &metrics).await;
        }

        stop_echo.store(true, Ordering::Release);
        for task in echo_tasks {
            wait_task(task, JOIN_TIMEOUT, &metrics).await;
        }
        listener.close();

        let mut samples = Vec::new();
        for shard in metrics.latency_shards.iter() {
            samples.extend(std::mem::take(&mut *shard.lock()));
        }
        let sample_count = samples.len();
        let ok = metrics.ok.load(Ordering::Relaxed);
        let shed = metrics.shed.load(Ordering::Relaxed);
        let offered_slots = metrics.offered_slots.load(Ordering::Relaxed);
        let task_failures = metrics.task_failures.load(Ordering::Relaxed);
        let accounted = ok.saturating_add(shed);
        let incomplete = offered_slots.saturating_sub(accounted);
        let overaccounted = accounted.saturating_sub(offered_slots);
        let sample_mismatch = sample_count.abs_diff(ok);
        let valid = task_failures == 0 && offered_slots == accounted && sample_mismatch == 0;
        let summary = stats(samples, offered_slots);
        let actual_rps = if args.duration == 0 {
            0.0
        } else {
            ok as f64 / args.duration as f64
        };
        let offered_rps = if args.concurrency == 0 { args.rps } else { 0 };
        println!(
            "RESULT connections={} offered_rps={} actual_rps={:.3} size={} samples={} ok={} shed={} \
             p50_us={:.1} p90_us={:.1} p99_us={:.1} p999_us={:.1} \
             completed_rps={:.3} queue_depth={} offered_slots={} accounted={} incomplete={} \
             overaccounted={} sample_mismatch={} task_failures={} valid={} \
             offered_p99_us={:.1} offered_p999_us={:.1} \
             p99_covered={} p999_covered={} success_only=1",
            args.connections,
            offered_rps,
            actual_rps,
            args.size,
            sample_count,
            ok,
            shed,
            summary.p50,
            summary.p90,
            summary.p99,
            summary.p999,
            actual_rps,
            args.queue_depth,
            offered_slots,
            accounted,
            incomplete,
            overaccounted,
            sample_mismatch,
            task_failures,
            valid,
            summary.offered_p99,
            summary.offered_p999,
            u8::from(summary.p99_covered),
            u8::from(summary.p999_covered),
        );
        if !valid {
            return Err(io::Error::other(
                "benchmark result failed task/accounting/sample validation",
            ));
        }
        Ok(())
    }

    #[cfg(feature = "async")]
    fn block_future(
        single: bool,
        future: std::pin::Pin<Box<dyn std::future::Future<Output = io::Result<()>> + Send>>,
    ) -> io::Result<()> {
        if single {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to build current-thread runtime");
            runtime.block_on(future)
        } else {
            knet::block_on(future)
        }
    }

    pub fn run() {
        let args = parse_args();
        let single = args.rt_single;
        let future: std::pin::Pin<Box<dyn std::future::Future<Output = io::Result<()>> + Send>> =
            Box::pin(run_benchmark(args));
        if let Err(error) = block_future(single, future) {
            eprintln!("multi_conn_latency: {error}");
            std::process::exit(1);
        }
    }
}

#[cfg(feature = "async")]
fn main() {
    benchmark::run();
}

#[cfg(not(any(feature = "async", feature = "async")))]
fn main() {
    eprintln!(
        "error: this example requires `async` or `async`, e.g.\n\
         cargo run -p kcp-rs --features async --example multi_conn_latency"
    );
    std::process::exit(2);
}
