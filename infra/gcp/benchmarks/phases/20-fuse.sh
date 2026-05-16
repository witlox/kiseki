#!/bin/bash
# Phase 20 — FUSE-over-native: mount + sequential write/read + metadata.
#
# Mounts via `kiseki-client mount --endpoint kiseki://...:9103`. Drives
# dd for sequential I/O (the OSS standard) and a simple mkdir+create
# loop for metadata throughput.
#
# Exit 2 — mount fails, or read returns < written bytes (the #51
#          FUSE-over-native silent-data-loss class).
#
# Env:
#   KISEKI_BENCH_FUSE_SIZE_MB   (default: 256)
#   KISEKI_BENCH_FUSE_METADATA_OPS  (default: 100)

set -uo pipefail
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
BENCH_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
# shellcheck disable=SC1091
source "$BENCH_DIR/perf-common.sh"

discover_leader > /dev/null 2>&1
leader_endpoints
SIZE_MB="${KISEKI_BENCH_FUSE_SIZE_MB:-256}"
META_OPS="${KISEKI_BENCH_FUSE_METADATA_OPS:-100}"
MOUNT_POINT="${KISEKI_BENCH_FUSE_MOUNTPOINT:-/mnt/kiseki-fuse-bench}"
CACHE_DIR="${KISEKI_BENCH_FUSE_CACHE_DIR:-/tmp/kiseki-cache-bench}"

OUT="$RESULTS/20-fuse.txt"
{
  echo "=== Phase 20: FUSE-over-native ==="
  echo "endpoint=$LEADER_NATIVE_URL mountpoint=$MOUNT_POINT"
  echo "size=${SIZE_MB}MB metadata_ops=$META_OPS"
  echo ""
} | tee "$OUT"

MODE=$(bench_mode)
CLIENT_BIN=""
if [ "$MODE" = "gcp" ]; then
  cands=(/usr/local/bin/kiseki-client)
else
  cands=(
    "$BENCH_DIR/../../../target/debug/kiseki-client"
    "$BENCH_DIR/../../../target/release/kiseki-client"
    "$(which kiseki-client 2>/dev/null || true)"
  )
fi
for cand in "${cands[@]}"; do
  [ -x "$cand" ] || continue
  CLIENT_BIN="$cand"; break
done
if [ -z "$CLIENT_BIN" ]; then
  echo "HALT: no kiseki-client binary found" | tee -a "$OUT"
  exit 2
fi

if [ "$MODE" = "gcp" ]; then
  echo "TODO: gcp mode SSHes into a client VM; pasted shape, untested" | tee -a "$OUT"
  client_run 0 <<EOF | tee -a "$OUT"
sudo umount -l $MOUNT_POINT 2>/dev/null
sudo mkdir -p $MOUNT_POINT $CACHE_DIR
sudo $CLIENT_BIN mount --endpoint $LEADER_NATIVE_URL --mountpoint $MOUNT_POINT \\
  --cache-mode organic --cache-dir $CACHE_DIR --read-write 2>&1 &
sleep 5
if mountpoint -q $MOUNT_POINT; then
  echo MOUNTED
  sudo dd if=/dev/zero of=$MOUNT_POINT/dd-write bs=1M count=$SIZE_MB conv=fdatasync 2>&1 | tail -1
  sudo sync; echo 3 | sudo tee /proc/sys/vm/drop_caches >/dev/null
  sudo dd if=$MOUNT_POINT/dd-write of=/dev/null bs=1M 2>&1 | tail -1
  sudo rm -f $MOUNT_POINT/dd-write
  sudo umount -l $MOUNT_POINT
else
  echo MOUNT-FAILED
