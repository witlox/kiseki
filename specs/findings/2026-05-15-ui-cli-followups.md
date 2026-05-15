# 2026-05-15 — Operator + user visibility follow-ups

Context: this is the deferred-work doc for the UI / CLI visibility
landing (commit on `feat/ui-cli-visibility`). Six categories were in
scope (dashboard tabs, kiseki-admin, kiseki-client, cross-cutting CLI,
docs, tests). The work shipped what could be done without rabbit-
holing into auth/RBAC, gRPC plumbing changes, or new ADRs.

Each item below is the **smallest thing** that would unblock further
work — not the whole future road. Reference ADRs in parentheses
where the design already exists.

## Deferred items

### 1. RBAC / authn on the admin HTTP surface (ADR-038 §D4, ADR-014)

**Status (2026-05-15)**: **PARTIALLY RESOLVED** by branch
`feat/admin-rbac-auth` — Bearer-token stopgap landed; SAN-based
binding remains future work.

The new `/admin/*` endpoints inherit the existing posture: the
metrics HTTP port is operator-only and firewalled. There is no
per-request auth.

What's deferred: a proper SAN-based authz gate over the admin HTTP
surface so the CLI can be exposed beyond the admin VLAN. The same
hook should bridge `/admin/audit/query`'s tenant filtering: today
the endpoint trusts the operator query (`?tenant=<uuid>`); a
tenant-scoped client should only see its own events.

Smallest next step: lift the gRPC `super::authz::require_admin`
gate to a `tower-http` middleware layer and apply it to the new
`/admin/*` routes.

<<<<<<< HEAD
**Resolution (interim)**: `crates/kiseki-server/src/web/auth.rs`
introduces an `admin_required` Axum middleware applied to `/admin/*`
and `/ui/*` route groups by `web::api::ui_router`. The middleware
requires `Authorization: Bearer <KISEKI_ADMIN_TOKEN>` by default;
`KISEKI_ADMIN_AUTH_DISABLED=true` provides a documented dev opt-out.
`KISEKI_CLIENT_TOKEN` is recognised on `/cluster/info` only — the
two-tier ACL is the smallest viable shape that lets CLI clients
bootstrap topology without granting them admin powers. Operator
guidance is in `docs/admin/dashboard.md` §"Authenticating to the
admin tier".

**Still deferred**: SAN-based binding (so the token carries a
tenant identity and `/admin/audit/query`'s `?tenant=<uuid>`
filter can be enforced server-side), full SSO integration via
Keycloak, and per-tenant filtering of `/cluster/info` `shards[]`.
The Bearer stopgap is structurally compatible — the same
middleware can switch its check from "matches `KISEKI_ADMIN_TOKEN`"
to "carries an admin claim verified against an issuer" without
touching the route registration.

### 2. Per-device health + repair queue in the Pools tab — RESOLVED 2026-05-15 (D2)

Landed in `feat/admin-cli-completions`. `KisekiMetrics` now exposes:

- `kiseki_pool_device_capacity_bytes{pool, device_id, kind}` where
  `kind ∈ {total, used, free}`.
- `kiseki_pool_device_errors_total{device_id, op}` where
  `op ∈ {read, write}`.

`GET /admin/pools` returns a `devices: [...]` array alongside the
existing per-pool view. `/ui/fragment/pools-table` renders both.
Wiring the gauges from `FileBackedDevice` stats is the remaining
mechanical step; the metric surface, parser, and dashboard render
path are in place. Repair-queue still lives behind
`StorageAdminService.repairs list` — Phase II.

### 3. Tenant CRUD beyond `create-org` — RESOLVED 2026-05-15 (D3)

`kiseki-admin tenant` now exposes `create-project`, `create-workload`,
`create-namespace`, `describe`, and `delete [--yes]`. HTTP surfaces:

- `POST /admin/tenants/projects` — body `{org_id, name}`
- `POST /admin/tenants/workloads` — body `{project_id, name}` (org_id
  resolved server-side via the project lookup).
- `POST /admin/tenants/namespaces` — body `{workload_id, name}` (org+
  project resolved server-side).
