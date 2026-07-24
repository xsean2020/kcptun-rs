# Plan: 客户端 acknodelay 失效修复 + FEC 启用 + ACK 批量优化

> **Status: DONE** — 代码改动已完成，编译/clippy/单元测试/e2e 互操作全绿

| Field | Value |
|-------|--------|
| Status | **DONE** |
| Created | 2026-07-22 |
| Scope | 客户端 UDP reader `kcp.input()` ack_nodelay 参数传递；ACK 批量发送；FEC 参数建议 |
| Out of scope | KCP 状态机逻辑变更、wire 格式变更、SMUX 协议变更 |
| Related | `kcptun-client/src/main.rs`, `kcptun-server/src/main.rs`, `kcp-rs/src/kcp.rs` |

---

## 1. 问题背景

### 1.1 运行环境

```
客户端参数: --mode fast2 --sndwnd 1024 --rcvwnd 1024 --mtu 1350 --dscp 46 --datashard 0 --parityshard 0 --autoexpire 300
服务器参数: 同上
```

### 1.2 SNMP 数据摘要（64 分钟窗口）

| 指标 | 增量 | 分析 |
|------|------|------|
| InSegs | +448,095 | 接收大量数据段 |
| OutSegs | +36,159 | 发送少量数据段（下载为主） |
| OutPkts | +153,587 | UDP 包数远超 OutSegs（ACK 占比 76.5%） |
| LostSegs | +2,581 | 丢包率 7.1%（2581/36159） |
| RepeatSegs | +24,418 | 重复率 5.5%（吞吐量上升后指数增长） |
| FastRetransSegs | +3 | 快重传几乎为零 |
| EarlyRetransSegs | +17 | 早期重传极低 |
| RetransSegs | +2,601 | 99.3% 是 RTO 超时重传 |

### 1.3 核心矛盾

- 丢包率 7.1%，但快重传（`fastresend=2`）几乎未触发
- 99.3% 的重传是 RTO 超时，说明 ACK 到达太慢
- FEC 被禁用（`--datashard 0 --parityshard 0`），所有丢包都依赖重传恢复
- RepeatSegs 随吞吐量指数增长，说明重传产生了大量重复数据

---

## 2. 根因分析

### 2.1 BUG：客户端 `--acknodelay` 完全失效

**服务端**正确传递了 `ack_nodelay` 参数：

```rust
// kcptun-server/src/main.rs:1651
let input_result = kcp.input(slice, self.ack_nodelay);
```

**客户端** UDP reader 任务（Task 1）中，所有 6 处 `kcp.input()` 调用均硬编码 `false`：

```rust
// kcptun-client/src/main.rs — 行 816, 827, 840, 854, 864, 870
kcp_guard.input(input, false)  // ← 硬编码，未使用 acknodelay 配置
```

`acknodelay` 字段存储在 `KcpConn` 结构体中（第 548 行），但 Task 1 的闭包从未捕获它。

**影响链：**

```
客户端收到数据段
  → kcp.input(data, false)        // ACK 仅入队 acklist，不触发 flush()
  → ACK 被延迟到下一个 flush 周期  // fast2 模式 = 20ms
  → 服务端在 20ms + RTT/2 内未收到 ACK
  → RTO 超时（nodelay=1 时 rx_minrto=30ms）
  → 服务端重传
  → 客户端收到重复段 → RepeatSegs 暴涨
  → 快重传无法触发（ACK 还没到，没有重复 ACK 累积）
```

### 2.2 ACK 发送效率低

客户端 ACK 发送路径（Task 1 第 935 行）逐包发送：

```rust
for data in acks {
    let pkt = ...;
    udp1.send(&pkt).await;  // 逐包发送，无批量
}
```

每次 `udp1.send()` 是一次系统调用。在 ACK 占 76.5% 出站包的场景下，系统调用开销显著。

### 2.3 FEC 禁用导致丢包全靠重传

当前 `--datashard 0 --parityshard 0` 禁用了前向纠错。在 7% 丢包率下，每个丢失的包都需要 RTO 超时 + 重传恢复，延迟代价 = RTO（30ms+） + 重传时间。如果启用 FEC 10/3，大部分单包丢失可以直接从冗余分组恢复，无需任何重传。

---

## 3. 修复方案

