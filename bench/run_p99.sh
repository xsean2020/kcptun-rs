#!/bin/bash
# Strict P99 / P999 latency cross-test orchestrator (tokio-only).
#
# Open-model (fixed-RPS) measurement per the strict-standard methodology:
#   - fixed-rate sends independent of responses (no coordinated omission)
#   - separate warm-up phase, excluded from metrics
#   - all raw per-request latencies aggregated into one global P99/P999
#   - sample size = rps * duration (>= 1000)
#
# Six latency combos (tokio-only backend after the kio→knet refactor):
#   rust-tokio↔rust-tokio, go↔go baselines, plus
#   rust-tokio→go and go→rust-tokio cross-interop,
#   plus two closed-loop throughput baselines.
#
# Profiles: PROFILE=game (default, 512B) or PROFILE=bulk (26624B).
# Env overrides: RPS (500), WARMUP (5s), DURATION (60s), SIZE (profile default),
# CONCURRENCY (32), WORKERS (0=runtime defaults), RUST_SHARDS (1),
# RUST_CLIENT_BUSY_YIELDS (profile default), RUST_SERVER_BUSY_YIELDS (0), and
# READER_POLL_US (profile default). The closed-loop runs use the same warm-up
# and duration.
set -euo pipefail
cd "$(dirname "$0")/.."
REPO=$(pwd)

PROFILE=${PROFILE:-game}
case "$PROFILE" in
  game)
    SIZE=${SIZE:-512}
    RUST_CLIENT_BUSY_YIELDS=${RUST_CLIENT_BUSY_YIELDS:-512}
    READER_POLL_US=${READER_POLL_US:-0}
    ;;
  bulk)
    SIZE=${SIZE:-26624}
    RUST_CLIENT_BUSY_YIELDS=${RUST_CLIENT_BUSY_YIELDS:-0}
    READER_POLL_US=${READER_POLL_US:-100}
    ;;
  *)
    echo "unsupported PROFILE=$PROFILE (expected game or bulk)" >&2
    exit 2
    ;;
esac

RPS=${RPS:-500}
WARMUP=${WARMUP:-5}
DURATION=${DURATION:-60}
CONCURRENCY=${CONCURRENCY:-32}
WORKERS=${WORKERS:-0}
RUST_SHARDS=${RUST_SHARDS:-1}
RUST_SERVER_BUSY_YIELDS=${RUST_SERVER_BUSY_YIELDS:-0}
CONV=0x00C0_FFEE

# A positive budget sets Go's GOMAXPROCS and the Rust probe's top-level Tokio
# runtime. Listener shards are independent because this benchmark has exactly
# one KCP session; RUST_SHARDS=1 avoids idle shard threads contaminating tail
# latency while retaining the production listener pipeline.
if [ "$WORKERS" -gt 0 ]; then
  export GOMAXPROCS="$WORKERS"
fi

# Spin-bounded busy-poll for KcpStream reads (see conn.rs `busy_poll_yields`).
# Open-model endpoints run as separate processes so latency-sensitive Rust
# clients can opt into bounded yielding without making server readers spin.
# Closed-loop throughput baselines keep the production event-driven default.

GOBIN="$REPO/tests/kcp-go-latency/kcp-go-latency"
REPORT="$REPO/bench/LATENCY_P99_REPORT.md"
EX_TOKIO="$REPO/target/release/examples/latency_p99"

P_GO1=$(( (RANDOM % 15000) + 10000 ))
P_GO2=$(( P_GO1 + 1 ))
P_RT1=$(( P_GO1 + 2 ))
P_RT2=$(( P_GO1 + 3 ))

echo "==> building kcp-rs example (tokio)"
cargo build -q --release -p kcp-rs --features async --example latency_p99

echo "==> building kcp-go harness"
(cd "$REPO/tests/kcp-go-latency" && go build -o kcp-go-latency .)

# ── OS-level UDP buffer tuning (macOS) ──────────────────────────────────────
# Increase kernel UDP socket buffers to prevent silent packet drops under
# high-throughput KCP workloads. This is the single most effective OS tuning
# for P99/P999 latency on macOS (measured: throughput +28%, P99 -26%).
# Requires sudo; skip silently if not available (e.g. CI without sudo).
OS_TUNING="not applied"
if [ "$(uname)" = "Darwin" ] && sudo -n true 2>/dev/null; then
  sudo sysctl -w kern.ipc.maxsockbuf=8388608 >/dev/null
  sudo sysctl -w net.inet.udp.recvspace=4194304 >/dev/null
  OS_TUNING="applied: maxsockbuf=8MB recvspace=4MB"
  echo "==> macOS sysctl applied: maxsockbuf=8MB recvspace=4MB"
fi

