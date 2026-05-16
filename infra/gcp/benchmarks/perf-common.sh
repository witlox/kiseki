#!/bin/bash
# Shared helpers for kiseki perf suites (default / transport / gpu).
#
# Sourced by perf-suite.sh, perf-suite-transport.sh, perf-suite-gpu.sh.
# Provides: env load, ssh wrapper, leader discovery, metrics collector
# lifecycle, fio JSON parser, GCS upload, and the SUMMARY writer.
#
# Required env (loaded from /etc/kiseki-bench.env, written by setup-bench-ctrl.sh):
#   STORAGE_IPS, CLIENT_IPS, FIRST_STORAGE, KISEKI_PERF_BUCKET, SSH_USER, KISEKI_PROFILE

set -o pipefail

# ----------------------------------------------------------------------------
# Env load + derived globals
# ----------------------------------------------------------------------------

# Env file path is overridable for local unit tests (see
# tests/test_pick_helpers.sh). On the bench-ctrl VM the default
# /etc/kiseki-bench.env is what setup-bench-ctrl.sh writes.
#
# Local-mode fallback (#134): when /etc/kiseki-bench.env is absent,
# fabricate a single-node env pointing at 127.0.0.1. Lets phases
# run against a `docker compose -f docker-compose.3node.yml` cluster
# from the dev box without any operator setup.
KISEKI_BENCH_ENV="${KISEKI_BENCH_ENV:-/etc/kiseki-bench.env}"
if [ -f "$KISEKI_BENCH_ENV" ]; then
  # shellcheck disable=SC1090
  source "$KISEKI_BENCH_ENV"
else
  STORAGE_IPS="${STORAGE_IPS:-127.0.0.1}"
  CLIENT_IPS="${CLIENT_IPS:-127.0.0.1}"
  FIRST_STORAGE="${FIRST_STORAGE:-127.0.0.1}"
  KISEKI_PERF_BUCKET="${KISEKI_PERF_BUCKET:-}"
  SSH_USER="${SSH_USER:-$(whoami)}"
  KISEKI_PROFILE="${KISEKI_PROFILE:-local}"
fi

# Whitespace-separated forms (most code prefers these).
ALL_STORAGE=$(echo "$STORAGE_IPS" | tr ',' ' ')
CLIENTS_WS=$(echo "$CLIENT_IPS" | tr ',' ' ')
read -r -a CLIENT_ARRAY <<< "$CLIENTS_WS"
# Array form for round-robin fan-out — see pick_storage_for_client.
read -r -a STORAGE_IPS_ARRAY <<< "$ALL_STORAGE"

PAR=${KISEKI_BENCH_PAR:-8}
GCS_BUCKET="${KISEKI_PERF_BUCKET:-gs://kiseki-perf-results}"

# Closes #55: $RESULTS is sticky per session, not per source.
#
# Pre-fix RESULTS used `$(date +%Y%m%d-%H%M%S)` evaluated AT SOURCE
# TIME, so every phase script that sourced perf-common.sh got its own
# timestamped dir. The 2026-05-16 GCP run scattered results across
# 7+ /tmp/kiseki-perf-compact-* dirs.
#
# New shape: $KISEKI_RUN_ID is the source of truth. If unset (first
# source of the session), read from /tmp/kiseki-bench-runid; if the
# marker is missing, generate a fresh timestamp + write it. Every
# subsequent source inside the same session reads the same id, so all
# phases land under the same $RESULTS.
#
# The `bench` driver (infra/gcp/benchmarks/bench) creates a fresh
# run-id on entry by writing a new marker; operators can also pin one
# explicitly via `KISEKI_RUN_ID=<value> bash phases/<name>.sh`.
RUN_ID_MARKER="${KISEKI_RUN_ID_MARKER:-/tmp/kiseki-bench-runid}"
if [ -z "${KISEKI_RUN_ID:-}" ]; then
  if [ -f "$RUN_ID_MARKER" ]; then
    KISEKI_RUN_ID=$(cat "$RUN_ID_MARKER" 2>/dev/null || echo "")
  fi
  if [ -z "${KISEKI_RUN_ID:-}" ]; then
    KISEKI_RUN_ID=$(date +%Y%m%d-%H%M%S)
    echo "$KISEKI_RUN_ID" > "$RUN_ID_MARKER" 2>/dev/null || true
  fi
  export KISEKI_RUN_ID
fi
# Legacy compatibility: some downstream tooling still reads $RUN_TS.
RUN_TS="$KISEKI_RUN_ID"
RESULTS="/tmp/kiseki-perf-${KISEKI_PROFILE:-unknown}-${KISEKI_RUN_ID}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[1]:-$0}")" && pwd)"
mkdir -p "$RESULTS"

log() { echo "[$(date +%H:%M:%S)] $*" | tee -a "$RESULTS/perf.log"; }

# ----------------------------------------------------------------------------
# SSH wrapper — uses OS Login user + key registered by setup-bench-ctrl.sh
# ----------------------------------------------------------------------------

