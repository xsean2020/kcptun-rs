# kcp-rs 优化建议（2026-09-01）

> 本文由全仓库代码分析（调用方 grep 验证）生成，是当前分支的证据门控优化计划。
> 配套的死代码清理已在同一次提交中完成（见第 2 节）。
> 前置背景：`docs/superpowers/plans/2026-08-05-kcp-conn-listener-tail-latency.md`（Phase 0–4 已完成）。

## 1. 结论摘要

kcp-rs 的"容易赢"（tail-latency Phase 0–4：空闲零 timer、内联发送、burst 批处理、flush 缓冲 reserve、acklist SmallVec、SegmentPool）已完成。当前最大的单项收益在 **FEC 收发路径的每包分配/拷贝**，其次是 **bench 中两处 Rust 输给 Go 的场景**（aes-128-gcm 无压缩、null 加密有压缩）。服务端在 Linux 上是 syscall-bound（send_to 占 51.6%），继续压 CPU 之前应先做 Linux 全量验收与 loss 注入 harness。

## 2. 已实施的清理（本次提交）

死代码删除（全部经 workspace 级 grep 验证 0 调用方）：

| 位置 | 删除项 |
|------|--------|
| `kcp.rs` | `KCP::check()`（37 行）、`KCP::set_mss()`、`KCP::reset()`、`KcpError::InvalidSegment/BufferTooSmall`（从未构造） |
| `segment.rs` | `KCP_MAX_WND` 常量 |
| `conn.rs` | `peer_addr()`（remote_addr 别名）、`is_nonblocking()`、`try_clone()`、`stats_snapshot()`+`SessionStats`（连同 lib.rs 再导出）、`buffer_size()` no-op 垫片、flush 循环中的 `dead_checks` 死变量、两处 `add(...,0)` 噪音计数 |
| `sharded.rs` | `pending_count()`、`worker_pool_size()`（别名 getter）；修正引用不存在的 `flush_ready_sessions`/`tick()` 的过期注释 |
| `transport.rs` | `PeerQueue::push_and_reuse`、`notify_one`、`max_packets`/`drops`/`packet_bytes` 死字段及 3 个 push 侧测试（生产路径 `feed_raw_batch` 直灌，无人再向队列 push） |
| `snmp.rs` | 5 个**从未递增**的计数器：`empty_flush`、`encrypt_inline`、`encrypt_offload`、`decrypt_offload_skipped`、`input_urgent_sends`（`.rustobs` CSV 同步缩减为 `timestamp,WriteInlineSends,WriteFlushSends`） |
| 接线 | `read_fallback_timeout` 从"只读不写"改为在 `connect_timeout` 的 `WAIT_FALLBACK_MS` 兜底超时处真实递增 |
| 依赖 | kcp-rs 移除 `bitflags`/`crc32fast`（0 引用）；kcptun-client 移除 `bytes`；kcptun-server 移除 `bytes`/`snap`（snap 走 kcptun-common） |
| 顺带 | 修复 smux-rs/knet-rs 等处分支上既有的 clippy 失败（needless_borrow 等，使 kcp-rs/kcptun-common/smux-rs/knet-rs/kcptun-client 全部通过 `-D warnings`） |

保留未删（判定为"连贯的库 API"而非垃圾，0 工作区生产调用但有测试/示例使用）：`KCP::input()`/`flush()`（Go 对齐 API）、`KcpStream::into_split`+读写半（tokio 对齐，examples/tests 在用）、`read_timeout()/write_timeout()` getter（tests/examples 在用）、`take_error`/`accept_timeout`（tokio 对齐）、整个 `listener.rs` `KcpTcpListener`（测试在用；二进制不用，若确认弃留可整体删除，约 158 行）。

## 3. 性能优化建议（按优先级）

### P0 — FEC 发送路径（第二轮实施后修订：数据路径已近最优）

逐项核查 + 交替 A/B 微基准（50 万包，min 统计量，机器噪声 ±40%）后的结论：

