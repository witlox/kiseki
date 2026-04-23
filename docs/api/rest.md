# REST & Admin API

The `kiseki-server` binary exposes an HTTP server (default port 9090) for
health checks, Prometheus metrics, and an admin dashboard. All endpoints
are served via axum.

---

## Health and metrics

### GET /health

Liveness probe for load balancers and orchestrators.

**Response**: `200 OK` with body `ok` when the server is running.

### GET /metrics

Prometheus text-format metrics endpoint.

**Response**: `200 OK` with `text/plain` body containing all registered
Prometheus metrics including:

- Raft state per shard (leader, follower, candidate)
- Chunk operations (reads, writes, dedup hits)
- Transport metrics (connections, bytes, errors per transport type)
- Pool utilization (capacity, used, free per pool)
- View materialization lag
- Advisory budget usage

---

## Admin dashboard

### GET /ui

HTML admin dashboard with HTMX live polling. Provides a visual overview
of cluster health, node status, and operational metrics.

The dashboard polls the JSON API endpoints below for live updates.

---

## JSON API endpoints

### GET /ui/api/cluster

Cluster-wide summary with aggregated metrics from all nodes.

**Response**: JSON object with node count, total capacity, total used,
shard count, and aggregated health status.

### GET /ui/api/nodes

List of all known nodes with per-node metrics.

**Response**: JSON array of node objects, each with node ID, address,
status, device count, shard count, and key metrics.

### GET /ui/api/history

Metric time series for charting.

**Query parameters**:

| Parameter | Type | Default | Description |
|---|---|---|---|
| `hours` | float | 3 | Number of hours of history to retrieve |

**Response**: JSON object with `hours` and `points` array containing
timestamped metric snapshots.

### GET /ui/api/events

Filtered event log for diagnostics and alerting.

**Query parameters**:

| Parameter | Type | Default | Description |
|---|---|---|---|
| `severity` | string | (all) | Filter by severity: `info`, `warning`, `error`, `critical` |
| `category` | string | (all) | Filter by category: `node`, `shard`, `device`, `tenant`, `security`, `admin` |
| `hours` | float | 3 | Hours to look back |

**Response**: JSON array of event objects with timestamp, severity,
category, message, and source.

---

## Operations endpoints

These endpoints trigger operational actions and require cluster admin
authentication.

### POST /ui/api/ops/maintenance

Toggle maintenance mode for the cluster or specific shards.

**Request body**: JSON with `enabled` (boolean) and optional `shard_id`.

**Effect**: Sets shards to read-only. Write commands are rejected with a
retriable error (I-O6). Shard splits, compaction, and GC for in-progress
operations continue.

### POST /ui/api/ops/backup

Trigger a backup operation.

**Request body**: JSON with backup configuration parameters.

**Effect**: Initiates backup per ADR-016. Returns a job ID for status
tracking.

### POST /ui/api/ops/scrub

Trigger a data integrity scrub.

**Request body**: JSON with optional scope (pool, device, or cluster-wide).

**Effect**: Verifies chunk integrity via EC checksums. Reports corrupt or
missing chunks. Triggers automatic repair for recoverable issues.

---

## HTMX fragment endpoints

These endpoints return HTML fragments for the admin dashboard's live
polling:

| Endpoint | Description |
|---|---|
| `GET /ui/fragment/cluster-cards` | Cluster status summary cards |
| `GET /ui/fragment/node-table` | Node list table rows |
| `GET /ui/fragment/chart-data` | Chart data for metrics graphs |
| `GET /ui/fragment/alerts` | Active alerts and warnings |
