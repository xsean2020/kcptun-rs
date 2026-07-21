# Bug Report: 单 KCP 通道高并发死锁

> **严重级别**: Critical  
> **影响范围**: 客户端 `kcptun-client`，所有使用 `--conn 1`（单 KCP 通道）的高并发场景  
> **引入版本**: commit `67b3c0c`（perf: zero-copy optimizations）  
> **修复版本**: commit `3af239a`（fix: single KCP channel deadlock）  
> **发现日期**: 2026-07-19

---

## 1. 现象描述

### 1.1 触发条件

- 客户端使用 `--conn 1`（单 KCP 通道）
- 50~100 个 TCP 连接同时通过 SMUX 多路复用在这一个 KCP 通道上
- 每个连接发送 64KB~128KB 数据

### 1.2 表现

| 测试场景 | 预期 | 实际 |
|----------|------|------|
| 100 流 × 128KB 单次发送 | 100/100 通过 | 仅 28/100 通过，72 个 60s 超时 |
| 50 线程 × (64KB + 128KB) 串行双发 | 50/50 通过 | 仅 2/50 通过，48 个 120s 超时 |
| 10 流 × 128KB 单次发送 | 10/10 通过 | 仅 4/10 通过 |

**关键特征**: 成功的流在 0.04~0.07s 内完成；失败的流在超时时间内（60~300s）**一个字节都收不到**。这表明不是"慢"，而是**死锁**。

### 1.3 对比测试

作为对照，Go kcptun 在同样的单 KCP 通道配置下，所有测试**全部失败**（`short read 0/131072`），因为 Go 的 `--conn 1` 行为与 Rust 不同。Rust 在修复前至少能通过部分流，修复后 100% 通过。

---

## 2. 根因分析

### 2.1 直接原因：客户端 FIN 帧过早标记 `mark_fin_sent()`

Bug 存在于客户端 flush 循环的 Phase 1a 中。以下是**有 bug 的代码**（已在 commit `3af239a` 中移除）：

```rust
// ── Phase 1: Drain SMUX send buffers (NO KCP lock) ──
let pending: Vec<(u32, Vec<u8>)> = stream_map
    .iter()
    .filter_map(|(id, s)| {
        let mut drain_buf = bytes::BytesMut::new();
        let n = s.drain_send_max(&mut drain_buf, MAX_FRAME_SIZE);  // ← 从 stream buffer 移除数据
        ...
    })
    .collect();
drop(stream_map);

// ── Phase 1a: Collect FIN-pending streams ──  ← BUG 在这里
let fin_streams: Vec<u32> = stream_map.iter()
    .filter(|(_, s)| {
        s.is_local_closed()       // poll_shutdown 被调用过
            && s.pending_send() == 0  // ← Phase 1 刚 drain 完，当然为 0！
            && !s.is_fin_sent()
    })
    .map(|(id, s)| {
        s.mark_fin_sent();       // ← 过早标记！数据还没通过 kcp.send() 发出
        *id
    })
    .collect();

// 后续: encode PSH + FIN → kcp.send()  ← 如果失败，FIN 永远丢失
```

**问题链条**:

```
Phase 1: drain_send_max() 从 stream.send_buf 移除数据
    ↓
Phase 1a: 检查 pending_send() == 0  → true（因为 Phase 1 刚移除了！）
    ↓
Phase 1a: mark_fin_sent()  → 设置 AtomicBool = true
    ↓
Phase 1a cleanup: is_local_closed() && is_remote_closed() && is_fin_sent()
    → stream 从 session 中移除！
    ↓
Phase 2: kcp.send(raw_frames)  → 可能成功，也可能 TooManyFragments 失败
    ↓
如果 kcp.send() 失败 → FIN 帧丢失，但 mark_fin_sent() 已经为 true
    → FIN 永远不会被重发
    → 对端永远收不到 FIN
    → 对端的 stream 永远不会被清理
    → 流资源泄漏
```

### 2.2 为什么会导致死锁

单 KCP 通道下，100 个 SMUX 流共享一个 KCP 连接。当大量流调用 `shutdown(Write)` 时：

