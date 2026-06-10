#!/bin/bash
# Unit tests for pick_storage_for_client / pick_namespace_for_client
# helpers in perf-common.sh.
#
# These helpers spread bench traffic across the storage IPs and the
# bench-managed namespaces. The fan-out is the whole point of Step B
# of the write-routing posture plan (see
# specs/implementation/write-routing-posture.md and
# specs/findings/2026-05-15-write-fanout-validation.md). If either
# helper goes back to "everything routes to client 0", the perf-suite
# silently degrades to the single-leader bottleneck the plan was
# written to remove — so this is the regression fence for that gap.
#
# Run directly:
#   bash infra/gcp/benchmarks/tests/test_pick_helpers.sh
#
# Exit code: 0 = all tests pass, 1 = at least one failure.

set -u

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
BENCH_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

# perf-common.sh normally sources /etc/kiseki-bench.env (only present
# on the bench-ctrl VM). For local unit tests we point it at a
# fixture via the KISEKI_BENCH_ENV env override.
TMP_ENV="$(mktemp)"
trap 'rm -f "$TMP_ENV"' EXIT
cat > "$TMP_ENV" <<'EOF'
STORAGE_IPS="10.0.0.1,10.0.0.2,10.0.0.3"
CLIENT_IPS="10.0.1.1,10.0.1.2,10.0.1.3"
FIRST_STORAGE="10.0.0.1"
KISEKI_PERF_BUCKET="gs://test-bucket"
SSH_USER="test-user"
KISEKI_PROFILE="test"
EOF

export KISEKI_BENCH_ENV="$TMP_ENV"

# shellcheck disable=SC1091
source "$BENCH_DIR/perf-common.sh"

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

echo "STORAGE_IPS_ARRAY: ${STORAGE_IPS_ARRAY[*]}"
echo

echo "[pick_storage_for_client] 3-element STORAGE_IPS_ARRAY round-robin"
assert_eq "idx=0 -> 10.0.0.1" "10.0.0.1" "$(pick_storage_for_client 0)"
assert_eq "idx=1 -> 10.0.0.2" "10.0.0.2" "$(pick_storage_for_client 1)"
assert_eq "idx=2 -> 10.0.0.3" "10.0.0.3" "$(pick_storage_for_client 2)"
assert_eq "idx=3 -> 10.0.0.1 (wraps)" "10.0.0.1" "$(pick_storage_for_client 3)"
assert_eq "idx=4 -> 10.0.0.2 (wraps)" "10.0.0.2" "$(pick_storage_for_client 4)"

echo
echo "[pick_namespace_for_client] 3-element STORAGE_IPS_ARRAY round-robin"
assert_eq "idx=0 -> perf-agg-ns0" "perf-agg-ns0" "$(pick_namespace_for_client 0)"
assert_eq "idx=1 -> perf-agg-ns1" "perf-agg-ns1" "$(pick_namespace_for_client 1)"
assert_eq "idx=2 -> perf-agg-ns2" "perf-agg-ns2" "$(pick_namespace_for_client 2)"
assert_eq "idx=3 -> perf-agg-ns0 (wraps)" "perf-agg-ns0" "$(pick_namespace_for_client 3)"
assert_eq "idx=5 -> perf-agg-ns2 (wraps)" "perf-agg-ns2" "$(pick_namespace_for_client 5)"

# Edge: single-storage-IP cluster (compact-of-1 dev shape) — degenerate
# but must not divide-by-zero. The helpers should always return a
# valid IP / namespace name.
echo
echo "[pick_storage_for_client] 1-element STORAGE_IPS_ARRAY (degenerate)"
STORAGE_IPS_ARRAY=("10.0.0.7")
assert_eq "idx=0 -> 10.0.0.7" "10.0.0.7" "$(pick_storage_for_client 0)"
assert_eq "idx=2 -> 10.0.0.7 (always same node)" "10.0.0.7" "$(pick_storage_for_client 2)"
assert_eq "ns idx=0 -> perf-agg-ns0" "perf-agg-ns0" "$(pick_namespace_for_client 0)"
assert_eq "ns idx=1 -> perf-agg-ns0 (single-node clamps)" "perf-agg-ns0" "$(pick_namespace_for_client 1)"

# Edge: 6-element STORAGE_IPS_ARRAY (default profile shape, post GH #38)
echo
echo "[pick_storage_for_client] 6-element STORAGE_IPS_ARRAY (default profile)"
STORAGE_IPS_ARRAY=("10.0.0.1" "10.0.0.2" "10.0.0.3" "10.0.0.4" "10.0.0.5" "10.0.0.6")
assert_eq "idx=5 -> 10.0.0.6" "10.0.0.6" "$(pick_storage_for_client 5)"
assert_eq "idx=6 -> 10.0.0.1 (wraps)" "10.0.0.1" "$(pick_storage_for_client 6)"
assert_eq "idx=11 -> 10.0.0.6 (wraps)" "10.0.0.6" "$(pick_storage_for_client 11)"

# GH #229: native_endpoints_csv — the comma-joined endpoint list every
# client passes to `kiseki-client bench --endpoint` under
# BENCH_SPREAD_ALL=1. native_endpoints reads $ALL_STORAGE (set at
# source time from the fixture's 3 IPs); override it here per case.
echo
echo "[native_endpoints_csv] comma-joined, no trailing comma (#229)"
ALL_STORAGE="10.0.0.1 10.0.0.2 10.0.0.3"
assert_eq "3 nodes" \
  "kiseki://10.0.0.1:9103,kiseki://10.0.0.2:9103,kiseki://10.0.0.3:9103" \
  "$(native_endpoints_csv)"
ALL_STORAGE="10.0.0.1 10.0.0.2 10.0.0.3 10.0.0.4 10.0.0.5 10.0.0.6"
assert_eq "6 nodes (default profile)" \
  "kiseki://10.0.0.1:9103,kiseki://10.0.0.2:9103,kiseki://10.0.0.3:9103,kiseki://10.0.0.4:9103,kiseki://10.0.0.5:9103,kiseki://10.0.0.6:9103" \
  "$(native_endpoints_csv)"
ALL_STORAGE="10.0.0.7"
assert_eq "1 node (degenerate, no comma)" \
  "kiseki://10.0.0.7:9103" \
  "$(native_endpoints_csv)"

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
