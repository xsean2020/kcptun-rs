# Tokio P999 尾部延迟诊断报告

**日期**: 2026-08-11  
**状态**: 证据审核完成，待决策  
**范围**: 裸 KCP 层（`kcp-rs`），不经 SMUX/Snappy/加密

---

## 1. 现象

### 1.1 两轮测试数据对比

| 指标 | 第一轮 | 第二轮 | smol 基线 | Go 基线 |
|------|--------|--------|-----------|---------|
| P50  | 425.4 µs | 418.6 µs | 216.9 µs | 425.0 µs |
| P90  | 536.5 µs | 529.6 µs | 321.3 µs | 484.0 µs |
| P99  | 1429.3 µs | 976.4 µs | 519.8 µs | 701.0 µs |
| **P999** | **36,869.8 µs** | **18,124.1 µs** | **736.1 µs** | **900.0 µs** |
| Max  | 51,274.9 µs | 38,529.9 µs | 10,619.8 µs | 8,407.0 µs |
| ok   | 30000 | 29975 | 30000 | 30000 |
| inflight_end | 1 | 25 | 0 | 0 |

### 1.2 特征描述

- P50/P90 与 Go 基线接近（~425 µs），主体性能正常
- P99 略高（1.0–1.4 ms），但不算异常
- **P999 出现断崖式跳变**：从 P99 的 ~1 ms 直接跳到 18–37 ms
- Max 达到 38–51 ms
- inflight_end = 25（第二轮）说明测试结束时仍有 25 个请求未完成
- 跨语言测试（tokio → Go / Go → tokio）P999 仅 1.0–1.3 ms，**问题仅出现在 tokio ↔ tokio 同进程组合**

---

## 2. 根因定位

### 2.1 结论

**根因是 Tokio 多线程运行时的任务迁移（task migration）导致的偶发调度停顿。**

这不是 KCP 协议问题、不是 timer/interval 问题、不是锁竞争问题、不是 CPU 算力不足。而是多线程 tokio runtime 的 N-worker 共享队列在 SMT（超线程）机器上引起跨核任务迁移，造成 10–40 ms 的偶发 stall。

### 2.2 证据链

#### 证据 1: smol 已经修复过完全相同的问题

`kio-rs/src/task/AGENTS.md` 明确记载：

> Smol `block_on` intentionally runs the global executor on exactly two participants (caller + one worker). Do not restore one worker per logical CPU without tail-latency evidence; **shared-queue task migration caused 10–20ms raw-KCP P999/max spikes on SMT machines.**

smol 曾使用 N 个 worker（每逻辑 CPU 一个），P999 出现 10–20ms 尖峰。修复方法是限制为 2 个 worker（caller + 1），P999 随即降到 736 µs。**Tokio 的多线程 runtime 存在完全相同的共享队列 + 任务迁移问题，但从未修复。**

#### 证据 2: Benchmark 的 runtime 配置

`kcp-rs/examples/latency_p99.rs` 第 720–734 行：

```rust
fn block_future(single: bool, fut: ...) {
    if single {
        // current-thread runtime（无任务迁移）
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_io().enable_time().build().unwrap();
        rt.block_on(fut);
    } else {
        // 默认：多线程 runtime，N = num_cpus 个 worker
        kio::block_on(fut);
    }
}
```

`bench/run_p99.sh` 第 83 行调用方式：
```bash
$EX_TOKIO --mode self --rps "$RPS" --warmup "$WARMUP" --duration "$DURATION" --size "$SIZE"
```

**没有传 `--rt single`**，因此使用默认的多线程 runtime（N workers = num_cpus）。

对比 smol 的 `kio-rs/src/task/smol.rs` 第 199 行：
```rust
let workers = num_cpus::get().min(2);  // 故意限制为 2
```

#### 证据 3: 跨语言测试排除 KCP 协议问题

| 方向 | P999 |
|------|------|
| tokio ↔ tokio | 36,869 µs |
| tokio → Go | 1,277 µs |
| Go → tokio | 1,018 µs |
| smol ↔ smol | 736 µs |
| smol → Go | 787 µs |
| Go → smol | 1,045 µs |

