# goruntime — 跨平台 Go 风格轻量级异步 I/O 运行时实现计划

> **给执行代理（agentic workers）：** 必选子技能：使用 superpowers:subagent-driven-development（推荐）或 superpowers:executing-plans 按任务逐项实施本计划。步骤使用复选框（`- [ ]`）语法跟踪进度。

**目标:** 构建 `goruntime`——一个 Rust 的轻量级 Go 风格异步 I/O 运行时（栈式 goroutine + Go 风格 channel 驱动 Rust Future），对外暴露 tokio 兼容的异步接口。本运行时**只用标准库异步原语（`std::future`/`std::task`）从零实现，不依赖、也不调用 tokio/smol 的异步功能**——它本身就是要做性能更好的异步 I/O。核心目的是消除 kcptun-rs 在 tokio 后端下观测到的 P999 长尾延迟；并且它必须**跨平台**（Linux / macOS / Windows，覆盖 corosensei 支持的全部架构），不锁定在某个平台上。

**架构:** G/M/P 调度器。**G** = 一个栈式协程（fiber，自带独立栈），通过轮询循环驱动一个 Rust `Future`；**M** = 一个运行调度循环的 OS 线程；**P** = 每线程调度状态（Go 风格无锁 runq 环形队列 + `runnext`、每-P 的"归属唤醒队列" wakeq、调度器上下文、当前 G 槽位）。上下文切换被抽象在 `Fiber`/`Park` trait 之后：**corosensei** 是默认的跨平台后端；`feature = "native-asm"` 可选地切换到手写的 x86_64/aarch64 汇编后端。Go 风格的 `hchan` channel 提供 MPMC 通信；计时轮驱动 `sleep`/`timeout`；基于 `mio` 的 reactor（epoll / kqueue / IOCP / io_uring）驱动 socket I/O。`kio-rs` 新增 `goroutine` feature，使 kcptun-rs 无需改动业务代码即可运行在本运行时上；另有一个基准测试对比 tokio 的 p50/p99/p999。实现全程只使用 `std::future`/`std::task` 等标准库异步原语——调度器、channel、计时器、reactor、I/O 全部从零构建，**不使用 tokio/smol 等任何第三方异步运行时**。

由于 corosensei 协程是 `!Send` 的，并且使用线程局部状态（用于栈增长与异常展开），**goroutine 在首次运行后被钉扎（pin）到其归属 OS 线程上**：工作窃取只作用于从未启动过的 goroutine，被唤醒的 goroutine 会路由回其归属者。Go 式的自由跨线程 goroutine 迁移被有意不实现（记录于"范围之外"一节）。

本运行时**取代**当前未跟踪、无法编译的 `goroutine/` 骨架代码。骨架的模块布局（`core/`、`sched/`、`chan/`、`sync/`）在合理处保留；其中每个文件都将被重写。

**技术栈:** Rust（edition 2021，resolver 2，workspace 成员）。异步原语只用 `std`（`std::future`/`std::task`），运行时无任何 tokio/smol 依赖。`corosensei`（默认栈式协程后端；内置保护页 + 栈增长 + panic 传播）。可选的手写上下文切换汇编（x86_64 + aarch64），通过同一 trait 接入，`feature = "native-asm"`。`mio`（I/O 阶段，跨平台事件循环）。手写自旋锁。线程/计时只用 `std`。

---

## 全局约束

以下每项任务都隐式包含本节内容。

1. **跨平台是硬性要求** —— 本运行时必须在 **Linux、macOS 和 Windows** 上构建并运行，覆盖上下文切换后端支持的所有架构。默认构建中不得出现平台锁定的原语：
   - 无手写汇编（后端为 corosensei，见第 3 条）。
   - 无 `libc::mprotect` 保护页（`DefaultStack` 通过 OS API 提供）。
   - 无直接调用 `epoll`/`kqueue`/`select`（reactor 使用 `mio`，它抽象了 epoll / kqueue / io_uring / IOCP）。
   - 线程、计时、原子操作均来自 `std`。
   - 平台支持矩阵（任务 1.4）：corosensei 覆盖 x86_64（ELF/Darwin/Windows）、aarch64（ELF/Darwin）、x86（ELF）、RISC-V / LoongArch64 / PowerPC64（ELF）。默认构建恰好继承该矩阵。`feature = "native-asm"` 是例外：仅支持 x86_64/aarch64，在其他目标上触发 `compile_error!`。
2. **工作区门禁（gate）** —— 每个阶段结束时：`cargo build --workspace` 成功，且 `make gate`（`cargo fmt --all -- --check` + `cargo test --workspace` + `cargo clippy --workspace -- -D warnings`）通过。仅在门禁干净时提交。
3. **纯标准库异步实现（不用 tokio/smol）** —— `goroutine` 是 workspace 成员（阶段 1 加入根 `Cargo.toml` 的 `[workspace] members`），且不得依赖 `kio`（集成方向相反，在阶段 9）。本运行时**只用 `std` 异步原语**从零实现（`std::future::Future`、`std::task::{Waker, RawWaker, RawWakerVTable, Context, Poll}`、`std::pin::Pin`）：调度器、channel、计时器、reactor、I/O 全部构建在 `std` 之上，**绝不使用 tokio/smol 或任何第三方执行器/异步运行时**。运行时依赖仅限基础设施：`corosensei`（上下文切换）、`mio`（跨平台事件循环，阶段 8）、`libc`（仅 `native-asm`）。`tokio` 只允许出现在 dev-dependencies，仅用于"签名一致性"对比测试；运行时路径上**禁止**出现任何 `tokio::`/`smol::` 调用（一律以 `#[cfg(test)]`/dev 特性门控，永不进入发布构建）。
4. **`unsafe` 隔离** —— 默认构建中，`unsafe` 只出现在：`src/core/fiber.rs`（corosensei 胶水代码 + waker vtable）和原始指针 `GoroutineRef`。在 `feature = "native-asm"` 下，`unsafe` 额外出现在 `src/core/context.rs` 与 `src/core/stack.rs`（手写后端）。调度器、channel、计时器、I/O 代码必须是 100% safe Rust。每个 `unsafe` 块都要带 `// SAFETY:` 注释说明不变量。
5. **混合上下文切换策略** —— `Fiber`/`Park` 这一对 trait 是接缝。corosensei 是默认后端（跨平台）。`feature = "native-asm"` 把手写 x86_64/aarch64 汇编作为零依赖选项接入同一 trait；它必须通过与 corosensei 后端相同的往返测试和驱动测试（任务 1.3/1.4 门禁）。
6. **goroutine 钉扎（pinning）** —— goroutine 一旦启动，即被其 OS 线程（即它的 M）拥有：首次运行时设置 `owner` 字段；被唤醒的 goroutine 路由到其归属者的唤醒队列；工作窃取只窃取 `owner == NO_OWNER`（从未启动）的 goroutine。这是与 corosensei 线程局部状态兼容所必需的，并且有意比 Go 的自由迁移更严格。
7. **在文档化处对齐 Go 语义** —— Go 的 `runnext`（LIFO）、runq 环形队列（256 项，head/tail 原子）、`runqgrab`（窃取一半）、`goschedImpl`（让出 → 重新入队自身）、`hchan` 算法（sendq/recvq + buf、直接交付、close 清空等待者）都是参照。当 tokio 的 API 与 Go 不同时（例如向已关闭 channel 发送返回 `Err` 而非 panic；`recv` 返回 `Err` 而非零值），以 tokio 兼容行为为准。
8. **tokio 接口兼容** —— 异步表面（`spawn`、`JoinHandle`、`sleep`、`timeout`、`mpsc`、`oneshot`、`watch`、`Mutex`、`Notify`、`yield_now`、`select!`）的签名与 tokio 完全一致（见下方 API 契约）。`kio::*` 是集成接缝（阶段 9）。
9. **不做投机性扩展** —— 除非某任务明确添加，否则不实现 `spawn_blocking`、`interval`、`JoinHandle::abort`、`run_until_idle`、`select!` 的 `biased`/`else` 分支。
10. **驱动契约** —— goroutine 只能由归属 M 的调度循环*恢复*；waker 只能*入队*（绝不可触碰寄存器/栈）。任何线程不得写另一线程的 fiber 栈。
11. **输出纪律** —— 测试/基准输出按项目 CLAUDE.md 第 7 节只过滤显示失败。
12. **性能是本运行时的存在理由** —— 本运行时就是为提供比 tokio 性能更好的异步 I/O 而存在（更低的长尾延迟 + 更高吞吐）。阶段 9 的基准（p50/p99/p999 对比 tokio）是验收标准而非可选项；任何阶段不得引入会让运行时退化为 "tokio 再包装" 或与 tokio 性能重叠的设计。

---

## 公共 API 契约（一次性定义，各阶段引用）

