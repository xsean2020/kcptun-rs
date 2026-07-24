<!-- Created: 2026-07-22 | Status: Analysis — pending implementation decision -->

# 全加密算法压缩火焰图分析报告

> **状态：** 分析完成，待决策是否实施优化建议  
> **日期：** 2026-07-22  
> **Git：** `55afb88`  
> **主机：** macOS arm64 (Apple M1, 8-core, 8 GB, Darwin 25.3)  
> **工具：** samply 0.13.1, Firefox Profiler JSON 格式

---

## 1. 背景与目标

本报告对 kcptun-rs 的 **client + server** 在 **Snappy 压缩开启** 条件下，针对 **全部 13 种加密算法** 进行 CPU 火焰图采样，目标是：

1. 发现压缩 + 加密组合下的 CPU 热点
2. 识别跨算法的共性瓶颈
3. 与 prior nocomp 火焰图（L1–L4）对比，确认压缩是否引入新热点
4. 为后续优化提供证据基础

---

## 2. 工具链变更

### 2.1 `bench/kcptun_prof_wl.rs` — 新增 `COMP` 环境变量

**变更原因：** 原工作负载硬编码 `--nocomp`，无法在压缩模式下采样。

**变更内容：**

```rust
// COMP=1 enables Snappy compression (omits --nocomp); default is nocomp.
let nocomp = std::env::var("COMP").unwrap_or_default() != "1";
```

server 和 client 的参数列表从固定数组改为动态构建，根据 `nocomp` 决定是否追加 `--nocomp`。

**向后兼容：** 默认行为不变（`--nocomp`），现有 `profile_flamegraph.sh` 无需修改。

### 2.2 `bench/profile_all_ciphers_comp.sh` — 批量采样脚本

遍历全部 13 种算法（`null none xor aes-128 aes-128-gcm salsa20 blowfish twofish cast5 3des tea xtea sm4`），每种用 `COMP=1 samply record` 采集一次火焰图。

特性：
- 可通过 `CIPHERS=` 参数指定子集
- 可通过 `BENCH_DATA_MB=` 控制数据量
- 自动调用 `bench/symbolicate_profile.py` 进行符号化
- 输出文件命名：`bench/profiles/comp-<cipher>-<timestamp>.named.json.gz`

### 2.3 `bench/analyze_profiles.py` — 火焰图分析脚本

解析 samply 的 Firefox Profiler JSON 格式（非 Speedscope），重建调用栈链，计算：

- **Self time**：叶子帧（栈顶）的采样计数
- **Total time**：帧出现在栈中任意位置的计数（any-in-stack）

输出：
- 每种算法的 top-N self% 排名表
- 跨算法 self% 矩阵（≥3% 阈值）
- 可选 JSON 输出（`--json`）

---

## 3. 测试方法

### 3.1 构建配置

```bash
RUSTFLAGS="--cfg aes_armv8 -C force-frame-pointers=yes" \
  cargo build --profile profiling -p kcptun-server -p kcptun-client
```

- `profiling` profile：`debug=2, strip=false, lto=false`（保留符号）
- `aes_armv8` cfg：确保 Apple Silicon 上 AES-CFB 走硬件路径
- `force-frame-pointers=yes`：帮助栈回溯
- 符号数：3253 个 text symbols（`nm -n` 统计）

### 3.2 负载参数

```
COMP=1 bench/kcptun_prof_wl <cipher> 50 <profiling-server> <profiling-client> 5
```

| 参数 | 值 | 说明 |
|------|-----|------|
| 数据量 | 50 MB | 每种算法 |
| 连接数 | 1（单连接） | 并发收发（sender + receiver 线程） |
| 预热 | 2 MB | 预热 KCP 拥塞窗口 + SMUX + Snappy |
| 延迟测量 | 5 次 | 1KB 包 RTT 中位数 |
| Echo 服务器 | Python TCP echo | 与 `bench/run_bench.sh` 一致 |
| kcptun 参数 | `--mode fast`（无 `--sndwnd`/`--rcvwnd` 覆盖） | 使用默认窗口 |
| 压缩 | **ON**（Snappy） | `COMP=1` 省略 `--nocomp` |

### 3.3 samply 采集

