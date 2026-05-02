#!/bin/bash
# Local wrapper: deploy, run, and collect performance benchmark results.
#
# Usage: ./infra/gcp/benchmarks/run-perf.sh [--zone ZONE] [--project PROJECT]
#
# Deploys perf-suite.sh + metrics-collector.sh to the ctrl node,
# runs via nohup, polls for progress, downloads results when done.
set -eo pipefail

ZONE="${KISEKI_GCP_ZONE:-europe-west6-a}"
PROJECT="${KISEKI_GCP_PROJECT:-}"
CTRL_NAME="kiseki-ctrl"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
LOCAL_RESULTS="$SCRIPT_DIR/results/$(date +%Y%m%d-%H%M%S)"

# Parse args
while [ $# -gt 0 ]; do
  case "$1" in
    --zone) ZONE="$2"; shift 2 ;;
    --project) PROJECT="$2"; shift 2 ;;
    *) echo "Unknown arg: $1"; exit 1 ;;
  esac
done

GC_ARGS="--zone=$ZONE"
[ -n "$PROJECT" ] && GC_ARGS="$GC_ARGS --project=$PROJECT"

# Optional: KISEKI_SSH_KEY_FILE / KISEKI_SSH_FLAG — let the caller
# supply a non-default SSH identity (e.g. a per-test ephemeral key
# under .gcp-build/) and override OpenSSH's system config (useful
# when /etc/ssh/ssh_config has a permission-mismatched include that
# would otherwise abort every gcloud-driven SSH call).
[ -n "${KISEKI_SSH_KEY_FILE:-}" ] && GC_ARGS="$GC_ARGS --ssh-key-file=$KISEKI_SSH_KEY_FILE"
# `--ssh-flag` / `--scp-flag` each accept a single ssh argument; use
# an env list with `;` separators to pass multiple (e.g.
# KISEKI_SSH_FLAGS="-F;/dev/null"). Build separate per-tool arg
# strings since `gcloud compute ssh` and `gcloud compute scp` reject
# each other's flag names.
SSH_FLAG_ARGS=""
SCP_FLAG_ARGS=""
if [ -n "${KISEKI_SSH_FLAGS:-}" ]; then
  IFS=';' read -ra _flags <<< "$KISEKI_SSH_FLAGS"
  for f in "${_flags[@]}"; do
    SSH_FLAG_ARGS="$SSH_FLAG_ARGS --ssh-flag=$f"
    SCP_FLAG_ARGS="$SCP_FLAG_ARGS --scp-flag=$f"
  done
fi

gcloud_ssh() {
  gcloud compute ssh "$CTRL_NAME" $GC_ARGS $SSH_FLAG_ARGS --command="$1" 2>/dev/null
}

gcloud_scp_to() {
  gcloud compute scp "$1" "$CTRL_NAME:$2" $GC_ARGS $SCP_FLAG_ARGS 2>/dev/null
}

gcloud_scp_from() {
  gcloud compute scp "$CTRL_NAME:$1" "$2" $GC_ARGS $SCP_FLAG_ARGS 2>/dev/null
}

echo "╔═══════════════════════════════════════════════════════════════╗"
echo "║       Kiseki Performance Test Runner                         ║"
echo "╠═══════════════════════════════════════════════════════════════╣"
echo "║ Ctrl node: $CTRL_NAME ($ZONE)"
echo "║ Local results: $LOCAL_RESULTS"
echo "╚═══════════════════════════════════════════════════════════════╝"

# ---------------------------------------------------------------------------
# 1. Deploy scripts to ctrl node
# ---------------------------------------------------------------------------
echo ""
echo "=== Deploying benchmark scripts ==="
gcloud_ssh "sudo mkdir -p /opt/kiseki-bench && sudo chmod 777 /opt/kiseki-bench"
# Upload all suites + the shared helpers; the ctrl picks based on
# KISEKI_BENCH_SUITE in /etc/kiseki-bench.env (set by setup-bench-ctrl.sh,
# which gets it from var.profile in the Terraform).
for f in perf-common.sh perf-suite.sh perf-suite-transport.sh perf-suite-gpu.sh metrics-collector.sh; do
  gcloud_scp_to "$SCRIPT_DIR/$f" "/opt/kiseki-bench/$f"
done
gcloud_ssh "chmod +x /opt/kiseki-bench/*.sh"
echo "  Scripts deployed to /opt/kiseki-bench/"

