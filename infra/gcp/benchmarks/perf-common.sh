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

source /etc/kiseki-bench.env 2>/dev/null || {
  echo "ERROR: /etc/kiseki-bench.env missing — was this script run from bench-ctrl?" >&2
  exit 1
}

# Whitespace-separated forms (most code prefers these).
ALL_STORAGE=$(echo "$STORAGE_IPS" | tr ',' ' ')
CLIENTS_WS=$(echo "$CLIENT_IPS" | tr ',' ' ')
read -r -a CLIENT_ARRAY <<< "$CLIENTS_WS"

PAR=${KISEKI_BENCH_PAR:-8}
GCS_BUCKET="${KISEKI_PERF_BUCKET:-gs://kiseki-perf-results}"

RUN_TS=$(date +%Y%m%d-%H%M%S)
RESULTS="/tmp/kiseki-perf-${KISEKI_PROFILE:-unknown}-${RUN_TS}"
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