1. **FIN 帧丢失**: `mark_fin_sent()` 被调用但 FIN 帧实际未到达对端
2. **对端流泄漏**: 服务端对应的 SMUX 流永远收不到 FIN，`is_remote_closed()` 永远为 false
3. **流资源累积**: 泄漏的流占用 SMUX session 的 `streams` HashMap，每次 flush 循环都要遍历这些僵尸流
4. **KCP 窗口耗尽**: 泄漏流的发送缓冲区可能残留数据，flush 循环不断尝试 drain 这些数据，填满 KCP 的 `snd_queue`
5. **backpressure 恶化**: `wait_send()` 因为 `snd_queue` 堆积而远超 `snd_wnd`，`poll_write` 阻止所有新数据写入
6. **死锁形成**: 新的 TCP 连接无法建立（SYN 帧发不出去），已有的流无法完成（数据发不完），整个 KCP 通道死锁

### 2.3 为什么服务端没有这个问题

服务端的 flush 循环也有类似的 FIN 帧发送逻辑（行 942-952），但服务端从第一个版本就有 `KCP_MAX_FRAG` 分块（行 1050-1064），`kcp.send()` 不会因为 `TooManyFragments` 失败。而客户端在引入 FIN 帧发送逻辑时**缺少 `KCP_MAX_FRAG` 分块**，导致大数据量时 `kcp.send()` 失败，FIN 帧丢失。

此外，Go smux 的 `Close()` 方法（`stream.go:548`）使用 `writeFrameInternal()` **同步**发送 FIN 帧（阻塞直到写入 session 的 output channel），然后才调用 `streamClosed()`。而 Rust 客户端的 flush 循环是**异步批量**处理——先标记、后发送——两者之间存在失败窗口。

### 2.4 加剧因子

除了 FIN 帧问题外，以下因素加剧了死锁：

| 因子 | 说明 |
|------|------|
| `kcp.send()` 无分块 | 客户端 flush 循环合并所有 SMUX 帧为一个大 `kcp.send()`，超过 `KCP_MAX_FRAG * MSS` 时返回 `TooManyFragments`，数据被**丢弃** |
| `poll_write` 实时锁 KCP | `poll_write` 每次调用都 `kcp.lock().wait_send()`，100 个 writer task 同时竞争 KCP 锁，导致 flush 循环被阻塞 |
| `send_frame` 立即 flush | `send_frame`（SYN 帧）中调用 `kcp.flush()`，与 flush 循环的 `kcp.flush()` 冲突，加剧锁竞争 |

---

## 3. 修复方案

### 3.1 核心修复：移除客户端 FIN 帧发送逻辑

**原则**: 永远不要在操作确认成功前修改状态。

客户端不需要主动发送 FIN 帧，原因：
1. 服务端 flush 循环已有完整的 FIN 帧发送逻辑（Phase 1 收集 → Phase 2 编码 → Phase 4 `kcp.send()`）
2. 客户端 `poll_shutdown` 只需标记 `local_closed = true`
3. 服务端通过 `handle_stream` 的 `mark_local_closed()` + flush 循环发送 FIN
4. 双向 FIN 到位后，流在 Phase 1a 的 cleanup 中被移除

```diff
- // ── Phase 1a: Collect FIN-pending streams ──
- let fin_streams: Vec<u32> = stream_map.iter()
-     .filter(|(_, s)| {
-         s.is_local_closed()
-             && s.pending_send() == 0
-             && !s.is_fin_sent()
-     })
-     .map(|(id, s)| {
-         s.mark_fin_sent();  // ← 移除：过早标记
-         *id
-     })
-     .collect();
-
- // Encode FIN frames
- let fin_frames: Vec<u8> = fin_streams.iter().flat_map(|&stream_id| {
-     let fin = Frame::new(Cmd::Fin, stream_id, Bytes::new());
-     ...
- }).collect();
- raw_frames.extend_from_slice(&fin_frames);

+ // ── Phase 1a: Clean up fully closed streams ──
+ // FIN frames are sent by the server's flush loop, not the client.
+ {
+     let streams = smux2.streams();
+     let mut stream_map = streams.lock();
+     let to_remove: Vec<u32> = stream_map.iter()
+         .filter(|(_, s)| s.is_local_closed() && s.is_remote_closed() && s.is_fin_sent())
+         .map(|(id, _)| *id)
+         .collect();
+     for id in &to_remove { stream_map.remove(id); }
+ }
```

