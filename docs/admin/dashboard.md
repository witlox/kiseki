# Admin Dashboard

Kiseki includes a built-in web dashboard for cluster monitoring and
basic operations. The dashboard is served by every storage node on the
metrics HTTP port.

---

## Access

```
http://<node>:9090/ui
```

Any node in the cluster serves the full cluster-wide view. The dashboard
scrapes metrics from peer nodes in the background and aggregates them
locally. There is no dedicated dashboard server; connect to whichever
node is most convenient.

The metrics HTTP server also serves:

| Path | Purpose | Auth tier |
|------|---------|-----------|
| `/health` | Health probe (returns `200 OK`). Used by load balancers. | open |
| `/metrics` | Prometheus text exposition format. | open |
| `/ui/logo` | Kiseki logo image. | open |
| `/ui`, `/ui/api/*`, `/ui/fragment/*` | Admin dashboard. | **admin** |
| `/admin/*` | Admin REST API (topology, tenants, audit, keys, snapshots, drains). | **admin** |
| `/cluster/info` | Bootstrap topology (ADR-008 rev 2). | **client / admin** |
| `/cluster/shards/{id}/leader` | Per-shard leader probe. | **client / admin** |

### Authenticating to the admin tier

The admin and client tiers are gated by a Bearer-token middleware
(stopgap until full SSO / mTLS integration lands; see ADR-008 rev 2
§"Authorization" and `specs/findings/2026-05-15-ui-cli-followups.md`
follow-up D1).

Configure tokens on each `kiseki-server` via environment variables:

| Env var | Purpose | Default |
|---------|---------|---------|
| `KISEKI_ADMIN_TOKEN` | Bearer token accepted by `/admin/*` and `/ui/*`. Also accepted on `/cluster/info`. | unset → admin auth misconfigured (503) |
| `KISEKI_CLIENT_TOKEN` | Bearer token accepted ONLY on `/cluster/info` (clients bootstrapping topology). | unset → only the admin token works |
| `KISEKI_ADMIN_AUTH_DISABLED` | `true` opts out of admin/UI auth (local dev only). | `false` |
| `KISEKI_CLUSTER_INFO_PUBLIC` | `true` opts out of `/cluster/info` auth (LB probes, public topology). | `false` |

Present the token as a Bearer header:

```bash
# As an admin operator (full /admin/* + /ui/* + /cluster/info):
curl -H "Authorization: Bearer $KISEKI_ADMIN_TOKEN" \
     http://node1:9090/admin/topology/shards

# As a client doing topology bootstrap (only /cluster/info):
curl -H "Authorization: Bearer $KISEKI_CLIENT_TOKEN" \
     http://node1:9090/cluster/info
```

Responses:

- `200 OK` — token accepted, handler ran.
- `401 Unauthorized` — `Authorization: Bearer …` header missing or empty.
- `403 Forbidden` — Bearer present but token doesn't match the tier's ACL.
- `503 Service Unavailable` — server has no token configured AND no
  override flag set. Operator must set `KISEKI_ADMIN_TOKEN` (or
  `KISEKI_CLIENT_TOKEN` for `/cluster/info`) or the corresponding
  override flag.

Tokens are compared in constant-time over the bytes; the secrets
should be at least 32 random bytes (`openssl rand -hex 32`).

### Dev / compose deployments

`docker-compose.yml` and `docker-compose.3node.yml` set
`KISEKI_ADMIN_AUTH_DISABLED=true` and `KISEKI_CLUSTER_INFO_PUBLIC=true`
so the e2e suite and the BDD ClusterHarness can drive the spawned
servers without managing tokens. Production deployments leave both
unset.

---

## Technology

The dashboard is a single-page HTML application using:

- **HTMX** for live updates via HTML fragment polling.
- **Chart.js** for time-series and per-node comparison charts.
- No build step, no JavaScript framework, no node_modules.

The dashboard HTML is embedded in the `kiseki-server` binary at compile
time (`include_str!`). No external files to deploy or manage.

---

## Overview tab

The main view shows six metric cards at the top, a time-series chart
in the middle, and a node table at the bottom. All data refreshes
automatically via HTMX polling.

### Metric cards

| Card | Source metric | Description |
|------|-------------|-------------|
| Cluster Health | Node liveness | `N/M nodes healthy` with color coding: green (all healthy), yellow (degraded), red (all down). |
| Raft Entries | `kiseki_raft_entries_total` | Total Raft entries applied across the cluster. |
| Gateway Requests | `kiseki_gateway_requests_total` | Total S3 and NFS requests served. |
| Data Written | `kiseki_chunk_write_bytes_total` | Aggregate chunk bytes written. |
| Data Read | `kiseki_chunk_read_bytes_total` | Aggregate chunk bytes read. |
| Connections | `kiseki_transport_connections_active` | Active transport connections. |

