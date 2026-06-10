#!/bin/bash
# Phase 11 — parallel native bench (N clients writing concurrently).
#
# GCP mode: fans out to every client VM via ssh, each runs
# `kiseki-client bench --shape put-heavy` against a DISTINCT storage
# node's native TCP-framed port (client i → storage i mod N — F5,
# 2026-06-10 bench-correctness review: aiming all clients at one
# leader endpoint pays the forward-to-leader hop on (N-1)/N of writes,
# the documented ~8× crush). BENCH_SINGLE_ENDPOINT=1 restores the old
# all-clients-one-leader behavior.
#
# BENCH_SPREAD_ALL=1 (GH #229, step 2 of the 100k roadmap #226): every
# client gets the FULL comma-separated native endpoint list instead of
# one node; `kiseki-client bench` assigns connection i → endpoint
# i mod E, so each client spreads its sockets across ALL storage nodes
# and writes enter via 6/6 ingress gateways instead of 3/6. Requires
# BENCH_CONNECTIONS >= the storage-node count or the tail endpoints
# are never dialled (the bench warns; this phase notes it too).
# Default stays client-i→storage-i; the runbook decides per run.
# Mutually exclusive with BENCH_SINGLE_ENDPOINT=1 (HALT if both).
#
# Local mode: spawns N concurrent bench processes on the same host
# (still useful for measuring server-side concurrent-connection
# handling, even though we don't get cross-NIC parallelism).
# BENCH_SPREAD_ALL is a no-op locally (single compose host).
#
# Env:
#   KISEKI_BENCH_DURATION_SECS   (default: 30)
#   BENCH_CONCURRENCY            (default: KISEKI_BENCH_CONCURRENCY, then 16)
#   BENCH_CONNECTIONS            (default: 1) per-client TCP connections
#   BENCH_SINGLE_ENDPOINT        (default: 0) 1 = all clients → leader
#   BENCH_SPREAD_ALL             (default: 0) 1 = every client → ALL storage
#                                endpoints (#229 6-ingress spread)
#   KISEKI_BENCH_OBJECT_SIZE     (default: 65536)
#   KISEKI_BENCH_PARALLEL_CLIENTS (default: 2 local / count of CLIENT_ARRAY on gcp)
#
# Output files embed the sweep-cell label conc<N>-conn<M> so cells of
# a concurrency × connections sweep never overwrite each other and
# re-runs don't truncate a prior cell's per-client JSONs (C-3/F13).

set -uo pipefail
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
BENCH_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
# shellcheck disable=SC1091
source "$BENCH_DIR/perf-common.sh"

discover_leader > /dev/null 2>&1
leader_endpoints
DURATION="${KISEKI_BENCH_DURATION_SECS:-30}"
CONC="${BENCH_CONCURRENCY:-${KISEKI_BENCH_CONCURRENCY:-16}}"
CONNS="${BENCH_CONNECTIONS:-1}"
OBJSZ="${KISEKI_BENCH_OBJECT_SIZE:-65536}"
CELL="conc${CONC}-conn${CONNS}"
# Fold the endpoint-arm into the cell label so A/B arms at the same
# conc/conn never overwrite each other's artifacts (the C-3/F13
# no-overwrite contract — review blocker on #229).
[ "${BENCH_SPREAD_ALL:-0}" = "1" ] && CELL="${CELL}-spread"
[ "${BENCH_SINGLE_ENDPOINT:-0}" = "1" ] && CELL="${CELL}-single"

MODE=$(bench_mode)
if [ "$MODE" = "gcp" ]; then
  N_CLIENTS="${KISEKI_BENCH_PARALLEL_CLIENTS:-${#CLIENT_ARRAY[@]}}"
else
  N_CLIENTS="${KISEKI_BENCH_PARALLEL_CLIENTS:-2}"
fi

OUT="$RESULTS/11-native-parallel-${CELL}.txt"

# GH #229: BENCH_SPREAD_ALL and BENCH_SINGLE_ENDPOINT contradict each
# other (all-ingress vs one-ingress) — refuse the ambiguous combination
# rather than silently picking one.
if [ "${BENCH_SPREAD_ALL:-0}" = "1" ] && [ "${BENCH_SINGLE_ENDPOINT:-0}" = "1" ]; then
  echo "HALT: BENCH_SPREAD_ALL=1 and BENCH_SINGLE_ENDPOINT=1 are mutually exclusive" | tee "$OUT"
  exit 2
fi

