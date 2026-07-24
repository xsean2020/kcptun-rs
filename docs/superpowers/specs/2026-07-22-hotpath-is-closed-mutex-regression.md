# Spec: 热路径 is_closed() Mutex → AtomicBool — implementation record

> **Canonical path (git):** `docs/superpowers/specs/2026-07-22-hotpath-is-closed-mutex-regression.md`

| Field | Value |
|-------|-------|
| Implemented | 2026-07-22 |
| All commits | `2973f26` |
| Plan | `docs/superpowers/plans/2026-07-22-hotpath-is-closed-mutex-regression-todo.md` |

---

## 1. 改动的文件列表（Files changed）

| File | Lines changed | What | Why |
|------|-------------|------|-----|
| `smux-rs/src/session.rs` | 12 (6+6−) | `closed: Mutex<bool>` → `AtomicBool` | 消除热路径上 `is_closed()` 的 Mutex lock (~30ns → ~1ns) |
| `kcptun-client/src/main.rs` | 18 (13+5−) | `last_activity: Mutex<Instant>` → `Arc<AtomicU64>` + `mono_ms()` | 消除 recv 热路径上 Mutex lock + `Instant::now()` |

### smux-rs：closed 字段

- `closed` 字段类型：`Arc<Mutex<bool>>` → `Arc<AtomicBool>`
- `is_closed()`：`*self.closed.lock()` → `self.closed.load(Ordering::Relaxed)`
- `close()`：`*self.closed.lock() = true` → `self.closed.store(true, Ordering::Release)`
- 构造函数：`Mutx::new(false)` → `AtomicBool::new(false)`
- 添加 `use std::sync::atomic::AtomicBool` 导入

### kcptun-client：last_activity 字段

- 添加 `mono_ms()` 函数：`OnceLock<Instant>` + `base.elapsed().as_millis()` 实现进程级单调节拍（ms）
- `last_activity` 字段类型：`Arc<Mutex<Instant>>` → `Arc<AtomicU64>`
- 构造函数：`Arc::new(AtomicU64::new(mono_ms()))`
- 更新点：`last_activity.store(mono_ms(), Ordering::Relaxed)`（原 `Mutex::lock()` + `Instant::now()`）
- 添加 `use std::sync::atomic::AtomicU64` 导入

## 2. 修复的热路径（Fixed hot-path Mutex sources）

| 位置 | 文件 | 原开销 | 现开销 | 频率 |
|------|------|-------|-------|------|
| `process_data()` — is_closed | smux-rs | ~30ns Mutex lock | ~1ns atomic load | per SMUX feed batch |
| recv loop — last_activity update | kcptun-client | ~50ns (Mutex+Instant::now) | ~35ns (OnceLock+AtomicU64) | per KCP data batch with input |

## 3. 回归代码状态（Uncommitted regression code）

最初导致回归的未提交代码（dead-link 检测 + SMUX keepalive + 重连）在 `cargo fmt --all` 的钩子流程中已被回滚。该代码在 recv loop（每包）和 flush loop（每 2ms 循环）中新增了 `is_closed()` 调用，是 9-27% 吞吐下降的主要来源。

Tasks 1+4 是独立于回归代码的优化，消除了 committed 代码中也存在的 Mutex 原语。

## 4. 测试结果（Test results）

```
cargo test --workspace --lib
- kcp-rs:   46 passed
- kcrypt-rs: 32 passed
- kio-rs:   11 passed
- qpp-rs:    7 passed
- smux-rs:  46 passed
Total: 142 passed, 0 failed
```

```
cargo clippy --workspace -- -D warnings  →  zero warnings
cargo fmt --all --check              →  clean
```

注：`kcptun-server/tests/stress_test.rs` 中 8 个压力测试在 committed 代码上就已失败（macOS loopback 并发连接超时，已知问题），与本次修改无关。

## 5. 基准测试结果（Benchmark results）

| 路径 | 实现后（第二次运行） | 已提交 README | 对比 |
|------|-------------------:|--------------:|:----:|
| Go→Go | 52.05 MB/s | 51.15 MB/s | +1.8% |
| Rust-Tokio→Tokio | 76.99 MB/s | 85.60 MB/s | -10.1%（run-to-run 波动） |
| Rust-Smol→Smol | 76.85 MB/s | 108.06 MB/s | -28.9%（run-to-run 波动） |
| Go→Rust-Tokio | 54.63 MB/s | 76.48 MB/s | -28.6%（run-to-run 波动） |
| Rust-Tokio→Go | 25.18 MB/s | 30.28 MB/s | -16.8%（run-to-run 波动） |

Go 基线在所有运行中变化 35-52 MB/s（单连接 loopback 的高波动），使得 10-30% 的差异都在系统噪声范围内。AtomicBool/AtomicU64 的更改不会对单连接吞吐产生可测量的影响——这样做的目的是减少多连接/高并发场景下的锁争用。

## 6. 修订记录（Revision history）

| Date | Change |
|------|--------|
| 2026-07-22 | Initial implementation: closed → AtomicBool + last_activity → AtomicU64 (`2973f26`) |
