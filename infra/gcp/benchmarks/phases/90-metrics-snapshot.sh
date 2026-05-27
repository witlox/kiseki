#!/bin/bash
# Phase 90 — final metrics snapshot across all storage nodes.
#
# Captures the post-run state of hydrator, raft log, chunk-store,
# and transport counters per node. Always safe to run; useful for
# diffing against the pre-run baseline (00-health captures the
# initial state implicitly via /cluster/info).

set -uo pipefail
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
BENCH_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
# shellcheck disable=SC1091
source "$BENCH_DIR/perf-common.sh"

discover_leader > /dev/null 2>&1
leader_endpoints
OUT="$RESULTS/90-metrics-snapshot.txt"
{
  echo "=== Phase 90: final metrics snapshot ==="
  echo "timestamp=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "mode=$(bench_mode)"
  echo ""
} | tee "$OUT"

# In local mode the storage IPs are all 127.0.0.1 (mapped to
# docker-compose ports), so use the leader_metrics_url as the
# single representative endpoint. In gcp mode each node has its
# own metrics endpoint at $ip:9090.
if [ "$(bench_mode)" = "local" ]; then
  ENDPOINTS=("$LEADER_METRICS_URL")
else
  ENDPOINTS=()
  for ip in $ALL_STORAGE; do
    ENDPOINTS+=("http://$ip:9090")
  done
fi

for url in "${ENDPOINTS[@]}"; do
  label=$(echo "$url" | sed 's|^http://||; s|:9090$||')
  NODE_FILE="$RESULTS/90-metrics-$label.txt"
  echo "--- $url ---" | tee -a "$OUT"
  curl -sf --max-time 5 "$url/metrics" 2>/dev/null \
    | grep -E "^kiseki_" \
    > "$NODE_FILE" || true
  # Key counters in the summary
  for metric in \
    kiseki_composition_hydrator_last_applied_seq \
    kiseki_composition_hydrator_stalled \
    kiseki_chunk_write_bytes_total \
    kiseki_chunk_read_bytes_total \
    kiseki_raft_log_committed \
    kiseki_transport_connections_active \
    kiseki_storage_device_used_bytes \
    kiseki_storage_device_total_bytes \
    kiseki_storage_logical_bytes \
    kiseki_storage_physical_bytes \
    kiseki_storage_chunk_count \
    kiseki_storage_tier_fast_used_bytes \
    kiseki_storage_tier_fast_total_bytes \
    kiseki_storage_tier_bulk_used_bytes \
    kiseki_storage_tier_cold_used_bytes; do
    line=$(grep "^${metric}[[:space:]{]" "$NODE_FILE" 2>/dev/null | head -1)
    [ -n "$line" ] && echo "  $line" | tee -a "$OUT"
  done
done

echo "" | tee -a "$OUT"
echo "Full per-node metrics → 90-metrics-<ip>.txt" | tee -a "$OUT"
echo "OK" | tee -a "$OUT"