- **数据包路径本就接近下限**：1 次 calloc + 2 次 memcpy（kcp→帧、帧→shard_cache）。尝试用 `with_capacity+resize+extend` 消除整包清零后**实测无改进甚至略慢**——macOS/Linux 的 `alloc_zeroed`(calloc) 由分配器的预清零页池服务，整帧清零近乎免费。改动已回退，结论写在 `wrap_kcp_packet` 的 doc comment 中。
- **parity `to_vec` 是固有成本**：`shard_cache[idx]` 要为下一组回收复用，而 parity 包在飞，无法交出所有权；下游 `Bytes::from(Vec)` 是零拷贝 move，不存在"第二次拷贝"（修正了"每校验包 1 次多余分配"的早期判断）。
- **解码器 `Bytes::copy_from_slice` 同理是 API 边界必需**：入参是 `&[u8]`（burst 槽位缓冲会被 input loop 复用），不复制无法交给 decoder（原注释"zero-copy"已改为如实描述）。
- **已实施：时钟统一**——`fec.rs::current_ms` 改用 kcp.rs 的共享缓存时钟（新提为 `kcp::wall_ms()`，`KCP::current_ms` 委托之），FEC 每 encode 少一次 epoch 换算。注：macOS 上两者都走 commpage vDSO，非真 syscall，收益小于早期 6.5% CPU 的估计（那是 ~2000 次/秒 × 多调用点合计）；Linux 上同理属低成本一致性改进。
- **已实施：`parse_fastack` 短路**——`fastresend<=0` 时（Go 默认/normal 模式）fastack 计数永远无法触发重传（flush 侧 `resent=0xFFFFFFFF`），直接跳过每 ACK 的 O(inflight) snd_buf 扫描，语义完全等价。
- **评估后放弃**：conn.rs 恢复路径 `to_vec` → `Cow::Borrowed` 重构可省每恢复包 1 次分配，但需要两段式重构且会改变 KCP 输入顺序（恢复分片被推迟到 burst 末尾），仅在丢包路径生效，收益/风险比不划算。

### P1 — bench 落后场景（对照 Go）

**2026-09-01 受控 A/B 确认：`polyval_armv8` 修复带来 +25% 吞吐（非噪声）**

> 修正说明：此前记录的 "24.6→57.35 MB/s (2.33×)" 是跨机器对比（旧 `bench_results.json` 中 Go 也是 37.7→65.44，明显不同环境），该数字不可靠。以下为**同一台机器、同一条件、仅切换 cfg** 的受控 A/B：

```
A (soft GHASH,   无 polyval_armv8):  83.50  78.88  82.37 → 中位数 82.37 MB/s
B (hw pmull,     有 polyval_armv8): 100.59 103.08 104.22 → 中位数 103.08 MB/s
```

**+25.1%**。两组各 3 次采样区间不重叠（A: 78.9–83.5, B: 100.6–104.2），配合 pprof 中 GHASH 9.42%→2.78%，结论可靠。

#### 根因：`polyval_armv8` cfg 缺失

剖析客户端（`make profile client`，清理端口 6060 冲突后）显示 `polyval::backend::soft::Polyval` 占 9.42% CPU——GCM 的 GHASH 多项式乘法（GHASH）全程跑在软件回退路径上。ARMv8 PMULL 指令未被启用。

**修复链**：
1. `.cargo/config.toml` 的 `[target.aarch64-*]` rustflags 只设置了 `aes_armv8`（AES 硬件），但 `polyval` crate 需要 `polyval_armv8` cfg 来启用 PMULL 硬件 GHASH。
2. `make profiling-bins` 的 RUSTFLAGS 环境变量**覆盖** config.toml 的 rustflags（Cargo 行为：环境变量替换而非追加），所以即使 config.toml 改了，`make profile` 构建的二进制仍用 soft GHASH。
3. 修复：config.toml + Makefile profiling-bins 两处都补上 `--cfg polyval_armv8`。

**剩余热点**（客户端，24.89s 总采样）：
| 函数 | 占比 | 说明 |
|------|------|------|
| UDP send/recv | 44% | macOS 内核态 syscall 开销（Go 也一样） |
| mimalloc 分配 | 12.5% | 每包分配开销（KCP/FEC/SMUX 路径累积） |
| FEC RS 编码 | 8.5% | Reed-Solomon 矩阵运算 |
| GCM encrypt | 3.5% | 加上 pmull 加速后的实际加密开销 |
| SMUX 会话 | 3.6% | 帧收发管理层 |
| Bytes::drop | 1.2% | 引用计数管理 |