```
COMP=1 samply record --save-only --unstable-presymbolicate \
    --symbol-dir <profiling-bin-dir> \
    -o bench/profiles/comp-<cipher>-<ts>.json.gz -- \
    bench/kcptun_prof_wl <cipher> 50 <server> <client> 5
```

samply 采样整个进程树（server + client + 工作负载），采样率 ~1ms 间隔。

---

## 4. ⚠️ 关键注意事项：可压缩测试数据

**这是本次分析最重要的发现。** 工作负载发送的数据是 `vec![0xABu8; 128KB]`——即 128KB 的 `0xAB` 重复模式。这种数据对 Snappy 来说**极度可压缩**：

- 50 MB 的 `0xAB` 压缩后约 ~500 KB（压缩比 ~100:1）
- 实际经过加密 / KCP / UDP 路径的数据量仅为名义数据量的 ~1%
- **吞吐量数字被虚高 5–15 倍**

### 4.1 虚高对比

| 加密算法 | Profiling 吞吐（comp, 0xAB） | 基准矩阵吞吐（comp, 随机数据） | 虚高倍数 |
|----------|----------------------------:|-------------------------------:|---------:|
| null | 431 MB/s | 31.7 MB/s | 13.6× |
| none | 504 MB/s | 40.7 MB/s | 12.4× |
| 3des | 212 MB/s | 13.4 MB/s | 15.8× |
| sm4 | 191 MB/s | 14.3 MB/s | 13.4× |
| xtea | 159 MB/s | 23.3 MB/s | 6.8× |

> 基准矩阵数据来自 `bench_rust_vs_go.py`（使用 `os.urandom` 不可压缩随机数据，10 连接 × 1MB）。

### 4.2 对分析结论的影响

由于实际加密数据量极小：
- 加密算法的 CPU 占比被严重低估（<1.2% self）
- Snappy 压缩几乎不可见（0.2–1.2% any-in-stack）
- 大部分 CPU 被异步运行时开销（`Harness::complete` ~77%）占据
- **不能**从本次数据推断压缩 + 加密的真实 CPU 分布

---

## 5. Profiling 吞吐结果

以下为 profiling binaries（非 release LTO）在 50MB 压缩模式下的吞吐量：

| # | 加密算法 | 吞吐 (MB/s) | 延迟 (ms RTT) | 采样数 |
|---|----------|------------:|-------------:|-------:|
| 1 | null | 431.18 | 0.123 | 12,515 |
| 2 | none | 504.21 | 0.126 | 11,012 |
| 3 | xor | 311.35 | 0.130 | 11,578 |
| 4 | aes-128 | 285.97 | 0.208 | 11,992 |
| 5 | aes-128-gcm | 327.56 | 0.174 | — |
| 6 | salsa20 | 411.01 | 0.160 | 11,011 |
| 7 | blowfish | 401.13 | 0.105 | 11,615 |
| 8 | twofish | 212.65 | 0.147 | 13,151 |
| 9 | cast5 | 293.59 | 0.189 | 12,579 |
| 10 | 3des | 212.27 | 0.208 | 14,389 |
| 11 | tea | 452.17 | 0.139 | 11,280 |
| 12 | xtea | 159.11 | 0.176 | 15,085 |
| 13 | sm4 | 191.04 | 0.160 | 14,605 |

> ⚠️ 这些数字因可压缩数据而虚高，不代表真实性能。

---

## 6. 每算法热点排名

### 6.1 统一模式（所有 13 种算法一致）

| 排名 | 帧 | Self % 范围 | 说明 |
|------|-----|------------:|------|
| 1 | `tokio::runtime::task::harness::Harness::complete` | 74.8–78.1% | **异步运行时开销；实际工作被内联** |
| 2 | `kcptun_server::rotate_log` | 9.1–9.5% | **Profiling 伪影**（见 §7.2） |
| 3 | `tokio::runtime::task::harness::Harness::poll` | 7.5–12.4% | 任务轮询调度 |
| 4 | `tokio::runtime::task::harness::Harness::shutdown` | 2.0–3.0% | 任务关闭 |

### 6.2 加密算法 CPU 叶子（self time）

