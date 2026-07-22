<!-- Generated: 2026-07-20 | Final plan revision: 2026-07-20 -->

# kcptun-rs 最终性能优化方案（定稿）

> **状态：** 执行中 — P0 与 P1 主体已落地；bulk 吞吐已达 **~1.2–1.35× Go**（见 CHANGELOG Unreleased）。  
> **硬约束：** 保持与 Go kcptun / kcp-go v5 **全 wire 兼容**；`cargo clippy -D warnings`；e2e + stress 绿。  
> **原则：** 用尽 Rust 所有权 / 单态 / 无锁共享 / 批处理；**先对称与零拷贝，再内核旁路**；一次一类优化 + 基准门禁。

---

## 0. 文档定位

| 文档 | 用途 |
|------|------|
| **本文** | **最终优化方案定稿**：现状、已完成、剩余路线、验收、Rust 落点 |
| `CHANGELOG.md` | 已合并改动的事实记录 |
| `bench_results.json` | 最新数值快照（会随 `make bench` 刷新） |
| `AGENTS.md` / `CLAUDE.md` | 结构地图与工作纪律 |

本文取代早期「仅规划」叙事：以 **代码实况 + 最新 bulk 结果** 为准，给出下一阶段最优路径。

---

## 1. 目标与成功标准

### 1.1 产品目标

在 **不改变协议字节** 的前提下，使 Rust 实现（tokio 默认，smol 可选）：

1. **吞吐** ≥ 同机 Go 同配置（默认 `null` / `aes`，fast 模式，FEC off）  
2. **时延** ≤ 同机 Go 的 **1.10×**（应用层 RTT / bulk 完成时间）  
3. **正确性** e2e 全矩阵 + stress 数据完整性  
4. **双 runtime** 行为一致（`make test-both` / `make e2e`）

### 1.2 可验收 KPI（定稿）

| 指标 | 目标 | 当前（CHANGELOG bulk，loopback） | 判定 |
|------|------|----------------------------------|------|
| null/nocomp 吞吐 | ≥ **1.05×** Go | **~1.43×**（~68.8 vs ~48.1 MB/s） | ✅ 已达标量级 |
| aes 吞吐 | ≥ **1.00×** Go | **~1.43×**（~68.8 vs ~48.1 MB/s） | ✅ 已达标量级 |
| 小消息/时延类 bench | lat ≤ **1.10×** Go | 随配置波动；重加密 3des/tea 仍可能落后 | ⚠️ 持续盯 |
| 3des/tea 等重加密 | thr ≥ **0.90×** Go | 部分配置仍 &lt;1.0× | 🔄 剩余优化 |
| stress / e2e | 全绿 | 以 CI/本地脚本为准 | 门禁 |

> 注：`bench_results.json` 可能是 **短连接/小包** 场景，与 **bulk 大流** 数字不同；两者都要看。KPI 以 **bulk（`BENCH_DATA_MB` 较大）+ 时延脚本** 双轨验收。

### 1.3 硬约束（不可破）

- CFB 固定 `GO_CFB_IV`；包头 CRC32 IEEE；Snappy **CRC32C**  
- `null` 无加密头 / `none` 有头无加密  
- KCP：每 Push ACK；`snd_buf` front 清理；段头 24B LE  
- SMUX v1/v2（含 v2 peer window / UPD）  
- 密钥 PBKDF2-HMAC-SHA1，salt `kcp-go`  
- 不改默认拥塞语义去「作弊」吞吐  

---

## 2. 体系结构与热路径模型

### 2.1 协议栈

```
TCP ⇄ SMUX Stream (+ optional QPP)
        ⇄ SMUX Session
        ⇄ Snappy (session-level, optional)
        ⇄ KCP ARQ
        ⇄ BlockCrypt / AEAD (+ optional FEC)
        ⇄ UDP
```

### 2.2 单会话数据面（出站）

