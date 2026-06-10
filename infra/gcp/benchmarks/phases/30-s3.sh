#!/bin/bash
# Phase 30 — S3: PUT latency (1KB×N), sequential PUT, parallel GET.
#
# Uses `kiseki-client bench` for the PUT-heavy + GET-heavy + mixed
# shapes against the HTTP listener (port 9000). PUT-latency is a
# distinct workload (N tiny sequential PUTs) so we run it as a
# separate sub-pass using python+urllib (no extra deps beyond
# python3, which the client VMs have).
#
# In gcp mode everything dispatches to client VM 0 via client_run —
# kiseki-client is installed on the client VMs, NOT on bench-ctrl,
# and ctrl-VM-local S3 numbers wouldn't be client-path numbers anyway
# (2026-06-10 bench-correctness review).
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
  # kiseki-client lives on the client VMs (the bench runs there via
  # client_run), NOT on bench-ctrl — probe it remotely.
  CLIENT_BIN=/usr/local/bin/kiseki-client
  if ! client_run 0 <<< "test -x $CLIENT_BIN && $CLIENT_BIN --help 2>&1 | grep -qE '^[[:space:]]+bench[[:space:]]'"; then
    echo "HALT: no $CLIENT_BIN with 'bench' subcommand on client VM 0" | tee -a "$OUT"
    exit 2
  fi
else
  cands=(
    "$BENCH_DIR/../../../target/debug/kiseki-client"
    "$BENCH_DIR/../../../target/release/kiseki-client"
    "$(which kiseki-client 2>/dev/null || true)"
  )
  for cand in "${cands[@]}"; do
    [ -x "$cand" ] || continue
    if "$cand" --help 2>&1 | grep -qE '^[[:space:]]+bench[[:space:]]'; then
      CLIENT_BIN="$cand"; break
    fi
  done
fi
if [ -z "$CLIENT_BIN" ]; then
  echo "HALT: no kiseki-client with 'bench' subcommand" | tee -a "$OUT"
  exit 2
fi
echo "client=$CLIENT_BIN" | tee -a "$OUT"
echo "" | tee -a "$OUT"

# 30a — PUT latency (sequential 1KB × N) via python+urllib so we get
# per-request timing. The latency tail is what hydrator-backlog
# wedges first (cf. 2026-05-15 evening's p99=93.8ms).
#
# Payloads are os.urandom per object (F7): a constant payload makes
# every PUT after the first a dedup hit that skips the small-object
# store commit — i.e. it measures the refcount path, not the write
# path. 1 KiB × N of urandom is negligible CPU.
#
# Dispatched through client_run so gcp mode measures from a client VM
# (local mode: client_run is a plain `bash -s`, same as before).
# NOTE: the heredoc below is intentionally UNQUOTED — $LEADER_S3_URL /
# $LAT_N expand here; the python body must stay free of `$`/backticks.
echo "--- 30a: PUT latency 1KB x ${LAT_N} ---" | tee -a "$OUT"
LAT_OUT="$RESULTS/30-s3-latency.txt"
client_run 0 > "$LAT_OUT" 2>&1 <<EOF
LEADER_S3_URL="$LEADER_S3_URL" LAT_N="$LAT_N" python3 <<'PY'
import os, time, urllib.request

URL = os.environ['LEADER_S3_URL'] + '/latency-test'
N = int(os.environ['LAT_N'])

# Ensure bucket exists
try:
    urllib.request.urlopen(urllib.request.Request(URL, method='PUT'), timeout=5)
except Exception:
    pass  # bucket may already exist; PUT is idempotent

lats_ms = []
errs = 0
for i in range(N):
    payload = os.urandom(1024)  # unique bytes per object (F7: defeat dedup)
    req = urllib.request.Request(f'{URL}/obj-{i}', data=payload, method='PUT')
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
EOF
LAT_RC=$?
tee -a "$OUT" < "$LAT_OUT"
LAT_ERRS=$(grep -oE 'errors=[0-9]+' "$LAT_OUT" | head -1 | cut -d= -f2)
if [ "$LAT_RC" -ne 0 ] || [ "${LAT_ERRS:-1}" -gt 0 ]; then
  echo "HALT (C-2): 30a latency loop rc=$LAT_RC errors=${LAT_ERRS:-unparsed}" | tee -a "$OUT"
  exit 2
fi

# 30b/c/d — Throughput shapes via kiseki-client bench. gcp mode runs
# on client VM 0 (client_run); local mode runs the dev-box binary.
ERR=0
for shape in put-heavy get-heavy mixed; do
  echo "" | tee -a "$OUT"
  echo "--- 30: shape=$shape ---" | tee -a "$OUT"
  if [ "$MODE" = "gcp" ]; then
    # pipefail is set, so a non-zero bench rc survives the tee.
    client_run 0 <<EOF | tee -a "$OUT" || ERR=$((ERR + 1))
$CLIENT_BIN bench --endpoint $LEADER_S3_URL --shape $shape \
  --concurrency $CONC --object-size $OBJSZ \
  --duration-secs $DURATION --json
EOF
  else
    "$CLIENT_BIN" bench --endpoint "$LEADER_S3_URL" --shape "$shape" \
      --concurrency "$CONC" --object-size "$OBJSZ" \
      --duration-secs "$DURATION" --json 2>&1 | tee -a "$OUT" || ERR=$((ERR + 1))
  fi
done

if [ "$ERR" -gt 0 ]; then
  echo "HALT: $ERR of 3 shapes failed (bench rc != 0)" | tee -a "$OUT"
  exit 2
fi

# C-2: independently sum the report JSONs' errors field.
ERRS=$(bench_errors_total "$OUT") || true
if [ "$ERRS" != "0" ]; then
  echo "HALT (C-2): bench reported errors=$ERRS (or no parsable report) — functional break, numbers invalid" | tee -a "$OUT"
  exit 2
fi

echo "" | tee -a "$OUT"
echo "OK" | tee -a "$OUT"