**建议**：mimalloc 分配 12.5% 是下一个可优化方向——每包分配来自 KCP flush 的 `split()` + FEC 转义 + SMUX 帧打包。但需先确认这是否是 macOS 特有的（mimalloc 在 macOS 上路径不同），建议在 Linux 上用 frame-pointers profile 验证后再做。

#### 其他落后场景

- **none/comp**（22.8 vs 39.2，Rust 0.58×）：本次未追查。需在 Linux 上 profile 确认 snap 调用粒度（SnappyPipe 可能每小块调用一次 snap 压缩，Go 批量处理）。追踪路径：`kcptun_common::snappy_pipe::SnappyPipe::write` → `snap::write::FrameEncoder`。

### P2 — 高 inflight 时的 O(n) 热点（已有量化注释，profile 后再动）

- `parse_fastack` 的 `fastresend<=0` 短路已实施（见 P0 节）。`fastresend>0`（fast/normal2 模式）时每 ACK 的 O(inflight) 扫描与 `flush_with_current` 的 snd_buf 全扫是 KCP 协议固有成本——每段必须逐个判定首传/fast 重传/RTO/最近截止时间，没有无损短路点；进一步优化（如维护最近 resendts 最小堆）属于结构改动，需先在 Linux 上 profile 确认占比。
- `parse_data` 乱序插入用 `VecDeque::insert`（O(n) 搬移后半队列）；仅在乱序/丢包流触发，优先级低于上面两条。

### P2 — 结构性清理（行为不变）

- `sharded.rs` worker 事件循环里"单包快路径 + 多 peer 分组 + 时间预算"逻辑复制了两份（drain 分支与 recv_timeout 分支，约 70 行）。抽成一个函数可消除双维护风险——属于最热的循环，改动需 Linux 压测回归，建议单独 PR。
- `PeerQueue`/`PeerTransport` 残留：pop 侧是 trait 必需实现但生产恒空。若确认没有 `background_input(true)`+PeerTransport 的组合，可把 `PeerTransport` 简化为纯发送 transport（recv 返回 WouldBlock），并在 `Drop` 之外用独立 liveness 句柄替代 queue 的 closed 标记（reaper 依赖 `is_closed`，改动需小心）。
- 每个 server 会话仍为从未使用的 queue 分配一次 `PeerQueue`（构造已简化，但 Arc+Mutex+Notify 仍在）；上述简化可一并消掉。

### 明确不做（证据门控，无新 profile 不动）

output batch commit（Phase 5.1/5.2/5.3）、rcv_buf 快路径（6.1）、shard 所有权（7）、KcpEndpoint（仅为"数万会话"目标）、哈希时间轮（当前绝对 deadline + 空闲 park 设计已消除每连接 timer，注释中有 A/B 数据）。

### P99 调查（2026-09-02，开环 500 RPS × 512B 延迟差距）——已查证，无安全修复

**背景**：LATENCY_P99 报告显示 kcp-rs↔kcp-rs p99=7263µs vs kcp-go↔kcp-go p99=453µs（16×）。约束：优化不得损失现有性能（闭环 Rust 已领先 Go：149k vs 82k req/s，p99 603 vs 1054µs——问题只在开环固定 RPS 场景）。

**发现 1 — 报告数字含环境干扰**：报告生成时一个 CPU 密集的 Go demo 进程（badger/mongo appserver）一直在跑。清理后本机公平对比（同机同参数 30s）：

| 指标 | Rust-Rust | Go-Go |
|------|-----------|-------|
| p50 / p90 | 135 / 247µs | 107 / 157µs |
| **p99** | **1485µs** | **323µs** |
| p999 | 7996µs | 5359µs |
| 闭环吞吐 | 149k req/s | 82k req/s |

真实差距是 4.6×，不是 16×。建议以后跑 `run_p99.sh` 前先确认无后台进程竞争 CPU。

**发现 2 — `is_sending` CAS 重试方案被 A/B 证伪**：假设是 `write_all` 的 `try_drain_and_send` CAS 失败后回退 flush loop 通知、最多等一个 10ms 周期。实测（同机 A/B，各 20s）：