fi
EOF
else
  # Local mode: needs fuse mount + sudo. Skip with a clear message if
  # the operator isn't running this with the right privileges.
  if ! command -v fusermount3 >/dev/null && ! command -v fusermount >/dev/null; then
    echo "SKIP: fusermount(3) not available on this host" | tee -a "$OUT"
    echo "OK" | tee -a "$OUT"
    exit 0
  fi

  echo "(local mode: requires sudo for mount)" | tee -a "$OUT"
  sudo -n true 2>/dev/null || {
    echo "SKIP: sudo not available non-interactively" | tee -a "$OUT"
    echo "OK" | tee -a "$OUT"
    exit 0
  }

  sudo umount -l "$MOUNT_POINT" 2>/dev/null || true
  sudo mkdir -p "$MOUNT_POINT" "$CACHE_DIR"
  echo "Mounting..." | tee -a "$OUT"
  sudo "$CLIENT_BIN" mount --endpoint "$LEADER_NATIVE_URL" --mountpoint "$MOUNT_POINT" \
    --cache-mode organic --cache-dir "$CACHE_DIR" --read-write >> "$OUT" 2>&1 &
  FUSE_PID=$!
  sleep 5

  if ! mountpoint -q "$MOUNT_POINT" 2>/dev/null; then
    echo "HALT: FUSE mount failed" | tee -a "$OUT"
    sudo kill "$FUSE_PID" 2>/dev/null || true
    exit 2
  fi
  echo "MOUNTED" | tee -a "$OUT"

  echo "--- write ${SIZE_MB}MB ---" | tee -a "$OUT"
  sudo rm -f "$MOUNT_POINT/dd-write" 2>/dev/null
  W=$(sudo dd if=/dev/zero of="$MOUNT_POINT/dd-write" bs=1M count="$SIZE_MB" conv=fdatasync 2>&1 | tail -1)
  echo "$W" | tee -a "$OUT"

  echo "--- read ${SIZE_MB}MB (caches dropped) ---" | tee -a "$OUT"
  sudo sync && sudo bash -c 'echo 3 > /proc/sys/vm/drop_caches' 2>/dev/null
  R=$(sudo dd if="$MOUNT_POINT/dd-write" of=/dev/null bs=1M 2>&1 | tail -1)
  echo "$R" | tee -a "$OUT"

  # #51 fence: read bytes must match write. The 2026-05-16 GCP run
  # showed "read 0 bytes" after a successful fdatasync — silent
  # data loss. Pin it here so the phase halts loudly instead of
  # quietly reporting a 0.0 GB/s GET.
  R_BYTES=$(echo "$R" | grep -oE '^[0-9]+' | head -1 || echo "0")
  EXPECTED=$((SIZE_MB * 1024 * 1024))
  if [ "${R_BYTES:-0}" -lt "$EXPECTED" ]; then
    echo "HALT (#51 class): read returned ${R_BYTES} bytes, expected $EXPECTED" | tee -a "$OUT"
    sudo rm -f "$MOUNT_POINT/dd-write" 2>/dev/null
    sudo umount -l "$MOUNT_POINT" 2>/dev/null || true
    exit 2
  fi
  sudo rm -f "$MOUNT_POINT/dd-write" 2>/dev/null

  echo "--- metadata: ${META_OPS} mkdir+create ---" | tee -a "$OUT"
  S=$(date +%s%N)
  for i in $(seq 1 "$META_OPS"); do
    sudo mkdir -p "$MOUNT_POINT/mdtest-$i" 2>/dev/null
    sudo bash -c "echo x > '$MOUNT_POINT/mdtest-$i/f'" 2>/dev/null
  done
  E=$(date +%s%N)
  MS=$(( (E - S) / 1000000 ))
  OPS=$(awk "BEGIN{print int($META_OPS * 2 * 1000 / $MS)}")
  echo "  ${MS}ms - ${OPS} ops/s" | tee -a "$OUT"

  # Cleanup
  sudo rm -rf "$MOUNT_POINT"/mdtest-* 2>/dev/null
  sudo umount -l "$MOUNT_POINT" 2>/dev/null || true
fi

echo "OK" | tee -a "$OUT"
