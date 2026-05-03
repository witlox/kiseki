#!/usr/bin/env bash
#
# Run the full profiling matrix: 5 protocols × 3 shapes.
# CPU profile is captured by spawning the pprof-enabled server.
# Heap profile is captured by spawning the dhat-enabled server.
# Both write per-protocol/per-shape output under OUT_DIR (default
# /tmp/kiseki-prof).

set -uo pipefail

OUT_DIR="${OUT_DIR:-/tmp/kiseki-prof}"
DURATION="${DURATION_SECS:-30}"
CONCURRENCY="${CONCURRENCY:-16}"
OBJECT_SIZE="${OBJECT_SIZE:-65536}"
WARMUP_OBJECTS="${WARMUP_OBJECTS:-256}"

PROFILE_BIN="${PROFILE_BIN:-/home/witlox/kiseki/target/release/kiseki-profile}"
SERVER_PPROF_BIN="${SERVER_PPROF_BIN:-/home/witlox/kiseki/target/release/kiseki-server}"
SERVER_DHAT_BIN="${SERVER_DHAT_BIN:-/home/witlox/kiseki/target-dhat/release/kiseki-server}"

mkdir -p "$OUT_DIR"

PROTOCOLS=(s3 nfs3 nfs4 pnfs fuse)
SHAPES=(put-heavy get-heavy mixed)

echo "== profile matrix =="
echo "concurrency=$CONCURRENCY object_size=$OBJECT_SIZE duration=${DURATION}s out=$OUT_DIR"

# Phase 1: CPU profiles via pprof-enabled server.
echo
echo "== phase 1: CPU profiles =="
for proto in "${PROTOCOLS[@]}"; do
  for shape in "${SHAPES[@]}"; do
    label="${proto}-${shape}"
    svg="$OUT_DIR/cpu-${label}.svg"
    summary="$OUT_DIR/cpu-${label}.txt"
    echo "-- $label"
    KISEKI_PPROF_OUT="$svg" \
      "$PROFILE_BIN" run \
        --protocol "$proto" \
        --shape "$shape" \
        --concurrency "$CONCURRENCY" \
        --object-size "$OBJECT_SIZE" \
        --duration-secs "$DURATION" \
        --warmup-objects "$WARMUP_OBJECTS" \
        --server-bin "$SERVER_PPROF_BIN" \
      > "$summary" 2>&1
    if [ -s "$svg" ]; then
      echo "   svg=$svg ($(stat -c%s "$svg") bytes)"
    else
      echo "   ⚠  no flamegraph written"
    fi
    grep -E '^(protocol|ops|latency)' "$summary" | sed 's/^/   /'
  done
done

# Phase 2: heap profiles via dhat-enabled server.
echo
echo "== phase 2: heap profiles =="
for proto in "${PROTOCOLS[@]}"; do
  for shape in "${SHAPES[@]}"; do
    label="${proto}-${shape}"
    json="$OUT_DIR/heap-${label}.json"
    summary="$OUT_DIR/heap-${label}.txt"
    echo "-- $label"
    DHAT_OUTPUT_FILE="$json" \
      "$PROFILE_BIN" run \
        --protocol "$proto" \
        --shape "$shape" \
        --concurrency "$CONCURRENCY" \
        --object-size "$OBJECT_SIZE" \
        --duration-secs "$DURATION" \
        --warmup-objects "$WARMUP_OBJECTS" \
        --server-bin "$SERVER_DHAT_BIN" \
      > "$summary" 2>&1
    if [ -s "$json" ]; then
      echo "   json=$json ($(stat -c%s "$json") bytes)"
    else
      echo "   ⚠  no heap profile written"
    fi
    grep -E '^(protocol|ops|latency)' "$summary" | sed 's/^/   /'
  done
done

echo
echo "== done =="
echo "browse SVGs with: xdg-open $OUT_DIR/cpu-s3-put-heavy.svg"
echo "view dhat with:   dh_view.html (drag-drop a heap-*.json)"
