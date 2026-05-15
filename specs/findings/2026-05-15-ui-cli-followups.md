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

### 2. Per-device health + repair queue in the Pools tab

`kiseki-storage device-health` and `kiseki-storage repairs list`
talk to the `StorageAdminService` gRPC handler (in
`kiseki-server/src/storage_admin.rs`). Those signals are *not*
mirrored into `KisekiMetrics` (the Prometheus registry), so the
new `/admin/pools` HTTP endpoint can't surface them without
either:

- Adding new gauges to `KisekiMetrics` populated by the same
  drivers (cheap, repeats data already in gRPC).
- Calling out to the StorageAdmin gRPC handler from the HTTP
  endpoint (clean, requires a tonic Channel inside the metrics
  server).

The dashboard's Pools tab links to the gRPC-only CLI verbs in its
notice card.

### 3. Tenant CRUD beyond `create-org`

`kiseki-admin tenant` exposes `list` and `create-org` today.
`create-project / create-workload / create-namespace` require:

- Resolving the parent org-id / project-id (extra fetch),
- Mapping compliance-tag enum strings to the proto values,
- Honouring authorization (org-scoped admin only).

The gRPC `ControlService.{CreateProject,CreateWorkload,CreateNamespace}`
handlers already exist; the CLI wrapping isn't free for an HTTP-only
binary. Operators currently use the gRPC service directly (see
`docs/api/grpc.md`).

### 4. Crypto-shred (`kiseki-admin keys shred`)

The HTTP `/admin/keys/rotate` endpoint is wired to
`KeyManagerOps::rotate`. Crypto-shred (`destroy`) is a more
dangerous operation that:

- Requires a tenant-id argument (system-wide shred is not the
  same surface).
- Should emit an `AuditEvent::KeyDestruction` (covered by
  `crypto_shred_force_override_event`).
- Needs a "you really mean it" prompt — implemented in the CLI as
  `--yes`, but only after the HTTP endpoint exists.

Smallest next step: a `POST /admin/keys/shred` endpoint that takes
`{tenant_id, force, reason}`. The audit event helper already
exists in `kiseki-audit::event::crypto_shred_force_override_event`.

### 5. Multi-node `kiseki-admin config show --all` quality of life

`config show --all` works (scrapes `/cluster/info` peers and hits
`/admin/config` on each), but emits a synthesised JSON object
`{"nodes": [...]}` whose human renderer (`format_config_all`)
just stacks per-node tables. A unified diff view ("knob X
disagrees on nodes 1 and 3") would help operators spot
configuration drift.

### 6. Audit-store backing for `/admin/audit/query`

The runtime wires the in-memory `kiseki_audit::AuditLog`. In a
multi-node cluster, querying any one node returns only that
node's local audit shard — there is no cross-node aggregation.
ADR-009 specifies per-tenant audit shards backed by the same
Raft groups as the data path; once `RaftAuditStore` is wired into
the runtime (`crates/kiseki-audit/src/raft_store.rs` already
exists), the same query endpoint will Just Work because it goes
through `AuditOps`.

### 7. SAN identity in `kiseki-client whoami`

Today `whoami` reads `KISEKI_TENANT_ID` from the environment and
the node id from `/cluster/info`. The SAN that identifies the
client against the data-path mTLS handshake is consumed by the
`san_interceptor` in `kiseki-gateway/src/native/` but is not
echoed on any HTTP endpoint. Adding `/admin/whoami` that returns
the headers / TLS info the request was authed with would close
the loop.

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
