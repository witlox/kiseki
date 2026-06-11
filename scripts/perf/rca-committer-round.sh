#!/usr/bin/env bash
# GH #266 — local off-CPU/span decomposition of the committer round.
# Runs ONE long sustained 3-node cell; scrapes kiseki_hotpath_* windows
# fresh (T+90..150s) and at-volume (T+1680..1740s); optional perf sched.
set -uo pipefail
cd /home/witlox/kiseki
OUT=.gcp-build/rca-266
mkdir -p $OUT
export KISEKI_PROFILE_DATA_ROOT=$PWD/$OUT/data
mkdir -p "$KISEKI_PROFILE_DATA_ROOT"
export KISEKI_CHUNK_DEVICE_BYTES=$((16 * 1024 * 1024 * 1024))

./target/release/kiseki-profile run --protocol native --binding tcp \
  --shape put-heavy --concurrency 64 --object-size 4096 --nodes 3 \
  --duration-secs 1800 \
  --server-bin $PWD/target/release/kiseki-server \
  --admin-bin $PWD/target/release/kiseki-admin \
  > $OUT/bench.log 2>&1 &
BENCH=$!
echo "bench pid=$BENCH"

# Discover metrics ports from the harness's own log lines
# ("node N metrics=http://127.0.0.1:PORT/metrics").
METRICS=()
for _try in $(seq 1 36); do
  sleep 5
  mapfile -t METRICS < <(grep -oE 'metrics=http://127\.0\.0\.1:[0-9]+' "$OUT/bench.log" 2>/dev/null | grep -oE '[0-9]+$' | sort -u)
  [ "${#METRICS[@]}" -ge 3 ] && break
done
echo "metrics ports: ${METRICS[*]}"
[ "${#METRICS[@]}" -ge 3 ] || { echo "FATAL: found <3 metrics ports"; kill $BENCH; exit 1; }

scrape() { # $1 tag
  for p in "${METRICS[@]}"; do
    curl -sf --max-time 5 "localhost:$p/metrics" | grep -E '^kiseki_hotpath|^kiseki_intent|^kiseki_log_committer' > "$OUT/$1-$p.txt"
  done
}

# Fresh window: T+90 .. T+150
sleep 50; scrape fresh-a
sleep 60; scrape fresh-b
echo "fresh window captured"

# Optional perf (10 s) on the busiest server pid.
SRV=$(pgrep -f 'target/release/kiseki-server' | head -1)
perf record -e sched:sched_switch -p "$SRV" -o $OUT/perf-fresh.data -- sleep 10 >/dev/null 2>&1 \
  && echo "perf fresh ok" || echo "perf unavailable (paranoid) — spans only"

# At-volume window: T+1680 .. T+1740
sleep 1470; scrape vol-a
sleep 60; scrape vol-b
echo "at-volume window captured"
perf record -e sched:sched_switch -p "$SRV" -o $OUT/perf-vol.data -- sleep 10 >/dev/null 2>&1 || true

wait $BENCH
tail -3 $OUT/bench.log
echo RCA-RUN-DONE
