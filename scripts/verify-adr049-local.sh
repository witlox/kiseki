#!/usr/bin/env bash
# Verify ADR-049 phase 5a-continued runtime tier-paths wiring against
# the local 3-node docker compose with bind-mounted fast-tier mounts.
#
# Assumes the cluster is already up via:
#   docker compose -f docker-compose.3node.yml \
#                  -f docker-compose.3node.adr049.yml up --build -d
#
# Checks (in order):
#   T1. Pointer file written on first boot
#   T2. Pointer paths resolve to /mnt/fast-{small,meta}/kiseki/<tier>
#   T3. fjall keyspace directories actually exist at the resolved paths
#   T4. Small-file PUT via S3 lands under /mnt/fast-small
#   T5. I-CP-Move trips when the pointer is tampered + node restarted
#
# Exits 0 on full success, 1 on first failure.

set -uo pipefail

TEST_ROOT=/tmp/kiseki-test
NODES=(node1 node2 node3)

# Maps node → host S3 port (from docker-compose.3node.yml port maps)
declare -A S3_PORT=(
  [node1]=9000
  [node2]=9010
  [node3]=9020
)
declare -A METRICS_PORT=(
  [node1]=9090
  [node2]=9091
  [node3]=9092
)

red()    { printf '\033[31m%s\033[0m\n' "$1" >&2; }
green()  { printf '\033[32m%s\033[0m\n' "$1"; }
yellow() { printf '\033[33m%s\033[0m\n' "$1"; }

step() { printf '\n=== %s ===\n' "$1"; }

fail() {
  red "FAIL: $1"
  exit 1
}

ok() { green "  PASS: $1"; }

# Wait for all three nodes' /cluster/info to be healthy (means resolver
# ran + fjall stores opened successfully).
wait_for_cluster() {
  step "waiting for cluster (up to 60 s)"
  for i in $(seq 1 60); do
    local ready=0
    for n in "${NODES[@]}"; do
      if curl -sf "http://127.0.0.1:${METRICS_PORT[$n]}/health" >/dev/null 2>&1; then
        ready=$((ready + 1))
      fi
    done
    if [ "$ready" -eq 3 ]; then
      ok "all 3 nodes healthy after ${i}s"
      return 0
    fi
    sleep 1
  done
  fail "cluster did not become healthy within 60s"
}

# T1: pointer file written on first boot
verify_pointer_files_exist() {
  step "T1: pointer file written"
  for n in "${NODES[@]}"; do
    local pointer="${TEST_ROOT}/${n}-data/kiseki-tier-paths.json"
    if [ ! -f "$pointer" ]; then
      fail "${n}: pointer file missing at ${pointer}"
    fi
    ok "${n}: ${pointer} exists"
  done
}

