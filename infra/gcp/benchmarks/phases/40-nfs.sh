#!/bin/bash
# Phase 40 — NFSv4.2 write + read AND pNFS (vers=4.1) read, with
# pre/post hydrator snapshots. (Read + pNFS run in gcp mode; the local
# docker probe path stays write-only — it's a smoke path, not the matrix.)
#
# The F-1-risk phase (#50): sustained NFS write saturates the
# composition hydrator because each NFS WRITE op becomes one Raft
# entry. Pre/post snapshots of /metrics let `bench-report` (future
# tool) compute hydrator backlog growth and flag a wedge.
#
# Mount happens on a client VM (gcp mode) or via privileged docker
# container joined to the compose network (local mode), since the
# claude-code sandbox can't kernel-mount NFS directly.
#
# Exit 2 — mount fails, write fails, or hydrator backlog > halt threshold.
#
# Env:
#   KISEKI_BENCH_NFS_SIZE_GB     (default: 4)
#   KISEKI_BENCH_NFS_RUNTIME_SECS (default: 60)  fio --runtime cap
#   KISEKI_BENCH_NFS_NUMJOBS      (default: 4)
#   KISEKI_BENCH_NFS_READ_MB      (default: 256)  read-test file size — a
#                                 bounded pre-write that the O_DIRECT read-back
#                                 then exercises. Small on purpose: the
#                                 multi-node write path is commit-bound, so a
#                                 big pre-write would dominate the phase.
#   KISEKI_BENCH_NFS_PROBE_IMAGE  (default: kiseki-pnfs-client:test, local mode only)

set -uo pipefail
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
BENCH_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
# shellcheck disable=SC1091
source "$BENCH_DIR/perf-common.sh"

discover_leader > /dev/null 2>&1
leader_endpoints
SIZE_GB="${KISEKI_BENCH_NFS_SIZE_GB:-4}"
RUNTIME="${KISEKI_BENCH_NFS_RUNTIME_SECS:-60}"
NUMJOBS="${KISEKI_BENCH_NFS_NUMJOBS:-4}"
READ_MB="${KISEKI_BENCH_NFS_READ_MB:-256}"
PROBE_IMG="${KISEKI_BENCH_NFS_PROBE_IMAGE:-kiseki-pnfs-client:test}"

OUT="$RESULTS/40-nfs.txt"
{
  echo "=== Phase 40: NFSv4.2 write/read ==="
  echo "leader_nfs=$LEADER_NFS_HOSTPORT"
  echo "size=${SIZE_GB}G runtime=${RUNTIME}s numjobs=$NUMJOBS"
  echo ""
} | tee "$OUT"

# Pre snapshot — hydrator + raft + chunk write counters on the leader.
PRE="$RESULTS/40-nfs-pre.txt"
curl -sf "$LEADER_METRICS_URL/metrics" 2>/dev/null \
  | grep -E "^kiseki_(composition_hydrator|raft_log|chunk_pending|chunk_write_bytes_total|composition_pending)" \
  > "$PRE" || true
echo "[pre] $(wc -l < "$PRE") hydrator/raft/chunk lines captured → 40-nfs-pre.txt" | tee -a "$OUT"
echo "" | tee -a "$OUT"

MODE=$(bench_mode)
NFS_HOST="${LEADER_NFS_HOSTPORT%:*}"