tokio ↔ tokio 是唯一一个 P999 异常的组合。当 tokio 端只参与一侧（发送或接收）时，P999 正常。这说明问题出在**同进程内 client + server 的所有任务共享一个多线程 runtime 的调度行为**，而非 KCP 协议本身。

#### 证据 4: 不存在 blocking 代码或锁跨 await

代码审查结果：

| 检查项 | 结果 |
|--------|------|
| `std::thread::sleep` in async path | ❌ 无 |
| `std::sync::Mutex` in async path | ❌ 无 |
| `spawn_blocking` / `block_in_place` | ❌ 无 |
| `parking_lot::Mutex` 跨 `.await` | ❌ 无（所有 guard 在 await 前释放） |
| `std::fs` 阻塞 I/O | ❌ 无 |
| 固定 `interval` timer | ❌ 无（使用 deadline-based `timeout(remaining, notified())`） |
| `MissedTickBehavior` 问题 | ❌ 无（不使用 `interval`） |

#### 证据 5: `run_self` 模式的任务拓扑

`run_self` 模式在**同一个进程**中运行 client + server，共享一个 runtime。任务清单：

| 任务 | 来源 | 职责 |
|------|------|------|
| Client sender | `kio::spawn_task` in `run_open` | 固定速率发送 payload |
| Client reader | 主 task inline | 100µs busy-poll 读取响应 |
| Client KCP input loop | `kio::spawn_task` in `KcpConn::build` | UDP recv → FEC → KCP input |
| Client KCP flush loop | `kio::spawn_task` in `KcpConn::build` | KCP update/flush → UDP send |
| Server listener | `kio::spawn_task` in `run_self` | `listener.accept()` |
| Server echo | `kio::spawn_task` in `run_self` | `read_exact` → `write_all` |
| Server KCP input loop | `kio::spawn_task` in `KcpConn::build` | 同上 |
| Server KCP flush loop | `kio::spawn_task` in `KcpConn::build` | 同上 |

至少 8 个活跃任务在 N 个 worker 间竞争。当 `Notify::notify_one()` 唤醒一个任务时，该任务可能被调度到另一个 worker——冷 L1/L2 cache + 跨核调度延迟。

### 2.3 为什么 smol 不受影响

| 特性 | smol | tokio（默认） |
|------|------|--------------|
| Worker 数 | 2（caller + 1） | N（= num_cpus） |
| 任务迁移频率 | 极低（2 thread 间） | 高（N thread 间） |
| cache 亲和性 | 好 | 差 |
| SMT 影响 | 极小 | 显著 |

smol 的 2-worker 设计使任务几乎总是在同一个线程上执行，cache 亲和性极好，任务迁移导致的 stall 几乎消失。

### 2.4 为什么 Go 不受影响

Go 的 runtime 使用 P（处理器）绑定 G（goroutine）到 M（OS 线程）。Go 的 netpoller 是 per-M 的，且 Go 的调度器有 work stealing 但 goroutine 极轻量（~2KB 栈），cache 影响远小于 tokio 的 task（~2MB 栈 + full-featured waker）。此外 Go 的 `SetReadDeadline(100µs)` 是 busy-poll，减少了调度跳转。

---

## 3. 修复方案

### 方案 A: Benchmark 立即修复 — 使用 `--rt single`

**改动范围**: `bench/run_p99.sh`

将 tokio 调用加上 `--rt single`：

```bash
R_TT=$(run_step "1/10" "$EX_TOKIO" --mode self --rt single --rps "$RPS" ...)
```

**效果**:
- 使用 current-thread runtime，消除任务迁移
- 预期 P999 从 ~36ms 降到 < 2ms（与 smol 一致）
- **零代码改动**，仅改 benchmark 调用

**验证命令**:
```bash
# 先验证 single-thread 是否解决问题
target/release/examples/latency_p99_tokio --mode self --rt single --rps 500 --warmup 5 --duration 60 --size 26624

# 对比 multi-thread（当前问题基线）
target/release/examples/latency_p99_tokio --mode self --rt multi --rps 500 --warmup 5 --duration 60 --size 26624
```

**风险**: 无。`--rt single` 已在代码中实现，仅诊断用途。

