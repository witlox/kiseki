# Phase D: Completeness — Go BDD + Security + Functional Gaps

## Context

Phases B (e2e), A (BDD real), C (protocols) complete. 460 tests passing.
31 Go BDD scenarios skipped (annoying). Security gaps tracked but unresolved.
NFS not wired in server. Stream processor not running. No mTLS on S3/NFS.

This plan organizes remaining work **by code area** — grouping changes
that touch the same files to minimize context switching.

---

## D.1: Go Control Plane — BDD + Steps (`control/`)

**Goal**: Get 31 skipped BDD scenarios passing. Currently 90 steps
implemented, 142 undefined (returning `godog.ErrPending`).

Files: `control/tests/acceptance/`, `control/pkg/`

### D.1a: New packages needed

| Package | Purpose |
|---------|---------|
| `control/pkg/namespace` | Namespace model + shard assignment |
| `control/pkg/flavor` | Flavor matching + best-fit scoring |
| `control/pkg/retention` | Retention hold lifecycle |
| `control/pkg/federation` | Peer registration + config sync |
| `control/pkg/maintenance` | Cluster maintenance mode |

### D.1b: Step implementation order (by scenario cluster)

| Cluster | Steps | Depends on |
|---------|-------|------------|
| Namespace management | 5 | `pkg/namespace` |
| Compliance tag enforcement | 6 | existing `pkg/tenant` |
| Quota scenarios | 6 | existing `pkg/tenant` |
| Flavor selection | 6 | `pkg/flavor` |
| Retention holds | 4 | `pkg/retention` |
| Maintenance mode | 5 | `pkg/maintenance` |
| Control plane outage | 6 | existing stores |
| Federation | 7 | `pkg/federation` |
| Advisory policy (budgets, profiles, FSM) | 28 | existing `pkg/advisory` |
| Audit & isolation | 5 | existing stores |

### D.1c: Go gRPC server mTLS

| File | Change |
|------|--------|
| `control/cmd/kiseki-control/main.go` | Add TLS from env vars (matching Rust pattern) |
| `control/pkg/grpc/control_service.go` | Wire remaining RPCs (namespace, quota, retention, maintenance) |

**Exit**: 32/32 Go BDD scenarios passing. `go test ./...` green.

---

## D.2: Server Runtime — Wire Missing Contexts (`kiseki-server/`)

**Goal**: NFS server, stream processor background task, audit wiring.

File: `crates/kiseki-server/src/runtime.rs` + `config.rs`

### D.2a: Spawn NFS server

| Change | Detail |
|--------|--------|
| `config.rs` | Add `nfs_addr: SocketAddr` (default `:2049`) |
| `runtime.rs` | Build `NfsGateway` from shared gateway, spawn `run_nfs_server` in `std::thread` |
| `docker-compose.yml` | Map port 2049 |

### D.2b: Stream processor background task

| Change | Detail |
|--------|--------|
| `runtime.rs` | Import `TrackedStreamProcessor`, spawn `tokio::spawn` polling every 100ms |
| | Track bootstrap view for the default namespace |
| | Log watermark advancement |

### D.2c: Wire audit store

| Change | Detail |
|--------|--------|
| `runtime.rs` | Remove `_` prefix from `audit_store`, pass to composition for event recording |

**Exit**: Server boots with gRPC + S3 + NFS + stream processor. Docker
compose exposes all three ports (9000, 9100, 2049).

---

## D.3: Gateway Security — mTLS + Auth (`kiseki-gateway/`, `kiseki-transport/`)

**Goal**: No plaintext protocol surfaces. Tenant identity from certs.

### D.3a: S3 gateway TLS

| File | Change |
|------|--------|
| `crates/kiseki-gateway/src/s3_server.rs` | Accept `Option<TlsConfig>`, use `axum_server::tls_rustls` or `tokio-rustls` |
| `crates/kiseki-server/src/runtime.rs` | Pass TLS config to S3 server when available |

### D.3b: NFS transport TLS

| File | Change |
|------|--------|
| `crates/kiseki-gateway/src/nfs_server.rs` | Wrap `TcpListener` with `tokio-rustls::TlsAcceptor` |
| `crates/kiseki-server/src/runtime.rs` | Pass TLS config to NFS server |

### D.3c: Tenant identity from mTLS cert

