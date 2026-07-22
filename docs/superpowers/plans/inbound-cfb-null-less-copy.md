# Plan: server/client 入站 in-place CFB + null 少拷

> **Canonical path (git):** `docs/superpowers/plans/inbound-cfb-null-less-copy.md`  
> `.omc/plans/` is gitignored; optional local symlink only for OMC.

| Field | Value |
|-------|--------|
| Status | **implemented (Phase 1–4 core; Phase 0/5 optional)** |
| Created | 2026-07-22 |
| Scope | Inbound decrypt / header strip / feed into KCP only |
| Out of scope | Cipher algorithms, outbound `encrypt_batch`, wire format, global crypto pool |
| Related | `kcp-rs/src/crypto_buf.rs`, `kcptun-server` `feed_data`, client UDP reader, `bench/profiles/HOTSPOTS.md` |

**原则：** 先可复现的 alloc 证据 → 再改代码 → 再 e2e/stress/bench 关门。  
**本文件只存方案，不表示已开工。**

---

## 0. 目标与非目标

### 目标

| ID | 目标 |
|----|------|
| G1 | CFB 入站：消除「每包 `data.to_vec()` 再 decrypt」的整包堆分配（server 主路径） |
| G2 | null 入站：能 slice 进 `KCP::input` 的场景不再 `to_vec` / `Bytes::copy_from_slice` |
| G3 | 解密后 **20B 头** 用 slice 偏移去掉，避免 `drain` 导致的 memmove（在 in-place 缓冲上） |
| G4 | client/server **语义对齐**（CRC、header 探测、FEC 与 KCP 输入边界一致） |
| G5 | 先有可复现 alloc/拷贝计数，再改实现，避免“感觉快了” |

### 非目标（明确不做）

- 改 CFB IV / nonce / CRC wire
- 3DES/AES 轮函数、monomorphize
- 出站 `CryptoBuf` / `encrypt_batch` / 并行 encrypt
- 全局 crypto 内存池
- SMUX/Snappy 出站缓冲
- FEC 恢复算法本身（仅减少「已解密缓冲上的多余 `to_vec`」）
- 为 zero-copy 改 `KCP::input` 所有权模型（`input(&[u8])` 已够用）

---

## 1. 现状对照（方案依据）

### 1.1 Server（`KcpServerSession::feed_data`）

```text
UDP: recv_from(&mut buf) | try_recv_batch → owned Vec
  → process_datagram(peer, data: &[u8])   // 借用，不拥有
  → feed_data(data)
       CFB:  dec_data = data.to_vec()     // 每包 1 次整包 alloc + memcpy
             crypt.decrypt(&mut dec_data)
             CRC → drain(..20)            // 头去掉：memmove 整 payload
       null: data.to_vec()                // 又一次整包 alloc
       FEC:  多处 feed_slice[FEC_HDR..].to_vec() / recovered.to_vec()
       KCP:  input(&[u8]) 后 recv → 再 to_vec 进 SMUX 结果
```

- 入参是 **`&[u8]`**，函数签名逼着你为了 in-place decrypt **再拷一份**。
- 主 recv 的 `buf` 与 batch_slots **本可以**成为唯一可变所有者，但被 `feed_data(&[u8])` 切断。
- 返回 `Vec<Vec<u8>>` 又在应用层堆出一串消息（与本次「解密少拷」可分阶段）。

### 1.2 Client（UDP reader 内联，无 `feed_data` 名）

```text
UDP: recv → buf[..n]
  CFB:  copy → dec_scratch[..n]          // 1 次 memcpy 到会话复用 scratch
        CryptoBuf::decrypt_cfb → 再 copy 进 enc_buf → Bytes payload  // 第 2 次 payload copy
  null: Bytes::copy_from_slice(&buf[..n]) // 每包 alloc+copy
  AEAD: open → Vec → Bytes::from
  FEC:  多数直接 input(&input[FEC_HDR..])  // 已较好
  KCP:  input(&[u8])；nocomp 时 process_data(&Bytes) 零拷贝感较好
```