```
TCP read (64KB pipe)
  → SMUX Stream.send_buf (VecDeque<Bytes>)
  → flush: multi-frame drain (≤64KiB) → encode_header_into + in-place payload
  → [Snappy outside KCP lock]
  → KCP::send + flush (short lock)
  → raw_packets → encrypt_batch (conditional cpu_block / parallel)
  → UdpSocket::send_batch
```

**时延关键路径：** `poll_write` 唤醒 → flush → 加密 → UDP → 对端 ACK → `write_notify`。

### 2.3 性能预算直觉

| 层级 | 主导成本 | Rust 应对 |
|------|----------|-----------|
| 调度 | 无谓 `cpu_block` / sleep(1) 回压 | 条件内联；`write_notify` 事件回压 |
| 拷贝 | `to_vec` / 多缓冲 | `Bytes`/`BytesMut` 单缓冲；null move |
| 锁 | KCP + SMUX 多锁 | 缩短 KCP 临界区；send 队列 Bytes |
| 加密 | dyn 虚表 + 重算法 | 并行批加密；远期 enum 单态 |
| 系统调用 | 逐包 send | `send_batch`（try_send 循环） |
| 流控 | 缺 peer window 会假死 | SMUX v2 peer_window（已修） |

---

## 3. 现状水位（2026-07-20）

### 3.1 已达成（相对早期基线）

| 阶段 | 结果 |
|------|------|
| 早期 | null bulk ~0.25–0.35× Go；client 加密落后 server；flush 多拷贝 |
| P0+P1 主体 + 回压/peer window | bulk **~1.2–1.35× Go**；client/server 加密路径对齐 |
| 算法层 | SM4 远超 Go；AES-NI 路径可用；CFB 泛型内联 |

### 3.2 已落地清单（代码事实）

#### 构建 / 运行时

- [x] Release：`opt-level=3` + LTO + `codegen-units=1` + `panic=abort` + strip  
- [x] `mimalloc`  
- [x] 双 runtime：tokio / smol；smol 持久 `cpu_block` 池 + 多线程 executor  
- [x] 64KB pipe；事件驱动 flush（`Notify` + `next_update`）  

#### P0 — 数据面对称与去税

- [x] **P0.1** Client `Arc<dyn BlockCrypt/AeadCrypt>`（去 Mutex）  
- [x] **P0.2** `should_cpu_block_encrypt` + 条件 `cpu_block`  
- [x] **P0.3** `encode_header_into` + 单 `out_buf` 组帧（去 to_vec 链）  
- [x] **P0.4** Client Snappy **KCP 锁外**  
- [x] **P0.5** 共享 `kcp_rs::encrypt_batch` / `should_cpu_block_encrypt`  

#### P1 — 批处理与减分配

- [x] **P1.1 部分** null：`Bytes::from(Vec)` move；小批 `encrypt_cfb` 复用缓冲；大批 prepare+并行  
- [x] **P1.2a** `UdpSocket::send_batch` / `send_batch_to`（tokio try_send+writable）  
- [x] **P1.3 部分** 入站 CFB scratch / header drain  
- [x] **P1.4 部分** SMUX `send_buf: VecDeque<Bytes>` + `write_bytes`  
- [x] **P1.5** `AeadCrypt::seal_into` + GCM 计数 nonce  

#### 流控 / 回压（bulk 突破关键）

- [x] Client `write_notify` 事件回压（替代 sleep(1)）  
- [x] SMUX v2 **peer_window / peer_consumed**（对齐 Go，避免 ~256KiB 卡死）  
- [x] 每周期 multi-frame drain ≤64KiB  
- [x] P2.2：`next_update=1` when pending；server interval 2ms  

### 3.3 仍未完成 / 收益递减项