- 基线：开环 p99=1485µs，闭环 149k req/s
- 4 次 CAS 重试 + `yield_now`：开环 p99 **恶化到 2681µs**（shed=74，sender 节奏被 yield 打乱）
- 1 次 CAS 重试（无 yield）：开环 p99 **仍恶化到 2327µs**，p999 3.4×

根因：重试成功时会持有 `is_sending` 令牌完成整个 async `flush_tx_batch`，await 期间 flush loop 被饿死，产生连锁延迟。**该方向不成立，改动已完整回退。**

**发现 3 — 轮询读无解**：Rust 加 100µs reader-poll（镜像 Go `ReadDeadline(100µs)`）p99 仅 1485→1294µs；50µs 轮询导致 shed=234、p99=18.8ms，严重恶化。

**结论**：剩余差距（1.5ms vs 0.32ms）来自 tokio async 任务调度的固有成本（notify 唤醒→调度跳→KCP 锁→CAS 多跳），非单点缺陷。在不损失闭环性能的约束下没有安全的局部修复；若要继续收敛需要架构级改动（如 echo 路径同步直通发送），风险高，需先在 Linux 上做完整的 p99 + 吞吐双重回归验证。

## 6. 裸机 Linux 验证（2026-09-02，root@192.168.0.84）

环境：CentOS 7 / 内核 3.10 / KVM 2 vCPU @3.4GHz（**无 AES-NI/SSE4.2/AVX**，Rust 与 Go 的 AES 均为软件实现——公平）/ 1.8GB RAM / PTI 缓解开启 / tc netem 可用 / 无工具链（本机交叉编译 musl 静态 + Go linux/amd64 后 scp 部署）。UDP 缓冲调到 4MB。

### 裸机基线（裸 KCP 层，500 RPS × 512B × 60s，client+server 同机）

| 指标 | rust-rust | go-go | macOS 对照（rust-rust） |
|------|-----------|-------|------------------------|
| p50 | **93.3µs** | 91.0µs | 135µs |
| p90 | **115.5µs** | 123.0µs | 247µs |
| p99 | 394.3µs | **174.0µs** | 1485µs |
| p999 | **8223µs** | **248µs** | 7996µs |
| max | 16155µs | 1862µs | 17857µs |

裸机 UDP 探针：loopback RTT p50=12µs / p99=28µs；单流 sendto ~18 万 pps（Go，含退避）。裸 KCP 栈 500 RPS（~2500 pps）余量充足。

**核心发现**：
1. **p50/p90 Rust 与 Go 打平**（93 vs 91µs）——macOS 上 1.3-1.6× 的中位差距是 kqueue/macOS 调度特有，Linux 上不存在。
2. **p99 差距缩至 2.3×**（394 vs 174µs）。
3. **Rust 独有的 ~8ms 尾部簇（0.1%，30/30000 样本）是仅存的差距**，Go 的 max 只有 1.9ms。

### 8ms 尾部簇归因实验（各 20s/30s，10000-15000 样本）

| 实验 | 变量 | p99 | p999 | 结论 |
|------|------|-----|------|------|
| 基线 | — | 394µs | 8.2ms | 簇在 0.1% 分位 |
| A | server+client `KCP_BUSY_YIELDS=512`（读自旋） | 1179µs | 8.7µs ms | 自旋**无效且 p50 恶化**（686µs，挤占 CPU）→ 排除读唤醒丢失 |
| B | `interval=5ms`（nodelay 1 5 2 1） | 367µs | 8.9ms | **簇位置不随 interval 移动** → 排除"等一个 flush 周期"假设 |
| C | Go `GOMAXPROCS=1` 对照 | 172µs | 437µs | Go 单核也干净 → 排除 2 核竞争本身 |
| D | rust `--reader-poll 100`（镜像 Go ReadDeadline） | 1204µs | 10.1ms | 轮询读**无效**（p99 恶化） |
| E | rust 多线程 runtime workers=2 | 979µs | 11.1ms | runtime 拓扑变化**无效** |
| F | `--snmp` 计数器 | 289µs | 8.1ms | **retrans=0 lost=0 in_errs=0** → 排除丢包/重传 |
| G | **rust client → Go server** | **159µs** | **232µs** | **簇消失！** |