```rust
// ── task / spawn（阶段 2–4）─────────────────────────────────────────────
pub fn spawn<F, T>(future: F) -> JoinHandle<T>
    where F: Future<Output = T> + Send + 'static, T: Send + 'static;
pub fn block_on<F, T>(future: F) -> T
    where F: Future<Output = T> + Send + 'static, T: Send + 'static;
pub fn yield_now() -> YieldNow;                    // impl Future<Output = ()>，让出一次
pub struct JoinHandle<T>;                          // impl Future<Output = Result<T, JoinError>>
#[derive(Debug)] pub struct JoinError;             // "任务被取消" — 为将来的 abort 保留

// ── time（阶段 6）────────────────────────────────────────────────────────
pub fn sleep(dur: Duration) -> Sleep;              // impl Future<Output = ()>
pub fn sleep_until(deadline: Instant) -> Sleep;
pub fn timeout<F: Future>(dur: Duration, fut: F) -> Timeout<F>
    where F: Future + Send + 'static, F::Output: Send;   // impl Future<Output = Result<F::Output, Elapsed>>
#[derive(Debug)] pub struct Elapsed;

// ── Go 风格 channel（阶段 5）—— MPMC ─────────────────────────────────────
pub fn channel<T>() -> (Sender<T>, Receiver<T>);       // 无界
pub fn bounded<T>(cap: usize) -> (Sender<T>, Receiver<T>); // cap == 0 => 无缓冲汇合（rendezvous）
pub struct Sender<T>;      // Clone; async send(&self, T) -> Result<(), SendError<T>>; try_send; close(); is_closed()
pub struct Receiver<T>;    // Clone; async recv(&self) -> Result<T, RecvError>; try_recv; close(); is_closed()
pub enum SendError<T> { Closed(T) }
pub enum TrySendError<T> { Full(T), Closed(T) }
pub enum RecvError { Closed }
pub enum TryRecvError { Empty, Closed }

// ── tokio 兼容同步原语（阶段 7）──────────────────────────────────────────
pub mod mpsc {
    pub fn channel<T>(capacity: usize) -> (Sender<T>, Receiver<T>);
    pub fn unbounded_channel<T>() -> (UnboundedSender<T>, UnboundedReceiver<T>);
    // Sender:   async send(&self, T) -> Result<(), SendError<T>>; try_send -> Result<(), TrySendError<T>>
    // Receiver: async recv(&mut self) -> Option<T>; try_recv -> Result<T, TryRecvError>
}
pub mod oneshot {
    pub fn channel<T>() -> (Sender<T>, Receiver<T>);
    // Sender:   async send(self, T) -> Result<(), T>;  is_closed() -> bool
    // Receiver: async recv(&mut self) -> Result<T, RecvError>
}
pub mod watch {
    pub fn channel<T: Clone + Send>(init: T) -> (Sender<T>, Receiver<T>);
    // Sender:   async send(&self, T) -> Result<(), SendError<T>>;  has_changed()
    // Receiver: async changed(&mut self) -> Result<(), RecvError>;  borrow_and_update() -> Ref<'_, T>
}
pub struct Mutex<T>;            // async lock(&self) -> MutexGuard<'_, T>; try_lock() -> Option<MutexGuard>
pub struct Notify;              // async notified(&self) -> Notified;  notify_one();  notify_waiters()
macro_rules! select { ... }     // tokio 风格：select! { $pat = $fut => $body ... }（v1 无 biased/else）

// ── 运行时句柄（阶段 3+）────────────────────────────────────────────────
pub struct Runtime;             // Runtime::new() -> std::io::Result<Runtime>; rt.block_on(fut);
                                // 以及进程级默认运行时，供自由函数 spawn/block_on 使用
```

阶段 9 增加 kio-rs 的 `goroutine` 后端。届时 `kio::task::{spawn_task, block_on, yield_now, cpu_block}`、`kio::time::{sleep, timeout}`、`kio::sync::{Mutex, Notify, bounded}`、`kio::net::*` 全部接到 goroutine。

**内部（跨任务共享）类型：**
```rust
// core/fiber.rs —— 上下文切换接缝（混合策略）
pub trait Fiber { fn resume(&mut self) -> FiberOutcome; }        // Yield | Return
pub trait Park { fn park(&self); }                               // 从内部挂起
pub enum FiberOutcome { Yield, Return }
pub type BoxedFiber = Box<dyn Fiber + Send>;                     // 由 cfg 选择（corosensei | native-asm）
pub fn new_fiber(gptr: *mut Goroutine,
                 entry: Box<dyn FnOnce(&mut Goroutine, &dyn Park) + Send>) -> BoxedFiber;

// core/goroutine.rs
pub const NO_OWNER: usize = usize::MAX;
pub struct Goroutine {
    fiber: Option<BoxedFiber>,      // 在 Goroutine::new 期间设置（通过 Box 解决先有鸡还是先有蛋）
    state: AtomicU8,                // G_WAITING/G_RUNNABLE/G_RUNNING/G_DEAD
    notified: AtomicBool,           // G_RUNNING 期间有唤醒到达
    yield_requested: AtomicBool,    // yield_now 请求驱动执行 gosched + 挂起
    owner: AtomicUsize,             // 最后运行我们的 P 的 id；NO_OWNER = 从未启动
    output: OnceLock<Box<dyn Any + Send>>,
    join_wakers: SpinLock<Vec<Waker>>,
    refs: AtomicUsize, id: u64,
}
pub struct GoroutineRef(NonNull<Goroutine>);  // Clone=+ref, Drop=-ref, get()=&G, get_mut()=&mut G
pub const G_WAITING: u8 = 0;  pub const G_RUNNABLE: u8 = 1;  pub const G_RUNNING: u8 = 2;  pub const G_DEAD: u8 = 3;

// core/processor.rs —— 每 P 双队列（钉扎安全）
pub struct Processor {
    pub id: usize,
    pub sched_ctx: Context,                       // 仅 native-asm 后端使用；corosensei 不使用
    pub runq: Runq,                               // 新鲜 goroutine（可被窃取）；Go 风格环形 + runnext
    pub wakeq: SpinLock<VecDeque<GoroutineRef>>,  // 被唤醒的 goroutine（仅归属者，不可窃取）
    pub current: Option<GoroutineRef>,
    pub work_signal: Notify,                      // 跨平台 std 唤醒空闲 M
}

// core/driver.rs —— 共享轮询循环，参数化为 `&dyn Park`
pub fn run_future<F, T>(g: &mut Goroutine, future: F, park: &dyn Park) -> T;
pub fn park_after_pending(g: &mut Goroutine, park: &dyn Park);
pub fn requeue_self(g: &mut Goroutine);           // gosched：state→RUNNABLE，推入自身 wakeq
pub fn schedule(g: &Goroutine);                   // waker 入口：路由到归属者 wakeq（若 NO_OWNER 则全局）
```

---

## 阶段 1 — 跨平台 fiber 原语

可工作、可测试的交付物：本 crate 作为 workspace 成员成功构建；一个裸 fiber 可以 `resume` 进入、`park`（挂起）、再被恢复——**两种后端**都要验证通过（默认 corosensei，`feature = "native-asm"` 下为手写汇编）。

### 任务 1.1: 脚手架：crate 作为 workspace 成员

**文件：**
- 替换：`goroutine/Cargo.toml`
- 替换：`goroutine/src/lib.rs`
- 修改：`Cargo.toml`（根，添加成员）
- 删除：`Cargo_goruntime.toml`（根——已被取代）

**接口：**
- 消费：无。
- 产出：`goroutine` crate，库名 `goruntime`，模块 `core`/`sched`/`chan`/`sync`/`task`（暂为空），workspace 成员身份。

- [ ] **第 1 步：替换 `goroutine/Cargo.toml`**

```toml
[package]
name = "goruntime"
version = "0.1.0"
edition = "2021"
description = "Cross-platform lightweight Go-style async runtime for Rust (G/M/P scheduler + Go channels), tokio-interface-compatible"

[lib]
name = "goruntime"

[dependencies]
corosensei = "0.2"                  # 默认跨平台栈式协程后端（保护页 + 增长 + panic 传播）

[features]
default = []
# 可选的手写 x86_64/aarch64 上下文切换，用于零依赖构建。
# 在不支持的目标上触发 compile_error!（任务 1.4）。
native-asm = ["dep:libc"]
libc = { version = "0.2", optional = true }

[dev-dependencies]
# 仅用于阶段 7 的"签名一致性"对比测试（验证我们的 API 与 tokio 一致）。
# 运行时实现中严禁调用 tokio/smol 的任何异步功能。
tokio = { version = "1", features = ["sync", "time", "macros", "rt"] }
```

- [ ] **第 2 步：替换 `src/lib.rs`** 为空的模块骨架（删除旧的 `RUNTIME`/`spawn`/`go`——全部作废）：

```rust
//! Cross-platform lightweight Go-style async runtime for Rust.
//!
//! G/M/P scheduler (stackful goroutines driving Rust futures), Go-style
//! channels, timers, and tokio-compatible async interfaces.
//! Implemented on std async primitives only (std::future / std::task) —
//! NO tokio/smol or any third-party executor is used or depended on.
//! See `docs/superpowers/plans/2026-08-12-goruntime-async-runtime.md`.

#![allow(clippy::missing_safety_doc)]

pub mod chan;
pub mod core;
pub mod sched;
pub mod sync;

pub mod task; // added in Phase 3 (spawn/block_on/JoinHandle)

pub mod prelude {
    pub use crate::chan::{bounded, channel};
    pub use crate::core::*;
    pub use crate::task::{block_on, spawn, yield_now};
}
```

- [ ] **第 3 步：创建空的 `src/{core,sched,chan,sync,task}/mod.rs`**——每个文件一行 `//! …` 文档注释。删除旧骨架文件（`goroutine.rs`、`context.rs`、`stack.rs`、`processor.rs`、`scheduler.rs`、`work_steal.rs`、`global_queue.rs`、`channel.rs`、`waiter.rs`、`select.rs`、`mutex.rs`）。原因：旧骨架内部引用了 tokio（如 `wrap_async` 在每个 goroutine 里创建 tokio runtime、`select.rs` 调用 `tokio::task::yield_now`、示例用 `tokio::time`）——正是本计划明令禁止的反模式，必须整体替换为纯 `std` 实现。

- [ ] **第 4 步：把 `goroutine` 加入根 workspace members**，并 `rm -f Cargo_goruntime.toml`。

- [ ] **第 5 步：验证门禁** —— 运行 `cargo build --workspace 2>&1 | tail -5`，然后 `make gate`
预期：构建成功；门禁通过。

- [ ] **第 6 步：提交**
```bash
git add Cargo.toml goruntime docs/superpowers/plans/2026-08-12-goruntime-async-runtime.md
rm -f Cargo_goruntime.toml
git commit -m "feat(goruntime): scaffold cross-platform async runtime crate as workspace member"
```

### 任务 1.2: `Fiber`/`Park` trait + corosensei 后端（默认，跨平台）

**文件：**
- 创建：`goroutine/src/core/fiber.rs`
- 修改：`goroutine/src/core/mod.rs`（导出 `fiber`）

**接口：**
- 消费：暂无（Goroutine 指针以原始 `*mut Goroutine` 传入；`Goroutine` 在任务 2.1 定义——任务 1.2 的 fiber 闭包体是测试闭包，而非驱动）。
- 产出：
  ```rust
  pub trait Fiber { fn resume(&mut self) -> FiberOutcome; }
  pub trait Park { fn park(&self); }
  pub enum FiberOutcome { Yield, Return }
  pub fn goroutine_stack_size() -> usize;   // 默认 256 KiB
  pub struct CsFiber { coro: Coroutine<(), (), (), DefaultStack> }   // corosensei 后端
  impl CsFiber { pub fn new(gptr: *mut Goroutine, entry: …) -> CsFiber; }
  impl Fiber for CsFiber;
  ```