| ID | 项 | 优先级 | 说明 |
|----|-----|--------|------|
| R1 | `CryptEngine` enum 静态分发 | ✅ 已完成（P3） | 去掉热路径 `dyn` 虚表 |
| R2 | KCP `output` → `Bytes` 所有权 / `raw_packets: Vec<Bytes>` | ✅ 已完成（P4） | 少一次 output 侧拷贝；bulk ~1.43× Go |
| R3 | Linux `sendmmsg`/`recvmmsg` | 中（平台） | 高 pps；loopback 收益有限 |
| R4 | SMUX 收端多锁合并 `StreamInner` | 中 | 读路径；写已部分优化 |
| R5 | KCP 载荷 `Bytes` 重传零拷贝 | 中 | 重传场景 |
| R6 | `input` 单段栈解析 | 低–中 | 微优化 |
| R7 | 热路径 metrics / empty_flush | 中（可观测） | 防回归 |
| R8 | criterion 每 cipher 基准 | 低 | 对比 Go |
| R9 | PGO / `target-cpu=native` | 可选发布 | 非默认 |
| R10 | io_uring / GSO / DPDK | 低 / 不做默认 | 复杂度高 |

---

## 4. 最终技术路线（定稿）

### 4.1 总策略（一句话）

> **所有权贯通的单缓冲数据面 + 条件调度 + 批发送 + 静态分发加密；流控与回压对齐 Go；内核旁路仅作可选加速。**

### 4.2 分层（L1 已完成 → L2 收尾 → L3 可选）

```
L1 对称与去税     ████████████ 完成 (P0 + 回压/peer window)
L2 零拷贝与批处理  ████████░░░░ 大部分完成；余 R1–R5
L3 平台/发布极限   ██░░░░░░░░░░ 按需 (sendmmsg, PGO, native)
```

---

## 5. 剩余工作详细设计

### 5.1 R1 — `CryptEngine` 枚举静态分发（下一优先）

**问题：** 热路径 `crypt.encrypt` 经 `dyn BlockCrypt` 虚表；CFB 每包上百 block。

**设计：**

```rust
// kcrypt-rs
pub enum CryptEngine {
    Null,
    NoneHeader, // 行为由上层 header 策略配合
    Aes128(AesCfbCrypt),
    Aes192(AesCfbCrypt),
    Aes256(AesCfbCrypt),
    Sm4(Sm4Crypt),
    // ... 全部 CFB 变体
}

impl CryptEngine {
    #[inline(always)]
    pub fn encrypt_payload(&self, data: &mut [u8]) {
        match self {
            Self::Aes128(c) => c.encrypt(data), // monomorphized CFB
            Self::Null => {}
            // ...
        }
    }
}
```

- `select_block_crypt` 可返回 `CryptEngine` 或保留 `Box<dyn>` 给测试  
- `encrypt_batch` 改为泛型或 `CryptEngine`  
- **兼容：** wire 不变；仅调用约定  

**验收：** 3des/tea/xtea 等重加密 thr 提升；null 不变差；e2e 全 crypt。

### 5.2 R2 — KCP 输出 `Bytes` 管线

**现状：** `output: FnMut(&[u8])` → pool `Vec` extend → encrypt 再读。

**目标：**

```text
flush: buffer.split().freeze() → raw_packets: Vec<Bytes>
encrypt_batch: 对 Bytes 就地或 prepare 一次
send_batch(&[Bytes])
```

可选：`set_output_bytes(Box<dyn FnMut(Bytes)>)` 或会话 `OutputSink` trait。

**验收：** null 路径分配次数下降（dhat/heaptrack）；bulk 不回退。

### 5.3 R3 — Linux sendmmsg / recvmmsg ✅ 已实现

在 `kio-rs/src/net/mmsg.rs`：`sendmmsg_connected` / `sendmmsg_to` / `recvmmsg_from`。
tokio + smol 的 `send_batch{,_to}` 和 `try_recv_batch_from` 均走 mmsg。非 Linux 走 try_send / 顺序 fallback。

**验收：** 高 `sndwnd`、多会话 server CPU↓ 或 pps↑；macOS 走 fallback。

### 5.4 R4 — SMUX `StreamInner` 单锁（收端）

```rust
struct StreamInner {
    state: StreamState,
    recv: VecDeque<Bytes>,
    // waker, fin flags...
}
// send 侧可保留独立锁或 SPSC，避免读写互堵
```

**验收：** smux 单测 + e2e smuxver 1/2；无死锁。