### 3.2 附加修复：KCP_MAX_FRAG 分块

客户端 flush 循环的 `kcp.send()` 缺少分块，当合并后的 SMUX 帧超过 `KCP_MAX_FRAG * MSS`（约 169KB）时返回 `TooManyFragments`，数据被丢弃。添加与服务端一致的分块逻辑：

```diff
- if let Err(e) = kcp_guard.send(&to_send) {
-     warn!("KCP send error: {:?}", e);
- }
+ // Split into chunks of at most (KCP_MAX_FRAG - 1) * MSS
+ let mss = kcp_guard.mss() as usize;
+ let max_chunk = (KCP_MAX_FRAG - 1) * mss;
+ let mut offset = 0;
+ while offset < to_send.len() {
+     let end = (offset + max_chunk).min(to_send.len());
+     if let Err(e) = kcp_guard.send(&to_send[offset..end]) {
+         warn!("KCP send error: {:?}", e);
+         break;
+     }
+     offset = end;
+ }
```

### 3.3 附加修复：`poll_write` backpressure 改回 AtomicUsize

`poll_write` 之前每次调用都 `kcp.lock().wait_send()`，100 个 writer task 同时竞争 KCP 锁。改回使用 flush 循环每 10ms 更新的共享 `AtomicUsize` 计数器：

```diff
- let ws = this.kcp.lock().wait_send();  // 每次都锁 KCP
+ let ws = this.wait_send.load(Ordering::Relaxed);  // 无锁读
  if ws >= this.snd_wnd {
      // sleep(5ms) + wake
      return Poll::Pending;
  }
```

### 3.4 附加修复：移除 `send_frame` 中的立即 flush

`send_frame`（SYN 帧发送）中的 `kcp.flush()` 与 flush 循环的 `kcp.flush()` 冲突，加剧锁竞争。移除后由 flush 循环统一处理：

```diff
  {
      let mut kcp = self.kcp.lock();
      kcp.send(&to_send)?;
-     kcp.flush();
  }
```

---

## 4. 测试验证

### 4.1 单 KCP 通道压力测试

测试脚本 `test_single_channel.py`（已清理，逻辑融入 stress_test.rs）:

```bash
# 100 流 × 128KB，单 KCP 通道，60s 超时
python3 test_single_channel.py 100 131072 60

# 期望输出:
# [Rust] OK=100/100  Fail=0  Time=3.3s
#   latency: min=0.04s avg=0.06s max=0.10s
```

### 4.2 串行双发测试

模拟"网页刷新"场景：每个线程发送 64KB 后 `shutdown(Write)`，再建新连接发送 128KB：

```bash
# 50 线程 × (64KB + 128KB)，单 KCP 通道，120s 超时
python3 test_serial_single.py 50 120

# 期望输出:
# === Results: 50 ok, 0 fail ===
```

### 4.3 Rust 集成测试

`kcptun-server/tests/stress_test.rs` 中的两个关键测试已恢复为 `--conn 1`：

```rust
fn test_multithread_large_data() {
    // Single KCP channel (--conn 1): 100 SMUX streams over 1 KCP connection
    let e = TestEnv::start_with_config(19044, 29944, 12994, "null", true, 1);
    // 100 threads × (64KB + 128KB) serial double-send
    ...
}

fn test_page_refresh_simulation() {
    // Single KCP channel: 80 SMUX streams over 1 KCP connection
    let e = TestEnv::start_with_config(19047, 29947, 12997, "null", true, 1);
    ...
}
```

```bash
# 运行全部 stress tests
cargo test -p kcptun-server --test stress_test --release -- --nocapture --test-threads=1

# 期望: 8 passed; 0 failed
```

### 4.4 修复前后对比