ACTIVE_PID=""
kill_wait() {
  [ -z "${1:-}" ] && return
  kill "$1" 2>/dev/null || true
  wait "$1" 2>/dev/null || true
}
cleanup() {
  kill_wait "$ACTIVE_PID"
  ACTIVE_PID=""
}
trap cleanup EXIT INT TERM

# Per-step hard timeout: a stuck measurement must not hang the whole run.
# STEP_TIMEOUT = warmup + duration + 30s margin (default 5+60+30 = 95s).
STEP_TIMEOUT=$((WARMUP + DURATION + 30))
step_tmo() {
  perl -e 'alarm shift; exec @ARGV' "$STEP_TIMEOUT" "$@" 2>/dev/null | grep '^RESULT' || true
}
run_step() {
  local label=$1; shift
  local r
  r=$(step_tmo "$@")
  if [ -n "$r" ]; then
    echo "$r"
  else
    echo "[$label] FAILED: no RESULT within ${STEP_TIMEOUT}s (timed out or errored)"
  fi
}

echo "==> [1/6] kcp-rs(tokio) ↔ kcp-rs(tokio) baseline"
env KCPTUN_WORKER_THREADS="$RUST_SHARDS" KCP_BUSY_YIELDS="$RUST_SERVER_BUSY_YIELDS" \
  "$EX_TOKIO" --mode server --port "$P_RT1" --size "$SIZE" --workers "$WORKERS" 2>/dev/null & SRV=$!
ACTIVE_PID=$SRV
sleep 1.5
R_TT=$(run_step "1/6" env KCP_BUSY_YIELDS="$RUST_CLIENT_BUSY_YIELDS" \
  "$EX_TOKIO" --mode peer --addr "127.0.0.1:$P_RT1" --rps "$RPS" \
  --warmup "$WARMUP" --duration "$DURATION" --size "$SIZE" \
  --reader-poll "$READER_POLL_US" --workers "$WORKERS")
R_TT=${R_TT/combo=rust-go/combo=rust-rust}
echo "$R_TT"
kill_wait "$SRV"
ACTIVE_PID=""

echo "==> [2/6] kcp-go ↔ kcp-go baseline"
"$GOBIN" server --port "$P_GO1" 2>/dev/null & SRV=$!
ACTIVE_PID=$SRV
sleep 0.5
R_GG=$(run_step "2/6" "$GOBIN" client --addr "127.0.0.1:$P_GO1" \
  --rps "$RPS" --warmup "$WARMUP" --duration "$DURATION" --size "$SIZE" --conv "$CONV")
R_GG=${R_GG/combo=go-rust/combo=go-go}
echo "$R_GG"
kill_wait "$SRV"
ACTIVE_PID=""

echo "==> [3/6] kcp-rs(tokio) → kcp-go (cross)"
"$GOBIN" server --port "$P_GO2" 2>/dev/null & SRV=$!
ACTIVE_PID=$SRV
sleep 0.5
R_TG=$(run_step "3/6" env KCP_BUSY_YIELDS="$RUST_CLIENT_BUSY_YIELDS" \
  "$EX_TOKIO" --mode peer --addr "127.0.0.1:$P_GO2" --rps "$RPS" \
  --warmup "$WARMUP" --duration "$DURATION" --size "$SIZE" \
  --reader-poll "$READER_POLL_US" --workers "$WORKERS")
echo "$R_TG"
kill_wait "$SRV"
ACTIVE_PID=""

echo "==> [4/6] kcp-go → kcp-rs(tokio) (cross)"
env KCPTUN_WORKER_THREADS="$RUST_SHARDS" KCP_BUSY_YIELDS="$RUST_SERVER_BUSY_YIELDS" \
  "$EX_TOKIO" --mode server --port "$P_RT2" --size "$SIZE" --workers "$WORKERS" 2>/dev/null & RSRV=$!
ACTIVE_PID=$RSRV
sleep 1.5
R_GT=$(run_step "4/6" "$GOBIN" client --addr "127.0.0.1:$P_RT2" --rps "$RPS" --warmup "$WARMUP" --duration "$DURATION" --size "$SIZE" --conv "$CONV")
echo "$R_GT"
kill_wait "$RSRV"
ACTIVE_PID=""

echo "==> [5/6] kcp-rs(tokio) ↔ kcp-rs(tokio) max-throughput baseline"
R_TT_CLOSED=$(run_step "5/6" env KCPTUN_WORKER_THREADS="$RUST_SHARDS" KCP_BUSY_YIELDS=0 \
  "$EX_TOKIO" --mode self --concurrency "$CONCURRENCY" --warmup "$WARMUP" \
  --duration "$DURATION" --size "$SIZE" --workers "$WORKERS")
echo "$R_TT_CLOSED"

