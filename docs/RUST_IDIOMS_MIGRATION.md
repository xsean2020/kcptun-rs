# kcptun-rs Rust 化优化方案（最终整合版）

> **目标**：在**保证 Go kcptun / kcp-go v5 线兼容性（wire compatibility）**和功能完全一致的前提下，识别并消除从 Go 直译带来的 Go-ism，使代码更符合 Rust 惯例（idiomatic Rust）。
>
> 本文档整合了原始分析与详细检查清单，形成可执行的优化路线图。

---

## 1. 总览：Go-ism 检查清单

### 1.1 类型系统

| # | Go-ism | 位置 | Rust 推荐方式 |
|---|--------|------|--------------|
| T1 | `u32`/`i32` 做 bool 标志（`nodelay`/`nocwnd`/`stream`） | `kcp-rs/src/kcp.rs` | 改用 `bool`，wire 边界做 `as u8` |
| T2 | `state: u32` 用 `0xFFFFFFFF` 表示 dead | `kcp-rs/src/kcp.rs` | 改用 `enum ConnState { Active, Dead }` |
| T3 | `peeksize()` 返回 `i32`（-1 = 无数据） | `kcp-rs/src/kcp.rs` | 返回 `Option<usize>` |
| T4 | `Command` 枚举用 `as u8` 比较，未穷尽匹配 | `kcp-rs/src/kcp.rs` input() | `match Command::from_u8(cmd)` |
| T5 | `Segment` 全部 `pub` 字段 | `kcp-rs/src/segment.rs` | 字段私有化，仅暴露必要方法 |
| T6 | `try_into().unwrap()` 遍布（已知长度） | 全项目 | 先检查长度或使用 `?` + 自定义错误 |

### 1.2 控制流

| # | Go-ism | 位置 | Rust 推荐方式 |
|---|--------|------|--------------|
| C1 | 17 条 `#![allow(clippy::...)]` | `kcp-rs/src/lib.rs` | 逐条消除；无法消除的改行内 `#[allow]` + 注释 |
| C2 | `_imin_`/`_imax_`/`_ibound_` 下划线函数 | `kcp-rs/src/kcp.rs` | `a.min(b)` / `a.max(b)` / `mid.clamp(lo, hi)` |
| C3 | `loop { match pop_front() { Some => .., None => break } }` | `kcp-rs/src/kcp.rs` recv() | `while let Some(seg) = ...` |
| C4 | `match cmd { c if c == Command::Push as u8 => ... }` | `kcp-rs/src/kcp.rs` input() | 穷尽 `Command` enum 匹配 |

### 1.3 数据结构

| # | Go-ism | 位置 | Rust 推荐方式 |
|---|--------|------|--------------|
| D1 | `rcv_buf: Vec<Segment>` + `remove(0)` | `kcp-rs/src/kcp.rs` move_receive_buffer() | `VecDeque<Segment>` + `pop_front()` |
| D2 | `HashMap<u32, ()>` | `kcp-rs/src/fec.rs` ShardHeap | `HashSet<u32>` |
| D3 | `Vec<(u32, bool)>` + `remove(0)` | `kcp-rs/src/fec.rs` AutoTune | `VecDeque` + `pop_front()` |
| D4 | `RwLock<KCP>` 但全部取 `write()` | `kcp-rs/src/session.rs` | `Mutex<KCP>`（更轻量，无需读写分离） |
| D5 | `Arc<Mutex<HashMap<u32, Arc<Stream>>>>` 全局锁 | `smux-rs/src/session.rs` | `DashMap<u32, Arc<Stream>>` |

### 1.4 错误处理

| # | Go-ism | 位置 | Rust 推荐方式 |
|---|--------|------|--------------|
| E1 | `AeadCrypt::open()` 返回 `Result<Vec<u8>, String>` | `kcrypt-rs/src/crypt.rs` | 自定义 `AeadError` enum（实现 `std::error::Error`） |
| E2 | `InboundCryptError` 未实现 `Display`/`Error` | `kcp-rs/src/crypto_buf.rs` | 补全 trait impl |
| E3 | `KcpError::InvalidSegment` / `BufferTooSmall` 从未使用 | `kcp-rs/src/kcp.rs` | 删除死代码 |
| E4 | 测试代码大量裸 `.unwrap()` | 全项目测试 | 改 `expect("hardcoded ... must ...")` 提高可读性 |

### 1.5 并发

| # | Go-ism | 位置 | Rust 推荐方式 |
|---|--------|------|--------------|
| N1 | `std::sync::Mutex` 在 smux-rs | `smux-rs/src/{session,stream}.rs` | 统一使用 `parking_lot::Mutex`（与 kcp-rs 一致） |
| N2 | `ctrl_c()` 用 `unsafe libc::signal` + 轮询 | `kio-rs/src/lib.rs` | `tokio::signal::ctrl_c()`（tokio 特性）；smol 保留或用 signal-hook |
| N3 | `UDPSession` 暴露 `RwLockWriteGuard<'_, KCP>` | `kcp-rs/src/session.rs` | 提供受控方法：`with_kcp<R>(f: impl FnOnce(&KCP) -> R)` / `set_*` |

