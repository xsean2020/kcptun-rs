//! Repro: accept()-shaped loop vs back-to-back notify_one.
use knet::Notify;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

fn main() {
    // 单 Notify + 单队列; notifier 快速连发 3 次 push+notify(同一读批的 SYN/PSH/FIN 形态),
    // waiter 每次醒来只取一个元素就返回(= accept() 返回), 下一轮 accept 重新进入。
    let notify = Arc::new(Notify::new());
    let queue: Arc<StdMutex<VecDeque<u32>>> = Arc::new(StdMutex::new(VecDeque::new()));
    let produced = Arc::new(AtomicUsize::new(0));
    let consumed = Arc::new(AtomicUsize::new(0));

    let (n_waiter, q_waiter) = (notify.clone(), queue.clone());
    let (p_c, c_c) = (produced.clone(), consumed.clone());
    std::thread::spawn(move || {
        // notifier: 连续 3 组 push+notify, 组间微秒级
        for batch in 0..3u32 {
            for i in 0..3u32 {
                q_waiter.lock().unwrap().push_back(batch * 10 + i);
                p_c.fetch_add(1, Ordering::Release);
                n_waiter.notify_one();
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    });

    // waiter: 模拟 9 次 accept() 调用(每次醒来处理 1 个元素)
    for _ in 0..9u32 {
        let n = notify.clone();
        let q = queue.clone();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            loop {
                if q.lock().unwrap().pop_front().is_some() {
                    c_c.fetch_add(1, Ordering::Release);
                    return;
                }
                if std::time::Instant::now() > deadline {
                    eprintln!("accept() TIMED OUT — lost wakeup");
                    std::process::exit(1);
                }
                n.notified().await;
            }
        });
    }
    let p = produced.load(Ordering::Acquire);
    let c = consumed.load(Ordering::Acquire);
    println!("produced={p} consumed={c}");
    if p != c {
        eprintln!("LOST WAKEUP: {p} produced but only {c} consumed");
        std::process::exit(1);
    }
    println!("no lost wakeup");
}
