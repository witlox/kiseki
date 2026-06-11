#!/usr/bin/env bash
# Perf-floor gate (GH #256 follow-up; #253 made the 60s sustained cell
# mandatory for perf PRs — this script is the runnable form of both).
#
# Compares the CURRENT tree against the pinned last-known-good baseline
# (docs/performance/perf-gate-baseline.toml) on the local 3-node
# kiseki-profile harness, interleaved arms, warm-run discarded:
#
#   1. reference cell ×N per arm (put-heavy, 4 KiB, conc64, 20s)
#      → median ratio current/baseline must be ≥ min_ratio
#   2. one 60s sustained cell per arm
#      → ZERO errors (halt-on-break: a single error fails the gate)
#      → sustained ratio must also be ≥ min_ratio (decay class)
#
# Run before merging any PR that touches the write path, the
# committer, the fan, the hydrator, or the stores:
#
#   make perf-gate          # or: bash scripts/perf-gate.sh
#
# Exit 0 = pass, 2 = gate failed, 1 = harness/setup error.
# Report: .perf-gate/report-<timestamp>.md
#
# LIMITATION (documented in the baseline toml): loopback cannot see
# network-latency-bound waits — #256 measured −71% on GCP and −2.4%
# (noise) locally. Cover that class with unit-level scaling probes
# (assert scaling shape, not wall clock).

set -euo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel)"
cd "$REPO_ROOT"
PIN_FILE="docs/performance/perf-gate-baseline.toml"
GATE_DIR="$REPO_ROOT/.perf-gate"
mkdir -p "$GATE_DIR"

toml_get() { grep -E "^$1 *= *" "$PIN_FILE" | head -1 | sed -E 's/^[^=]*= *"([^"]*)".*/\1/'; }
PIN=$(toml_get pin)
MIN_RATIO=$(toml_get min_ratio)
CONC=$(toml_get concurrency)
OBJSZ=$(toml_get object_size)
NODES=$(toml_get nodes)
MEASURED_SECS=$(toml_get measured_secs)
SUSTAINED_SECS=$(toml_get sustained_secs)
RUNS=$(toml_get runs)
[ -n "$PIN" ] || { echo "ERROR: no pin in $PIN_FILE" >&2; exit 1; }

echo "=== perf-gate: current tree vs baseline $PIN (min_ratio $MIN_RATIO) ==="

# ---------------------------------------------------------------------------
# Build current arm + harness.
# ---------------------------------------------------------------------------
echo "--- building current arm + harness"
cargo build --release -p kiseki-server --bin kiseki-server --bin kiseki-admin
cargo build --release -p kiseki-profile
CUR_SERVER="$REPO_ROOT/target/release/kiseki-server"
CUR_ADMIN="$REPO_ROOT/target/release/kiseki-admin"
PROFILE_BIN="$REPO_ROOT/target/release/kiseki-profile"

# ---------------------------------------------------------------------------
# Build (or reuse cached) baseline arm from the pinned sha.
# ---------------------------------------------------------------------------
BASE_DIR="$GATE_DIR/baseline-$PIN"
if [ ! -x "$BASE_DIR/kiseki-server" ]; then
  echo "--- building baseline $PIN (cache miss)"
  WT="$GATE_DIR/src-$PIN"
  git worktree remove --force "$WT" 2>/dev/null || true
  git worktree add "$WT" "$PIN"
  (cd "$WT" && CARGO_TARGET_DIR="$GATE_DIR/baseline-target" \
    cargo build --release -p kiseki-server --bin kiseki-server --bin kiseki-admin)
  mkdir -p "$BASE_DIR"
  cp "$GATE_DIR/baseline-target/release/kiseki-server" "$BASE_DIR/"
  cp "$GATE_DIR/baseline-target/release/kiseki-admin" "$BASE_DIR/"
  git worktree remove --force "$WT"
else
  echo "--- baseline $PIN cached"
fi
BASE_SERVER="$BASE_DIR/kiseki-server"
BASE_ADMIN="$BASE_DIR/kiseki-admin"

# ---------------------------------------------------------------------------
# Harness invocation. Fresh cluster per run (the harness spawns and
# tears down); per-run data dirs on real disk (the repo's filesystem),
# NOT tmpfs — fsync must be real or durability arms measure nothing.
# ---------------------------------------------------------------------------
export KISEKI_PROFILE_DATA_ROOT="$GATE_DIR/data"
mkdir -p "$KISEKI_PROFILE_DATA_ROOT"
export KISEKI_CHUNK_DEVICE_BYTES=$((16 * 1024 * 1024 * 1024))