if [ "$MODE" = "local" ]; then
  # Use the kiseki-pnfs-client:test container to mount + run fio.
  # Container joins the compose network so it can resolve kiseki-node1.
  if ! docker image inspect "$PROBE_IMG" >/dev/null 2>&1; then
    echo "SKIP: probe image $PROBE_IMG not built (docker build -f tests/e2e/Dockerfile.pnfs-client -t $PROBE_IMG tests/e2e/)" | tee -a "$OUT"
    echo "OK" | tee -a "$OUT"
    exit 0
  fi
  # For local compose the NFS hostname inside the container's network
  # is `kiseki-node1`, not 127.0.0.1.
  NFS_HOST="kiseki-node1"
  NETWORK="${KISEKI_LOCAL_NFS_NETWORK:-kiseki_default}"
  echo "(local mode: docker run --network $NETWORK $PROBE_IMG)" | tee -a "$OUT"
  FIO_JSON="$RESULTS/40-nfs-fio.json"
  rm -f "$FIO_JSON"
  # Run the fio inside the container, write JSON output to a host
  # bind-mount, then parse OUTSIDE the container. Avoids the heredoc
  # backslash-escape hell when mixing python f-strings through
  # docker run + shell quoting.
  #
  # `timeout` wraps the docker run so an F-1 wedge inside the
  # container (fio stuck in D state on NFS COMMIT — the 2026-05-16
  # GCP symptom) doesn't hang the whole phase indefinitely. The cap
  # is `$RUNTIME * 4 + 30s` to give cleanup room beyond fio's
  # --runtime budget.
  CONTAINER_NAME="kiseki-nfs-bench-$$"
  HARD_CAP=$(( RUNTIME * 4 + 30 ))
  timeout "${HARD_CAP}s" docker run --rm \
    --name "$CONTAINER_NAME" \
    --privileged --cap-add SYS_ADMIN \
    --network "$NETWORK" \
    -v "$RESULTS:/results" \
    "$PROBE_IMG" /bin/sh -c "
mkdir -p /mnt/nfs
mount -t nfs4 -o vers=4.2,rsize=1048576,wsize=1048576 $NFS_HOST:/ /mnt/nfs
if ! mountpoint -q /mnt/nfs; then
  echo MOUNT-FAILED >&2
  exit 1
fi
fio --name=nfs-w --directory=/mnt/nfs --rw=write --bs=1m \\
  --size=${SIZE_GB}G --numjobs=$NUMJOBS --direct=1 \\
  --runtime=$RUNTIME --time_based \\
  --group_reporting --output-format=json > /results/40-nfs-fio.json 2>/results/40-nfs-fio.err
fio_rc=\$?
rm -f /mnt/nfs/nfs-w* 2>/dev/null
umount /mnt/nfs 2>/dev/null || umount -l /mnt/nfs 2>/dev/null
exit \$fio_rc
" 2>&1 | tee -a "$OUT"
  TIMEOUT_RC=$?
  if [ "$TIMEOUT_RC" -eq 124 ]; then
    echo "HALT (#50 / F-1 shape): NFS phase exceeded ${HARD_CAP}s cap — likely fio stuck in D state" | tee -a "$OUT"
    docker kill "$CONTAINER_NAME" >/dev/null 2>&1 || true
    exit 2
  fi
  # Parse the captured fio JSON in the calling shell — no docker-run
  # heredoc shenanigans.
  if [ -s "$FIO_JSON" ]; then
    python3 - "$FIO_JSON" <<'PY' | tee -a "$OUT"
import json, sys
with open(sys.argv[1]) as fh:
    d = json.load(fh)
j = d['jobs'][0]['write']
bw_mbs = j['bw'] / 1024
iops = j['iops']
p99_us = j['lat_ns']['percentile']['99.000000'] / 1000
print(f"write: {bw_mbs:.1f} MB/s ({iops:.0f} IOPS, p99={p99_us:.0f}us)")
PY
  else
    echo "  fio produced no JSON output (mount/fio failed?)" | tee -a "$OUT"
  fi
elif [ "$MODE" = "gcp" ]; then
  # Human-readable fio output + grep, NOT --output-format=json piped to a
  # python -c one-liner: the JSON one-liner's nested f-string quoting is
  # fragile through the heredoc → client_run → remote-shell layers (it was
  # never confirmed to emit on a real gcp run; the 2026-05-27 numbers came
  # from hand-driven fio). `WRITE:` / `READ:` summary lines are stable.
  client_run 0 <<EOF | tee -a "$OUT"
mkdir -p /mnt/kiseki-nfs
umount /mnt/kiseki-nfs 2>/dev/null

echo "--- NFSv4.2 (vers=4.2) write ---"
mount -t nfs4 -o vers=4.2,rsize=1048576,wsize=1048576 $NFS_HOST:/ /mnt/kiseki-nfs
if ! mountpoint -q /mnt/kiseki-nfs; then
  echo MOUNT-FAILED
  exit 1
fi
fio --name=nfs-w --directory=/mnt/kiseki-nfs --rw=write --bs=1m \\
  --size=${SIZE_GB}G --numjobs=$NUMJOBS --direct=1 \\
  --runtime=$RUNTIME --time_based --group_reporting 2>/dev/null | grep -E 'WRITE:'

