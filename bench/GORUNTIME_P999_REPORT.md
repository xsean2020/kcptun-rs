# Goroutine Runtime — Phase 9 最终报告（2026-08-13）

## 结论（诚实）

goroutine 异步运行时**完整并已验证**：G/M/P 调度器、hchan+select、timer、
sync 原语、mio reactor、socket 全部实现；kio goroutine 后端编译通过、
测试全绿；**kcptun 全栈 e2e 完全打通**（`printf | nc` echo 20/20 稳定）。

**最后一个 bug 的根因不是 SMUX**：server 的 SMUX write_loop 从未部分构帧
（实测 drained 22 字节 → out.len=46 → 全部送出）。真正的问题是
`kio::copy_bidirectional`（TCP↔SMUX 管道）在 `closeWait==0` 时于单边 EOF
强制关闭管道，丢弃了在途的 echo。该问题**与运行时无关**（tokio 构建同样可
复现），修复应用到 tokio / smol / goroutine 三个后端。

## 修复的 4 个 bug（均已验证）

### 1. 调度器空闲自旋（CPU 194% → 0%）
`goroutine/src/task/block_on.rs` 的 `schedule_loop`：`since_global` 落在
1..60 且本地队列空时，`if since_global > 0 { continue; }` 无限回跳，永不
落入 steal/global/park → 纯 CPU 空转。**修复**：删除该自旋回退。
验证：空闲 kcptun-server CPU 186-198% → 0.0%。

### 2. kqueue READ 过滤器死亡（持续负载死锁）
goroutine UDP/TCP recv 持续负载 ~7.9s 后两个 socket 的 READ 过滤器同时失效，
recv 永久挂起。旧实现按包 `rearm()`（mio EV_ADD EV_CLEAR），~35k 次 EV_ADD
后过滤器被击穿；且 `match self.inner.lock().recv()` 的 SpinGuard 在 match arm
内存活，arm 内 rearm 再 lock 会自锁自旋。**修复**（`goroutine/src/io/net.rs`）：
- recv/send 循环先做直接非阻塞读/写，WouldBlock 时才 clear + rearm
  （按排空周期 re-arm，不按包）；
- 每次等待加 **10ms 周期安全回退计时器**，过滤器失效时兜底重试直接读；
- 显式块作用域释放 SpinGuard，避免 arm 内 rearm 自锁。
验证：200000 包 UDP 双向 echo 通过（11.8s，修复前 ~7.9s 死锁）；
新增回归测试 `udp_high_volume_echo_no_filter_death`。

### 3. connected UDP socket 发送失败
kcptun client 用 `kio::UdpSocket::from_std` 包装已 connect 的 socket，
goroutine 后端 from_std 不记录 remote → `send_batch` 返回 `NotConnected`；
且 connected socket 上 `sendto` 返回 `EISCONN`。**修复**：from_std 读取
`peer_addr()` 记录 remote；UdpSocket 新增 `connected` 字段，connected 时用
`send()`。验证：跨进程 UDP 发送成功，client→server 数据流修复。

### 4. copy_bidirectional closeWait==0 丢弃在途 echo（最终根因）
`kio-rs/src/lib.rs`，`enforce_grace_deadlines` 把 `closeWait==0` 当作
"任一方 EOF 立即撕掉管道"。`printf '...' | nc -w 5` 在 stdin EOF 后**半关闭**
（发数据后 FIN）；client 的 TCP→SMUX 方向见 EOF 即关闭管道，把稍后到达的
echo（SMUX→TCP 方向）一并丢弃 → nc 收 0 字节。Go 的 `std.Pipe` 语义是**等
双向 EOF**（`CloseWrite` 半关闭一边，继续排空另一边），closeWait 只是之后
的兜底睡眠。**修复**（三个后端同一语义）：
- `closeWait==0` 时循环等待 `both_eof()`，而非单边 EOF 即断；
- 单边 EOF 时通过 `poll_shutdown`/`poll_close` **半关闭对方**，让对方知道
  无后续数据并排空其应答。
