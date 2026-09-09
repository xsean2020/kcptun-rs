# kcp-rs ↔ kcp-go v5 — P99 / P999 延迟交叉测试报告（开放模型）

- 日期: 2026-09-09 14:54
- 环境: macOS 26.3.1 / arm64
- Rust: 1.92.0-nightly / kcp-rs "0.2.7"
- Go: go1.25.5 / kcp-go v5.6.64
- 方法: **有界开放模型固定速率**回声 RTT（不经 kcptun / SMUX / snappy / 加密层），127.0.0.1 UDP
- 画像: profile=game, payload=512 B
- 配置: Fast3 (nodelay=1, interval=10, resend=2, nc=1), MTU 1350, 窗口 512/512, stream=true, acknodelay=true, 无 FEC, 无加密
- **构建: kcp-rs 用 release（opt-level=3 + LTO）；kcp-go 默认优化构建**。
- 结构: 四个开放模型组合均为独立的「客户端进程计时 + 服务端进程回声」（kcp-rs=KcpListener+KcpStream, kcp-go=ListenWithOptions+NewConn3）
- Rust 读取策略: client_busy_yields=512, server_busy_yields=0, reader_poll_us=0
- 调度口径: Go reader 固定使用 100µs ReadDeadline；Rust reader 使用上述配置。两者是各自 runtime 的原生唤醒路径，并非相同 polling 实现，因此结果同时包含 probe/runtime 调度开销。
- 参数: target_rps=500, warmup=5s (排除), duration=60s, workers=0 (0=runtime 默认), rust_shards=1
- **OS 调优**: not applied

## 结果（微秒 µs，越小越好；P50/P90/P99/P999 由全部原始样本一次性聚合计算）

| 组合 | offered | ok | shed | inflight end | completed RPS | P50 | P90 | P99 | P999 | avg | min | max |
|------|--------:|---:|-----:|-------------:|--------------:|----:|----:|----:|-----:|----:|----:|----:|
| kcp-rs(tokio)↔kcp-rs(tokio) | 30000 | 30000 | 0 | 0 | 500 | 137.0 | 235.3 | 501.1 | 6577.5 | 177.3 | 25.5 | 24204.5 |
| kcp-go↔kcp-go            | 30000 | 30000 | 0 | 0 | 500 | 106.0 | 131.0 | 199.0 | 621.0 | 108.4 | 43.0 | 5901.0 |
| kcp-rs(tokio)→kcp-go     | 30000 | 29909 | 91 | 0 | 498 | 159.5 | 271.3 | 665.8 | 3579.5 | 191.2 | 5.5 | 28836.5 |
| kcp-go→kcp-rs(tokio)     | 30000 | 30000 | 0 | 0 | 500 | 110.0 | 146.0 | 241.0 | 614.0 | 115.7 | 55.0 | 6512.0 |

## 最大可持续性能（闭环，concurrency=32）

| 组合 | completed req/s | P99 (µs) | P999 (µs) | max (µs) |
|------|----------------:|----------:|-----------:|---------:|
| kcp-rs(tokio)↔kcp-rs(tokio) | 163621 | 505.8 | 1271.0 | 116347.8 |
| kcp-go↔kcp-go | 80351 | 911.0 | 1662.0 | 29900.0 |

## 严格标准合规

- **有界开放模型**：sender 与 reader 独立；sender 按 target_rps=500 调度，落后超过 50ms 时显式计入 shed，避免无界追赶掩盖过载。
- **计时边界**：RTT 从 Write/write_all 成功接纳后开始；写窗口等待不计入已完成请求的 RTT，无法接纳的计划槽由 shed 单独暴露。因此验收必须同时看 offered、ok、shed 和 percentile。
- **无百分位二次平均**：每个组合的 P50/P90/P99/P999 均由测量期全部原始样本（约 30000 个）排序后一次性计算，绝不跨批次取均值。
- **独立预热**：前 5s 为预热阶段（连接建立/KCP 窗口/分配器），样本排除。
- **样本量**：目标 offered = target_rps × duration = 30000；实际值以表中 offered 为准，正式运行应要求 shed=0 且 inflight_end=0。
- **时长**：duration=60s（可配；正式报告建议延长到 10–30min，以覆盖周期性系统抖动）。
- **环境**：同机回环、四组合使用相同的双进程拓扑与负载；这隔离语言 runtime，但不模拟公网 RTT、抖动、丢包或路由排队。
- **性能口径**：固定 RPS 表只回答延迟；最大性能由 concurrency=32 的闭环测试回答，避免把 500 RPS 下的空闲延迟误当吞吐能力。

## 结论

- **互操作双向通过**：kcp-rs(tokio) ↔ kcp-go 在裸 KCP 层跨语言互通，回声逐位一致。
- **最大 raw KCP 吞吐**：kcp-rs(tokio) 163621 req/s，kcp-go 80351 req/s；Rust 相对 Go 为 103.6%（正数表示 Rust 更快）。
- 固定 RPS 的 P50/P99 是延迟与调度稳定性指标，不能用于推断实现的最大吞吐；上面的闭环结果才是本报告的 raw KCP 性能结论。

_本报告由 bench/run_p99.sh 生成（开放模型 4 组合矩阵 + 2 闭环基线）。_