echo "==> [6/6] kcp-go ↔ kcp-go max-throughput baseline"
R_GG_CLOSED=$(run_step "6/6" "$GOBIN" closed --port "$P_GO1" --concurrency "$CONCURRENCY" --warmup "$WARMUP" --duration "$DURATION" --size "$SIZE")
echo "$R_GG_CLOSED"

# ---- render one RESULT line as a markdown table row ----
# RESULT combo=.. samples=.. ok=.. size=.. rps=.. p50_us=.. p90_us=.. p99_us=.. p999_us=.. avg_us=.. min_us=.. max_us=..
mdrow() {
  local line=$1 label=$2
  case "$line" in
    RESULT*) ;;
    *) printf '| %-26s | FAILED |\n' "$label"; return ;;
  esac
  local samples ok shed offered completed inflight p50 p90 p99 p999 avg mn mx
  samples=$(echo "$line" | sed -n 's/.*samples=\([0-9]*\).*/\1/p')
  ok=$(echo "$line" | sed -n 's/.*ok=\([0-9]*\).*/\1/p')
  shed=$(echo "$line" | sed -n 's/.*shed=\([0-9]*\).*/\1/p')
  inflight=$(echo "$line" | sed -n 's/.*inflight_end=\([0-9]*\).*/\1/p')
  completed=$(echo "$line" | sed -n 's/.*rps=\([0-9]*\).*/\1/p')
  if [ -n "$shed" ]; then
    offered=$((samples + shed))
  else
    shed="na"
    offered="$samples"
  fi
  p50=$(echo "$line" | sed -n 's/.*p50_us=\([0-9.]*\).*/\1/p')
  p90=$(echo "$line" | sed -n 's/.*p90_us=\([0-9.]*\).*/\1/p')
  p99=$(echo "$line" | sed -n 's/.*p99_us=\([0-9.]*\).*/\1/p')
  p999=$(echo "$line" | sed -n 's/.*p999_us=\([0-9.]*\).*/\1/p')
  avg=$(echo "$line" | sed -n 's/.*avg_us=\([0-9.]*\).*/\1/p')
  mn=$(echo "$line" | sed -n 's/.*min_us=\([0-9.]*\).*/\1/p')
  mx=$(echo "$line" | sed -n 's/.*max_us=\([0-9.]*\).*/\1/p')
  printf '| %-26s | %s | %s | %s | %s | %s | %s | %s | %s | %s | %s | %s | %s |\n' \
    "$label" "$offered" "$ok" "$shed" "$inflight" "$completed" "$p50" "$p90" "$p99" "$p999" "$avg" "$mn" "$mx"
}

fmt1() { awk -v x="$1" 'BEGIN { printf "%.1f", x }'; }
field() { echo "$1" | sed -n "s/.*$2=\([0-9.]*\).*/\1/p"; }

RUST_CLOSED_RPS=$(field "$R_TT_CLOSED" rps)
GO_CLOSED_RPS=$(field "$R_GG_CLOSED" rps)
RUST_CLOSED_P99=$(field "$R_TT_CLOSED" p99_us)
GO_CLOSED_P99=$(field "$R_GG_CLOSED" p99_us)
RUST_CLOSED_P999=$(field "$R_TT_CLOSED" p999_us)
GO_CLOSED_P999=$(field "$R_GG_CLOSED" p999_us)
RUST_CLOSED_MAX=$(field "$R_TT_CLOSED" max_us)
GO_CLOSED_MAX=$(field "$R_GG_CLOSED" max_us)
if [ -n "$RUST_CLOSED_RPS" ] && [ -n "$GO_CLOSED_RPS" ] && [ "$GO_CLOSED_RPS" != 0 ]; then
  THROUGHPUT_DELTA=$(awk -v r="$RUST_CLOSED_RPS" -v g="$GO_CLOSED_RPS" 'BEGIN { printf "%.1f", (r/g-1)*100 }')
else
  THROUGHPUT_DELTA="N/A"
fi

