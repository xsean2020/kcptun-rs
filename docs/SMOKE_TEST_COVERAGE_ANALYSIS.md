# kcptun-rs 冒烟测试场景覆盖完整性分析报告

**分析日期**: 2026-07-26
**分析对象**:
- `smoke_test_rust_rust.sh` (Rust↔Rust 冒烟测试)
- `test_e2e.sh` (Go↔Rust 互操作测试)
- `kcptun-server/tests/stress_test.rs` (压力测试)
- `kcptun-server/tests/reconnect_test.rs` (重连测试)
- 各 crate 内嵌单元测试

---

## 一、测试基础设施概览

| 测试套件 | 行数 | 用途 | 调用方式 |
|---------|------|------|---------|
| `smoke_test_rust_rust.sh` | 1771 | Rust(tokio/smol)↔Rust 功能+稳定性 | 手动 `bash smoke_test_rust_rust.sh` |
| `test_e2e.sh` | 272 | Go↔Rust 线缆兼容性矩阵 | `make e2e` 或手动 |
| `stress_test.rs` | 683 | 多线程并发数据完整性 | `make stress` (需 release 构建) |
| `reconnect_test.rs` | 430 | 多连接 + 服务端重启重连 | 仅 `cargo test --release -p kcptun-server --test reconnect_test` |
| `kio-rs/src/tests.rs` | 420 | 运行时抽象层 (tokio/smol) | `cargo test` |
| 各 crate 内嵌测试 | ~31 个含 `#[cfg(test)]` 的文件 | 单元测试 | `cargo test --workspace` |

**关键观察**: `reconnect_test` **仅能手动调用**，未集成到任何自动化流程中。

---

## 二、冒烟测试覆盖的场景矩阵

### 2.1 smoke_test_rust_rust.sh 的 22 个 Section（共 44 个小节）

| Section | 覆盖内容 | 测试强度 |
|---------|---------|---------|
| 1. 基础连通性 | 1B/64B/1KB/8KB/64KB/512KB | 每尺寸 1 次 |
| 2. 全加密算法 | 15 种 cipher + `--nocomp` | 256B |
| 3. KCP 模式 | normal/fast/fast2/fast3 + aes + nocomp | 4KB |
| 4. SMUX 版本 | v1/v2 + aes + nocomp | 4KB |
| 5. Snappy 压缩 | on/off + 多 cipher + 多尺寸 | 4KB/8KB/64KB |
| 6. FEC 前向纠错 | 10/3, 4/2, 15/5 + aes + nocomp | 4KB |
| 7. 窗口 + 多 conn | 小窗/大窗/--conn 4 | 4KB/64KB |
| 8. 并发稳定性 | 10/50/30/20 并发连接 | 1KB~128KB |
| 9. 连接抖动 | 50/100 次建断连 | 256B/1KB/2KB |
| 10. 保活 + 空闲 | 30s/60s 保活 | - |
| 11. 组合极限 | aes-fast3-压缩-FEC, sm4-normal-FEC, salsa20-fast3-nocomp-大窗 | 16KB~128KB |
| 12. SMUX v1 + 压缩 | v1+aes-压缩, v1+sm4-nocomp | 4KB/16KB |
| 13. 代理场景模拟 | 50并发×32KB (单 KCP), 抖动80×4KB-aes-压缩 | 32KB/4KB |
| 14. 多线程完整性（核心） | 20/50 线程 × 4 尺寸 (1B/4KB/64KB/128KB) | 逐字节校验 |
| 15. 单连接多轮 | 20/30 轮 × 混合尺寸 (1B~128KB) | 逐字节校验 |
| 16. QPP 量子置换 | --QPP --QPPCount 61 + 多 cipher/尺寸 | 4KB/16KB/64KB |
| 17. 大数据传输 | 1MB/2MB + FEC | 1MB/2MB |
| 18. 全双工双向 | 1MB/512KB 双向同时 | 逐字节校验 |
| 19. --nc 1 无拥塞 | nc1 + 大窗 + 压缩 | 128KB/512KB/1MB |
| 20. 内存增长监控 | 200 次抖动 + RSS 阈值检查 | 对应 BUGREPORT_PROXY_MEMORY_GROWTH |
| 21. Wave 波次并发 | 3波 80连接 (10×8KB + 20×32KB + 50×混合) | 模拟浏览器加载 |
| 22. 半关闭 + 不可压缩 | FIN + urandom + Snappy | 64KB/128KB |

### 2.2 加密算法覆盖 (15 种)

```
null, none, xor, aes-128, aes-192, aes, sm4, tea, xtea,
salsa20, blowfish, twofish, cast5, 3des, aes-128-gcm
```

**注意**: 所有 cipher 都在 Section 2 用 `--nocomp` 测试过；部分在 Section 5/11/13/14/16/22 搭配压缩/FEC/QPP 测试。

### 2.3 KCP 模式覆盖 (4 种)

```
normal, fast, fast2, fast3
```

### 2.4 特性开关覆盖情况

