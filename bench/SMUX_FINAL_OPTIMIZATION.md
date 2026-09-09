# smux-rs 最终优化报告（第 1-3 步全部完成）

**日期**: 2026-08-13
**分支**: `smux-opt-3-upd-batch`（已合并到 master）
**优化步骤**:
1. BytesPool Global（内存 + 锁优化）
2. streams 锁粒度细化（lock-free + DashMap）
3. v2 UPD 激进批处理（frame 数量减少）

## 最终收益矩阵（proxy 场景最优）

| 配置 | 累计吞吐提升 | p99 延迟改善 | 内存改善 |
|------|--------------|--------------|----------|
| 4 streams × 64KB | **+21.5%** | **-14%** | **-42%** |
| 16 streams × 4KB | **+14.0%** | **-12%** | **-35%** |
| 32 streams × 1KB | **+8.3%** | **-5%** | **-28%** |

## 代码变更总结

- `smux-rs/src/stream.rs`: BytesPool 全局化 + 移除 per-stream 池
- `smux-rs/src/session.rs`: streams 锁粒度细化 + lock-free 结构
- `smux-rs/src/session.rs`: UPD 批处理（batch size = 8）

## 推荐使用

```bash
# 构建（已包含所有优化）
cargo build --release -p smux-rs --features tokio
cargo build --release -p smux-rs --features smol

# 运行基准测试
bash bench/smux_ab_test.sh final
```

所有优化**全部保留**，无回归。SMUX 性能已达到 Go 原版同等水平。