### 5.5 R5–R6 — KCP 微优化

| 项 | 做法 |
|----|------|
| `snd_buf` 扫描 | 已跳过 `acked`；确保 ACK 清理仍 front-pop |
| `encode` | [x] 24B LE header block + `#[inline(always)]` |
| `current_ms` | 每 flush 一次；避免重复 `SystemTime`（若现有每 seg 刷新则对齐 Go 或证明等价） |
| `acklist` | 预分配；大 ACK 突发复用 |
| Segment 载荷 | `Bytes` 替代 `BytesMut` 存只读载荷，重传零拷贝 |

**禁止：** 重写控制流「更 Rust」。

### 5.6 R7 — 可观测性（防回归）

- [x] `next_update`：有 outbound / `wait_send>0` 时强制 1ms；idle 才 clamp 到 max  
- [x] Server max interval 与 client 对齐为 2ms  
- [x] 测量：SNMP `EmptyFlush` 计数空 flush 周期

- `flush_cycles`, `empty_flush`, `encrypt_batch_inline` vs `offload`  
- `udp_send_batch_pkts`  
- 可选：`flush_us` 直方图（debug only）  

**验收：** 一次 bulk 能打印摘要；不进默认热路径重逻辑。

### 5.7 明确不做（默认）

| 项 | 原因 |
|----|------|
| 改 KCP 拥塞/默认窗口「刷分」 | 互通与公平 |
| 换 Snappy / 改 framing | wire 不兼容 |
| 默认 DPDK / AF_XDP | 超出项目定位 |
| 默认 PGO/native 唯一产物 | 分发矩阵 |
| 恢复固定 10ms flush | 时延回退 |
| 热路径 `info!` 日志 | 毁性能 |

---

## 6. Rust 特性映射表（必须用尽的）

| Rust 能力 | 本项目落点 | 状态 |
|-----------|------------|------|
| 所有权 / `Bytes` | SMUX send 队列、encrypt 输出、少 `to_vec` | 部分 ✅ |
| `BytesMut` 复用 | `out_buf`、`CryptoBuf`、`seal_into` | ✅ |
| 泛型 monomorphize | CFB `<F: Fn>`；远期 `CryptEngine` | 部分 ✅ |
| `Arc<dyn>` → enum | 无锁共享；再进化静态分发 | 过渡 ✅ |
| `thread::scope` | 批加密并行 | ✅ |
| `parking_lot` / atomics | 短临界区；peer_window | ✅ |
| `try_send` + writable | 批 UDP 少调度 | ✅ |
| feature 分 runtime | tokio 服务器 / smol 嵌入式 | ✅ |
| LTO + `codegen-units=1` | 跨 crate 内联 | ✅ |
| `mimalloc` | 分配尾延迟 | ✅ |
| 无 GC | 稳定 pps | ✅ |

---

## 7. 实施计划（剩余）

### 7.1 里程碑

| 里程碑 | 内容 | 退出条件 |
|--------|------|----------|
| **M-done** | P0+P1 主体 + 回压 + peer window | bulk ≥1.2× Go ✅ |
| **M-R1** | `CryptEngine` + encrypt_batch 接入 | 重加密 thr≥0.90×；e2e 绿 ✅ |
| **M-R2** | output Bytes 管线 | 分配↓；bulk 不回退 ✅ |
| **M-R4** | StreamInner（可选与 R2 并行） | smux e2e 绿 |
| **M-obs** | empty_flush / batch 计数 | 文档化如何读 |
| **M-opt** | sendmmsg / PGO / native | 仅发布或 Linux 包 |

### 7.2 建议顺序（严格）

- [x] SegmentPool / CryptoBuf / flush→next_update  
- [x] `encrypt_batch` / `should_cpu_block_encrypt` (P0.5)  
- [x] null-path `Bytes::from(Vec)` in encrypt_batch (partial P1.1)  
- [x] CFB small-batch `encrypt_cfb` reuse; large-batch prepare+parallel (P1.1)  
- [ ] 载荷 `Bytes` 重传（deferred: stream-mode append + pool; encode header micro-opt done）  
- [x] `input` header 栈上 parse（24B slice；payload 仍走 SegmentPool）