- [ ] **第 1 步：编写失败测试** —— 进入 fiber、挂起两次、完成：

```rust
// tests/fiber_smoke.rs
use goruntime::core::{CsFiber, Fiber, FiberOutcome};
use std::sync::atomic::{AtomicUsize, Ordering};

#[test]
fn fiber_suspend_resume_roundtrip() {
    let count = std::sync::Arc::new(AtomicUsize::new(0));
    let c1 = count.clone();
    // 入口接收一个 Park；调用 park() 会挂起 fiber。
    let mut fiber = CsFiber::new_entry(move |park| {
        c1.fetch_add(1, Ordering::SeqCst);
        park.park();                       // 挂起 #1
        c1.fetch_add(10, Ordering::SeqCst);
        park.park();                       // 挂起 #2
        c1.fetch_add(100, Ordering::SeqCst);
    });
    assert!(matches!(fiber.resume(), FiberOutcome::Yield));
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert!(matches!(fiber.resume(), FiberOutcome::Yield));
    assert_eq!(count.load(Ordering::SeqCst), 11);
    assert!(matches!(fiber.resume(), FiberOutcome::Return));
    assert_eq!(count.load(Ordering::SeqCst), 111);
}
```

- [ ] **第 2 步：运行测试确认失败** —— `cargo test -p goroutine fiber_suspend_resume_roundtrip`
预期：FAIL —— `CsFiber` 未定义。

- [ ] **第 3 步：实现 `fiber.rs`**

```rust
//! The context-switch seam. A `Fiber` is one suspendable unit of execution
//! (the stackful coroutine under a goroutine); `Park` is how the future driver
//! suspends it from inside. The default backend is corosensei (cross-platform);
//! `feature = "native-asm"` swaps in hand-rolled x86_64/aarch64 assembly.

use corosensei::{Coroutine, CoroutineResult, DefaultStack, Yielder};

pub trait Fiber {
    /// Enter or re-enter the fiber. `Yield` = it parked itself (goroutine
    /// suspended, waiting for a wake); `Return` = its entry completed.
    fn resume(&mut self) -> FiberOutcome;
}
pub trait Park { fn park(&self); }
pub enum FiberOutcome { Yield, Return }

pub const DEFAULT_STACK_SIZE: usize = 256 * 1024;
pub fn goroutine_stack_size() -> usize { DEFAULT_STACK_SIZE }

// ─── Backend 1 (default): corosensei ────────────────────────────────────────
pub struct CsFiber {
    coro: Coroutine<(), (), (), DefaultStack>,
}

/// `Park` that suspends the running corosensei coroutine back to its resumer.
struct CsPark<'a>(&'a Yielder<'_, (), ()>);
impl Park for CsPark<'_> {
    fn park(&self) {
        // Yielder::suspend(val) -> Input: switches control to the last caller
        // of Coroutine::resume, then returns with the next resume's input.
        self.0.suspend(());
    }
}

impl CsFiber {
    /// `entry` is `Box<dyn FnOnce(&mut Goroutine, &dyn Park) + Send>` in the
    /// real runtime (Task 2.1); Task 1.2 uses a lighter `FnOnce(&dyn Park)`.
    pub fn new_entry<F>(entry: F) -> CsFiber
    where F: FnOnce(&dyn Park) + Send + 'static {
        // DefaultStack is allocated with a guard page via OS APIs and grows on
        // demand (signal-based) — cross-platform, no libc calls of our own.
        let stack = DefaultStack::with_size(goroutine_stack_size())
            .expect("failed to allocate coroutine stack");
        let coro = Coroutine::with_stack(stack, move |yielder, ()| {
            let park = CsPark(yielder);
            entry(&park);
        });
        CsFiber { coro }
    }
}

impl Fiber for CsFiber {
    fn resume(&mut self) -> FiberOutcome {
        match self.coro.resume(()) {
            CoroutineResult::Yield(_) => FiberOutcome::Yield,
            CoroutineResult::Return(()) => FiberOutcome::Return,
        }
    }
}
```
> 说明：`Coroutine::with_stack(stack, f)`，其中 `f: FnOnce(&Yielder<Input, Yield>, Input) -> Return`。此处 `Input = Yield = Return = ()`。协程内的 panic 会穿过 `resume()` 传播回调用者（corosensei 的 `unwind` 特性）——运行时在入口外包一层 `catch_unwind`（任务 2.1），使用户 panic 无法破坏调度器。实施时请对照锁定的 corosensei 版本确认 `DefaultStack::with_size` / `StackOptions` 的确切构造方式；其语义（保护页 + 增长）由 `default-stack` 特性保证。

- [ ] **第 4 步：运行测试确认通过**
运行：`cargo test -p goroutine fiber_suspend_resume_roundtrip`
预期：PASS。连跑 3 次——这里的挂起/恢复若不稳定，说明后端契约有误。

- [ ] **第 5 步：提交**
```bash
git add goroutine && git commit -m "feat(goruntime): Fiber/Park trait + corosensei cross-platform backend"
```

### 任务 1.3: `native-asm` 后端（x86_64 + aarch64），接入同一 trait

**文件：**
- 创建：`goroutine/src/core/context.rs`（x86_64）、`goroutine/src/core/context_aarch64.rs`（aarch64）
- 创建：`goroutine/src/core/stack.rs`（native 后端的保护页栈）
- 创建：`goroutine/src/core/fiber_native.rs`
- 修改：`goroutine/src/core/fiber.rs`（cfg 门控的 `new_fiber`）、`Cargo.toml`（特性接线）

**接口：**
- 消费：`Fiber`/`Park`/`FiberOutcome`（任务 1.2）。
- 产出：
  ```rust
  pub struct NativeFiber { stack: Stack, ctx: Context, entry: Option<Box<dyn FnOnce(&dyn Park) + Send>>, alive: bool }
  impl Fiber for NativeFiber;   // 与 CsFiber 相同的 FiberOutcome 语义
  pub fn new_fiber<F: FnOnce(&dyn Park) + Send + 'static>(entry: F) -> BoxedFiber;  // cfg 选择
  ```
  选择规则：`#[cfg(feature = "native-asm")]` → `NativeFiber`（若非 x86_64/aarch64 则 `compile_error!`）；否则 `CsFiber`。

- [ ] **第 1 步：编写失败测试** —— 与任务 1.2 完全相同的往返测试，针对 native 后端运行：

```rust
// tests/fiber_native.rs  —— 断言与任务 1.2 相同，特性门控：
#![cfg(feature = "native-asm")]
#[test] fn native_fiber_suspend_resume_roundtrip() { /* 主体完全相同 */ }
```

- [ ] **第 2 步：运行确认失败** —— `cargo test -p goroutine --features native-asm native_fiber_suspend_resume_roundtrip`
预期：FAIL —— `NativeFiber` 未定义。（本机为 x86_64，故执行 x86_64 路径。）

- [ ] **第 3 步：实现 `stack.rs`（保护页栈，仅 native 后端）**

布局同原计划的 1.2 任务（通过 `libc::mprotect` 实现保护页，`top` 16 字节对齐），以 `#[cfg(feature = "native-asm")]` 和 `#[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]` 门控。由于 `native-asm` 特性在非 Unix 目标上已 `compile_error!`（第 5 步），这里引用 `libc` 是安全的。

- [ ] **第 4 步：实现 `context.rs`（x86_64）—— 红区安全的切换**

汇编与原计划 1.3 任务完全相同：在 128 字节红区之下保存被调用者保存寄存器，从 `switch` 调用的返回地址捕获 `rip`，恢复后 `jmpq *to_rip`；新上下文的布局保证首次进入跳板函数时 `rsp ≡ 8 (mod 16)`。提供 `Context::{empty, initial(stack_top, trampoline), switch(from, to)}`。

- [ ] **第 5 步：实现 `fiber_native.rs`** —— 跳板函数 + `NativeFiber::resume`：

```rust
pub struct NativeFiber {
    stack: Stack,
    ctx: Context,
    entry: Option<Box<dyn FnOnce(&dyn Park) + Send>>,
    alive: bool,          // 跳板函数在 Return 后切换回来时为 false
}

thread_local! { static NATIVE_PARK: std::cell::Cell<*mut ()> = const { std::cell::Cell::new(std::ptr::null_mut()) }; }

impl NativeFiber {
    pub fn new<F>(entry: F) -> NativeFiber
    where F: FnOnce(&dyn Park) + Send + 'static {
        let stack = Stack::new(goroutine_stack_size());
        let ctx = Context::initial(stack.top(), Self::trampoline);
        NativeFiber { stack, ctx, entry: Some(Box::new(entry)), alive: true }
    }

    extern "C" fn trampoline() {
        // 运行在 fiber 自身的栈上，由上下文切换进入。
        // 入口指针通过 resume 前设置的线程局部变量找到。
        let raw = NATIVE_PARK.with(|c| c.get());
        // SAFETY: resume() 调用者把 NATIVE_PARK 设为一个 boxed（FnOnce）入口
        // + park 指针，其生命周期覆盖 fiber 的本次运行。
        let (entry, park) = unsafe { Box::from_raw(raw as *mut (&'static mut Box<dyn FnOnce(&dyn Park) + Send>, *mut dyn Park)) };
        (entry)(unsafe { &*park });
        // 从 resume 返回：fiber 已完成。
    }

    /// fiber 内部使用的 Park：切换回恢复我们的人。
    struct NativePark { sched_ctx: *mut Context, self_ctx: *mut Context }
    impl Park for NativePark {
        fn park(&self) {
            // SAFETY: 两个 context 分别归运行中的 M 与挂起的 fiber 所有；
            // 我们持有唯一的执行引用。
            unsafe { crate::core::context::Context::switch(&mut *self.self_ctx, &*self.sched_ctx) }
        }
    }
}

impl Fiber for NativeFiber {
    fn resume(&mut self) -> FiberOutcome {
        // M 的调度器上下文会跨越 fiber 运行被保存。
        let sched_ctx = &mut crate::core::current_p().sched_ctx;
        let self_ctx = &mut self.ctx;
        // SAFETY: park + entry 在切换前被移到 fiber 自身的栈上。
        if self.alive {
            self.alive = false;  // 跳板函数通过标志置真…见第 6 步说明
            unsafe { Context::switch(sched_ctx, self_ctx) };
            FiberOutcome::Yield
        } else {
            unsafe { Context::switch(sched_ctx, self_ctx) };
            FiberOutcome::Return
        }
    }
}
```
> **正确性说明（第 6 步）：** `resume` 必须区分"fiber 已挂起"与"fiber 已完成"。最干净的做法：跳板函数在最后一次切回之前设置 `done: bool` 标志，`resume` 在每次切换返回后读取该标志。不要采用上面草图里的 `alive`；应保留 `done: AtomicBool`（或仅由 fiber 自身跳板函数修改的普通 `bool`，因为只有一个线程触碰它），并把 `resume` 实现为：
> ```rust
> fn resume(&mut self) -> FiberOutcome {
>     self.done = false;                       // 即将进入
>     unsafe { Context::switch(&mut current_p().sched_ctx, &mut self.ctx) };
>     if self.done { FiberOutcome::Return } else { FiberOutcome::Yield }
> }
> ```
> 跳板函数在最后一次切换前执行 `self.done = true;`。这是两个后端在 `Fiber` 语义上唯一不同之处——保持语义一致。