# Resolve which suite this profile runs.
BENCH_SUITE=$(gcloud_ssh "grep -oP 'KISEKI_BENCH_SUITE=\"\\K[^\"]+' /etc/kiseki-bench.env 2>/dev/null" | head -1)
BENCH_SUITE="${BENCH_SUITE:-perf-suite.sh}"
echo "  Suite for this profile: $BENCH_SUITE"

# ---------------------------------------------------------------------------
# 2. Check cluster health before starting
# ---------------------------------------------------------------------------
echo ""
echo "=== Pre-flight cluster health ==="
HEALTH=$(gcloud_ssh "
  source /etc/kiseki-bench.env 2>/dev/null
  for ip in \$(echo \"\$STORAGE_IPS\" | tr ',' ' '); do
    STATUS=\$(curl -sf http://\$ip:9090/health 2>/dev/null || echo 'DOWN')
    echo \"  \$ip: \$STATUS\"
  done
")
echo "$HEALTH"

DOWN_COUNT=$(echo "$HEALTH" | grep -c "DOWN" || true)
if [ "$DOWN_COUNT" -gt 0 ]; then
  echo ""
  echo "WARNING: $DOWN_COUNT node(s) are DOWN. Continue anyway? (y/N)"
  read -r REPLY
  [ "$REPLY" != "y" ] && { echo "Aborted."; exit 1; }
fi

# ---------------------------------------------------------------------------
# 3. Launch benchmark (nohup, background)
# ---------------------------------------------------------------------------
echo ""
echo "=== Launching benchmark ==="
gcloud_ssh "sudo nohup bash /opt/kiseki-bench/$BENCH_SUITE > /tmp/kiseki-perf.log 2>&1 & echo \$!"
echo "  Benchmark running on ctrl node (output: /tmp/kiseki-perf.log)"

# ---------------------------------------------------------------------------
# 4. Poll for progress
# ---------------------------------------------------------------------------
echo ""
echo "=== Polling for progress (every 60s) ==="
echo "  Press Ctrl+C to stop polling (benchmark continues on VM)"
echo ""

LAST_LINES=0
while true; do
  sleep 60

  # Check if still running
  RUNNING=$(gcloud_ssh "pgrep -f $BENCH_SUITE || echo 'done'" 2>/dev/null || echo "error")

  # Get latest output
  CURRENT=$(gcloud_ssh "wc -l < /tmp/kiseki-perf.log 2>/dev/null || echo 0" 2>/dev/null || echo "0")
  if [ "$CURRENT" -gt "$LAST_LINES" ]; then
    NEW_OFFSET=$((LAST_LINES + 1))
    gcloud_ssh "tail -n +$NEW_OFFSET /tmp/kiseki-perf.log" 2>/dev/null || true
    LAST_LINES="$CURRENT"
  fi

  if echo "$RUNNING" | grep -q "done"; then
    echo ""
    echo "=== Benchmark complete ==="
    break
  fi
done

# ---------------------------------------------------------------------------
# 5. Download results
# ---------------------------------------------------------------------------
echo ""
echo "=== Downloading results ==="
mkdir -p "$LOCAL_RESULTS"

# Get results directory path
REMOTE_RESULTS=$(gcloud_ssh "cat /tmp/kiseki-perf-latest 2>/dev/null" 2>/dev/null || echo "")
if [ -z "$REMOTE_RESULTS" ]; then
  echo "  Could not determine remote results path"
  echo "  Downloading full log instead"
  gcloud_scp_from "/tmp/kiseki-perf.log" "$LOCAL_RESULTS/perf.log"
else
  echo "  Remote: $REMOTE_RESULTS"
  # Download SUMMARY.txt and key files
  for f in SUMMARY.txt cluster-info.txt s3-write.txt s3-parallel-write.txt s3-latency.txt s3-read.txt nfs-write.txt pnfs.txt fuse.txt bandwidth.txt metrics-summary.txt perf.log; do
    gcloud_scp_from "$REMOTE_RESULTS/$f" "$LOCAL_RESULTS/$f" 2>/dev/null || true
  done
  # Download metrics snapshots
  mkdir -p "$LOCAL_RESULTS/metrics"
  gcloud_scp_from "$REMOTE_RESULTS/metrics/*" "$LOCAL_RESULTS/metrics/" 2>/dev/null || true
fi

echo ""
echo "╔═══════════════════════════════════════════════════════════════╗"
echo "║ Results downloaded to: $LOCAL_RESULTS"
echo "╚═══════════════════════════════════════════════════════════════╝"
echo ""

if [ -f "$LOCAL_RESULTS/SUMMARY.txt" ]; then
  cat "$LOCAL_RESULTS/SUMMARY.txt"
fi