# T2: each tier resolves to the correct /mnt/fast-* mount
verify_pointer_paths() {
  step "T2: pointer paths point at /mnt/fast-{small,meta}/kiseki/<tier>"
  for n in "${NODES[@]}"; do
    local pointer="${TEST_ROOT}/${n}-data/kiseki-tier-paths.json"
    if ! command -v jq >/dev/null 2>&1; then
      yellow "  jq not installed — falling back to grep"
      grep -q '/mnt/fast-small/kiseki/small-object' "$pointer" \
        || fail "${n}: small_object not on /mnt/fast-small"
      grep -q '/mnt/fast-meta/kiseki/intent-store' "$pointer" \
        || fail "${n}: intent_store not on /mnt/fast-meta"
      grep -q '/mnt/fast-meta/kiseki/composition-meta' "$pointer" \
        || fail "${n}: composition_meta not on /mnt/fast-meta"
      grep -q '/mnt/fast-meta/kiseki/chunk-meta' "$pointer" \
        || fail "${n}: chunk_meta not on /mnt/fast-meta"
      ok "${n}: all 4 tiers on /mnt/fast-* (grep check)"
      continue
    fi
    local small intent compo chunk
    small=$(jq -r '.paths.small_object'      "$pointer")
    intent=$(jq -r '.paths.intent_store'     "$pointer")
    compo=$(jq -r '.paths.composition_meta'  "$pointer")
    chunk=$(jq -r '.paths.chunk_meta'        "$pointer")
    [[ "$small" == "/mnt/fast-small/kiseki/small-object" ]] \
      || fail "${n}: small_object=${small} expected /mnt/fast-small/kiseki/small-object"
    [[ "$intent" == "/mnt/fast-meta/kiseki/intent-store" ]] \
      || fail "${n}: intent_store=${intent} expected /mnt/fast-meta/kiseki/intent-store"
    [[ "$compo" == "/mnt/fast-meta/kiseki/composition-meta" ]] \
      || fail "${n}: composition_meta=${compo} expected /mnt/fast-meta/kiseki/composition-meta"
    [[ "$chunk" == "/mnt/fast-meta/kiseki/chunk-meta" ]] \
      || fail "${n}: chunk_meta=${chunk} expected /mnt/fast-meta/kiseki/chunk-meta"
    ok "${n}: small_object → ${small}"
    ok "${n}: intent_store → ${intent}"
    ok "${n}: composition_meta → ${compo}"
    ok "${n}: chunk_meta → ${chunk}"
  done
}

# T3: fjall keyspace directories exist (= the stores actually opened
# at the resolved paths, not the data_dir-relative fallbacks)
verify_keyspace_dirs() {
  step "T3: fjall keyspaces exist on /mnt/fast-{small,meta}"
  for n in "${NODES[@]}"; do
    local small="${TEST_ROOT}/${n}-small/kiseki/small-object"
    local meta_compo="${TEST_ROOT}/${n}-meta/kiseki/composition-meta"
    local meta_chunk="${TEST_ROOT}/${n}-meta/kiseki/chunk-meta"
    [ -d "$small" ]      || fail "${n}: SmallObjectStore dir missing at ${small}"
    [ -d "$meta_compo" ] || fail "${n}: CompositionMeta dir missing at ${meta_compo}"
    [ -d "$meta_chunk" ] || fail "${n}: ChunkMeta dir missing at ${meta_chunk}"
    ok "${n}: small-object/, composition-meta/, chunk-meta/ all present"
    # Also verify the FALLBACK paths are EMPTY — if these have content
    # the runtime is still opening at data_dir-relative paths (the bug
    # this whole feature fixes).
    if [ -d "${TEST_ROOT}/${n}-data/small/objects" ] && \
       [ -n "$(ls -A "${TEST_ROOT}/${n}-data/small/objects" 2>/dev/null)" ]; then
      fail "${n}: fallback /data/small/objects is non-empty — runtime opened at fallback"
    fi
    if [ -d "${TEST_ROOT}/${n}-data/metadata/compositions" ] && \
       [ -n "$(ls -A "${TEST_ROOT}/${n}-data/metadata/compositions" 2>/dev/null)" ]; then
      fail "${n}: fallback /data/metadata/compositions is non-empty — runtime opened at fallback"
    fi
    ok "${n}: fallback paths empty (resolver is wiring through)"
  done
}