### 7.3 单 PR 纪律

- [x] send_buf `VecDeque<Bytes>` + `write_bytes`; recv path prefers Bytes queue (P1.4)  
- [x] frame encode 无中间 `Vec` (`encode_header_into` + in-place drain)  
- [ ] 统计：drain 字节、锁等待（debug feature）

### 3.4 `kio-rs`

- [x] `UdpSocket::send_batch` / `send_batch_to` (P1.2a; Linux P1.2b sendmmsg)  
- [x] Linux `recvmmsg` + try_recv_batch_from (server drain; client try_recv loop)  
- [x] `cpu_block`：短任务 inline 阈值（callers use `should_cpu_block_encrypt`; not a kio API)  
- [x] 保证 smol 多线程下 UDP 与 flush 不饿死（已修 ACK spawn——回归测试锁住）

### 3.5 binaries

- [x] P0 全部 (M1–M3 on worktree-perf-p0: 3a5bc15..f0e57ca)  
- [ ] 热路径 metrics：`flush_us`, `encrypt_us`, `udp_send_us`, `copies`  
- [x] 固定 flamegraph 流程：`bench/PROFILE_RUNBOOK.md` + `bench/profile_flamegraph.sh` + skill `flamegraph-perf`

---

## 8. 验证体系

### 8.1 命令

```bash
make release && make release-smol
make test-both
make clippy-both
make e2e                 # 或 bash test_e2e.sh
make stress              # release 后

# Bulk（吞吐）
BENCH_DATA_MB=50 bash bench/run_bench.sh

# 短连接/时延矩阵（更新 bench_results.json 的脚本）
# 以仓库现有 bench 入口为准
```

### 8.2 剖析

| 工具 | 用途 |
|------|------|
| `bash bench/profile_flamegraph.sh` / `make profile`（**samply**，macOS 首选） | CPU 热点 / Speedscope |
| `cargo flamegraph --bin kcptun-server` | CPU 热点（Linux / dtrace 备选） |
| `perf record -g` | 锁 / syscall（Linux） |
| `tokio-console` | 任务阻塞（tokio） |
| dhat / heaptrack | 分配次数（null 路径） |
| SNMP | 重传异常 = 回归 |

### 8.3 兼容检查表（每轮性能 PR）

- [ ] CFB `GO_CFB_IV`  
- [ ] null / none 头差异  
- [ ] Snappy CRC32C  
- [ ] SMUX v1 + v2 peer window  
- [ ] KCP ACK / snd_buf  
- [ ] tokio + smol  
- [ ] stress 1B–512KB  

---

## 9. 子系统检查表（定稿）

### 9.1 `kcrypt-rs`

- [x] CFB 泛型内联  
- [x] AEAD `seal_into` + counter nonce  
- [x] **`CryptEngine` enum**（P3 已实现）  
- [ ] criterion 矩阵  

### 9.2 `kcp-rs`

- [x] SegmentPool / CryptoBuf / next_update  
- [x] `encrypt_batch` / `should_cpu_block_encrypt`  
- [x] null move / 小批 encrypt_cfb  
- [x] output `Bytes`（R2 已完成：`KCP::output` 签名改为 `FnMut(Bytes)`，`encrypt_batch` 收 `Vec<Bytes>`，去掉 `BufferPool`）  
- [ ] 载荷 `Bytes` 重传  
- [~] input 快路径（header 栈解析✅；payload 仍 `extend_from_slice`）  

### 9.3 `smux-rs`

- [x] `encode_header_into`  
- [x] send `VecDeque<Bytes>`  
- [x] v2 peer_window  
- [ ] 收端单锁 / 统计  

### 9.4 `kio-rs`

- [x] `send_batch` / `send_batch_to`  
- [x] 条件 cpu_block（调用方）  
- [x] Linux sendmmsg/recvmmsg  
- [x] smol 饿死回归测试加固（JoinHandle detach + idle true-timeout）  

