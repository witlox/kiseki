# Observability + Authentication Subplan

**Date**: 2026-04-23
**Parent**: `specs/implementation/mvp-to-production-plan.md`
**Workstream items**: 6.1, 6.2, 6.3, 5.1, 5.4, 1.1, 1.3

## Baseline

- **Logging**: 73 `eprintln!` calls across codebase. No structured logging
  framework. `tracing` not in workspace deps.
- **Metrics**: Custom `TransportMetrics` (in-memory counters + latency window).
  No Prometheus or external backend.
- **S3 Auth**: No SigV4. Bootstrap tenant_id hardcoded per gateway instance.
- **Multi-tenant**: Static tenant per gateway. No per-request extraction.
- **Key rotation**: Epoch management + rewrap worker + crypto-shred all
  implemented. Missing: background rotation monitor task.
- **OpenTelemetry**: Not present.

## Revised scope

Items 1.1 (key rotation) and 1.3 (crypto-shred) are mostly done — only the
background monitor and propagation wiring remain. This reduces the slice.

---

## Phase A: Structured Logging (WS 6.1)

**Goal**: Replace all `eprintln!` with `tracing` spans and events.

### A1: Add tracing infrastructure

Workspace `Cargo.toml`:
- Add `tracing = "0.1"`, `tracing-subscriber = { version = "0.3", features = ["json", "env-filter"] }`

`kiseki-server/src/main.rs`:
- Initialize `tracing_subscriber` with `EnvFilter` (from `RUST_LOG`)
- JSON output format (for log aggregation: ELK, Loki)
- Human-readable fallback when `KISEKI_LOG_FORMAT=pretty`

### A2: Replace eprintln! across codebase (73 calls)

Per-crate replacement rules:
- Startup banners → `tracing::info!`
- Error conditions → `tracing::error!` with structured fields
- Warnings → `tracing::warn!`
- Debug diagnostics → `tracing::debug!`

Structured fields on every event:
- `shard_id`, `tenant_id` where available
- `operation` (read/write/append/delete)
- `latency_us` on completion events
- `peer_addr` on network events

### A3: Add spans for hot paths

- `#[tracing::instrument]` on: gateway write/read, Raft commit,
  chunk put/get, EC encode/decode
- Span hierarchy: request → operation → sub-operation
- Tenant-scoped: no cross-tenant data in shared spans

### Validation A

| Check | Method | CI? |
|-------|--------|-----|
| Zero `eprintln!` remaining | `grep -r eprintln crates/` returns only bench example | Yes |
| `RUST_LOG=debug` produces structured output | Integration test | Yes |
| JSON format valid | Parse sample output with `jq` | Yes |
| Structured fields present | Grep test output for `shard_id=` | Yes |
| No regression | All tests pass | Yes |

**Effort**: 2–3 sessions

---

## Phase B: Metrics + Prometheus (WS 6.2)

**Goal**: Per-crate metrics exposed via Prometheus HTTP endpoint.

### B1: Metrics registry

New file: `kiseki-server/src/metrics.rs`

Use `prometheus` crate (or `metrics` + `metrics-exporter-prometheus`):
- Global registry with per-subsystem prefixes
- `kiseki_raft_commit_latency_seconds` — histogram
- `kiseki_raft_entries_total` — counter
- `kiseki_chunk_write_bytes_total` — counter
- `kiseki_chunk_read_bytes_total` — counter
- `kiseki_chunk_ec_encode_seconds` — histogram
- `kiseki_gateway_requests_total` — counter (by method, status)
- `kiseki_gateway_request_duration_seconds` — histogram
- `kiseki_pool_capacity_bytes` — gauge (total, used per pool)
- `kiseki_transport_connections` — gauge (active, idle)
- `kiseki_transport_rpc_duration_seconds` — histogram (by transport)
- `kiseki_shard_delta_count` — gauge (per shard)

### B2: Prometheus HTTP endpoint

`kiseki-server/src/runtime.rs`:
- Spawn metrics HTTP server on separate port (default 9090)
- `GET /metrics` → Prometheus text exposition format
- `GET /health` → `200 OK` (for load balancer probes)

### B3: Wire metrics into hot paths

Instrument at call sites:
- Gateway: record request count + duration in S3/NFS handlers
- Raft: record commit latency in `apply_command`
- Chunk: record bytes written/read in `ChunkOps`
- Transport: bridge `TransportMetrics` to Prometheus counters

### Validation B

| Check | Method | CI? |
|-------|--------|-----|
| `/metrics` returns valid Prometheus format | Integration test | Yes |
| `/health` returns 200 | Integration test | Yes |
| Request counter increments on S3 PUT | Integration test | Yes |
| Histogram has observed samples | Check `/metrics` output | Yes |
| No perf regression | Benchmark before/after | Manual |

