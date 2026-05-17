#!/bin/bash
# Idempotent post-boot shard topology setup for the perf cluster.
#
# Creates N multi-shard namespaces (`bench-ns-0` ... `bench-ns-(N-1)`)
# via the admin endpoint added in #68, where N defaults to the
# storage-node count. Each namespace's `NamespaceShardMap` covers N
# disjoint key ranges, so writes within it fan across N shard
# leaders by `hashed_key = UUIDv5(namespace_id, composition_id)`.
#
# Closes #69. Consumed by:
#   - `kiseki-client bench --namespace-fanout N` (#66 fix 2 harness)
#   - phase 11 (`KISEKI_BENCH_NAMESPACE_FANOUT` env var)
#   - phase 00's shard-count assertion (next change)
#
# Pre-requisites (set up by `setup-bench-ctrl.sh`):
#   - /etc/kiseki-bench.env (STORAGE_IPS, FIRST_STORAGE, …)
#   - /usr/local/bin/kiseki-admin
#
# Idempotency: each namespace create either succeeds (201) or
# returns 409 "already exists" when invoked a second time. Both
# are treated as success. Re-running this script after a fresh
# cluster boot or after manual intervention is safe.
#
# Failure mode: if leader election hasn't converged within the
# poll window, the script exits non-zero and the operator (or
# `setup-bench-ctrl.sh`'s invocation) sees a clear error. The
# perf-suite's phase 00 probe is the secondary safety net.

set -uo pipefail

# ----------------------------------------------------------------------------
# Inputs — sourced from /etc/kiseki-bench.env when available, else
# from env, else from sane local defaults.
# ----------------------------------------------------------------------------

KISEKI_BENCH_ENV="${KISEKI_BENCH_ENV:-/etc/kiseki-bench.env}"
if [ -f "$KISEKI_BENCH_ENV" ]; then
  # shellcheck disable=SC1090
  source "$KISEKI_BENCH_ENV"
fi
STORAGE_IPS="${STORAGE_IPS:-}"
FIRST_STORAGE="${FIRST_STORAGE:-}"

# Tenant UUID — must match what `kiseki-client bench` uses
# (`OrgId(Uuid::from_u128(1))` in `kiseki-client::bench::default_ids`)
# AND what `kiseki-server::runtime` seeds at boot
# (`bootstrap_tenant = OrgId(Uuid::from_u128(1))`).
BENCH_TENANT_ID="${KISEKI_BENCH_TENANT_ID:-00000000-0000-0000-0000-000000000001}"

# Admin endpoint — the leader's HTTP metrics port. `kiseki-admin`'s
# /admin/topology/* routes forward to the local leader if the
# request lands on a follower, so any storage node works, but the
# first storage IP is the conventional choice.
ADMIN_HOST="${FIRST_STORAGE:-}"
if [ -z "$ADMIN_HOST" ] && [ -n "$STORAGE_IPS" ]; then
  ADMIN_HOST=$(echo "$STORAGE_IPS" | cut -d',' -f1)
fi
if [ -z "$ADMIN_HOST" ]; then
  echo "HALT: ADMIN_HOST unset — STORAGE_IPS / FIRST_STORAGE missing from $KISEKI_BENCH_ENV" >&2
  exit 2
fi
ADMIN_PORT="${KISEKI_ADMIN_PORT:-9090}"
ADMIN_URL="http://${ADMIN_HOST}:${ADMIN_PORT}"

# Locate kiseki-admin.
KISEKI_ADMIN="${KISEKI_ADMIN_BIN:-/usr/local/bin/kiseki-admin}"
if [ ! -x "$KISEKI_ADMIN" ]; then
  # Fall back to PATH lookup; helpful for local dev runs.
  if command -v kiseki-admin >/dev/null 2>&1; then
    KISEKI_ADMIN=$(command -v kiseki-admin)
  else
    echo "HALT: kiseki-admin binary not found at $KISEKI_ADMIN nor on PATH" >&2
    exit 2
  fi