### M1 — 客户端 input() 传递 acknodelay 参数（最高优先级）✅ 已完成

**目标：** 修复 `--acknodelay` 配置在客户端 UDP reader 中的传递。

**改动范围：** `kcptun-client/src/main.rs`

**具体步骤：**

1. 在 `start_background_loops()` 中，将 `acknodelay` 值传入 Task 1 闭包
2. 将 Task 1 中所有 6 处 `kcp_guard.input(input, false)` 改为 `kcp_guard.input(input, acknodelay1)`
3. 涉及的代码行（当前行号，可能因编辑变化）：
   - 第 816 行：FEC 解码后 input
   - 第 827 行：FEC 恢复后 input
   - 第 840 行：FEC 类型分支 input
   - 第 854 行：非 FEC input 路径 1
   - 第 864 行：非 FEC input 路径 2
   - 第 870 行：非 FEC input 路径 3

**验证：**
- `cargo build --workspace` 编译通过
- `cargo clippy --workspace -- -D warnings` 零警告
- `cargo test --workspace` 全绿
- 运行后观察 SNMP：`LostSegs` 和 `RepeatSegs` 增量大幅下降

### M2 — 客户端 ACK 批量发送 ✅ 已完成

**目标：** 将逐包 ACK 发送改为批量发送，减少系统调用。

**改动范围：** `kcptun-client/src/main.rs` Task 1 ACK 发送路径（第 925–960 行附近）

**具体步骤：**

1. 收集所有 ACK 的 `Bytes` 后，先批量加密
2. 使用 `udp1.send_batch(&encrypted_acks)` 一次发送
3. 保留当前 FEC 编码逻辑（在加密前对 ACK 包做 `fec_expand_packets`）

**当前代码结构：**
```rust
let acks: Vec<bytes::Bytes> = std::mem::take(&mut *raw_packets1.lock());
// FEC expand...
for data in acks {
    let pkt = encrypt(data);
    udp1.send(&pkt).await;  // ← 逐包发送
}
```

**目标代码结构：**
```rust
let acks: Vec<bytes::Bytes> = std::mem::take(&mut *raw_packets1.lock());
// FEC expand...
let encrypted: Vec<bytes::Bytes> = acks.iter().map(|d| encrypt(d)).collect();
udp1.send_batch(&encrypted).await;  // ← 批量发送
```

**验证：**
- `cargo build --workspace` 编译通过
- `cargo clippy --workspace -- -D warnings` 零警告
- 观察 SNMP：`OutPkts` 不变但 CPU 使用率下降

### M3 — 应用参数优化建议（运行时配置，非代码改动）

**目标：** 修正启动参数以匹配实际网络条件。

**状态：** M3 为部署参数建议，不涉及代码改动。需在实际运行环境中验证效果后决定是否启用 FEC。

| 参数 | 当前值 | 建议值 | 理由 |
|------|--------|--------|------|
| `--acknodelay` | 未设置 | **启用** | M1 修复后生效；ACK 即时发出，消除 RTO 超时 |
| `--datashard` | 0 | **10** | 7% 丢包率下 FEC 10/3 可恢复绝大多数单包丢失 |
| `--parityshard` | 0 | **3** | 配合 datashard=10，30% 开销换取消除重传 |
| `--mode` | fast2 | **fast3**（可选） | interval 20ms→10ms，进一步降低 flush 延迟 |
| `--mtu` | 1350 | 保持 | 当前 MSS 利用率 49% 是小包 ACK 导致，修复 acknodelay 后会改善 |
| `--sndwnd` | 1024 | 保持 | RingBufferSndQueue=0，窗口充足 |
| `--rcvwnd` | 1024 | 保持 | RingBufferRcvQueue=0，接收窗口充足 |
| `--dscp` | 46 | 保持 | EF 级别 QoS 标记合理 |
| `--autoexpire` | 300 | 保持 | 5 分钟空闲超时合理 |

**注意：** `--datashard` 和 `--parityshard` 必须客户端和服务端同时修改，否则握手失败。

---

## 4. 实施顺序

```
M1 (acknodelay 修复) → M2 (ACK 批量发送) → M3 (参数建议)
```

M1 是根因修复，单独完成即可获得最大收益。M2 是性能优化，锦上添花。M3 是参数调整，需在 M1 修复后验证效果再决定是否需要 FEC。

---

## 5. 预期效果