# T4: PUT a small file via S3 and confirm it lands under fast-small
verify_small_put_lands_in_fast_small() {
  step "T4: S3 PUT lands under /mnt/fast-small/kiseki/small-object"
  local bucket="adr049smoke"
  local key="hello.txt"
  local payload="hello adr-049"
  # Use node1's S3 endpoint
  local s3="http://127.0.0.1:${S3_PORT[node1]}"
  # Snapshot small-object dir sizes BEFORE
  local before_size_n1
  before_size_n1=$(du -sb "${TEST_ROOT}/node1-small/kiseki/small-object" 2>/dev/null | awk '{print $1}')
  before_size_n1=${before_size_n1:-0}
  # Create bucket + PUT
  curl -s -X PUT "${s3}/${bucket}" -o /dev/null -w '%{http_code}\n' | grep -qE '^(200|409)$' \
    || fail "S3 bucket create failed"
  echo "${payload}" | curl -s -X PUT --data-binary @- "${s3}/${bucket}/${key}" -o /dev/null -w '%{http_code}\n' \
    | grep -qE '^(200|201|204)$' \
    || fail "S3 PUT failed"
  # Allow the gateway to flush
  sleep 2
  # Snapshot AFTER on ALL nodes (chunk may have been routed to any node)
  local grew=0
  for n in "${NODES[@]}"; do
    local before=0 after=0
    [ "$n" = "node1" ] && before=$before_size_n1
    after=$(du -sb "${TEST_ROOT}/${n}-small/kiseki/small-object" 2>/dev/null | awk '{print $1}')
    after=${after:-0}
    if [ "$after" -gt "$before" ]; then
      grew=$((grew + 1))
      ok "${n}: small-object/ grew ${before} → ${after} bytes"
    fi
  done
  if [ "$grew" -eq 0 ]; then
    fail "no node's small-object/ grew — PUT did not land on fast-small"
  fi
  ok "${grew} node(s) recorded the inline payload under /mnt/fast-small"
}

# T5: tamper with the pointer, restart node, expect I-CP-Move trip
verify_icp_move_guard() {
  step "T5: I-CP-Move guard trips on tampered pointer"
  local pointer="${TEST_ROOT}/node1-data/kiseki-tier-paths.json"
  # Bring node1 down cleanly
  docker compose -f docker-compose.3node.yml -f docker-compose.3node.adr049.yml \
    stop kiseki-node1 >/dev/null 2>&1
  # Tamper: rewrite pointer so small_object claims a different mount
  # while the actual keyspace still lives on /mnt/fast-small. Boot must
  # refuse rather than silently switching.
  cat > "$pointer" <<EOF
{
  "paths": {
    "small_object":     "/mnt/fast-different/kiseki/small-object",
    "intent_store":     "/mnt/fast-meta/kiseki/intent-store",
    "composition_meta": "/mnt/fast-meta/kiseki/composition-meta",
    "chunk_meta":       "/mnt/fast-meta/kiseki/chunk-meta"
  }
}
EOF
  ok "tampered pointer written (small_object now claims /mnt/fast-different)"
  # Restart node1 in foreground-ish mode and capture exit
  docker compose -f docker-compose.3node.yml -f docker-compose.3node.adr049.yml \
    start kiseki-node1 >/dev/null 2>&1
  # Wait for logs to contain the error OR the node to come up healthy
  local saw_error=0
  for _ in $(seq 1 30); do
    if docker compose -f docker-compose.3node.yml -f docker-compose.3node.adr049.yml \
         logs --tail=200 kiseki-node1 2>&1 | grep -qE 'I-CP-Move|PathVersionMismatch'; then
      saw_error=1
      break
    fi
    sleep 1
  done
  if [ "$saw_error" -eq 1 ]; then
    ok "I-CP-Move guard logged the path mismatch — refuse-to-open enforced"
  else
    yellow "  did not see I-CP-Move in logs; dumping recent node1 output:"
    docker compose -f docker-compose.3node.yml -f docker-compose.3node.adr049.yml \
      logs --tail=80 kiseki-node1 2>&1 | sed 's/^/    /'
    fail "I-CP-Move was expected to trip; it didn't"
  fi
  # Restore the pointer + restart so the cluster is healthy on exit
  yellow "  restoring pointer + restarting node1"
  rm -f "$pointer"
  docker compose -f docker-compose.3node.yml -f docker-compose.3node.adr049.yml \
    restart kiseki-node1 >/dev/null 2>&1
}

wait_for_cluster
verify_pointer_files_exist
verify_pointer_paths
verify_keyspace_dirs
verify_small_put_lands_in_fast_small
verify_icp_move_guard

green ""
green "All ADR-049 local checks passed."