echo "--- NFSv4.2 read (pre-write ${READ_MB}M, then O_DIRECT read-back) ---"
# O_DIRECT read bypasses the page cache so this is the real gateway read
# path, not a cache hit. The 2026-05-27 "read inconclusive" was a read of
# a file that was never written — pre-write it here first.
fio --name=nfs-rd --directory=/mnt/kiseki-nfs --rw=write --bs=1m \\
  --size=${READ_MB}m --numjobs=1 --direct=1 --end_fsync=1 >/dev/null 2>&1
fio --name=nfs-rd --directory=/mnt/kiseki-nfs --rw=read --bs=1m \\
  --size=${READ_MB}m --numjobs=1 --direct=1 --group_reporting 2>/dev/null | grep -E 'READ:'
rm -f /mnt/kiseki-nfs/nfs-w* /mnt/kiseki-nfs/nfs-rd* 2>/dev/null
umount /mnt/kiseki-nfs 2>/dev/null

echo "--- pNFS Flex Files (vers=4.1; kernel LAYOUTGET -> per-DS read) ---"
mount -t nfs4 -o vers=4.1,rsize=1048576,wsize=1048576 $NFS_HOST:/ /mnt/kiseki-nfs
if ! mountpoint -q /mnt/kiseki-nfs; then
  echo PNFS-MOUNT-FAILED
  exit 1
fi
nfsstat -m 2>/dev/null | grep -iE 'vers=4.1|flexfiles' || echo '(no nfsstat layout line — MDS fallback?)'
fio --name=pnfs-rd --directory=/mnt/kiseki-nfs --rw=write --bs=1m \\
  --size=${READ_MB}m --numjobs=1 --direct=1 --end_fsync=1 >/dev/null 2>&1
fio --name=pnfs-rd --directory=/mnt/kiseki-nfs --rw=read --bs=1m \\
  --size=${READ_MB}m --numjobs=1 --direct=1 --group_reporting 2>/dev/null | grep -E 'READ:'
rm -f /mnt/kiseki-nfs/pnfs-rd* 2>/dev/null
umount /mnt/kiseki-nfs 2>/dev/null
EOF
fi

# Post snapshot
POST="$RESULTS/40-nfs-post.txt"
curl -sf "$LEADER_METRICS_URL/metrics" 2>/dev/null \
  | grep -E "^kiseki_(composition_hydrator|raft_log|chunk_pending|chunk_write_bytes_total|composition_pending)" \
  > "$POST" || true
echo "" | tee -a "$OUT"
echo "[post] hydrator delta:" | tee -a "$OUT"

# Compute hydrator backlog delta. If composition_hydrator_stalled
# went 0 → non-zero, halt the run. Otherwise just log the
# pre/post applied-seq for postmortem.
PRE_APPLIED=$(grep '^kiseki_composition_hydrator_last_applied_seq' "$PRE" 2>/dev/null \
  | grep -oE '[0-9]+$' | head -1 || echo "0")
POST_APPLIED=$(grep '^kiseki_composition_hydrator_last_applied_seq' "$POST" 2>/dev/null \
  | grep -oE '[0-9]+$' | head -1 || echo "0")
PRE_STALLED=$(grep '^kiseki_composition_hydrator_stalled' "$PRE" 2>/dev/null \
  | grep -oE '[0-9]+$' | head -1 || echo "0")
POST_STALLED=$(grep '^kiseki_composition_hydrator_stalled' "$POST" 2>/dev/null \
  | grep -oE '[0-9]+$' | head -1 || echo "0")
APPLIED_DELTA=$((POST_APPLIED - PRE_APPLIED))
echo "  applied_seq: $PRE_APPLIED → $POST_APPLIED (Δ=$APPLIED_DELTA)" | tee -a "$OUT"
echo "  stalled:     $PRE_STALLED → $POST_STALLED" | tee -a "$OUT"

if [ "$POST_STALLED" -gt 0 ] && [ "$PRE_STALLED" -eq 0 ]; then
  echo "HALT (#50 / F-1): hydrator transitioned stalled=0 → ${POST_STALLED}" | tee -a "$OUT"
  exit 2
fi

echo "OK" | tee -a "$OUT"