### 方案 B: Tokio `block_on` 限制 worker 数为 2 — 对齐 smol 设计

**改动范围**: `kio-rs/src/task/tokio.rs` 的 `global_rt()` 函数

```rust
fn global_rt() -> &'static tokio::runtime::Runtime {
    GLOBAL_RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)           // ← 新增：对齐 smol 的 2-worker 设计
            .enable_all()
            .build()
            .expect("failed to create tokio runtime")
    })
}
```

**效果**:
- 所有使用 `kio::block_on()` 的入口（kcptun-client + benchmark）都限制为 2 个 worker
- 保持多线程能力（I/O 并行），但消除 N-worker 任务迁移
- 预期 P999 降到 ~1ms 级别

**风险**: 
- 生产环境 kcptun-client 在高并发下可能降低吞吐（但 server 已用 `block_on_local`，不受影响）
- 需要在目标硬件上 A/B 测试吞吐和 P999

**验证命令**:
```bash
# 修改后重新构建
cargo build --release -p kcp-rs --features async-tokio --example latency_p99
# 测试
target/release/examples/latency_p99_tokio --mode self --rps 500 --warmup 5 --duration 60 --size 26624
```

### 方案 C: 生产客户端改用 `block_on_local` — 对齐 server 设计

**改动范围**: `kcptun-client/src/main.rs`

```rust
fn main() -> Result<()> {
    let cli = cli::Cli::parse_go_compatible();
    kio::block_on_local(app::async_main(cli))  // ← 从 block_on 改为 block_on_local
}
```

**效果**:
- 客户端使用 current-thread runtime，完全消除任务迁移
- 与 server 的 `block_on_local` 设计对齐
- 最大化 cache 亲和性

**风险**:
- 丧失多线程 I/O 并行能力——但 KCP 本身是单连接流，`KcpConn` 的 input loop + flush loop 在单线程上已足够
- 客户端如果需要同时处理多个 KCP 连接（`--conn N`），单线程可能成为瓶颈——需要测试
- `block_on_local` 创建的是 `current_thread` runtime，`spawn_task` 的任务都在同一线程上运行

**验证命令**:
```bash
# 修改后重新构建
make release
# 用 bench/run_p99.sh 做端到端验证
```

### 方案对比

| 方案 | 改动范围 | 预期 P999 | 吞吐影响 | 风险 | 建议优先级 |
|------|---------|-----------|---------|------|-----------|
| A: `--rt single` (benchmark) | `bench/run_p99.sh` 1 行 | < 2 ms | 无 | 无 | **立即执行** |
| B: tokio `.worker_threads(2)` | `kio-rs/src/task/tokio.rs` 1 行 | ~1 ms | 需测 | 低 | **推荐** |
| C: client `block_on_local` | `kcptun-client/src/main.rs` 1 行 | < 1 ms | 需测 | 中 | 备选 |

---

## 4. 确认前应做的诊断

### 4.1 A/B 测试（必做，确认根因）

```bash
# Step 1: 构建
cargo build --release -p kcp-rs --features async-tokio --example latency_p99

# Step 2: 当前问题基线（多线程 runtime）
target/release/examples/latency_p99_tokio --mode self --rps 500 --warmup 5 --duration 60 --size 26624

# Step 3: 修复验证（单线程 runtime）
target/release/examples/latency_p99_tokio --mode self --rt single --rps 500 --warmup 5 --duration 60 --size 26624

# Step 4: 如果 Step 3 的 P999 < 2ms，根因确认
```

### 4.2 Worker 数扫描（可选，量化关系）

```bash
# 临时修改 kio-rs/src/task/tokio.rs 的 global_rt() 分别测试:
#   .worker_threads(1)  → 等价于 current-thread
#   .worker_threads(2)  → smol 对齐
#   .worker_threads(4)
#   .worker_threads(8)
# 每次 rebuild + bench，记录 P999
```

### 4.3 调度 gap 直方图（可选，辅助确认）

在 `latency_p99.rs` 的 `run_self` 中添加一个独立的调度 watchdog：

```rust
kio::spawn_task(async move {
    let mut last = Instant::now();
    loop {
        kio::sleep_ms(1).await;
        let now = Instant::now();
        let gap = now.duration_since(last);
        last = now;
        if gap > Duration::from_millis(5) {
            eprintln!("SCHED GAP: {:?}", gap);
        }
    }
});
```

