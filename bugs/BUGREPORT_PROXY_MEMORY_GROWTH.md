# Bug Report: 代理场景 SMUX 流泄漏导致 RSS 持续上涨

> **严重级别**: High（小内存设备上 Critical）  
> **影响范围**: 客户端 `kcptun-client`（及同构代理用法），尤其 `--conn 1` + 本地大量短 TCP 连接  
> **状态**: **Fixed** — 见 修订记录  
> **发现日期**: 2026-07-21  
> **相关历史**: `BUGREPORT.md`（单 KCP 通道死锁 / 客户端 FIN 逻辑变更）

---

## 1. 现象描述

### 1.1 触发条件（生产/路由代理）

典型命令（koolshare 等）：

```text
kcptun -l 127.0.0.1:1091 -r HOST:50200-50250 \
  --mode normal --mtu 1300 --autoexpire 300 \
  --snmplog /tmp/snmp.log --snmpperiod 120
```

- 作为 **本地代理**：业务侧不停 **创建 / 关闭** 到 `127.0.0.1:1091` 的 TCP。
- 每条本地 TCP 对应 **一条 SMUX stream**（同一条 KCP/UDP 上复用）。
- `--conn` 未写时默认为 **1**（单 KCP 通道）。
- 远端 `50200-50250` 仅为端口范围，**不会**自动变成 51 条 KCP 连接。

### 1.2 表现

| 观察 | 数值/特征 |
|------|-----------|
| RSS | 约 **785 MB → 913 MB**（持续上涨） |
| 内存占比 | 约 **89% → 103%+** 物理内存（小内存路由） |
| 行为 | 代理负载下 **只涨不跌**（或几乎不回落） |
| 对比预期 | 单隧道空闲/轻载合理量级多为 **数十 MB**，非近 GB |

**关键特征**: 内存随「历史连接次数 / 未清理流」近似线性增长，而非稳定在「当前活跃连接数」对应水位。

### 1.3 与「大默认缓冲」的区别

| 类型 | 能否解释「偏大」 | 能否解释「持续上涨」 |
|------|------------------|----------------------|
| 默认 `streambuf=2MB` / `smuxbuf=4MB` / sockbuf | ✅ | ❌（活跃流稳定后应平台） |
| **流从 map 中无法 remove + 每流 capacity** | ✅ | ✅ |
| 仅端口范围 51 口 | ❌（非 ×51 连接） | ❌ |

结论：**默认缓冲放大伤害；无界流表 + 预分配才是「一直涨」的主因。**

---

## 2. 根因分析

### 2.1 数据面生命周期（代理）

```text
App TCP connect → client accept
  → session.open_stream()          // streams HashMap 插入
  → SYN + pipe(local TCP ↔ SMUX)
  → App TCP close / idle timeout
  → handle_client 结束:
       mark_local_closed()
       mark_fin_sent()             // 仅打标，不保证发出 FIN 帧
       // 不调用 stream.close() 清空缓冲
       // 不从 session.streams remove
  → 仅当 local_closed && remote_closed && fin_sent
       才在 flush 循环中 remove(id)
```

### 2.2 缺陷 A — 清理条件过严 + Client 不发 FIN（逻辑泄漏）

**位置（client）**

- `handle_client` 结束：`mark_local_closed` + `mark_fin_sent`（`kcptun-client/src/main.rs`）。
- 历史原因：为修复单通道死锁，**移除了客户端主动发送 FIN 帧**（见 `BUGREPORT.md` / commit `3af239a` 叙事），假设 **由服务端 flush 发 FIN**。
- Flush 清理条件仍为：

```text
local_closed && remote_closed && fin_sent
```

| 标志 | 设置方式 | 代理短连接常见结果 |
|------|----------|--------------------|
| `local_closed` | 本地 pipe 结束 | ✅ |
| `fin_sent` | **本地直接 mark，未必发过 FIN** | ✅ 被置 true |
| `remote_closed` | **须收到对端 FIN** | 常为 **false** |

当对端 FIN 迟到、丢失，或对端半开未关时：

- 三条件永不满足 → **`session.streams` 永不 `remove`**
- 条目数随「打开过的流」**无界增长**

**这是代理场景下的逻辑泄漏（unbounded growth）。**

### 2.3 缺陷 B — 每流按 `streambuf` 预分配（放大 RSS）

`Stream::with_buffer(id, streambuf)` 使用：

```text
recv_buf ≈ BytesMut::with_capacity(streambuf)
默认 streambuf = 2 * 1024 * 1024  // 2MB
```