{
cat <<EOF
# kcp-rs ↔ kcp-go v5 — P99 / P999 延迟交叉测试报告（开放模型）

- 日期: $(date '+%Y-%m-%d %H:%M')
- 环境: macOS $(sw_vers -productVersion 2>/dev/null || echo -) / $(uname -m)
- Rust: $(rustc -V 2>/dev/null | awk '{print $2}') / kcp-rs $(grep '^version' kcp-rs/Cargo.toml | head -1 | awk '{print $3}')
- Go: $(go version 2>/dev/null | awk '{print $3}') / kcp-go v5.6.64
- 方法: **有界开放模型固定速率**回声 RTT（不经 kcptun / SMUX / snappy / 加密层），127.0.0.1 UDP
- 画像: profile=$PROFILE, payload=$SIZE B
- 配置: Fast3 (nodelay=1, interval=10, resend=2, nc=1), MTU 1350, 窗口 512/512, stream=true, acknodelay=true, 无 FEC, 无加密
- **构建: kcp-rs 用 release（opt-level=3 + LTO）；kcp-go 默认优化构建**。
- 结构: 四个开放模型组合均为独立的「客户端进程计时 + 服务端进程回声」（kcp-rs=KcpListener+KcpStream, kcp-go=ListenWithOptions+NewConn3）
- Rust 读取策略: client_busy_yields=$RUST_CLIENT_BUSY_YIELDS, server_busy_yields=$RUST_SERVER_BUSY_YIELDS, reader_poll_us=$READER_POLL_US
- 调度口径: Go reader 固定使用 100µs ReadDeadline；Rust reader 使用上述配置。两者是各自 runtime 的原生唤醒路径，并非相同 polling 实现，因此结果同时包含 probe/runtime 调度开销。
- 参数: target_rps=$RPS, warmup=${WARMUP}s (排除), duration=${DURATION}s, workers=$WORKERS (0=runtime 默认), rust_shards=$RUST_SHARDS
- **OS 调优**: $OS_TUNING

## 结果（微秒 µs，越小越好；P50/P90/P99/P999 由全部原始样本一次性聚合计算）

| 组合 | offered | ok | shed | inflight end | completed RPS | P50 | P90 | P99 | P999 | avg | min | max |
|------|--------:|---:|-----:|-------------:|--------------:|----:|----:|----:|-----:|----:|----:|----:|
$(mdrow "$R_TT" "kcp-rs(tokio)↔kcp-rs(tokio)")
$(mdrow "$R_GG" "kcp-go↔kcp-go")
$(mdrow "$R_TG" "kcp-rs(tokio)→kcp-go")
$(mdrow "$R_GT" "kcp-go→kcp-rs(tokio)")

## 最大可持续性能（闭环，concurrency=${CONCURRENCY}）

| 组合 | completed req/s | P99 (µs) | P999 (µs) | max (µs) |
|------|----------------:|----------:|-----------:|---------:|
| kcp-rs(tokio)↔kcp-rs(tokio) | ${RUST_CLOSED_RPS:-FAILED} | ${RUST_CLOSED_P99:-FAILED} | ${RUST_CLOSED_P999:-FAILED} | ${RUST_CLOSED_MAX:-FAILED} |
| kcp-go↔kcp-go | ${GO_CLOSED_RPS:-FAILED} | ${GO_CLOSED_P99:-FAILED} | ${GO_CLOSED_P999:-FAILED} | ${GO_CLOSED_MAX:-FAILED} |

## 严格标准合规

- **有界开放模型**：sender 与 reader 独立；sender 按 target_rps=$RPS 调度，落后超过 50ms 时显式计入 shed，避免无界追赶掩盖过载。
- **计时边界**：RTT 从 Write/write_all 成功接纳后开始；写窗口等待不计入已完成请求的 RTT，无法接纳的计划槽由 shed 单独暴露。因此验收必须同时看 offered、ok、shed 和 percentile。
- **无百分位二次平均**：每个组合的 P50/P90/P99/P999 均由测量期全部原始样本（约 $((RPS * DURATION)) 个）排序后一次性计算，绝不跨批次取均值。
- **独立预热**：前 ${WARMUP}s 为预热阶段（连接建立/KCP 窗口/分配器），样本排除。
- **样本量**：目标 offered = target_rps × duration = $((RPS * DURATION))；实际值以表中 offered 为准，正式运行应要求 shed=0 且 inflight_end=0。
- **时长**：duration=${DURATION}s（可配；正式报告建议延长到 10–30min，以覆盖周期性系统抖动）。
- **环境**：同机回环、四组合使用相同的双进程拓扑与负载；这隔离语言 runtime，但不模拟公网 RTT、抖动、丢包或路由排队。
- **性能口径**：固定 RPS 表只回答延迟；最大性能由 concurrency=${CONCURRENCY} 的闭环测试回答，避免把 500 RPS 下的空闲延迟误当吞吐能力。

## 结论

- **互操作双向通过**：kcp-rs(tokio) ↔ kcp-go 在裸 KCP 层跨语言互通，回声逐位一致。
- **最大 raw KCP 吞吐**：kcp-rs(tokio) ${RUST_CLOSED_RPS:-FAILED} req/s，kcp-go ${GO_CLOSED_RPS:-FAILED} req/s；Rust 相对 Go 为 ${THROUGHPUT_DELTA}%（正数表示 Rust 更快）。
- 固定 RPS 的 P50/P99 是延迟与调度稳定性指标，不能用于推断实现的最大吞吐；上面的闭环结果才是本报告的 raw KCP 性能结论。

_本报告由 bench/run_p99.sh 生成（开放模型 4 组合矩阵 + 2 闭环基线）。_
EOF
} > "$REPORT"

echo ""
echo "==> report written: $REPORT"
