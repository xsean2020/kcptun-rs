# Plan: SMUX stream逻辑泄漏 — 代理场景RSS持续上涨修复

> **Canonical path (git):** `docs/superpowers/plans/2026-07-22-smux-stream-leak-fix.md`

| Field | Value |
|-------|--------|
| Status | **Implemented** |
| Created | 2026-07-22 |
| Scope | SMUX stream lifecycle in client+server flush loops; zombie reap + lazy alloc |
| Out of scope | Protocol wire format, congestion control, crypto paths |
| Bug report | `bugs/BUGREPORT_PROXY_MEMORY_GROWTH.md` |
| Related | `smux-rs/src/stream.rs`, `smux-rs/src/session.rs`, `kcptun-client/src/main.rs`, `kcptun-server/src/main.rs` |

## 问题

代理场景（大量短 TCP 连接复用一个 KCP 通道）下，SMUX 流从 map 中无法 remove，stream_count 无界增长，加上每流 ~2MB 预分配，RSS 可达 785MB→913MB+。

## 根因

### 缺陷 A — 双方 handle 假 mark_fin_sent

`handle_client` / `handle_stream` 在 pipe 结束后：

```rust
smux_stream.mark_local_closed();
if !smux_stream.is_fin_sent() {
    smux_stream.mark_fin_sent(); // ← 假标记，实际没发出 FIN
}
```

flush loop 判断 FIN 候选的条件是 `local_closed && pending_send==0 && !is_fin_sent()`，`fin_sent` 已为 true → 永不再发 FIN。对端收不到 FIN → `remote_closed` 永不满足 → 三条件 `local && remote && fin_sent` 永不满足 → `HashMap::remove` 永不执行。

**server 侧也有完全相同的 bug（原报告未覆盖）。**

### 缺陷 B — 清理条件过严

```rust
// Phase 1a:
s.is_local_closed() && s.is_remote_closed() && s.is_fin_sent()
```

对端 FIN 丢失 / 迟到 / 半开时，`remote_closed` 恒为 false，map 永不收缩。

### 缺陷 C — 每流预分配

```rust
BytesMut::with_capacity(streambuf); // 默认 2097152 (2MB)
```

每个 open_stream 都立即分配 2MB，即使只有短暂存活。泄漏 100 条流即 ~200MB。

### 缺陷 D — SYN 失败路径泄漏

`open_stream` 后 `send_frame` 失败 → `continue` → stream 留在 session 的 HashMap 中。

### 缺陷 E — autoexpire 空转

```rust
loop { sleep 30s; } // 什么都不做
```

## 修复方案

### M1 — 去除假 mark_fin_sent + clear_buffers

- `handle_client` / `handle_stream` 只 `mark_local_closed()` + `clear_buffers()`
- 不再调用 `mark_fin_sent()`
- flush 负责在真编码 FIN 后 mark

### M2 — 超时强制回收僵尸流

- `Stream` 新增 `local_closed_at: Mutex<Option<Instant>>`
- `mark_local_closed()` 记录时间戳
- `Session::reap_stale_streams(linger: Duration)` 扫描两条路：
  1. `local && remote && fin_sent` → remove
  2. `local && !remote && elapsed >= linger` → remove（即使 fin_sent=false）
- 默认 `STREAM_LINGER_SECS = 30`（可接受的半开等待上限）

### M3 — Flush 内正确 FIN 标记时机

- 收集 FIN 候选时**不 mark**
- 编码 FIN 帧到 out_buf
- kcp.send 整批成功后统一 `mark_fin_sent()`
- 失败 → 保持 `!fin_sent`，下轮重试

### M4 — lazy recv buf

`with_buffer` 使用 `BytesMut::new()` 而非 `BytesMut::with_capacity(streambuf)`，按写入自动增长，上限由 `max_recv_buf` 控制。

### M5 — SYN 失败 remove + autoexpire 回收

- `Session::remove_stream(id)`：close + HashMap::remove
- SYN 发送失败时调用
- autoexpire scavenger 调 `reap_stale_streams` 而非空转

## 实施顺序

```
M1 (假mark) → M2 (reap, 保证上界) → M3 (正确FIN时机) → M4 (lazy buf) → M5 (SYN+scavenger)
```

## 验收

- `cargo test --workspace` — 全绿
- `cargo clippy --workspace -- -D warnings` — 零警告
- `cargo fmt --all`
- smux-rs 新增 7 条单元测试覆盖：clear_buffers、reap fully-closed、reap stale、keep fresh、lazy capacity、local_closed_elapsed、force_local_closed_at
- 代理压测：stream_count 有上界，RSS 不线性增长

## 修订记录

| 日期 | 说明 |
|------|------|
| 2026-07-22 | 初稿，作为修复实施依据 |