- [ ] **第 6 步：运行确认通过** —— `cargo test -p goroutine --features native-asm --test fiber_native`
预期：PASS。随后重跑 corosensei 往返测试（默认，无特性），确认两个后端满足同一契约。

- [ ] **第 7 步：加入平台守卫** —— 在 `lib.rs` 中：
```rust
#[cfg(all(feature = "native-asm", not(any(target_arch = "x86_64", target_arch = "aarch64"))))]
compile_error!("feature \"native-asm\" supports only x86_64 and aarch64; use the default corosensei backend on other targets");
```

- [ ] **第 8 步：提交**
```bash
git add goroutine && git commit -m "feat(goruntime): native-asm backend (x86_64/aarch64) behind Fiber/Park trait"
```

### 任务 1.4: 平台支持矩阵 + CI 验证

**文件：**
- 创建：`goroutine/PLATFORM.md`
- 修改：根 CI（如存在：`.github/workflows/*.yml`）或文档化一组 `cargo test --target` 命令

- [ ] **第 1 步：编写 `PLATFORM.md`** —— 支持矩阵（继承自 corosensei 0.2.x）：

| 目标 OS | x86_64 | aarch64 | x86 | RISC-V | LoongArch64 | PowerPC64 |
|---|---|---|---|---|---|---|
| Linux / BSD | ✅（默认 + native-asm） | ✅（默认 + native-asm） | ✅（默认） | ✅（默认） | ✅（默认） | ✅（默认，corosensei ≥0.3） |
| macOS / iOS | ✅（默认 + native-asm） | ✅（默认 + native-asm） | — | — | — | — |
| Windows | ✅（默认） | ❌（corosensei） | ⚠️ | — | — | — |

文档中写明：默认 corosensei 后端定义了该矩阵；`native-asm` 仅支持 x86_64/aarch64；已知缺口（aarch64-windows、x86-macos）来自 corosensei，v1 接受。reactor（阶段 8）使用 `mio`（epoll/kqueue/io_uring/IOCP），覆盖以上全部。

- [ ] **第 2 步：添加 CI 目标** —— 对每个可达目标：`cargo check -p goruntime --target <t>`（或用 `cross check`）。本机最小集合：`x86_64-apple-darwin`（宿主）。文档化 Linux/Windows/aarch64 的 CI 命令。
- [ ] **第 3 步：运行门禁** —— 宿主上执行 `make gate`。
- [ ] **第 4 步：提交**
```bash
git add goroutine/PLATFORM.md && git commit -m "docs(goruntime): platform support matrix + cross-platform CI targets"
```

**阶段 1 门禁：** `make gate` 通过；两种 fiber 后端都满足往返契约。

---

## 阶段 2 — Goroutine、waker、future 驱动

可工作、可测试的交付物：通过手写测试循环，单个 goroutine 能把一个 Rust future 跑完——包括先返回 `Pending`、挂起 fiber、再由 waker 唤醒重新轮询的 future。两种 fiber 后端下测试均通过。

### 任务 2.1: 自旋锁、Goroutine、引用计数、fiber 接线

**文件：**
- 创建：`goroutine/src/core/spinlock.rs`
- 创建：`core/` 下的 `goroutine.rs`
- 修改：`goroutine.rs` 包装 `BoxedFiber` + `new_fiber`（任务 1.3）、`core/mod.rs`

**接口：**
- 消费：`Fiber`、`Park`、`new_fiber`（阶段 1）；`SpinLock`。
- 产出：`Goroutine`/`GoroutineRef`/`GState` 内部契约（见 API 契约）。`Goroutine::new(entry)`，其中 `entry: Box<dyn FnOnce(&mut Goroutine, &dyn Park) + Send>`。

- [ ] **第 1 步：编写失败测试** —— 自旋锁互斥 + 引用计数往返（同原计划 2.1 任务）。
- [ ] **第 2 步：运行确认失败** —— `cargo test -p goroutine spinlock_ goroutine_refcount`
预期：FAIL。

- [ ] **第 3 步：实现 `spinlock.rs`**（与原计划完全相同：CAS + 退避，16 次自旋后 `std::thread::yield_now`；不使用任何 OS 特定原语）。

- [ ] **第 4 步：实现 `goroutine.rs`**，采用先有鸡还是先有蛋安全的 fiber 构造：

```rust
use crate::core::{new_fiber, BoxedFiber, G_DEAD, G_RUNNABLE};
use std::any::Any;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::OnceLock;
use std::task::Waker;

pub const G_WAITING: u8 = 0;  pub const G_RUNNABLE: u8 = 1;
pub const G_RUNNING: u8 = 2;  pub const G_DEAD: u8 = 3;
pub const NO_OWNER: usize = usize::MAX;

static NEXT_GID: AtomicU64 = AtomicU64::new(1);

pub struct Goroutine {
    pub(crate) fiber: Option<BoxedFiber>,
    pub(crate) state: AtomicU8,
    pub(crate) notified: AtomicBool,
    pub(crate) yield_requested: AtomicBool,
    pub(crate) owner: AtomicUsize,
    pub(crate) output: OnceLock<Box<dyn Any + Send>>,
    pub(crate) join_wakers: SpinLock<Vec<Waker>>,
    pub(crate) refs: AtomicUsize,
    pub(crate) id: u64,
}

// SAFETY: a Goroutine is exclusively *executed* by one M at a time; all
// cross-thread access is through atomics. The fiber (corosensei Coroutine) is
// !Send but never moves — it stays at a fixed heap address.
unsafe impl Send for Goroutine {}
unsafe impl Sync for Goroutine {}

impl Goroutine {
    pub(crate) fn new(entry: Box<dyn FnOnce(&mut Goroutine, &dyn Park) + Send>) -> GoroutineRef {
        let mut g = Box::new(Goroutine {
            fiber: None,
            state: AtomicU8::new(G_RUNNABLE),
            notified: AtomicBool::new(false),
            yield_requested: AtomicBool::new(false),
            owner: AtomicUsize::new(NO_OWNER),
            output: OnceLock::new(),
            join_wakers: SpinLock::new(),
            refs: AtomicUsize::new(1),
            id: NEXT_GID.fetch_add(1, Ordering::Relaxed),
        });
        let gptr = &mut *g as *mut Goroutine;
        // SAFETY: gptr is heap-stable (Box). The fiber is constructed before the
        // Goroutine is shared, so it can only ever run while g is alive; the
        // returned GoroutineRef keeps g alive.
        g.fiber = Some(new_fiber(gptr, entry));
        let raw = Box::into_raw(g);
        // SAFETY: raw is non-null and uniquely owned here.
        unsafe { GoroutineRef::from_raw(raw) }
    }

    pub(crate) fn add_ref(&self) { self.refs.fetch_add(1, Ordering::Relaxed); }
    pub(crate) fn release(&self) {
        if self.refs.fetch_sub(1, Ordering::Release) == 1 {
            std::sync::atomic::fence(Ordering::Acquire);
            // SAFETY: refcount 0 means no other reference can exist; reclaim the
            // Box created in `new`.
            unsafe { drop(Box::from_raw(self as *const Goroutine as *mut Goroutine)); }
        }
    }
}

pub struct GoroutineRef(NonNull<Goroutine>);
impl Clone for GoroutineRef { fn clone(&self) -> Self { self.get().add_ref(); GoroutineRef(self.0) } }
impl GoroutineRef {
    pub fn from_raw(ptr: *mut Goroutine) -> GoroutineRef { GoroutineRef(NonNull::new_unchecked(ptr)) }
    pub fn owned(g: &Goroutine) -> GoroutineRef { g.add_ref(); GoroutineRef(NonNull::from(g)) }
    pub fn get(&self) -> &Goroutine { unsafe { self.0.as_ref() } }
    /// 供持有唯一执行引用的归属 M 使用的可变访问。
    /// SAFETY: 调用者必须是持有唯一执行引用的归属 M（调度循环或 fiber 自身的
    /// 跳板函数），绝不能与另一个 `get_mut` 并发。
    pub unsafe fn get_mut(&self) -> &mut Goroutine { unsafe { &mut *self.0.as_ptr() } }
}
impl Drop for GoroutineRef { fn drop(&mut self) { self.get().release(); } }
```

- [ ] **第 5 步：运行确认通过** —— `cargo test -p goroutine spinlock_ goroutine_refcount`
预期：PASS。
- [ ] **第 6 步：提交**
```bash
git add goroutine && git commit -m "feat(goruntime): Goroutine + refcount + fiber wiring"
```

### 任务 2.2: goroutine waker + park/schedule 状态机

**文件：**
- 创建：`goroutine/src/core/waker.rs`
- 修改：`goroutine/src/core/mod.rs`（线程局部 `current_g`/`current_p`，导出）

