#!/bin/bash
# Phase 00 — cluster health + leader discovery.
#
# Mode-agnostic: works against either the local 3-node compose
# (KISEKI_BENCH_MODE=local) or the GCP perf cluster (default).
#
# Exit 0 — leader found, all storage nodes report a consistent topology.
# Exit 2 — cluster is unhealthy / split-brain / no leader (functional break).

set -uo pipefail
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
BENCH_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
# shellcheck disable=SC1091
source "$BENCH_DIR/perf-common.sh"

MODE=$(bench_mode)
OUT="$RESULTS/00-health.txt"
{
  echo "=== Phase 00: health + leader discovery ==="
  echo "mode=$MODE"
  echo "storage_ips=$ALL_STORAGE"
  echo "client_array=${CLIENT_ARRAY[*]}"
  echo ""
} | tee "$OUT"

# discover_leader sets LEADER_ID / LEADER_S3 / LEADER_HOST. The pipe
# form `discover_leader | tee` forks discover_leader into a subshell
# and the assignments don't propagate — use process substitution so
# the function runs in the current shell.
discover_leader > >(tee -a "$OUT") 2>&1
wait  # ensure the process-sub tee has flushed before reading $OUT

if [ "${LEADER_ID:-}" = "unknown" ] || [ -z "${LEADER_ID:-}" ]; then
  echo "HALT: no Raft leader found across $ALL_STORAGE" | tee -a "$OUT"
  exit 2
fi

leader_endpoints

# Cross-check: every node should agree on the same leader_id.
declare -A SEEN_LEADERS=()
ALL_AGREE=1
for ip in $ALL_STORAGE; do
  l=$(curl -sf --max-time 3 "http://$ip:9090/cluster/info" 2>/dev/null \
    | python3 -c "import sys,json;d=json.load(sys.stdin);print(d.get('leader_id',''))" 2>/dev/null || echo "?")
  SEEN_LEADERS["$l"]=1
  echo "  $ip → leader_id=$l" | tee -a "$OUT"
done
if [ "${#SEEN_LEADERS[@]}" -ne 1 ]; then
  echo "HALT: nodes disagree on leader_id (split-brain or transient election)" | tee -a "$OUT"
  exit 2
fi

cat <<RESOLVED | tee -a "$OUT"

Resolved leader endpoints:
  LEADER_HOST       = $LEADER_HOST
  LEADER_S3_URL     = $LEADER_S3_URL
  LEADER_NFS_HOSTPORT = $LEADER_NFS_HOSTPORT
  LEADER_NATIVE_URL = $LEADER_NATIVE_URL
RESOLVED

# GH #115 regression guard: each node must use its full provisioned
# chunk device (raw NVMe), not the silent 4 GiB file-backed fallback.
# `kiseki_storage_device_total_bytes` reports the node's chunk-pool
# capacity. Anything below KISEKI_MIN_DEVICE_GB (default 32 GiB) means
# `KISEKI_RAW_DEVICES` was not wired and the node fell back to the small
# file — the exact failure the May-2026 matrix hit. HALT so the run
# doesn't silently measure a 4 GiB-capped cluster.
MIN_DEVICE_GB="${KISEKI_MIN_DEVICE_GB:-32}"
echo "" | tee -a "$OUT"
echo "Chunk-store capacity per node (GH #115 guard, floor ${MIN_DEVICE_GB} GiB):" | tee -a "$OUT"
CAP_OK=1
for ip in $ALL_STORAGE; do
  total=$(curl -sf --max-time 3 "http://$ip:9090/metrics" 2>/dev/null \
    | grep '^kiseki_storage_device_total_bytes ' | awk '{print $2}' | head -1)
  used=$(curl -sf --max-time 3 "http://$ip:9090/metrics" 2>/dev/null \
    | grep '^kiseki_storage_device_used_bytes ' | awk '{print $2}' | head -1)
  total="${total:-0}"; used="${used:-0}"
  gib=$(awk -v b="$total" 'BEGIN{printf "%.1f", b/1073741824}')
  ugib=$(awk -v b="$used" 'BEGIN{printf "%.2f", b/1073741824}')
  echo "  $ip → total=${gib} GiB used=${ugib} GiB" | tee -a "$OUT"
  below=$(awk -v t="$total" -v m="$MIN_DEVICE_GB" 'BEGIN{print (t < m*1073741824) ? 1 : 0}')
  [ "$below" = "1" ] && CAP_OK=0
done
if [ "$CAP_OK" != "1" ]; then
  echo "HALT (GH #115): a node's chunk device is below ${MIN_DEVICE_GB} GiB — KISEKI_RAW_DEVICES not wired (4 GiB file fallback). Fix device wiring before measuring." | tee -a "$OUT"
  exit 2
fi

# C-4 (#212 A/B provenance): record which durability arm the storage
# nodes booted with. setup-raw-storage.sh's unit reads the optional
# /etc/kiseki/perf-arm.env (EnvironmentFile=-); absence = post-#217
# group-commit defaults. Without this label in the results dir, two
# A/B arms are indistinguishable after the fact.
echo "" | tee -a "$OUT"
echo "Perf arm (storage-1 /etc/kiseki/perf-arm.env):" | tee -a "$OUT"
if [ "$MODE" = "gcp" ]; then
  node_ssh "$FIRST_STORAGE" "cat /etc/kiseki/perf-arm.env 2>/dev/null || echo ARM-DEFAULT" 2>/dev/null \
    | sed 's/^/  /' | tee -a "$OUT"
else
  echo "  ARM-DEFAULT (local mode — no perf-arm.env)" | tee -a "$OUT"
fi

echo "OK" | tee -a "$OUT"