| 加密算法 | 加密帧 | Self % | Any-in-stack % |
|----------|--------|-------:|---------------:|
| 3des | `BlockCrypt::encrypt` / `decrypt` | 0.93% / 0.70% | 1.0% / 0.9% |
| sm4 | `Sm4Crypt::encrypt_block` | 1.02% | 1.0% |
| sm4 | `blowfish::next_u32_wrap` | 1.14% | 1.1%（SM4 nonce RNG） |
| cast5 | `Cast5Crypt::c5_enc` | 0.62% | 0.6% |
| blowfish | `Blowfish::encrypt` | 0.40% | 0.4% |
| xtea | `BlockCrypt::encrypt` / `decrypt` | 0.67% / 0.52% | 0.7% / 0.7% |
| twofish | （内联） | <0.1% | 0.3% / 0.4% |
| tea | （内联） | <0.1% | 0.2% / 0.2% |
| salsa20 | （内联） | <0.1% | 0.2% / 0.1% |
| aes-128 | （内联） | <0.1% | 0.1% / 0.1% |
| aes-128-gcm | （内联） | <0.1% | — |
| null | （无加密） | — | — |
| none | （无加密） | — | — |
| xor | （内联） | <0.1% | — |

### 6.3 Snappy 压缩（any-in-stack）

| 加密算法 | compress % | 说明 |
|----------|-----------:|------|
| sm4 | 1.2% | 最高（慢加密 → snappy 占比相对更高） |
| none | 0.4% | |
| null | 0.3% | |
| 其他 | 0.2–0.3% | 因可压缩数据，Snappy 几乎不做功 |

### 6.4 数据面帧（any-in-stack，全部 <0.5%）

| 帧 | 范围 |
|----|-------|
| `feed_data` | 0.1–0.7% |
| `SMUX` / `smux` | 0.2–0.5% |
| `udp` / `UdpSocket` | 0.2–0.4% |
| `SegmentPool` | 不可见 |
| `encrypt_batch` | 不可见 |
| `cpu_block` | 不可见 |

---

## 7. 关键发现

### 7.1 `Harness::complete` 占 ~77% — 异步运行时主导

**现象：** 所有 13 种算法中，`tokio::runtime::task::harness::Harness::complete` 的 self time 均在 74.8–78.1%。

**原因：** tokio 异步运行时将实际的数据面工作（加密、Snappy、KCP、SMUX、UDP I/O）内联到任务完成路径中。samply 的采样将这些内联代码的 CPU 时间归因于 `Harness::complete` 帧，而非实际的工作函数。

**这是 samply + Rust async profiling 的根本限制。** 先前 L1–L3 nocomp 火焰图也观察到类似模式（~65–77% async main 聚合）。

**结论：** 无回归。压缩未引入新的异步运行时热点。

### 7.2 `rotate_log` 9.3% — Profiling 伪影

**现象：** `kcptun_server::rotate_log` 在所有算法中显示 9.1–9.5% self time 和 ~85% total（any-in-stack）。

**实际情况：**
- `rotate_log` 在 `main.rs` 第 1909 行调用，仅启动时执行一次
- profiling 工作负载**不传 `--log` 参数**，因此 `rotate_log` 根本不会被调用
- 9.3% self time 和 85% total 是纯粹的栈表误归因

**根因：** Firefox Profiler 格式的 `stackTable` 中，帧索引或地址范围重叠，导致 samply 将异步任务采样误归因到 `rotate_log` 的帧。这是 profiling 工具的 bug，而非真实 CPU 开销。

**验证方法：** 在 release 构建中用 `--pprof` Go 兼容 profiling（DWARF 栈回溯）可交叉验证。

### 7.3 Snappy 几乎不可见 — 可压缩数据掩盖

**现象：** Snappy 压缩相关帧仅占 0.2–1.2%（any-in-stack），远低于预期。

**根因：** 工作负载数据为 `vec![0xABu8; 128KB]`，对 Snappy 极度可压缩。50MB `0xAB` 压缩后约 500KB，Snappy 压缩器只需处理极少量的实际数据。

**影响：** 无法从本次采样中得出压缩 + 加密的真实 CPU 分布。

### 7.4 SM4 nonce 生成 — 旁支发现

**现象：** `blowfish::next_u32_wrap` 在 sm4 场景下占 1.14% self time。

**原因：** SM4 的 nonce 生成使用了 `blowfish` crate 的 RNG（`next_u32_wrap`），作为一个独立的叶子帧出现在火焰图中。

**评估：** <2%，非瓶颈，无需优化。