**接口：**
- 消费：`Goroutine`、`GoroutineRef`（任务 2.1）。
- 产出：
  ```rust
  pub fn goroutine_waker(g: &Goroutine) -> Waker;     // RawWaker 持有一个计数引用
  pub fn park_after_pending(g: &mut Goroutine, park: &dyn Park);  // 核心协议
  pub fn schedule(g: &Goroutine);                     // waker 入口 → 归属者 wakeq / 全局
  pub fn requeue_self(g: &mut Goroutine);             // gosched：state→RUNNABLE，推入自身 wakeq
  // 线程局部
  pub fn current_g() -> *mut Goroutine;  pub fn set_current_g(p: *mut Goroutine);
  pub fn current_p() -> &'static Processor;  pub fn try_current_p() -> Option<&'static Processor>;
  ```

- [ ] **第 1 步：编写失败测试** —— 用手动两步驱动练习协议（形状同原计划 2.2 任务，但 park 路径走一个只记录调用、不真正挂起的 `dyn Park` 桩，从而在无 fiber 的情况下可测试协议逻辑）：

```rust
struct CountingPark(Arc<AtomicUsize>);
impl Park for CountingPark { fn park(&self) { self.0.fetch_add(1, SeqCst); } }

#[test]
fn park_protocol_repolls_on_notified() {
    let g = Goroutine::new(Box::new(|_g, _park| {}));
    let parks = Arc::new(AtomicUsize::new(0));
    let park = CountingPark(parks.clone());
    let g = unsafe { g.get_mut() };
    // 模拟"poll 期间"到达的唤醒：notified=true，state=RUNNING。
    g.notified.store(true, SeqCst);
    g.state.store(G_RUNNING, SeqCst);
    park_after_pending(g, &park);
    assert_eq!(parks.load(SeqCst), 0, "notified ⇒ must re-poll, not park");
    assert_eq!(g.state.load(SeqCst), G_RUNNING, "must stay RUNNING for re-poll");

    // 模拟安静的 poll：必须挂起。
    g.notified.store(false, SeqCst);
    park_after_pending(g, &park);
    assert_eq!(parks.load(SeqCst), 1, "quiet poll ⇒ must park");
    assert_eq!(g.state.load(SeqCst), G_WAITING);
}

#[test]
fn schedule_wakes_waiting_and_sets_runnable() { /* schedule() 把 G_WAITING 的 G 转为 G_RUNNABLE 并入队 */ }
```

- [ ] **第 2 步：运行确认失败** —— `cargo test -p goroutine park_protocol_`
预期：FAIL。

- [ ] **第 3 步：实现 `waker.rs`**

```rust
use crate::core::{Goroutine, GoroutineRef, G_RUNNING, G_RUNNABLE, G_WAITING};
use crate::sched;
use std::sync::atomic::Ordering;
use std::task::{RawWaker, RawWakerVTable, Waker};

/// waker 入口。可在任意线程被调用（reactor 线程、另一个 M 或归属者）。
/// 只触碰原子量与运行队列——绝不触碰 fiber 寄存器。
pub fn schedule(g: &Goroutine) {
    loop {
        match g.state.load(Ordering::Acquire) {
            G_WAITING => {
                if g.state.compare_exchange(G_WAITING, G_RUNNABLE, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                    let owner = g.owner.load(Ordering::Acquire);
                    if owner == NO_OWNER { sched::enqueue_global(g); }  // 被唤醒的 G 不应出现此情况
                    else { sched::enqueue_owner(g, owner); }            // 归属者 wakeq + 唤醒归属者
                    return;
                }
            }
            G_RUNNING => { g.notified.store(true, Ordering::Release); return; }
            G_RUNNABLE => return,   // 已入队
            _ /* DEAD */ => return,
        }
    }
}

/// 驱动在 `poll` 返回 `Poll::Pending` 后立即调用。
pub fn park_after_pending(g: &mut Goroutine, park: &dyn Park) {
    // yield_now 请求了主动让出——重新入队自身，然后挂起。
    if g.yield_requested.swap(false, Ordering::AcqRel) {
        requeue_self(g);
        park.park();
        return;
    }
    if g.notified.swap(false, Ordering::AcqRel) {
        return; // 上次 poll 期间有唤醒到达——立即重新轮询
    }
    if g.state.compare_exchange(G_RUNNING, G_WAITING, Ordering::AcqRel, Ordering::Acquire).is_err() {
        return; // 状态已离开 RUNNING；重新轮询
    }
    // 已提交为 G_WAITING。若 schedule() 在窗口内把状态转成 G_RUNNABLE 并入队，
    // 此刻挂起仍然安全——队列会恢复我们。
    park.park();
}

/// gosched（Go）：把运行中的 goroutine 移到其归属者队列的队尾。
pub fn requeue_self(g: &mut Goroutine) {
    g.state.store(G_RUNNABLE, Ordering::Release);
    let p = crate::core::current_p();
    p.wakeq.lock().push_back(GoroutineRef::owned(g));
}
```

RawWaker vtable（结构与原计划相同：`waker_clone`/`waker_wake`/`waker_wake_by_ref`/`waker_drop`，各自持有一个计数 `GoroutineRef`；`wake` 调用 `schedule` 后释放引用）。在 `core/mod.rs` 中加入线程局部 `current_g`/`current_p`/`set_current_*`（`Processor` 类型在任务 3.1 出现；在此之前保留一个最小占位符，使 `park_after_pending`/`requeue_self` 可编译）。

- [ ] **第 4 步：运行确认通过** —— `cargo test -p goroutine park_protocol_ schedule_wakes_`
预期：PASS。
- [ ] **第 5 步：提交**
```bash
git add goroutine && git commit -m "feat(goruntime): waker + park/schedule state machine with yield_now flag"
```

### 任务 2.3: future 驱动 + 端到端 park/wake

**文件：**
- 创建：`goroutine/src/core/driver.rs`
- 修改：`goroutine/src/core/mod.rs`、`goroutine/src/sched/mod.rs`（测试用桩 `enqueue_global`/`enqueue_owner`）

**接口：**
- 消费：`park_after_pending`、`goroutine_waker`、`requeue_self`、`Goroutine`（任务 2.2）。
- 产出：
  ```rust
  pub fn run_future<F, T>(g: &mut Goroutine, future: F, park: &dyn Park) -> T
      where F: Future<Output = T> + Send + 'static, T: Send + 'static;
  ```
  以及等价于 `Goroutine::trampoline` 的完成路径：当 fiber 的入口返回时，置 `G_DEAD` 并唤醒 join 等待者。corosensei 下该逻辑位于协程闭包（任务 2.1 的构造处）；native-asm 下位于跳板函数。二者都必须运行同一个 `finish_g(g)` 辅助函数：
  ```rust
  pub fn finish_g(g: &mut Goroutine) {
      g.state.store(G_DEAD, Ordering::Release);
      for w in g.join_wakers.lock().drain(..) { w.wake(); }
  }
  ```

- [ ] **第 1 步：编写失败测试** —— 用手写单线程循环跑完完整驱动周期（形状同原计划 2.3 任务），但**跑两次**——一次用 corosensei fiber，一次在 `--features native-asm` 下——证明两个后端都能驱动"先挂起一次再完成"的 future：

```rust
// tests/driver_smoke.rs
use goruntime::core::*;
static PARKS: AtomicUsize = AtomicUsize::new(0);
struct YieldsOnce;
impl Future for YieldsOnce {
    type Output = u32;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<u32> {
        if PARKS.fetch_add(1, SeqCst) == 0 { cx.waker().wake_by_ref(); Poll::Pending }
        else { Poll::Ready(42) }
    }
}

#[test]
fn driver_runs_future_through_park_cycle() {
    let mut p = Processor::new(0);            // 任务 3.1 提供 Processor；见说明
    set_current_p(&mut p);
    let g = Goroutine::new(Box::new(|g, park| {
        let out = run_future(g, YieldsOnce, park);
        let _ = g.output.set(Box::new(out));
        finish_g(g);
    }));
    let mut guard = 0;
    while g.get().state.load(SeqCst) != G_DEAD && guard < 100 {
        guard += 1;
        let gptr = g.get() as *const Goroutine as *mut Goroutine;
        set_current_g(gptr);
        // SAFETY: 手写循环持有唯一执行引用。
        let g = unsafe { g.get_mut() };
        g.state.store(G_RUNNING, SeqCst);
        let outcome = g.fiber.as_mut().unwrap().resume();
        match outcome {
            FiberOutcome::Yield => { schedule(g); }   // waker 路径重新入队
            FiberOutcome::Return => {}
        }
    }
    assert_eq!(PARKS.load(SeqCst), 1);
    assert_eq!(*g.output().downcast_ref::<u32>().unwrap(), 42);
    assert!(g.state.load(SeqCst) == G_DEAD);
}
```
> 任务 2.2 的 `Processor` 占位符需提供 `id`/`wakeq`/`runq`，足以让 `requeue_self` 与 `schedule` 编译（任务 3.1 用完整的双队列 `Processor` 替换）。

- [ ] **第 2 步：运行确认失败** —— `cargo test -p goroutine --test driver_smoke`
预期：FAIL —— `run_future`/`finish_g` 缺失。

- [ ] **第 3 步：实现 `driver.rs`**

```rust
use crate::core::{goroutine_waker, park_after_pending, requeue_self, Goroutine, Park};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context as TaskCtx, Poll};

pub fn run_future<F, T>(g: &mut Goroutine, future: F, park: &dyn Park) -> T
where F: Future<Output = T> + Send + 'static, T: Send + 'static {
    let mut fut = Pin::from(Box::new(future));
    let waker = goroutine_waker(g);
    let mut cx = TaskCtx::from_waker(&waker);
    loop {
        // yield_now() 请求？gosched + 挂起，然后重新轮询。
        if g.yield_requested.swap(false, std::sync::atomic::Ordering::AcqRel) {
            requeue_self(g);
            park.park();
            continue;
        }
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(v) => return v,
            Poll::Pending => park_after_pending(g, park),
        }
    }
}
```
> corosensei 协程闭包（任务 2.1 构造处）与 native 跳板函数（任务 1.3）都用同一个 `&dyn Park` 调用入口，然后 `finish_g`。入口内的 panic 被捕获（corosensei 从 `resume` 展开；在 `entry` 外包 catch），从而使用户 panic 把 goroutine 标记为 DEAD 而非破坏调度器。

- [ ] **第 4 步：运行确认通过（两种后端）**
运行：`cargo test -p goroutine --test driver_smoke` 和 `cargo test -p goroutine --test driver_smoke --features native-asm`
预期：两次 PASS。