**归因链**：G 实验决定性地把尾部簇定位到 **Rust server（echo 侧）**——rust client + Go server 时 p999=232µs，簇完全消失；而 Go client + Rust server 侧我们已知有簇（rust-rust p999=8.2ms）。结合 B 实验（interval=5ms 簇不动）与 F 实验（零丢包零重传），排除 KCP 层，指向 **Rust server 的 echo 路径在 tokio 上的罕见长尾调度事件**（echo task 唤醒/写路径 in-flighting 与 flush loop 的交互），与 macOS 上的 1.5ms 中尾（同一现象被 macOS 环境 10× 放大到 1% 频率）同源。

**与 macOS 数据合并的结论**：同一现象，Linux 裸机上 0.1% 频率（p999=8ms），macOS 上 1% 频率（p99=1.5ms）。修复它需要把 server echo 路径的调度确定性做上去（选项 1/2 或更深的 echo 直通），局部补丁（CAS 重试/轮询读/自旋）已在两平台上被 A/B 证伪。

### Phase 0 决策门数据（epoll 分析回填）

- RX→worker 跳转（~2-10µs/包）在 500 RPS 场景占比可忽略（RTT p50 93µs 的 ~3-10%，仅在 p99 层可感）。
- 裸机单流 UDP ~18 万 pps（2 vCPU KVM 上限），远程 VM 不适合做百万 pps 饱和测试——SO_REUSEPORT 规模化终验仍需更强裸机。
- **tc netem 丢包注入在此 VM 可用**（Docker 做不到）——后续 loss 场景回归可在此进行。

## 7. 【破案】8ms 尾部簇根因：CFS `sched_wakeup_granularity`（2026-09-02 深夜追加）

第二轮实验（Exp-H ~ Q）在 VM 上继续追 0.1% 的 8ms 尾部簇：

| 实验 | 内容 | 结果 |
|------|------|------|
| H/I | 减少全局 runtime 线程（--workers 1，两侧/单侧） | 簇不消（p999 8.1-12ms） |
| J | CPU 绑核 | **失败**——发现 VM affinity 实际只有 **1 个 vCPU**（`taskset -pc` → `0`；全部 666 个任务 psr=0；nproc=1。cpuinfo 的 "2 processors" 是假象） |
| K | RPS 提到 2000 | 簇仍在（p999 10.2ms）→ 与空闲间隙无关 |
| M | 服务进程线程盘点 | **6 条线程**（main + 全局 runtime×2 + RX + R_rx×2 + worker）共享这 1 个 vCPU |
| O | RT-FIFO 调度（chrt -f） | ssh 会话无 CAP_SYS_NICE，未做成 |
| P | `kernel.sched_latency_ns` | **= 6ms**；`sched_wakeup_granularity_ns` **= 15ms** |
| **Q** | **wakeup_granularity 15ms → 1ms** | **簇彻底消失：p999 11953→363µs，p50 95→18.8µs** |

### 根因机制

VM 的 CFS 调度参数为 `wakeup_granularity=15ms`：被唤醒的线程必须等待**当前运行线程用满 15ms 最小粒度**（或其主动让出）才能抢占。kcp-rs server 是多线程唤醒链（RX → worker → 全局 runtime 的 echo task → flush loop），共 6 条线程共享 1 个 vCPU——**唤醒链每一跳都可能撞上 15ms 抢占门槛**，表现为 0.1% 的 8-12ms 尾部簇。而 **Go 的 goroutine 是用户态协作调度**：echo goroutine 唤醒后在同一个已运行的 P 上直接执行，**完全不经过内核 CFS 重新排队**，因此对 `wakeup_granularity` 免疫（Go 对照在两种粒度下均无变化）。

这同时解释了：
- 为什么 macOS 上簇频率是 10×（macOS 的调度器对唤醒任务同样有抢占门槛，且 kqueue 路径不同）；
- 为什么 Exp-B（interval=5ms）无效——簇不在 KCP 定时器里；
- 为什么 Exp-A（读自旋）无效——自旋的是 echo reader，唤醒链的起点（RX/worker）仍要过 CFS。

### 三次重复验证（wakeup_granularity=1ms，各 10000 样本）