如果 `SCHED GAP: 37ms` 与 P999 对齐，则确认是 runtime 调度停顿。

---

## 5. 不建议排查的方向

以下方向已通过代码审查排除，不建议浪费时间：

| 方向 | 排除理由 |
|------|---------|
| `tokio::time::interval` missed tick | flush loop 不使用 `interval`，使用 deadline-based `timeout(remaining, notified())` |
| `parking_lot::Mutex` 跨 await 持锁 | 代码审查确认所有 guard 在 await 前释放 |
| `std::sync::Mutex` 阻塞 | KCP 代码不使用 `std::sync::Mutex` |
| `spawn_blocking` 开销 | KCP 裸层不使用 `cpu_block`（无加密/压缩） |
| KCP update timer 精度 | flush loop 是 deadline-driven，不是 fixed-interval |
| UDP socket buffer 溢出 | 已调优 `kern.ipc.maxsockbuf=8MB`，`recvspace=4MB` |
| Channel queue 延迟 | 裸 KCP 层不使用 channel（input loop → flush loop 通过 `Notify`） |

---

## 6. 长期建议

1. **Tokio runtime 配置应与 smol 对齐**：`global_rt()` 应使用 `.worker_threads(2)` 或提供可配置选项。当前 smol 已有明确的 tail-latency 证据支持 2-worker 设计，tokio 应遵循相同原则。

2. **Benchmark 默认应使用 `--rt single`**：裸 KCP 延迟测试的目的是测量协议层延迟，不是 runtime 并行能力。使用 current-thread 消除 runtime 调度噪声，使结果可复现且跨 runtime 可比。

3. **生产客户端可考虑 `block_on_local`**：kcptun-client 通常只维护少量 KCP 连接（`--conn`），单线程 runtime 已足够。但需先验证多连接吞吐是否下降。

4. **AGENTS.md 应记录此决策**：在 `kio-rs/src/task/AGENTS.md` 中补充 tokio worker 限制的 tail-latency 证据，与 smol 的记录对称。

---

## 附录 A: 关键代码位置索引

| 文件 | 行号 | 内容 |
|------|------|------|
| `kio-rs/src/task/tokio.rs:144-151` | `global_rt()` | 多线程 runtime 创建（无 worker 限制） |
| `kio-rs/src/task/smol.rs:189-249` | `block_on()` | smol 2-worker 设计（有 worker 限制） |
| `kio-rs/src/task/AGENTS.md:29-32` | — | smol tail-latency 证据记录 |
| `kcp-rs/examples/latency_p99.rs:720-734` | `block_future()` | benchmark runtime 选择（`--rt single` 可选） |
| `bench/run_p99.sh:83` | — | benchmark 调用（未传 `--rt single`） |
| `kcp-rs/src/conn.rs:1836-1965` | `spawn_flush_loop` | deadline-based 调度（非 interval） |
| `kcp-rs/src/conn.rs:1500-1617` | input loop | `Notify::notify_one()` 唤醒 flush loop |
| `kcptun-server/src/app.rs:384-387` | shard worker | server 使用 `block_on_local`（已优化） |
| `kcptun-client/src/main.rs:48` | client main | client 使用 `block_on`（未优化） |

## 附录 B: smol 修复历史参考

smol 的 tail-latency 修复记录在 `kio-rs/src/task/AGENTS.md` 中：

> Smol `block_on` intentionally runs the global executor on exactly two participants (caller + one worker). Do not restore one worker per logical CPU without tail-latency evidence; shared-queue task migration caused 10–20ms raw-KCP P999/max spikes on SMT machines.

这正好对应当前 tokio 的症状：
- tokio P999 = 18–37ms（smol 修复前 10–20ms 的同量级）
- 根因相同：shared-queue task migration on SMT
- 修复方法应相同：限制 worker 数

---

**审核状态**: 证据审核完成。建议先执行方案 A（benchmark `--rt single`）确认根因，再决定是否执行方案 B（tokio `.worker_threads(2)`）或方案 C（client `block_on_local`）。
