#!/bin/bash
# Unit tests for $KISEKI_RUN_ID / $RESULTS stickiness in perf-common.sh.
#
# Pre-#55 bug: each `source perf-common.sh` re-evaluated
# `RUN_TS=$(date +%Y%m%d-%H%M%S)`, so phases that ran from different
# scripts (or different invocations within a session) wrote to
# DIFFERENT /tmp/kiseki-perf-* dirs. The fix keys $RESULTS off
# $KISEKI_RUN_ID, which is sticky via a marker file. This file is
# the regression fence for that fix.
#
# Run directly:
#   bash infra/gcp/benchmarks/tests/test_run_id.sh
#
# Exit code: 0 = all tests pass, 1 = at least one failure.

set -u

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
BENCH_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

TMP_ENV="$(mktemp)"
TMP_MARKER="$(mktemp -u)"
trap 'rm -f "$TMP_ENV" "$TMP_MARKER"' EXIT
cat > "$TMP_ENV" <<'EOF'
STORAGE_IPS="10.0.0.1"
CLIENT_IPS="10.0.1.1"
FIRST_STORAGE="10.0.0.1"
KISEKI_PERF_BUCKET="gs://test-bucket"
SSH_USER="test-user"
KISEKI_PROFILE="test"
EOF

export KISEKI_BENCH_ENV="$TMP_ENV"
# Point the marker at a per-test path so we don't trample whatever a
# real run created on this dev box.
export KISEKI_RUN_ID_MARKER="$TMP_MARKER"

PASS=0
FAIL=0
FAILURES=""

assert_eq() {
  local name="$1" expected="$2" actual="$3"
  if [ "$expected" = "$actual" ]; then
    PASS=$((PASS + 1))
    echo "  ok   $name"
  else
    FAIL=$((FAIL + 1))
    FAILURES="$FAILURES\n    $name: expected='$expected' actual='$actual'"
    echo "  FAIL $name: expected='$expected' actual='$actual'"
  fi
}

assert_matches() {
  local name="$1" pattern="$2" actual="$3"
  if [[ "$actual" =~ $pattern ]]; then
    PASS=$((PASS + 1))
    echo "  ok   $name"
  else
    FAIL=$((FAIL + 1))
    FAILURES="$FAILURES\n    $name: pattern='$pattern' actual='$actual'"
    echo "  FAIL $name: pattern='$pattern' actual='$actual'"
  fi
}

# ---------- Test 1: explicit override is honored ----------
echo "[KISEKI_RUN_ID override]"
rm -f "$TMP_MARKER"
unset KISEKI_RUN_ID
export KISEKI_RUN_ID="explicit-pin"
# shellcheck disable=SC1091
source "$BENCH_DIR/perf-common.sh" >/dev/null
assert_eq "RESULTS uses pinned id" "/tmp/kiseki-perf-test-explicit-pin" "$RESULTS"
assert_eq "RUN_TS legacy alias matches" "explicit-pin" "$RUN_TS"
unset KISEKI_RUN_ID

echo
# ---------- Test 2: marker file is read when env unset ----------
echo "[Marker file persistence]"
rm -f "$TMP_MARKER"
echo "from-marker-001" > "$TMP_MARKER"
unset KISEKI_RUN_ID
# shellcheck disable=SC1091
source "$BENCH_DIR/perf-common.sh" >/dev/null
assert_eq "RESULTS uses marker id" "/tmp/kiseki-perf-test-from-marker-001" "$RESULTS"
assert_eq "KISEKI_RUN_ID picked up from marker" "from-marker-001" "$KISEKI_RUN_ID"
unset KISEKI_RUN_ID

echo
# ---------- Test 3: first source generates id + writes marker ----------
echo "[Fresh session — id is generated + persisted]"
rm -f "$TMP_MARKER"
unset KISEKI_RUN_ID
# shellcheck disable=SC1091
source "$BENCH_DIR/perf-common.sh" >/dev/null
assert_matches "generated id matches YYYYMMDD-HHMMSS" '^[0-9]{8}-[0-9]{6}$' "$KISEKI_RUN_ID"
if [ -f "$TMP_MARKER" ]; then
  assert_eq "marker file persists the generated id" "$KISEKI_RUN_ID" "$(cat "$TMP_MARKER")"
else
  FAIL=$((FAIL + 1))
  echo "  FAIL marker file was not created at $TMP_MARKER"
fi
SAVED_ID="$KISEKI_RUN_ID"

echo
# ---------- Test 4: re-source within session is sticky ----------
echo "[Re-source within session — id is sticky]"
unset KISEKI_RUN_ID
# shellcheck disable=SC1091
source "$BENCH_DIR/perf-common.sh" >/dev/null
assert_eq "second source reuses the same id" "$SAVED_ID" "$KISEKI_RUN_ID"
unset KISEKI_RUN_ID

echo
# ---------- Test 5: removing the marker rolls a fresh id ----------
echo "[Marker reset → fresh id]"
rm -f "$TMP_MARKER"
unset KISEKI_RUN_ID
# shellcheck disable=SC1091
source "$BENCH_DIR/perf-common.sh" >/dev/null
# Sleep is unnecessary; the only invariant we care about is that the
# id ≠ the SAVED_ID was when the marker pinned it — but a same-second
# regen would still match. Just check the id matches the timestamp
# shape and that the marker now exists.
assert_matches "fresh id still matches timestamp shape" '^[0-9]{8}-[0-9]{6}$' "$KISEKI_RUN_ID"
if [ -f "$TMP_MARKER" ]; then
  PASS=$((PASS + 1))
  echo "  ok   marker re-created after reset"
else
  FAIL=$((FAIL + 1))
  echo "  FAIL marker not re-created at $TMP_MARKER"
fi

echo
echo "═══════════════════════════════════════════════════════"
if [ "$FAIL" -eq 0 ]; then
  echo "PASS: $PASS tests passed"
  exit 0
else
  echo "FAIL: $FAIL of $((PASS + FAIL)) tests failed"
  printf "%b\n" "$FAILURES"
  exit 1
fi