Numbers are formatted with SI suffixes (K, M, B) and byte units (KB,
MB, GB, TB) for readability.

### Time-series charts

The dashboard stores up to 3 hours of metric history (configurable) in
memory. Time-series charts show:

- Raft entries over time
- Gateway request rate
- Chunk write/read throughput
- Connection count

Historical data is available via the API:

```bash
# Get 3 hours of history (default)
curl http://node1:9090/ui/api/history

# Get 1 hour of history
curl http://node1:9090/ui/api/history?hours=1
```

### Node table

A table listing every node in the cluster with per-node metrics:

| Column | Description |
|--------|-------------|
| Node | Node address (hostname:port) |
| Status | Health badge: green "Healthy" or red "Unreachable" |
| Raft | Raft entries applied by this node |
| Requests | Gateway requests served by this node |
| Written | Chunk bytes written by this node |
| Read | Chunk bytes read by this node |
| Conns | Active transport connections on this node |

Click a node row to drill down to the node detail view.

---

## Performance tab

The performance tab shows per-node comparison charts for identifying
hotspots and imbalances:

- **Write throughput by node**: Bar chart comparing chunk bytes written
  per node.
- **Read throughput by node**: Bar chart comparing chunk bytes read per
  node.
- **Request count by node**: Bar chart comparing gateway requests per
  node.

Chart data is sourced from the chart-data API:

```bash
curl http://node1:9090/ui/fragment/chart-data
# Returns: {"labels": [...], "writes": [...], "reads": [...], "requests": [...]}
```

---

## Topology tab

The Topology tab shows ADR-008 rev 2 cluster routing state: per-shard
leader assignments and proxy-fallback / stale-leader counters.

| Panel | Source |
|-------|--------|
| Per-shard leaders | `/ui/fragment/topology-shards` (mirrors `/cluster/info` `shards`) |
| Proxy fallback toggle | `KISEKI_NATIVE_PROXY_FALLBACK` env var |
| Proxy forwards | `kiseki_native_proxy_forwards_total{source_node,leader_node}` |
| Stale-leader redirects | `kiseki_native_topology_stale_leader_redirects_total{protocol}` |

Operators can also pull this via `kiseki-admin shards` and
`kiseki-admin forwarding` (both honour `--json`).

---

## Pools tab

The Pools tab surfaces per-pool capacity from
`kiseki_pool_capacity_total_bytes` and `kiseki_pool_capacity_used_bytes`,
**plus** per-device capacity and IO error counters from
`kiseki_pool_device_capacity_bytes{pool,device_id,kind=total|used|free}`
and `kiseki_pool_device_errors_total{device_id,op=read|write}` (D2).

The same data is reachable via:

- `GET /admin/pools` → `{pools: [...], devices: [...]}`

The full per-device health stream (latency histograms, repair queue)
still lives behind the `StorageAdminService` gRPC handler — see
`kiseki-storage device-health` and `kiseki-storage repairs list`.
The Pools tab links to those commands for the deep view; the gauges
covered here are the always-on Prometheus surface so alert rules can
target individual devices without scraping gRPC.

---

## Tenants tab

The Tenants tab lists the three-level tenant hierarchy
(Organization → Project → Workload) plus namespaces, all read from the
in-process `TenantStore` and `NamespaceStore` (ADR-009 / I-T1..I-T4).

The same data is reachable via:

- `GET /admin/tenants/orgs`
- `GET /admin/tenants/projects`
- `GET /admin/tenants/workloads`
- `GET /admin/tenants/namespaces`

`kiseki-admin tenant list --type org|project|workload|namespace`
prints the JSON or a tabular view.

Create + describe verbs (D3) are now available over HTTP:

- `POST /admin/tenants/orgs`
- `POST /admin/tenants/projects` — body `{org_id, name}`
- `POST /admin/tenants/workloads` — body `{project_id, name}`
- `POST /admin/tenants/namespaces` — body `{workload_id, name}`
- `GET /admin/tenants/describe?id=<id>` — auto-detects type
- `POST /admin/tenants/delete` — currently `501`; gRPC ControlService
  remains canonical for lifecycle deletes

CLI equivalents:

```bash
kiseki-admin tenant create-project <org-id> <name>
kiseki-admin tenant create-workload <project-id> <name>
kiseki-admin tenant create-namespace <workload-id> <name>
kiseki-admin tenant describe <id>
kiseki-admin tenant delete <id> [--yes]
```

---

## Audit tab

The Audit tab shows the most recent events from the system audit
shard.  This is the compliance-facing trail (key rotations, namespace
lifecycle, drain requests, ...) — *distinct* from the Alerts tab,
which surfaces severity-driven operational state.

Backed by `GET /admin/audit/query` (tenant-scoped queries take
`?tenant=<uuid>` and event-type filters take `?event_type=key-rotation`
or any other variant in `AuditEventType`).  The CLI equivalent is
`kiseki-admin audit query [--tenant T] [--type X] [--limit N]`.