- `GET /admin/tenants/describe?id=<id>` — auto-detects type.
- `POST /admin/tenants/delete` — currently returns `501`; CLI
  scaffolding + typed-id confirmation are in place, gRPC
  `ControlService` remains canonical for lifecycle deletes.

### 4. Crypto-shred (`kiseki-admin keys shred`) — RESOLVED 2026-05-15 (D4)

`POST /admin/keys/shred` records a `KeyDestruction` audit event via
`kiseki_audit::event::crypto_shred_force_override_event`. The
tenant-side KMS destruction remains the authoritative step (ADR-014
§K11 — KMS loss = data loss). The CLI prompts the operator to retype
the tenant id unless `--yes` is passed. See
`docs/admin/key-management.md` § "Operator CLI" for the runbook.

### 5. Multi-node `kiseki-admin config show --all` quality of life

Unchanged — still emits per-node JSON. A "knob X disagrees on nodes
1 and 3" diff renderer is the next QoL win.

### 6. Audit-store backing for `/admin/audit/query` — PARTIALLY RESOLVED 2026-05-15 (D5)

`/admin/audit/query` now fans out to all peers in `node_info.peers`
by default, merges results, dedupes by `(node_id, tenant_id, sequence)`,
and truncates to `limit`. Per-peer fetches use `tokio::net::TcpStream`
with a 5-second read timeout — no new dependency. The CLI gains a
`--local-only` opt-out flag.

Still deferred: replacing the in-memory `AuditLog` with the
`RaftAuditStore` so per-tenant shards are durable. The aggregation
layer added here works against either backend because it goes
through `AuditOps`.

### 7. SAN identity in `kiseki-client whoami` — RESOLVED 2026-05-15 (D6)

`GET /admin/whoami` echoes the client SAN from
`x-kiseki-client-san` / `x-ssl-client-san` /
`x-forwarded-client-cert` (set by a TLS-terminating reverse proxy
in front of the metrics listener). `kiseki-client whoami` calls
`/admin/whoami` first, falls back to `/cluster/info` on older
servers, and prints `(no SAN) — connection is not
mTLS-authenticated` when no header is present.

### 8. Drain orchestrator not pumped by openraft

The new `/admin/drains` POST endpoint creates a drain record in
the orchestrator but the **execution** (voter-replacement step
loop, ADR-035 §5) requires a `RaftMembershipAdapter`. The runtime
does not yet wire one. Operators using the CLI today will see
the request recorded, but no actual membership change. The
existing test path in `kiseki-control/src/node_lifecycle.rs`
uses a mock adapter; the production wiring lands when the openraft
control-plane membership API is exposed.

## What landed

- Dashboard tabs: Topology, Pools, Tenants, Audit.
- HTTP endpoints: `/admin/topology/{shards,forwarding}`,
  `/admin/pools`, `/admin/tenants/{orgs,projects,workloads,namespaces}`,
  `/admin/audit/query`, `/admin/config`, `/admin/keys/{status,rotate}`,
  `/admin/snapshots`, `/admin/snapshots/restore`, `/admin/drains`,
  `/admin/drains/cancel`.
- `kiseki-admin` subcommands: `shards`, `forwarding`, `audit query`,
  `tenant list`, `tenant create-org`, `snapshot {create,list,restore}`,
  `drain [status|cancel]`, `keys {status,rotate}`,
  `config show [--node N | --all]`, plus `--json` global flag and
  `--version`.
- `kiseki-client` subcommands: `mount --seeds`, `whoami`,
  `namespaces list`, `quota`, `topology`, plus `--version`.
- `TenantStore::list_projects` / `list_workloads` (the gRPC
  `ControlService` was always going to need them anyway).
- `ControlGrpc::with_namespaces` constructor so the gRPC and
  admin-UI share the same `Arc<NamespaceStore>`.

## Hard constraints honoured

- `/cluster/info` JSON shape unchanged (Step C territory).
- `kiseki-gateway/src/s3_server.rs`, `mem_gateway.rs`, and
  `native/proxy_client.rs` untouched.
- No new ADRs; references to ADR-008 rev 2, ADR-014, ADR-035,
  ADR-009, ADR-007, ADR-042 §4 / §3.1, ADR-040.
- `Cargo.toml` workspace version not bumped.