| 特性 | 默认值 | 冒烟测试是否覆盖 | 备注 |
|-----|-------|-----------------|------|
| `--crypt` | aes | ✅ 全矩阵 | 15 种 |
| `--mode` | fast | ✅ 4 种 | normal/fast/fast2/fast3 |
| `--nocomp` | false (压缩开启) | ✅ on/off | Section 1/2/5/8/9/13/14/15/17/18/19/20/21/22 |
| `--datashard/--parityshard` | 10/3 | ✅ 0/0 + 4/2 + 10/3 + 15/5 | Section 6/11/14/19/20/21/22 |
| `--QPP/--QPPCount` | off | ✅ --QPP 61 | Section 16 |
| `--conn` | 1 | ✅ 1/4 | Section 7/8/13/14/20/21 |
| `--sndwnd/--rcvwnd` | 1024 | ✅ 32/32 ~ 1024/1024 | Section 7/17/18/19 |
| `--nc` | 0 | ✅ --nc 1 | Section 19 |
| `--keepalive` | 10 | ✅ 10/30 | Section 10 |
| `--smuxver` | 2 | ✅ 1/2 | Section 4/12 |
| 内存泄漏检测 (RSS) | - | ✅ 200 次抖动 | Section 20 |
| 半关闭 (FIN) | - | ✅ | Section 22 |
| 不可压缩随机数据 | - | ✅ urandom | Section 22 |
| 全双工 | - | ✅ | Section 18 |
| Wave 波次 | - | ✅ 80 连接 3 波 | Section 21 |
| 多线程完整性 | - | ✅ 50 线程 × 4 尺寸 | Section 14 |

---

## 三、关键覆盖缺口

### 3.1 严重缺口（可能导致生产问题未被发现）

| 缺口 | 现状 | 风险 | 建议 |
|-----|------|------|------|
| **FEC 丢包恢复未验证** | FEC 只在无损网络上跑 `--datashard N --parityshard M`，**从未主动丢包** | FEC 恢复路径可能有 bug | 引入 `tc netem loss` 或 Python 丢包代理 |
| **服务端重启重连未自动化** | `reconnect_test` 仅手动运行 | BUGREPORT_NO_RECONNECT_ON_SERVER_RESTART 这种问题可能复发 | 将 reconnect 场景纳入 smoke |
| **stress tests 当前全部失败** | 8/8 failed，模式为 "recv 0 bytes" / 短读 | 说明高并发波次下存在真实 bug 或时序问题 | 修复 stress_test 后再信任其结果 |
| **`--tcp` 传输模式未测试** | 代码支持 UDP/TCP 两种底层传输 | TCP 路径可能 bitrot | 至少 smoke 里加 1-2 个 `--tcp` case |
| **client 侧连接管理参数未测试** | `--autoexpire`, `--scavengettl`, `--conn N` 的自动过期行为 | 连接池泄漏/不回收 | 增加 churn + autoexpire 组合测试 |

### 3.2 中等缺口（功能未覆盖但不阻塞主路径）

| 缺口 | 现状 | 影响 |
|-----|------|------|
| `--ratelimit` | 完全未测试 | 限速功能可能无效 |
| `--dscp` | 完全未测试 | QoS 标记可能不生效 |
| `--sockbuf` | 只设置默认值，未变动 | 大 buffer / 小 buffer 行为未知 |
| `--smuxbuf` / `--streambuf` / `--framesize` | 未测试 | 极端 buffer 可能导致死锁或丢帧 |
| `--mtu` 显式设置 | 未测试 | 路径 MTU 变化可能触发问题 |
| `--acknodelay` | 未测试 | ACK 延迟行为未验证 |
| `--nodelay` / `--interval` / `--resend` 显式（非 mode） | 未测试 | 细粒度调优未覆盖 |
| `--closewait` | 未测试 | 半关闭等待超时未验证 |
| `--snmplog` / `--snmpperiod` | 未测试 | SNMP 收集路径未跑 |
| `--quiet` / `--log` | 未测试 | 日志路径未验证 |
| 配置文件 `-c` 合并 | 未测试 | 配置优先级可能错 |
| `--pprof` | 未测试 | 仅性能分析，不影响功能 |
| 跨运行时 (tokio ↔ smol) | smoke 只测同构对 (tokio↔tokio, smol↔smol) | 混合部署可能有问题 |
| 混合 Go↔Rust | 仅在 `test_e2e.sh` 中，不在 smoke 中 | smoke 场景下无法发现互操作回归 |

### 3.3 低优先级/边缘场景

| 缺口 | 说明 |
|-----|------|
| 恶意/畸形输入 | 无截断包、错 CRC、非法 KCP cmd、SMUX 协议违规测试 |
| 资源耗尽 | 文件描述符、内存、CPU 打满下的行为 |
| 长时间运行 | 数小时/数天的稳定性（当前最长 ~3 分钟的 keepalive） |
| 网络条件模拟 | 延迟、抖动、带宽限制、双向丢包 |
| 密码/密钥边界 | 空密码、超长密码、特殊字符（PBKDF2 路径） |
| 并发连接数上限 | 1000+ 连接的系统资源压力 |

---