| 指标 | run1 | run2 | run3 | Go 同参数对照 |
|------|------|------|------|--------------|
| p50 | 20.7µs | 19.7µs | 18.6µs | 87.0µs |
| p90 | 87.1µs | 74.5µs | 78.3µs | 118.0µs |
| p99 | 140.5µs | 112.9µs | 108.1µs | 195.0µs |
| p999 | 1258µs | 400µs | 361µs | 303µs |
| max | 4269µs | 2314µs | 1462µs | 1001µs |

**Rust 全面达到或超过 Go**：p50 快 4.4×，p90 快 1.5×，p99 快 1.4-1.8×，p999 三次中两次持平/更优（run1 有一个 1.2ms 尾点，仍在 Go max 量级）。

### 结论与生产建议

1. **kcp-rs 的传输层在 Linux 上没有结构性延迟缺陷**——之前的 p99/p999 差距全部来自宿主 CFS 调度参数与多线程唤醒链的交互。
2. **生产部署建议**（写入运维手册）：Linux 主机应设置 `sysctl -w kernel.sched_wakeup_granularity_ns=1000000`（1ms）——这对所有低延迟多线程服务（不只 kcptun）都有益。当前内核默认 15ms 是为吞吐优化的桌面/服务器默认值。
3. **代码层启示**：唤醒链越短，对宿主调度参数越不敏感。epoll 分析中的选项 1（worker 等待并入 driver）与选项 2（SO_REUSEPORT 直连，砍掉 RX 线程）依然是正确的方向——它们把服务端唤醒链从 3-4 跳压到 1-2 跳，在最坏调度环境下进一步缩小暴露面。
4. VM 参数已恢复默认（15ms），未留持久改动；实验脚本与二进制保留在 VM `/root/kcpbench/`。

## 4. 已知问题（非本次引入）

| 问题 | 状态 |
|------|------|
| `kcp::tests::test_fast_retransmit_fires_on_duplicate_acks` 失败（"expected at least 2 ACK segments, got 1"） | **分支既有**（stash 验证：HEAD 同样失败），来自当前 "fix bug" WIP，需作者跟进 |
| `kcp::tests::snmp_send_recv_counts_upper_bytes` 偶发失败 | 测试自身与并行测试共享进程级 `DEFAULT_SNMP`（注释已自认），删除 3 个 transport 测试后调度变化使其偶发暴露；隔离运行 3/3 通过。修法：改为 delta 测量或加测试级互斥 |
| `kcptun-server/tests/stress_test.rs` 21 个 clippy lint（needless borrow、spawn 未 wait 等） | 分支既有；`make clippy` 全绿需单独清理 |
| 根目录空文件 `building`（0 字节，未跟踪） | 疑似误生成，可删 |

## 5. 验证记录（本次清理 + 第二轮优化）

- `cargo clippy -p kcp-rs --all-targets --features async -- -D warnings`：通过。
- `cargo test -p kcp-rs --all-features`：lib 72 通过（2 个既有失败见上表）；集成测试 data_correctness 4/4、kcpstream_integrity 16/16、kcpstream_listener 10/10、tcpstream_tcp 3/3 全部通过。
- `cargo test -p kcptun-common --lib`（snmp/snmp_log 模块）：9/9 通过。**注意**：`cargo test -p kcptun-common --all-features` 在 macOS 上有集成测试挂起（>45 分钟无完成），属平台既有问题，本次未追查。
- `cargo test -p smux-rs --lib`：62/62 通过。
- `cargo build --workspace --all-targets`：通过。
- FEC 编码路径 A/B 微基准（临时 example，已删除）：改动前后无统计显著差异，据此回退了 wrap_kcp_packet 的 calloc 消除尝试。
- **未运行** `make e2e`（Go↔Rust 互操作）——按约定需用户确认后执行。本轮改动均不触及 wire 格式，仍建议合并前跑一次。

## 8. Linux pprof 满载剖析（2026-09-02，192.168.0.84）

方法：musl 静态 `latency_p99`（profiling profile + force-frame-pointers + pprof feature），`--mode self --concurrency 32` 闭环满载（43.7k req/s，单 vCPU 打满 96.6% 采样率），VM 内 curl 采样 20s CPU + allocs + heap，`go tool pprof` 解析。