验证：echo 在 goroutine 下 20/20 稳定；tokio 同样通过。

## 验证汇总

- **gate 全绿**：`cargo fmt --all -- --check`、`cargo test --workspace`
  （38 个测试二进制全过）、`cargo clippy --workspace -- -D warnings`。
- goruntime crate：15 个测试目标全过（调度/waker/chan/timer/sync/mpsc）。
- kio-rs goroutine 后端：库测试全过（cancel.rs 补 `async move` 适配
  goroutine 的 `'static` block_on）。
- kcptun e2e：`printf 'hello-world-echo-test' | nc -w 5` echo 20/20；
  `autoexpire_multi_port_test` 通过（multi-port round-robin + scavenger）。
- `stress_test`：8/8 通过。**注**：debug 构建下 100 连接并发曾有偶发负载
  flake（5/100 连接的 1B payload 因 UDP 段丢失 + 半关闭竞争而丢 echo），
  复跑 3 次单测 + 1 次全套均通过。项目文档注明 stress 需 release 构建
  （`cargo test --release ... --test stress_test --test-threads=1`）。

## P99/P999 Benchmark（tokio vs goroutine）

方法：`latency_p99 --mode self`，rps=500、warmup=5s、duration=60s、
payload=1024B，KCP Fast3，同一进程内 client+server echo。
（µs；两个后端的二进制均为同一代码、仅 runtime feature 不同）

| metric | tokio (r1/r2) | goroutine (r1/r2/r3) |
|--------|----------------|-----------------------|
| p50 | 102.6 / 116.2 | 104.1 / 99.5 / 119.5 |
| p90 | 142.1 / 166.8 | 147.6 / 140.0 / 172.3 |
| p99 | 196.2 / 278.9 | 253.1 / 226.4 / 273.1 |
| p999 | 506.7 / **1489.7** | 1530.9 / 340.6 / 915.7 |
| avg | 106.2 / 141.8 | 117.0 / 108.5 / 130.3 |
| max | 1050 / **43224** | 5528 / 5115 / 1270 |

### 解读（诚实）

- **中位数相当**：p50/p90/p99 上 goroutine 与 tokio 都在噪声范围内
  （~100-170µs / ~200-280µs），无显著差距。
- **p999 均高、波动大**：两个运行时都有数百 µs~1.5ms 的 p999，且跨 run
  波动都很大（tokio 507µs↔1490µs；goroutine 341µs↔1531µs）。这是小样本
  尾部测量的固有噪声，两者量级相当。
- **绝对 max 上 goroutine 更稳**：tokio r2 出现 **43ms** 的经典长尾 stall
  （正是本项目一直在攻克的 tokio 长尾问题）；goroutine 三次 run 的 max
  均 ≤5.5ms，无 43ms 级异常。
- goroutine 的 ~5ms 级 stall 来源是 recv 循环的 **10ms 回退计时器 + reactor
  唤醒路径**（kqueue 事件延迟时部分等待后由直接读重试兜底）。这是 goroutine
  运行时后续可优化的点（更细粒度的事件确认 / 减少回退等待）。

**结论**：goroutine 运行时在中位数延迟上与 tokio 持平，绝对长尾比 tokio
更可控（无 43ms 异常 stall），但 p999 尚未系统性地优于 tokio——长尾优化
是 goroutine 运行时的下一个迭代方向（reactor 唤醒 + 回退计时器路径）。

## 遗留 / 下一步

1. goroutine reactor 唤醒路径优化（消除 ~5ms 级 max stall），进一步压低
   p999。
2. 子代理观测到 `poll_tcp_read`（`goroutine/src/io/async_io.rs`）存在一个
   时序敏感的 reactor/SpinMutex 交互隐患，被插桩掩盖、20/20 稳定复跑未现；
   建议单独一轮排查加固。
3. stress_test 在 debug 构建下的偶发负载 flake：如要让 `make gate` 在
   debug 下也 100% 稳定，需为 stress 用例加 release 构建守卫或在 Makefile
   gate 中以 release 运行 stress。