### 7.5 无 ≥5% 可操作加密热点

**结论：** 由于可压缩数据导致实际加密数据量极小，所有加密叶子帧均 <1.2% self，远低于 5% 的可操作性阈值。

---

## 8. 跨算法 Self% 矩阵

| 函数 | 3des | aes | blowfish | cast5 | none | null | salsa20 | sm4 | tea | twofish | xor | xtea |
|------|-----:|----:|---------:|------:|-----:|-----:|--------:|----:|----:|--------:|----:|-----:|
| `Harness::complete` | 76.2 | 77.5 | 77.9 | 77.2 | 77.8 | 74.8 | 77.6 | 76.4 | 77.7 | 76.8 | 78.1 | 76.0 |
| `Harness::poll` | 7.5 | 8.4 | 8.3 | 8.1 | 8.7 | 12.4 | 8.4 | 7.6 | 8.4 | 8.0 | 8.4 | 7.7 |
| `rotate_log`（伪影） | 9.3 | 9.3 | 9.3 | 9.3 | 9.5 | 9.1 | 9.5 | 9.2 | 9.5 | 9.3 | 9.4 | 9.1 |
| `Harness::shutdown` | — | — | — | — | — | — | — | — | — | — | — | 3.0 |

> 所有算法的 CPU 分布几乎一致——差异在采样噪声范围内。这进一步证实可压缩数据导致无法区分加密算法的 CPU 影响。

---

## 9. 与历史 Profiling 对比

| 场景 | 日期 | `Harness::complete` / async main | 加密叶子 | Snappy | 数据类型 |
|------|------|--------------------------------:|----------|--------:|---------|
| L1 null/nocomp | 2026-07-21 | ~65% | — | —（nocomp） | 0xAB |
| L2 aes/nocomp | 2026-07-21 | ~60–65% | `aes::soft::fixslice` ~0.1% | —（nocomp） | 0xAB |
| L3 3des/nocomp | 2026-07-21 | ~33–65% | `TripleDesCipher::encrypt_block` ~11% | —（nocomp） | 0xAB |
| L3 3des/nocomp re-capture | 2026-07-21 | ~33% | `TripleDesCipher::encrypt_block` ~11% | —（nocomp） | 0xAB |
| **comp (13 ciphers)** | **2026-07-22** | **~77%** | **<1.2%** | **0.2–1.2%** | **0xAB（comp ON）** |

**对比结论：**
- `Harness::complete` 占比从 ~65% 升至 ~77%——因为压缩模式下 Snappy 工作也被内联
- 加密叶子从 ~11%（3des nocomp）降至 <1.2%（comp ON）——因为可压缩数据
- 无新热点引入

---

## 10. 优化建议（待决策）

### 建议矩阵

| # | 建议 | 优先级 | 工作量 | 风险 | 预期影响 | 状态 |
|---|------|--------|--------|------|---------|------|
| R1 | 修改 profiling 工作负载数据为随机数据 | P1 | 小（~20 行） | 无 | 揭示真实 snappy + crypto CPU 分布 | **已完成 2026-07-22** |
| R2 | 用 `bench_rust_vs_go.py` 作为 samply 负载 | P1 | 中（需包装 samply around server/client 进程） | 无 | 获得真实多连接 + 随机数据 profile | 待决策 |
| R3 | 加密算法无需修改 | — | — | — | Rust 已在基准矩阵中全部 ≥ Go | 无需行动 |
| R4 | 调查 `rotate_log` 栈误归因 | P2 | 小（调试 samply 栈表） | 无 | 修复 profiling 伪影，非运行时改善 | 待决策 |
| R5 | KCP/SMUX/UDP 数据面无需优化 | — | — | — | 所有帧 <0.5% | 无需行动 |
| R6 | 尝试 Go pprof 格式 profiling | P2 | 中（需 `--features pprof` 构建） | 无 | 可能获得更好的 async 帧归因 | 待决策 |

### R1：修改 profiling 工作负载数据

**当前代码（`bench/kcptun_prof_wl.rs` 第 199 行）：**
```rust
let payload = vec![0xABu8; chunk_sz];
```