| File | Change |
|------|--------|
| `crates/kiseki-gateway/src/s3_server.rs` | Extract `OrgId` from client cert SAN/OU instead of hardcoded bootstrap |
| `crates/kiseki-gateway/src/nfs_ops.rs` | Accept `OrgId` from TLS peer cert |
| `crates/kiseki-log/src/grpc.rs` | Wire tonic interceptor to validate tenant_id (existing TODO) |

### D.3d: Per-tenant dedup policy

| File | Change |
|------|--------|
| `crates/kiseki-gateway/src/mem_gateway.rs` | Look up tenant's `DedupPolicy` from a registry instead of per-gateway config |

**Exit**: All protocol surfaces require mTLS when TLS files are configured.
Tenant ID extracted from cert, not hardcoded. Open findings resolved.

---

## D.4: Protocol Completeness (`kiseki-gateway/`)

**Goal**: Fill in NFS LOOKUP/CREATE/READDIR, S3 LIST, pagination.

### D.4a: NFS v3 missing procedures

| File | Change |
|------|--------|
| `crates/kiseki-gateway/src/nfs3_server.rs` | Implement LOOKUP, CREATE, REMOVE, READDIR dispatchers |
| `crates/kiseki-gateway/src/nfs_ops.rs` | Add `lookup_by_name`, `readdir`, `remove` to `NfsContext` |

### D.4b: NFS v4.2 missing operations

| File | Change |
|------|--------|
| `crates/kiseki-gateway/src/nfs4_server.rs` | Implement LOOKUP, REMOVE, READDIR ops in COMPOUND |
| | Wire IO_ADVISE → advisory subsystem (currently accepts but ignores) |

### D.4c: S3 LIST + pagination

| File | Change |
|------|--------|
| `crates/kiseki-gateway/src/s3_server.rs` | Add `GET /:bucket` → ListObjectsV2 |
| `crates/kiseki-log/src/grpc.rs` | Add `max_count` to `ReadDeltasRequest` or server-side cap |

### D.4d: NFS + cross-protocol e2e tests

| File | Tests |
|------|-------|
| `tests/e2e/test_nfs_gateway.py` | NFSv3 NULL + GETATTR via raw TCP (python struct pack) |
| `tests/e2e/test_cross_protocol.py` | Add S3 write → NFS read (if NFS e2e feasible) |

**Exit**: NFS LOOKUP/CREATE/READDIR work. S3 LIST works. ReadDeltas
has pagination. NFS e2e test via raw TCP.

---

## Execution Order

```
D.1 Go BDD (independent, no Rust changes)
    │
    ├──→ D.2 Server runtime wiring (NFS + stream proc + audit)
    │        │
    │        └──→ D.3 Security (mTLS on S3 + NFS + Go + interceptors)
    │                  │
    │                  └──→ D.4 Protocol completeness (LOOKUP, LIST, pagination)
    │
    └──→ D.1 can run in parallel with D.2
```

D.1 is independent (Go only). D.2-D.4 are sequential (each builds on prior).

## Test Projections

| Phase | New Tests | Total |
|-------|-----------|-------|
| D.1 | ~31 Go BDD scenarios | ~491 |
| D.2 | +2 (NFS wired, stream proc) | ~493 |
| D.3 | +2 e2e (mTLS handshake) | ~495 |
| D.4 | +4 (NFS e2e, S3 LIST, pagination) | ~499 |

## Key Files

| File | Phases |
|------|--------|
| `control/tests/acceptance/steps_*.go` | D.1 |
| `control/pkg/{namespace,flavor,retention,federation,maintenance}/` | D.1 |
| `control/cmd/kiseki-control/main.go` | D.1, D.3 |
| `crates/kiseki-server/src/runtime.rs` | D.2, D.3 |
| `crates/kiseki-server/src/config.rs` | D.2 |
| `crates/kiseki-gateway/src/s3_server.rs` | D.3, D.4 |
| `crates/kiseki-gateway/src/nfs_server.rs` | D.2, D.3 |
| `crates/kiseki-gateway/src/nfs3_server.rs` | D.4 |
| `crates/kiseki-gateway/src/nfs4_server.rs` | D.4 |
| `crates/kiseki-gateway/src/nfs_ops.rs` | D.4 |
| `crates/kiseki-log/src/grpc.rs` | D.3, D.4 |
| `docker-compose.yml` | D.2 |

## Out of Scope (deferred to later)

- Multi-node Raft (persistence, failover) — major effort
- EC erasure coding — needs storage device abstraction
- CI pipeline (GitHub Actions)
- PyO3 client bindings
- Federation implementation
- SigV4 S3 auth (use mTLS instead for now)