- 已有 **P1.3 `dec_scratch`**，比 server 先进一档，但仍是 **「scratch 解密 + decrypt_cfb 再拷出 Bytes」** 双拷。
- null 仍强制 `Bytes` 拥有一份，尽管 `input` 只需要在调用期间有效的 `&[u8]`。

### 1.3 共享约束

| 事实 | 含义 |
|------|------|
| `KCP::input(data: &[u8], …)` | 解析期只借；payload 进 rcv 时 **库内会拷入 Segment**（协议状态需要） |
| CFB 必须 in-place 改整包含 20B 头 | 解密缓冲必须是 **`&mut [u8]` 且长度 = 密文长** |
| CRC = `crc32fast::hash(payload)` | 校验只需 `data[20..]`，不必拥有新 `Vec` |
| `null` 无 crypto 头 | 可直接 `input` 原始 datagram（及 FEC 偏移） |
| batch_slots 是 `Vec<Vec<u8>>` | 包所有权可下沉到 session 处理完再归还 slot |

**因此「少拷」的上界：**  
入站侧最多再省 **1～2 次 每包整包/payload 堆分配与 memcpy**；**无法**省掉 KCP 内部把 segment data 收进 rcv_buf 的那次（除非大改 KCP，非本方案）。

---

## 2. 目标数据流（目标态）

### 2.1 Server（推荐）

```text
recv / batch_slot: 唯一可变缓冲 B (Vec 或 BytesMut)
  → process_datagram_owned(peer, &mut B[..n])  // 或 take Vec 再还池
  → inbound_decode_in_place(&mut B[..n], crypt, …) -> InboundView
       CFB: decrypt in-place on B
            CRC on B[20..]
            view = B[20..n]   // 零 alloc 去头
       null: view = B[0..n]
       AEAD: 见阶段 C（可 open 进会话 scratch）
  → FEC: 对 view 做类型分支；kcp.input(&view[fec_off..]) 尽量无中间 Vec
  → kcp.recv_bytes() → SMUX（消息缓冲另阶段）
  → 归还 B 到 batch 槽 / 复用主 buf
```

### 2.2 Client（对齐）

```text
buf: 唯一 recv 缓冲
  CFB: decrypt in-place on buf[..n]（或先确认无并发读同一 buf）
       CRC；kcp.input(&buf[20..n][fec?])
       不再 decrypt_cfb 二次拷进 CryptoBuf.enc_buf
  null: kcp.input(&buf[..n][fec?])  // 无 Bytes::copy_from_slice
  同一 task 内 input 同步完成后再下次 recv 覆写 buf  // 生命周期安全
```

**生命周期铁律：**  
`input(&[u8])` 返回后，KCP **不得**再持有该 slice；当前实现满足 → in-place 后 **同 task 内同步 input 完毕** 即可再次 `recv` 覆写。

---

## 3. 阶段划分（按序；可独立 revert）

```text
Phase 0  基线与可复现 alloc 探针
Phase 1  共享入站解码原语（kcp-rs 或 binaries 旁路 helper）
Phase 2  Server：feed_data 改为 in-place / owned mut buffer
Phase 3  Client：去掉双拷 CFB + null 少拷
Phase 4  FEC 切片少 to_vec（依赖 2/3 的 view）
Phase 5  AEAD open_into（可选，同主题延伸）
Phase 6  验证矩阵 + 是否回写 AGENTS（仅当 API 真变）
```

每阶段：**可独立 revert、有验收、不依赖下一阶段完成才可合并**（建议仍按序合入 e2e）。

---

## 4. Phase 0 — 可复现 alloc / 拷贝探针（先于改逻辑）

### 4.1 目的

1. 入站路径每包实际 **heap alloc 次数 / 字节** 是多少？  
2. 改完后是否 **显著下降**（而不是 thr 噪声）？

### 4.2 探针设计（由轻到重）