| 未从 map 删除的流数量 | 仅 recv capacity 量级 |
|----------------------|------------------------|
| 100 | ~200 MB |
| 300 | ~600 MB |
| 400+ | **~800 MB+**（与 785→913 MB 同量级） |

即使缓冲未写满，**capacity 仍占 RSS**（分配器/页）。

### 2.4 缺陷 C — `--autoexpire` 实现为空转（无法兜底）

当 `autoexpire > 0` 时，scavenger 当前逻辑等价于：

```text
loop { sleep 30s; }  // 仅日志，不删 KcpConn、不扫 streams
```

注释写明：单连接 client 上 KCP 会话长期存活。  
因此 **`--autoexpire 300` 不能回收 SMUX 僵尸流**，对本次 RSS 上涨 **无实质帮助**。

### 2.5 次要放大因素（非「只涨」的充分条件）

| 项 | 默认（Rust client） | 说明 |
|----|---------------------|------|
| `smuxbuf` | 4 MB / session | session 级 |
| sock 缓冲 | `kio` `SOCK_BUF=2MB` 收+发 | 每 UDP |
| FEC | datashard=10, parityshard=3 | 额外缓存；次要 |
| `stream_id` | 只增不减 | 本身小；map 条目在涨 |
| 关流不 `stream.close()` | — | 不清 send/recv 队列 |
| mimalloc /（若为 Go 二进制）堆 | — | 归还 OS 延迟，次要 |

### 2.6 与历史 FIN 修复的张力

| 时期 | 行为 | 后果 |
|------|------|------|
| 引入 client 发 FIN 但分块不完整 | 死锁 / 超时 | `BUGREPORT.md` Critical |
| 去掉 client 发 FIN，靠 server 发 FIN | 部分场景可用 | **代理 client 侧 map 依赖 remote FIN** |
| 清理仍要求 `remote_closed` | — | **半开流堆积 → 内存涨** |

根治需在 **不重引入死锁** 的前提下，补齐 **FIN 发送或超时强制回收**。

---

## 3. 证据与观测（已有 / 建议）

### 3.1 已有现场证据

- RSS 785 MB → 913 MB，MEM 超 100%。
- 负载形态：代理 + 高频建连断连。
- 代码路径审查：清理条件 + 预分配 + 空 scavenger（见上）。

### 3.2 设备上建议复测（执行修复前）

```sh
# RSS 时间序列
while sleep 30; do ps | grep kcptun | grep -v grep; done

# 停掉所有使用 1091 的业务 10 分钟后再看 RSS
#   几乎不回落 → 泄漏 / 不归还 OS
#   明显下降 → 以「按流缓存」为主（仍可能有清理延迟）

# 若可开日志：accepted/open 次数 vs stream closed / map 长度
# RUST_LOG=info 关注 stream id 是否只增、closed 是否不对称
```

### 3.3 代码侧可加的诊断（下期实现，本期不做）

- 周期性日志：`session.stream_count()`、RSS（若平台可得）。
- 指标：open_stream 累计 / remove 累计 / 僵尸流（local_closed && !remote_closed 时长）。

---

## 4. 影响评估

| 环境 | 影响 |
|------|------|
| 路由器 / koolshare（≤1 GB） | OOM、进程被杀、整机卡顿 |
| 桌面/服务器大内存 | 可能长期不被注意，仍属泄漏 |
| 长连接、流数少 | 可能不明显 |
| 代理 / 浏览器 / 多设备短连接 | **高风险** |

---

## 5. 缓解措施（不改代码，立即可做）

在修复合并前，用配置压单流成本并 **定期重启**：

```sh
kcptun \
  -l 127.0.0.1:1091 \
  -r HOST:50200-50250 \
  --mode normal --mtu 1300 \
  --autoexpire 300 \
  --conn 1 \
  --smuxbuf 262144 \
  --streambuf 32768 \
  --sockbuf 262144 \
  --sndwnd 128 --rcvwnd 128 \
  --datashard 0 --parityshard 0
```

| 参数 | 作用 |
|------|------|
| `streambuf 32K` | 每流 2 MB → 32 KB（降速涨幅） |
| `smuxbuf 256K` | session 4 MB → 256 KB |
| 关 FEC | 少缓存/CPU |
| 进程重启 | 释放已泄漏 map（治标） |

**注意**: 缩小缓冲 **不能替代** 关流回收修复。

---