- [ ] **第 5 步：提交**
```bash
git add goroutine && git commit -m "feat(goruntime): future driver proves park/wake cycle on both fiber backends"
```

**阶段 2 门禁：** 默认后端 `make gate` 通过 + `--features native-asm` 下 `make gate` 通过。

---

## 阶段 3 — 单线程运行时：spawn / block_on / JoinHandle / yield_now

可工作、可测试的交付物：`goruntime::block_on(async { … })` 在单个 OS 线程上端到端运行 future，支持 `spawn`、`JoinHandle` 与 `yield_now`。

### 任务 3.1: Processor（P）—— 双队列 + 调度器上下文

**文件：**
- 创建：`goroutine/src/core/processor.rs`（替换占位符）
- 修改：`goroutine/src/core/mod.rs`

**接口：**
- 消费：`Runq` 在任务 4.2 定义（Go 风格环形）。阶段 3 中 `runq` 与 `wakeq` 都用 `SpinLock<VecDeque<GoroutineRef>>`；任务 4.2 把 `runq` 升级为环形（下面的 `Processor` API 不变）。
- 产出：
  ```rust
  pub struct Processor {
      pub id: usize,
      pub sched_ctx: Context,          // 仅 native-asm 后端；corosensei 忽略
      pub runq: SpinLock<VecDeque<GoroutineRef>>,  // 新鲜 goroutine（阶段 4 起可被窃取）
      pub wakeq: SpinLock<VecDeque<GoroutineRef>>, // 被唤醒的 goroutine（仅归属者）
      pub current: Option<GoroutineRef>,
      pub work_signal: Notify,          // 唤醒空闲 worker（阶段 4）；阶段 3 为桩
  }
  impl Processor {
      pub fn new(id: usize) -> Processor;
      pub fn pop(&self) -> Option<GoroutineRef>;   // 先 runq（新鲜）后 wakeq
      pub fn push_runq(&self, g: GoroutineRef);
      pub fn push_wakeq(&self, g: GoroutineRef);
      pub fn len(&self) -> usize;
  }
  ```

- [ ] **第 1 步：编写失败测试** —— 队列优先级（runq 先于 wakeq）+ 往返：

```rust
#[test]
fn processor_pop_prefers_fresh_over_woken() {
    let mut p = Processor::new(0);
    let fresh = Goroutine::new(Box::new(|_, _| {}));
    let woken = Goroutine::new(Box::new(|_, _| {}));
    p.push_wakeq(woken.clone());
    p.push_runq(fresh.clone());
    let first = p.pop().unwrap();
    assert_eq!(first.get().id, fresh.get().id, "fresh (runq) must run before woken (wakeq)");
    let second = p.pop().unwrap();
    assert_eq!(second.get().id, woken.get().id);
}
```

- [ ] **第 2 步：运行确认失败** —— `cargo test -p goroutine processor_pop_`
预期：FAIL。

- [ ] **第 3 步：实现** —— `pop` = `runq.pop_back()` 否则 `wakeq.pop_front()`（runnext 式的新鲜任务局部性；woken 用 FIFO 保持唤醒顺序）。把 `try_current_p()`/`current_p()` 接到线程局部（任务 2.2 已桩），并在 `sched/mod.rs` 中把 `enqueue_global`/`enqueue_owner` 接到全局队列 / 归属 P 的 wakeq。

- [ ] **第 4 步：运行确认通过** —— `cargo test -p goroutine processor_pop_`
预期：PASS。

- [ ] **第 5 步：提交**
```bash
git add goroutine && git commit -m "feat(goruntime): Processor with fresh-runq + owner-wakeq"
```

### 任务 3.2: 单线程 `block_on`

**文件：**
- 创建：`goroutine/src/task/mod.rs`、`goroutine/src/task/block_on.rs`
- 修改：`goroutine/src/lib.rs`（导出 `task`）

**接口：**
- 产出：`pub fn block_on<F, T>(future: F) -> T`（阶段 3 为单线程；阶段 4 为多线程）。

- [ ] **第 1 步：编写失败测试** —— main 完成；main 等待被 spawn 的任务；`yield_now` 变体（任务 3.4 添加）：

```rust
// tests/block_on_smoke.rs
use goruntime::{block_on, spawn};

#[test] fn block_on_completes_main_future() { assert_eq!(block_on(async { 6 * 7 }), 42); }
#[test] fn block_on_awaits_spawned_task() {
    assert_eq!(block_on(async { spawn(async { 21 + 21 }).unwrap().await.unwrap() }), 42);
}
```

- [ ] **第 2 步：运行确认失败** —— `cargo test -p goroutine --test block_on_smoke`
预期：FAIL。

- [ ] **第 3 步：实现** —— 构建 main goroutine，推入 `main_p.runq`，然后运行单线程循环：

```rust
pub fn block_on<F, T>(future: F) -> T
where F: Future<Output = T> + Send + 'static, T: Send + 'static {
    assert!(crate::core::try_current_p().is_none(),
        "block_on called from inside a goroutine; use await instead");
    let mut p = Processor::new(0);
    set_current_p(&mut p);
    let main = Goroutine::new(Box::new(move |g, park| {
        let out = run_future(g, future, park);
        let _ = g.output.set(Box::new(out));
        finish_g(g);
    }));
    p.push_runq(main.clone());

    let mut guard = 0;
    while main.get().state.load(Ordering::Acquire) != G_DEAD {
        guard += 1;
        assert!(guard < 1_000_000, "block_on loop did not terminate");
        let Some(g) = p.pop() else { std::thread::yield_now(); continue; };
        let gptr = g.get() as *const Goroutine as *mut Goroutine;
        set_current_g(gptr);
        p.current = Some(g.clone());
        g.get().state.store(G_RUNNING, Ordering::Release);
        // SAFETY: we are the sole resumer of g.
        let outcome = unsafe { g.get_mut() }.fiber.as_mut().unwrap().resume();
        p.current = None;
        set_current_g(std::ptr::null_mut());
        if let FiberOutcome::Yield = outcome {
            // 已挂起：绝不能在这里重新入队。它会由 schedule()（当 waker 触发时）
            // 或 requeue_self（yield_now）重新入队。两者都没发生就保持挂起——
            // 这是正确的。（阶段 3 单线程说明：waker 永不触发的已挂起 G 保持
            // 挂起是对的；循环只在 runq 非空时自旋。）
        }
    }
    main.get().output.get().and_then(|b| b.downcast::<T>().ok().map(|b| *b)).expect("main output")
}
```

- [ ] **第 4 步：运行确认通过** —— `cargo test -p goroutine --test block_on_smoke`
预期：PASS。
- [ ] **第 5 步：提交**
```bash
git add goroutine && git commit -m "feat(goruntime): single-thread block_on over the two-queue Processor"
```

### 任务 3.3: `spawn` + `JoinHandle`

**文件：**
- 创建：`goroutine/src/task/spawn.rs`
- 修改：`goroutine/src/task/mod.rs`

**接口：**
- 产出：
  ```rust
  pub fn spawn<F, T>(future: F) -> JoinHandle<T>;
  pub struct JoinHandle<T> { g: GoroutineRef, _marker: PhantomData<T> }
  impl Future for JoinHandle<T> { type Output = Result<T, JoinError>; … }
  pub struct JoinError;
  ```

- [ ] **第 1 步：编写失败测试** —— spawn/join 往返 + 丢弃句柄不取消任务（同原计划 3.3 任务）。
- [ ] **第 2 步：运行确认失败** —— `cargo test -p goroutine --test spawn_join`
预期：FAIL。
- [ ] **第 3 步：实现** —— `spawn` 构建 goroutine：若在运行时内则推入当前 P 的 `runq`（新鲜），否则推入全局队列。`JoinHandle::poll` 在 `state == G_DEAD` 时返回 `Ok(T)`，否则把 `cx.waker()` 注册进 `join_wakers` 并返回 `Pending`。`finish_g`（任务 2.3）唤醒 join 等待者。v1 无 abort。
- [ ] **第 4 步：运行确认通过** —— PASS。
- [ ] **第 5 步：提交**
```bash
git add goroutine && git commit -m "feat(goruntime): spawn + JoinHandle with detach semantics"
```

### 任务 3.4: `yield_now`

**文件：**
- 修改：`goroutine/src/task/mod.rs`

**接口：**
- 产出：`pub struct YieldNow;` —— 置 `g.yield_requested`，返回 `Pending`；驱动执行 gosched + 挂起；下一次轮询完成。`yielded: bool` 字段区分第一次（让出）与第二次（完成）轮询。

- [ ] **第 1 步：编写失败测试** —— 让出后另一任务获得时间片（原计划 3.4 任务测试）。
- [ ] **第 2 步：运行确认失败** —— `cargo test -p goroutine --test yield_now`
预期：FAIL。
- [ ] **第 3 步：实现**

```rust
pub struct YieldNow { yielded: bool }
impl Future for YieldNow {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, _cx: &mut std::task::Context<'_>) -> Poll<()> {
        if self.yielded { return Poll::Ready(()); }
        self.yielded = true;
        let g = crate::core::current_g();
        // SAFETY: we are the running goroutine; the M holds our execution ref.
        unsafe { (&mut *g).yield_requested.store(true, Ordering::Release); }
        Poll::Pending   // 驱动看到标志，gosched + 挂起，然后重新轮询我们
    }
}
```
- [ ] **第 4 步：运行确认通过** —— PASS。
- [ ] **第 5 步：提交**
```bash
git add goroutine && git commit -m "feat(goruntime): yield_now via driver gosched flag"
```

**阶段 3 门禁：** 两种后端 `make gate` 通过。

---

## 阶段 4 — M:N 调度器 + 工作窃取（钉扎安全）

可工作、可测试的交付物：`block_on` 派生 N 个 worker 线程；goroutine 通过"新鲜 runq + 归属 wakeq + 全局队列 + 仅窃取新鲜任务"分发；高并发下的正确性得到验证；**goroutine 只被其归属者恢复**（测试中断言）。

### 任务 4.1: 全局队列 + worker 线程 + 每-P 空闲通知

**文件：**
- 创建：`goroutine/src/sched/global.rs`
- 修改：`goroutine/src/sched/mod.rs`、`goroutine/src/task/block_on.rs`