### 1.6 代码重复（最高优先级项）

| # | Go-ism | 位置 | Rust 推荐方式 |
|---|--------|------|--------------|
| R1 | CFB 特化代码在 TEA/XTEA/Blowfish/3DES 中复制 ~400 行 | `kcrypt-rs/src/crypt/` | 泛型 `BlockCipher8` trait + 统一 `cfb8_encrypt` 函数 |
| R2 | `select_block_crypt` 与 `CryptEngine::select` match 重复 | `kcrypt-rs/src/crypt.rs` | 合并为一个实现 |
| R3 | client/server 的 `Config` + `Cli` + `merge()` + `SnappyStreamDecoder` 大量重复 | `kcptun-client/src/main.rs` / `kcptun-server/src/main.rs` | 抽取 `kcptun-common` crate |
| R4 | `mono_ms()` 在 3 处重复定义 | smux-rs / client / server | 抽取到 `kio-rs::time::mono_ms()` |
| R5 | `SnappyStreamDecoder` / Go 兼容解码逻辑重复 | client & server | 统一实现放公共 crate |

---

## 2. 分层重构方案

### 2.1 kcp-rs — KCP 状态机（核心，谨慎改动）

#### 类型系统 Rust 化（T1, T2, T3）
```rust
// 推荐
pub struct KCP {
    nodelay: bool,
    nocwnd: bool,
    stream: bool,
    state: ConnState,
    fastresend: u32,
    // ...
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnState { Active, Dead }

pub fn peeksize(&self) -> Option<usize> { ... }
```

- 外部 `set_nodelay(nodelay: u32, ...)` 保留 Go 兼容 API，内部 `self.nodelay = nodelay != 0`。

#### 消除 clippy lint（C1）
- 目标：把 crate 级 17 条 allow 降到最少。
- 无法消除的（必须保留 Go 控制流对应关系）改成**行内** `#[allow(clippy::xxx)]` + `// SAFETY/Go-compat: ...` 注释。

#### 数据结构（D1）
```rust
// rcv_buf
rcv_buf: VecDeque<Segment>,

// move_receive_buffer
while let Some(seg) = self.rcv_buf.front() {
    if seg.sn == self.rcv_nxt && ... {
        let seg = self.rcv_buf.pop_front().unwrap();
        ...
    } else { break; }
}
```

#### 控制流（C2, C3, C4）
- 删除 `_imin_` / `_imax_` / `_ibound_`，直接用 `min/max/clamp`。
- `recv()` 改 `while let Some(seg) = self.rcv_queue.pop_front() { ... }`。
- `input()` 改用 `match Command::from_u8(cmd) { Some(Command::Push) => ..., None => return Err(...) }`。

#### 并发（D4, N3）
```rust
// session.rs
pub kcp: parking_lot::Mutex<KCP>,   // 原来是 RwLock

// UDPSession 不再暴露 guard
pub fn with_kcp<R>(&self, f: impl FnOnce(&KCP) -> R) -> R {
    f(&*self.inner.kcp.lock())
}
pub fn set_nodelay(&self, ...) { self.inner.kcp.lock().set_nodelay(...); }
```

#### 死代码清理（E3）
- 删除 `KcpError::InvalidSegment`、`BufferTooSmall`。
- 删除未使用的 `pad8` / `pad16`。

---

### 2.2 kcrypt-rs — 加密层（最高 ROI）

#### 消除 CFB 重复（R1）
定义：
```rust
pub trait BlockCipher8 {
    fn encrypt_block(&self, out: &mut [u8; 8], inp: &[u8; 8]);
}

pub fn cfb8_encrypt<C: BlockCipher8>(data: &mut [u8], c: &C) { ... }
pub fn cfb8_decrypt<C: BlockCipher8>(data: &mut [u8], c: &C) { ... }

impl BlockCipher8 for TeaCrypt { ... }
impl BlockCrypt for TeaCrypt {
    fn encrypt(&self, d: &mut [u8]) { cfb8_encrypt(d, self); }
    fn decrypt(&self, d: &mut [u8]) { cfb8_decrypt(d, self); }
}
```

同理为 16 字节块定义 `BlockCipher16`（AES/SM4/Twofish）。

**目标**：删除约 400 行重复代码。

#### AEAD 错误类型（E1）
```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AeadError {
    #[error("data too short")]
    Short,
    #[error("authentication failed")]
    AuthFailed,
}
```

#### 合并 select（R2）
```rust
pub fn select_block_crypt(method: &str, pass: &[u8]) -> (Box<dyn BlockCrypt>, String) {
    let (eng, name) = CryptEngine::select(method, pass);
    (Box::new(eng), name)
}
```

#### 其他
- `InboundCryptError` 补全 `Display + Error`（E2）。
- `GO_CFB_IV` 切片赋值简化。

---

### 2.3 smux-rs — 多路复用

- `streams: DashMap<u32, Arc<Stream>>`（N1）。
- 统一 `parking_lot::Mutex`（N1）。
- `DEFAULT_CONFIG` 改 `pub const`。
- 可选：引入 `thiserror` 简化 `SessionError`。
- 考虑把 19 个平铺字段按模块分组（渐进式）。