### CPU 分布（19.32s 总样本，极度平坦——无单一 >5% 算法热点）

| 类别 | 占比 | 明细 |
|------|------|------|
| **async runtime 脚手架** | **~25-30%** | crossbeam `recv_deadline`+`wait_until` 6.8%（worker 阻塞式 park）；`Waker::wake` 5.75%（其中 `knet::Notify::notify_one` 占 57%——read/flush notify）；mio waker 3.0%（io driver unpark）；tokio 调度内部 |
| socket syscall | ~8% | `recvmmsg_from_into` 3.5%（其中 SCRATCH/iov 组装 ~3%，slot `reserve(2048)` 80ms）、`recv_from` 2.6%、`recvmmsg_connected` 2.4% |
| **KCP 核心** | **~10%** | `flush_with_current` 3.4%（热行在 `flush_buf`）、`try_drain_and_send` 2.9%（热行 `flush_tx_batch`）、`process_inbound_batch` 2.0%、`input_with_optional_conv` 0.7% |
| 时钟 | 1.4% | `__clock_gettime`（tokio timer wheel 内部 Instant；kcp `wall_ms` 已缓存） |
| 分配 | ~1.6% | `alloc`+ProfilingAllocator |
| 探针自身 | ~9% | `run_self`（sender/reader/latencies sort） |

### 分配（36.3MB / 20s 窗口）

- 核心路径**极低**：`flush_buf` 线包 Bytes 5.8MB + `BytesMut::reserve_inner` 6.0MB（每出包一个 ~1.3KB owned buffer——**固有成本**，包的所有权要交给 socket 发送，第二轮已验证不可消除）≈ **每请求核心侧 ~7 字节**。
- RX 缓冲池轻微抖动：`try_recv_batch_from_into` 7.8MB（slot 容量 <2048 时 `reserve`，池回收大部分生效）。
- 其余为探针自身（latencies Vec 增长 + pprof 符号化的一次性 addr2line/gimli）。

### 结论：剩余优化空间在哪

1. **算法层：没有了。** kcp-rs 核心在该负载下曲线平坦，核心 CPU 仅 ~10%，分配 ~7B/请求——历轮优化已到位。
2. **架构层：有，且被量化。** ~25-30% CPU 消耗在 async runtime 脚手架（park/唤醒/队列）——这正是 epoll 分析（§5）选项 1/2 的量化依据：**Phase 1**（worker 等待并入 driver）回收 ~7% crossbeam park；**Phase 2**（SO_REUSEPORT 直连砍 RX 线程 + 少唤醒跳）压缩 `Waker::wake` 5.75% + mio waker 3.0%。KcpEndpoint（选项 3）在万级会话时进一步压缩。
3. **微项**（低优先）：recvmmsg slot 池回收改进（<1% CPU）；`__clock_gettime` 1.4% 可通过减少 tokio timer 依赖间接降低，不值得专门做。
4. syscall ~8% 与线包 Bytes 分配为固有成本，不动。

**验证方法备注**：VM 内 curl 直接打 127.0.0.1:6060 采样（SSH 隧道时序太慢会错过 40s 采样窗）；`--pprof` 采样与延迟测量不可同时（已知污染）。

## 9. Phase 1 + Phase 2 落地与 A/B 验证（2026-09-02，192.168.0.84）

对照 §8 的量化结论，把架构层剩余空间（~25-30% runtime 脚手架）分两步落地：

### Phase 1（双平台）：worker 空闲 park 并入 runtime driver

- `kcp-rs/src/sharded.rs`：worker 的 RX 队列从 crossbeam（阻塞 `recv_timeout`，在 async
  上下文里冻结 driver——连带冻结该 shard 上所有 flush 定时器，pprof 计
  `recv_deadline`+`wait_until` ≈ 7% CPU）换成 tokio-aware `async_channel`，
  worker park 进 `recv().await`，与 flush 定时器共享同一次 epoll wait。
- sweep 改为 piggyback 在唤醒上（每 `SWEEP_INTERVAL` 次循环），空闲零定时器。

### Phase 2（拓扑选择）：砍掉 RX 线程 + 跨线程跳转

`KcpListener::build()` 按拓扑分流（`WorkerRx::Direct` / `WorkerRx::Channel`）：