SSH_USER="${SSH_USER:-$(gcloud compute os-login describe-profile --format='value(posixAccounts[0].username)' 2>/dev/null || echo root)}"
SSH_KEY=""
[ -f /root/.ssh/id_ed25519 ] && SSH_KEY="-i /root/.ssh/id_ed25519"

node_ssh() {
  local host=$1; shift
  # OS Login service-account user needs sudo for mount/fio/kiseki-client.
  # Pipe the command via stdin to avoid quoting hell with multi-line
  # scripts that contain single quotes (python -c '...').
  ssh -o StrictHostKeyChecking=no -o ConnectTimeout=10 $SSH_KEY \
    "$SSH_USER@$host" "sudo bash -s" <<< "$*"
}

# ----------------------------------------------------------------------------
# Bench-mode helpers (#134) — phases run against either the running
# `docker compose -f docker-compose.3node.yml` (local mode) OR the GCP
# perf cluster (gcp mode). The dispatch helpers below keep phase
# scripts mode-agnostic.
# ----------------------------------------------------------------------------

# Resolve the operational mode. `auto` (default) detects by the
# presence of /etc/kiseki-bench.env (only setup-bench-ctrl.sh writes
# it). Operators can force a mode with `KISEKI_BENCH_MODE=local|gcp`.
bench_mode() {
  local mode="${KISEKI_BENCH_MODE:-auto}"
  if [ "$mode" = "auto" ]; then
    if [ -f /etc/kiseki-bench.env ]; then echo "gcp"; else echo "local"; fi
  else
    echo "$mode"
  fi
}

# Run a command on a client. In gcp mode → node_ssh into the client
# VM at `$CLIENT_ARRAY[idx]`. In local mode → run on the current host
# (the "client" is loopback). Command is taken from stdin so callers
# can pipe a heredoc without quoting hell.
client_run() {
  local idx=$1
  if [ "$(bench_mode)" = "gcp" ]; then
    local cip="${CLIENT_ARRAY[$idx]}"
    # node_ssh reads from stdin; just pass it through.
    ssh -o StrictHostKeyChecking=no -o ConnectTimeout=10 $SSH_KEY \
      "$SSH_USER@$cip" "sudo bash -s"
  else
    # Local mode: stdin → bash. Same shape as node_ssh's redirected
    # stdin so phase scripts work unmodified.
    bash -s
  fi
}

# Resolve the leader's S3 / NFS / native endpoints as URLs the
# bench/curl/mount commands can dial directly. In gcp mode these are
# the leader's internal-network IPs (10.0.0.X); in local mode they
# default to 127.0.0.1 with the docker compose port mapping.
leader_endpoints() {
  local mode
  mode=$(bench_mode)
  if [ "$mode" = "gcp" ]; then
    LEADER_S3_URL="${LEADER_S3:-http://$LEADER_HOST:9000}"
    LEADER_NFS_HOSTPORT="$LEADER_HOST:2049"
    LEADER_NATIVE_URL="kiseki://$LEADER_HOST:9103"
    LEADER_METRICS_URL="http://$LEADER_HOST:9090"
  else
    # docker-compose.3node.yml maps node1's 9000/2049/9103/9090 to
    # the host's same ports. discover_leader's LEADER_HOST is the
    # container's internal hostname (kiseki-node1) which doesn't
    # resolve from the host network, so we override.
    LEADER_S3_URL="${KISEKI_LOCAL_S3_URL:-http://127.0.0.1:9000}"
    LEADER_NFS_HOSTPORT="${KISEKI_LOCAL_NFS_HOSTPORT:-127.0.0.1:2049}"
    LEADER_NATIVE_URL="${KISEKI_LOCAL_NATIVE_URL:-kiseki://127.0.0.1:9103}"
    LEADER_METRICS_URL="${KISEKI_LOCAL_METRICS_URL:-http://127.0.0.1:9090}"
  fi
}

# ----------------------------------------------------------------------------
# Write fan-out helpers — spread bench load across the storage IPs and
# bench-managed namespaces (Step B of the write-routing posture plan;
# see specs/implementation/write-routing-posture.md).
#
# Today's perf-suite phase 9 funnels every PUT to $LEADER_S3, which
# masks ADR-033 §1's per-shard leader spread. The two helpers below
# let phase 9 (and the new phase 9b) round-robin clients across
# different ingest gateways and different namespaces — the two
# effects compound: different namespace = different shard set =
# different shard leaders; different node = different gateway
# process (S3-server -> chunk-store -> per-shard Raft client).
# ----------------------------------------------------------------------------

