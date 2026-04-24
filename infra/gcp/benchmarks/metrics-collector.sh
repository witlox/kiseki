#!/bin/bash
# Background metrics collector for Kiseki performance tests.
#
# Scrapes Prometheus /metrics from all storage nodes every 10s.
# Writes timestamped snapshots + generates a delta summary at the end.
#
# Usage: metrics-collector.sh <results_dir> &
#        COLLECTOR_PID=$!
#        ... run benchmarks ...
#        kill $COLLECTOR_PID; wait $COLLECTOR_PID 2>/dev/null
#        bash metrics-collector.sh --summarize <results_dir>
set -eo pipefail

ALL_STORAGE="10.0.0.10 10.0.0.11 10.0.0.12 10.0.0.20 10.0.0.21"
INTERVAL=10

# Key metrics to extract (grep patterns)
METRIC_PATTERNS=(
  "kiseki_raft_commit_latency_seconds"
  "kiseki_gateway_requests_total"
  "kiseki_gateway_request_duration_seconds"
  "kiseki_chunk_write_bytes_total"
  "kiseki_chunk_read_bytes_total"
  "kiseki_transport_connections_active"
  "kiseki_shard_delta_count"
  "kiseki_pool_capacity_used_bytes"
)

summarize() {
  local DIR="$1/metrics"
  [ -d "$DIR" ] || { echo "No metrics directory at $DIR"; return 1; }

  local SUMMARY="$1/metrics-summary.txt"
  echo "=== Metrics Summary ===" > "$SUMMARY"
  echo "Collected: $(ls "$DIR"/snap-*.txt 2>/dev/null | wc -l) snapshots" >> "$SUMMARY"

  FIRST=$(ls "$DIR"/snap-*.txt 2>/dev/null | head -1)
  LAST=$(ls "$DIR"/snap-*.txt 2>/dev/null | tail -1)

  if [ -z "$FIRST" ] || [ -z "$LAST" ]; then
    echo "Insufficient snapshots for delta computation" >> "$SUMMARY"
    cat "$SUMMARY"
    return 0
  fi

  FIRST_TS=$(basename "$FIRST" | sed 's/snap-//;s/\.txt//')
  LAST_TS=$(basename "$LAST" | sed 's/snap-//;s/\.txt//')
  ELAPSED=$(( LAST_TS - FIRST_TS ))
  [ "$ELAPSED" -le 0 ] && ELAPSED=1

  echo "Duration: ${ELAPSED}s" >> "$SUMMARY"
  echo "" >> "$SUMMARY"

  # Per-node counter deltas
  for ip in $ALL_STORAGE; do
    echo "--- $ip ---" >> "$SUMMARY"

    for metric in kiseki_gateway_requests_total kiseki_chunk_write_bytes_total kiseki_chunk_read_bytes_total; do
      FIRST_VAL=$(grep "^${metric}" "$FIRST" 2>/dev/null | grep "$ip" | awk '{sum+=$2} END{print sum+0}')
      LAST_VAL=$(grep "^${metric}" "$LAST" 2>/dev/null | grep "$ip" | awk '{sum+=$2} END{print sum+0}')
      DELTA=$(( ${LAST_VAL:-0} - ${FIRST_VAL:-0} ))
      RATE=$(python3 -c "print(f'{$DELTA / $ELAPSED:.1f}')" 2>/dev/null || echo "N/A")
      SHORT=$(echo "$metric" | sed 's/kiseki_//')
      echo "  $SHORT: delta=$DELTA rate=${RATE}/s" >> "$SUMMARY"
    done

    # Latest gauge values
    for metric in kiseki_transport_connections_active kiseki_shard_delta_count; do
      VAL=$(grep "^${metric}" "$LAST" 2>/dev/null | grep "$ip" | awk '{print $2}' | head -1)
      SHORT=$(echo "$metric" | sed 's/kiseki_//')
      echo "  $SHORT: ${VAL:-0}" >> "$SUMMARY"
    done
    echo "" >> "$SUMMARY"
  done

  # Raft commit latency trend (p99 from histogram)
  echo "--- Raft Commit Latency (cluster-wide) ---" >> "$SUMMARY"
  for snap in "$DIR"/snap-*.txt; do
    TS=$(basename "$snap" | sed 's/snap-//;s/\.txt//')
    COUNT=$(grep 'kiseki_raft_commit_latency_seconds_count' "$snap" 2>/dev/null | awk '{sum+=$2} END{print sum+0}')
    SUM=$(grep 'kiseki_raft_commit_latency_seconds_sum' "$snap" 2>/dev/null | awk '{sum+=$2} END{printf "%.6f", sum+0}')
    if [ "${COUNT:-0}" -gt 0 ]; then
      AVG=$(python3 -c "print(f'{$SUM / $COUNT * 1000:.2f}')" 2>/dev/null || echo "?")
      echo "  t=$TS count=$COUNT avg=${AVG}ms" >> "$SUMMARY"
    fi
  done

  echo "" >> "$SUMMARY"
  cat "$SUMMARY"
}

# --summarize mode: generate summary from existing snapshots
if [ "${1:-}" = "--summarize" ]; then
  summarize "$2"
  exit 0
fi

# Collector mode: scrape metrics every INTERVAL seconds
RESULTS="${1:?Usage: metrics-collector.sh <results_dir>}"
METRICS_DIR="$RESULTS/metrics"
mkdir -p "$METRICS_DIR"

echo "Metrics collector started (interval=${INTERVAL}s, dir=$METRICS_DIR)"

while true; do
  TS=$(date +%s)
  SNAP="$METRICS_DIR/snap-${TS}.txt"

  for ip in $ALL_STORAGE; do
    PATTERN=$(printf "%s\n" "${METRIC_PATTERNS[@]}" | paste -sd'|')
    curl -sf "http://$ip:9090/metrics" 2>/dev/null | \
      grep -E "^($PATTERN)" | \
      sed "s/^/# node=$ip\n/" >> "$SNAP" 2>/dev/null || true
  done

  sleep "$INTERVAL"
done