| 探针 | 做法 | 指标 |
|------|------|------|
| **A. 逻辑计数器（首选，CI 友好）** | `#[cfg(feature = "inbound_alloc_trace")]` 或 test-only `AtomicU64`：在 `data.to_vec` / `Bytes::copy_from_slice` / `decrypt_cfb` 的 copy 出口、`drain` 等打点 | `inbound_full_copies`, `inbound_payload_copies`, `inbound_null_copies` 每包均值 |
| **B. 微基准** | `criterion` 或现有 bench 旁路：固定 512B/1200B 密文，循环 N 次「decode + input 空 KCP」 | ns/op、allocs/op（`dhat` 或 `stats_alloc`） |
| **C. dhat / heaptrack（本地）** | 短时 server+client L2/L3 bulk | 热点栈是否仍落在 `feed_data`/`to_vec` |
| **D. thr 对照** | 已有 `bench_rust_vs_go` / profile L2 aes、L3 3des、L1 null | MB/s；**辅证**，不作唯一成功标准 |

### 4.3 最小可复现场景

| 场景 | 配置 | 期望看到的基线浪费 |
|------|------|-------------------|
| S-null | `--crypt null --nocomp` 单连接 bulk | server 每包 ≥1× `to_vec`；client 每包 `Bytes::copy_from_slice` |
| S-cfb | `--crypt aes` 或 `aes-128`，nocomp | server 每包 `to_vec`+decrypt；client scratch copy + decrypt_cfb copy |
| S-cfb-fec | 开启 datashard/parityshard | 额外 FEC 切片 `to_vec` |
| S-multi | stress 多连接 | 分配器争用放大 A 的计数 |

### 4.4 基线记录模板（改代码前填）

```text
date / git / host / profile (release|profiling)
scenario: S-cfb | size | duration
counters: full_copies/pkt = ?  payload_copies/pkt = ?  null_copies/pkt = ?
optional: dhat total blocks, thr MB/s
```

**出门标准（Phase 0）：**  
同一场景跑两次计数差 <5%；文档里写清「改前数字」。无数字不开 Phase 2。

---

## 5. Phase 1 — 共享入站解码原语

### 5.1 建议 API 形状（逻辑，非最终签名）

放在 **`kcp-rs`（与 `CryptoBuf` 同层）** 优先，让 client/server 共用 CRC/header 规则：

```text
pub struct InboundPacket<'a> {
    /// 指向「KCP 或 FEC 头开始」的明文视图（已无 CFB 20B 头）
    pub body: &'a [u8],
}

/// 在 buf 上原地 CFB 解密；成功则 body = &buf[CRYPT_HDR..]
pub fn decrypt_cfb_in_place(
    buf: &mut [u8],
    crypt: &dyn BlockCrypt,
) -> Result<&[u8], InboundCryptError>;  // Short / CrcMismatch

/// null：body = 全缓冲
pub fn inbound_null(buf: &[u8]) -> &[u8] { buf }
```

**与现有 `CryptoBuf::decrypt_cfb` 关系：**

| | 现 `decrypt_cfb` | 新 `decrypt_cfb_in_place` |
|--|------------------|---------------------------|
| 输入 | `&mut [u8]` | 同 |
| 输出 | `Option<Bytes>`（**再拷进 enc_buf**） | `Result<&[u8]>`（**零额外 payload 拷**） |
| 用途 | 需要 `'static`/跨 await 持有明文 | **同 task 内立刻 `kcp.input`** |

**策略：**

- 热路径改用 in-place view。
- 保留 `decrypt_cfb` 给测试/需要 `Bytes` 的调用方，或标 `#[deprecated]` 仅测用。
- **不要**让 in-place 路径再走 `enc_buf.copy_from_slice`。

### 5.2 Header 探测（server 现有逻辑）

Server 今日：

```text
decrypt 整包后看 dec_data[4] 是否为 KCP cmd 0x51–0x54
  不是 → 认为有 20B crypto 头，做 CRC 再 drain
  是   → 当作无头（兼容路径）
```

Client 今日：走 `decrypt_cfb`，**默认总有 20B 头**（与 Go CFB 正常包一致）。

**方案要求：**

1. 抽出 **单一函数** `strip_cfb_header_if_present(buf: &mut [u8]) -> Result<&[u8], …>`  
   - 先 `decrypt` 整包（若尚未解密则由调用方保证）  
   - CRC + 可选 cmd 探测与 server **字节级一致**  