pick_storage_for_client() {
  local idx=$1
  local n=${#STORAGE_IPS_ARRAY[@]}
  echo "${STORAGE_IPS_ARRAY[$((idx % n))]}"
}

pick_namespace_for_client() {
  local idx=$1
  local n=${#STORAGE_IPS_ARRAY[@]}
  echo "perf-agg-ns$((idx % n))"
}

# ----------------------------------------------------------------------------
# Cluster health + Raft leader discovery
# ----------------------------------------------------------------------------

discover_leader() {
  LEADER_S3=""
  LEADER_ID=""
  for ip in $ALL_STORAGE; do
    local status
    status=$(curl -sf "http://$ip:9090/health" 2>/dev/null || echo "DOWN")
    log "  $ip: $status"
    if [ -z "$LEADER_S3" ]; then
      local info
      info=$(curl -sf "http://$ip:9090/cluster/info" 2>/dev/null || echo "{}")
      local cand cand_id
      cand=$(echo "$info"   | python3 -c "import sys,json; d=json.load(sys.stdin); print(d.get('leader_s3',''))" 2>/dev/null || echo "")
      cand_id=$(echo "$info" | python3 -c "import sys,json; d=json.load(sys.stdin); l=d.get('leader_id'); print(l if l else '')" 2>/dev/null || echo "")
      if [ -n "$cand" ]; then
        LEADER_S3="http://$cand"
        LEADER_ID="$cand_id"
      fi
    fi
  done
  if [ -z "$LEADER_S3" ]; then
    log "  WARNING: no Raft leader found — falling back to FIRST_STORAGE"
    LEADER_S3="http://$FIRST_STORAGE:9000"
    LEADER_ID="unknown"
  fi
  LEADER_HOST=$(echo "$LEADER_S3" | sed 's|http://||; s|:.*||')
  LEADER_NFS_HOST="$LEADER_HOST"
  {
    echo "leader_id=$LEADER_ID"
    echo "leader_s3=$LEADER_S3"
    echo "leader_host=$LEADER_HOST"
    echo "timestamp=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  } > "$RESULTS/cluster-info.txt"
}

# ----------------------------------------------------------------------------
# Background metrics collector lifecycle
# ----------------------------------------------------------------------------

start_metrics() {
  log "Starting metrics collector (10s interval)"
  bash "$SCRIPT_DIR/metrics-collector.sh" "$RESULTS" </dev/null \
    >"$RESULTS/collector.log" 2>&1 &
  COLLECTOR_PID=$!
}

stop_metrics() {
  if [ -n "${COLLECTOR_PID:-}" ]; then
    log "Stopping metrics collector (pid=$COLLECTOR_PID)"
    kill "$COLLECTOR_PID" 2>/dev/null
    wait "$COLLECTOR_PID" 2>/dev/null || true
  fi
  bash "$SCRIPT_DIR/metrics-collector.sh" --summarize "$RESULTS" 2>/dev/null || true
}

# ----------------------------------------------------------------------------
# fio JSON parser — extracts MB/s for a given direction (read|write)
# ----------------------------------------------------------------------------

fio_mbps() {
  local dir=$1
  python3 -c "
import sys, json
try:
    d = json.load(sys.stdin)
    bw_kib = d['jobs'][0]['$dir']['bw']
    print(f'{bw_kib/1024:.1f}')
except Exception as e:
    print('parse-error', file=sys.stderr)
    sys.exit(1)
"
}

fio_iops() {
  local dir=$1
  python3 -c "
import sys, json
try:
    d = json.load(sys.stdin)
    iops = d['jobs'][0]['$dir']['iops']
    lat_us = d['jobs'][0]['$dir']['lat_ns']['mean'] / 1000
    print(f'{iops:.0f} {lat_us:.0f}')
except Exception:
    print('parse-error', file=sys.stderr)
    sys.exit(1)
"
}

# ----------------------------------------------------------------------------
# Bandwidth-vs-baseline ratio (Gbps observed / Gbps expected)
# ----------------------------------------------------------------------------

ratio_pct() {
  python3 -c "obs=float('$1'); base=float('$2'); print(f'{obs/base*100:.0f}%' if base>0 else 'N/A')"
}

# ----------------------------------------------------------------------------
# Summary writer + GCS upload — call from each suite's trap EXIT
# ----------------------------------------------------------------------------

write_summary() {
  local title="$1"; shift
  local files=("$@")
  {
    echo "=== KISEKI ${title} RESULTS ==="
    echo "Profile: ${KISEKI_PROFILE:-unknown}"
    echo "Date: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "Results dir: $RESULTS"
    echo "Parallelism: $PAR"
    echo ""
    for f in "${files[@]}"; do
      if [ -f "$RESULTS/$f.txt" ]; then
        echo "--- $f ---"
        cat "$RESULTS/$f.txt"
        echo ""
      fi
    done
    if [ -f "$RESULTS/metrics-summary.txt" ]; then
      echo "--- metrics ---"
      cat "$RESULTS/metrics-summary.txt"
    fi
  } > "$RESULTS/SUMMARY.txt"
  log ""
  log "=== SUMMARY ==="
  cat "$RESULTS/SUMMARY.txt"
}

upload_results() {
  if command -v gsutil &>/dev/null; then
    local run_id
    run_id=$(basename "$RESULTS")
    log "Uploading results to $GCS_BUCKET/$run_id/"
    gsutil -m cp -r "$RESULTS" "$GCS_BUCKET/$run_id/" 2>/dev/null && \
      log "Upload complete: $GCS_BUCKET/$run_id/" || \
      log "GCS upload failed (results still at $RESULTS)"
  else
    log "gsutil not found — results only at $RESULTS"
  fi
  echo "$RESULTS" > /tmp/kiseki-perf-latest
}