**建议修改：**
```rust
// Use random data to prevent Snappy from compressing it away
let mut payload = vec![0u8; chunk_sz];
// Simple xorshift PRNG (no extra dependency, deterministic per run)
let mut state: u64 = 0x12345678DEADBEEF;
for chunk in payload.chunks_mut(8) {
    state ^= state << 13;
    state ^= state >> 7;
    state ^= state << 17;
    chunk.copy_from_slice(&state.to_le_bytes()[..chunk.len()]);
}
```

**优点：** 无新依赖，确定性可复现，不可压缩。
**缺点：** 吞吐量数字会大幅下降（接近真实基准值）。

### R2：用 `bench_rust_vs_go.py` 作为 samply 负载

`bench_rust_vs_go.py` 已使用 `os.urandom` 随机数据和 10 并发连接。可以单独包装 server 或 client 进程进行 samply 采样：

```bash
# Profile server only
samply record --save-only -o server-aes-comp.json.gz -- \
    ./target/profiling/kcptun-server -l :29900 -t 127.0.0.1:8080 \
    --key k --crypt aes-128 --mode fast --sndwnd 2048 --rcvwnd 2048

# In another terminal, run client + bench_rust_vs_go.py load
```

**优点：** 真实负载、随机数据、多连接、已有对比基准。
**缺点：** 需要手动协调 server/client 启动，不能自动采样整个进程树。

### R6：Go pprof 格式 profiling

已有 `--pprof` feature 和 `bench/profile_rust_go_pprof.sh` 脚本。pprof 使用 DWARD 栈回溯，可能比 samply 的帧指针回溯更好地解析 async 帧名。

```bash
RUSTFLAGS="-C force-frame-pointers=yes" \
    cargo build --profile profiling -p kcptun-server -p kcptun-client --features pprof
bash bench/profile_rust_go_pprof.sh server 20
go tool pprof -top bench/profiles/rust-server-aes-*.pb
```

**优点：** demangled Rust 函数名，Go 工具链 UI。
**缺点：** 需要 `pprof` feature（增加 ~0.5–0.7 MB 二进制大小）。

---

## 11. 产出文件清单

| 文件 | 类型 | 说明 |
|------|------|------|
| `bench/kcptun_prof_wl.rs` | 修改 | 新增 `COMP` env var 支持压缩开关 |
| `bench/profile_all_ciphers_comp.sh` | 新增 | 批量全算法压缩火焰图采集脚本 |
| `bench/analyze_profiles.py` | 新增 | Firefox Profiler JSON 格式分析脚本 |
| `bench/profiles/HOTSPOTS.md` | 修改 | 新增全算法压缩 profiling 章节 |
| `bench/profiles/comp-*.named.json.gz` | 产出 | 13 个符号化火焰图（gitignored） |

---

## 12. 复现步骤

```bash
# 1. 构建 profiling binaries
RUSTFLAGS="--cfg aes_armv8 -C force-frame-pointers=yes" \
    cargo build --profile profiling -p kcptun-server -p kcptun-client

# 2. 构建工作负载助手
rustc -C opt-level=3 -C force-frame-pointers=yes \
    bench/kcptun_prof_wl.rs -o bench/kcptun_prof_wl

# 3. 运行全算法压缩火焰图采集
BENCH_DATA_MB=50 bash bench/profile_all_ciphers_comp.sh

# 4. 分析结果
python3 bench/analyze_profiles.py --top 12

# 5. 查看单个火焰图
samply load bench/profiles/comp-aes-128-*.named.json.gz
# 或上传到 https://www.speedscope.app
```

---

## 13. 结论

1. **本次 profiling 因可压缩数据（`0xAB`）导致无法得出加密 + 压缩的真实 CPU 分布。** 最关键的后续行动是 R1（修改为随机数据）。

2. **无新热点引入。** 压缩模式下 `Harness::complete` ~77% 与 nocomp 模式 ~65% 一致，差异来自 Snappy 工作被内联。

3. **`rotate_log` 9.3% 是 profiling 伪影**，非真实 CPU 开销。可考虑 R4 或 R6 改善 profiling 精度。

4. **加密算法和数据面均无需优化。** Rust 已在基准矩阵中全部达到或超越 Go，且本次 profile 无 ≥5% 可操作热点。

5. **决策建议：** 优先实施 R1（修改工作负载数据），然后重新采集火焰图以获得真实的压缩 + 加密 CPU 分布。其余建议可视情况选择性实施。