### 5.1 M1 修复后预期

| 指标 | 当前增量 | 预期改善 |
|------|----------|----------|
| LostSegs | 2,581 | 不变（丢包是网络层） |
| RepeatSegs | 24,418 | **→ ~2,000**（ACK 及时到达，重传大幅减少） |
| RetransSegs | 2,601 | **→ ~500**（快重传接管大部分恢复） |
| FastRetransSegs | 3 | **→ ~400+**（ACK 不再延迟，快重传正常工作） |
| OutPkts (ACK) | 117,428 | **→ ~30,000**（无重复 ACK 触发的额外 ACK） |

### 5.2 M1 + M3 (FEC 10/3) 预期

| 指标 | 当前增量 | 预期改善 |
|------|----------|----------|
| LostSegs | 2,581 | **→ ~200**（FEC 恢复 90%+ 丢包） |
| RepeatSegs | 24,418 | **→ ~500**（几乎无重传） |
| RetransSegs | 2,601 | **→ ~100**（FEC + 快重传覆盖） |

### 5.3 M2 优化预期

- ACK 发送系统调用减少 ~70%（从逐包到批量）
- CPU 使用率在 ACK 密集场景下降低 ~5-10%

---

## 6. 验收标准

### 6.1 编译与测试

- [x] `cargo build --workspace` — 零错误
- [x] `cargo clippy --workspace -- -D warnings` — 零警告
- [x] `cargo test --workspace --lib` — 全绿（71 个单元测试通过）
- [x] `cargo fmt --all` — 格式正确

### 6.2 功能验证

- [x] 客户端 `--acknodelay` 参数在 Task 1 中正确传递（代码审查确认：`acknodelay1` 传递至全部 6 处 `kcp.input()`）
- [x] ACK 批量发送功能正常（代码审查确认：`udp1.send_batch()` 替代逐包 `udp1.send()`，SNMP 统计模式与 flush loop 一致）
- [ ] 启用 `--acknodelay` 后，SNMP `FastRetransSegs` 增量显著上升（待部署验证）
- [ ] 启用 `--acknodelay` 后，SNMP `RepeatSegs` 增量显著下降（待部署验证）

### 6.3 互操作验证

- [x] `bash test_e2e.sh` — Go↔Rust 互操作测试全绿（138 passed, 0 failed, 0 skipped）
- [ ] 客户端 `--acknodelay` + 服务端 `--acknodelay` 组合正常（待部署验证）
- [ ] FEC 10/3 客户端 + 服务端组合正常（待部署验证）

### 6.4 性能验证（待部署后 64 分钟窗口对比）

- [ ] 64 分钟运行后 SNMP 数据对比：`RepeatSegs` 增量下降 >80%
- [ ] `FastRetransSegs` 增量从个位数上升到百级
- [ ] `OutPkts` 中 ACK 占比从 76.5% 下降到 <40%

---

## 7. 风险与回退

### 7.1 风险

| 风险 | 概率 | 影响 | 缓解 |
|------|------|------|------|
| acknodelay 导致 ACK 风暴 | 低 | ACK 包数过多 | KCP 内部有 acklist 满 flush 机制（mtu/KCP_OVERHEAD 个 ACK 触发 flush） |
| FEC 10/3 增加带宽开销 | 确定 | +30% UDP 流量 | 7% 丢包率下 FEC 恢复代价远小于重传代价 |
| 批量 ACK 发送兼容性 | 极低 | 无 | `send_batch` 已在 flush loop 中使用，路径成熟 |

### 7.2 回退方案

- M1：将 `acknodelay1` 改回 `false` 即可回退
- M2：恢复逐包发送循环
- M3：参数改回 `--datashard 0 --parityshard 0`

---

## 8. 涉及文件

| 文件 | 改动类型 |
|------|----------|
| `kcptun-client/src/main.rs` | M1: Task 1 闭包传入 acknodelay + 6 处 input() 参数修改；M2: ACK 发送路径重构 |
| 无新增文件 | — |

---

## 9. 修订记录

| 日期 | 说明 |
|------|------|
| 2026-07-22 | 初稿，基于 SNMP 日志分析和代码审查 |
| 2026-07-23 | M1 + M2 代码实现完成；编译/clippy/单元测试/fmt/e2e 全绿（138 互操作测试通过） |