## 6. 修复计划（下一执行计划 — 暂不实施）

> **本文件为计划与依据；在用户明确要求前不改代码。**

### 6.1 目标

1. 代理高频开关连接下，RSS **有上界**（与活跃流数相关，而非历史流数）。  
2. 不重引入 `BUGREPORT.md` 中的单通道死锁。  
3. 默认值对嵌入式更安全（可选，可第二阶段）。

### 6.2 推荐实现顺序（一次一类）

| 步骤 | 内容 | 验收 |
|------|------|------|
| **M1** | `handle_client` 结束调用 **`stream.close()`**（清空 send/recv） | 单测；关流后缓冲释放 |
| **M2** | **超时强制回收**：`local_closed` 且超过 T 秒仍无 `remote_closed` → remove + close | 代理压测：断干净后 map 回落 |
| **M3** | Client **可靠发送 FIN**（对齐 Go 同步入队 FIN，再 mark_fin_sent；含失败重试/与 flush 协作） | e2e + stress 100 流 |
| **M4** | `open_stream` **按需扩容**，避免 `with_capacity(streambuf)` 一次吃满 | 空闲流 RSS 下降 |
| **M5** | 实现或删除空 **autoexpire scavenger**；至少扫僵尸流 | autoexpire 文档与行为一致 |
| **M6**（可选） | 嵌入式默认 / `lowmem` 预设（smuxbuf/streambuf/sockbuf/FEC） | 文档 + 路由推荐命令 |

**依赖**: M3 与死锁修复冲突风险最高，须在 stress（单 conn 多流）下回归 `BUGREPORT.md` 场景。

### 6.3 建议测试矩阵

| 测试 | 内容 |
|------|------|
| 单元 | stream close 清空缓冲；超时 remove |
| Stress | `make stress` / 100 连接反复开关 |
| 代理模拟 | 脚本：N 线程循环 connect-write-close，观察 `stream_count` 与 RSS |
| e2e | `bash test_e2e.sh`（FIN 变更后） |
| 回归 | 单 KCP 100×128KB（原死锁场景） |

### 6.4 非目标（本期）

- 不改协议 wire 格式。  
- 不默认开启危险拥塞参数。  
- 不把 DPDK/io_uring 纳入本 bugfix。

---

## 7. 相关代码索引

| 区域 | 文件（约） |
|------|------------|
| Client 关流 | `kcptun-client/src/main.rs` — `handle_client` |
| Client 流清理 | `kcptun-client/src/main.rs` — flush Phase 1a `remove` |
| Client 开流 | `kcptun-client/src/main.rs` — accept + `open_stream` |
| 空 scavenger | `kcptun-client/src/main.rs` — `autoexpire` 任务 |
| SMUX FIN / map | `smux-rs/src/session.rs` — `Cmd::Fin`，`streams` |
| Stream 缓冲 | `smux-rs/src/stream.rs` — `with_buffer`，`close` |
| 默认缓冲 | client `smuxbuf`/`streambuf`；`kio-rs` `SOCK_BUF` |
| 历史 FIN 死锁 | `BUGREPORT.md` |

---

## 8. 判定摘要

| 问题 | 结论 |
|------|------|
| 是否像内存泄漏？ | **是（逻辑泄漏：SMUX 流表无界增长）** |
| 主因？ | **关流清不掉 map + 每流 ~2 MB 预分配** |
| 代理场景？ | **高危**（创建/关闭连接放大） |
| autoexpire？ | **当前无效兜底** |
| 端口范围？ | **非主因** |
| 本期是否改代码？ | **否** — 仅文档与计划 |

---

## 9. 修订记录

| 日期 | 说明 |
|------|------|
| 2026-07-21 | 初稿：现场 RSS 上涨 + 代码审查；列为下一执行计划，暂不实现 |
| **2026-07-22** | **修复合入**（M1/M2/M4/M5）：<br>• `handle_*` 只 `mark_local_closed` + `clear_buffers`（不 touch `fin_sent`）<br>• 双方 flush: FIN 候选 **先编码后 mark**（send 成功后统一 mark_fin_sent）<br>• `reap_stale_streams(30s)` → 僵尸流强制回收<br>• lazy `BytesMut`（不再 `with_capacity(streambuf)`）<br>• `Session::remove_stream()` — SYN 失败路径泄漏<br>• autoexpire scavenger 调 `reap_stale_streams`<br>• server 对称修复（原报告未覆盖 server 同款假 mark_fin_sent） |