2. Client/server 都调用它，避免「一边总是 20B、一边探测」的长期分叉（若 Go 互操作要求 client 必须始终有头，则探测仅 server 历史兼容，**写进注释与测试向量**）。

### 5.3 单元测试（Phase 1 验收）

| 用例 | 断言 |
|------|------|
| AES roundtrip in-place | plaintext body 与加密前 payload 相等；输出为原 buf 子切片 |
| CRC 错误 | `Err`；buf 可脏，调用方 drop |
| 短于 20B | `Err` |
| 与 `encrypt_cfb` 互操作 | `CryptoBuf::encrypt_cfb` → in-place decrypt |
| null | `inbound_null` 恒等 |
|（可选）cmd 探测边界 | 构造「无头」假包若仍支持 |

**出门标准：** `cargo test -p kcp-rs` 全绿；新旧 API 各至少 1 条 roundtrip。

---

## 6. Phase 2 — Server 改造

### 6.1 API 演进选项（三选一，推荐 B）

| 方案 | 描述 | 优点 | 缺点 |
|------|------|------|------|
| **A** | `feed_data(&self, data: &mut [u8])` | 最小概念 | 主循环 `buf` 与 batch 都要变成 mut 下传；`&[u8]` 调用点全改 |
| **B（推荐）** | `feed_data_mut(&self, data: &mut [u8])` 新方法；旧 `feed_data` 内部 `to_vec` 转调并短期兼容 | 热路径干净；可灰度 | 双入口一阵 |
| **C** | `feed_datagram(&self, mut data: Vec<u8>) -> …` 吃 owned，返回后 Vec 可还池 | 与 batch_slots 所有权匹配 | 主路径 `recv_from` 仍是固定 buf，要 copy 进 slot 或改 recv 模型 |

**推荐落地组合：**

1. 热路径：**B** — `process_datagram` 改为对 **`&mut [u8]`** 调用 `feed_data_mut`。  
2. 主循环：`recv_from(&mut buf)` → `process_datagram(peer, &mut buf[..n])`。  
3. batch：`try_recv_batch` 得到 owned `Vec` → `process_datagram(peer, pkt.as_mut_slice())`，处理完 **clear 还槽**。  
4. 旧 `feed_data(&[u8])`：仅测试或 `to_vec` 转调，changelog 标明 deprecated。

### 6.2 `feed_data_mut` 内部步骤（伪流程）

```text
1. 长度 / 空包
2. AEAD: 暂仍 open→Vec（Phase 5）；或 open_into(session_scratch)
3. CFB:
     decrypt_cfb_in_place(buf, crypt)?
     body = strip header view  // 无 drain
4. null:
     body = buf  // 无 to_vec
5. FEC:
     若有 decoder：decode 可能仍 alloc（库内）；对「本包 data 片」优先
     kcp.input(&body[fec_hdr..]) 或 input(body)  // 禁止 body[fec..].to_vec() 仅因 input
6. recv_bytes 循环 → 组装 messages
7. 不在此函数返回后使用 body
```

### 6.3 `drain(..20)` 替换

| 旧 | 新 |
|----|-----|
| `dec_data.drain(..20)` 拥有缩短的 Vec | `let body = &buf[20..]` |

**注意：** 若 FEC decoder API 要 `Vec`，**单独**为 recovered 分配；**不要**因此把整包 data 路径也 `to_vec`。

### 6.4 与 `flush` / 并发

- in-place 只碰 **本包缓冲**，不碰出站 `CryptoBuf.enc_buf`。  
- **入站 CFB 不要占用出站 `CryptoBuf.enc_buf`**（client 今 `decrypt_cfb` 会写 enc_buf — Phase 3 一并去掉）。

### 6.5 Server 验收

| 检查 | 方法 |
|------|------|
| 计数 | S-cfb：`inbound_full_copies/pkt` → 0（或仅 FEC recovered） |
| 正确性 | `bash test_e2e.sh` crypt 矩阵；`make stress` |
| 回归 | null/nocomp、aes、3des、可选 FEC |
| 性能 | L2/L3 profiling 可选；主看计数 |

