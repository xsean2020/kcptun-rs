#!/usr/bin/env bash
# Sweep H2 (compress offload bytes) and H1 (heavy8 encrypt offload) thresholds.
# Requires bins rebuilt WITH env_usize support (kcp-rs crypto_buf).
#
# One process lifetime caches env via OnceLock — each run starts fresh processes.
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$(pwd)"
OUT="${OUT_DIR:-bench/profiles/sweep-h1h2-$(date +%Y%m%d-%H%M%S)}"
mkdir -p "$OUT"
RUNS="${RUNS:-3}"
# Random payload size (bytes). Default 2MiB — matches bench_rust_vs_go style, not patterned thr.
SIZE="${SIZE:-2097152}"
KEY="${KEY:-bench-key}"
MODE="${MODE:-fast2}"
SKIP_REBUILD="${SKIP_REBUILD:-0}"

TOKIO_S="$ROOT/target/release/kcptun-server"
TOKIO_C="$ROOT/target/release/kcptun-client"

log() { echo "$*" | tee -a "$OUT/notes.md"; }

if [ "$SKIP_REBUILD" != "1" ]; then
  log "## rebuild release"
  make release 2>&1 | tee "$OUT/build-tokio.log" | tail -3
fi
for b in "$TOKIO_S" "$TOKIO_C"; do
  [ -x "$b" ] || { log "missing $b"; exit 1; }
  log "bin $(ls -la "$b" | awk '{print $5,$6,$7,$8,$9}')"
done
log "git $(git rev-parse --short HEAD) $(date -Iseconds 2>/dev/null || date)"
log ""

cleanup_ports() {
  local base=$1
  for p in "$base" $((base+1)) $((base+2)); do
    lsof -tiTCP:"$p" -sTCP:LISTEN 2>/dev/null | xargs kill -9 2>/dev/null || true
  done
}

# thr one shot; prints MB/s only
# env vars for child: passed through environment
measure() {
  local label="$1" SBIN="$2" CBIN="$3" CRYPT="$4" NOCOMP="$5"
  local PORT_BASE=$((35000 + RANDOM % 2000))
  local ECHO=$PORT_BASE SP=$((PORT_BASE+1)) CP=$((PORT_BASE+2))
  cleanup_ports "$PORT_BASE"
  local COMP=()
  [ "$NOCOMP" = "1" ] && COMP=(--nocomp)
  local COMMON=(--key "$KEY" --crypt "$CRYPT" --mode "$MODE" --sndwnd 1024 --rcvwnd 1024 --smuxver 2)

  python3 -u -c "
import socket,threading
def echo(s,a):
  try:
    while True:
      d=s.recv(65536)
      if not d: break
      s.sendall(d)
  except: pass
  s.close()
s=socket.socket(); s.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1)
s.bind(('0.0.0.0',$ECHO)); s.listen(32)
while True:
  c,a=s.accept(); threading.Thread(target=echo,args=(c,a),daemon=True).start()
" &
  local EPID=$!
  # shellcheck disable=SC2086
  env "${EXTRA_ENV[@]+"${EXTRA_ENV[@]}"}" "$SBIN" -l "0.0.0.0:$SP" -t "127.0.0.1:$ECHO" \
    "${COMMON[@]}" "${COMP[@]+"${COMP[@]}"}" >"$OUT/s-$label.log" 2>&1 &
  local SPID=$!
  sleep 0.5
  env "${EXTRA_ENV[@]+"${EXTRA_ENV[@]}"}" "$CBIN" -l "127.0.0.1:$CP" -r "127.0.0.1:$SP" \
    "${COMMON[@]}" "${COMP[@]+"${COMP[@]}"}" >"$OUT/c-$label.log" 2>&1 &
  local CPID=$!
  local tries=60
  while [ $tries -gt 0 ]; do
    python3 -c "import socket,sys;s=socket.socket();s.settimeout(0.15)
try:
 s.connect(('127.0.0.1',$CP));s.close()
except:sys.exit(1)" 2>/dev/null && break
    sleep 0.1; tries=$((tries-1))
  done
  if [ $tries -le 0 ]; then
    log "FAIL ready $label"; tail -15 "$OUT/s-$label.log" "$OUT/c-$label.log" | tee -a "$OUT/notes.md" || true
    kill $CPID $SPID $EPID 2>/dev/null || true; wait 2>/dev/null || true
    echo 0; return 1
  fi
  # Random payload (os.urandom) — patterned data inflates Snappy thr by 10×+ and is invalid for H2.
  python3 "$ROOT/bench/thr_random.py" "$CP" --size "$SIZE" --timeout 90 \
    >"$OUT/thr-$label.out" 2>"$OUT/thr-$label.err" || true
  local thr
  thr=$(python3 - <<PY
import re, os
p = open("$OUT/thr-$label.out").read() if os.path.exists("$OUT/thr-$label.out") else ""
p += "\n" + (open("$OUT/thr-$label.err").read() if os.path.exists("$OUT/thr-$label.err") else "")
m = re.search(r"THR_MBps=([0-9]+(?:\\.[0-9]+)?)", p)
print(m.group(1) if m else "0")
PY
)
  kill $CPID $SPID $EPID 2>/dev/null || true
  wait $CPID $SPID $EPID 2>/dev/null || true
  cleanup_ports "$PORT_BASE"
  echo "$thr"
}

