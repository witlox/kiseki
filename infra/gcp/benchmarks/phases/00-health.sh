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

# ----------------------------------------------------------------------------
# Topology probe (#69): assert each bench-ns-<i> has the expected
# shard count. Without this, a missing/failed `setup-shards.sh` run
# would silently produce a single-shard cluster and PR #67's
# `--namespace-fanout` would deliver no fanout — exactly the
# illusion-of-fanout shape #66's investigation comment called out.
#
# Skipped when KISEKI_BENCH_NAMESPACE_FANOUT=1 (the "I'm explicitly
# benching per-shard ceiling, no fanout expected" mode used by
# phases 10 and 30).
# ----------------------------------------------------------------------------

STORAGE_COUNT=$(echo "$STORAGE_IPS" | tr ',' '\n' | grep -c .)
EXPECTED_FANOUT="${KISEKI_BENCH_NAMESPACE_FANOUT:-$STORAGE_COUNT}"
EXPECTED_SHARDS_PER_NS="${KISEKI_BENCH_SHARDS_PER_NS:-$STORAGE_COUNT}"

if [ "$EXPECTED_FANOUT" -gt 1 ]; then
  echo "" | tee -a "$OUT"
  echo "Topology probe: expecting $EXPECTED_FANOUT bench-ns-<i> namespaces, $EXPECTED_SHARDS_PER_NS shard(s) each" | tee -a "$OUT"

  topo_json=$(curl -sf --max-time 5 "http://$LEADER_HOST:9090/admin/topology/shards" 2>/dev/null || echo "{}")
  missing=0
  short=0
  for i in $(seq 0 $((EXPECTED_FANOUT - 1))); do
    ns_id="bench-ns-${i}"
    found=$(echo "$topo_json" | python3 -c "
import sys, json
try:
    d = json.load(sys.stdin)
    ns = '$ns_id'
    n = sum(1 for s in d.get('shards', []) if s.get('namespace_id') == ns)
    print(n)
except Exception:
    print(0)
" 2>/dev/null || echo 0)
    if [ "$found" -eq 0 ]; then
      echo "  $ns_id → MISSING (no shards)" | tee -a "$OUT"
      missing=$((missing + 1))
    elif [ "$found" -lt "$EXPECTED_SHARDS_PER_NS" ]; then
      echo "  $ns_id → shard_count=$found < expected $EXPECTED_SHARDS_PER_NS" | tee -a "$OUT"
      short=$((short + 1))
    else
      echo "  $ns_id → shard_count=$found ✓" | tee -a "$OUT"
    fi
  done

  if [ "$missing" -gt 0 ] || [ "$short" -gt 0 ]; then
    cat <<HALT | tee -a "$OUT"

HALT: bench namespace topology incomplete ($missing missing, $short under-sharded).
      Run setup-shards.sh on the bench-ctrl host:

        bash /opt/kiseki-bench/setup-shards.sh

      Or set KISEKI_BENCH_NAMESPACE_FANOUT=1 to skip this probe and
      measure single-shard ceiling intentionally.
HALT
    exit 2
  fi
fi

echo "OK" | tee -a "$OUT"