---

## 7. Phase 3 — Client 改造

### 7.1 CFB：单缓冲 in-place

**现状问题：** `buf` → `dec_scratch` copy → `decrypt_cfb` → 再 copy 到 `CryptoBuf.enc_buf` → `Bytes`。

**目标：**

```text
选项 1（首选）: 直接 crypt.decrypt(&mut buf[..n]); CRC; body=&buf[20..n]
选项 2: 若必须保留「recv buf 不被改」：只保留 dec_scratch 一次 copy，
        但 decrypt 后 body=&dec_scratch[20..]，禁止再进 enc_buf
```

因 client 同 task 内 **先处理完再 recv**，**选项 1** 安全且少一次 copy。

**锁：** 入站不要 `crypto_buf.lock()` 只为 decrypt；nonce 计数只用于出站。

### 7.2 null：去掉 `Bytes::copy_from_slice`

```text
旧: let data = Bytes::copy_from_slice(&buf[..n]); input(&data)
新: input(&buf[..n])  // 或 FEC 子切片
```

仅当后续要把报文 **跨 await 持有** 才需要 `Bytes`；当前 KCP input 同步 → **不需要**。

### 7.3 FEC 分支

保持现有 `input(&input[FEC_HDR..])` 模式；确保 `input` 来自 in-place body，而不是先 `Bytes` 再切。

### 7.4 Client 验收

| 检查 | 期望 |
|------|------|
| S-cfb 计数 | payload 二次 copy = 0；full copy ≤ 0（选项 1） |
| S-null | null_copies/pkt = 0 |
| e2e | 与 Go server/client 矩阵通过 |

---

## 8. Phase 4 — FEC 少 `to_vec`（server 重点）

在 Phase 2 的 `body: &[u8]` 上：

| 旧 | 新 |
|----|-----|
| `kcp_inputs.push(feed_slice[FEC_HDR..].to_vec())` | `kcp.input(&body[FEC_HDR..])` 直接调，或同步喂完的 `Vec<&[u8]>` |
| recovered 的 `r[FEC_HDR..].to_vec()` | 若 `decode` 返回 owned `Vec`，可 `input(&r[FEC_HDR..])` **不**再 clone；若 decoder 复用内部缓冲，必须在 `input` 前拷贝或改 decoder 生命周期文档 |

**风险：** `FecDecoder::decode` 若返回指向内部 ring 的引用，**不能**在二次 `decode` 后仍用旧引用 → 实施前 **读 `fec.rs` 所有权**，必要时 recovered **允许一次** owned（恢复路径稀有）。

**验收：** S-cfb-fec 下 data 片路径无 `to_vec`；recovered 有注释说明为何仍 alloc。

---

## 9. Phase 5 — AEAD（可选延伸）

| 现状 | 目标 |
|------|------|
| `open` → `Vec<u8>` | `open_into(&mut scratch) -> Result<&[u8]>` 或写进 `BytesMut` |

优先级低于 CFB/null；可单独立项。

---

## 10. 风险与回滚

| 风险 | 缓解 |
|------|------|
| 生命周期：input 后误用已覆写 buf | 规范：input 同步完成；禁止把 `body` 存进 session 字段 |
| client/server header 探测不一致 → 与 Go 不互通 | 共享 strip 函数 + e2e 全 crypt 矩阵 |
| in-place decrypt 弄脏 batch 槽未 clear | 还槽前 `clear`/`truncate(0)`；容量保留 |
| 入站抢 `CryptoBuf` 锁拖慢出站 | 入站 **不锁** 出站 `CryptoBuf` |
| FEC recovered 悬垂引用 | 见 Phase 4；宁可 recovered 一次 alloc |
| 探针 feature 残留生产 | `cfg(feature)` / `debug_assertions` 默认关 |
| thr 无提升被误判失败 | 成功标准以 **alloc 计数 + 正确性** 为主，thr 为辅 |

**回滚：** 每 Phase 独立 commit；可选 feature flag `inbound_in_place`。

---

## 11. 验收总表