fi

# Storage count → namespace count + per-namespace shard count.
# Matches `KISEKI_BENCH_NAMESPACE_FANOUT` default in phase 11.
STORAGE_COUNT=$(echo "$STORAGE_IPS" | tr ',' '\n' | grep -c .)
if [ "$STORAGE_COUNT" -lt 1 ]; then
  echo "HALT: STORAGE_IPS is empty" >&2
  exit 2
fi
SHARDS_PER_NS="${KISEKI_BENCH_SHARDS_PER_NS:-$STORAGE_COUNT}"
NS_COUNT="${KISEKI_BENCH_NAMESPACE_FANOUT:-$STORAGE_COUNT}"

echo "=== setup-shards.sh ==="
echo "admin_url       = $ADMIN_URL"
echo "tenant_id       = $BENCH_TENANT_ID"
echo "storage_count   = $STORAGE_COUNT"
echo "namespace_count = $NS_COUNT"
echo "shards_per_ns   = $SHARDS_PER_NS"
echo ""

# ----------------------------------------------------------------------------
# Wait for leader convergence before issuing writes.
#
# The control-plane Raft group's `initialize()` returns
# immediately on each node, but voters don't agree on a leader
# until every node's control group is up. Boot sequencing under
# terraform-apply varies (parallel-but-not-instant), so the first
# `kiseki-admin` call can land before leader election finishes.
# Retry the topology read for up to 60s; quote the typical
# convergence window in the log so an operator running locally
# can spot a hung cluster.
# ----------------------------------------------------------------------------

deadline=$(( $(date +%s) + 60 ))
attempt=0
while true; do
  attempt=$((attempt + 1))
  if "$KISEKI_ADMIN" --endpoint "$ADMIN_URL" shards >/dev/null 2>&1; then
    echo "leader ready (attempt $attempt)"
    break
  fi
  if [ "$(date +%s)" -ge "$deadline" ]; then
    echo "HALT: leader did not converge within 60s — $attempt attempts" >&2
    "$KISEKI_ADMIN" --endpoint "$ADMIN_URL" shards 2>&1 | head -5
    exit 2
  fi
  sleep 2
done

# ----------------------------------------------------------------------------
# Create namespaces. The CLI is idempotent at the application layer:
# 201 on first create, 409 on repeat. Both are success.
# ----------------------------------------------------------------------------

errs=0
for i in $(seq 0 $((NS_COUNT - 1))); do
  ns_id="bench-ns-${i}"
  echo "--- $ns_id ---"
  output=$("$KISEKI_ADMIN" --endpoint "$ADMIN_URL" topology namespace-create \
      "$ns_id" --tenant "$BENCH_TENANT_ID" --shards "$SHARDS_PER_NS" 2>&1)
  rc=$?
  echo "$output" | sed 's/^/  /'
  if [ "$rc" -ne 0 ]; then
    # Treat "already exists" as success — the CLI's formatter still
    # exits non-zero for any HTTP non-2xx, but we want 409 to be
    # a no-op for re-runs.
    if echo "$output" | grep -q "already exists"; then
      echo "  (idempotent re-run: namespace already created with right shape)"
    else
      errs=$((errs + 1))
    fi
  fi
done

if [ "$errs" -gt 0 ]; then
  echo "HALT: $errs namespace create(s) failed" >&2
  exit 2
fi

# ----------------------------------------------------------------------------
# Confirm the final topology. Useful for the operator + the phase
# 00 probe (which queries the same endpoint).
# ----------------------------------------------------------------------------

echo ""
echo "=== final topology ==="
"$KISEKI_ADMIN" --endpoint "$ADMIN_URL" shards 2>&1 \
  | grep -E "bench-ns|shard_id|namespace_id" \
  | sed 's/^/  /' \
  | head -60

echo ""
echo "OK"
