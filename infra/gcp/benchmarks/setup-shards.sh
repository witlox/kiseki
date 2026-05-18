#!/bin/bash
# Idempotent post-boot shard topology setup for the perf cluster.
#
# Creates the dedicated bench namespace (`kiseki-bench`, owned by the
# `kiseki-bench-tenant`) with multi-shard topology via the admin
# endpoint added in #68. Run after cluster boot, before `kiseki-client
# bench`.
#
# Why a dedicated bench namespace (and not the system "default"):
#   - Per ADR-033 §1, the sharding formula `max(min(3×N, 64), 3)`
#     targets tenant-admin-created namespaces with parallel-write
#     workloads. The system "default" namespace serves casual S3
#     traffic + BDD scenarios that are sequential by shape; multi-
#     shard there is overhead with no benefit. So the bench gets
#     its own namespace.
#   - The bench client (`kiseki-client bench`) defaults to the same
#     namespace via `bench_default_ids()` so the operator + client
#     agree on the topology without flags.
#
# Pre-requisites (set up by `setup-bench-ctrl.sh`):
#   - /etc/kiseki-bench.env (STORAGE_IPS, FIRST_STORAGE, ...)
#   - /usr/local/bin/kiseki-admin
#
# Idempotency: the admin endpoint returns 409 on repeat with the
# existing shard count. Treated as success. Safe to re-run after
# a fresh cluster boot or after manual intervention.

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

# Deterministic bench tenant + namespace UUIDs. These MUST match
# `kiseki-client::bench::bench_default_ids` so the client and the
# admin-side setup agree without flags:
#   tenant_id    = UUIDv5(NAMESPACE_DNS, "kiseki-bench-tenant")
#                = 179e565c-d506-5c59-8f82-7ae6e13f0aff
#   namespace_id = UUIDv5(NAMESPACE_DNS, "kiseki-bench")
#                = 6658810a-1c4d-564c-a888-7564b5e9e576
# Verify locally:
#   python3 -c "import uuid; print(uuid.uuid5(uuid.NAMESPACE_DNS, 'kiseki-bench-tenant'))"
BENCH_TENANT_ID="${KISEKI_BENCH_TENANT_ID:-179e565c-d506-5c59-8f82-7ae6e13f0aff}"
BENCH_NAMESPACE_ID="${KISEKI_BENCH_NAMESPACE_ID:-6658810a-1c4d-564c-a888-7564b5e9e576}"

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
  if command -v kiseki-admin >/dev/null 2>&1; then
    KISEKI_ADMIN=$(command -v kiseki-admin)
  else
    echo "HALT: kiseki-admin binary not found at $KISEKI_ADMIN nor on PATH" >&2
    exit 2
  fi
fi

# Shard count = formula(node_count). Default = 3 × node_count
# (capped at 64, floored at 3 per ADR-033 §1). The admin endpoint
# defaults to this when `--shards` is omitted; we let it pick.
STORAGE_COUNT=$(echo "$STORAGE_IPS" | tr ',' '\n' | grep -c .)
if [ "$STORAGE_COUNT" -lt 1 ]; then
  echo "HALT: STORAGE_IPS is empty" >&2
  exit 2
fi
SHARDS="${KISEKI_BENCH_SHARDS:-}"  # empty → server-side formula

echo "=== setup-shards.sh ==="
echo "admin_url      = $ADMIN_URL"
echo "tenant_id      = $BENCH_TENANT_ID"
echo "namespace_id   = $BENCH_NAMESPACE_ID"
echo "storage_count  = $STORAGE_COUNT"
echo "shards         = ${SHARDS:-<server-default (formula)>}"
echo ""

# ----------------------------------------------------------------------------
# Wait for leader convergence before issuing writes.
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
# Create the bench namespace. 201 on first create, 409 on repeat;
# both are success.
# ----------------------------------------------------------------------------

echo "--- creating $BENCH_NAMESPACE_ID ---"
admin_args=("--endpoint" "$ADMIN_URL" "topology" "namespace-create" \
            "$BENCH_NAMESPACE_ID" "--tenant" "$BENCH_TENANT_ID")
if [ -n "$SHARDS" ]; then
  admin_args+=("--shards" "$SHARDS")
fi
output=$("$KISEKI_ADMIN" "${admin_args[@]}" 2>&1)
rc=$?
echo "$output" | sed 's/^/  /'
if [ "$rc" -ne 0 ]; then
  if echo "$output" | grep -q "already exists"; then
    echo "  (idempotent re-run: namespace already created)"
  else
    echo "HALT: namespace create failed (rc=$rc)" >&2
    exit 2
  fi
fi

# ----------------------------------------------------------------------------
# Confirm the final topology.
# ----------------------------------------------------------------------------

echo ""
echo "=== final topology ==="
"$KISEKI_ADMIN" --endpoint "$ADMIN_URL" shards 2>&1 \
  | grep -E "$BENCH_NAMESPACE_ID|shard_id|namespace_id" \
  | sed 's/^/  /' \
  | head -40

echo ""
echo "OK"