---

### 2.4 kio-rs — 异步抽象

- `ctrl_c()` 移除 `unsafe libc::signal`（N2）。
- 把 `mono_ms()` 抽取到 `kio_rs::time::mono_ms()`（R4）。

---

### 2.5 客户端 / 服务端二进制（大量重复）

#### 抽取公共 crate（R3）
新建 `kcptun-common`：
- `config.rs`：`Config` + `Cli` + `merge()`
- `snappy.rs`：统一的 `SnappyStreamDecoder`（Go 兼容 framing）
- `crypto.rs`：PBKDF2 派生等
- `time.rs`：`mono_ms()`

#### Config 简化（可选，P3）
从大量 `Option<T>` + 手动 merge 逐步迁移到 `#[serde(default)]` + 非 Option 字段，CLI 层做覆盖。

#### Snappy 解码统一
把 client 的 `GoSnappyStream` / server 的 `SnappyStreamDecoder` 合并为单一实现。

---

## 3. 优先级执行路线图

### P0 — 高收益、低风险（立即开始）

1. **R1**：CFB 泛型化（删 400 行重复）
2. **D1**：`rcv_buf` 改 `VecDeque`
3. **D2/D3**：FEC 的 `HashMap<_,()>` → `HashSet` + `Vec` → `VecDeque`
4. **C2**：删除 `_imin_` 等辅助函数
5. **E3**：清理 KCP 死代码
6. **R4**：`mono_ms()` 抽取到 kio-rs

### P1 — 中等收益

7. **T1/T2**：KCP 类型系统 bool + enum
8. **C3/C4**：`while let` + 穷尽匹配
9. **D4**：`RwLock<KCP>` → `Mutex<KCP>`
10. **R2**：合并 select 函数
11. **E1/E2**：AEAD 错误类型 + InboundCryptError 补全
12. **C1**：逐条消除 clippy allow
13. **N3**：封装 `UDPSession` 的锁暴露

### P2 — 较大重构（充分测试后）

14. **N1**：SMUX `streams` 改 `DashMap`
15. **N2**：`ctrl_c()` 移除 unsafe
16. **R3**：抽取 `kcptun-common` crate
17. **T3**：`peeksize()` → `Option<usize>`
18. **T5**：`Segment` 字段私有化

---

## 4. 绝对不改动的内容（Go 线兼容硬约束）

- KCP segment：24B LE `conv|cmd|frg|wnd|ts|sn|una|len`
- FEC header：6B seq + 2B type + 2B size
- SMUX frame：8B `ver|cmd|len(2LE)|sid(4LE)`
- CFB wire：`[nonce 16][CRC32 4][payload]`
- AES-GCM wire：`[nonce 12][ct+tag 16]`
- PBKDF2：salt `b"kcp-go"`，4096 轮，32B key
- 固定 IV `GO_CFB_IV`
- TEA 8 rounds、SM4 S-box、3DES IP/FP 规则
- Command 字节值（81/82/83/84）
- `null` cipher 无 crypto header（区别于 `none`）
- SNMP CSV 字段顺序

---

## 5. 验收标准（每批次必过）

1. `cargo build --workspace` 零 error、零 warning（除行内保留的 allow）
2. `cargo clippy --workspace -- -D warnings`
3. `cargo test --workspace`
4. 压力测试：`cargo test --release -p kcptun-server --test stress_test -- --nocapture --test-threads=1`
5. **Go 互操作**：`bash test_e2e.sh` 全矩阵通过（tokio+smol × 所有 crypt × 所有 mode）
6. Wire 格式抓包验证（KCP/FEC/SMUX 字节序、长度、字段值）

---

## 6. 渐进式执行建议

1. 先做 **P0** 中不改算法逻辑的项（R1、D1、D2/D3、C2、E3、R4）。
2. 每完成一个 P0 项，跑全量 E2E + clippy。
3. P1 中的类型系统改动（T1/T2）放在后面，因为会影响大量 `set_nodelay` 调用处。
4. 抽取 `kcptun-common` 放在 P2，影响最大。
5. 任何涉及“暴露锁 guard”的改动，必须先把调用方全部迁移到受控方法。

---

## 7. 附录：额外 Rust 惯例增强（来自前期分析）

- **封装**：不要让上层获得 `RwLockWriteGuard<KCP>` 或 `Arc<Mutex<T>>` 直接操作内部。
- **可见性**：`SessionInner` 所有字段改为 private，仅通过方法暴露。
- **配置**：考虑把 `smux_rs::Config` 包在 `Arc` 里，减少 clone 开销。
- **测试可读性**：所有测试中的 `parse().unwrap()` 至少改成带语义的 `expect`。

---

**文档状态**：本文件是当前项目的**最终整合优化方案**。后续实现应严格按照 P0 → P1 → P2 顺序，**每步验证 E2E 通过**后再推进。

维护者：优先处理 R1（CFB 重复）和 D1（O(n) remove）—— 这两个是当前性价比最高的改动。
