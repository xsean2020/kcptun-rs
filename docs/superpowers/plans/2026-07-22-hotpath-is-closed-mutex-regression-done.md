# Plan: 热路径 is_closed() Mutex 回退修复

> **Canonical path (git):** `docs/superpowers/plans/2026-07-22-hotpath-is-closed-mutex-regression-todo.md`

| Field | Value |
|-------|-------|
| Status | **implemented** |
| Created | 2026-07-22 |
| Scope | 消除未提交代码在 flush loop / recv loop 热路径上新增的 `is_closed()` Mutex 锁，恢复 benchmark 吞吐至 committed README 水平 |
| Out of scope | TEA monomorphize（已完成）、重连逻辑（功能正确，保留）、keepalive AtomicU64（已是改进） |
| Related | `smux-rs/src/session.rs`、`kcptun-client/src/main.rs`、`kcptun-server/src/main.rs`、`README.md`、`bench/run_bench.sh` |

---

## 1. Problem

`bench/run_bench.sh` 结果显著低于 README 记录：

| 来源 | Rust-Tokio→Tokio | Rust-Smol→Smol | Go→Go |
|------|------------------:|---------------:|------:|
| committed README (HEAD `55afb88`) | 85.60 MB/s | 108.06 MB/s | 51.15 |
| working dir README (未提交) | 77.58 MB/s (-9%) | 78.79 MB/s (-27%) | 55.14 |
| 实测 100MB (当前代码) | 70.64 MB/s (-18%) | 59.06 MB/s (-45%) | 47.85 |

- `bench_results.json` 从 78 条（全 13 cipher）被覆盖为仅 6 条（tea）
- README 自身已更新承认 ~9-27% 回退，但实测进一步偏低

## 2. Root cause

未提交代码为实现 dead-link 检测 + SMUX keepalive + 自动重连，在**数据面热路径**上新增了 Mutex 锁和 `Instant::now()` 调用：

### 2.1 `smux.is_closed()` — Mutex 锁（最主要回退源）

`is_closed()` 实现：
```rust
pub fn is_closed(&self) -> bool {
    *self.closed.lock()   // parking_lot::Mutex<bool> lock+unlock
}
```

新增调用点：

| 位置 | 调用频率 | committed 版本 |
|------|---------|---------------|
| flush loop (client + server) | 每 ~2ms = ~500 次/秒 | **无** (新增) |
| recv loop (client) | 每个 UDP 包前 = ~57000 次/秒 | **无** (新增) |
| `process_data()` 开头 | 每个 UDP 包 = ~57000 次/秒 | 已有 (非新增) |

recv loop 最严重：77 MB/s、1400B MTU 下 ~57000 包/秒，每包一次 Mutex lock+unlock。

### 2.2 `update_activity()` — `Instant::now()` 调用

新增到 `process_data()` 末尾：
```rust
if saw_frame {
    self.update_activity();  // → mono_ms() → base.elapsed() → Instant::now()
}
```

~57000 包/秒 × ~30ns = ~1.7ms/s 额外开销。

### 2.3 Phase 0 健康检查（影响较小）

flush loop 每 50 周期（~100ms）一次：KCP lock + `is_dead()` + keepalive 检查。被 `health_checks_left` 节流到 ~10 次/秒，~1µs/s，可忽略。

### 2.4 定量汇总

| 开销源 | 频率 | 单次成本 | 总计/秒 |
|--------|------|---------|---------|
| flush loop `is_closed()` | 500/s | ~30ns | ~15µs |
| recv loop `is_closed()` | 57000/s | ~30ns | ~1.7ms |
| `update_activity()` | 57000/s | ~30ns | ~1.7ms |
| Phase 0 健康检查 | 10/s | ~100ns | ~1µs |
| **合计** | | | **~3.4ms/s (0.3%)** |

纯计算开销 0.3%，不足单独解释 9-30% 回退。叠加因素：
- 单连接 benchmark 噪声大（20MB:52, 100MB:70, 200MB:77-85）
- 系统级波动（Go→Go 也降 13%）
- README 数字来自"最佳"运行，难以精确复现

### 2.5 已是改进的改动（无需处理）

| 改动 | 影响 |
|------|------|
| SMUX keepalive `Mutex<Instant>` → `AtomicU64` | ✅ 更快 |
| TEA cipher monomorphize | ✅ 更快（不影响 AES bench） |
| Client 重连逻辑 | 功能正确，不在 bulk 热路径 |
| `kcp.rs` `is_dead()`/`state()` | 仅新增方法 |

## 3. 方案（Solution）

### P0: `closed: Arc<Mutex<bool>>` → `Arc<AtomicBool>`

核心修复——消除热路径上所有 `is_closed()` Mutex 锁：

**`smux-rs/src/session.rs`:**

```rust
// Before
closed: Arc<Mutex<bool>>,

// After
closed: Arc<AtomicBool>,
```

`is_closed()`:
```rust
// Before: Mutex lock (~30ns)
pub fn is_closed(&self) -> bool {
    *self.closed.lock()
}

// After: atomic load (~1ns)
pub fn is_closed(&self) -> bool {
    self.closed.load(Ordering::Relaxed)
}
```

`close()`:
```rust
// Before
pub fn close(&self) {
    *self.closed.lock() = true;
    // ...
}

// After
pub fn close(&self) {
    self.closed.store(true, Ordering::Release);
    // ...
}
```

构造函数（`new_client` / `new_server`）：
```rust
// Before
closed: Arc::new(Mutex::new(false)),

// After
closed: Arc::new(AtomicBool::new(false)),
```

### P1: 节流 recv loop `is_closed()` 检查

recv loop 不需要每个包检查 `is_closed()`，改为每 N 个包检查一次（匹配 flush loop 的 `health_checks_left` 模式）：

```rust
// recv loop
let mut closed_checks_left: u32 = 0;
loop {
    if closed_checks_left == 0 {
        closed_checks_left = 100; // ~100 packets per check
        if dead1.load(Ordering::Acquire) || smux1.is_closed() {
            break;
        }
    } else {
        closed_checks_left -= 1;
    }
    // ... udp.recv() ...
}
```

### P2（可选）: 恢复 bench_results.json 完整数据

当前 bench_results.json 只有 6 条 tea 记录，需重跑 `python3 bench_rust_vs_go.py` 恢复全部 78 条。

## 4. 实施顺序（Implementation order）

1. **P0: `closed` → `AtomicBool`** → verify: `cargo test --workspace -p smux-rs` + `cargo clippy --workspace -- -D warnings`
2. **P1: recv loop 节流** → verify: 同上
3. **Rebuild release** → `make release && make release-smol`
4. **Re-bench** → `BENCH_DATA_MB=200 BENCH_LATENCY_ITERS=50 bash bench/run_bench.sh`，对比修复前后
5. **恢复 bench_results.json**（可选 P2）→ `python3 bench_rust_vs_go.py`
6. **更新 README** 如果数字恢复

## 5. 验收（Acceptance）

- [ ] `cargo test --workspace` 全绿
- [ ] `cargo clippy --workspace -- -D warnings` 零警告
- [ ] `cargo fmt --all` 无变化
- [ ] `bench/run_bench.sh` Rust-Tokio→Tokio 吞吐 ≥ 75 MB/s（接近 README 77.58）
- [ ] Rust-Tokio / Go 比率 ≥ 1.35×（恢复到 committed 水平 1.41× 附近）
- [ ] `bench_results.json` 恢复完整 78 条（如执行 P2）