**Effort**: 3–4 sessions

---

## Phase C: OpenTelemetry Tracing (WS 6.3)

**Goal**: Distributed traces across gateway → log → chunk with span propagation.

### C1: OTLP exporter

Workspace deps:
- `opentelemetry = "0.28"`, `opentelemetry-otlp = "0.28"`
- `tracing-opentelemetry = "0.28"` (bridges tracing → OTel)

`kiseki-server/src/main.rs`:
- Optional OTLP exporter when `OTEL_EXPORTER_OTLP_ENDPOINT` is set
- Configurable sampling rate via `OTEL_TRACES_SAMPLER_ARG` (default 0.1)
- Graceful shutdown: flush pending spans

### C2: Span context propagation

- gRPC metadata: inject/extract `traceparent` header (W3C Trace Context)
- Internal calls: pass `tracing::Span::current()` context
- Raft RPC: propagate trace context in length-prefixed header

### C3: Key spans

- `gateway.request` → `log.append_delta` → `chunk.put` (write path)
- `gateway.request` → `view.read_deltas` → `chunk.get` (read path)
- `raft.commit` → `state_machine.apply` (consensus path)
- Each span: operation, shard_id, tenant_id, latency

### Validation C

| Check | Method | CI? |
|-------|--------|-----|
| Traces exported to OTLP collector | Integration test with mock collector | Yes |
| Span hierarchy correct (parent-child) | Assert trace tree structure | Yes |
| Sampling rate respected | Send 100 requests at 10%, expect ~10 traces | Yes |
| gRPC metadata carries traceparent | Assert header present | Yes |
| No perf regression | Benchmark with tracing on vs off | Manual |

**Effort**: 2–3 sessions

---

## Phase D: S3 SigV4 Authentication (WS 5.1)

**Goal**: Parse and validate AWS Signature Version 4 on every S3 request.

### D1: SigV4 parser

New file: `kiseki-gateway/src/s3_auth.rs`

Parse the `Authorization` header:
```
AWS4-HMAC-SHA256
Credential=<access_key>/<date>/<region>/s3/aws4_request,
SignedHeaders=host;x-amz-content-sha256;x-amz-date,
Signature=<hex_signature>
```

Extract:
- `access_key` → lookup tenant secret key
- `date`, `region` → canonical request components
- `signed_headers` → which headers were signed
- `signature` → to validate

Also support presigned URLs (query string auth):
- `X-Amz-Algorithm`, `X-Amz-Credential`, `X-Amz-Date`,
  `X-Amz-SignedHeaders`, `X-Amz-Signature`, `X-Amz-Expires`

### D2: Signature validation

Signing key derivation (per AWS spec):
```
DateKey       = HMAC-SHA256("AWS4" + secret, date)
DateRegionKey = HMAC-SHA256(DateKey, region)
DateRegionServiceKey = HMAC-SHA256(DateRegionKey, "s3")
SigningKey    = HMAC-SHA256(DateRegionServiceKey, "aws4_request")
```

Canonical request:
```
HTTPMethod\n
CanonicalURI\n
CanonicalQueryString\n
CanonicalHeaders\n
SignedHeaders\n
HashedPayload
```

String to sign:
```
AWS4-HMAC-SHA256\n
Timestamp\n
Scope\n
SHA256(CanonicalRequest)
```

Validate: `HMAC-SHA256(SigningKey, StringToSign) == Signature`

Use `aws-lc-rs` (already in workspace) for HMAC-SHA256.

### D3: Access key store

New file: `kiseki-gateway/src/access_key_store.rs`

- In-memory map: `access_key_id → (secret_key, OrgId)`
- Loaded from config or control plane at boot
- `lookup(access_key_id) → Option<(SecretKey, OrgId)>`
- Later: backed by control plane gRPC for dynamic key management

### D4: Wire into S3 router

Modify `kiseki-gateway/src/s3_server.rs`:
- Extract middleware: validate SigV4 on every request before routing
- On success: set `tenant_id` from access key lookup
- On failure: return `403 Forbidden` with `SignatureDoesNotMatch` XML
- Skip auth for health check endpoint

### Validation D

| Check | Method | CI? |
|-------|--------|-----|
| Valid SigV4 header accepted | Unit test with known test vectors | Yes |
| Invalid signature returns 403 | Unit test | Yes |
| Missing Authorization returns 403 | Unit test | Yes |
| Presigned URL accepted | Unit test | Yes |
| Expired presigned URL returns 403 | Unit test | Yes |
| Correct tenant_id extracted from access key | Unit test | Yes |
| aws-cli `aws s3 cp` works against server | Integration test | Manual |
| boto3 client works | Integration test | Manual |

**Effort**: 3–4 sessions

---

## Phase E: Multi-Tenant Gateway (WS 5.4)

