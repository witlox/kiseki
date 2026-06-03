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

# Container names — docker compose project is the repo dir basename
# ("kiseki") so the pattern is kiseki-kiseki-node{1,2,3}-1.
declare -A CONTAINER=(
  [node1]=kiseki-kiseki-node1-1
  [node2]=kiseki-kiseki-node2-1
  [node3]=kiseki-kiseki-node3-1
)

# kiseki writes pointer file + fjall data as root with 0600 / 0700,
# so the verify checks all go through `docker exec` rather than
# the host bind paths (the host user can't read root-owned tmpfs).
exec_in() { docker exec "${CONTAINER[$1]}" "${@:2}"; }

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
    if ! exec_in "$n" test -f /data/kiseki-tier-paths.json; then
      fail "${n}: pointer file missing at /data/kiseki-tier-paths.json"
    fi
    ok "${n}: /data/kiseki-tier-paths.json exists"
  done
}

# T2: each tier resolves to the correct /mnt/fast-* mount
verify_pointer_paths() {
  step "T2: pointer paths point at /mnt/fast-{small,meta}/kiseki/<tier>"
  for n in "${NODES[@]}"; do
    local pointer_json
    pointer_json=$(exec_in "$n" cat /data/kiseki-tier-paths.json)
    for expected in \
        '/mnt/fast-small/kiseki/small-object' \
        '/mnt/fast-meta/kiseki/intent-store' \
        '/mnt/fast-meta/kiseki/composition-meta' \
        '/mnt/fast-meta/kiseki/chunk-meta'; do
      if ! grep -q "$expected" <<< "$pointer_json"; then
        red "  $n: pointer file content:"
        echo "$pointer_json" | sed 's/^/    /'
        fail "${n}: expected path ${expected} not in pointer"
      fi
    done
    ok "${n}: all 4 tiers on /mnt/fast-{small,meta}"
  done
}

# T3: fjall keyspace directories exist (= the stores actually opened
# at the resolved paths, not the data_dir-relative fallbacks)
verify_keyspace_dirs() {
  step "T3: fjall keyspaces exist on /mnt/fast-{small,meta}"
  for n in "${NODES[@]}"; do
    for path in \
        /mnt/fast-small/kiseki/small-object \
        /mnt/fast-meta/kiseki/composition-meta \
        /mnt/fast-meta/kiseki/chunk-meta; do
      if ! exec_in "$n" test -d "$path"; then
        fail "${n}: fjall keyspace dir missing at ${path}"
      fi
    done
    ok "${n}: small-object/, composition-meta/, chunk-meta/ all present"
    # Verify the FALLBACK paths are EMPTY — if these have content the
    # runtime is still opening at data_dir-relative paths (the bug this
    # whole feature fixes).
    local count
    count=$(exec_in "$n" sh -c 'ls /data/small/objects 2>/dev/null | wc -l')
    if [ "${count:-0}" -gt 0 ]; then
      fail "${n}: fallback /data/small/objects non-empty (${count} entries) — runtime opened at fallback"
    fi
    count=$(exec_in "$n" sh -c 'ls /data/metadata/compositions 2>/dev/null | wc -l')
    if [ "${count:-0}" -gt 0 ]; then
      fail "${n}: fallback /data/metadata/compositions non-empty — runtime opened at fallback"
    fi
    ok "${n}: fallback paths empty (resolver wired through)"
  done
}

# T4: PUT a small file via S3 and confirm it lands under fast-small
verify_small_put_lands_in_fast_small() {
  step "T4: S3 PUT lands under /mnt/fast-small/kiseki/small-object"
  local bucket="adr049smoke"
  local key="hello-$(date +%s).txt"
  local payload="hello adr-049"
  local s3="http://127.0.0.1:${S3_PORT[node1]}"
  # fjall preallocates a 64 MiB journal so `du` can't detect a small
  # write. Track the mtime of the journal file instead — a PUT advances
  # it. (POSIX `stat -c %Y` returns seconds since epoch.)
  declare -A before
  for n in "${NODES[@]}"; do
    before[$n]=$(exec_in "$n" sh -c 'stat -c %Y /mnt/fast-small/kiseki/small-object/0.jnl 2>/dev/null' || echo 0)
    before[$n]=${before[$n]:-0}
  done
  # Create bucket (409 = already exists is fine on a re-run)
  local bucket_code
  bucket_code=$(curl -s -o /dev/null -w '%{http_code}' -X PUT "${s3}/${bucket}")
  case "$bucket_code" in
    200|409) ;;
    *) fail "S3 bucket create returned ${bucket_code}" ;;
  esac
  local put_code
  put_code=$(echo "${payload}" | curl -s -o /dev/null -w '%{http_code}' -X PUT --data-binary @- "${s3}/${bucket}/${key}")
  case "$put_code" in
    200|201|204) ;;
    *) fail "S3 PUT returned ${put_code}" ;;
  esac
  # Allow the gateway + raft + fjall flush
  sleep 3
  local grew=0
  for n in "${NODES[@]}"; do
    local after
    after=$(exec_in "$n" sh -c 'stat -c %Y /mnt/fast-small/kiseki/small-object/0.jnl 2>/dev/null' || echo 0)
    after=${after:-0}
    if [ "$after" -gt "${before[$n]}" ]; then
      grew=$((grew + 1))
      ok "${n}: small-object journal mtime advanced ${before[$n]} → ${after}"
    fi
  done
  if [ "$grew" -eq 0 ]; then
    fail "no node's small-object journal advanced — inline PUT did not write fast-small"
  fi
  ok "${grew} node(s) wrote to /mnt/fast-small/kiseki/small-object"
}

# T5: tamper with the pointer, restart node, expect I-CP-Move trip
verify_icp_move_guard() {
  step "T5: I-CP-Move guard trips on tampered pointer"
  # Bring node1 down cleanly
  docker compose -f docker-compose.3node.yml -f docker-compose.3node.adr049.yml \
    stop kiseki-node1 >/dev/null 2>&1
  # Tamper: rewrite pointer so small_object claims a different mount.
  # We use docker run with the bind volume so the write happens as root
  # (the host user can't write 0600 root-owned files).
  docker run --rm -v /tmp/kiseki-test/node1-data:/data alpine:latest sh -c '
    cat > /data/kiseki-tier-paths.json <<EOF
{
  "paths": {
    "small_object":     "/mnt/fast-different/kiseki/small-object",
    "intent_store":     "/mnt/fast-meta/kiseki/intent-store",
    "composition_meta": "/mnt/fast-meta/kiseki/composition-meta",
    "chunk_meta":       "/mnt/fast-meta/kiseki/chunk-meta"
  }
}
EOF
    chmod 0600 /data/kiseki-tier-paths.json
  '
  ok "tampered pointer written (small_object now claims /mnt/fast-different)"
  # Restart node1 — it should refuse to start
  docker compose -f docker-compose.3node.yml -f docker-compose.3node.adr049.yml \
    start kiseki-node1 >/dev/null 2>&1
  # Wait for logs to contain the error
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
  # Restore: delete the pointer file so first-boot resolve regenerates it
  yellow "  restoring pointer + restarting node1"
  docker run --rm -v /tmp/kiseki-test/node1-data:/data alpine:latest \
    rm -f /data/kiseki-tier-paths.json
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