## 四、当前执行状态（关键发现）

### 4.1 stress_test.rs 运行结果（2026-07-26 实测）

```
test result: FAILED. 0 passed; 8 failed; 0 ignored

失败模式示例:
  thread 9: [conn 9 / 8KB] MISMATCH: sent 8192 bytes, recv 0 bytes
  thread 33: [conn 33 / 128KB] MISMATCH: sent 131072 bytes, recv 108996 bytes
  thread 38: [conn 38 / 128KB] MISMATCH: sent 131072 bytes, recv 60000 bytes
```

**影响**: 波次并发（Wave）和高并发场景下，存在**短读 / 零读 / 部分数据**问题。这类问题 smoke 脚本的 Wave 部分（Section 21）和多线程完整性部分（Section 14）**理论上也会暴露**。

### 4.2 smoke 脚本自身是否能捕获同类问题？

- smoke 的 `try_wave_concurrency` 和 `try_multithread_integrity` 使用**逐字节校验** + 确定性 payload 模式
- 如果底层出现 "recv 0" 或短读，**smoke 会失败**并报告具体哪个 conn/round
- 因此：**smoke 脚本的设计是能发现这类问题的**，只是当前 stress tests 先暴露了

---

## 五、与 Go 兼容性相关的未覆盖点

从 `CLAUDE.md` 和代码注释提取的**必须与 Go 行为一致**的点：

| 要求 | 是否在冒烟中验证 |
|-----|-----------------|
| Wire 格式 (KCP segment 24B, SMUX 8B, CFB `[nonce+CRC]`) | ✅ 间接通过所有测试 |
| CFB 固定 IV `GO_CFB_IV` | ✅ 所有 cipher 测试 |
| TEA 8 rounds | ✅ tea/xtea cipher 测试 |
| Snappy session-level | ✅ 所有压缩测试 |
| FEC session-layer (非 KCP 层) | ✅ FEC sections |
| 默认 FEC 10/3 | ✅ Section 6 |
| `--nocomp` 默认关闭（压缩开启） | ✅ 多处 |
| PBKDF2 salt `b"kcp-go"` | ✅ 所有测试（derive_key） |
| SMUX keepalive NOP | ⚠️ 部分（keepalive 测试存在，但未验证 NOP 帧格式） |
| 死链检测 + 重连 | ❌ **完全缺失**（reconnect_test 未自动化） |
| SNMP 字段 Go 兼容 | ⚠️ 未验证（snmplog 未跑） |

---

## 六、建议的改进优先级

### P0（立即）
1. **将 `reconnect_test` 集成到 smoke**（或至少加入一个简化版的服务端 kill + 重连场景）
2. **修复 stress_test 当前失败**，或在 smoke 中增加等效的波次压力测试并标记为必过
3. **增加 FEC 丢包场景**（用 `tc netem loss 5%` 或 Python 中间人丢包）

### P1（本迭代）
4. 增加 `--tcp` 至少 1-2 个 case
5. 增加 `--conn N` + `--autoexpire` 组合抖动测试
6. 增加 `--ratelimit` 基础验证（至少不崩溃 + 限速生效的粗粒度检查）
7. 增加跨运行时 (tokio server ↔ smol client) 至少一个 case

### P2（后续）
8. `--dscp`, `--sockbuf`, `--smuxbuf`, `--closewait`, `--acknodelay` 等参数的冒烟
9. 配置文件 `-c` 合并测试
10. 长时间运行（1 小时+）稳定性
11. 恶意输入 / 协议鲁棒性测试

---

## 七、总结

| 维度 | 评价 |
|-----|------|
| **功能路径覆盖** | 优秀（15 cipher × 4 mode × 压缩/FEC/QPP 组合已覆盖大部分） |
| **并发/压力覆盖** | 良好（100 并发、200 抖动、80 波次、1MB+ 大包、50 线程多轮） |
| **协议互操作** | 良好（e2e.sh 覆盖 Go↔Rust 矩阵；smoke 专注 Rust↔Rust） |
| **边界/异常** | **薄弱**（无丢包、无重启重连、无畸形包、无资源耗尽） |
| **自动化完整性** | **不足**（reconnect_test 未自动化，stress_test 当前失败） |
| **参数矩阵完整性** | **中等偏弱**（核心参数覆盖好，高级调优参数几乎未碰） |

**总体结论**:
- smoke 脚本在**正常功能 + 稳定性压力**维度已经相当全面
- **最危险的缺口**是：FEC 实际丢包恢复、服务端重启重连、以及当前 stress tests 暴露的并发短读问题
- 建议优先补齐 P0 项，其次将 smoke 作为 CI gate（目前似乎只有手动运行）

---

**附：未在 smoke 中出现的 CLI 参数清单（供参考）**

```
--ratelimit, --dscp, --sockbuf, --smuxbuf, --streambuf, --framesize,
--nodelay/--interval/--resend (显式), --acknodelay, --closewait,
--snmplog/--snmpperiod, --quiet, --log, --tcp, --c (config),
--autoexpire, --scavengettl, --pprof (仅性能)
```