| 阶段 | 正确性 | 资源 | 性能（辅） |
|------|--------|------|------------|
| 0 | 探针自身自检 | 两次基线稳定 | — |
| 1 | kcp-rs unit | — | — |
| 2 | e2e + stress | S-cfb full_copies/pkt≈0 | 可选 L2 |
| 3 | e2e client 侧 | S-null null_copies=0；CFB 无双拷 | 可选 L2 |
| 4 | e2e+FEC | data 片无多余 to_vec | — |
| 5 | e2e aes-gcm | open 无每包 Vec（若做） | — |

**总成功判据（立项关闭）：**

1. S-cfb：server+client 入站 **每包整包 `to_vec` = 0**。  
2. S-null：server+client **无**仅为进 KCP 的 `to_vec`/`Bytes::copy_from_slice`。  
3. `make e2e` + `make stress` 通过。  
4. 与 Phase 0 基线对比，计数项有文档记录。  
5. **不**要求 3des thr 超过现 KPI 门槛才合并。

---

## 12. 文件触点清单（实施时）

| 文件 | 变更类型 |
|------|----------|
| `kcp-rs/src/crypto_buf.rs`（或新 `inbound.rs`） | `decrypt_cfb_in_place`、错误类型、测试 |
| `kcp-rs/src/lib.rs` | re-export |
| `kcptun-server/src/main.rs` | `feed_data_mut`、recv/batch 下传 `&mut [u8]`、去 `to_vec`/`drain` |
| `kcptun-client/src/main.rs` | UDP 环 in-place CFB/null |
| `kcp-rs` 或 server 旁路 | 可选 `inbound_alloc_trace` 计数 |
| `bench/` 或 profile 笔记 | Phase 0 基线数字（可选） |
| `kcrypt-rs` | **原则上不动** |
| AGENTS | 仅当公开 API 稳定后轻量补 Key Files；纯内部 helper 可 no sync |

---

## 13. 建议排期（工程量级）

| 阶段 | 量级 | 依赖 |
|------|------|------|
| Phase 0 | 0.5–1d | 无 |
| Phase 1 | 0.5–1d | 0 的计数挂钩点 |
| Phase 2 | 1–2d | 1 |
| Phase 3 | 0.5–1d | 1 |
| Phase 4 | 0.5–1d | 2 |
| Phase 5 | 0.5d | 可选 |

顺序：**0 → 1 → 2 → 3 → 4**；3 与 2 在 1 完成后可并行开发，e2e 建议串行合入。

---

## 14. 决策摘要

1. **瓶颈不在算法，在 server 入站 `to_vec` + client 双拷/null 拥有化。**  
2. **`KCP::input(&[u8])` 已支持真·借用**；关键是让 **recv 缓冲成为 decrypt 的唯一 mut 所有者**，并在同步 `input` 后结束借用。  
3. **先 Phase 0 计数，再改**，避免 thr 噪声决策。  
4. **共享 in-place 原语**，消灭 client/server 语义分叉。  
5. **入站与出站 `CryptoBuf` 解耦**，避免锁与二次 copy。  
6. **FEC recovered / AEAD** 允许保留少量 alloc；不阻塞 CFB/null 主收益。  
7. **成功 = 计数归零类指标 + e2e/stress**，不是再抠 Feistel。

---

## 15. Implementation notes（后续开工时）

- [x] Phase 1: `decrypt_cfb_in_place` / `strip_cfb_header_if_present` / `inbound_null` in `kcp-rs` (2026-07-22)
- [x] Phase 2: server `feed_data_mut` + inline SMUX; no CFB/null `to_vec`/`drain`
- [x] Phase 3: client in-place CFB on recv buf; null slice; no inbound `CryptoBuf` lock
- [x] Phase 4: FEC data/recovered paths `kcp.input(&[u8])` without intermediate copies
- [ ] Phase 0 alloc counters / Phase 5 AEAD `open_into` still optional follow-ups

**AGENTS sync:** `kcp-rs` re-exports new inbound helpers — update `kcp-rs/AGENTS.md` Key Files / public re-exports if not already.