run_cell() { # $1 server_bin $2 admin_bin $3 duration_secs -> "ops_per_sec errors p99"
  local out
  out=$("$PROFILE_BIN" run --protocol native --binding tcp --shape put-heavy \
    --concurrency "$CONC" --object-size "$OBJSZ" --nodes "$NODES" \
    --duration-secs "$3" --server-bin "$1" --admin-bin "$2" 2>/dev/null) || {
      echo "HARNESS-ERROR"; return 0; }
  local ops errors p99
  ops=$(sed -nE 's/.*throughput=([0-9.]+) op\/s.*/\1/p' <<<"$out" | head -1)
  errors=$(sed -nE 's/^errors=([0-9]+)$/\1/p' <<<"$out" | head -1)
  p99=$(sed -nE 's/.*p99=([0-9]+).*/\1/p' <<<"$out" | head -1)
  echo "${ops:-0} ${errors:-0} ${p99:-0}"
}

median3() { printf '%s\n' "$@" | sort -n | sed -n 2p; }

declare -a BASE_OPS CUR_OPS
REPORT="$GATE_DIR/report-$(date +%Y%m%d-%H%M%S).md"
{
  echo "# perf-gate report"
  echo
  echo "baseline pin: \`$PIN\` · min_ratio: $MIN_RATIO · cell: put-heavy ${OBJSZ}B conc${CONC} ${NODES}-node"
  echo
  echo "| run | arm | op/s | errors | p99 (µs) |"
  echo "|---|---|---|---|---|"
} > "$REPORT"

fail=0
for i in $(seq 1 "$RUNS"); do
  for arm in baseline current; do
    if [ "$arm" = baseline ]; then srv="$BASE_SERVER"; adm="$BASE_ADMIN"; else srv="$CUR_SERVER"; adm="$CUR_ADMIN"; fi
    # warm run — discarded
    run_cell "$srv" "$adm" "$MEASURED_SECS" >/dev/null
    read -r ops errors p99 <<<"$(run_cell "$srv" "$adm" "$MEASURED_SECS")"
    if [ "$ops" = "HARNESS-ERROR" ]; then echo "ERROR: harness failed ($arm run $i)" >&2; exit 1; fi
    echo "| m$i | $arm | $ops | $errors | $p99 |" >> "$REPORT"
    echo "    m$i $arm: $ops op/s errors=$errors p99=${p99}us"
    if [ "$errors" != "0" ]; then
      echo "GATE FAIL: errors in measured cell ($arm m$i) — halt-on-break" | tee -a "$REPORT"
      fail=1
    fi
    if [ "$arm" = baseline ]; then BASE_OPS+=("$ops"); else CUR_OPS+=("$ops"); fi
  done
done

BASE_MED=$(median3 "${BASE_OPS[@]}")
CUR_MED=$(median3 "${CUR_OPS[@]}")
RATIO=$(awk -v c="$CUR_MED" -v b="$BASE_MED" 'BEGIN { printf "%.3f", (b > 0) ? c / b : 0 }')
echo | tee -a "$REPORT"
echo "median: baseline=$BASE_MED current=$CUR_MED ratio=$RATIO (floor $MIN_RATIO)" | tee -a "$REPORT"
if awk -v r="$RATIO" -v m="$MIN_RATIO" 'BEGIN { exit !(r < m) }'; then
  echo "GATE FAIL: reference-cell ratio $RATIO < $MIN_RATIO" | tee -a "$REPORT"
  fail=1
fi

# ---------------------------------------------------------------------------
# Sustained cells (#253 class + decay class).
# ---------------------------------------------------------------------------
echo "--- sustained ${SUSTAINED_SECS}s cells"
read -r b_ops b_err b_p99 <<<"$(run_cell "$BASE_SERVER" "$BASE_ADMIN" "$SUSTAINED_SECS")"
read -r c_ops c_err c_p99 <<<"$(run_cell "$CUR_SERVER" "$CUR_ADMIN" "$SUSTAINED_SECS")"
if [ "$b_ops" = "HARNESS-ERROR" ] || [ "$c_ops" = "HARNESS-ERROR" ]; then
  echo "ERROR: harness failed on a sustained cell" >&2; exit 1
fi
S_RATIO=$(awk -v c="$c_ops" -v b="$b_ops" 'BEGIN { printf "%.3f", (b > 0) ? c / b : 0 }')
{
  echo
  echo "sustained ${SUSTAINED_SECS}s: baseline=$b_ops (errors=$b_err) current=$c_ops (errors=$c_err) ratio=$S_RATIO"
} | tee -a "$REPORT"
if [ "$b_err" != "0" ] || [ "$c_err" != "0" ]; then
  echo "GATE FAIL: errors under sustained load (baseline=$b_err current=$c_err) — the #253 class" | tee -a "$REPORT"
  fail=1
fi
if awk -v r="$S_RATIO" -v m="$MIN_RATIO" 'BEGIN { exit !(r < m) }'; then
  echo "GATE FAIL: sustained ratio $S_RATIO < $MIN_RATIO (decay class, see #256)" | tee -a "$REPORT"
  fail=1
fi

echo
if [ "$fail" -ne 0 ]; then
  echo "=== perf-gate: FAIL (report: $REPORT) ==="
  exit 2
fi
echo "=== perf-gate: PASS (ratio $RATIO, sustained $S_RATIO, 0 errors) — report: $REPORT ==="