**Cross-node aggregation (D5)**: by default, `/admin/audit/query`
fans out to every peer in `/cluster/info`'s `peers[]`, merges the
results, dedupes by `(node_id, tenant_id, sequence)`, and truncates
to `limit`. The response includes `aggregated: true` plus
`reachable_nodes` and `unreachable_nodes` lists so operators can
spot partial-fan-out runs.

Pass `?local_only=true` (HTTP) or `--local-only` (CLI) to query
only the responding node's audit shard. Per-peer fetches use a 5-second
read timeout — a slow peer cannot pin the coordinator.

---

## Alerts tab

The alerts tab shows health status and capacity warnings. Each alert is
a row with a colored dot (green, yellow, red, blue), a message, and a
timestamp.

### Alert types

| Dot | Meaning | Example |
|-----|---------|---------|
| Green | All clear | "All 3 nodes healthy" |
| Red | Critical | "Node node2:9100 unreachable" |
| Blue | Informational | "Capacity monitoring active (3 nodes reporting)" |
| Green | Activity | "node1:9100: 1.2K gateway requests served" |

Alerts are generated by comparing the current cluster state against
expected conditions. The alert endpoint returns HTML fragments for HTMX
polling:

```bash
curl http://node1:9090/ui/fragment/alerts
```

---

## Operations tab

The operations tab provides buttons for common administrative actions.
Each action calls a REST endpoint and records an event in the diagnostic
event store.

### Available operations

| Operation | Endpoint | Method | Description |
|-----------|----------|--------|-------------|
| Maintenance Mode | `/ui/api/ops/maintenance` | POST | Enable or disable cluster-wide maintenance mode. Body: `{"enabled": true}` or `{"enabled": false}`. |
| Backup | `/ui/api/ops/backup` | POST | Initiate a background backup. |
| Scrub | `/ui/api/ops/scrub` | POST | Initiate a background integrity scrub. |

Example:

```bash
# Enable maintenance mode
curl -X POST http://node1:9090/ui/api/ops/maintenance \
  -H 'Content-Type: application/json' \
  -d '{"enabled": true}'

# Trigger a scrub
curl -X POST http://node1:9090/ui/api/ops/scrub
```

All operations return `{"status": "ok", "message": "..."}` on success.

---

## Node drill-down

Click a node in the node table to see its detailed view. The drill-down
shows:

- Node-specific metric history (time-series)
- Device health for devices attached to that node
- Shard assignments on that node
- Raft role (leader/follower/learner) per shard

---

## API endpoints

All dashboard data is available via JSON APIs for scripting and
integration:

| Endpoint | Method | Description |
|----------|--------|-------------|
| `/ui/api/cluster` | GET | Cluster summary: healthy nodes, total nodes, aggregate metrics. |
| `/ui/api/nodes` | GET | List of all nodes with per-node metrics and health status. |
| `/ui/api/history` | GET | Time-series metric history. Query: `?hours=3` (default). |
| `/ui/api/events` | GET | Diagnostic event log. Query parameters below. |

### Event log query parameters

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `severity` | string | (all) | Filter by severity: `info`, `warning`, `error`, `critical`. |
| `category` | string | (all) | Filter by category: `node`, `shard`, `device`, `tenant`, `security`, `admin`, `gateway`, `raft`. |
| `hours` | float | 3 | Hours to look back. |
| `limit` | integer | 100 | Maximum events to return. |

Example:

```bash
# Get last 50 error events in the past hour
curl 'http://node1:9090/ui/api/events?severity=error&hours=1&limit=50'
```

Response format:

```json
{
  "count": 2,
  "events": [
    {
      "timestamp": "2026-04-23T14:30:00Z",
      "severity": "error",
      "category": "device",
      "source": "nvme-0001",
      "message": "Device SMART wear exceeds 90%"
    }
  ]
}
```

---

## Cluster-wide view architecture

Every node in the cluster runs the same dashboard. The cluster-wide
view is assembled by scraping `/metrics` from peer nodes:

1. Each node knows its peers from `KISEKI_RAFT_PEERS`.
2. A background task scrapes each peer's `/metrics` endpoint at a
   configurable interval (default 10 seconds).
3. Scraped metrics are cached locally in a `MetricsAggregator`.
4. Dashboard requests aggregate local + cached peer metrics.

This means:

- **No single point of failure.** Any node serves the dashboard.
- **Stale data tolerance.** If a peer is unreachable, the dashboard
  shows the last known state and marks the node as "Unreachable."
- **No additional infrastructure.** No dedicated monitoring server is
  needed for basic cluster visibility.

For production monitoring with alerting and long-term retention, use
Prometheus and Grafana (see [Monitoring](monitoring.md)).