| 测试场景 | 修复前 | 修复后 |
|----------|--------|--------|
| 10 流 × 128KB 单通道 | 4/10 | **10/10** (0.4s) |
| 100 流 × 128KB 单通道 | 28/100 (60s 超时) | **100/100** (3.3s) |
| 50 线程串行双发 64KB+128KB | 2/50 (120s 超时) | **50/50** (10s) |
| 8 个 stress test (`--conn 1`) | 2 个失败 | **8/8 通过** (62s) |

---

## 5. 防止再次发生的措施

### 5.1 编码规范

**原则: 状态变更必须在操作确认成功后进行**

```rust
// ❌ 错误: 在 kcp.send() 之前就标记
s.mark_fin_sent();
kcp.send(&fin_frame)?;  // 如果失败，fin_sent 永远不会被重置

// ✅ 正确: 先发送，成功后再标记
kcp.send(&fin_frame)?;
s.mark_fin_sent();

// ✅ 最佳: 由对端处理，本地不发送
// 客户端只标记 local_closed，FIN 由服务端 flush 循环发送
```

### 5.2 测试覆盖

**单 KCP 通道压力测试必须作为 CI 的必跑项**：

```bash
# Makefile 中已添加
stress:
    cargo test -p kcptun-server --test stress_test --release -- --nocapture --test-threads=1
```

关键测试用例：
- `test_multithread_large_data`: 100 线程 × (64KB + 128KB) 串行双发，`--conn 1`
- `test_page_refresh_simulation`: 80 线程模拟网页刷新，`--conn 1`
- `test_multithread_100_connections`: 100 线程 × (1B + 4KB)，`--conn 1`

### 5.3 代码审查检查清单

在修改 flush 循环或 SMUX 流状态管理时，检查以下要点：

- [ ] `mark_fin_sent()` 是否在 `kcp.send()` **成功之后**调用？
- [ ] `kcp.send()` 是否有 `KCP_MAX_FRAG` 分块？
- [ ] `poll_write` 的 backpressure 是否使用无锁的 `AtomicUsize`（而非 `kcp.lock()`）？
- [ ] `send_frame` 中是否避免了 `kcp.flush()`（由 flush 循环统一处理）？
- [ ] 单 KCP 通道（`--conn 1`）下 100 流压力测试是否通过？

### 5.4 回归测试命令

```bash
# 快速验证（10 秒级）
python3 test_single_channel.py 100 131072 60

# 完整 stress test（60 秒级）
cargo test -p kcptun-server --test stress_test --release -- --nocapture --test-threads=1

# Go 互操作
bash test_e2e.sh
```

---

## 6. 相关文件

| 文件 | 说明 |
|------|------|
| `kcptun-client/src/main.rs` | 客户端主逻辑，flush 循环、poll_write、send_frame |
| `kcptun-server/src/main.rs` | 服务端主逻辑，flush 循环（含正确的 FIN 发送） |
| `kcptun-server/tests/stress_test.rs` | 压力测试，`--conn 1` 单通道场景 |
| `smux-rs/src/stream.rs` | SMUX 流状态管理，`mark_fin_sent()`、`is_local_closed()` 等 |
| `kcp-rs/src/kcp.rs` | KCP 协议，`send()`、`flush()`、`wait_send()` |

---

## 7. 时间线

| 时间 | 事件 |
|------|------|
| 早期 | 原始代码：客户端无 FIN 帧发送，100 流单通道 100% 通过 |
| commit `67b3c0c` | 引入 FIN 帧发送逻辑 + 其他优化，未加 `KCP_MAX_FRAG` 分块 |
| 测试发现 | 100 流单通道从 100% 降至 28%，串行双发从 100% 降至 2% |
| 调试过程 | 尝试 Phase 0 backpressure、drain 总量限制、Notify 通知等多种方案 |
| 定位根因 | 逐步移除改动，发现移除 FIN 帧发送后立即恢复 100% |
| commit `3af239a` | 移除客户端 FIN 帧发送 + 加 KCP_MAX_FRAG 分块 + 恢复 AtomicUsize backpressure |
| 验证 | 100 流单通道 100/100，串行双发 50/50，8 个 stress test 全通过 |
