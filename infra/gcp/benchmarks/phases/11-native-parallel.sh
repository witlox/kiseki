#!/bin/bash
# Phase 11 — parallel native bench (N clients writing concurrently).
#
# GCP mode: fans out to every client VM via ssh, each runs
# `kiseki-client bench --shape put-heavy` against the leader's native
# TCP-framed port. Aggregates the per-client throughput.
#
# Local mode: spawns N concurrent bench processes on the same host
# (still useful for measuring server-side concurrent-connection
# handling, even though we don't get cross-NIC parallelism).
#
# Env:
#   KISEKI_BENCH_DURATION_SECS   (default: 30)
#   KISEKI_BENCH_CONCURRENCY     (default: 16)   per-client concurrency
#   KISEKI_BENCH_OBJECT_SIZE     (default: 65536)
#   KISEKI_BENCH_PARALLEL_CLIENTS (default: 2 local / count of CLIENT_ARRAY on gcp)
#   KISEKI_BENCH_NAMESPACE_FANOUT (default: storage_count) — issue #66 fix 2.
#     `kiseki-client bench --namespace-fanout N` round-robins PUTs across
#     N namespaces; with N = storage_count and shards split out, each
#     namespace can land on its own shard leader (the perf-harness
#     consumer of the multi-shard architecture).

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

MODE=$(bench_mode)
if [ "$MODE" = "gcp" ]; then
  N_CLIENTS="${KISEKI_BENCH_PARALLEL_CLIENTS:-${#CLIENT_ARRAY[@]}}"
  # Default fanout = storage_count, so each bench-ns-<i> aims for
  # its own shard once splits are wired. Defaults to 1 when
  # STORAGE_IPS_ARRAY is empty (local single-node debug runs).
  N_STORAGE="${#STORAGE_IPS_ARRAY[@]}"
  FANOUT="${KISEKI_BENCH_NAMESPACE_FANOUT:-${N_STORAGE:-1}}"
else
  N_CLIENTS="${KISEKI_BENCH_PARALLEL_CLIENTS:-2}"
  FANOUT="${KISEKI_BENCH_NAMESPACE_FANOUT:-1}"
fi

OUT="$RESULTS/11-native-parallel.txt"
{
  echo "=== Phase 11: parallel native put-heavy ==="
  echo "mode=$MODE n_clients=$N_CLIENTS endpoint=$LEADER_NATIVE_URL"
  echo "per-client: duration=${DURATION}s concurrency=$CONC object_size=$OBJSZ"
  echo "namespace_fanout=$FANOUT"
  echo ""
} | tee "$OUT"

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

# Per-client output goes to RESULTS so we can post-process later.
PIDS=()
for i in $(seq 0 $((N_CLIENTS - 1))); do
  CLIENT_OUT="$RESULTS/11-native-parallel-client-$i.json"
  if [ "$MODE" = "gcp" ]; then
    (
      idx=$((i % ${#CLIENT_ARRAY[@]}))
      client_run "$idx" > "$CLIENT_OUT" 2>&1 <<EOF
$CLIENT_BIN bench --endpoint $LEADER_NATIVE_URL --shape put-heavy \
  --concurrency $CONC --object-size $OBJSZ \
  --duration-secs $DURATION --namespace-fanout $FANOUT --json
EOF
    ) &
  else
    (
      "$CLIENT_BIN" bench --endpoint "$LEADER_NATIVE_URL" --shape put-heavy \
        --concurrency "$CONC" --object-size "$OBJSZ" \
        --duration-secs "$DURATION" --namespace-fanout "$FANOUT" --json > "$CLIENT_OUT" 2>&1
    ) &
  fi
  PIDS+=($!)
done

ERR=0
for pid in "${PIDS[@]}"; do
  wait "$pid" || ERR=$((ERR + 1))
done

# Aggregate. Each per-client JSON has ops_per_sec + mib_per_sec.
RESULTS="$RESULTS" python3 <<'PY' | tee -a "$OUT"
import glob, json, os, sys
files = sorted(glob.glob(os.environ['RESULTS'] + '/11-native-parallel-client-*.json'))
total_ops_s = 0.0
total_mib_s = 0.0
worst_p99 = 0
client_n = 0
parsed_n = 0
for f in files:
    client_n += 1
    try:
        with open(f) as fh:
            txt = fh.read()
        # Take the last JSON line (some runs prepend banner text in local mode)
        last = [l for l in txt.splitlines() if l.startswith('{')]
        if not last:
            print(f'  client-{client_n}: no JSON in {os.path.basename(f)}', file=sys.stderr)
            continue
        d = json.loads(last[-1])
        parsed_n += 1
        ops_s = d['ops_per_sec']
        mib_s = d['mib_per_sec']
        p99 = d['p99_us']
        total_ops_s += ops_s
        total_mib_s += mib_s
        if p99 > worst_p99: worst_p99 = p99
        print(f'  client-{client_n}: {ops_s:>8.0f} op/s · {mib_s:>7.1f} MiB/s · p99={p99}us')
    except Exception as e:
        print(f'  client-{client_n}: parse error: {e}', file=sys.stderr)

print()
print(f'aggregate ({parsed_n}/{client_n} clients): {total_ops_s:.0f} op/s · {total_mib_s:.1f} MiB/s · worst-client p99={worst_p99}us')
sys.exit(0 if parsed_n > 0 else 2)
PY
PY_RC=$?

if [ "$ERR" -gt 0 ] || [ "$PY_RC" -ne 0 ]; then
  echo "HALT: $ERR client(s) failed, py_rc=$PY_RC" | tee -a "$OUT"
  exit 2
fi
echo "OK" | tee -a "$OUT"
