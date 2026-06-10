#!/bin/bash
# Phase 10 — single-client native (ADR-042 TCP-framed) PUT/GET/Mixed.
#
# Drives `kiseki-client bench` against the leader's native TCP-framed
# port (9103 by default). Single client, varying shape.
#
# Exit 0 — three shapes ran cleanly. Errors > 0 → exit 2 (functional
#          break, halt the run).
#
# Configurable via env:
#   KISEKI_BENCH_DURATION_SECS   (default: 30)
#   BENCH_CONCURRENCY            (default: KISEKI_BENCH_CONCURRENCY, then 16)
#   BENCH_CONNECTIONS            (default: 1 — the realistic single-
#                                 process mount shape; sweep axis for
#                                 the #212 saturation A/B)
#   KISEKI_BENCH_OBJECT_SIZE     (default: 65536)
#   KISEKI_BENCH_WARMUP_OBJECTS  (default: 256)
#
# Output embeds the sweep-cell label conc<N>-conn<M> so cells of a
# concurrency × connections sweep never overwrite each other (C-3/F13,
# 2026-06-10 bench-correctness review).

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
WARMUP="${KISEKI_BENCH_WARMUP_OBJECTS:-256}"
CELL="conc${CONC}-conn${CONNS}"

OUT="$RESULTS/10-native-single-${CELL}.txt"
{
  echo "=== Phase 10: native single-client (TCP-framed) ==="
  echo "endpoint=$LEADER_NATIVE_URL cell=$CELL"
  echo "duration=${DURATION}s concurrency=$CONC connections=$CONNS object_size=$OBJSZ warmup=$WARMUP"
  echo ""
} | tee "$OUT"

# Locate kiseki-client. In gcp mode it's installed on client VMs; in
# local mode use the dev-box binary built via `cargo build`. The
# `--features native` build is required for kiseki:// endpoints.
CLIENT_BIN=""
# Pick the first kiseki-client binary that's executable AND supports
# the `bench` subcommand (#58). An older release binary on the path
# would otherwise be selected silently and print the top-level help
# instead of running the workload — exactly what happened in the
# initial local smoke.
if [ "$(bench_mode)" = "gcp" ]; then
  # kiseki-client lives on the client VMs (the bench runs there via
  # client_run), NOT on bench-ctrl — probe it remotely.
  CLIENT_BIN=/usr/local/bin/kiseki-client
  if ! client_run 0 <<< "test -x $CLIENT_BIN && $CLIENT_BIN --help 2>&1 | grep -qE '^[[:space:]]+bench[[:space:]]'"; then
    echo "HALT: no $CLIENT_BIN with 'bench' subcommand on client VM 0" | tee -a "$OUT"
    echo "  ensure the staged tarball was built post-#58" | tee -a "$OUT"
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
      CLIENT_BIN="$cand"
      break
    fi
  done
fi
if [ -z "$CLIENT_BIN" ]; then
  echo "HALT: no kiseki-client binary with 'bench' subcommand found." | tee -a "$OUT"
  echo "  local: cargo build -p kiseki-client --features native,remote-http --bin kiseki-client" | tee -a "$OUT"
  exit 2
fi
echo "client=$CLIENT_BIN" | tee -a "$OUT"
echo "" | tee -a "$OUT"

ERR=0
for shape in put-heavy get-heavy mixed; do
  echo "--- shape=$shape ---" | tee -a "$OUT"
  if [ "$(bench_mode)" = "gcp" ]; then
    # pipefail is set, so a non-zero bench rc survives the tee.
    client_run 0 <<EOF | tee -a "$OUT" || ERR=$((ERR + 1))
$CLIENT_BIN bench --endpoint $LEADER_NATIVE_URL --shape $shape \
  --concurrency $CONC --connections $CONNS --object-size $OBJSZ \
  --duration-secs $DURATION --warmup-objects $WARMUP --json
EOF
  else
    "$CLIENT_BIN" bench --endpoint "$LEADER_NATIVE_URL" --shape "$shape" \
      --concurrency "$CONC" --connections "$CONNS" --object-size "$OBJSZ" \
      --duration-secs "$DURATION" --warmup-objects "$WARMUP" --json 2>&1 \
      | tee -a "$OUT" || ERR=$((ERR + 1))
  fi
done

if [ "$ERR" -gt 0 ]; then
  echo "HALT: $ERR of 3 shapes failed (bench rc != 0)" | tee -a "$OUT"
  exit 2
fi

# C-2: never report numbers while ops are failing — the bench's exit
# code is checked above, but independently sum the report JSONs'
# errors field too.
ERRS=$(bench_errors_total "$OUT") || true
if [ "$ERRS" != "0" ]; then
  echo "HALT (C-2): bench reported errors=$ERRS (or no parsable report) — functional break, numbers invalid" | tee -a "$OUT"
  exit 2
fi
echo "" | tee -a "$OUT"
echo "OK" | tee -a "$OUT"
