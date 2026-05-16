#!/usr/bin/env bash
# Build + upload the benchmarks tarball that setup-bench-ctrl.sh
# fetches at boot. Mirrors the kiseki-{server,client}-x86_64.tar.gz
# upload pattern so operators have one consistent workflow.
#
# Usage (from repo root):
#   bash infra/gcp/scripts/stage-benchmarks.sh
#
# Reads bucket from $REPO_ROOT/.gcp-build/bucket.env (BUCKET=…) —
# same file the binary upload uses. Writes the tarball to
# .gcp-build/dist/benchmarks.tar.gz and uploads to
# gs://$BUCKET/benchmarks.tar.gz.
#
# Closes #54 (operator workflow): the setup-bench-ctrl.sh boot script
# now expects this tarball at ${binary_url_base}/benchmarks.tar.gz.
# Without it the cluster boots with an empty /opt/kiseki-bench/ and
# the operator has to ssh-tee scripts up — what bit us in the
# 2026-05-16 GCP run.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# This script lives at infra/gcp/scripts/, so repo root is three up.
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
BENCH_DIR="$REPO_ROOT/infra/gcp/benchmarks"
GCP_BUILD_DIR="$REPO_ROOT/.gcp-build"
DIST_DIR="$GCP_BUILD_DIR/dist"
OUT_TAR="$DIST_DIR/benchmarks.tar.gz"

mkdir -p "$DIST_DIR"

# Resolve target bucket. $REPO_ROOT/.gcp-build/bucket.env is the
# canonical operator-set file (matches the binary upload pattern).
if [ -f "$GCP_BUILD_DIR/bucket.env" ]; then
  # shellcheck disable=SC1091
  source "$GCP_BUILD_DIR/bucket.env"
fi
BUCKET="${BUCKET:-${KISEKI_BENCH_BUCKET:-}}"
if [ -z "$BUCKET" ]; then
  echo "ERROR: BUCKET not set." >&2
  echo "       Either populate .gcp-build/bucket.env with 'BUCKET=...'" >&2
  echo "       or export KISEKI_BENCH_BUCKET=... before running." >&2
  exit 1
fi

echo "=== Packaging benchmarks ==="
# Pack from the benchmarks/ dir's parent so the tarball extracts to
# the same layout the boot script expects:
#   /opt/kiseki-bench/bench
#   /opt/kiseki-bench/perf-common.sh
#   /opt/kiseki-bench/phases/...
# We strip the leading "benchmarks/" so consumers can extract with
# `tar xzf benchmarks.tar.gz -C /opt/kiseki-bench/` (no extra mv).
tar czf "$OUT_TAR" \
  -C "$BENCH_DIR" \
  --exclude='results' \
  --exclude='.gitkeep' \
  .

size=$(stat -c%s "$OUT_TAR")
sha=$(sha256sum "$OUT_TAR" | awk '{print $1}')
echo "  wrote $OUT_TAR ($size bytes, sha256 $sha)"

echo "=== Verifying tarball structure ==="
# Quick sanity: the tarball MUST contain the bench driver + perf-common.sh
# at the top level. If a future refactor moves them, this script
# fails loudly rather than uploading a broken artifact.
required=(./bench ./perf-common.sh)
for f in "${required[@]}"; do
  if ! tar tzf "$OUT_TAR" | grep -q "^${f}\$"; then
    echo "ERROR: tarball missing required entry: $f" >&2
    exit 1
  fi
done
echo "  contents look correct"

echo "=== Uploading to gs://$BUCKET/benchmarks.tar.gz ==="
gcloud storage cp "$OUT_TAR" "gs://$BUCKET/benchmarks.tar.gz"

echo "=== Done ==="
echo "  setup-bench-ctrl.sh will pull this on the next terraform apply"
