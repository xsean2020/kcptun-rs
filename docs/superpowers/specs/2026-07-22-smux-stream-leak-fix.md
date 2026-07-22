# Spec: SMUX stream memory leak fix — implementation record

> **Canonical path (git):** `docs/superpowers/specs/2026-07-22-smux-stream-leak-fix.md`

| Field | Value |
|-------|--------|
| Implemented | 2026-07-22 |
| All commits | single session, ahead of `origin/master` |
| Bug report | `bugs/BUGREPORT_PROXY_MEMORY_GROWTH.md` |

## 改动清单

### `smux-rs/src/stream.rs`

| 改动 | 原因 |
|------|------|
| 新增 `local_closed_at: Mutex<Option<Instant>>` | 记 `mark_local_closed` 时的 wall clock，用于 linger 判断 |
| 新增 `clear_buffers()` | 只清 send/recv 缓冲区，不伪造 `remote_closed` 或 `fin_sent` |
| 新增 `recv_buf_capacity()` | 测试和诊断用 |
| 新增 `local_closed_elapsed()` | 返回自 `mark_local_closed` 经过的时间 |
| 新增 `force_local_closed_at(at)` | 测试用，设为指定时间戳的本地关闭 |
| `mark_local_closed()` 扩展 | 同步 stamp `local_closed_at`，只 stamp 一次 |
| `with_buffer()` 修改 | `BytesMut::with_capacity(recv_capacity)` → `BytesMut::new()`（lazy） |
| `close()` 扩展 | 同步 stamp `local_closed_at` + 调 `clear_buffers()` |
| 新增 7 条单元测试 | clear 不假冒、lazy capacity、stamp elapsed、force_at |

### `smux-rs/src/session.rs`

| 改动 | 原因 |
|------|------|
| 新增 `remove_stream(id) -> bool` | 关闭 + HashMap::remove，供 SYN 失败 path |
| 新增 `reap_stale_streams(linger) -> Vec<u32>` | 清除已 fully-closed 或 local-closed 超时的流，返回仍需发 FIN 的 id |
| 新增 4 条单元测试 | remove、reap fully-closed、reap stale、keep fresh |

### `kcptun-client/src/main.rs`

| 改动 | 原因 |
|------|------|
| `handle_client` 结尾 | 只 `mark_local_closed + clear_buffers`，**删除 `mark_fin_sent`** |
| flush Phase 1a | 替换为：先收集 FIN candidates（不 mark）→ 编码 FIN 帧 → kcp.send 成功后 mark |
| flush Phase 1b | 加入 30s linger reap → remove + close 僵尸流 |
| SYN 失败路径 | `conn.session().remove_stream(smux_stream.id())` 修复泄漏 |
| autoexpire scavenger | 每 30s 调 `smux.reap_stale_streams(30s)` |
| kcp.send 成功回调 | `send_ok && !fin_candidates.is_empty()` → flush 后 `mark_fin_sent` |

### `kcptun-server/src/main.rs`

| 改动 | 原因 |
|------|------|
| `handle_stream` 结尾 | 只 `mark_local_closed + clear_buffers`，**删除 `mark_fin_sent`** |
| flush FIN collect | 去掉 `s.mark_fin_sent()` → 改为 **send 成功后统一 mark** |
| flush Phase 1a | 加入 30s linger reap + 同步清理 `handled_streams` |
| autoexpire scavenger | 每 30s 调 `smux.reap_stale_streams(30s)` |
| kcp.send 成功回调 | `send_ok && !fin_streams.is_empty()` → flush 后 `mark_fin_sent` |

### `kcp-rs/src/lib.rs`

| 改动 | 原因 |
|------|------|
| clippy lint 更新 | `manual_checked_ops` → `manual_hash_one` + `collapsible_else_if`（预存已有需求） |

## 修复的故障路径

1. **Client/server handle 假 mark_fin_sent** → 不再 blocking flush FIN 编码
2. **remote_closed 永不达成 → map 不 shrink** → 30s linger 兜底
3. **每流预分配 2MB → RSS 放大** → lazy BytesMut
4. **SYN 发送失败 → stream 残留 map** → remove_stream
5. **flush 中 mark_fin_sent 早于 kcp.send** → send 成功后才 mark，失败可重试
6. **autoexpire 空转** → 调 reap_stale_streams
7. **server 同款假 mark_fin_sent** → 对称修复

## 测试 coverage

- `cargo test --workspace` — 全部通过（包含 smux-rs 新增 11 条测试）
- `cargo clippy --workspace -- -D warnings` — 零警告
- `cargo fmt --all`

## 修订记录

| 日期 | 说明 |
|------|------|
| 2026-07-22 | 实现记录 |