**接口：**
- 消费：`GoroutineRef`、`SpinLock`、`Notify`（std 兼容通知器——任务 7.5；现在可用最小 `std::sync::Condvar` 垫片）。
- 产出：
  ```rust
  pub struct GlobalQueue { queue: SpinLock<VecDeque<GoroutineRef>> }   // 仅新鲜 spawn
  impl GlobalQueue { fn push(&self, g: GoroutineRef); fn pop(&self) -> Option<GoroutineRef>; }
  pub fn enqueue_global(g: &Goroutine);       // 推入 + 唤醒某 worker（如有空闲）
  pub fn enqueue_owner(g: &Goroutine, owner: usize);  // 推入归属者 wakeq + 唤醒归属者
  ```

- [ ] **第 1 步：编写失败测试** —— 跨线程全局 push/pop + 归属 wakeq 路由（原计划 4.1 任务测试，外加 `enqueue_owner` 路由断言：`enqueue_owner(g, 1)` 后 P1 的 wakeq 包含 g 而 P0 不包含）。
- [ ] **第 2 步：运行确认失败** —— `cargo test -p goroutine --test global_queue`
预期：FAIL。
- [ ] **第 3 步：实现** —— 普通 `SpinLock<VecDeque<GoroutineRef>>`。每-P `work_signal`：基于 `std::sync::Condvar` 的通知器（`parked: AtomicBool` + `Condvar`），使空闲 worker 可睡眠并在 push 时被唤醒——跨平台，无需 OS 特定 eventfd。worker 派生逻辑：`block_on` 派生 N-1 个线程，各带自己的 `Processor`；主线程成为第 N 个 worker。
- [ ] **第 4 步：运行确认通过** —— PASS。
- [ ] **第 5 步：提交**
```bash
git add goroutine && git commit -m "feat(goruntime): global queue + per-P idle notification (std Condvar)"
```

### 任务 4.2: Go 风格无锁本地 runq 环形队列（runnext + runqput + runqget）

**文件：**
- 替换：`Processor::runq`（自旋锁 VecDeque → 环形）
- 修改：`goroutine/src/core/processor.rs`

**接口：**
- 消费：`GoroutineRef` → 环形存储原始 `*mut Goroutine` + 计数引用（推入加引用，弹出释放）。
- 产出（Go 忠实，参照 `proc.go:7478/7598`）：
  ```rust
  const RUNQ_CAP: u32 = 256;
  struct Runq { ring: Box<[AtomicPtr<Goroutine>; 256]>, head: AtomicU32, tail: AtomicU32, runnext: AtomicPtr<Goroutine> }
  impl Runq {
      pub fn put(&self, g: &GoroutineRef, next: bool);  // runnext CAS 或环形入队；满 → 全局
      pub fn get(&self) -> Option<GoroutineRef>;         // 先 runnext，后环形 pop_front
      pub fn steal_half(&self) -> Vec<GoroutineRef>;     // runqgrab：窃取一半，FIFO
      pub fn len(&self) -> u32;
  }
  ```

- [ ] **第 1 步：编写失败测试** —— runnext LIFO + 环形 FIFO + 窃取一半（原计划 4.2 任务测试，适配 `Processor::pop`——现在为 `runq.get()` 再 `wakeq.pop_front()`）。
- [ ] **第 2 步：运行确认失败** —— `cargo test -p goroutine runq_`
预期：FAIL。
- [ ] **第 3 步：实现** 环形（按原计划 4.2 任务的算法：runnext CAS；`put` 重试 + 满时 `putslow` 到全局；`get` 先 runnext 后环形；`steal_half` 取前半）。`Processor::pop` = `runq.get()` 否则 `wakeq.pop_front()`。
- [ ] **第 4 步：运行确认通过** —— PASS。重跑阶段 2/3 的 driver + block_on 测试。
- [ ] **第 5 步：提交**
```bash
git add goroutine && git commit -m "feat(goruntime): Go-style lock-free runq ring + runnext"
```

### 任务 4.3: 工作窃取 —— 仅窃取新鲜 goroutine

**文件：**
- 创建：`goroutine/src/sched/steal.rs`
- 修改：`goroutine/src/sched/mod.rs`

**接口：**
- 消费：`Processor::runq.steal_half`。
- 产出：`pub fn steal_fresh(pool: &[&Processor], self_id: usize) -> Option<GoroutineRef>` —— 随机选受害者（用线程 id 作种子的 xorshift，不依赖 `rand`），窃取其 `runq` 的一半。**绝不动受害者的 `wakeq`**，因此被窃取的每个 goroutine 都是 `owner == NO_OWNER`（从未启动）——钉扎不变量由构造保证。

- [ ] **第 1 步：编写失败测试** —— P0 有 10 个新鲜任务，P1 窃取一半；同时断言 P0 的 `wakeq` 永不被窃取者清空：

```rust
#[test]
fn stealing_takes_only_fresh_and_never_wakeq() {
    let p0 = Processor::new(0);
    let p1 = Processor::new(1);
    for _ in 0..10 { p0.push_runq(Goroutine::new(Box::new(|_, _| {}))); }
    // P0 上一个被唤醒（归 P0 所有）的 goroutine 不可被窃取：
    let woken = Goroutine::new(Box::new(|_, _| {}));
    p0.push_wakeq(woken.clone());
    let stolen = steal_fresh(&[&p0, &p1], 1);
    assert!(stolen.is_some());
    assert_eq!(p0.len(), 5, "exactly half the runq was stolen");
    assert_eq!(p0.wakeq_len(), 1, "wakeq is untouched by stealers");
}
```

- [ ] **第 2 步：运行确认失败** —— `cargo test -p goroutine stealing_`
预期：FAIL。

- [ ] **第 3 步：实现** —— 随机受害者，`steal_half()`，取第一个窃到的 G。绝不查 `wakeq`。文档写明：这是对 Go `runqgrab`（可窃取任意可运行 G）的钉扎安全替代。
- [ ] **第 4 步：运行确认通过** —— PASS。
- [ ] **第 5 步：提交**
```bash
git add goroutine && git commit -m "feat(goruntime): work stealing of fresh goroutines only (pinning-safe)"
```

### 任务 4.4: 多线程 `block_on` + 调度循环

**文件：**
- 替换：`goroutine/src/task/block_on.rs`
- 修改：`goroutine/src/sched/mod.rs`

**接口：**
- 消费：`Runq`、`GlobalQueue`、`steal_fresh`、`Notify`。
- 产出：
  ```rust
  pub fn block_on<F, T>(future: F) -> T;  // N 个 worker，钉扎安全
  pub struct Runtime { workers: Vec<thread::JoinHandle>, main_p: Box<Processor>, global: Arc<GlobalQueue>, shutdown: Arc<AtomicBool> }
  ```

- [ ] **第 1 步：编写失败测试** —— spawn 风暴全部 join；多次让出不挂起（原计划 4.4 任务测试）+ 一个**钉扎不变量测试**：

```rust
#[test]
fn pinning_invariant_owner_resumes_only() {
    // 一个挂起并被唤醒 100 次的 goroutine：每次恢复都必须发生在最后运行它的线程上。
    // 每次恢复通过线程局部记录（归属者）并断言其永不改变。
    const ITER: usize = 100;
    let out = block_on(async {
        let jh = spawn(async {
            let mut last_owner = None;
            for _ in 0..ITER {
                goruntime::yield_now().await;           // 挂起 + 重新入队到归属者
                let cur = goruntime::thread_id_of_current_m();  // 仅测试辅助
                if let Some(prev) = last_owner { assert_eq!(prev, cur, "goroutine migrated threads!"); }
                last_owner = Some(cur);
            }
            last_owner.unwrap()
        }).unwrap();
        jh.await.unwrap()
    });
    assert!(out >= 0);
}
```
> `thread_id_of_current_m()` 是仅测试辅助函数，从线程局部读取当前 `Processor::id`（在 `core/mod.rs` 的 `#[cfg(test)]` 下添加）。若钉扎被破坏，该测试会明确失败。

- [ ] **第 2 步：运行确认失败** —— `cargo test -p goroutine --test mnn_stress`
预期：FAIL 或挂起（block_on 仍是单线程）。
- [ ] **第 3 步：实现调度循环**（优先级：runnext → 本地 runq → wakeq → 全局 → 窃取 → 空闲）：

```rust
fn schedule_loop(p: &Processor, global: &GlobalQueue, shutdown: &AtomicBool) {
    let mut since_global = 0u32;
    while !shutdown.load(Ordering::Acquire) {
        // 1. 本地：runnext / runq 环形（新鲜）然后 wakeq（仅归属者）。
        if let Some(g) = p.pop() { run_g(p, g); since_global += 1; continue; }
        // 2. 全局：约每 61 次本地弹出（或本地为空）时检查一次。
        if since_global >= 61 || since_global == 0 {
            since_global = 0;
            if let Some(g) = global.pop() { run_g(p, g); continue; }
        }
        if since_global > 0 { continue; }
        // 3. 从随机受害者窃取新鲜 goroutine。
        if let Some(g) = steal_fresh(&p.pool, p.id) { run_g(p, g); continue; }
        // 4. 再次检查全局，然后空闲（在 Condvar 上睡眠，由 push 唤醒）。
        if let Some(g) = global.pop() { run_g(p, g); continue; }
        p.work_signal.park_until_work(global);   // 睡眠直到 push 或 shutdown
    }
}

fn run_g(p: &Processor, g: GoroutineRef) {
    // SAFETY: 调用者（本 M）持有唯一执行引用。
    let g = unsafe { g.get_mut() };
    set_current_g(g);
    p.current = Some(g.clone());   // 运行期间保持存活
    if g.owner.load(Ordering::Relaxed) == NO_OWNER { g.owner.store(p.id, Ordering::Release); }
    g.state.store(G_RUNNING, Ordering::Release);
    let outcome = g.fiber.as_mut().unwrap().resume();
    p.current = None;
    set_current_g(std::ptr::null_mut());
    // FiberOutcome::Yield → 已挂起；只能由 schedule()（waker）或
    // requeue_self（yield_now）重新入队。绝不要在这里重新入队。
}
```

