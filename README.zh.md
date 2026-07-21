# kcptun-rs

[English](README.md) | 中文

## 免责声明

> ⚠️ **本项目是 Vibe Coding 的移植测试，仅供学习交流使用。**
>
> 本项目是一次 **Vibe Coding**（利用 AI 辅助编程的实践方式）的移植测试——通过尝试移植现有代码库来实践 AI 辅助编程。核心是探索和验证 Vibe Coding 这套工作流本身，而非专门做一个生产级软件移植。本项目**不是**生产级软件，不保证功能正确性、稳定性和安全性。
>
> **严禁用于任何违法违规用途**，包括但不限于翻墙、非法数据传输、网络攻击等。使用者的任何违法行为均与本项目及作者无关，由行为人自行承担全部法律责任。
>
> 完整免责声明请参阅 [DISCLAIMER.zh.md](DISCLAIMER.zh.md)。

## 关于本项目

**kcptun-rs** 是一次 **Vibe Coding** 移植测试——以 [kcptun](https://github.com/xtaci/kcptun)（[xtaci](https://github.com/xtaci) 开发的基于 KCP 的 TCP 流加速器）为参照对象的 AI 辅助编程实验。

> kcptun 是一个稳定且安全的隧道，通过 KCP 传输 TCP，支持 SMUX 多路复用、
> 前向纠错（FEC）和可选的 kcp-go 加密。

本项目 的目的**不是**做一个生产级的 kcptun 替代品。它是一个 **Vibe Coding** 工作流的真实测试案例——AI 辅助编程能否产出一个可工作的、线路兼容的复杂网络系统移植？代码库、bug 修复、基准测试和测试结果都是该实验的产物。

## 功能特性

- ✅ **Go kcptun 兼容**——与原始 Go 实现互通
- ✅ **Snappy 压缩**（会话级，与 Go kcptun 架构一致）
- ✅ **多种加密后端**：AES-128、AES-192、AES-256、AES-128-GCM、SM4、XOR、TEA、XTEA、Salsa20、Blowfish、Twofish、CAST5、3DES 或 none
- ✅ **KCP 协议模式**：`normal`、`fast`、`fast2`、`fast3`
- ✅ **SMUX 多路复用**（v1/v2）——单个 KCP 连接上承载多个 TCP 流
- ✅ **FEC**（Reed-Solomon 前向纠错）
- ✅ **QPP**（量子置换垫）——可选的后量子混淆层（按流）
- ✅ **多端口**客户端拨号器和服务端监听器
- ✅ **自动过期**会话清理
- ✅ **SNMP** 统计日志
- ✅ **JSON 配置**文件支持

## 快速开始

### 构建

```bash
cargo build --release
```

二进制文件位于：
- `target/release/kcptun-client`
- `target/release/kcptun-server`

### 运行

**服务端**（监听 UDP :29900，转发到本地 HTTP 服务）：

```bash
./target/release/kcptun-server -t "127.0.0.1:8080" -l ":29900" --key "my-secret"
```

**客户端**（本地监听 :12948，隧道到远程服务端）：

```bash
./target/release/kcptun-client -r "server-ip:29900" -l ":12948" --key "my-secret"
```

现在将你的应用指向 `127.0.0.1:12948`。TCP 数据将被加密、压缩并通过 KCP 加速传输到远程服务端。

### 使用配置文件

```bash
kcptun-server -c config.json
kcptun-client -c config.json
```

`config.json` 示例：

```json
{
    "localaddr": ":12948",
    "remoteaddr": "vps:29900",
    "key": "my-secret",
    "crypt": "aes-128",
    "mode": "fast2",
    "conn": 2,
    "mtu": 1350,
    "sndwnd": 1024,
    "rcvwnd": 1024,
    "datashard": 10,
    "parityshard": 3,
    "nocomp": false,
    "smuxver": 2,
    "keepalive": 10
}
```

## CLI 选项

### kcptun-client

| 参数 | 默认值 | 说明 |
|------|--------|------|
| `-l` / `--localaddr` | `:12948` | 本地监听地址 |
| `-r` / `--remoteaddr` | （必填） | KCP 服务端地址，如 `"IP:29900"` 或 `"IP:min-max"` 多端口 |
| `--key` | `it's a secrect` | 预共享密钥 |
| `--crypt` | `aes` | 加密：`null`、`none`、`xor`、`aes`、`aes-128`、`aes-192`、`aes-128-gcm`、`sm4`、`tea`、`xtea`、`salsa20`、`blowfish`、`twofish`、`cast5`、`3des` |
| `--mode` | `fast` | KCP 模式：`normal`、`fast`、`fast2`、`fast3` |
| `--conn` | `1` | UDP 连接数 |
| `--mtu` | `1350` | 最大传输单元 |
| `--sndwnd` | `1024` | 发送窗口（包数） |
| `--rcvwnd` | `1024` | 接收窗口（包数） |
| `--datashard` | `0` | FEC 数据分片 |
| `--parityshard` | `0` | FEC 冗余分片 |
| `--nocomp` | `false` | 禁用 Snappy 压缩 |
| `--smuxver` | `2` | SMUX 协议版本（1 或 2） |
| `--keepalive` | `10` | 保活间隔（秒） |
| `--autoexpire` | `0` | 自动过期连接（秒，0=关闭） |
| `--QPP` | `false` | 启用量子置换垫 |
| `--QPPCount` | `61` | QPP 垫数（应为质数） |
| `-c` | — | JSON 配置文件路径 |

### kcptun-server

| 参数 | 默认值 | 说明 |
|------|--------|------|
| `-l` / `--listen` | `:29900` | KCP 监听地址 |
| `-t` / `--target` | （必填） | TCP 目标地址 |
| `--key` | `it's a secrect` | 预共享密钥 |
| `--crypt` | `aes` | 加密（与客户端相同） |
| `--mode` | `fast` | KCP 模式 |
| `--nocomp` | `false` | 禁用 Snappy 压缩 |

其他参数与客户端一致（mtu、sndwnd、rcvwnd、datashard 等）。

> **重要**：`--key`、`--crypt`、`--mode` 和 `--nocomp` 在客户端和服务端之间**必须一致**。只有设置 `--nocomp` 时才禁用压缩，与 Go 行为一致。

## 架构

### 工作区结构

这是一个包含 6 个 crate 的 Cargo 工作区：

```
kcptun-rs/
├── kcp-rs/          — KCP 可靠 UDP 传输协议（库）
├── kcrypt-rs/       — 共享块密码/AEAD 密码库（库，从 kcp-rs 提取）
│   └── src/crypt/   — 每个密码一个文件：none、xor、aes_cfb、sm4、tea、xtea、
│                       salsa20、blowfish、twofish、cast5_crypt、triple_des、aes_gcm
├── smux-rs/         — SMUX 流多路复用器（库）
├── qpp-rs/          — 量子置换垫加密（库）
├── kcptun-client/   — 客户端二进制
└── kcptun-server/   — 服务端二进制
```

密码算法位于 `kcrypt-rs` 中，由 `kcp-rs` 重新导出以保持向后兼容。新代码应直接依赖 `kcrypt-rs`。

### 协议栈

协议栈，自底向上：

```
┌─────────────────────────────────────────┐
│          TCP / UNIX Socket              │
├─────────────────────────────────────────┤
│           SMUX Stream (mux)             │
├─────────────────────────────────────────┤
│         SMUX Session (mux)              │
├─────────────────────────────────────────┤
│   Snappy Compression (session-level)    │  ← 与 Go kcptun 一致
├─────────────────────────────────────────┤
│   BlockCrypt / FEC / KCP (kcp-go)       │
├─────────────────────────────────────────┤
│            UDP / TCPraw                 │
└─────────────────────────────────────────┘
```

## Go 兼容性

本 Rust 移植设计为与原始 Go kcptun **完全线路兼容**。关键兼容点：

| 特性 | 状态 |
|------|------|
| KCP 段线路格式（kcp-go v5） | ✅ |
| 加密头（nonce 16B + CRC32 4B） | ✅ |
| Snappy 压缩（会话级） | ✅ |
| SMUX 帧格式（v1/v2） | ✅ |
| 密钥派生（PBKDF2-HMAC-SHA1） | ✅ |
| FEC 头格式 | ✅ |
| 多端口寻址 | ✅ |
| QPP 混淆（流级） | ✅ |
| SM4 分组密码（GB/T 32907） | ✅（tjfoc/gmsm S-box + CK 修复） |
| CAST5（CAST-128） | ✅（完整 RFC 2144 实现，Go 兼容） |

## 压缩

Snappy 压缩**默认启用**（`--nocomp = false`），与 Go kcptun 默认行为一致。

- 在 **SMUX 会话级**工作——将所有多路复用流数据作为一个 Snappy 帧流压缩
- 兼容 Go 的 `github.com/golang/snappy`（NewBufferedWriter / NewReader）
- 通过 KCP 发送前批量压缩待发 SMUX Data 帧；接收后解压再分发给 SMUX
- 在客户端和服务端同时设置 `--nocomp` 禁用

## 日志

日志输出由标准 `RUST_LOG` 环境变量控制。默认只显示 `info` 及以上级别。

### 日志级别

| 级别 | 使用场景 | 示例输出 |
|------|----------|----------|
| `error` | 系统故障（始终显示） | `connection refused`、`decrypt failed` |
| `warn` | 运行时警告（始终显示） | `KCP send error`、`UDP send_to error` |
| `info` | 关键操作（**默认**） | `listening on :29900`、`stream 5 opened` |
| `debug` | 调试（`RUST_LOG=debug`） | `SMUX UPD frames`、`flush backpressure` |
| `trace` | 每包追踪（`RUST_LOG=trace`） | `send_frame details`、`feed_data per-packet` |

### 用法

```bash
# 默认——只显示 info/warn/error（干净输出）
./target/release/kcptun-server -l :29900 -t 127.0.0.1:8080
./target/release/kcptun-client -l :12948 -r server:29900

# 调试——显示 debug 级别日志
RUST_LOG=debug ./target/release/kcptun-server -l :29900 -t 127.0.0.1:8080

# 追踪——显示所有（非常冗长，包含每包详情）
RUST_LOG=trace ./target/release/kcptun-client -l :12948 -r server:29900

# 按模块控制——不同 crate 不同级别
RUST_LOG=kcptun_client=debug,kcp_rs=warn ./target/release/kcptun-client

# 服务端文件日志（也遵守 RUST_LOG）
./target/release/kcptun-server -l :29900 -t 127.0.0.1:8080 --log /var/log/kcptun.log
```

## 性能基准测试：Rust vs Go

### 性能剖析（火焰图）

数据面 CPU 采样（macOS arm64 优先 **samply** → Speedscope）：

```bash
cargo install samply --locked
make release
bash bench/profile_flamegraph.sh all    # 或: make profile
```

- 操作手册：[`bench/PROFILE_RUNBOOK.md`](bench/PROFILE_RUNBOOK.md)
- 热点记录：[`bench/profiles/HOTSPOTS.md`](bench/profiles/HOTSPOTS.md)
- Agent skill：[`.claude/skills/flamegraph-perf/SKILL.md`](.claude/skills/flamegraph-perf/SKILL.md)

Apple Silicon 上 release 构建会启用 RustCrypto **`aes_armv8`**（见 `.cargo/config.toml`），使 AES-CFB 走硬件 AES 而非 soft fixslice。

剩余优化项与 KPI 见 [`PERF_OPTIMIZATION_PLAN.md`](PERF_OPTIMIZATION_PLAN.md)。

两个基准测试脚本测量不同维度的性能：

- **`bench/run_bench.sh`** — 批量吞吐（200 MB，单连接，AES-128-CFB，`--nocomp`）。
  测量持续数据传输速率和 RTT 延迟。
- **`bench_rust_vs_go.py`** — 全量加密算法 × 压缩矩阵（10 并发连接 × 每连接 1 MB，
  `--mode fast --sndwnd 2048 --rcvwnd 2048`）。并发收发 + 256KB 预热。结果保存到 `bench_results.json`。

### 三方批量吞吐（50 MB，AES-128，`--nocomp`）

| 路径                      | 吞吐 (MB/s)      | 延迟 (ms RTT) | vs Go   |
|---------------------------|-------------------|----------------|---------|
| Go → Go                   | 48.10             | 0.22           | 1.00x   |
| **Rust-Tokio → Rust-Tokio** | **68.81**     | 0.17           | **1.43x** |
| **Rust-Smol → Rust-Smol**   | **75.01**     | 0.23           | **1.56x** |
| Go → Rust-Tokio           | 46.79             | 0.18           | 0.97x   |
| Rust-Tokio → Go           | 19.92             | 0.22           | 0.41x   |

> Rust-Tokio 和 Rust-Smol 的批量吞吐均超过 Go→Go（分别为 1.43x 和 1.56x），
> 且延迟更低。跨实现路径（Go→Rust、Rust→Go）受较慢一方瓶颈限制。

### 全量加密算法 × 压缩矩阵（10 连接 × 1 MB）

#### 无压缩（`--nocomp`）

| 密码         | Tokio MB/s | Smol MB/s | Go MB/s | T/Go   | S/Go   |
|--------------|-----------|-----------|---------|--------|--------|
| null         | 27.3      | 25.0      | 23.5    | 1.16x  | 1.06x  |
| none         | 20.7      | 20.5      | 20.4    | 1.01x  | 1.00x  |
| xor          | 20.1      | 18.1      | 16.4    | 1.23x  | 1.11x  |
| aes-128      | 19.7      | 19.7      | 20.7    | 0.95x  | 0.95x  |
| aes-128-gcm  | 22.5      | 23.5      | 19.5    | 1.15x  | 1.20x  |
| salsa20      | 22.5      | 17.3      | 21.3    | 1.06x  | 0.81x  |
| blowfish     | 20.9      | 19.9      | 15.6    | 1.33x  | 1.27x  |
| twofish      | 19.7      | 19.9      | 14.9    | 1.32x  | 1.33x  |
| cast5        | 21.1      | 16.1      | 14.9    | 1.42x  | 1.08x  |
| 3des         | 11.6      | 11.7      | 10.1    | 1.14x  | 1.16x  |
| tea          | 22.4      | 21.7      | 14.0    | 1.60x  | 1.55x  |
| xtea         | 20.4      | 17.4      | 14.7    | 1.39x  | 1.19x  |
| **sm4**      | **13.2**  | **16.0**  | **3.9** | **3.34x** | **4.05x** |

#### 启用压缩（Snappy）

| 密码         | Tokio MB/s | Smol MB/s | Go MB/s | T/Go   | S/Go   |
|--------------|-----------|-----------|---------|--------|--------|
| null         | 26.3      | 15.2      | 22.3    | 1.18x  | 0.68x  |
| none         | 20.0      | 22.0      | 19.4    | 1.03x  | 1.14x  |
| xor          | 17.2      | 16.6      | 20.4    | 0.84x  | 0.81x  |
| aes-128      | 21.0      | 16.0      | 20.6    | 1.02x  | 0.78x  |
| aes-128-gcm  | 25.0      | 23.8      | 22.5    | 1.11x  | 1.06x  |
| salsa20      | 24.3      | 21.6      | 18.9    | 1.28x  | 1.14x  |
| blowfish     | 15.4      | 11.8      | 15.4    | 1.00x  | 0.76x  |
| twofish      | 22.3      | 19.3      | 14.4    | 1.54x  | 1.34x  |
| cast5        | 21.3      | 19.3      | 15.4    | 1.39x  | 1.26x  |
| 3des         | 12.3      | 7.8       | 9.6     | 1.29x  | 0.82x  |
| tea          | 18.7      | 18.3      | 21.1    | 0.89x  | 0.86x  |
| xtea         | 19.3      | 13.1      | 7.6     | 2.53x  | 1.71x  |
| **sm4**      | **19.7**  | **16.0**  | **3.1** | **6.30x** | **5.12x** |

> **sm4** 是 Rust 最强的加密算法——比 Go 实现**快 3.3–6.3 倍**（Go 的 SM4
> 比其其他密码慢约 5 倍）。Rust-SM4 的吞吐与 Rust 其他密码持平，而 Go-SM4
> 是严重异常值。
>
> 使用优化后的 1 MB 负载（并发收发 + 预热），Rust-Tokio 和 Rust-Smol 在大多数
> 密码上超越 Go（1.1x–1.6x）。亮点包括 **3des**（1.14× 无压缩，优化前为 0.61×），
> **xor**（1.23× 无压缩，优化前为 0.80×），**tea**（1.60× 无压缩），
> **xtea**（2.53× 压缩），**twofish**（1.54× 压缩）。
>
> **twofish** 在预计算查找表优化后从 0.10x 提升到 1.32x。
> **3des** 在自定义 feistel box DES 实现后从 0.61x 提升到 1.14x。

### 优化历史

| 版本                           | 吞吐        | 延迟（平均）   | vs Go       |
|--------------------------------|------------|---------------|-------------|
| 优化前                         | 5.4 MB/s   | 0.210 s       | 1.41× 慢于   |
| + 实时 `wait_send()`           | 6.2 MB/s   | 0.142 s       | 1.18× 慢于   |
| + 发送时立即 flush             | 7.0 MB/s   | 0.111 s       | 1.19× 慢于   |
| + `Notify` + ACK 通知          | 7.1 MB/s   | 0.114 s       | 1.15× 慢于   |
| + BufferPool + `register_read_waker` | —   | —             | 消除 ~60K allocs/s |
| + `block_in_place` + `spawn_blocking` 批量加密 | — | — | 释放 Reactor 处理 CPU 工作 |
| + 密钥调度修复（blowfish/twofish/3des/aes） | 0.0→3.0 MB/s (blowfish) | — | **100x** |

**净提升：** 吞吐 +31%（null 密码 5.4 → 7.1 MB/s），延迟 −46%（0.210 → 0.114 s）。
Blowfish 100x、Twofish 11x 提升来自密钥调度 bug 修复 + 预计算查找表。

### 密钥调度 Bug 修复

**根因：** `blowfish`、`twofish`、`triple_des` 和 `aes_cfb` 在每块加密函数内调用
`new_from_slice(&self.key)`，对每个块重新执行完整密钥调度。在 CFB-8 模式
（blowfish/3des）下，一个 1350 字节的包触发 1350 次密钥调度。在 CFB-16 模式
（twofish/aes）下，约 85 次密钥调度。

**修复：** 在构造函数中创建一次密码实例并存储在结构体中。对于 Twofish，额外
将 RustCrypto crate（v0.7.1，每块计算 `sbox()` + `gf_mult()`）替换为预计算
`s [4][256]u32` 查找表的自定义实现（与 Go 方式一致）。

| 密码      | 修复前    | 修复后   | 提升    |
|-----------|----------|---------|---------|
| blowfish  | 0.0 MB/s | 3.0 MB/s | 100x    |
| twofish   | 0.4 MB/s | 4.5 MB/s | 11x     |
| 3des      | 2.5 MB/s | 3.3 MB/s | 32%     |
| aes-128   | 3.4 MB/s | 3.1 MB/s | ~持平   |

### 零拷贝优化

| 路径                           | 修复前                     | 修复后                          |
|--------------------------------|----------------------------|--------------------------------|
| 加密：每包分配                 | `vec![]` + `rand`          | 可复用 `BytesMut` + 计数器       |
| 加密：返回到 tokio             | `Vec` 深拷贝               | `Bytes` 引用计数                |
| SMUX：帧负载                   | `copy_from_slice`          | `split_to + slice`（零拷贝）    |
| SMUX：`push_data`             | `extend_from_slice`        | `VecDeque<Bytes>` 追加          |
| 解密：返回负载                 | `.to_vec()`                | `Bytes` 引用计数                |
| KCP `recv`（单段）            | `extend_from_slice`        | `split_to + freeze`（零拷贝）   |
| 服务端会话查找                 | 全局 `Mutex<HashMap>`      | `DashMap`（分片锁）            |

## 构建与测试

### 构建

```bash
# Release 构建（优化、LTO、strip）
cargo build --release
# 或
make release

# Debug 构建
cargo build
# 或
make build
```

Release profile 优化（`Cargo.toml`）：
- `opt-level = 3`——全优化
- `lto = true`——跨 crate 链接时优化
- `codegen-units = 1`——更好的优化，代价是编译时间
- `panic = "abort"`——更小的二进制，无 unwind 表
- `strip = true`——strip 调试符号

### 交叉编译

Makefile 提供 ARM 平台交叉编译目标（如树莓派、OpenWrt 路由器、AWS Graviton）：

```bash
# 列出所有支持的构建目标
make targets

# 安装 Rust 交叉编译工具链（一次性设置）
make install-cross
# 然后安装 C 交叉编译器：
#   macOS:  brew install arm-linux-gnueabihf-binutils aarch64-linux-gnu-binutils
#   Debian: sudo apt install gcc-arm-linux-gnueabihf gcc-aarch64-linux-gnu

# ARMv7（树莓派 2/3、OpenWrt、嵌入式 Linux）
make release-armv7
# 二进制位于：target/armv7-unknown-linux-gnueabihf/release/{kcptun-client,kcptun-server}

# ARM64（树莓派 4/5、AWS Graviton、ARM 服务器）
make release-arm64
# 二进制位于：target/aarch64-unknown-linux-gnu/release/{kcptun-client,kcptun-server}
```

| 目标 | Triple | 典型硬件 |
|------|--------|----------|
| `release-armv7` | `armv7-unknown-linux-gnueabihf` | 树莓派 2/3、OpenWrt、大多数 ARM 单板机 |
| `release-arm64` | `aarch64-unknown-linux-gnu` | 树莓派 4/5、AWS Graviton、ARM 服务器 |

### 运行测试

```bash
# 所有测试
cargo test --all
# 或
make test

# 多线程压力测试（数据完整性 + 并发）
make stress
# 或
cargo test --release --package kcptun-server --test stress_test -- --nocapture --test-threads=1

# 特定并发级别
cargo test --release --package kcptun-server --test stress_test -- test_multithread_100_connections -- --nocapture

# Snappy Go-Rust 互通测试
cargo test test_snappy_go_rust_interop -- --nocapture

# Go 兼容性测试（需要安装 Go）
cd /tmp/kcptun && go test ./std/ -run TestCompStreamRoundTrip -v

# 完整 Go↔Rust 端到端互通测试（需要 Go kcptun 二进制）
bash test_e2e.sh

# Clippy（警告 = 错误）
make clippy
```

### 压力测试覆盖

压力测试验证**数据完整性**，不仅仅是连接成功：

| 测试 | 连接数 | 负载大小 | 验证 |
|------|--------|----------|------|
| `test_single_connection_mixed_sizes` | 1 | 1B、10B、100B、1KB、10KB、64KB | 逐字节回显校验 |
| `test_multithread_10_connections` | 10 | 各 256B | 逐字节回显校验 |
| `test_multithread_50_connections` | 50 | 各 255B | 逐字节回显校验 |
| `test_multithread_100_connections` | 100 | 每连接 1B + 4KB | 两种大小逐字节校验 |
| `test_multithread_large_data` | 100 | 每连接 64KB + 128KB | 两种大小逐字节校验 |

每个负载使用确定性模式（`conn_id + offset ^ 0xA5`），因此任何数据损坏、流混合、截断或丢失都能通过比较回显响应与原始数据立即检测到。

### Go↔Rust 端到端互通测试结果

`test_e2e.sh` 脚本测试所有加密算法、KCP 模式、SMUX 版本、压缩设置和 FEC 参数的 Go↔Rust 兼容性。

**总计：68 通过，0 失败，0 跳过**

#### 加密算法兼容性（Go(version 20260101)↔Rust，`--nocomp`）

| 密码 | Go→Rust | Rust→Go | 备注 |
|------|---------|---------|------|
| `null` | ✅ | ✅ | 无加密，无加密头（已修复：null 模式不再 strip 头） |
| `none` | ✅ | ✅ | 无加密，有加密头 |
| `xor` | ✅ | ✅ | SimpleXOR + PBKDF2 密钥扩展 |
| `aes-128` | ✅ | ✅ | AES-128-CFB |
| `aes-192` | ✅ | ✅ | AES-192-CFB |
| `aes` (aes-256) | ✅ | ✅ | AES-256-CFB（默认） |
| `sm4` | ✅ | ✅ | 手动实现 + tjfoc/gmsm S-box + CK 修复 |
| `tea` | ✅ | ✅ | 已修复：copy_from_slice panic + 8 轮（Go rounds/2） |
| `xtea` | ✅ | ✅ | XTEA，从 Go 源码移植 |
| `salsa20` | ✅ | ✅ | Salsa20 + Go 兼容状态矩阵 |
| `blowfish` | ✅ | ✅ | Blowfish-CFB |
| `twofish` | ✅ | ✅ | Twofish-CFB |
| `cast5` | ✅ | ✅ | 完整 RFC 2144 CAST-128 实现（从 Go cast5 移植） |
| `3des` | ✅ | ✅ | TripleDES-CFB |
| `aes-128-gcm` | ✅ | ✅ | AEAD（已修复：FEC 头偏移 + 正确的缓冲区长度） |

#### KCP 模式兼容性（Go↔Rust）

| 模式 | Go→Rust | Rust→Go |
|------|---------|---------|
| `normal` | ✅ | ✅ |
| `fast` | ✅ | ✅ |
| `fast2` | ✅ | ✅ |
| `fast3` | ✅ | ✅ |

#### SMUX 版本兼容性（Go↔Rust）

| 版本 | Go→Rust | Rust→Go | 备注 |
|------|---------|---------|------|
| SMUX v1 | ✅ | ✅ | 已修复：PSH 帧现在包含 .with_ver(smuxver) 以兼容 v1 |
| SMUX v2 | ✅ | ✅ | 默认，完全兼容 |

#### 压缩 + 加密（Go↔Rust）

| 密码 | Go→Rust + 压缩 | Rust→Go + 压缩 |
|------|---------------------|---------------------|
| `aes-128` | ✅ | ✅ |
| `aes` | ✅ | ✅ |
| `sm4` | ✅ | ✅ | 已修复（CK 常量 + S-box） |
| `tea` | ✅ | ✅ | 已修复（copy_from_slice + 轮数） |
| `blowfish` | ✅ | ✅ |
| `twofish` | ✅ | ✅ |
| `3des` | ✅ | ✅ |

#### FEC 兼容性（Go↔Rust）

| FEC | Go→Rust | Rust→Go |
|-----|---------|---------|
| 10/3 | ✅ | ✅ |
| 4/2 | ✅ | ✅ |

## 许可证

MIT——详见 [LICENSE](LICENSE)。

本项目是 [xtaci](https://github.com/xtaci) 的 [kcptun](https://github.com/xtaci/kcptun) 的 Rust 移植。