echo "series,run,thr_MBps,env" >"$OUT/raw.csv"

median3() {
  python3 -c "import statistics,sys; v=sorted(float(x) for x in sys.argv[1:]); print(statistics.median(v) if v else 0)" "$@"
}

run_cfg() {
  local series="$1" SBIN="$2" CBIN="$3" CRYPT="$4" NOCOMP="$5"
  shift 5
  # remaining are env KEY=VAL
  EXTRA_ENV=("$@")
  local env_desc
  env_desc=$(printf '%s;' "${EXTRA_ENV[@]+"${EXTRA_ENV[@]}"}")
  local vals=()
  local i thr
  for i in $(seq 1 "$RUNS"); do
    thr=$(measure "${series}-r${i}" "$SBIN" "$CBIN" "$CRYPT" "$NOCOMP")
    vals+=("$thr")
    echo "$series,$i,$thr,$env_desc" >>"$OUT/raw.csv"
    log "  $series run$i thr=$thr env=$env_desc"
  done
  local med
  med=$(median3 "${vals[@]}")
  echo "$series,$med,$env_desc" >>"$OUT/medians.csv"
  log "MEDIAN $series = $med  ($env_desc)"
}

echo "series,median_MBps,env" >"$OUT/medians.csv"

log "## H2 baseline + compress threshold sweep (tokio xor COMP)"
# baseline default (no env)
run_cfg "tokio-xor-comp-baseline" "$TOKIO_S" "$TOKIO_C" xor 0
run_cfg "tokio-xor-comp-16k" "$TOKIO_S" "$TOKIO_C" xor 0 KCPTUN_COMPRESS_CPU_BLOCK_BYTES=16384
run_cfg "tokio-xor-comp-32k" "$TOKIO_S" "$TOKIO_C" xor 0 KCPTUN_COMPRESS_CPU_BLOCK_BYTES=32768
run_cfg "tokio-xor-comp-48k" "$TOKIO_S" "$TOKIO_C" xor 0 KCPTUN_COMPRESS_CPU_BLOCK_BYTES=49152
# control: null comp should not regress badly at 16k
run_cfg "tokio-null-comp-baseline" "$TOKIO_S" "$TOKIO_C" null 0
run_cfg "tokio-null-comp-16k" "$TOKIO_S" "$TOKIO_C" null 0 KCPTUN_COMPRESS_CPU_BLOCK_BYTES=16384
# xor no-comp control
run_cfg "tokio-xor-nocomp-baseline" "$TOKIO_S" "$TOKIO_C" xor 1

log "## H1 tokio xtea/cast5 no-comp heavy8 threshold sweep"
run_cfg "tokio-xtea-nocomp-h8-1-512" "$TOKIO_S" "$TOKIO_C" xtea 1
run_cfg "tokio-xtea-nocomp-h8-4-4096" "$TOKIO_S" "$TOKIO_C" xtea 1 \
  KCPTUN_HEAVY8_ENCRYPT_MIN_PKTS=4 KCPTUN_HEAVY8_ENCRYPT_MIN_BYTES=4096
run_cfg "tokio-xtea-nocomp-h8-8-8192" "$TOKIO_S" "$TOKIO_C" xtea 1 \
  KCPTUN_HEAVY8_ENCRYPT_MIN_PKTS=8 KCPTUN_HEAVY8_ENCRYPT_MIN_BYTES=8192
run_cfg "tokio-xtea-nocomp-h8-16-16384" "$TOKIO_S" "$TOKIO_C" xtea 1 \
  KCPTUN_HEAVY8_ENCRYPT_MIN_PKTS=16 KCPTUN_HEAVY8_ENCRYPT_MIN_BYTES=16384

run_cfg "tokio-cast5-nocomp-h8-1-512" "$TOKIO_S" "$TOKIO_C" cast5 1
run_cfg "tokio-cast5-nocomp-h8-4-4096" "$TOKIO_S" "$TOKIO_C" cast5 1 \
  KCPTUN_HEAVY8_ENCRYPT_MIN_PKTS=4 KCPTUN_HEAVY8_ENCRYPT_MIN_BYTES=4096
run_cfg "tokio-cast5-nocomp-h8-8-8192" "$TOKIO_S" "$TOKIO_C" cast5 1 \
  KCPTUN_HEAVY8_ENCRYPT_MIN_PKTS=8 KCPTUN_HEAVY8_ENCRYPT_MIN_BYTES=8192

log ""
log "## medians"
column -t -s, "$OUT/medians.csv" 2>/dev/null || cat "$OUT/medians.csv"
log "OUT=$OUT"
echo "$OUT"