- [ ] **第 4 步：接线 `block_on` 派生 N-1 个 worker**（各带自己的 `Processor` + `set_current_p`），把 main goroutine 跑在 `main_p` 上，main goroutine 死亡后置 `shutdown`，唤醒所有空闲 worker（经 `work_signal`），然后 `join` 它们。结构与原计划 4.4 任务相同，但用上述钉扎安全循环。
- [ ] **第 5 步：运行确认通过** —— `cargo test -p goroutine --test mnn_stress`，连跑 5 次；再跑 `cargo test -p goroutine --test mnn_stress --features native-asm`。
预期：两种后端、所有轮次均 PASS。
- [ ] **第 6 步：提交**
```bash
git add goroutine && git commit -m "feat(goruntime): M:N block_on — pinning-safe scheduler loop + fresh-only stealing"
```

### 任务 4.5: 调度器正确性加固（死锁 / 丢失唤醒 / 迁移回归套件）

**文件：**
- 创建：`goroutine/tests/scheduler_hardening.rs`

- [ ] **第 1 步：编写测试** —— 每个都包在 `block_on` 里并带 30 秒看门狗：
  - `single_producer_single_consumer_pingpong_1k`
  - `many_spawn_many_yields_no_hang`（1000 任务 × 100 次让出）
  - `park_wake_across_cores`（N 个任务挂起在由其他任务触发的一次性 waker 上）
  - `nested_spawn_closure`（深度 8）
  - `owner_never_migrates`（把任务 4.4 的钉扎测试提级到此处）
- [ ] **第 2 步：重复运行** —— `for i in $(seq 10); do cargo test -p goroutine --test scheduler_hardening; done`
预期：两种后端每轮全 PASS。任何挂起 = `park_after_pending`/`schedule` 中丢失唤醒、`runnext` 窃取竞争或 `work_signal` 唤醒失效——在阶段 4 关闭前必须修复。
- [ ] **第 3 步：提交**
```bash
git add goroutine && git commit -m "test(goruntime): pinning-safe scheduler hardening suite"
```

**阶段 4 门禁：** `make gate` 通过 + 10 轮加固测试干净（两种后端）。

---

## 阶段 5 — Go 风格 channel（hchan）+ select

*与原计划相同：channel 是纯 safe Rust（`SpinLock` + waker），完全跨平台。任务 5.1–5.4 按原计划（ChanState + try 路径 → 加锁挂起的异步 send/recv → close 语义 → 带随机公平性的 channel `select!`），以 Go `chan.go` 算法（sendq/recvq + buf、直接交付、close 清空等待者）为参照，以 Go `chan_test.go` 语义为测试基准。channel 操作中使用的 `waker.wake()` 是我们的 goroutine waker（任务 2.2），因此唤醒阻塞中的发送方/接收方会正确地把其重新入队到归属者。*

---

## 阶段 6 — 计时器

*与原计划相同（计时轮 + `sleep`/`sleep_until`/`timeout`），跨平台：计时轮基于 `std`，计时线程使用 `std::thread::sleep`/`Condvar`。传给 `TIMERS.insert` 的 waker 是 goroutine waker，因此计时器到期会把归属者的 goroutine 重新入队。*

---

## 阶段 7 — tokio 兼容同步原语表面

*与原计划相同（`mpsc`/`oneshot`/`watch`/`Mutex`/`Notify` + tokio 兼容 `select!` 宏 + tokio 对比 dev 测试）。所有原语都是 safe Rust、从零构建在 `std` 之上；全程使用 goroutine waker，**不调用 tokio/smol 的任何异步功能**。tokio 仅作为 dev-dependency 用于"签名一致性"对比测试（`#[cfg(feature = "tokio-parity-tests")]`，dev-only，永不进入发布构建）。说明：任务 7.5 构建的 `Notify` 也是阶段 4 中 `Processor::work_signal` 使用的原语——实现一次，两处复用。*

---

## 阶段 8 — I/O reactor（mio）+ sockets

*与原计划相同，并把跨平台一点明确化：`mio` 抽象了 epoll（Linux）/ kqueue（macOS/BSD）/ io_uring（Linux，可选）/ IOCP（Windows），因此 `TcpStream`/`TcpListener`/`UdpSocket` 与就绪模型（AtomicU32 就绪位 + 每注册 waker → goroutine waker）在每个平台完全相同。reactor 线程通过调用我们的 waker 唤醒 goroutine——它会路由到 goroutine 的归属者（钉扎安全）。*

---

## 阶段 9 — kio-rs `goroutine` 后端 + 长尾延迟基准

*与原计划相同（kio `goroutine` 特性的 task/time/sync/net 后端；kcptun 在其下构建并启动；`bench/run_goroutine_p99.sh` 对比 tokio 的 p50/p99/p999；`bench/GORUNTIME_P999_REPORT.md` 撰写分析）。补充要点：**goroutine 后端不得启用 tokio/smol 特性**（与 tokio/smol 的 `compile_error!` 互斥已存在）；以 `--features goroutine` 构建的 kcptun 二进制中，运行时路径上不得存在任何 tokio/smol 异步代码——本运行时的实现本身只用 `std`。基准在 Linux 和 macOS 上运行（CI 提供时也含 Windows），使"跨平台"与"性能更好"的论断都有数据支撑，而非仅靠编译通过。*

---

## 范围之外（未来工作）

- `JoinHandle::abort` / `AbortHandle`，任务取消传播。
- `spawn_blocking`（goroutine 没有阻塞线程池；`kio::cpu_block` 暂以 std 线程桥接）。
- `interval`、亚毫秒计时调优、每-P 计时轮。
- 超越 corosensei 内建增长的栈增长（Go 风格 copystack 不适用于 `native-asm` 后端；native 后端使用固定 256 KiB 栈 + 保护页）。
- `select!` 的 `biased`、`else`、stream 分支。
- 超出 `mio` 暴露范围的 io_uring 后端。
- **自由跨线程 goroutine 迁移（Go 风格）。** 有意省略：corosensei 协程是 `!Send` 的且依赖线程局部状态（栈增长、展开），因此挂起的 goroutine 必须由最后运行它的线程恢复。运行时改为把 goroutine 钉扎到归属 M，只窃取从未启动的 goroutine。将来若采用完全控制上下文切换的手写后端，或可放宽此限制。

---

## 自查

**1. 规格覆盖（含跨平台指令）。**
- "必须跨平台，不能固定在某个平台" → 全局约束 #1 + 任务 1.4 平台矩阵；上下文切换接缝（`Fiber`/`Park`）以 corosensei 为跨平台默认；默认构建无 `libc::mprotect`/`epoll`；reactor 用 `mio`；线程/计时用 `std`；`native-asm`（唯一平台锁定的代码）以特性门控，且在目标不支持时 `compile_error!`（任务 1.3 第 7 步）。
- "参考 Go goroutine/chan" → 阶段 1–5（G/M/P、runnext/runq/steal、gosched、hchan + select），引用 Go `proc.go`/`chan.go`。
- "rust 异步 io runtime" → 阶段 1–8。
- "兼容 tokio" → 阶段 7 签名一致 + 阶段 9 kio 后端。
- "解决 tokio 长尾" → 任务 9.3 基准（p50/p99/p999 + 诚实验收标准）；每-P 唤醒通知（任务 4.1），使被唤醒的 goroutine 不必等待 100 µs 轮询。
- "各种情况下测试用例" → 每个阶段以行为测试收尾；加固套件（4.5）、MPMC 压力（5.2）、close 语义（5.3）、select 公平性（5.4）、计时精度（6.x）、I/O 压力（8.4）、钉扎不变量测试（4.4）、tokio 对比（9.1–9.2）。
- "用标准库异步实现、不用 tokio/smol、做性能更好的异步 I/O" → 全局约束 #3/#12；目标/架构/技术栈明示；任务 1.1 删除骨架中的 tokio 反模式；阶段 7 的 tokio 对比测试限定为 dev-only；阶段 9 的 goroutine 后端禁止启用 tokio/smol；阶段 9 基准即验收标准。

**2. 占位符扫描。** 无 TBD/TODO/推延。两处"较软"的位置（任务 1.3 `resume` 语义说明、corosensei `DefaultStack::with_size` API 说明）都内联了实际决策，并附"实施时对照锁定的版本确认"——这是版本钉扎说明，不是占位符。每个正确性关键步骤的代码都是完整的。

**3. 类型一致性。**
- `Fiber`/`Park`/`FiberOutcome`/`new_fiber` —— 任务 1.2 定义，任务 1.3 扩展，任务 2.1（`Goroutine::new`）、2.2/2.3（`run_future`/`park_after_pending`）、4.4（调度器 `run_g`）使用。各处以相同的名称/签名出现。
- `Goroutine` 字段（`state`/`notified`/`yield_requested`/`owner`/`output`/`join_wakers`/`refs`/`fiber`）、`GoroutineRef::{from_raw, owned, get, get_mut}` —— 任务 2.1 引入，全程一致使用（Runq 4.2 的推入加引用规则、`get_mut` 仅由归属 M 使用）。
- `Processor::{id, runq, wakeq, current, work_signal, pop, push_runq, push_wakeq}` —— 任务 3.1，4.2 升级，`requeue_self`/`enqueue_owner`/`schedule_loop` 使用。`work_signal` 在 4.1 实现（Condvar），7.5（`Notify`）复用作同一原语。
- `GState` 原子常量（`G_WAITING`…`G_DEAD`）在 2.1–4.4 保持一致；`NO_OWNER` 在 2.1 引入，被 `schedule`/`run_g`/`steal_fresh` 使用。
- channel 与 kio 的 API 名称与原计划一致，且匹配现有 `kio-rs` 导出。

需要向执行者指出的一点：阶段 3 的 `block_on` 绝不重新入队已挂起的 goroutine（无"拐杖"）——原计划里"重新推入"的拐杖**已删除**，因为在 corosensei 下，已挂起的 G 绝不能被非归属者恢复。阶段 4 的 `run_g` 是唯一恢复者。

---

## 执行交接

计划已保存至 `docs/superpowers/plans/2026-08-12-goruntime-async-runtime.md`。两种执行方式：

**1. 子代理驱动（推荐）** —— 我为每个任务派遣一个新子代理，任务之间进行审查，迭代快速。

**2. 内联执行** —— 我在本会话中使用 superpowers:executing-plans 执行任务，分批执行并在检查点审查。

选哪种方式？