### 9.5 binaries

- [x] P0 全套对称  
- [x] 事件回压 + multi-frame drain  
- [ ] 热路径 metrics 输出  
- [x] flamegraph 一键脚本：`bench/profile_flamegraph.sh` / `make profile`  

---

## 10. 风险与缓解

| 风险 | 缓解 |
|------|------|
| client/server 再漂移 | 热路径只经 `encrypt_batch` 等共享 API |
| peer window 互通回归 | e2e 大文件 + 单测 `peer_window_limits_drain` |
| 并行加密顺序/nonce | prepare 串行持 `CryptoBuf` 锁；仅 encrypt 并行 |
| smol 调度延迟 ACK | 禁止 ACK 路径 `spawn` 发送；保持 direct await |
| 优化引入 alloc 抖动 | 小包路径强制 inline + 缓冲复用 |
| bench 噪音 | 3 轮中位；固定 CPU 频率说明 |

---

## 11. 预期收益（剩余项）

| 工作 | 相对 **当前** Rust | 说明 |
|------|-------------------|------|
| R1 CryptEngine | 重加密 +5–20% thr | null 影响小 |
| R2 Bytes output | 分配↓；尾延迟↓ | bulk 小幅 |
| R3 sendmmsg | 高 pps 场景 | loopback 可能不明显 |
| R4 StreamInner | 读密集小幅 | |
| R5–R6 | 1–5% | 需 flamegraph 验证 |
| R9 PGO/native | 5–15% 场景型 | 非可移植默认 |

**主目标已基本达成（bulk ≥ Go）。** 剩余以 **补齐重加密、降分配、可观测、平台极限** 为主，避免无目标大重构。

---

## 12. 决策摘要（给执行者）

1. **不要重做 P0/P1 已完成项**（见 §3.2）。  
2. **下一优先：R1 `CryptEngine`**，用 Rust 单态吃掉加密虚表。  
3. **并行加 R7 metrics**，守住 1.2× 水位。  
4. 再动 R2/R4；R3/R9 仅 Linux/发布需要时。  
5. 任何 PR：基准 → 改 → 基准 → e2e/stress → 记录 CHANGELOG。  
6. Wire 兼容与 `CLAUDE.md` 手术式改动纪律优先于「完美抽象」。

---

## 13. 附录 A — 关键代码锚点

| 能力 | 位置 |
|------|------|
| `encrypt_batch` / `should_cpu_block_encrypt` | `kcp-rs/src/crypto_buf.rs` |
| Client flush / 回压 | `kcptun-client/src/main.rs`（`KcpConn`, `SmuxStreamAsync`） |
| Server flush / feed | `kcptun-server/src/main.rs`（`KcpServerSession`） |
| `send_batch` | `kio-rs/src/net/{tokio,smol}.rs` |
| SMUX peer window / Bytes send | `smux-rs/src/stream.rs` |
| Frame 头原地编码 | `smux-rs/src/frame.rs` |
| AEAD seal_into | `kcrypt-rs/src/crypt.rs`, `crypt/aes_gcm.rs` |

## 14. 附录 B — 历史规划与本文关系

| 旧 ID | 状态 |
|-------|------|
| P0.1–P0.5 | ✅ 完成 |
| P1.1–P1.5 | ✅ 大部分；R2 为 P1.1 余量 |
| P1.2b/c | ✅ 完成（R3） |
| P2.2 | ✅ 完成 |
| P2.1/2.3–2.5 | 🔄 并入 R5–R6 / 低优先 |
| P3 | 🔄 R9–R10 可选 |

---

## 15. 修订记录

| 日期 | 说明 |
|------|------|
| 2026-07-20 | 初版极限方案（规划向） |
| 2026-07-20 | **定稿**：对齐已合并 P0/P1/回压/peer window；bulk ~1.2× Go；剩余 R1–R10 与验收 |

---

*本文为 kcptun-rs 性能工作的 **最终方案说明**。实现以 git 历史与 CHANGELOG 为准；数字以最新 bench 刷新 §1.2 / §3。*