{
  echo "=== Phase 11: parallel native put-heavy ==="
  echo "mode=$MODE n_clients=$N_CLIENTS cell=$CELL single_endpoint=${BENCH_SINGLE_ENDPOINT:-0} spread_all=${BENCH_SPREAD_ALL:-0}"
  echo "per-client: duration=${DURATION}s concurrency=$CONC connections=$CONNS object_size=$OBJSZ"
  echo "native endpoints:"
  native_endpoints | sed 's/^/  /'
  echo ""
} | tee "$OUT"

if [ "${BENCH_SPREAD_ALL:-0}" = "1" ]; then
  if [ "$MODE" != "gcp" ]; then
    echo "NOTE: BENCH_SPREAD_ALL=1 ignored in local mode (single compose host)" | tee -a "$OUT"
  else
    N_ENDPOINTS=${#STORAGE_IPS_ARRAY[@]}
    if [ "$CONNS" -lt "$N_ENDPOINTS" ]; then
      echo "NOTE: BENCH_CONNECTIONS=$CONNS < $N_ENDPOINTS endpoints — only the first $CONNS endpoint(s) get a connection (conn i -> endpoint i mod E); raise BENCH_CONNECTIONS to >= $N_ENDPOINTS for full 6-ingress spread" | tee -a "$OUT"
    fi
  fi
fi

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

# Per-client output goes to RESULTS so we can post-process later.
PIDS=()
for i in $(seq 0 $((N_CLIENTS - 1))); do
  CLIENT_OUT="$RESULTS/11-native-parallel-${CELL}-client-$i.json"
  if [ "${BENCH_SPREAD_ALL:-0}" = "1" ] && [ "$MODE" = "gcp" ]; then
    # #229: every client dials ALL storage nodes; the bench assigns
    # connection i → endpoint i mod E.
    EP=$(native_endpoints_csv)
  else
    EP=$(pick_native_endpoint_for_client "$i")
  fi
  echo "client-$i → $EP" | tee -a "$OUT"
  if [ "$MODE" = "gcp" ]; then
    (
      idx=$((i % ${#CLIENT_ARRAY[@]}))
      client_run "$idx" > "$CLIENT_OUT" 2>&1 <<EOF
$CLIENT_BIN bench --endpoint $EP --shape put-heavy \
  --concurrency $CONC --connections $CONNS --object-size $OBJSZ \
  --duration-secs $DURATION --json
EOF
    ) &
  else
    (
      "$CLIENT_BIN" bench --endpoint "$EP" --shape put-heavy \
        --concurrency "$CONC" --connections "$CONNS" --object-size "$OBJSZ" \
        --duration-secs "$DURATION" --json > "$CLIENT_OUT" 2>&1
    ) &
  fi
  PIDS+=($!)
done

ERR=0
for pid in "${PIDS[@]}"; do
  wait "$pid" || ERR=$((ERR + 1))
done

# C-2: independently sum each per-client report's errors field — the
# bench's own exit code (collected above) is not sufficient evidence
# of a clean run.
BENCH_ERRS=0
for i in $(seq 0 $((N_CLIENTS - 1))); do
  CLIENT_OUT="$RESULTS/11-native-parallel-${CELL}-client-$i.json"
  e=$(bench_errors_total "$CLIENT_OUT") || true
  if [ "$e" != "0" ]; then
    echo "client-$i: errors=$e (or no parsable report) in $(basename "$CLIENT_OUT")" | tee -a "$OUT"
    BENCH_ERRS=$((BENCH_ERRS + 1))
  fi
done

# Aggregate. Each per-client JSON has ops_per_sec + mib_per_sec.
RESULTS="$RESULTS" CELL="$CELL" python3 <<'PY' | tee -a "$OUT"
import glob, json, os, sys
pattern = os.environ['RESULTS'] + '/11-native-parallel-' + os.environ['CELL'] + '-client-*.json'
files = sorted(glob.glob(pattern))
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
print(f'aggregate ({parsed_n}/{client_n} clients, {os.environ["CELL"]}): {total_ops_s:.0f} op/s · {total_mib_s:.1f} MiB/s · worst-client p99={worst_p99}us')
sys.exit(0 if parsed_n > 0 else 2)
PY
PY_RC=$?

if [ "$ERR" -gt 0 ] || [ "$PY_RC" -ne 0 ] || [ "$BENCH_ERRS" -gt 0 ]; then
  echo "HALT: $ERR client(s) rc!=0, $BENCH_ERRS client(s) with errors>0, py_rc=$PY_RC" | tee -a "$OUT"
  exit 2
fi
echo "OK" | tee -a "$OUT"
