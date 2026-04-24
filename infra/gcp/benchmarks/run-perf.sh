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

gcloud_ssh() {
  gcloud compute ssh "$CTRL_NAME" $GC_ARGS --command="$1" 2>/dev/null
}

gcloud_scp_to() {
  gcloud compute scp "$1" "$CTRL_NAME:$2" $GC_ARGS 2>/dev/null
}

gcloud_scp_from() {
  gcloud compute scp "$CTRL_NAME:$1" "$2" $GC_ARGS 2>/dev/null
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
gcloud_scp_to "$SCRIPT_DIR/perf-suite.sh" "/opt/kiseki-bench/perf-suite.sh"
gcloud_scp_to "$SCRIPT_DIR/metrics-collector.sh" "/opt/kiseki-bench/metrics-collector.sh"
gcloud_ssh "chmod +x /opt/kiseki-bench/*.sh"
echo "  Scripts deployed to /opt/kiseki-bench/"

# ---------------------------------------------------------------------------
# 2. Check cluster health before starting
# ---------------------------------------------------------------------------
echo ""
echo "=== Pre-flight cluster health ==="
HEALTH=$(gcloud_ssh "
  for ip in 10.0.0.10 10.0.0.11 10.0.0.12 10.0.0.20 10.0.0.21; do
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
gcloud_ssh "nohup bash /opt/kiseki-bench/perf-suite.sh > /tmp/kiseki-perf.log 2>&1 & echo \$!"
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
  RUNNING=$(gcloud_ssh "pgrep -f perf-suite.sh || echo 'done'" 2>/dev/null || echo "error")

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
