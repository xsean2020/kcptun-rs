# goruntime 共享 work-signal 丢失唤醒：nested-spawn 死锁 + timer 批量挂起（fixed）

## Status

**Fixed** (2026-08-13) — 根因是 Phase 4 调度器的共享 `WorkSignal` 丢失唤醒：`push_wakeq`
把被唤醒 goroutine 推入**归属者 P 的 wakeq**，但 `notify_one()` 唤醒的是**共享** global
signal 上的任意等待线程。忙线程可能在 pop→wait 循环中抢走全部许可，归属者 P 一直沉睡，
其 wakeq 中的 goroutine 永不运行 → 挂死。

修复：每 P 独立 `WorkSignal`，`push_wakeq` 精确唤醒归属者线程（`core/processor.rs`、
`task/block_on.rs`）。同步修复 `JoinHandle::poll` 的 TOCTOU（DEAD 检查与 waker 注册
不原子，`task/spawn.rs`）。

## Symptom（修复前）

两个独立症状，同一根因：

1. **nested-spawn 死锁** — `scheduler_hardening.rs::nested_spawn_closure_depth_8`
   隔离复现 **1/20**（30s 看门狗 → 30.01s 失败）。
2. **timer 批量挂起** — `time_smoke.rs::many_timers_all_wake`（50 个 spawn + sleep）
   **~55%** 挂起。计时线程触发所有计时器后堆空，但被唤醒的 goroutine 从未运行。

## 根因（确认）

- 计时线程在 `Timers::run` 的 `Condvar::wait`（空堆分支）——所有计时器都已触发，但
  某个 goroutine 的唤醒被丢：被唤醒 goroutine 入队归属者 wakeq，归属者线程未醒。
- `Processor::push_wakeq` 用 `self.global.signal().notify_one()`（共享 signal）。
  `schedule_loop` 所有线程都在同一 signal 上 `wait()`。`notify_one` 唤醒任意一个线程，
  它 pop **自己的** 队列（空），重新入睡；归属者 P 未醒 → wakeq 无人处理。
- 大批准时器到期时：计时线程连发 ~40 次 notify，忙线程抢走全部许可，归属者被饿死。

## 修复

1. **每-P signal**（`core/processor.rs`）：`Processor` 增加 `signal: WorkSignal`；
   `push_wakeq` 改 `self.signal.notify_one()`（精确唤醒归属者）。`schedule_loop` 改等
   `p.signal.wait()`；`block_on` 关闭时对池内所有 P `signal.shutdown()`。
   移除不再使用的 `Processor.global` 字段。
2. **JoinHandle TOCTOU**（`task/spawn.rs`）：DEAD 检查与 waker 注册放入同一把
   `join_wakers` 锁（`finish_g` 在同一锁下 drain），消除"检查与注册之间完成"的丢失唤醒。

## 验证

- `many_timers_all_wake`：修复前 ~55% 挂起 → 修复后 **15/15** 通过。
- `nested_spawn_closure_depth_8`：修复前 1/20 → 修复后 **20/20** 通过。
- `cargo test -p goruntime --all-targets`：全部通过（51 测试）。