**Goal**: Per-request tenant extraction from auth credentials.

### E1: Tenant extraction middleware

Modify `kiseki-gateway/src/s3_server.rs`:
- Remove static `tenant_id` from `S3State`
- Replace with `AccessKeyStore` reference
- Each request: SigV4 → access_key → `OrgId` → scoped to that tenant

Modify `kiseki-gateway/src/nfs_server.rs`:
- For now: keep static tenant (Kerberos auth is WS 5.3, not in this slice)
- Add TODO marker for Kerberos integration

### E2: Namespace isolation

- S3 bucket → (tenant_id, namespace) mapping
- `PUT /bucket/key` → resolve bucket to tenant's namespace
- Cross-tenant access: rejected unless explicit sharing policy exists
- Audit log: include tenant_id on every operation

### E3: mTLS tenant extraction (gRPC path)

Modify `kiseki-gateway/src/mem_gateway.rs`:
- When mTLS is used (gRPC clients), extract `OrgId` from cert OU
- This path already exists in `kiseki-transport` — wire it through

### Validation E

| Check | Method | CI? |
|-------|--------|-----|
| Two access keys → different tenants → isolated data | Integration test | Yes |
| Tenant A cannot read Tenant B's objects | Integration test | Yes |
| Bucket resolves to correct namespace | Unit test | Yes |
| Audit log includes tenant_id | Unit test | Yes |

**Effort**: 2–3 sessions

---

## Phase F: Key Rotation Monitor + Crypto-Shred Wiring (WS 1.1 + 1.3)

**Goal**: Background rotation task + propagation of crypto-shred across nodes.

### F1: Key rotation monitor

New file: `kiseki-keymanager/src/rotation_monitor.rs`

- Background tokio task: polls epoch TTL every 60 seconds
- When TTL expires: call `key_manager.rotate()`
- Emit `tracing::info!` with old/new epoch
- Metric: `kiseki_key_rotation_total` counter

Configuration:
- `KISEKI_KEY_ROTATION_TTL` — epoch lifetime (default 90 days)
- `KISEKI_KEY_ROTATION_CHECK_INTERVAL` — poll interval (default 60s)

### F2: Crypto-shred propagation

- `shred_tenant()` already implemented in `kiseki-crypto/src/shred.rs`
- Wire to control plane: `DeleteTenant` gRPC → triggers shred on all nodes
- Cache invalidation: `key_cache.remove(tenant_id)` on shred event
- Metric: `kiseki_crypto_shred_total` counter

### F3: Rewrap orchestration

- `run_rewrap_batch()` exists in `kiseki-keymanager/src/rewrap_worker.rs`
- Wire to rotation monitor: after rotation, start background rewrap
- Progress tracking via `RewrapProgress` (atomic counters)
- Metric: `kiseki_rewrap_progress` gauge (completed / total)

### Validation F

| Check | Method | CI? |
|-------|--------|-----|
| Rotation monitor triggers after TTL | Unit test (short TTL) | Yes |
| New epoch created on rotation | Unit test | Yes |
| Rewrap starts after rotation | Integration test | Yes |
| Crypto-shred invalidates tenant data | Unit test (existing) | Yes |
| Cache cleared on shred | Unit test (existing) | Yes |

**Effort**: 2–3 sessions

---

## Phase Dependency Graph

```
Phase A (structured logging)
    │
    ├──────────────────┐
    ▼                  ▼
Phase B (metrics)    Phase D (S3 SigV4)
    │                  │
    ▼                  ▼
Phase C (OTel)      Phase E (multi-tenant)
    │                  │
    └──────┬───────────┘
           ▼
    Phase F (rotation + shred wiring)
```

A must come first (everything logs). B+D are parallel. C depends on B
(metrics infra). E depends on D (auth). F depends on A+B (logging + metrics).

## CI Integration

### New workspace dependencies

```toml
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["json", "env-filter"] }
```

Optional (Phase B):
```toml
prometheus = "0.13"
```

Optional (Phase C):
```toml
opentelemetry = "0.28"
opentelemetry-otlp = "0.28"
tracing-opentelemetry = "0.28"
```

### New CI checks

- `grep -r 'eprintln!' crates/ --include='*.rs' | grep -v bench | grep -v test` → must be empty
- Metrics endpoint integration test in e2e suite

## Estimated Total Effort

| Phase | Sessions | Hardware needed |
|-------|----------|-----------------|
| A: Structured logging | 2–3 | No |
| B: Metrics + Prometheus | 3–4 | No |
| C: OpenTelemetry | 2–3 | No |
| D: S3 SigV4 | 3–4 | No |
| E: Multi-tenant gateway | 2–3 | No |
| F: Key rotation + shred | 2–3 | No |
| **Total** | **14–20** | All CI-testable |