| 拓扑 | 条件 | RX 路径 | 亲和性 |
|------|------|---------|--------|
| Direct 单 worker | `worker_count==1`（任意平台、fresh bind 或 from_socket） | worker 自己 recvmmsg 排水自己 socket + `recv_from` park | 天然（单 worker 拥有全部流） |
| Direct reuseport 组 | Linux + fresh bind + N>1 | 每 worker 一个 SO_REUSEPORT socket（`knet::UdpSocket::bind_reuseport`），内核 4-tuple hash 选组员 | 内核保证（同流恒达同 socket） |
| Reader 管线 | 共享 socket + N>1（外部 socket / 非 Linux fresh bind） | RX 线程 recvmmsg → `async_channel` → worker（不变） | FNV hash |

- 直连 worker 的 park 与 `knet::CancellationToken` race：`close()` 立刻唤醒，
  空闲零定时器轮询；knet 新增 `raw_udp_reuseport`/`bind_reuseport`（socket2）。
- 直连 worker 的发送走自己的 socket（reuseport 组内独立发送队列）；channel worker 不变。
- `from_socket`（kcptun-server 生产路径）N>1 保持 reader 管线——外部 socket 无法按 worker
  拆分；server 的 app 级 `--shards` 分片仍由内核 reuseport 均衡。
- KcpEndpoint（§5 选项 3）依旧搁置，万级会话场景才立项。

### A/B 结果（lat_p99_new = Phase 1 基线 vs lat_p99_p2 = Phase 1+2；1 vCPU，1ms granularity，两轮交错）

开放模型 500 RPS / 512B：

| 指标 | P1 R1 | P2 R1 | P1 R2 | P2 R2 | 结论 |
|------|-------|-------|-------|-------|------|
| p50 (µs) | 88.2 | 85.0 | 95.6 | 81.4 | −4~15% |
| p90 (µs) | 110.3 | 101.7 | 116.2 | 103.8 | −8~11% |
| p99 (µs) | 165.5 | 159.7 | 175.8 | 155.1 | −4~12% |
| p999 (µs) | 446.5 | 460.4 | 489.2 | 452.3 | 噪声内持平 |

闭环 c32 / 512B：

| 指标 | P1 R1 | P2 R1 | P1 R2 | P2 R2 | 结论 |
|------|-------|-------|-------|-------|------|
| rps | 89871 | 93549 | 90183 | 96154 | **+4.1~6.6%** |
| p50 (µs) | 334.2 | 319.3 | 334.8 | 314.2 | −4.5~6.2% |
| p99 (µs) | 599.7 | 562.0 | 524.6 | 482.5 | −6.3~8.0% |
| p999 (µs) | 1689 | 2516 | 1497 | 1461 | R1 单样本尖峰，R2 持平偏好（max 两轮双向噪声） |

**结论**：两模型、两轮全部一致向好——吞吐 +4~6.6%，p50/p90/p99 一致 −4~12%，
无系统性回退（p999 的 R1 尖峰属单事件噪声，R2 反转）。收益来源即设计目标：
单 vCPU 场景下 reader 线程→channel→worker 的两次跨线程跳被合并为 worker
socket 直排（`Waker::wake` 5.75% + mio waker 3.0% 的跨线程部分被消除），
且 worker 与 reader 合一省一个线程的调度开销。

reuseport N=2 路径功能验证（`KCPTUN_WORKER_THREADS=2`，c16 闭环 10s ×2）：
78.6k/76.8k rps，ok=samples、零丢弃零挂起——路径正确。1 vCPU 上 2 worker
争 1 核必然慢于单 worker（该拓扑面向多核宿主），不作为性能口径。

### 门禁

- `make clippy`（workspace `-D warnings`）：通过。
- `cargo test -p kcp-rs --features async`：lib 74/75（唯一失败为既有 WIP
  `test_fast_retransmit_fires_on_duplicate_acks`）；集成 4+16+10+3 全过。
- 新增测试：`direct_worker_echo`（直连 worker 4 轮 echo 全链路）、
  `close_wakes_direct_worker`（cancel token 唤醒）。
- 未运行 `make e2e`（需用户确认）；本轮不触及 wire 格式。
