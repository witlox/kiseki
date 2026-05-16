#!/bin/bash
# Phase 30 — S3: PUT latency (1KB×N), sequential PUT, parallel GET.
#
# Uses `kiseki-client bench` for the PUT-heavy + GET-heavy + mixed
# shapes against the HTTP listener (port 9000). PUT-latency is a
# distinct workload (N tiny sequential PUTs) so we run it as a
# separate sub-pass using python+urllib (no extra deps on the
# bench-ctrl).
#
# Env:
#   KISEKI_BENCH_DURATION_SECS    (default: 30)
#   KISEKI_BENCH_CONCURRENCY      (default: 16)
#   KISEKI_BENCH_OBJECT_SIZE      (default: 65536)
#   KISEKI_BENCH_S3_LATENCY_COUNT (default: 100)

set -uo pipefail
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
BENCH_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
# shellcheck disable=SC1091
source "$BENCH_DIR/perf-common.sh"

discover_leader > /dev/null 2>&1
leader_endpoints
DURATION="${KISEKI_BENCH_DURATION_SECS:-30}"
CONC="${KISEKI_BENCH_CONCURRENCY:-16}"
OBJSZ="${KISEKI_BENCH_OBJECT_SIZE:-65536}"
LAT_N="${KISEKI_BENCH_S3_LATENCY_COUNT:-100}"

OUT="$RESULTS/30-s3.txt"
{
  echo "=== Phase 30: S3 ==="
  echo "endpoint=$LEADER_S3_URL"
  echo "duration=${DURATION}s concurrency=$CONC object_size=$OBJSZ"
  echo "latency_count=${LAT_N}"
  echo ""
} | tee "$OUT"

MODE=$(bench_mode)
CLIENT_BIN=""
if [ "$MODE" = "gcp" ]; then
  cands=(/usr/local/bin/kiseki-client)
else
  cands=(
    "$BENCH_DIR/../../../target/debug/kiseki-client"
    "$BENCH_DIR/../../../target/release/kiseki-client"
    "$(which kiseki-client 2>/dev/null || true)"
  )
fi
for cand in "${cands[@]}"; do
  [ -x "$cand" ] || continue
  if "$cand" --help 2>&1 | grep -qE '^[[:space:]]+bench[[:space:]]'; then
    CLIENT_BIN="$cand"; break
  fi
done
if [ -z "$CLIENT_BIN" ]; then
  echo "HALT: no kiseki-client with 'bench' subcommand" | tee -a "$OUT"
  exit 2
fi
echo "client=$CLIENT_BIN" | tee -a "$OUT"
echo "" | tee -a "$OUT"

# 30a — PUT latency (sequential 1KB × N) via python+urllib so we get
# per-request timing. The latency tail is what hydrator-backlog
# wedges first (cf. 2026-05-15 evening's p99=93.8ms).
echo "--- 30a: PUT latency 1KB x ${LAT_N} ---" | tee -a "$OUT"
LEADER_S3_URL="$LEADER_S3_URL" LAT_N="$LAT_N" python3 <<'PY' | tee -a "$OUT"
import os, time, urllib.request

URL = os.environ['LEADER_S3_URL'] + '/latency-test'
N = int(os.environ['LAT_N'])
PAYLOAD = b'x' * 1024

# Ensure bucket exists
try:
    urllib.request.urlopen(urllib.request.Request(URL, method='PUT'), timeout=5)
except Exception:
    pass  # bucket may already exist; PUT is idempotent

lats_ms = []
errs = 0
for i in range(N):
    req = urllib.request.Request(f'{URL}/obj-{i}', data=PAYLOAD, method='PUT')
    t0 = time.perf_counter()
    try:
        urllib.request.urlopen(req, timeout=10).read()
        lats_ms.append((time.perf_counter() - t0) * 1000)
    except Exception:
        errs += 1

lats_ms.sort()
n = len(lats_ms)
def p(q): return lats_ms[min(int(n*q), n-1)] if n else 0
print(f'  count={n}/{N} errors={errs} p50={p(0.5):.1f}ms p95={p(0.95):.1f}ms p99={p(0.99):.1f}ms min={min(lats_ms):.1f} max={max(lats_ms):.1f}' if n else f'  ALL FAILED (errors={errs})')
PY

# 30b/c/d — Throughput shapes via kiseki-client bench
for shape in put-heavy get-heavy mixed; do
  echo "" | tee -a "$OUT"
  echo "--- 30: shape=$shape ---" | tee -a "$OUT"
  "$CLIENT_BIN" bench --endpoint "$LEADER_S3_URL" --shape "$shape" \
    --concurrency "$CONC" --object-size "$OBJSZ" \
    --duration-secs "$DURATION" --json 2>&1 | tee -a "$OUT"
done

echo "" | tee -a "$OUT"
echo "OK" | tee -a "$OUT"
